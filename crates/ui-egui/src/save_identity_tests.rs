//! Save identity belongs to the editable document, after a successful write only.

use crate::{ExportSettings, PhotocraftApp, Services, menus};
use photocraft_engine::Session;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn app() -> PhotocraftApp {
    let mut app = PhotocraftApp::new(
        Session::new(),
        Services {
            import: Some(Box::new(|name, bytes, opts| {
                photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc, path, _| {
                photocraft_io::export(doc, path, &Default::default()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            write: Some(Box::new(|_, _| Ok(()))),
            automation_write: Some(Box::new(|_, _| Ok(()))),
            ..Default::default()
        },
    );
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-1"})).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    app
}

fn identity(app: &PhotocraftApp) -> (String, Option<String>, u64, u64) {
    let st = app.session.active().unwrap();
    (st.doc.name.clone(), st.path.clone(), st.revision, st.saved_revision)
}

#[test]
fn account_open_adopts_the_decoded_project_once_and_never_opens_invalid_bytes() {
    let source = app();
    let original = &source.session.active().unwrap().doc;
    let bytes = photocraft_format::save_to_bytes(original, &Default::default()).unwrap();
    let mut editor =
        PhotocraftApp::new(Session::new(), Services { account_save: Some(Box::new(|_, _| Ok(()))), ..Default::default() });
    assert!(editor.start_account_document(Some(("Pictures/Broken.pcraft", b"broken")), None).is_err());
    assert!(editor.start_account_document(Some(("../Outside.pcraft", &bytes)), None).is_err());
    assert!(editor.session.documents().is_empty());

    let path = "Pictures/Copy.pcraft";
    let (id, revision) = editor.start_account_document(Some((path, &bytes)), None).unwrap();
    let live = editor.session.active().unwrap();
    assert_eq!(live.path.as_deref(), Some(path));
    assert_eq!(live.doc.id, id);
    assert_eq!(live.saved_revision, revision);
    assert_eq!(live.doc.name, "Copy.pcraft");
    assert_eq!(live.doc.layers.len(), original.layers.len());
    assert_eq!(live.doc.size, original.size);
    assert!(!live.is_dirty());
    assert!(editor.start_account_document(None, None).is_err());
    assert_eq!(editor.session.documents().len(), 1);
}

#[test]
fn account_import_uses_the_native_decoder_without_a_saved_binding() {
    let mut source = app();
    let original = &source.session.active().unwrap().doc;
    let bytes = photocraft_format::save_to_bytes(original, &Default::default()).unwrap();
    let mut editor = PhotocraftApp::new(
        Session::new(),
        Services { import: source.services.import.take(), account_save: Some(Box::new(|_, _| Ok(()))), ..Default::default() },
    );
    assert!(editor.start_account_import("Broken.png", b"broken").is_err());
    assert!(editor.session.documents().is_empty());
    editor.start_account_import("Local.pcraft", &bytes).unwrap();
    let (id, revision) = editor.finish_account_import().unwrap().unwrap();
    let live = editor.session.active().unwrap();
    assert_eq!(live.path, None);
    assert_eq!(live.doc.size, original.size);
    assert_eq!(live.doc.layers.len(), original.layers.len());
    assert!(live.is_dirty());
    assert!(editor.capture_account_recovery(id, revision).is_ok());
    assert!(editor.open_bytes("Second.pcraft", &bytes).is_err());
    assert_eq!(editor.session.documents().len(), 1);
    assert_eq!(editor.session.active().unwrap().doc.id, id);
}

#[test]
fn account_operation_uses_the_native_layer_identity_and_protects_active_type() {
    let mut editor = app();
    editor.services.account_save = Some(Box::new(|_, _| Ok(())));
    let live = editor.session.active().unwrap();
    let document = live.doc.id;
    let layer = live.active_layer.unwrap();
    let state = editor.execute_account_operation(document, None, "document.inspect", json!({})).unwrap();
    assert!(state["layers"].as_array().unwrap().iter().any(|entry| entry["id"] == layer.0.to_string()));
    let rename = json!({"layerId":layer.0.to_string(),"name":"Approved rename"});
    assert!(editor.execute_account_operation(document, Some("Pictures/Other.pcraft"), "layer.rename", rename.clone()).is_err());
    assert!(editor.execute_account_operation(document, None, "layer.delete", rename.clone()).is_err());
    editor.execute_account_operation(document, None, "layer.rename", rename.clone()).unwrap();
    assert_eq!(editor.session.active().unwrap().doc.layer(layer).unwrap().name,"Approved rename");

    editor.run("type.create", json!({"text":"Human edit","x":1,"y":1})).unwrap();
    crate::type_tool::edit_active(&mut editor).unwrap();
    let changed = json!({"layerId":layer.0.to_string(),"name":"Blocked rename"});
    assert!(editor.execute_account_operation(document, None, "layer.rename", changed).unwrap_err().contains("Finish the active"));
    assert_eq!(editor.session.active().unwrap().doc.layer(layer).unwrap().name,"Approved rename");
}

#[test]
fn account_open_creates_a_clean_untitled_document_through_file_new() {
    let mut editor =
        PhotocraftApp::new(Session::new(), Services { account_save: Some(Box::new(|_, _| Ok(()))), ..Default::default() });
    let (id, revision) = editor.start_account_document(None, Some(json!({"width": 32, "height": 24}))).unwrap();
    let live = editor.session.active().unwrap();
    assert_eq!(live.doc.id, id);
    assert_eq!(live.revision, revision);
    assert!(live.path.is_none());
    assert_eq!(live.doc.size, photocraft_geom::Size::new(32, 24));
    assert!(!editor.has_unsaved_work());
}

#[test]
fn account_recovery_round_trips_layers_as_unsaved_without_changing_the_live_editor() {
    let mut source = app();
    source.services.account_save = Some(Box::new(|_, _| panic!("recovery must not save to the account")));
    let before = identity(&source);
    let live = source.session.active().unwrap();
    let (document, revision, size, layers) = (live.doc.id, live.revision, live.doc.size, live.doc.layers.len());
    assert!(source.capture_account_recovery(document, revision - 1).is_err());
    let bytes = source.capture_account_recovery(document, revision).unwrap();
    assert_eq!(identity(&source), before);

    for path in [None, Some("Pictures/Layered.pcraft")] {
        let mut recovered =
            PhotocraftApp::new(Session::new(), Services { account_save: Some(Box::new(|_, _| Ok(()))), ..Default::default() });
        assert!(recovered.start_account_recovery(path, b"invalid").is_err());
        assert!(recovered.start_account_recovery(Some("../wrong.pcraft"), &bytes).is_err());
        assert!(recovered.session.documents().is_empty());
        let admitted = recovered.start_account_recovery(path, &bytes).unwrap();
        let active = recovered.session.active().unwrap();
        assert_eq!(admitted, (active.doc.id, active.revision));
        assert_eq!(active.doc.size, size);
        assert_eq!(active.doc.layers.len(), layers);
        assert_eq!(active.path.as_deref(), path);
        assert!(active.is_dirty());
        assert!(recovered.start_account_recovery(None, &bytes).is_err());
    }
}

#[test]
fn account_recovery_preserves_the_previous_snapshot_during_a_live_type_edit() {
    let mut source = app();
    source.services.account_save = Some(Box::new(|_, _| Ok(())));
    let state = source.session.active().unwrap();
    let (document, revision) = (state.doc.id, state.revision);
    source.ui.text_edit = Some(crate::state::TextEdit {
        layer: state.active_layer.unwrap().0,
        document: Some(document.0),
        caret: 0,
        anchor: 0,
        session: "pending-type".into(),
        created: false,
        mask: false,
        dragging: false,
        resize: None,
        preedit: Some((0, 1)),
    });
    assert!(source.capture_account_recovery(document, revision).is_err());
    assert_eq!(source.ui.text_edit.as_ref().unwrap().preedit, Some((0, 1)));
    assert_eq!(source.session.active().unwrap().revision, revision);
}

#[test]
fn account_recovery_restores_pending_type_and_its_native_cancel_baseline() {
    let mut source = app();
    source.services.account_save = Some(Box::new(|_, _| Ok(())));
    let layer = source.run("type.create", json!({"text":"Before","x":1,"y":2})).unwrap()["layer"].as_u64().unwrap();
    crate::type_tool::edit_active(&mut source).unwrap();
    let key = source.ui.text_edit.as_ref().unwrap().session.clone();
    source.run("type.edit", json!({"layer":layer,"text":"Pending","coalesce":key})).unwrap();
    let state = source.account_project_state().unwrap();
    assert!(state.dirty && state.capturable);
    let bytes = source.capture_account_recovery(state.document, state.revision).unwrap();
    assert_eq!(source.account_project_state(), Some(state));
    assert!(source.ui.text_edit.is_some());

    let mut restored = PhotocraftApp::new(Session::new(), Services {
        account_save: Some(Box::new(|_, _| Ok(()))), ..Default::default()
    });
    restored.start_account_recovery(None, &bytes).unwrap();
    assert_eq!(restored.ui.text_edit.as_ref().unwrap().layer, layer);
    let text = |app: &PhotocraftApp| {
        let doc = &app.session.active().unwrap().doc;
        let photocraft_doc::LayerContent::Text(text) = &doc.layer(photocraft_doc::LayerId(layer)).unwrap().content else {
            panic!("Expected editable text layer");
        };
        text.text.clone()
    };
    assert_eq!(text(&restored), "Pending");
    crate::type_tool::cancel(&mut restored);
    assert_eq!(text(&restored), "Before");
    assert!(restored.ui.text_edit.is_none());
    assert_eq!(text(&source), "Pending");
}

#[test]
fn account_autosave_captures_only_the_exact_named_edit_without_a_dialog_or_action_step() {
    let mut source = app();
    let requests = Rc::new(RefCell::new(Vec::new()));
    let writes = requests.clone();
    source.services.account_save = Some(Box::new(move |request, _| {
        writes.borrow_mut().push(request.clone());
        Ok(())
    }));
    let path = "Pictures/Layered.pcraft";
    let state = source.account_project_state().unwrap();
    assert!(!source.begin_account_autosave(state.document, state.revision, path).unwrap(), "an untitled document needs an explicit Save");
    source.session.active_mut().unwrap().path = Some(path.into());
    let layer = source.run("type.create", json!({"text":"Before","x":1,"y":2})).unwrap()["layer"].as_u64().unwrap();
    crate::type_tool::edit_active(&mut source).unwrap();
    let key = source.ui.text_edit.as_ref().unwrap().session.clone();
    source.run("type.edit", json!({"layer":layer,"text":"Pending","coalesce":key})).unwrap();
    let typing = source.account_project_state().unwrap();
    assert!(typing.capturable && !typing.autosave_eligible);
    assert!(!source.begin_account_autosave(typing.document, typing.revision, path).unwrap());
    assert!(source.ui.text_edit.is_some());
    crate::type_tool::commit(&mut source);
    let current = source.account_project_state().unwrap();
    let journal = source.session.journal.len();
    assert!(!source.begin_account_autosave(current.document, current.revision - 1, path).unwrap());
    assert!(!source.begin_account_autosave(current.document, current.revision, "Pictures/Other.pcraft").unwrap());
    assert!(source.begin_account_autosave(current.document, current.revision, path).unwrap());
    assert!(!source.file_dialog_open());
    assert_eq!(source.session.journal.len(), journal);
    let request = requests.borrow().first().unwrap().clone();
    assert_eq!((request.document, request.revision, request.path.as_str()), (current.document, current.revision, path));
    assert!(source.session.active().unwrap().is_dirty());
    assert!(!source.begin_account_autosave(current.document, current.revision, path).unwrap());
    assert_eq!(requests.borrow().len(), 1);
    source.finish_account_save(&request, Ok(()), &egui::Context::default()).unwrap();
    assert!(!source.session.active().unwrap().is_dirty());
}

#[test]
fn account_session_pause_checks_the_reviewed_revision_and_pending_native_work() {
    let mut source = app();
    source.services.account_save = Some(Box::new(|_, _| Ok(())));
    let current = source.account_project_state().unwrap();
    let check = |app: &PhotocraftApp, state:crate::AccountProjectState| {
        app.check_account_pause(state.document, state.revision, state.dirty, state.unfinished_edit)
    };
    assert!(check(&source,current).is_ok(), "the exact reviewed dirty revision can be held");
    assert!(check(&source,crate::AccountProjectState {dirty:false,..current}).is_err());
    source.run("layer.new.layer", json!({})).unwrap();
    assert!(check(&source,current).is_err(), "a later edit invalidates the review");
    let path = "Pictures/Layered.pcraft";
    source.session.active_mut().unwrap().path = Some(path.into());
    let current = source.account_project_state().unwrap();
    assert!(source.begin_account_autosave(current.document,current.revision,path).unwrap());
    assert!(check(&source,current).is_err(), "a pending save cannot be frozen");
}

#[test]
fn account_session_save_requires_review_and_can_commit_an_ordinary_type_edit() {
    let mut source = app();
    let request = Rc::new(RefCell::new(None));
    let received = request.clone();
    source.services.account_save = Some(Box::new(move |ticket, _| {
        *received.borrow_mut() = Some(ticket.clone());
        Ok(())
    }));
    let layer = source.run("type.create", json!({"text":"Before","x":1,"y":2})).unwrap()["layer"].as_u64().unwrap();
    crate::type_tool::edit_active(&mut source).unwrap();
    let key = source.ui.text_edit.as_ref().unwrap().session.clone();
    source.run("type.edit",json!({"layer":layer,"text":"Reviewed","coalesce":key})).unwrap();
    let current = source.account_project_state().unwrap();
    let path = "Pictures/Reviewed.pcraft";
    assert!(!source.begin_account_session_save(current.document,current.revision - 1,path).unwrap());
    assert!(source.ui.text_edit.is_some());
    assert!(source.begin_account_session_save(current.document,current.revision,path).unwrap());
    assert!(source.ui.text_edit.is_none());
    assert!(source.session.active().unwrap().is_dirty());
    let save = request.borrow().clone().unwrap();
    assert_eq!(save.document,current.document);
    assert_eq!(save.path,path);
    source.finish_account_save(&save,Ok(()),&egui::Context::default()).unwrap();
    assert!(!source.session.active().unwrap().is_dirty());
}

#[test]
fn account_save_captures_real_layered_bytes_and_adopts_only_the_confirmed_revision() {
    let mut app = app();
    let out = Rc::new(RefCell::new(None));
    let receipt = out.clone();
    app.services.account_save = Some(Box::new(move |request, bytes| {
        *receipt.borrow_mut() = Some((request.clone(), bytes));
        Ok(())
    }));
    let ctx = egui::Context::default();
    let path = "Projects/Layered.pcraft";
    let initial = identity(&app);
    let value = menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path":path})).unwrap();
    assert_eq!(value["pending"], true);
    assert_eq!(identity(&app), initial);
    let (request, bytes) = out.borrow_mut().take().unwrap();
    let decoded = photocraft_io::import("Layered.pcraft", &bytes).unwrap();
    let captured = app.session.active().unwrap();
    assert_eq!(decoded.document.layers.len(), captured.doc.layers.len());
    assert_eq!(decoded.document.size, captured.doc.size);
    assert!(app.save_as(Some(path.into())).is_err());
    let mut foreign = request.clone();
    foreign.ticket += 1;
    assert!(app.finish_account_save(&foreign, Ok(()), &ctx).is_err());
    let mut other_path = request.clone();
    other_path.path = "Projects/Other.pcraft".into();
    assert!(app.finish_account_save(&other_path, Ok(()), &ctx).is_err());

    app.run("layer.new.layer", json!({})).unwrap();
    app.finish_account_save(&request, Ok(()), &ctx).unwrap();
    let saved = app.session.active().unwrap();
    assert_eq!(saved.doc.id, request.document);
    assert_eq!(saved.path.as_deref(), Some(path));
    assert_eq!(saved.doc.name, "Layered.pcraft");
    assert_eq!(saved.saved_revision, request.revision);
    assert!(saved.is_dirty());
    assert_eq!(saved.doc.layers.len(), decoded.document.layers.len() + 1);
    assert!(app.finish_account_save(&request, Ok(()), &ctx).is_err());
}

#[test]
fn failed_account_save_retains_the_committed_binding_and_allows_a_new_ticket() {
    let mut app = app();
    app.save_as(Some("Projects/Original.pcraft".into())).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    let before = identity(&app);
    let out = Rc::new(RefCell::new(None));
    let receipt = out.clone();
    app.services.account_save = Some(Box::new(move |request, _| {
        *receipt.borrow_mut() = Some(request.clone());
        Ok(())
    }));
    let ctx = egui::Context::default();
    assert!(app.save_as(Some("../Outside.pcraft".into())).is_err());
    assert!(app.save_as(Some("Projects/Flat.psd".into())).is_err());
    assert!(out.borrow().is_none());
    let path = Some("Projects/Recovered.pcraft".into());
    app.save_as(path.clone()).unwrap();
    let failed = out.borrow().clone().unwrap();
    assert!(app.finish_account_save(&failed, Err("Write conflict".into()), &ctx).is_err());
    assert_eq!(identity(&app), before);
    app.save_as(path).unwrap();
    let retry = out.borrow().clone().unwrap();
    assert_ne!(retry.ticket, failed.ticket);
    assert!(app.finish_account_save(&failed, Ok(()), &ctx).is_err());
    app.finish_account_save(&retry, Ok(()), &ctx).unwrap();
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some("Projects/Recovered.pcraft"));
}

#[test]
fn save_settings_share_only_the_open_documents_native_cache() {
    let mut app = app();
    let first = app.save_settings().io_options().pcraft_cache.unwrap();
    assert_eq!(first, app.session.active().unwrap().pcraft_cache);
    assert_eq!(first, app.save_settings().io_options().pcraft_cache.unwrap());
    app.run("file.new", json!({"width": 2, "height": 2})).unwrap();
    assert_ne!(first, app.save_settings().io_options().pcraft_cache.unwrap());
}

#[test]
fn new_document_psd_and_subsequent_save_as_adopt_the_written_file_name() {
    for depth in [8, 16, 32] {
        let mut app = app();
        app.run("file.new", json!({"width": 4, "height": 4, "depth": depth, "name": "Untitled-1"})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        let bytes = Rc::new(RefCell::new(Vec::new()));
        let written = bytes.clone();
        app.services.write = Some(Box::new(move |_, data| {
            *written.borrow_mut() = data.to_vec();
            Ok(())
        }));
        let id = app.session.active().unwrap().doc.id;
        let revision = app.session.active().unwrap().revision;
        let ctx = egui::Context::default();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "作品/My image.PSD"})).unwrap();
        assert_eq!(identity(&app), ("My image.PSD".into(), Some("作品/My image.PSD".into()), revision, revision));
        assert_eq!(app.session.active().unwrap().doc.id, id);
        let reopened = photocraft_io::import("My image.PSD", &bytes.borrow()).unwrap();
        assert_eq!(reopened.document.size, app.session.active().unwrap().doc.size);
        assert_eq!(reopened.document.depth.bits(), depth);
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "作品/次の画像.psd"})).unwrap();
        assert_eq!(app.session.active().unwrap().doc.name, "次の画像.psd");
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        assert_eq!(app.session.active().unwrap().path.as_deref(), Some("作品/次の画像.psd"));
        app.open_bytes("existing.psd", &bytes.borrow()).unwrap();
        let id = app.active_doc_id().unwrap();
        assert_eq!(app.session.active().unwrap().doc.name, "existing.psd");
        app.save_as(Some("renamed existing.psd".into())).unwrap();
        assert_eq!(app.active_doc_id().unwrap(), id);
        assert_eq!(app.session.active().unwrap().doc.name, "renamed existing.psd");
    }
}

