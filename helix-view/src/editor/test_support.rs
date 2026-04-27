use std::{path::PathBuf, sync::Arc};

use arc_swap::ArcSwap;

use crate::{
    collab::{location::Source, Location, ParticipantId, RangeAnchor},
    graphics::Rect,
    theme, Document, Editor, View,
};
use helix_core::syntax;

pub(super) fn collab_test_editor() -> Editor {
    let theme_loader = Arc::new(theme::Loader::new(&[]));
    let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
    let config = Arc::new(ArcSwap::from_pointee(super::Config::default()));
    let tokio = Box::leak(Box::new(tokio::runtime::Runtime::new().expect("runtime")));
    let _guard = tokio.enter();
    let runtime = helix_runtime::Runtime::new(tokio.handle().clone());
    let handlers = crate::handlers::Handlers::dummy();
    let mut editor = Editor::new(
        Rect::new(0, 0, 120, 40),
        theme_loader,
        syn_loader,
        config,
        runtime,
        handlers,
    );
    let doc_id = editor.new_document(Document::default(
        editor.config.clone(),
        editor.syn_loader.clone(),
    ));
    let mut view = View::new(doc_id, editor.config().gutters.clone());
    editor.bind_view_redraw(&mut view);
    let view_id = editor.tree.insert(view);
    let _ = editor.track_tree_surface(view_id);
    let doc = crate::doc_mut!(editor, &doc_id);
    doc.ensure_view_init(view_id);
    doc.mark_as_focused();
    editor
}

pub(super) fn collab_test_location(
    editor: &Editor,
    participant: ParticipantId,
    range: std::ops::Range<usize>,
) -> Location {
    let view = editor.tree.get(editor.tree.focus);
    let doc = editor.document(view.doc).expect("doc");
    let path = doc
        .path()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(format!("participant-{}.rs", participant.value().get())));

    let mut location =
        Location::new(path, Source::Tool).with_range(RangeAnchor::new(range.start, range.end));
    if let Some(surface) = editor.surface_registry.get_by_view(view.id) {
        location = location.on_surface(surface);
    }
    location
}

pub(super) fn collab_test_path(editor: &Editor) -> PathBuf {
    let view = editor.tree.get(editor.tree.focus);
    let doc = editor.document(view.doc).expect("doc");
    doc.path()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("collab-test.rs"))
}
