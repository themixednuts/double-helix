use helix_view::Editor;
use std::cell::RefCell;

#[derive(Clone, Copy)]
enum EditorContext {
    Query(*const Editor),
    Mutate(*mut Editor),
}

thread_local! {
    static CURRENT_EDITOR: RefCell<Option<EditorContext>> = const { RefCell::new(None) };
}

struct EditorContextGuard {
    previous: Option<EditorContext>,
}

impl Drop for EditorContextGuard {
    fn drop(&mut self) {
        CURRENT_EDITOR.with(|e| *e.borrow_mut() = self.previous);
    }
}

/// Helper to set the current mutable editor context during Lua callback execution.
pub fn with_editor_context<F, R>(editor: &mut Editor, f: F) -> R
where
    F: FnOnce() -> R,
{
    let previous =
        CURRENT_EDITOR.with(|e| e.replace(Some(EditorContext::Mutate(editor as *mut _))));
    let _guard = EditorContextGuard { previous };
    f()
}

/// Read-only variant of [`with_editor_context`] for immutable phases.
pub fn with_editor_context_ref<F, R>(editor: &Editor, f: F) -> R
where
    F: FnOnce() -> R,
{
    let previous =
        CURRENT_EDITOR.with(|e| e.replace(Some(EditorContext::Query(editor as *const _))));
    let _guard = EditorContextGuard { previous };
    f()
}

pub fn with_current_editor<T>(f: impl FnOnce(&Editor) -> T) -> std::result::Result<T, mlua::Error> {
    CURRENT_EDITOR.with(|e| match *e.borrow() {
        Some(EditorContext::Query(ptr)) => Ok(f(unsafe { &*ptr })),
        Some(EditorContext::Mutate(ptr)) => Ok(f(unsafe { &*ptr })),
        None => Err(mlua::Error::RuntimeError(
            "No active editor context. This function can only be called from within a plugin callback.".to_string(),
        )),
    })
}

pub fn with_current_editor_mut<T>(
    f: impl FnOnce(&mut Editor) -> T,
) -> std::result::Result<T, mlua::Error> {
    CURRENT_EDITOR.with(|e| match *e.borrow() {
        Some(EditorContext::Mutate(ptr)) => Ok(f(unsafe { &mut *ptr })),
        Some(EditorContext::Query(_)) => Err(mlua::Error::RuntimeError(
            "No mutable editor context. This function can only be called from within a plugin callback that allows editor mutation.".to_string(),
        )),
        None => Err(mlua::Error::RuntimeError(
            "No active editor context. This function can only be called from within a plugin callback.".to_string(),
        )),
    })
}