#[test]
fn failed_encode_or_write_keeps_the_previous_name_path_and_dirty_state() {
    for saved_before in [false, true] {
        let mut app = app();
        if saved_before {
            app.save_as(Some("before.psd".into())).unwrap();
            app.run("layer.new.layer", json!({})).unwrap();
        }
        let before = identity(&app);
        app.services.write = Some(Box::new(|_, _| Err("disk full".into())));
        assert!(app.save_as(Some("after.psd".into())).is_err());
        assert_eq!(identity(&app), before);
        assert!(app.save_automation(Some("after.psd".into())).is_ok());
        app.run("layer.new.layer", json!({})).unwrap();
        let before = identity(&app);
        app.services.automation_write = Some(Box::new(|_, _| Err("denied".into())));
        assert!(app.save_automation(Some("another.psd".into())).is_err());
        assert_eq!(identity(&app), before);
        app.services.export = Some(Box::new(|_, _, _| Err("cannot encode".into())));
        assert!(app.save_as(Some("after.psd".into())).is_err());
        assert_eq!(identity(&app), before);
    }
}

#[test]
fn save_copy_and_export_keep_the_editable_document_identity() {
    let mut app = app();
    app.save_as(Some("original.psd".into())).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    let before = identity(&app);
    app.write_document("copy.psd".into(), &ExportSettings::default(), true).unwrap();
    assert_eq!(identity(&app), before);
    let (dialog, _) = crate::file_dialog::fake(vec![Some(crate::file_dialog::FileDialogAnswer::SaveTo("export.png".into()))]);
    app.services.file_dialog = Some(dialog);
    let ctx = egui::Context::default();
    crate::export_dialog::confirm(&mut app, json!({"format": "png", "scale": 100}).as_object().unwrap()).unwrap();
    app.poll_file_dialog(&ctx, None);
    assert_eq!(identity(&app), before);
    assert!(app.ui.status.starts_with("Exported export.png"), "{}", app.ui.status);
}

