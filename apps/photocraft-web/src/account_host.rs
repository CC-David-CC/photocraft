//! Account project exchange for PhotoCraft embedded by the same-origin desktop.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use photocraft_doc::DocId;
use photocraft_ui_egui::{AccountDialogFn, AccountDialogRequest, AccountProjectState, AccountSaveFn, AccountSaveRequest, FileDialogAnswer, FileDialogReply, PhotocraftApp};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{MessageEvent, Window};

// This is the WebOS Storage gateway's maximum binary upload size.
const MAX_PROJECT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct Parent {
    window: Window,
    origin: String,
    generation: String,
}

impl Parent {
    pub fn connect() -> Result<Self, String> {
        let window = web_sys::window().ok_or("No browser window")?;
        let parent = window.parent().ok().flatten().ok_or("No parent desktop")?;
        let origin = window.location().origin().map_err(|_| "No browser origin")?;
        if Object::is(window.as_ref(), parent.as_ref()) || parent.location().origin().ok().as_ref() != Some(&origin) {
            return Err("An account editor requires a same-origin parent desktop".into());
        }
        let mut entropy = [0u8; 16];
        window
            .crypto()
            .and_then(|crypto| crypto.get_random_values_with_u8_array(&mut entropy))
            .map_err(|_| "Could not identify the native editor generation")?;
        let generation = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { window: parent, origin, generation })
    }

    fn send(&self, packet: &Object, transfer: &Array) -> Result<(), String> {
        put(packet, "generation", self.generation.as_str())?;
        self.window.post_message_with_transfer(packet, &self.origin, transfer).map_err(|_| "Could not reach the parent desktop".into())
    }
}

fn packet(kind: &str) -> Result<Object, String> {
    let object = Object::new();
    put(&object, "type", kind)?;
    Ok(object)
}

fn put(object: &Object, key: &str, value: impl Into<JsValue>) -> Result<(), String> {
    if Reflect::set(object, &JsValue::from_str(key), &value.into()).ok() == Some(true) { Ok(()) } else { Err("Could not encode an account message".into()) }
}

fn text(value: &JsValue, name: &str) -> Option<String> {
    field(value, name)?.as_string()
}

fn field(value: &JsValue, name: &str) -> Option<JsValue> {
    Reflect::get(value, &JsValue::from_str(name)).ok()
}

