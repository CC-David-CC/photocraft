//! Account writes settle separately from encoding and transport dispatch.

use photocraft_doc::DocId;

use crate::{PhotocraftApp, dialogs::discard_ui};

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
pub(crate) struct AccountSaveState {
    last_ticket: u64,
    pending: Option<Pending>,
}

impl AccountSaveState {
    pub(crate) fn request(&self) -> Option<&AccountSaveRequest> {
        self.pending.as_ref().map(|p| &p.request)
    }
}

impl PhotocraftApp {
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
            let warnings = crate::file_open::with_missing_fonts_of(&doc, photocraft_io::open_warnings(&doc));
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
                self.ui.views[index].fit_pending = true;
            }
        }
        let state = self.session.active().ok_or("PhotoCraft did not open a document")?;
        Ok((state.doc.id, state.revision))
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