#[test]
fn web_download_and_automation_saves_use_the_same_identity_completion() {
    let mut app = app();
    let (dialog, _) = crate::file_dialog::fake(vec![Some(crate::file_dialog::FileDialogAnswer::SaveTo("Untitled-1.psd".into()))]);
    app.services.file_dialog = Some(dialog);
    app.save_as(None).unwrap();
    app.poll_file_dialog(&egui::Context::default(), None);
    assert_eq!(app.session.active().unwrap().doc.name, "Untitled-1.psd");
    app.save_automation(Some("保存/agent save.psd".into())).unwrap();
    assert_eq!(app.session.active().unwrap().doc.name, "agent save.psd");
}

/// Save As to a flat format writes a copy when the flat file can't hold the document: its layers,
/// or the layered file it already lives in. The document keeps its file, so Save still writes the
/// layered original and the edits stay unsaved (#2550).
#[test]
fn save_as_flat_keeps_the_layered_original() {
    let ctx = egui::Context::default();
    // A layered document, and a one-layer one (the issue's case: the dialog offered a JPEG).
    for layered in [true, false] {
        let mut app = app();
        if !layered {
            app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-2"})).unwrap();
        }
        let written = Rc::new(RefCell::new(Vec::new()));
        let w = written.clone();
        app.services.write = Some(Box::new(move |path, _| {
            w.borrow_mut().push(path.to_string());
            Ok(())
        }));
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "art.psd"})).unwrap();
        app.run("edit.fill", json!({"color": "#000000"})).unwrap();
        let before = identity(&app);
        for flat in ["art.jpg", "art.png"] {
            // A JPEG Options answer, so the JPEG doesn't wait on the prompt.
            menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": flat, "jpegFormat": "baseline"})).unwrap();
            assert_eq!(identity(&app), before, "{flat}, layered: {layered}");
        }
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        assert_eq!(*written.borrow(), ["art.psd", "art.jpg", "art.png", "art.psd"]);
        let st = app.session.active().unwrap();
        assert_eq!(st.saved_revision, st.revision);
    }
    // An untitled layered document saved as a JPEG is still untitled and unsaved.
    let mut app = app();
    let written = Rc::new(RefCell::new(Vec::new()));
    let w = written.clone();
    app.services.write = Some(Box::new(move |path, _| {
        w.borrow_mut().push(path.to_string());
        Ok(())
    }));
    let before = identity(&app);
    menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": "flat.jpg", "jpegFormat": "baseline"})).unwrap();
    assert_eq!(*written.borrow(), ["flat.jpg"]);
    assert_eq!(identity(&app), before);
}

