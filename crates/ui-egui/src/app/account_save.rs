//! Account writes settle separately from encoding and transport dispatch.

use photocraft_doc::{DocId, Document, LayerContent, LayerId, SmartSource, TextLayer};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashMap, sync::Arc};

use crate::{PhotocraftApp, dialogs::discard_ui};

mod nested;

const TYPE_DRAFT: &[u8] = b"PCDRAFT1";
const TYPE_DRAFT_HEADER: usize = 16;

const EXTERNAL_ASSET_WARNING: &str = "This project references a file outside the browser. Use Layer → Smart Objects → Replace Contents to embed a copy in the project.";

fn account_external_asset_warning(doc: &Document) -> Option<String> {
    doc.walk().into_iter().any(|(_, _, layer)| {
        matches!(&layer.content, LayerContent::Smart(smart)
            if matches!(&smart.source, SmartSource::Linked { path }
                if photocraft_io::linked::embedded_payload(&doc.metadata, path).is_none()))
    }).then(|| EXTERNAL_ASSET_WARNING.to_owned())
}

/// Exact native state observed by the browser journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountProjectState {
    pub document: DocId,
    pub revision: u64,
    pub recovery_revision: u64,
    pub dirty: bool,
    pub capturable: bool,
    pub autosave_eligible: bool,
    pub operation_pending: bool,
    pub unfinished_edit: bool,
}

#[derive(Serialize, Deserialize)]
struct TypeDraft {
    layer: u64,
    anchor: usize,
    caret: usize,
    created: bool,
    mask: bool,
}

impl TypeDraft {
    fn text<'a>(&self, doc: &'a Document) -> Result<&'a TextLayer, String> {
        let layer = doc.layer(LayerId(self.layer)).ok_or("The recovered text layer is missing")?;
        let LayerContent::Text(text) = &layer.content else { return Err("The recovered layer is not editable text".into()) };
        let count = text.text.chars().count();
        if self.anchor > count || self.caret > count {
            return Err("The recovered text selection is invalid".into());
        }
        Ok(text)
    }
}

fn encode_type_draft(bytes: Vec<u8>, before: &Document, draft: &TypeDraft) -> Result<Vec<u8>, String> {
    let details = serde_json::to_vec(draft).map_err(|error| error.to_string())?;
    let before = photocraft_format::save_to_bytes(before, &Default::default()).map_err(|error| error.to_string())?;
    let details_len = u32::try_from(details.len()).map_err(|error| error.to_string())?;
    let before_len = u32::try_from(before.len()).map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    out.try_reserve_exact(TYPE_DRAFT_HEADER.saturating_add(details.len()).saturating_add(before.len()).saturating_add(bytes.len()))
        .map_err(|error| error.to_string())?;
    out.extend_from_slice(TYPE_DRAFT);
    out.extend_from_slice(&details_len.to_le_bytes());
    out.extend_from_slice(&before_len.to_le_bytes());
    out.extend(details);
    out.extend(before);
    out.extend(bytes);
    Ok(out)
}

type RecoveredType = (Document, TypeDraft);

fn decode_recovery(bytes: &[u8]) -> Result<(Document, Option<RecoveredType>), String> {
    let load = |part| photocraft_format::load_from_bytes(part).map_err(|error| error.to_string());
    if !bytes.starts_with(TYPE_DRAFT) {
        return Ok((load(bytes)?, None));
    }
    let invalid = || "Invalid PhotoCraft Type recovery snapshot".to_owned();
    let u32_at = |offset: usize| {
        let word = bytes.get(offset..offset + 4).ok_or_else(invalid)?;
        Ok::<usize, String>(u32::from_le_bytes(word.try_into().map_err(|_| invalid())?) as usize)
    };
    let details_len = u32_at(8)?;
    let before_len = u32_at(12)?;
    if !(1..=1024).contains(&details_len) || before_len == 0 {
        return Err(invalid());
    }
    let before_at = TYPE_DRAFT_HEADER + details_len;
    let project_at = before_at.checked_add(before_len).ok_or_else(invalid)?;
    let draft: TypeDraft = serde_json::from_slice(bytes.get(TYPE_DRAFT_HEADER..before_at).ok_or_else(invalid)?)
        .map_err(|error| error.to_string())?;
    let committed = load(bytes.get(before_at..project_at).ok_or_else(invalid)?)?;
    let working = load(bytes.get(project_at..).ok_or_else(invalid)?)?;
    if committed.id != working.id || committed.layer(LayerId(draft.layer)).is_some() == draft.created {
        return Err(invalid());
    }
    draft.text(&working)?;
    Ok((working, Some((committed, draft))))
}