fn request_id(value: &JsValue) -> Option<String> {
    text(value, "requestId").filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

fn fields(object: &Object, request: &AccountSaveRequest) -> Result<(), String> {
    put(object, "ticket", request.ticket.to_string())?;
    put(object, "documentId", request.document.0.to_string())?;
    put(object, "revision", request.revision.to_string())?;
    put(object, "path", request.path.as_str())
}

fn project_fields(object: &Object, state: Option<AccountProjectState>) -> Result<(), String> {
    put(object, "available", state.is_some())?;
    if let Some(state) = state {
        put(object, "documentId", state.document.0.to_string())?;
        put(object, "revision", state.revision.to_string())?;
        put(object, "dirty", state.dirty)?;
        put(object, "capturable", state.capturable)?;
        put(object, "autosaveEligible", state.autosave_eligible)?;
        put(object, "operationPending", state.operation_pending)?;
        put(object, "unfinishedEdit", state.unfinished_edit)?;
    }
    Ok(())
}

fn browser_project_state(app: &PhotocraftApp, browser_work_pending: bool) -> Option<AccountProjectState> {
    let mut state = app.account_project_state()?;
    state.operation_pending |= browser_work_pending;
    state.autosave_eligible &= !browser_work_pending;
    Some(state)
}

struct StartRequest {
    id: String,
    project: Option<(Option<String>, ArrayBuffer)>,
    new_settings: Option<serde_json::Value>,
    recover: bool,
}

impl StartRequest {
    fn read(packet: &JsValue) -> Option<Self> {
        let id = request_id(packet)?;
        let recover = field(packet, "recover").and_then(|value| value.as_bool()).unwrap_or(false);
        let path = field(packet, "path")?;
        let bytes = field(packet, "bytes")?;
        let project = if !recover && path.is_null() && bytes.is_null() {
            None
        } else {
            let path = if recover && path.is_null() {
                None
            } else {
                Some(path.as_string().filter(|path| !path.is_empty() && path.len() <= 2048)?)
            };
            let bytes = bytes.dyn_into::<ArrayBuffer>().ok()?;
            let length = bytes.byte_length() as usize;
            if !(1..=MAX_PROJECT_BYTES).contains(&length) {
                return None;
            }
            Some((path, bytes))
        };
        let settings = field(packet, "newSettings")?;
        let new_settings = if settings.is_null() || settings.is_undefined() {
            None
        } else {
            let settings = settings.as_string().filter(|settings| settings.len() <= 4096)?;
            let value: serde_json::Value = serde_json::from_str(&settings).ok()?;
            Some(value.is_object().then_some(value)?)
        };
        if recover && new_settings.is_some() {
            return None;
        }
        Some(Self { id, project, new_settings, recover })
    }
}

#[derive(Default)]
struct Exchange {
    start: Option<StartRequest>,
    started: bool,
    last_dialog: u64,
    dialog: Option<PendingDialog>,
    pending: Option<AccountSaveRequest>,
    reply: Option<Result<(), String>>,
    save_uncertain: bool,
    close: Option<CloseRequest>,
    close_approved: bool,
    recovery: Option<RecoveryRequest>,
    project_state: Option<AccountProjectState>,
    save_capture: Option<SaveCaptureRequest>,
    dispatching_capture: Option<SaveCaptureRequest>,
    session_commands: VecDeque<SessionRequest>,
    session_lease: Option<String>,
    session_finalized: bool,
}

struct SessionRequest {
    request_id: String,
    lease: String,
    operation: SessionOperation,
}

#[derive(Clone, Copy)]
enum SessionOperation {
    Pause { document: DocId, revision: u64, dirty: bool, unfinished_edit: bool },
    Release,
    Finalize,
}

impl SessionRequest {
    fn read(data: &JsValue) -> Option<Self> {
        let request_id = request_id(data)?;
        let lease = text(data, "lease").filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))?;
        let operation = match text(data, "operation").as_deref() {
            Some("pause") => SessionOperation::Pause {
                document: DocId(text(data, "documentId")?.parse().ok()?),
                revision: text(data, "revision")?.parse().ok()?,
                dirty: field(data, "dirty")?.as_bool()?,
                unfinished_edit: field(data, "unfinishedEdit")?.as_bool()?,
            },
            Some("release") => SessionOperation::Release,
            Some("finalize") => SessionOperation::Finalize,
            _ => return None,
        };
        Some(Self {request_id, lease, operation})
    }
}

struct SaveCaptureRequest {
    project: RecoveryRequest,
    path: String,
    session_save: bool,
}

impl SaveCaptureRequest {
    fn result_packet(&self) -> Result<Object, String> {
        let message = packet(if self.session_save {"photocraft:account-session-save-result"} else {"photocraft:account-autosave-result"})?;
        put(&message, "requestId", self.project.id.as_str())?;
        put(&message, "reviewedRevision", self.project.revision.to_string())?;
        Ok(message)
    }
}

struct RecoveryRequest {
    id: String,
    document: DocId,
    revision: u64,
}

impl RecoveryRequest {
    fn read(packet: &JsValue) -> Option<Self> {
        Some(Self {
            id: request_id(packet)?,
            document: DocId(text(packet, "documentId")?.parse().ok()?),
            revision: text(packet, "revision")?.parse().ok()?,
        })
    }
}

struct CloseRequest {
    id: String,
    interactive: bool,
    started: bool,
}

struct PendingDialog {
    id: u64,
    save: bool,
    reply: FileDialogReply,
}

fn receive_dialog(exchange: &mut Exchange, data: &JsValue) {
    let id = text(data, "requestId").and_then(|id| id.parse::<u64>().ok());
    let Some(pending) = exchange.dialog.as_ref().filter(|pending| Some(pending.id) == id) else { return };
    let result = match text(data, "outcome").as_deref() {
        Some("cancelled") => None,
        Some("opened") if !pending.save => None,
        Some("destination") if pending.save => {
            let Some(path) = text(data, "path").filter(|path| !path.is_empty() && path.len() <= 2048) else { return };
            Some(FileDialogAnswer::SaveTo(path))
        }
        Some("failed") => Some(FileDialogAnswer::Unavailable(
            text(data, "message").filter(|why| !why.is_empty() && why.len() <= 1024).unwrap_or_else(|| "The account dialog failed".into()),
        )),
        _ => return,
    };
    if let Some(pending) = exchange.dialog.take() {
        pending.reply.send(result);
    }
}