/// A flat document that isn't in a layered file becomes the flat file it is saved as, as before.
#[test]
fn save_as_flat_retargets_a_flat_document() {
    let mut app = app();
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-2"})).unwrap();
    app.save_as(Some("shot.png".into())).unwrap();
    assert_eq!(app.session.active().unwrap().path.as_deref(), Some("shot.png"));
    app.save_as(Some("shot.jpg".into())).unwrap();
    // JPEG Options asks first; OK writes.
    crate::jpeg_options_ui::confirm(&mut app, &egui::Context::default());
    let st = app.session.active().unwrap();
    assert_eq!((st.doc.name.as_str(), st.path.as_deref()), ("shot.jpg", Some("shot.jpg")));
    assert_eq!(st.saved_revision, st.revision);
}

/// `app.save` (control channel, MCP bridge) to a flat format is a copy, as in the headless server
/// (#1547); only a layered format becomes the document's file (#2579).
#[test]
fn automation_flat_save_is_a_copy() {
    let mut app = app();
    let before = identity(&app);
    for flat in ["flat.png", "flat.jpg", "flat.tif"] {
        app.save_automation(Some(flat.into())).unwrap();
        assert_eq!(identity(&app), before, "{flat}");
    }
    app.save_automation(Some("layers.psd".into())).unwrap();
    let st = app.session.active().unwrap();
    assert_eq!((st.doc.name.as_str(), st.path.as_deref()), ("layers.psd", Some("layers.psd")));
    assert_eq!(st.saved_revision, st.revision);
}