/// The half of an account write owned by the native editor. The host binds it
/// to its account/session and the expected Storage revision before sending bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSaveRequest {
    pub ticket: u64,
    pub document: DocId,
    pub revision: u64,
    pub path: String,
}

pub type AccountSaveFn = Box<dyn FnMut(&AccountSaveRequest, Vec<u8>) -> Result<(), String>>;

pub(super) fn validate_path(path: &str) -> Result<(), String> {
    if path.len() > 2048
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || photocraft_engine::file_cmds::extension(path).as_deref() != Some(photocraft_format::EXTENSION)
    {
        return Err("Choose an account-relative .pcraft project path".into());
    }
    Ok(())
}

struct Pending {
    request: AccountSaveRequest,
    base_path: Option<String>,
    base_revision: u64,
    warnings: Vec<String>,
}

#[derive(Default)]
struct RecoveryRevision {
    value: u64,
    active: Option<DocId>,
    tabs: Vec<(DocId, u64, bool)>,
}

impl RecoveryRevision {
    fn observe(&mut self, session: &photocraft_engine::Session) -> u64 {
        let active = session.active().map(|tab| tab.doc.id);
        let tabs = || session.documents().iter().map(|tab| (tab.doc.id, tab.revision, tab.is_dirty()));
        if self.active != active || !self.tabs.iter().copied().eq(tabs()) {
            self.value += 1;
            self.active = active;
            self.tabs.clear();
            self.tabs.extend(tabs());
        }
        self.value
    }
}

#[derive(Default)]
pub(crate) struct AccountSaveState {
    last_ticket: u64,
    pending: Option<Pending>,
    pub(super) document: Option<DocId>,
    recovery_revision: RefCell<RecoveryRevision>,
    pub(crate) editor_context: Option<egui::Context>,
    pub(crate) type_editor_id: Option<egui::Id>,
    pub(crate) composing_type_session: Option<String>,
    pub(crate) close_approved: bool,
}

impl AccountSaveState {
    pub(crate) fn request(&self) -> Option<&AccountSaveRequest> {
        self.pending.as_ref().map(|p| &p.request)
    }
}

impl PhotocraftApp {
    /// Hosted lifecycle and command checks also cover text fields owned by egui.
    pub fn set_account_editor_context(&mut self, ctx: &egui::Context) {
        self.account_save.editor_context = Some(ctx.clone());
    }

    pub(crate) fn account_type_composing(&self) -> bool {
        self.ui.text_edit.as_ref().is_some_and(|edit| {
            self.account_save.composing_type_session.as_deref() == Some(edit.session.as_str())
        })
    }

    fn account_type_tab(&self, edit: &crate::state::TextEdit) -> Option<DocId> {
        edit.document.map(DocId).or_else(|| self.session.active().map(|tab| tab.doc.id))
    }

    pub fn account_project_state(&self) -> Option<AccountProjectState> {
        if self.services.account_save.is_none() {
            return None;
        }
        let documents = self.session.documents();
        let st = match self.account_save.document {
            Some(id) => documents.iter().find(|state| state.doc.id == id)?,
            None if documents.len() == 1 => self.session.active()?,
            None => return None,
        };
        let multiple = documents.len() > 1;
        let other_edit = self.account_edit_in_progress(false);
        let operation_pending = multiple || self.saving() || !self.session.jobs().is_empty() || self.file_dialog_open() || self.discard.is_some();
        let unfinished_edit = other_edit || self.ui.text_edit.is_some();
        let type_can_recover = self.ui.text_edit.as_ref().is_none_or(|edit| {
            if self.account_type_composing() { return false; }
            let target = self.account_type_tab(edit);
            documents.iter().any(|tab| Some(tab.doc.id) == target
                && tab.coalesce.as_deref() == Some(edit.session.as_str()) && tab.history.can_undo())
        });
        let recovery_revision = self.account_save.recovery_revision.borrow_mut().observe(&self.session);
        let capturable = !other_edit && type_can_recover && (!multiple || nested::can_capture(&self.session, st.doc.id));
        Some(AccountProjectState { document: st.doc.id, revision: st.revision, recovery_revision, dirty: documents.iter().any(|state| state.is_dirty()),
            capturable,
            autosave_eligible: st.path.is_some() && !unfinished_edit && !operation_pending,
            operation_pending, unfinished_edit })
    }

