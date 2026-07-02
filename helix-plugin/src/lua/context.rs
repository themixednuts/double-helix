use crate::types::SurfaceRenderOps;
use helix_view::Editor;
use std::cell::RefCell;

#[derive(Clone, Copy)]
enum EditorContext {
    Query(*const Editor),
    Mutate(*mut Editor),
}

thread_local! {
    static CURRENT_EDITOR: RefCell<Option<EditorContext>> = const { RefCell::new(None) };
    static CURRENT_RENDER_OPS: RefCell<Option<*mut SurfaceRenderOps>> = const { RefCell::new(None) };
    static CURRENT_THEME: RefCell<Option<*const helix_view::Theme>> = const { RefCell::new(None) };
}

struct EditorContextGuard {
    previous: Option<EditorContext>,
}

impl Drop for EditorContextGuard {
    fn drop(&mut self) {
        CURRENT_EDITOR.with(|e| *e.borrow_mut() = self.previous);
    }
}

struct RenderContextGuard {
    previous_ops: Option<*mut SurfaceRenderOps>,
    previous_theme: Option<*const helix_view::Theme>,
}

impl Drop for RenderContextGuard {
    fn drop(&mut self) {
        CURRENT_RENDER_OPS.with(|ops| *ops.borrow_mut() = self.previous_ops);
        CURRENT_THEME.with(|t| *t.borrow_mut() = self.previous_theme);
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

/// Set up render command + theme context for a Lua render callback.
///
/// # Safety contract
///
/// The stored pointers are cleared by the `RenderContextGuard` RAII drop
/// before this function returns, so they are never used after the references
/// go out of scope.
pub fn with_render_context<F, R>(ops: &mut SurfaceRenderOps, theme: &helix_view::Theme, f: F) -> R
where
    F: FnOnce() -> R,
{
    let previous_ops =
        CURRENT_RENDER_OPS.with(|current_ops| current_ops.replace(Some(ops as *mut _)));
    let previous_theme = CURRENT_THEME.with(|t| t.replace(Some(theme as *const _)));
    let _guard = RenderContextGuard {
        previous_ops,
        previous_theme,
    };
    f()
}

pub fn with_current_render_context<T>(
    f: impl FnOnce(&mut SurfaceRenderOps, &helix_view::Theme) -> T,
) -> std::result::Result<T, mlua::Error> {
    // SAFETY: The raw pointer was stored by `with_render_context` which holds
    // an RAII guard ensuring the reference stays valid for the duration of the
    // callback. The references are only exposed to the closure and cannot
    // escape through this API.
    let ops = CURRENT_RENDER_OPS.with(|current_ops| match *current_ops.borrow() {
        Some(ptr) => Ok(ptr),
        None => Err(mlua::Error::RuntimeError(
            "No active render context. Drawing functions can only be called from a panel render callback.".to_string(),
        )),
    })?;
    let theme = CURRENT_THEME.with(|t| match *t.borrow() {
        Some(p) => Ok(p),
        None => Err(mlua::Error::RuntimeError(
            "No active theme context.".to_string(),
        )),
    })?;

    Ok(f(unsafe { &mut *ops }, unsafe { &*theme }))
}