/// Preferences › File Handling › Image Previews reaches the exporter of Save As, Save and
/// automation saves: Always and Ask When Saving embed the PSD thumbnail, Never doesn't.
#[test]
fn image_previews_preference_reaches_the_exporter() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut app = app();
    let log = seen.clone();
    app.services.export = Some(Box::new(move |_, _, settings: &ExportSettings| {
        log.borrow_mut().push(settings.image_previews);
        Ok((b"out".to_vec(), Vec::new()))
    }));
    let ctx = egui::Context::default();
    for (value, expect) in [("always", true), ("never", false), ("ask", true)] {
        app.run("prefs.set", json!({"path": "fileHandling.imagePreviews", "value": value})).unwrap();
        seen.borrow_mut().clear();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": format!("{value}.psd")})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.save_automation(Some(format!("{value} agent.psd"))).unwrap();
        assert_eq!(*seen.borrow(), [expect; 3], "{value}");
    }
}

/// Preferences › File Handling › Maximize PSD and PSB File Compatibility reaches the exporter of
/// Save As, Save and automation saves: Always and Ask When Saving keep the PSD's real merged
/// image, Never doesn't.
#[test]
fn maximize_compatibility_preference_reaches_the_exporter() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut app = app();
    let log = seen.clone();
    app.services.export = Some(Box::new(move |_, _, settings: &ExportSettings| {
        log.borrow_mut().push(settings.psd_maximize_compatibility);
        Ok((b"out".to_vec(), Vec::new()))
    }));
    let ctx = egui::Context::default();
    for (value, expect) in [("always", true), ("never", false), ("ask", true)] {
        app.run("prefs.set", json!({"path": "fileHandling.maximizePsdCompatibility", "value": value})).unwrap();
        seen.borrow_mut().clear();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": format!("{value}.psd")})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.save_automation(Some(format!("{value} agent.psd"))).unwrap();
        assert_eq!(*seen.borrow(), [expect; 3], "{value}");
    }
}