    /// A session pause can cover dirty work only after the host has reviewed
    /// this exact native state. It never commits an unfinished tool transaction.
    pub fn check_account_pause(&self, document: DocId, revision: u64, dirty: bool, unfinished_edit: bool) -> Result<(), String> {
        let state = self.account_project_state().ok_or("PhotoCraft has no single account document to pause.")?;
        if state.document != document || state.revision != revision || state.dirty != dirty || state.unfinished_edit != unfinished_edit {
            return Err("PhotoCraft changed after you reviewed it.".into());
        }
        if state.operation_pending {
            return Err("Finish the pending PhotoCraft operation before ending the session.".into());
        }
        Ok(())
    }

    /// Dispatch a scoped browser host operation against the same editable
    /// document and command implementation as the native Layers panel.
    pub fn execute_account_operation(
        &mut self, document: DocId, path: Option<&str>, command: &str, args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use serde_json::json;
        let state = self.account_project_state().ok_or("The PhotoCraft document is unavailable")?;
        let live = self.session.active().filter(|live| live.doc.id == document && state.document == document && live.path.as_deref() == path)
            .ok_or("The PhotoCraft document binding changed")?;
        let args = args.as_object().ok_or("A PhotoCraft argument object is required")?;
        match command {
            "document.inspect" if args.is_empty() => {
                let entries = live.doc.walk();
                let layers: Vec<_> = entries.iter().take(256).map(|(_,_,layer)| json!({
                    "id":layer.id.0.to_string(), "name":layer.name, "kind":layer.content.kind_name()
                })).collect();
                Ok(json!({"revision":state.revision.to_string(), "dirty":state.dirty,
                    "size":live.doc.size,"layers":layers,"layerCount":entries.len()}))
            }
            "layer.rename" if args.len() == 2 => {
                if state.unfinished_edit || state.operation_pending {
                    return Err("Finish the active PhotoCraft edit or operation first".into());
                }
                let layer = args.get("layerId").and_then(|id| id.as_str()).and_then(|id| id.parse::<u64>().ok())
                    .and_then(|id| live.doc.layer(LayerId(id))).ok_or("Unknown PhotoCraft layer")?;
                let name = args.get("name").and_then(|name| name.as_str())
                    .filter(|name| name.len() <= 256 && !name.chars().any(char::is_control))
                    .map(str::trim).filter(|name| !name.is_empty())
                    .ok_or("Choose a valid PhotoCraft layer name")?;
                let layer = layer.id;
                self.run("layer.setProps", json!({"layer":layer.0,"name":name}))?;
                let revision = self.session.active().ok_or("The PhotoCraft document closed")?.revision;
                Ok(json!({"layerId":layer.0.to_string(),"name":name,"revision":revision.to_string()}))
            }
            _ => Err("Unsupported PhotoCraft document operation".into()),
        }
    }

    /// Capture only the named revision authorized by the host. Use the same
    /// native encoder and pending-save receipt as manual Save, without opening
    /// a dialog, ending a tool gesture or adding a step to a recorded action.
    pub fn begin_account_autosave(&mut self, document: DocId, revision: u64, path: &str) -> Result<bool, String> {
        validate_path(path)?;
        let Some(state) = self.account_project_state() else { return Ok(false) };
        if state.document != document || state.revision != revision || !state.dirty || !state.autosave_eligible
            || !self.session.active().is_some_and(|st| st.path.as_deref() == Some(path))
        {
            return Ok(false);
        }
        let settings = self.save_settings();
        self.write_document(path.to_owned(), &settings, false)?;
        Ok(true)
    }

    /// The user explicitly chose Save while ending the session. An ordinary
    /// pending Type edit can be committed by that choice. Live IME composition
    /// and other unfinished tools must keep their native editing state.
    pub fn begin_account_session_save(&mut self, document: DocId, revision: u64, path: &str) -> Result<bool, String> {
        validate_path(path)?;
        let Some(state) = self.account_project_state() else { return Ok(false) };
        if state.document != document || state.revision != revision || !state.dirty || state.operation_pending
            || !self.session.active().is_some_and(|st| st.path.as_deref().is_none_or(|current| current == path))
        {
            return Ok(false);
        }
        if state.unfinished_edit {
            if !state.capturable || self.ui.text_edit.is_none() {
                return Err("Finish or cancel the active PhotoCraft edit before saving.".into());
            }
            crate::type_tool::commit(self);
        }
        let settings = self.save_settings();
        self.write_document(path.to_owned(), &settings, false)?;
        Ok(true)
    }

