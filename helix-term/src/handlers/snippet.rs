use helix_event::register_hook;
use helix_view::bench::log_command_phase;
use helix_view::events::{DocumentDidChange, DocumentFocusLost, SelectionDidChange};
use helix_view::handlers::Handlers;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if let Some(snippet) = event.doc.active_snippet() {
            if !snippet.is_valid(event.doc.selection(event.view)) {
                event.doc.clear_active_snippet();
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
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
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        let editor = &mut event.editor;
        focused!(editor).1.clear_active_snippet();
        Ok(())
    });
}