/// Preferences › File Handling › Save Full-Size Composite in .pcraft Files reaches the exporter
/// of Save As, Save and automation saves, and is off by default.
#[test]
fn full_composite_preference_reaches_the_exporter() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut app = app();
    assert!(!app.save_settings().pcraft_full_composite, "off by default");
    let log = seen.clone();
    app.services.export = Some(Box::new(move |_, _, settings: &ExportSettings| {
        log.borrow_mut().push(settings.io_options().pcraft_full_composite);
        Ok((b"out".to_vec(), Vec::new()))
    }));
    let ctx = egui::Context::default();
    for value in [true, false] {
        app.run("prefs.set", json!({"path": "fileHandling.pcraftFullComposite", "value": value})).unwrap();
        seen.borrow_mut().clear();
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({"path": format!("{value}.pcraft")})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.save_automation(Some(format!("{value} agent.pcraft"))).unwrap();
        assert_eq!(*seen.borrow(), [value; 3], "{value}");
    }
}

/// #2399: Save and Save As start at the opened file's own encoding: a JPEG's estimated quality,
/// a WebP lossy (at its quality) or lossless; a new document keeps the codec defaults.
#[test]
fn save_settings_start_at_the_opened_files_encoding() {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
    let px: Vec<u8> = (0..32u32 * 24).flat_map(|i| [(i * 3) as u8, (i / 32 * 9) as u8, (i % 97) as u8]).collect();
    let img = Image::from_raw(32, 24, ChannelLayout::Rgb, SampleType::U8, px).unwrap();
    let mut app = app();
    assert_eq!(app.save_settings().jpeg_quality, None);
    assert!(app.save_settings().webp_lossless);

    let jpeg = photocraft_codecs::encode(&img, Format::Jpeg, &EncodeOptions { jpeg_quality: 65, ..Default::default() }).unwrap();
    app.open_bytes("photo.jpg", &jpeg).unwrap();
    let q = app.save_settings().jpeg_quality.unwrap();
    assert!(q.abs_diff(65) <= 2, "estimated {q}");

    let lossy = photocraft_codecs::encode(&img, Format::WebP, &EncodeOptions { webp_lossless: false, webp_quality: 80, ..Default::default() }).unwrap();
    app.open_bytes("photo.webp", &lossy).unwrap();
    let s = app.save_settings();
    assert!(!s.webp_lossless);
    assert!(s.webp_quality.unwrap().abs_diff(80) <= 2, "{:?}", s.webp_quality);

    let lossless = photocraft_codecs::encode(&img, Format::WebP, &EncodeOptions::default()).unwrap();
    app.open_bytes("art.webp", &lossless).unwrap();
    assert!(app.save_settings().webp_lossless);
}