    /// Start the initial account view with a native blank or a saved project.
    /// An account path represents a Storage resource and never a local path.
    pub fn start_account_document(&mut self, project: Option<(&str, &[u8])>, new_settings: Option<serde_json::Value>) -> Result<(DocId, u64), String> {
        if self.services.account_save.is_none() || !self.session.documents().is_empty() {
            return Err("Start requires an empty account editor".into());
        }
        if let Some((path, bytes)) = project {
            if new_settings.is_some() {
                return Err("New project settings require an untitled document".into());
            }
            validate_path(path)?;
            let mut doc = photocraft_format::load_from_bytes(bytes).map_err(|error| error.to_string())?;
            let name = crate::file_open::display_name(path);
            doc.name.clone_from(&name);
            let mut warnings = crate::file_open::with_missing_fonts_of(&doc, photocraft_io::open_warnings(&doc));
            warnings.extend(account_external_asset_warning(&doc));
            let status = crate::i18n::fmt(tl!("Opened {name}"), &[("name", &name)]);
            self.session.add_document(doc, Some(path.to_owned()));
            self.sync_views();
            self.push_recent(path);
            self.ui.status.clone_from(&status);
            self.ui.status_error = false;
            crate::notices::io_warnings(self, &status, &warnings);
        } else {
            self.run("file.new", new_settings.unwrap_or_else(|| serde_json::json!({})))?;
            if let Some(index) = self.session.active_index() {
                self.ui.views[index].place(craft::ui::canvas::ViewCommands::FitOnScreen);
            }
        }
        let state = self.session.active().ok_or("PhotoCraft did not open a document")?;
        self.account_save.document = Some(state.doc.id);
        Ok((state.doc.id, state.revision))
    }

    /// Begin an untitled import using the native browser file decoder and its
    /// normal memory, color profile and Camera Raw prompts.
    pub fn start_account_import(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        if self.services.account_save.is_none() || !self.session.documents().is_empty() || self.account_import_pending() {
            return Err("An import requires an empty PhotoCraft account editor".into());
        }
        if crate::preset_files_ui::is_preset_file(name) {
            return Err("Use the PhotoCraft preset import controls for preset files".into());
        }
        self.open_bytes(name, bytes)?;
        Ok(())
    }

    pub(super) fn account_import_pending(&self) -> bool {
        !self.jobs.opens.is_empty() || !self.held_opens.is_empty() || !self.held_memory_prompts.is_empty()
            || self.raw.is_some() || !self.raw_shelf.is_empty() || !self.raw_redevelop.is_empty()
    }

    /// Report only a finished native import. Its library bytes have no account
    /// identity and must stay recoverable until an explicit account Save.
    pub fn finish_account_import(&mut self) -> Result<Option<(DocId, u64)>, String> {
        if self.account_import_pending() {
            return Ok(None);
        }
        if self.session.documents().len() != 1 {
            let reason = if self.ui.status_error { self.ui.status.as_str() } else { "The PhotoCraft import did not open a document" };
            return Err(reason.into());
        }
        let state = self.session.active_mut().ok_or("The PhotoCraft import is unavailable")?;
        state.saved_revision = 0;
        let admitted = (state.doc.id, state.revision);
        self.account_save.document = Some(admitted.0);
        if let Some(warning) = account_external_asset_warning(&state.doc) {
            let status = self.ui.status.clone();
            crate::notices::io_warnings(self, &status, &[warning]);
        }
        Ok(Some(admitted))
    }

    /// Encode a browser-local recovery snapshot without saving the editor or
    /// acknowledging any server write. The caller owns the base Storage revision.
    pub fn capture_account_recovery(&self, document: DocId, revision: u64) -> Result<Vec<u8>, String> {
        let state = self.account_project_state().filter(|st| st.document == document && st.recovery_revision == revision && st.dirty)
            .ok_or("The PhotoCraft recovery revision is no longer current")?;
        if !state.capturable {
            return Err("A PhotoCraft edit is still in progress".into());
        }
        if self.session.documents().len() > 1 {
            return nested::encode(&self.session, document, |tab| self.encode_account_recovery_tab(tab));
        }
        let live = self.session.active().ok_or("PhotoCraft recovery needs one account document")?;
        self.encode_account_recovery_tab(live)
    }

