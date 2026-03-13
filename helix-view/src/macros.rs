//! These are macros to make getting very nested fields in the `Editor` struct easier
//! These are macros instead of functions because functions will have to take `&mut self`
//! However, rust doesn't know that you only want a partial borrow instead of borrowing the
//! entire struct which `&mut self` says.  This makes it impossible to do other mutable
//! stuff to the struct because it is already borrowed. Because macros are expanded,
//! this circumvents the problem because it is just like indexing fields by hand and then
//! putting a `&mut` in front of it. This way rust can see that we are only borrowing a
//! part of the struct and not the entire thing.

/// Get the focused view's ID and document mutably.
/// Returns `(ViewId, &mut Document)`
#[macro_export]
macro_rules! focused {
    ($editor:expr) => {{
        let focus = $editor.tree.focus;
        let view = $editor.tree.get(focus);
        let __id = view.id;
        let __doc_id = view.doc;
        let doc = $editor
            .documents
            .get_mut(&__doc_id)
            .expect("document not found");
        (__id, doc)
    }};
}

/// Get the focused view's ID and document immutably.
/// Returns `(ViewId, &Document)`
#[macro_export]
macro_rules! focused_ref {
    ($editor:expr) => {{
        let view = $editor.tree.get($editor.tree.focus);
        let __id = view.id;
        let __doc_id = view.doc;
        let doc = $editor
            .documents
            .get(&__doc_id)
            .expect("document not found");
        (__id, doc)
    }};
}

/// Get a document mutably by ID, searching both user and component documents.
/// Returns `&mut Document`
#[macro_export]
macro_rules! doc_mut {
    ($editor:expr, $id:expr) => {{
        let __id = $id;
        if let Some(d) = $editor.documents.get_mut(__id) {
            d
        } else {
            $editor
                .component_docs
                .get_mut(__id)
                .expect("document not found in documents or component_docs")
        }
    }};
}

/// Get the current view mutably.
/// Returns `&mut View`
#[macro_export]
macro_rules! view_mut {
    ($editor:expr, $id:expr) => {{
        $editor.tree.get_mut($id)
    }};
    ($editor:expr) => {{
        $editor.tree.get_mut($editor.tree.focus)
    }};
}

/// Get the current view immutably
/// Returns `&View`
#[macro_export]
macro_rules! view {
    ($editor:expr, $id:expr) => {{
        $editor.tree.get($id)
    }};
    ($editor:expr) => {{
        $editor.tree.get($editor.tree.focus)
    }};
}

/// Get a document immutably by ID, searching both user and component documents.
#[macro_export]
macro_rules! doc {
    ($editor:expr, $id:expr) => {{
        let __id = $id;
        if let Some(d) = $editor.documents.get(__id) {
            d
        } else {
            $editor
                .component_docs
                .get(__id)
                .expect("document not found in documents or component_docs")
        }
    }};
}
