use crate::runtime::RuntimeIngress;

fn submit(doc: &helix_view::Document, ingress: &RuntimeIngress) {
    let Some(request) = doc.prepare_syntax_refresh() else {
        return;
    };
    if let Err(error) = ingress.syntax_refresh(request) {
        log::warn!(
            "[syntax_service] admission_failed document={:?} version={} error={error}",
            doc.id(),
            doc.version(),
        );
    }
}

pub(super) fn attach(editor: &helix_view::Editor, ingress: RuntimeIngress) {
    let changed = ingress.clone();
    editor.lifecycle().on_document_change(move |event| {
        submit(event.doc, &changed);
        Ok(())
    });

    editor.lifecycle().on_document_open(move |event| {
        if let Some(doc) = event.editor.document(event.doc) {
            submit(doc, &ingress);
        }
        Ok(())
    });
}
