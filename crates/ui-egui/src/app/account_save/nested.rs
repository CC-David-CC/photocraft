//! Browser recovery of native Edit Contents tabs and their account project.

use std::collections::{HashMap, HashSet};

use photocraft_doc::{DocId, Document, LayerContent, LayerId, SmartContentsId};
use photocraft_engine::{DocState, Session, smart_cmds::SmartLink};
use serde::{Deserialize, Serialize};

pub(super) const MAGIC: &[u8] = b"PCNEST01";
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_INDEX: usize = 64 * 1024;
const INVALID: &str = "Invalid PhotoCraft smart-object recovery snapshot";

#[derive(Serialize, Deserialize)]
struct Tab {
    id: DocId,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct Binding {
    child: DocId,
    parent: DocId,
    layer: LayerId,
    contents: SmartContentsId,
}

#[derive(Serialize, Deserialize)]
struct Index {
    root: DocId,
    active: DocId,
    tabs: Vec<Tab>,
    bindings: Vec<Binding>,
}

pub(super) struct Snapshot {
    pub root: DocId,
    pub active: DocId,
    pub tabs: Vec<(Document, bool, Option<super::RecoveredType>)>,
    pub links: Vec<SmartLink>,
}

fn append(out: &mut Vec<u8>, part: &[u8]) -> Result<(), String> {
    if out.len().saturating_add(4).saturating_add(part.len()) > MAX_BYTES {
        return Err("The PhotoCraft draft exceeds the browser recovery limit".into());
    }
    out.try_reserve_exact(4 + part.len()).map_err(|error| error.to_string())?;
    out.extend_from_slice(&(part.len() as u32).to_le_bytes());
    out.extend_from_slice(part);
    Ok(())
}

fn take<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let (length, rest) = bytes.split_at_checked(4).ok_or(INVALID)?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| INVALID)?) as usize;
    let (part, rest) = rest.split_at_checked(length).ok_or(INVALID)?;
    *bytes = rest;
    Ok(part)
}

pub(super) fn can_capture(session: &Session, root: DocId) -> bool {
    let docs = session.documents();
    if session.smart_links.len() + 1 != docs.len() {
        return false;
    }
    docs.iter().all(|state| state.doc.id == root || session.smart_links.iter().any(|link| link.child == state.doc.id))
}

fn has_contents(doc: &Document, contents: SmartContentsId) -> bool {
    doc.walk().iter().any(|(_, _, layer)| matches!(&layer.content, LayerContent::Smart(smart) if smart.contents_id == contents))
}

pub(super) fn encode(session: &Session, root: DocId, encode_tab: impl Fn(&DocState) -> Result<Vec<u8>, String>) -> Result<Vec<u8>, String> {
    if !can_capture(session, root) || session.smart_links.iter().any(|link| !session.documents().iter().any(|state| {
        state.doc.id == link.parent && has_contents(&state.doc, link.contents_id)
    })) {
        return Err("Finish or close detached smart-object tabs before recovering this project".into());
    }
    let index = Index {
        root,
        active: session.active().ok_or(INVALID)?.doc.id,
        tabs: session.documents().iter().map(|state| Tab {id: state.doc.id, dirty: state.is_dirty()}).collect(),
        bindings: session.smart_links.iter().map(|link| Binding {
            child: link.child, parent: link.parent, layer: link.layer, contents: link.contents_id,
        }).collect(),
    };
    let index = serde_json::to_vec(&index).map_err(|error| error.to_string())?;
    if index.len() > MAX_INDEX {
        return Err("The PhotoCraft draft has too many smart-object tabs".into());
    }
    let mut out = MAGIC.to_vec();
    append(&mut out, &index)?;
    for state in session.documents() {
        super::super::files::ensure_memory_to_save(&state.doc, "PhotoCraft recovery.pcraft")?;
        let part = encode_tab(state)?;
        append(&mut out, &part)?;
    }
    Ok(out)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Snapshot, String> {
    let mut remainder = bytes.strip_prefix(MAGIC).filter(|_| bytes.len() <= MAX_BYTES).ok_or(INVALID)?;
    let header = take(&mut remainder)?;
    if header.len() > MAX_INDEX {
        return Err(INVALID.into());
    }
    let index: Index = serde_json::from_slice(header).map_err(|_| INVALID)?;
    if index.tabs.len() < 2 || index.tabs.len() != index.bindings.len() + 1 || !index.tabs.iter().any(|tab| tab.dirty) {
        return Err(INVALID.into());
    }
    let mut positions = HashMap::new();
    let mut tabs = Vec::new();
    let mut has_type_draft = false;
    for tab in index.tabs {
        let (doc, type_draft) = super::decode_recovery(take(&mut remainder)?)?;
        if doc.id != tab.id || positions.insert(tab.id, tabs.len()).is_some() || type_draft.is_some() && (has_type_draft || !tab.dirty) {
            return Err(INVALID.into());
        }
        has_type_draft |= type_draft.is_some();
        tabs.push((doc, tab.dirty, type_draft));
    }
    if !remainder.is_empty() || !positions.contains_key(&index.root) || !positions.contains_key(&index.active) {
        return Err(INVALID.into());
    }
    let mut parents = HashMap::new();
    let mut targets = HashSet::new();
    let mut links = Vec::new();
    for binding in index.bindings {
        let parent = *positions.get(&binding.parent).ok_or(INVALID)?;
        if binding.child == index.root || !positions.contains_key(&binding.child)
            || parents.insert(binding.child, binding.parent).is_some() || !targets.insert((binding.parent, binding.contents))
            || !has_contents(&tabs[parent].0, binding.contents)
        {
            return Err(INVALID.into());
        }
        links.push(SmartLink {child:binding.child, parent:binding.parent, layer:binding.layer, contents_id:binding.contents});
    }
    for &id in positions.keys() {
        let mut id = id;
        for _ in 0..tabs.len() {
            if id == index.root { break; }
            id = *parents.get(&id).ok_or(INVALID)?;
        }
        if id != index.root { return Err(INVALID.into()); }
    }
    Ok(Snapshot {root:index.root, active:index.active, tabs, links})
}
