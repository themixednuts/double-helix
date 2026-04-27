use helix_view::bench::log_command_phase;
use helix_view::handlers::Handlers;

pub(super) fn attach(editor: &helix_view::Editor, _handlers: &Handlers) {
    editor.lifecycle().on_selection_change(move |event| {
        if let Some(snippet) = event.doc.active_snippet() {
            if !snippet.is_valid(event.doc.selection(event.view)) {
                event.doc.clear_active_snippet();
            }
        }
        Ok(())
    });
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        if let Some(snippet) = event.doc.active_snippet_mut() {
            let invalid = snippet.map(event.changes);
            if invalid {
                event.doc.clear_active_snippet();
            }
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase("document_did_change_hook", "snippet", hook_dur, || {
            format!(
                "doc_id={:?} has_active_snippet={} lines={} bytes={}",
                event.doc.id(),
                event.doc.active_snippet().is_some(),
                event.doc.text().len_lines(),
                event.doc.text().len_bytes()
            )
        });
        Ok(())
    });
    editor.lifecycle().on_document_focus_lost(move |event| {
        if let Some(doc) = event.editor.document_mut(event.doc) {
            doc.clear_active_snippet();
        }
        Ok(())
    });
}