    fn encode_account_recovery_tab(&self, live: &photocraft_engine::DocState) -> Result<Vec<u8>, String> {
        let bytes = photocraft_format::save_to_bytes(&live.doc, &Default::default()).map_err(|error| error.to_string())?;
        let Some(edit) = self.ui.text_edit.as_ref().filter(|edit| self.account_type_tab(edit) == Some(live.doc.id)) else { return Ok(bytes) };
        let before = live.history.clone().undo(live.doc.clone()).map(|pair| pair.0).ok_or("The Type edit baseline is unavailable")?;
        let draft = TypeDraft {layer: edit.layer, anchor: edit.anchor, caret: edit.caret, created: edit.created, mask: edit.mask};
        draft.text(&live.doc)?;
        encode_type_draft(bytes, &before, &draft)
    }

    /// Recover a native snapshot as unsaved work. The browser supplies a path
    /// only when it can keep the exact account binding under a conditional save.
    pub fn start_account_recovery(&mut self, path: Option<&str>, bytes: &[u8]) -> Result<(DocId, u64), String> {
        if self.services.account_save.is_none() || !self.session.documents().is_empty() {
            return Err("Recovery requires an empty account editor".into());
        }
        if let Some(path) = path {
            validate_path(path)?;
        }
        if bytes.starts_with(nested::MAGIC) {
            return self.restore_nested_account_recovery(path, nested::decode(bytes)?);
        }
        let (mut doc, pending_type) = decode_recovery(bytes)?;
        if let Some(path) = path {
            doc.name = crate::file_open::display_name(path);
        }
        let mut warnings = crate::file_open::with_missing_fonts_of(&doc, photocraft_io::open_warnings(&doc));
        warnings.extend(account_external_asset_warning(&doc));
        let index = self.session.add_document(doc, path.map(str::to_owned));
        let state = self.session.active_mut().ok_or("PhotoCraft did not restore the project")?;
        state.saved_revision = 0;
        let admitted = (state.doc.id, state.revision);
        self.account_save.document = Some(admitted.0);
        self.sync_views();
        if let Some((before, draft)) = pending_type {
            let text = draft.text(&self.session.active().ok_or("The recovered project closed")?.doc)?;
            let orientation = text.orientation;
            self.restore_account_type(before, draft, orientation)?;
        }
        self.ui.views[index].place(craft::ui::canvas::ViewCommands::FitOnScreen);
        let status = "Recovered PhotoCraft project from this browser";
        self.ui.status = status.into();
        self.ui.status_error = false;
        crate::notices::io_warnings(self, status, &warnings);
        Ok(admitted)
    }

    fn restore_nested_account_recovery(&mut self, path: Option<&str>, snapshot: nested::Snapshot) -> Result<(DocId, u64), String> {
        let mut ids = HashMap::new();
        let mut warnings = Vec::new();
        let mut pending_type = None;
        for (mut doc, dirty, type_draft) in snapshot.tabs {
            let id = doc.id;
            let is_root = id == snapshot.root;
            let tab_path = path.filter(|_| is_root);
            if let Some(path) = tab_path { doc.name = crate::file_open::display_name(path); }
            warnings.extend(photocraft_io::open_warnings(&doc));
            warnings.extend(account_external_asset_warning(&doc));
            let index = self.session.add_document(doc, tab_path.map(str::to_owned));
            let state = self.session.active_mut().ok_or("PhotoCraft did not restore the project tab")?;
            if dirty || is_root && path.is_none() { state.saved_revision = 0; }
            ids.insert(id, (index, state.doc.id, state.revision));
            if let Some((before, draft)) = type_draft {
                pending_type = Some((index, before, draft));
            }
        }
        for mut link in snapshot.links {
            link.child = ids[&link.child].1;
            link.parent = ids[&link.parent].1;
            self.session.smart_links.push(link);
        }
        self.sync_views();
        if let Some((index, before, draft)) = pending_type {
            self.session.set_active(index);
            let text = draft.text(&self.session.active().ok_or("The recovered project tab closed")?.doc)?;
            let orientation = text.orientation;
            self.restore_account_type(before, draft, orientation)?;
        }
        self.session.set_active(ids[&snapshot.active].0);
        let (_, root, revision) = ids[&snapshot.root];
        self.account_save.document = Some(root);
        let status = "Recovered PhotoCraft project and smart-object tabs from this browser";
        self.ui.status = status.into();
        self.ui.status_error = false;
        crate::notices::io_warnings(self, status, &warnings);
        Ok((root, revision))
    }