pub struct AccountHost {
    parent: Parent,
    window: Window,
    exchange: Rc<RefCell<Exchange>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
}

impl AccountHost {
    pub fn install(parent: Parent, ctx: &egui::Context) -> Result<(Self, AccountSaveFn), String> {
        let window = web_sys::window().ok_or("No browser window")?;
        let exchange = Rc::new(RefCell::new(Exchange::default()));
        let inbox = exchange.clone();
        let wake = ctx.clone();
        let sender = parent.clone();
        let listener = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if event.origin() != sender.origin || !event.source().is_some_and(|source| Object::is(source.as_ref(), sender.window.as_ref())) {
                return;
            }
            let data = event.data();
            // A reload can keep the same WindowProxy and recycle the app's
            // native document, revision and ticket numbers.
            if text(&data, "generation").as_deref() != Some(sender.generation.as_str()) {
                return;
            }
            let Ok(mut inbox) = inbox.try_borrow_mut() else { return };
            let kind = text(&data, "type");
            if kind.as_deref() == Some("photocraft:account-session") {
                if inbox.started && inbox.session_commands.len() < 8
                    && let Some(command) = SessionRequest::read(&data)
                {
                    inbox.session_commands.push_back(command);
                    wake.request_repaint();
                }
                return;
            }
            if inbox.session_lease.is_some() || inbox.session_finalized {
                return;
            }
            match kind.as_deref() {
                Some("photocraft:account-start") => {
                    if !inbox.started && inbox.start.is_none() {
                        inbox.start = StartRequest::read(&data);
                        if inbox.start.is_some() {
                            wake.request_repaint();
                        }
                    }
                    return;
                }
                Some("photocraft:account-save-result") => {}
                Some("photocraft:account-dialog-result") => {
                    receive_dialog(&mut inbox, &data);
                    return;
                }
                Some("photocraft:account-close") => {
                    if inbox.started && inbox.close.is_none() && !inbox.close_approved
                        && let Some(id) = request_id(&data)
                        && let Some(interactive) = field(&data, "interactive").and_then(|value| value.as_bool())
                    {
                        inbox.close = Some(CloseRequest { id, interactive, started: false });
                        wake.request_repaint();
                    }
                    return;
                }
                Some("photocraft:account-recovery-capture") => {
                    if inbox.started && !inbox.close_approved && inbox.recovery.is_none() {
                        inbox.recovery = RecoveryRequest::read(&data);
                        if inbox.recovery.is_some() {
                            wake.request_repaint();
                        }
                    }
                    return;
                }
                Some("photocraft:account-autosave" | "photocraft:account-session-save") => {
                    if inbox.started && !inbox.close_approved && inbox.save_capture.is_none()
                        && let Some(project) = RecoveryRequest::read(&data)
                        && let Some(path) = text(&data, "path").filter(|path| !path.is_empty() && path.len() <= 2048)
                    {
                        let session_save = kind.as_deref() == Some("photocraft:account-session-save");
                        inbox.save_capture = Some(SaveCaptureRequest {project, path, session_save});
                        wake.request_repaint();
                    }
                    return;
                }
                _ => return,
            }
            let Some(request) = inbox.pending.as_ref() else { return };
            let matches = |key, value: &str| text(&data, key).as_deref() == Some(value);
            if inbox.reply.is_some()
                || !matches("ticket", &request.ticket.to_string())
                || !matches("documentId", &request.document.0.to_string())
                || !matches("revision", &request.revision.to_string())
                || !matches("path", &request.path)
            {
                return;
            }
            let result = match text(&data, "outcome").as_deref() {
                Some("written") => Ok(()),
                Some("failed") => Err(text(&data, "message")
                    .filter(|message| !message.is_empty() && message.len() <= 1024)
                    .unwrap_or_else(|| "The account write failed".into())),
                Some("unknown") => {
                    inbox.save_uncertain = true;
                    wake.request_repaint();
                    return;
                }
                _ => return,
            };
            inbox.reply = Some(result);
            wake.request_repaint();
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).map_err(|_| "Could not attach the account host listener")?;
        let outbox = exchange.clone();
        let writer_parent = parent.clone();
        let write: AccountSaveFn = Box::new(move |request, bytes| {
            if bytes.len() > MAX_PROJECT_BYTES {
                return Err("The project exceeds the account upload limit".into());
            }
            let mut exchange = outbox.try_borrow_mut().map_err(|_| "Account save transport is busy")?;
            if !exchange.started {
                return Err("Start the PhotoCraft account view before saving".into());
            }
            if exchange.pending.is_some() {
                return Err("An account save is awaiting a result".into());
            }
            let message = if let Some(capture) = &exchange.dispatching_capture {
                let message = capture.result_packet()?;
                put(&message, "outcome", "captured")?;
                message
            } else {
                packet("photocraft:account-save")?
            };
            fields(&message, request)?;
            let buffer = Uint8Array::from(bytes.as_slice()).buffer();
            put(&message, "bytes", buffer.clone())?;
            writer_parent.send(&message, &Array::of1(&buffer))?;
            exchange.pending = Some(request.clone());
            Ok(())
        });
        Ok((Self { parent, window, exchange, listener }, write))
    }

    pub fn ready(&self) -> Result<(), String> {
        self.parent.send(&packet("photocraft:account-ready")?, &Array::new())
    }

    pub fn dialog(&self) -> AccountDialogFn {
        let exchange = self.exchange.clone();
        let parent = self.parent.clone();
        Box::new(move |request, reply| match send_dialog(&exchange, &parent, request) {
            Ok((id, save)) => exchange.borrow_mut().dialog = Some(PendingDialog { id, save, reply }),
            Err(error) => reply.send(Some(FileDialogAnswer::Unavailable(error))),
        })
    }

    pub fn request_window_close(&self) -> impl Fn() -> Result<(), String> + use<> {
        let parent = self.parent.clone();
        move || parent.send(&packet("photocraft:account-close-intent")?, &Array::new())
    }

    /// Tick before native input and again after the prompt has drawn.
    /// Once approved, stop running the embedded editor until its owner removes it.
    pub fn apply_close(&self, app: &mut PhotocraftApp, browser_work_pending: impl Fn() -> bool) -> Result<bool, String> {
        let mut exchange = self.exchange.borrow_mut();
        if exchange.close_approved {
            return Ok(true);
        }
        let Some(close) = exchange.close.as_mut() else { return Ok(false) };
        let outcome = if close.started {
            app.account_close_outcome().map(Ok)
        } else {
            close.started = true;
            if browser_work_pending() {
                Some(Err("Wait for the current PhotoCraft browser operation to finish.".into()))
            } else {
                match app.request_account_close(close.interactive) {
                    Ok(()) => app.account_close_outcome().map(Ok),
                    Err(error) => Some(Err(error)),
                }
            }
        };
        let Some(mut outcome) = outcome else { return Ok(false) };
        if outcome == Ok(true) && browser_work_pending() {
            outcome = Err("Wait for the current PhotoCraft browser operation to finish.".into());
        }
        let interactive = close.interactive;
        let message = packet("photocraft:account-close-result")?;
        put(&message, "requestId", close.id.as_str())?;
        match &outcome {
            Ok(approved) => put(&message, "approved", *approved)?,
            Err(error) => put(&message, "message", error.as_str())?,
        }
        self.parent.send(&message, &Array::new())?;
        exchange.close = None;
        exchange.close_approved = interactive && outcome == Ok(true);
        Ok(exchange.close_approved)
    }

    pub fn session_suspended(&self) -> bool {
        let exchange = self.exchange.borrow();
        exchange.session_lease.is_some() || exchange.session_finalized
    }

    /// Pause the native app at the exact reviewed revision before Accounts is
    /// revoked. The same lease must release or finalize the immutable view.
    pub fn apply_session(&self, app: &PhotocraftApp, browser_work_pending: impl Fn() -> bool) -> Result<(), String> {
        let mut exchange = self.exchange.borrow_mut();
        while let Some(request) = exchange.session_commands.pop_front() {
            let token_matches = exchange.session_lease.as_deref() == Some(request.lease.as_str());
            let (operation, result) = match request.operation {
                SessionOperation::Pause {document, revision, dirty, unfinished_edit} => {
                    let ready = !exchange.close_approved && exchange.close.is_none() && !exchange.session_finalized
                        && exchange.session_lease.is_none() && exchange.save_capture.is_none()
                        && !browser_work_pending();
                    ("pause", if ready {
                        app.check_account_pause(document, revision, dirty, unfinished_edit)
                    } else {
                        Err("Finish the pending PhotoCraft operation before ending the session.".into())
                    })
                }
                SessionOperation::Release => ("release", token_matches.then_some(()).ok_or_else(|| "PhotoCraft session pause has changed.".into())),
                SessionOperation::Finalize => ("finalize", token_matches.then_some(()).ok_or_else(|| "PhotoCraft session pause has changed.".into())),
            };
            let message = packet("photocraft:account-session-result")?;
            put(&message, "requestId", request.request_id)?;
            put(&message, "lease", request.lease.as_str())?;
            put(&message, "operation", operation)?;
            put(&message, "ok", result.is_ok())?;
            if let Err(detail) = &result {
                put(&message, "message", detail.as_str())?;
            }
            self.parent.send(&message, &Array::new())?;
            if result.is_ok() {
                match request.operation {
                    SessionOperation::Pause {..} => exchange.session_lease = Some(request.lease),
                    SessionOperation::Release => exchange.session_lease = None,
                    SessionOperation::Finalize => {
                        exchange.session_lease = None;
                        exchange.session_finalized = true;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn apply_start(&self, app: &mut PhotocraftApp) -> Result<(), String> {
        let Some(start) = self.exchange.borrow_mut().start.take() else { return Ok(()) };
        let mut path = None;
        let opened = if let Some((name, buffer)) = start.project {
            let bytes = Uint8Array::new(buffer.as_ref()).to_vec();
            path = name;
            if start.recover {
                app.start_account_recovery(path.as_deref(), &bytes)
            } else {
                app.start_account_document(path.as_deref().map(|name| (name, bytes.as_slice())), start.new_settings)
            }
        } else {
            app.start_account_document(None, start.new_settings)
        };
        let message = packet("photocraft:account-started")?;
        put(&message, "requestId", start.id)?;
        put(&message, "ok", opened.is_ok())?;
        match opened {
            Ok((document, revision)) => {
                self.exchange.borrow_mut().started = true;
                put(&message, "documentId", document.0.to_string())?;
                put(&message, "revision", revision.to_string())?;
                put(&message, "path", path.map_or(JsValue::NULL, JsValue::from))?;
            }
            Err(error) => put(&message, "message", error)?,
        }
        self.parent.send(&message, &Array::new())
    }

    pub fn apply_recovery_capture(&self, app: &PhotocraftApp) -> Result<(), String> {
        let Some(request) = self.exchange.borrow_mut().recovery.take() else { return Ok(()) };
        let message = packet("photocraft:account-recovery-captured")?;
        put(&message, "requestId", request.id)?;
        put(&message, "documentId", request.document.0.to_string())?;
        put(&message, "revision", request.revision.to_string())?;
        let transfer = Array::new();
        match app.capture_account_recovery(request.document, request.revision) {
            Ok(bytes) if bytes.len() <= MAX_PROJECT_BYTES => {
                let buffer = Uint8Array::from(bytes.as_slice()).buffer();
                put(&message, "bytes", buffer.clone())?;
                transfer.push(buffer.as_ref());
            }
            Ok(_) => put(&message, "message", "This PhotoCraft recovery snapshot exceeds the browser recovery limit")?,
            Err(error) => put(&message, "message", error)?,
        }
        self.parent.send(&message, &transfer)
    }

    pub fn publish_project_state(&self, app: &PhotocraftApp, browser_work_pending: bool) -> Result<(), String> {
        let mut exchange = self.exchange.borrow_mut();
        if !exchange.started || exchange.close_approved {
            return Ok(());
        }
        let state = browser_project_state(app, browser_work_pending);
        if state == exchange.project_state {
            return Ok(());
        }
        let message = packet("photocraft:account-project-state")?;
        project_fields(&message, state)?;
        self.parent.send(&message, &Array::new())?;
        exchange.project_state = state;
        Ok(())
    }

    pub fn apply_save_capture(&self, app: &mut PhotocraftApp) -> Result<(), String> {
        let Some(request) = self.exchange.borrow_mut().save_capture.take() else { return Ok(()) };
        let (document, revision) = (request.project.document, request.project.revision);
        let path = request.path.clone();
        let session_save = request.session_save;
        self.exchange.borrow_mut().dispatching_capture = Some(request);
        let outcome = if session_save {
            app.begin_account_session_save(document, revision, &path)
        } else {
            app.begin_account_autosave(document, revision, &path)
        };
        let request = self.exchange.borrow_mut().dispatching_capture.take().ok_or("The PhotoCraft save capture changed")?;
        if outcome == Ok(true) {
            // The ordinary save writer already transferred bytes and the ticket.
            return Ok(());
        }
        let message = request.result_packet()?;
        put(&message, "documentId", document.0.to_string())?;
        put(&message, "revision", revision.to_string())?;
        put(&message, "path", path)?;
        if let Err(error) = outcome {
            put(&message, "outcome", "failed")?;
            put(&message, "message", error)?;
        } else {
            put(&message, "outcome", "busy")?;
        }
        self.parent.send(&message, &Array::new())
    }

    pub fn apply_save_result(&self, app: &mut PhotocraftApp, ctx: &egui::Context, browser_work_pending: bool) -> Result<(), String> {
        let Some((request, result)) = ({
            let mut exchange = self.exchange.borrow_mut();
            if std::mem::take(&mut exchange.save_uncertain) {
                app.account_save_uncertain();
            }
            exchange.reply.take().and_then(|result| exchange.pending.take().map(|request| (request, result)))
        }) else {
            return Ok(());
        };
        let adopted = app.finish_account_save(&request, result, ctx);
        let message = packet("photocraft:account-save-settled")?;
        fields(&message, &request)?;
        put(&message, "adopted", adopted.is_ok())?;
        let state = Object::new();
        project_fields(&state, browser_project_state(app, browser_work_pending))?;
        put(&message, "projectState", state)?;
        if let Err(error) = adopted {
            put(&message, "message", error)?;
        }
        self.parent.send(&message, &Array::new())
    }
}

fn send_dialog(exchange: &RefCell<Exchange>, parent: &Parent, request: AccountDialogRequest) -> Result<(u64, bool), String> {
    let mut exchange = exchange.try_borrow_mut().map_err(|_| "Account dialog transport is busy")?;
    if !exchange.started || exchange.dialog.is_some() {
        return Err("The account dialog is unavailable".into());
    }
    let id = exchange.last_dialog.checked_add(1).ok_or("Account dialog request limit reached")?;
    let message = packet("photocraft:account-dialog")?;
    put(&message, "requestId", id.to_string())?;
    let save = match request {
        AccountDialogRequest::New { settings } => {
            put(&message, "operation", "new")?;
            put(&message, "settings", settings.to_string())?;
            false
        }
        AccountDialogRequest::Open { path } => {
            put(&message, "operation", "open")?;
            put(&message, "path", path.map_or(JsValue::NULL, JsValue::from))?;
            false
        }
        AccountDialogRequest::Save { suggested } => {
            put(&message, "operation", "save")?;
            put(&message, "suggested", suggested)?;
            true
        }
    };
    parent.send(&message, &Array::new())?;
    exchange.last_dialog = id;
    Ok((id, save))
}

pub fn profile_key(base: &str, profile: Option<&str>) -> String {
    profile.map_or_else(|| base.to_owned(), |profile| format!("{base}.account.{profile}"))
}

impl Drop for AccountHost {
    fn drop(&mut self) {
        let _ = self.window.remove_event_listener_with_callback("message", self.listener.as_ref().unchecked_ref());
    }
}