/// The names of the active document's History panel snapshots.
fn snapshots(app: &PhotocraftApp) -> Vec<String> {
    app.session.active().unwrap().snapshots.iter().map(|s| s.name.clone()).collect()
}

/// History Options › Automatically Create New Snapshot When Saving (#2966) through the desktop
/// save path: a save adds one snapshot, saving again unchanged adds none, Save a Copy adds none.
#[test]
fn a_save_adds_one_snapshot_and_a_copy_none() {
    let mut app = app();
    app.run("history.options", json!({"automaticallyCreateNewSnapshotWhenSaving": true})).unwrap();
    let before = snapshots(&app).len();
    app.save_as(Some("saved.psd".into())).unwrap();
    assert_eq!(snapshots(&app).len(), before + 1, "the save adds a snapshot");
    let st = app.session.active().unwrap();
    assert!(std::sync::Arc::ptr_eq(&st.snapshots.last().unwrap().doc, &st.doc), "of the document as saved");
    app.save_as(Some("saved.psd".into())).unwrap();
    assert_eq!(snapshots(&app).len(), before + 1, "saving an unchanged document adds no duplicate");

    app.run("layer.new.layer", json!({})).unwrap();
    app.write_document("copy.psd".into(), &ExportSettings::default(), true).unwrap();
    assert_eq!(snapshots(&app).len(), before + 1, "Save a Copy adds none");
    app.save_as(Some("saved.psd".into())).unwrap();
    assert_eq!(snapshots(&app).len(), before + 2);
}