    fn restore_account_type(&mut self, mut before: Document, draft: TypeDraft, orientation: photocraft_doc::text::Orientation) -> Result<(), String> {
        let layer = LayerId(draft.layer);
        let session = format!("type-{}", self.ui.alloc_id());
        let st = self.session.active_mut().ok_or("The recovered project closed")?;
        before.id = st.doc.id;
        before.name.clone_from(&st.doc.name);
        let previous_layer = if before.layer(layer).is_some() {Some(layer)} else {before.top_layer()};
        st.history.set_current_layers(photocraft_engine::LayerTarget {active: previous_layer, selected: previous_layer.into_iter().collect()});
        st.history.record("Edit Type", Arc::new(before), photocraft_engine::LayerTarget {active: Some(layer), selected: vec![layer]});
        st.active_layer = Some(layer);
        st.selected_layers = vec![layer];
        st.layer_anchor = Some(layer);
        st.coalesce = Some(session.clone());
        self.ui.text_edit = Some(crate::state::TextEdit {
            layer: draft.layer, document: Some(st.doc.id.0), anchor: draft.anchor, caret: draft.caret, session,
            created: draft.created, mask: draft.mask,
        });
        self.ui.tool = match (orientation, draft.mask) {
            (photocraft_doc::text::Orientation::Vertical, true) => crate::state::Tool::VerticalTypeMask,
            (photocraft_doc::text::Orientation::Vertical, false) => crate::state::Tool::VerticalType,
            (_, true) => crate::state::Tool::HorizontalTypeMask,
            _ => crate::state::Tool::Type,
        };
        Ok(())
    }

    pub(super) fn submit_account_save(&mut self, path: String, bytes: Vec<u8>, warnings: Vec<String>) -> Result<(), String> {
        if self.account_save.pending.is_some() {
            return Err("An account save is awaiting a result".into());
        }
        let document = self.session.active().ok_or("No document to save")?;
        let ticket = self.account_save.last_ticket.checked_add(1).ok_or("Account save ticket limit reached")?;
        let request = AccountSaveRequest { ticket, document: document.doc.id, revision: document.revision, path };
        let writer = self.services.account_save.as_mut().ok_or("No account save service")?;
        self.account_save.last_ticket = ticket;
        writer(&request, bytes)?;
        self.account_save.pending = Some(Pending { request, base_path: document.path.clone(), base_revision: document.saved_revision, warnings });
        self.ui.status = "Saving to your account…".into();
        self.ui.status_error = false;
        Ok(())
    }

    /// Complete only the exact request dispatched from this live editor.
    /// Ambiguous outcomes stay with the host until it can verify a result.
    pub fn finish_account_save(&mut self, request: &AccountSaveRequest, result: Result<(), String>, ctx: &egui::Context) -> Result<(), String> {
        if self.account_save.request() != Some(request) {
            return Err("The account save receipt does not match a pending request".into());
        }
        let pending = self.account_save.pending.take().ok_or("No pending account save")?;
        if let Err(error) = result {
            self.ui.status.clone_from(&error);
            self.ui.status_error = true;
            return Err(error);
        }
        let live = self.session.documents().iter().find(|d| d.doc.id == request.document).ok_or("The saved document is no longer open")?;
        // Selection-only changes can advance a clean revision without
        // replacing the document's saved content.
        let clean_selection = request.revision == pending.base_revision && !live.is_dirty();
        if live.path != pending.base_path || (!clean_selection && live.saved_revision != pending.base_revision) {
            return Err("The document's saved binding changed during the account write".into());
        }
        let saved_revision = if clean_selection { live.revision } else { request.revision };
        // Reuse native identity adoption without running local file hooks
        // against a path owned by the account server.
        self.saved_automation(request.document, saved_revision, &request.path, &pending.warnings, false);
        self.push_recent(&request.path);
        discard_ui::saved_document(self, ctx, request.document);
        Ok(())
    }
}
