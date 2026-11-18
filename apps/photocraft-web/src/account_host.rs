//! Account project exchange for PhotoCraft embedded by the same-origin desktop.

use std::{cell::RefCell, rc::Rc};

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use photocraft_ui_egui::{AccountDialogFn, AccountDialogRequest, AccountSaveFn, AccountSaveRequest, FileDialogAnswer, FileDialogReply, PhotocraftApp};
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

fn fields(object: &Object, request: &AccountSaveRequest) -> Result<(), String> {
    put(object, "ticket", request.ticket.to_string())?;
    put(object, "documentId", request.document.0.to_string())?;
    put(object, "revision", request.revision.to_string())?;
    put(object, "path", request.path.as_str())
}

struct StartRequest {
    id: String,
    project: Option<(String, ArrayBuffer)>,
}

impl StartRequest {
    fn read(packet: &JsValue) -> Option<Self> {
        let id = text(packet, "requestId").filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))?;
        let path = field(packet, "path")?;
        let bytes = field(packet, "bytes")?;
        let project = if path.is_null() && bytes.is_null() {
            None
        } else {
            let path = path.as_string().filter(|path| !path.is_empty() && path.len() <= 2048)?;
            let bytes = bytes.dyn_into::<ArrayBuffer>().ok()?;
            let length = bytes.byte_length() as usize;
            if !(1..=MAX_PROJECT_BYTES).contains(&length) {
                return None;
            }
            Some((path, bytes))
        };
        Some(Self { id, project })
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
            match text(&data, "type").as_deref() {
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
                // An uncertain write remains pending until the host has a verified result.
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
            let message = packet("photocraft:account-save")?;
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

    pub fn apply_start(&self, app: &mut PhotocraftApp) -> Result<(), String> {
        let Some(start) = self.exchange.borrow_mut().start.take() else { return Ok(()) };
        let mut path = None;
        let opened = if let Some((name, buffer)) = start.project {
            let bytes = Uint8Array::new(buffer.as_ref()).to_vec();
            path = Some(name);
            app.start_account_document(path.as_deref().map(|name| (name, bytes.as_slice())))
        } else {
            app.start_account_document(None)
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

    pub fn apply_save_result(&self, app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
        let Some((request, result)) = ({
            let mut exchange = self.exchange.borrow_mut();
            exchange.reply.take().and_then(|result| exchange.pending.take().map(|request| (request, result)))
        }) else {
            return Ok(());
        };
        let adopted = app.finish_account_save(&request, result, ctx);
        let message = packet("photocraft:account-save-settled")?;
        fields(&message, &request)?;
        put(&message, "adopted", adopted.is_ok())?;
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