/// A background save that finishes while another document is active snapshots the document it
/// saved, not the active one.
#[test]
fn a_background_save_snapshots_its_own_document() {
    let mut app = app();
    app.run("history.options", json!({"automaticallyCreateNewSnapshotWhenSaving": true})).unwrap();
    let (saved_doc, revision) = {
        let st = app.session.active().unwrap();
        (st.doc.id, st.revision)
    };
    let saved_snaps = snapshots(&app).len();
    app.run("file.new", json!({"width": 4, "height": 4, "name": "Untitled-2"})).unwrap();
    let active = app.active_doc_id().unwrap();
    assert_ne!(active, saved_doc);
    let active_snaps = snapshots(&app).len();
    // What jobs_ui does when the save job of `saved_doc` ends.
    app.saved(saved_doc, revision, "background.psd", &[], false);
    assert_eq!(app.active_doc_id(), Ok(active), "the active document stays active");
    assert_eq!(snapshots(&app).len(), active_snaps, "the active document gets no snapshot");
    let st = app.session.documents().iter().find(|st| st.doc.id == saved_doc).unwrap();
    assert_eq!(st.snapshots.len(), saved_snaps + 1, "the saved document gets one");
    assert_eq!(st.path.as_deref(), Some("background.psd"));
}

/// A browser download is only requested: the document takes the name but keeps its unsaved
/// changes, so the leave-page guard stays armed (#4962). A confirmed write still saves it.
#[test]
fn unconfirmed_download_names_the_document_but_keeps_it_unsaved() {
    assert_eq!(crate::saved_revision_after_write(true, 7, 3), 7);
    assert_eq!(crate::saved_revision_after_write(false, 7, 3), 3);
    let mut app = app();
    app.services.write_unconfirmed = true;
    let (_, _, revision, saved) = identity(&app);
    app.save_as(Some("download.psd".into())).unwrap();
    assert_eq!(identity(&app), ("download.psd".into(), Some("download.psd".into()), revision, saved));
    assert!(app.session.active().unwrap().is_dirty());
    assert!(app.has_unsaved_work());
    app.services.write_unconfirmed = false;
    app.save_as(Some("download.psd".into())).unwrap();
    assert!(!app.has_unsaved_work());
}
