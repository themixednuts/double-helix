use crate::contract::{CommandHandle, PanelHandle, PluginId, SubscriptionHandle, UiCallbackToken};
use crate::error::{PluginError, Result};
use crate::types::{PluginCallbackKey, UiCallbackId};
use helix_view::Editor;
use mlua::prelude::*;
use mlua::RegistryKey;
use parking_lot::{Mutex, RwLock};
use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

pub(crate) struct PluginQueryContext {
    editor: &'static Editor,
}

impl PluginQueryContext {
    pub const fn into_editor(self) -> &'static Editor {
        self.editor
    }
}

pub(crate) struct PluginMutationContext {
    editor: &'static mut Editor,
}

impl PluginMutationContext {
    pub fn into_editor(self) -> &'static mut Editor {
        self.editor
    }
}

pub(crate) struct PluginRenderContext {
    surface: &'static mut dyn crate::types::DrawSurface,
    theme: &'static helix_view::Theme,
}

impl PluginRenderContext {
    pub fn surface(&mut self) -> &mut dyn crate::types::DrawSurface {
        self.surface
    }

    pub const fn theme(&self) -> &'static helix_view::Theme {
        self.theme
    }
}

#[derive(Clone, Copy)]
enum EditorContext {
    Query(*const Editor),
    Mutate(*mut Editor),
}

thread_local! {
    static CURRENT_EDITOR: RefCell<Option<EditorContext>> = const { RefCell::new(None) };
    static CURRENT_SURFACE: RefCell<Option<*mut dyn crate::types::DrawSurface>> = const { RefCell::new(None) };
    static CURRENT_THEME: RefCell<Option<*const helix_view::Theme>> = const { RefCell::new(None) };
}

/// RAII guard that clears the editor thread-local on drop, ensuring cleanup
/// even if the closure panics.
struct EditorContextGuard;

impl Drop for EditorContextGuard {
    fn drop(&mut self) {
        CURRENT_EDITOR.with(|e| *e.borrow_mut() = None);
    }
}

/// RAII guard that clears the render thread-locals on drop.
struct RenderContextGuard;

impl Drop for RenderContextGuard {
    fn drop(&mut self) {
        CURRENT_SURFACE.with(|s| *s.borrow_mut() = None);
        CURRENT_THEME.with(|t| *t.borrow_mut() = None);
    }
}

/// Helper to set the current editor context during a function execution
pub fn with_editor_context<F, R>(editor: &mut Editor, f: F) -> R
where
    F: FnOnce() -> R,
{
    CURRENT_EDITOR.with(|e| {
        *e.borrow_mut() = Some(EditorContext::Mutate(editor as *mut _));
    });
    let _guard = EditorContextGuard;
    f()
}

/// Internal helper to get the active editor context immutably.
pub(crate) fn query_context() -> std::result::Result<PluginQueryContext, mlua::Error> {
    CURRENT_EDITOR.with(|e| match *e.borrow() {
        Some(EditorContext::Query(ptr)) => Ok(PluginQueryContext {
            editor: unsafe { &*ptr },
        }),
        Some(EditorContext::Mutate(ptr)) => Ok(PluginQueryContext {
            editor: unsafe { &*ptr },
        }),
        None => Err(mlua::Error::RuntimeError(
            "No active editor context. This function can only be called from within a plugin callback.".to_string(),
        )),
    })
}

/// Internal helper to get the active mutable editor context.
pub(crate) fn mutation_context() -> std::result::Result<PluginMutationContext, mlua::Error> {
    CURRENT_EDITOR.with(|e| match *e.borrow() {
        Some(EditorContext::Mutate(ptr)) => Ok(PluginMutationContext {
            editor: unsafe { &mut *ptr },
        }),
        Some(EditorContext::Query(_)) => Err(mlua::Error::RuntimeError(
            "No mutable editor context. This function can only be called from within a plugin callback that allows editor mutation.".to_string(),
        )),
        None => Err(mlua::Error::RuntimeError(
            "No active editor context. This function can only be called from within a plugin callback.".to_string(),
        )),
    })
}

pub fn get_editor() -> std::result::Result<&'static Editor, mlua::Error> {
    Ok(query_context()?.into_editor())
}

pub fn get_editor_mut() -> std::result::Result<&'static mut Editor, mlua::Error> {
    Ok(mutation_context()?.into_editor())
}

/// Read-only variant of [`with_editor_context`] for immutable phases.
///
/// Mutation accessors such as [`get_editor_mut`] are intentionally unavailable
/// from this context.
pub fn with_editor_context_ref<F, R>(editor: &Editor, f: F) -> R
where
    F: FnOnce() -> R,
{
    CURRENT_EDITOR.with(|e| {
        *e.borrow_mut() = Some(EditorContext::Query(editor as *const _));
    });
    let _guard = EditorContextGuard;
    f()
}

/// Set up surface + theme context for a Lua render callback.
///
/// # Safety contract
///
/// The stored pointers are cleared by the `RenderContextGuard` RAII drop
/// before this function returns, so they are never used after the references
/// go out of scope. The `transmute` below erases the fat pointer's implicit
/// lifetime so it can be stored in the `'static` thread-local — this is
/// safe because the guard enforces the real lifetime boundary.
pub fn with_render_context<F, R>(
    surface: &mut dyn crate::types::DrawSurface,
    theme: &helix_view::Theme,
    f: F,
) -> R
where
    F: FnOnce() -> R,
{
    // `*mut dyn Trait` is invariant over the trait object's implicit lifetime,
    // so the compiler won't let us store a non-'static fat pointer in a
    // 'static thread-local. A single transmute erases that lifetime. This
    // replaces the old decompose-to-[data,vtable]-and-reconstruct approach.
    let surface_ptr: *mut dyn crate::types::DrawSurface =
        unsafe { std::mem::transmute(surface as *mut dyn crate::types::DrawSurface) };
    CURRENT_SURFACE.with(|s| {
        *s.borrow_mut() = Some(surface_ptr);
    });
    CURRENT_THEME.with(|t| {
        *t.borrow_mut() = Some(theme as *const _);
    });
    let _guard = RenderContextGuard;
    f()
}

pub(crate) fn render_context() -> std::result::Result<PluginRenderContext, mlua::Error> {
    // SAFETY: The raw pointer was stored by `with_render_context` which holds
    // an RAII guard ensuring the reference stays valid for the duration of the
    // callback. The `'static` lifetime is a lie — it is bounded by the guard's
    // scope — but is required because Lua closures cannot carry Rust lifetimes.
    let surface = CURRENT_SURFACE.with(|s| {
        match *s.borrow() {
            Some(ptr) => Ok(unsafe { &mut *ptr }),
            None => Err(mlua::Error::RuntimeError(
                "No active render context. Drawing functions can only be called from a panel render callback.".to_string(),
            )),
        }
    })?;
    let theme = CURRENT_THEME.with(|t| match *t.borrow() {
        Some(p) => Ok(unsafe { &*p }),
        None => Err(mlua::Error::RuntimeError(
            "No active theme context.".to_string(),
        )),
    })?;

    Ok(PluginRenderContext { surface, theme })
}

pub(crate) struct CurrentPluginName(pub Arc<RwLock<Option<String>>>);

pub(crate) fn current_plugin_name(lua: &Lua) -> std::result::Result<String, mlua::Error> {
    let state = lua
        .app_data_ref::<CurrentPluginName>()
        .ok_or_else(|| mlua::Error::RuntimeError("current plugin context unavailable".into()))?
        .0
        .clone();
    let plugin_name = state
        .read()
        .clone()
        .ok_or_else(|| mlua::Error::RuntimeError("current plugin context unavailable".into()))?;
    Ok(plugin_name)
}

pub fn with_current_plugin_name<F, R, E>(
    lua: &Lua,
    plugin_name: &str,
    f: F,
) -> std::result::Result<R, E>
where
    F: FnOnce() -> std::result::Result<R, E>,
    E: From<mlua::Error>,
{
    let state = lua
        .app_data_ref::<CurrentPluginName>()
        .ok_or_else(|| mlua::Error::RuntimeError("current plugin context unavailable".into()))?
        .0
        .clone();
    let previous = state.write().replace(plugin_name.to_string());
    let result = f();
    *state.write() = previous;
    result
}

/// Convert a `DynamicValue` to a Lua value.
fn dynamic_value_to_lua(
    lua: &Lua,
    value: &crate::contract::value::DynamicValue,
) -> Result<LuaValue> {
    use crate::contract::value::DynamicValue;
    Ok(match value {
        DynamicValue::Nil => LuaNil,
        DynamicValue::Bool(b) => LuaValue::Boolean(*b),
        DynamicValue::Int(n) => LuaValue::Integer(*n),
        DynamicValue::Float(f) => LuaValue::Number(*f),
        DynamicValue::String(s) => {
            LuaValue::String(lua.create_string(s).map_err(PluginError::LuaError)?)
        }
    })
}

pub mod api;
pub mod loader;

impl FromLua for UiCallbackToken {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(mlua::Error::FromLuaConversionError {
                from: value.type_name(),
                to: "UiCallbackToken".to_string(),
                message: Some("expected a UiCallbackToken userdata".into()),
            }),
        }
    }
}

impl LuaUserData for UiCallbackToken {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.raw().get()));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.raw().get()));
    }
}

// ---------------------------------------------------------------------------
// Suspended coroutine tracking — for coroutine-based async UI
// ---------------------------------------------------------------------------

/// A Lua thread (coroutine) that yielded a `UiCallbackToken` and is waiting
/// for the host to deliver a response. The engine stores these keyed by the
/// token's internal identity and resumes them when the UI response arrives.
pub(crate) struct SuspendedCoroutine {
    /// Registry key for the `mlua::Thread` so it survives GC.
    pub(crate) thread_key: RegistryKey,
    /// Which plugin owns this coroutine (for logging / error attribution).
    pub(crate) plugin_name: String,
}

/// UI callback tokens issued by the frontend but not yet bound to a coroutine.
/// A token must be consumed by the same plugin before a suspended coroutine is
/// stored under it.
pub(crate) struct PendingUiCallbackRegistry(pub Arc<RwLock<HashMap<UiCallbackId, String>>>);

pub(crate) struct RegisteredCommand {
    pub(crate) handle: CommandHandle,
    pub(crate) plugin_name: String,
    pub(crate) metadata: crate::types::CommandMetadata,
    pub(crate) callback_ref: RegistryKey,
}

#[derive(Debug, Clone)]
pub struct RegisteredPanelCallbacks {
    pub plugin_name: String,
    pub render_callback_id: UiCallbackId,
    pub event_callback_id: Option<UiCallbackId>,
}

#[derive(Default)]
pub struct LoadedPluginRegistry {
    by_name: HashMap<String, PluginId>,
    by_id: HashMap<PluginId, String>,
}

impl LoadedPluginRegistry {
    fn insert(&mut self, name: String, handle: PluginId) {
        self.by_id.insert(handle, name.clone());
        self.by_name.insert(name, handle);
    }

    pub fn id_for_name(&self, name: &str) -> Option<PluginId> {
        self.by_name.get(name).copied()
    }

    pub fn name_for_id(&self, handle: PluginId) -> Option<&str> {
        self.by_id.get(&handle).map(String::as_str)
    }

    fn clear(&mut self) {
        self.by_name.clear();
        self.by_id.clear();
    }
}

#[derive(Default)]
pub(crate) struct CommandRegistry {
    by_name: HashMap<String, CommandHandle>,
    by_handle: HashMap<CommandHandle, RegisteredCommand>,
}

impl CommandRegistry {
    pub(crate) fn name_available(&self, name: &str, except: Option<CommandHandle>) -> bool {
        self.by_name
            .get(name)
            .is_none_or(|handle| Some(*handle) == except)
    }

    pub(crate) fn insert(
        &mut self,
        command: RegisteredCommand,
    ) -> std::result::Result<(), Box<(RegisteredCommand, String)>> {
        if !self.name_available(&command.metadata.name, None) {
            let message = format!("command already registered: {}", command.metadata.name);
            return Err(Box::new((command, message)));
        }
        self.by_name
            .insert(command.metadata.name.clone(), command.handle);
        self.by_handle.insert(command.handle, command);
        Ok(())
    }

    pub(crate) fn get_by_name(&self, name: &str) -> Option<&RegisteredCommand> {
        self.by_name
            .get(name)
            .and_then(|handle| self.by_handle.get(handle))
    }

    pub(crate) fn owner_for_handle(&self, handle: CommandHandle) -> Option<&str> {
        self.by_handle
            .get(&handle)
            .map(|command| command.plugin_name.as_str())
    }

    pub(crate) fn handles_with_plugin_names(&self) -> Vec<(CommandHandle, String)> {
        self.by_handle
            .values()
            .map(|command| (command.handle, command.plugin_name.clone()))
            .collect()
    }

    pub(crate) fn metadata_for_update(
        &self,
        req: &crate::contract::requests::CommandUpdateRequest,
    ) -> std::result::Result<crate::types::CommandMetadata, String> {
        let command = self
            .by_handle
            .get(&req.command)
            .ok_or_else(|| format!("stale handle: {} no longer exists", req.command))?;

        let name = req
            .name
            .clone()
            .unwrap_or_else(|| command.metadata.name.clone());
        if !self.name_available(&name, Some(req.command)) {
            return Err(format!("command already registered: {name}"));
        }

        let doc = req
            .doc
            .clone()
            .unwrap_or_else(|| command.metadata.doc.clone());
        let args = req.args.as_ref().map_or_else(
            || command.metadata.args.clone(),
            |args| (!args.is_empty()).then(|| args.join(" ")),
        );

        Ok(crate::types::CommandMetadata { name, doc, args })
    }

    pub(crate) fn update(
        &mut self,
        handle: CommandHandle,
        metadata: crate::types::CommandMetadata,
        callback_ref: Option<RegistryKey>,
    ) -> std::result::Result<Option<RegistryKey>, String> {
        if !self.name_available(&metadata.name, Some(handle)) {
            return Err(format!("command already registered: {}", metadata.name));
        }

        let command = self
            .by_handle
            .get_mut(&handle)
            .ok_or_else(|| format!("stale handle: {handle} no longer exists"))?;

        if command.metadata.name != metadata.name {
            self.by_name.remove(&command.metadata.name);
            self.by_name.insert(metadata.name.clone(), handle);
        }

        command.metadata = metadata;
        Ok(callback_ref
            .map(|callback_ref| std::mem::replace(&mut command.callback_ref, callback_ref)))
    }

    pub(crate) fn remove(&mut self, handle: CommandHandle) -> Option<RegisteredCommand> {
        let command = self.by_handle.remove(&handle)?;
        if self.by_name.get(&command.metadata.name) == Some(&handle) {
            self.by_name.remove(&command.metadata.name);
        }
        Some(command)
    }

    pub(crate) fn metadata(&self) -> Vec<crate::types::CommandMetadata> {
        let mut metadata: Vec<_> = self
            .by_handle
            .values()
            .map(|command| command.metadata.clone())
            .collect();
        metadata.sort_by(|left, right| left.name.cmp(&right.name));
        metadata
    }

    pub(crate) fn clear(&mut self) {
        self.by_name.clear();
        self.by_handle.clear();
    }
}

pub(crate) struct PluginHandleCounter(pub Arc<std::sync::atomic::AtomicU64>);

impl PluginHandleCounter {
    pub(crate) fn next(&self) -> PluginId {
        loop {
            let raw = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(id) = NonZeroU64::new(raw) {
                return PluginId::from_raw(id);
            }
        }
    }
}

/// App-data wrapper so Lua callbacks can store suspended coroutines.
pub(crate) struct SuspendedCoroutineRegistry(
    pub Arc<RwLock<HashMap<UiCallbackId, SuspendedCoroutine>>>,
);

pub(crate) fn remember_pending_ui_callback(
    lua: &Lua,
    plugin_name: String,
    callback_id: UiCallbackId,
) -> std::result::Result<(), mlua::Error> {
    let registry = lua
        .app_data_ref::<PendingUiCallbackRegistry>()
        .ok_or_else(|| {
            mlua::Error::RuntimeError("pending UI callback registry unavailable".into())
        })?;
    let mut pending = registry.0.write();
    if let Some(owner) = pending.get(&callback_id) {
        return Err(mlua::Error::RuntimeError(format!(
            "UI callback {} is already pending for plugin '{owner}'",
            callback_id.get()
        )));
    }
    pending.insert(callback_id, plugin_name);
    Ok(())
}

pub(crate) fn claim_pending_ui_callback(
    lua: &Lua,
    plugin_name: &str,
    callback_id: UiCallbackId,
) -> std::result::Result<(), mlua::Error> {
    let registry = lua
        .app_data_ref::<PendingUiCallbackRegistry>()
        .ok_or_else(|| {
            mlua::Error::RuntimeError("pending UI callback registry unavailable".into())
        })?;
    claim_pending_ui_callback_from(&registry.0, plugin_name, callback_id)
        .map_err(mlua::Error::RuntimeError)
}

fn claim_pending_ui_callback_from(
    pending: &RwLock<HashMap<UiCallbackId, String>>,
    plugin_name: &str,
    callback_id: UiCallbackId,
) -> std::result::Result<(), String> {
    let mut pending = pending.write();
    match pending.get(&callback_id) {
        Some(owner) if owner == plugin_name => {
            pending.remove(&callback_id);
            Ok(())
        }
        Some(owner) => Err(format!(
            "permission denied: plugin '{plugin_name}' does not own UI callback {} (owner: '{owner}')",
            callback_id.get()
        )),
        None => Err(format!(
            "invalid request: UI callback {} was not issued to plugin '{plugin_name}'",
            callback_id.get()
        )),
    }
}

pub(crate) struct RegisteredEventHandler {
    pub(crate) handle: SubscriptionHandle,
    pub(crate) plugin_name: String,
    pub(crate) callback_ref: RegistryKey,
}

type ContractEventHandlers =
    HashMap<crate::contract::events::EventKind, Vec<RegisteredEventHandler>>;

/// Wrapper to store contract event handlers in Lua app data.
pub(crate) struct ContractEventHandlersWrapper(pub Arc<RwLock<ContractEventHandlers>>);

pub(crate) struct UiHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginUiHost + Send + Sync>>>,
);

pub(crate) struct PanelHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>>>,
);

pub(crate) struct CommandHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>>>,
);

pub(crate) struct EventHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginEventHost + Send + Sync>>>,
);

pub(crate) struct CommandRegistryWrapper(pub Arc<RwLock<CommandRegistry>>);

pub(crate) struct LoadedPluginRegistryWrapper(pub Arc<RwLock<LoadedPluginRegistry>>);

pub(crate) struct PanelCallbackRegistry(
    pub Arc<RwLock<HashMap<PanelHandle, RegisteredPanelCallbacks>>>,
);

/// Lua scripting engine for Helix plugins
pub struct LuaEngine {
    /// The Lua runtime
    lua: Lua,
    /// Contract-based event handlers: EventKind -> Vec<(plugin_name, callback_ref)>
    contract_event_handlers: Arc<RwLock<ContractEventHandlers>>,
    /// Loaded plugins by name
    plugins: HashMap<String, crate::types::Plugin>,
    /// Registered commands keyed by typed command handles.
    commands: Arc<RwLock<CommandRegistry>>,
    /// Loaded plugin handles keyed by plugin name and reverse lookup by id.
    plugin_registry: Arc<RwLock<LoadedPluginRegistry>>,
    /// Next available plugin handle.
    next_plugin_handle: Arc<std::sync::atomic::AtomicU64>,
    /// Rust-owned current plugin context for callbacks and API ownership checks.
    current_plugin_name: Arc<RwLock<Option<String>>>,
    /// UI callbacks: (plugin_name, callback_id) -> callback_ref
    ui_callbacks: Arc<RwLock<HashMap<PluginCallbackKey, RegistryKey>>>,
    /// Panel render/event callback metadata keyed by panel handle.
    panel_callbacks: Arc<RwLock<HashMap<PanelHandle, RegisteredPanelCallbacks>>>,
    /// Next available UI callback identity.
    next_ui_callback_id: Arc<std::sync::atomic::AtomicU64>,
    /// Frontend UI host.
    ui_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginUiHost + Send + Sync>>>>,
    /// Frontend panel host.
    panel_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>>>>,
    /// Frontend command host.
    command_host:
        Option<Arc<Mutex<Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>>>>,
    /// Frontend event host.
    event_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginEventHost + Send + Sync>>>>,
    /// Suspended coroutines waiting for UI responses, keyed by callback token identity.
    suspended_coroutines: Arc<RwLock<HashMap<UiCallbackId, SuspendedCoroutine>>>,
    /// UI callback tokens issued by the frontend and awaiting coroutine binding.
    pending_ui_callbacks: Arc<RwLock<HashMap<UiCallbackId, String>>>,
}

impl LuaEngine {
    /// Create a new Lua engine
    pub fn new() -> Result<Self> {
        let lua = Lua::new();

        // Set up sandboxing - remove dangerous functions
        lua.load(
            r#"
            -- Remove dangerous functions
            os.execute = nil
            os.exit = nil
            io = nil
            loadfile = nil
            dofile = nil
            "#,
        )
        .exec()
        .map_err(|e| {
            PluginError::InitializationFailed(format!("Failed to setup sandbox: {}", e))
        })?;

        let contract_event_handlers = Arc::new(RwLock::new(HashMap::new()));
        let commands = Arc::new(RwLock::new(CommandRegistry::default()));
        let plugin_registry = Arc::new(RwLock::new(LoadedPluginRegistry::default()));
        let next_plugin_handle = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let current_plugin_name = Arc::new(RwLock::new(None));
        lua.set_app_data(CurrentPluginName(Arc::clone(&current_plugin_name)));
        let ui_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let panel_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let next_ui_callback_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let suspended_coroutines = Arc::new(RwLock::new(HashMap::new()));
        let pending_ui_callbacks = Arc::new(RwLock::new(HashMap::new()));
        lua.set_app_data(PendingUiCallbackRegistry(Arc::clone(&pending_ui_callbacks)));

        Ok(Self {
            lua,
            contract_event_handlers,
            plugins: HashMap::new(),
            commands,
            plugin_registry,
            next_plugin_handle,
            current_plugin_name,
            ui_callbacks,
            panel_callbacks,
            next_ui_callback_id,
            ui_host: None,
            panel_host: None,
            command_host: None,
            event_host: None,
            suspended_coroutines,
            pending_ui_callbacks,
        })
    }

    /// Reset the Lua engine, clearing all state and plugins
    pub fn reset(&mut self) -> Result<()> {
        let lua = Lua::new();

        // Set up sandboxing - remove dangerous functions
        lua.load(
            r#"
            -- Remove dangerous functions
            os.execute = nil
            os.exit = nil
            io = nil
            loadfile = nil
            dofile = nil
            "#,
        )
        .exec()
        .map_err(|e| {
            PluginError::InitializationFailed(format!("Failed to setup sandbox: {}", e))
        })?;

        self.clear_event_subscriptions()?;
        self.clear_command_registrations()?;
        self.ui_callbacks.write().clear();
        self.panel_callbacks.write().clear();
        self.suspended_coroutines.write().clear();
        self.pending_ui_callbacks.write().clear();
        self.current_plugin_name.write().take();
        self.lua = lua;
        self.lua
            .set_app_data(CurrentPluginName(Arc::clone(&self.current_plugin_name)));
        self.lua.set_app_data(PendingUiCallbackRegistry(Arc::clone(
            &self.pending_ui_callbacks,
        )));
        self.plugins.clear();
        self.plugin_registry.write().clear();

        Ok(())
    }

    fn clear_event_subscriptions(&self) -> Result<()> {
        let subscriptions: Vec<_> = self
            .contract_event_handlers
            .read()
            .values()
            .flat_map(|entries| {
                entries
                    .iter()
                    .map(|entry| (entry.plugin_name.clone(), entry.handle))
            })
            .collect();

        if let Some(host) = &self.event_host {
            let mut host = host.lock();
            for (plugin_name, handle) in subscriptions {
                let plugin = self.plugin_id_for_name(&plugin_name)?;
                match host.unsubscribe(plugin, handle) {
                    Ok(()) | Err(crate::contract::ContractError::StaleHandle { .. }) => {}
                    Err(err) => {
                        return Err(PluginError::InitializationFailed(format!(
                            "Failed to clear event subscription {handle}: {err}"
                        )));
                    }
                }
            }
        }

        self.contract_event_handlers.write().clear();
        Ok(())
    }

    fn clear_command_registrations(&self) -> Result<()> {
        let handles = self.commands.read().handles_with_plugin_names();

        if let Some(host) = &self.command_host {
            let mut host = host.lock();
            for (command, plugin_name) in handles {
                let plugin = self.plugin_id_for_name(&plugin_name)?;
                let req = crate::contract::requests::CommandRemoveRequest { command };
                match host.remove_command(plugin, req) {
                    Ok(()) | Err(crate::contract::ContractError::StaleHandle { .. }) => {}
                    Err(err) => {
                        return Err(PluginError::InitializationFailed(format!(
                            "Failed to clear command registration {command}: {err}"
                        )));
                    }
                }
            }
        }

        self.commands.write().clear();
        Ok(())
    }

    fn plugin_id_for_name(&self, plugin_name: &str) -> Result<PluginId> {
        self.plugin_registry
            .read()
            .id_for_name(plugin_name)
            .ok_or_else(|| {
                PluginError::InitializationFailed(format!("Plugin not registered: {plugin_name}"))
            })
    }

    pub fn set_ui_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginUiHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(UiHostWrapper(Arc::clone(&host)));
        self.ui_host = Some(host);
    }

    pub fn set_panel_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(PanelHostWrapper(Arc::clone(&host)));
        self.panel_host = Some(host);
    }

    pub fn set_command_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(CommandHostWrapper(Arc::clone(&host)));
        self.command_host = Some(host);
    }

    pub fn set_event_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginEventHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(EventHostWrapper(Arc::clone(&host)));
        self.event_host = Some(host);
    }

    /// Register the Helix API with Lua
    pub fn register_api(&self, config: crate::types::PluginConfig) -> Result<()> {
        let globals = self.lua.globals();

        if let Some(ref host) = self.ui_host {
            self.lua.set_app_data(UiHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.panel_host {
            self.lua.set_app_data(PanelHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.command_host {
            self.lua.set_app_data(CommandHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.event_host {
            self.lua.set_app_data(EventHostWrapper(Arc::clone(host)));
        }

        self.lua
            .set_app_data(crate::types::UiCallbackRegistry(Arc::clone(
                &self.ui_callbacks,
            )));
        self.lua
            .set_app_data(PanelCallbackRegistry(Arc::clone(&self.panel_callbacks)));
        self.lua
            .set_app_data(crate::types::UiCallbackCounter(Arc::clone(
                &self.next_ui_callback_id,
            )));
        self.lua
            .set_app_data(PluginHandleCounter(Arc::clone(&self.next_plugin_handle)));
        self.lua
            .set_app_data(CurrentPluginName(Arc::clone(&self.current_plugin_name)));
        self.lua
            .set_app_data(LoadedPluginRegistryWrapper(Arc::clone(
                &self.plugin_registry,
            )));
        self.lua
            .set_app_data(ContractEventHandlersWrapper(Arc::clone(
                &self.contract_event_handlers,
            )));
        self.lua.set_app_data(SuspendedCoroutineRegistry(Arc::clone(
            &self.suspended_coroutines,
        )));
        self.lua.set_app_data(PendingUiCallbackRegistry(Arc::clone(
            &self.pending_ui_callbacks,
        )));
        self.lua.set_app_data(config);

        // Create the main helix table
        let helix = self.lua.create_table()?;

        // Register all API through the contract-based facade
        api::register_facade(&self.lua, &helix, Arc::clone(&self.commands))?;

        // Register version info
        helix.set("version", env!("CARGO_PKG_VERSION"))?;

        // Set the global helix table
        globals.set("helix", helix)?;

        // Inject Lua wrappers that reference the `helix` global (coroutine UI wrappers, helix.async).
        // Must be called AFTER the global is set.
        api::facade::inject_lua_wrappers(&self.lua)?;

        Ok(())
    }

    /// Execute a registered plugin command.
    ///
    /// The handler is wrapped in a Lua coroutine so it can yield (e.g. to wait
    /// for `helix.ui.prompt()`). If the coroutine yields a `UiCallbackToken`, it is
    /// stored in `suspended_coroutines` and will be resumed by
    /// [`handle_ui_callback`].
    pub fn execute_command(
        &self,
        editor: &mut Editor,
        name: &str,
        args: Vec<String>,
    ) -> Result<()> {
        let (plugin_name, callback) = {
            let commands = self.commands.read();
            let command = commands.get_by_name(name).ok_or_else(|| {
                PluginError::CommandExecutionFailed(format!("Command not found: {name}"))
            })?;
            let callback = self
                .lua
                .registry_value(&command.callback_ref)
                .map_err(|e| {
                    PluginError::CommandExecutionFailed(format!("Failed to retrieve callback: {e}"))
                })?;
            (command.plugin_name.clone(), callback)
        };

        let thread = self.lua.create_thread(callback).map_err(|e| {
            PluginError::CommandExecutionFailed(format!("Failed to create coroutine: {}", e))
        })?;

        let lua_args = self
            .lua
            .create_sequence_from(args)
            .map_err(PluginError::LuaError)?;

        with_editor_context(editor, || {
            with_current_plugin_name(&self.lua, &plugin_name, || {
                let result: LuaMultiValue = thread.resume(lua_args).map_err(|e| {
                    PluginError::CommandExecutionFailed(format!("Execution failed: {}", e))
                })?;
                self.handle_coroutine_yield(&thread, &plugin_name, result)
            })
        })?;

        Ok(())
    }

    /// If a coroutine yielded a callback token, store it for later resumption.
    /// If it returned normally (finished), do nothing.
    fn handle_coroutine_yield(
        &self,
        thread: &LuaThread,
        plugin_name: &str,
        yielded: LuaMultiValue,
    ) -> Result<()> {
        if thread.status() != LuaThreadStatus::Resumable {
            return Ok(());
        }

        // The coroutine yielded — expect a single typed UI callback token.
        let id_val = yielded.into_iter().next().ok_or_else(|| {
            PluginError::CommandExecutionFailed(
                "coroutine yielded without a UI callback token".into(),
            )
        })?;

        let token: UiCallbackToken = self.lua.unpack(id_val).map_err(|e| {
            PluginError::CommandExecutionFailed(format!(
                "coroutine yielded non-UiCallbackToken value: {e}"
            ))
        })?;

        let callback_id = UiCallbackId::new(token.raw().get()).ok_or_else(|| {
            PluginError::CommandExecutionFailed("coroutine yielded zero UI callback token".into())
        })?;

        if self.suspended_coroutines.read().contains_key(&callback_id) {
            return Err(PluginError::CommandExecutionFailed(format!(
                "UI callback {} is already bound to a coroutine",
                callback_id.get()
            )));
        }
        claim_pending_ui_callback_from(&self.pending_ui_callbacks, plugin_name, callback_id)
            .map_err(PluginError::CommandExecutionFailed)?;

        let thread_key = self
            .lua
            .create_registry_value(thread.clone())
            .map_err(PluginError::LuaError)?;

        self.suspended_coroutines.write().insert(
            callback_id,
            SuspendedCoroutine {
                thread_key,
                plugin_name: plugin_name.to_string(),
            },
        );

        Ok(())
    }

    /// Get all registered commands metadata
    pub fn get_commands(&self) -> Vec<crate::types::CommandMetadata> {
        self.commands.read().metadata()
    }
    /// Handle a UI callback from the editor (prompt response, picker selection, etc.).
    ///
    /// If a coroutine yielded this callback token, resume it with the response value.
    /// Unknown callback tokens are ignored; persistent render callbacks are keyed in a
    /// separate registry and must not be consumed by UI responses.
    ///
    /// If the resumed coroutine yields *again* (chained async ops), it is re-stored
    /// under the new callback token identity.
    pub fn handle_ui_callback(
        &self,
        editor: &mut Editor,
        callback_id: UiCallbackId,
        value: crate::contract::value::DynamicValue,
    ) -> Result<()> {
        let suspended = self.suspended_coroutines.write().remove(&callback_id);
        if let Some(entry) = suspended {
            let thread: LuaThread = self
                .lua
                .registry_value(&entry.thread_key)
                .map_err(PluginError::LuaError)?;

            let lua_value = dynamic_value_to_lua(&self.lua, &value)?;

            with_editor_context(editor, || {
                with_current_plugin_name(&self.lua, &entry.plugin_name, || {
                    let result: LuaMultiValue = thread.resume(lua_value).map_err(|e| {
                        PluginError::CommandExecutionFailed(format!(
                            "coroutine resume failed (plugin: {}): {}",
                            entry.plugin_name, e
                        ))
                    })?;
                    // Handle re-yield for chained operations.
                    self.handle_coroutine_yield(&thread, &entry.plugin_name, result)
                })
            })?;

            return Ok(());
        }
        self.pending_ui_callbacks.write().remove(&callback_id);
        Ok(())
    }

    fn ensure_plugin_id(&self, plugin_name: &str) -> PluginId {
        if let Some(handle) = self.plugin_registry.read().id_for_name(plugin_name) {
            return handle;
        }

        let handle = PluginHandleCounter(Arc::clone(&self.next_plugin_handle)).next();
        self.plugin_registry
            .write()
            .insert(plugin_name.to_string(), handle);
        handle
    }

    pub fn load_plugin(&mut self, editor: &mut Editor, plugin: crate::types::Plugin) -> Result<()> {
        let entry_file = plugin
            .path
            .join(plugin.metadata.entry.as_deref().unwrap_or("init.lua"));

        if !entry_file.exists() {
            return Err(PluginError::InvalidPluginStructure(format!(
                "Entry file not found: {:?}",
                entry_file
            )));
        }

        // Load and execute the plugin
        let code = std::fs::read_to_string(&entry_file)?;
        self.ensure_plugin_id(&plugin.metadata.name);
        with_editor_context(editor, || {
            with_current_plugin_name(&self.lua, &plugin.metadata.name, || {
                self.lua
                    .load(&code)
                    .set_name(&plugin.metadata.name)
                    .exec()
                    .map_err(PluginError::LuaError)
            })
        })?;

        self.plugins.insert(plugin.metadata.name.clone(), plugin);

        Ok(())
    }

    /// Dispatch a contract event to all subscribed plugin handlers.
    pub fn call_event_handlers(
        &self,
        editor: &mut Editor,
        event: &crate::contract::events::PluginEvent,
    ) -> Result<()> {
        let handlers = self.contract_event_handlers.read();
        let kind = event.kind();

        if let Some(callbacks) = handlers.get(&kind) {
            let event_data = with_editor_context_ref(editor, || {
                api::facade::contract_event_to_table(&self.lua, event)
                    .map_err(PluginError::LuaError)
            })?;

            for entry in callbacks {
                let callback: LuaFunction =
                    self.lua.registry_value(&entry.callback_ref).map_err(|e| {
                        PluginError::EventHandlerError {
                            plugin: entry.plugin_name.clone(),
                            error: format!("Failed to retrieve callback: {}", e),
                        }
                    })?;

                let plugin_name_captured = entry.plugin_name.clone();
                with_editor_context(editor, || {
                    with_current_plugin_name(&self.lua, &entry.plugin_name, || {
                        callback.call::<()>(event_data.clone())
                    })
                    .map_err(|e| PluginError::EventHandlerError {
                        plugin: plugin_name_captured,
                        error: format!("Handler execution failed: {}", e),
                    })
                })?;
            }
        }

        Ok(())
    }

    /// Get the Lua runtime (for advanced operations)
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Get the UI callback registry (for looking up render/event callbacks).
    pub fn ui_callbacks(&self) -> &Arc<RwLock<HashMap<PluginCallbackKey, RegistryKey>>> {
        &self.ui_callbacks
    }

    /// Get the panel callback registry (for looking up panel render/event handlers).
    pub fn panel_callbacks(&self) -> &Arc<RwLock<HashMap<PanelHandle, RegisteredPanelCallbacks>>> {
        &self.panel_callbacks
    }

    /// Get the loaded plugin registry.
    pub fn plugin_registry(&self) -> &Arc<RwLock<LoadedPluginRegistry>> {
        &self.plugin_registry
    }

    /// Get loaded plugins
    pub fn plugins(&self) -> &HashMap<String, crate::types::Plugin> {
        &self.plugins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arc_swap::ArcSwap;
    use helix_core::syntax;
    use helix_runtime::Runtime;
    use helix_view::{
        editor::{Action, Config},
        graphics::Rect,
        handlers::Handlers,
        theme,
    };

    fn test_editor() -> Editor {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let tokio = Box::leak(Box::new(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("tokio runtime"),
        ));
        let _guard = tokio.enter();
        let runtime = Runtime::new(tokio.handle().clone());

        Editor::new(
            Rect::new(0, 0, 120, 40),
            theme_loader,
            syn_loader,
            config,
            runtime,
            Handlers::dummy(),
        )
    }

    fn register_loaded_plugin(engine: &LuaEngine, name: &str, id: u64) {
        engine.plugin_registry.write().insert(
            name.into(),
            PluginId::from_raw(NonZeroU64::new(id).unwrap()),
        );
    }

    fn set_current_plugin(engine: &LuaEngine, name: &str) {
        *engine.current_plugin_name.write() = Some(name.into());
    }

    fn exec_as(
        engine: &LuaEngine,
        plugin_name: &str,
        code: &str,
    ) -> std::result::Result<(), mlua::Error> {
        with_current_plugin_name(&engine.lua, plugin_name, || engine.lua.load(code).exec())
    }

    struct TestUiHost {
        next: u64,
    }

    impl TestUiHost {
        fn next_token(&mut self) -> crate::contract::host::UiCallbackToken {
            let token = crate::contract::host::UiCallbackToken::from_raw(
                NonZeroU64::new(self.next).unwrap(),
            );
            self.next += 1;
            token
        }
    }

    impl crate::contract::host::PluginUiHost for TestUiHost {
        fn notify(
            &mut self,
            _req: crate::contract::requests::NotifyRequest,
        ) -> crate::contract::ContractResult<()> {
            Ok(())
        }

        fn prompt(
            &mut self,
            _plugin: crate::contract::PluginId,
            _req: crate::contract::requests::PromptRequest,
        ) -> crate::contract::ContractResult<crate::contract::host::UiCallbackToken> {
            Ok(self.next_token())
        }

        fn confirm(
            &mut self,
            _plugin: crate::contract::PluginId,
            _req: crate::contract::requests::ConfirmRequest,
        ) -> crate::contract::ContractResult<crate::contract::host::UiCallbackToken> {
            Ok(self.next_token())
        }

        fn picker(
            &mut self,
            _plugin: crate::contract::PluginId,
            _req: crate::contract::requests::PickerRequest,
        ) -> crate::contract::ContractResult<crate::contract::host::UiCallbackToken> {
            Ok(self.next_token())
        }
    }

    #[test]
    fn test_engine_creation() {
        let engine = LuaEngine::new();
        assert!(engine.is_ok());
    }

    #[test]
    fn test_api_registration() {
        let engine = LuaEngine::new().unwrap();
        assert!(engine
            .register_api(crate::types::PluginConfig::default())
            .is_ok());

        // Test that helix global exists
        let result: std::result::Result<(), mlua::Error> =
            engine.lua.load("assert(helix ~= nil)").exec();
        assert!(result.is_ok());
    }

    #[test]
    fn test_lua_current_plugin_global_cannot_spoof_identity() {
        let engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "owner-plugin", 1);
        register_loaded_plugin(&engine, "caller-plugin", 2);
        engine
            .register_api(crate::types::PluginConfig {
                plugins: vec![
                    crate::types::IndividualPluginConfig {
                        name: "owner-plugin".into(),
                        enabled: true,
                        config: serde_json::json!({ "name": "owner" }),
                    },
                    crate::types::IndividualPluginConfig {
                        name: "caller-plugin".into(),
                        enabled: true,
                        config: serde_json::json!({ "name": "caller" }),
                    },
                ],
                ..Default::default()
            })
            .unwrap();

        exec_as(
            &engine,
            "owner-plugin",
            r#"
            _current_plugin_name = "caller-plugin"
            local config = helix.config()
            assert(config.name == "owner")
            "#,
        )
        .unwrap();
    }

    #[test]
    fn test_load_plugin_runs_with_editor_context() {
        let mut engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("init.lua"),
            r#"_G.mode_at_load = helix.workspace.mode()"#,
        )
        .unwrap();
        let plugin = crate::types::Plugin {
            metadata: crate::types::PluginMetadata {
                name: "load-context".into(),
                version: "0.1.0".into(),
                description: None,
                author: None,
                entry: Some("init.lua".into()),
            },
            path: dir.path().to_path_buf(),
            enabled: true,
        };

        engine.load_plugin(&mut editor, plugin).unwrap();

        let mode_at_load: String = engine.lua.globals().get("mode_at_load").unwrap();
        assert_eq!(mode_at_load, "normal");
    }

    #[test]
    fn test_document_select_all_targets_view_showing_document() {
        let engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();
        let doc_one = editor.open_markdown_scratch(Action::VerticalSplit, "one".to_owned());
        let view_one = editor.tree.focus;
        let _doc_two = editor.open_markdown_scratch(Action::VerticalSplit, "two".to_owned());

        with_editor_context(&mut editor, || {
            engine
                .lua
                .load(
                    r#"
                local focused = helix.workspace.focused_document()
                for _, doc in ipairs(helix.documents.list()) do
                    if doc.handle ~= focused.handle then
                        doc:select_all()
                    end
                end
                "#,
                )
                .exec()
        })
        .unwrap();

        let doc = editor.document(doc_one).unwrap();
        let range = doc.selection(view_one).primary();
        assert_eq!(range.from(), 0);
        assert_eq!(range.to(), doc.text().len_chars());
    }

    #[test]
    fn test_panel_registration_failure_clears_render_callbacks() {
        struct FailingPanelHost;

        impl crate::contract::host::PluginPanelHost for FailingPanelHost {
            fn register_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _reg: crate::contract::requests::PanelRegistration,
            ) -> crate::contract::ContractResult<crate::contract::PanelHandle> {
                Err(crate::contract::ContractError::invalid_request(
                    "panel rejected",
                ))
            }

            fn update_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::PanelUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn close_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::PanelCloseRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn toggle_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::TogglePanelRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn focus_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::FocusPanelRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn resize_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::ResizePanelRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn list_panels(&self) -> Vec<crate::contract::snapshots::PanelSnapshot> {
                Vec::new()
            }
        }

        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_panel_host(Box::new(FailingPanelHost));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let result = engine
            .lua
            .load(
                r#"
                helix.ui.panel({
                    title = "Rejected",
                    render = function() end,
                    on_event = function() end,
                })
                "#,
            )
            .exec();

        assert!(result.is_err());
        assert!(engine.ui_callbacks.read().is_empty());
        assert!(engine.panel_callbacks.read().is_empty());
    }

    #[test]
    fn test_panel_operations_reject_foreign_handles() {
        #[derive(Clone)]
        struct TestPanelHost {
            toggled: Arc<Mutex<Vec<crate::contract::requests::TogglePanelRequest>>>,
        }

        impl crate::contract::host::PluginPanelHost for TestPanelHost {
            fn register_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _reg: crate::contract::requests::PanelRegistration,
            ) -> crate::contract::ContractResult<crate::contract::PanelHandle> {
                Ok(crate::contract::PanelHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::PanelUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn close_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::PanelCloseRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn toggle_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::TogglePanelRequest,
            ) -> crate::contract::ContractResult<()> {
                self.toggled.lock().push(req);
                Ok(())
            }

            fn focus_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::FocusPanelRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn resize_panel(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::ResizePanelRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn list_panels(&self) -> Vec<crate::contract::snapshots::PanelSnapshot> {
                vec![crate::contract::snapshots::PanelSnapshot {
                    handle: crate::contract::PanelHandle::from_raw(NonZeroU64::new(1).unwrap()),
                    title: "Owned".into(),
                    side: crate::contract::requests::PanelSide::Right,
                    visible: true,
                    is_focused: false,
                }]
            }
        }

        let toggled = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "owner-plugin", 1);
        register_loaded_plugin(&engine, "caller-plugin", 2);
        engine.set_panel_host(Box::new(TestPanelHost {
            toggled: Arc::clone(&toggled),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "owner-plugin",
            r#"
            panel = helix.ui.panel({
                title = "Owned",
                render = function() end,
            })
            assert(#helix.ui.panels() == 1)
            "#,
        )
        .unwrap();

        let result = exec_as(
            &engine,
            "caller-plugin",
            r#"
            assert(#helix.ui.panels() == 0)
            panel:toggle()
            "#,
        );

        let err = result.expect_err("foreign panel handle should be rejected");
        assert!(err.to_string().contains("permission denied"));
        assert!(toggled.lock().is_empty());
        assert_eq!(engine.panel_callbacks.read().len(), 1);
    }

    #[test]
    fn test_plugin_float_close_clears_render_callback() {
        let engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();

        with_editor_context(&mut editor, || {
            engine
                .lua
                .load(
                    r#"
                    local float = helix.floats.create({
                        placement = { type = "centered", width = 20, height = 5 },
                        render = function() end,
                    })
                    float:close()
                    "#,
                )
                .exec()
        })
        .unwrap();

        assert!(engine.ui_callbacks.read().is_empty());
    }

    #[test]
    fn test_float_operations_reject_foreign_handles() {
        let engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "owner-plugin", 1);
        register_loaded_plugin(&engine, "caller-plugin", 2);
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();

        with_editor_context(&mut editor, || {
            exec_as(
                &engine,
                "owner-plugin",
                r#"
                float = helix.floats.create({
                    title = "Owned",
                    placement = { type = "centered", width = 20, height = 5 },
                    content = { { text = "owned" } },
                })
                assert(#helix.floats.list() == 1)
                "#,
            )
        })
        .unwrap();

        let update_result = with_editor_context(&mut editor, || {
            exec_as(
                &engine,
                "caller-plugin",
                r#"
                assert(#helix.floats.list() == 0)
                float:update({ title = "Stolen" })
                "#,
            )
        });
        let err = update_result.expect_err("foreign float update should be rejected");
        assert!(err.to_string().contains("permission denied"));

        let entry = editor.model.floats.iter().next().unwrap().1;
        assert_eq!(entry.title.as_deref(), Some("Owned"));

        let close_result = with_editor_context(&mut editor, || {
            exec_as(&engine, "caller-plugin", "float:close()")
        });
        let err = close_result.expect_err("foreign float close should be rejected");
        assert!(err.to_string().contains("permission denied"));
        assert_eq!(editor.model.floats.len(), 1);
    }

    #[test]
    fn test_plugin_float_update_preserves_title_and_rejects_invalid_placement() {
        let engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "test-plugin", 1);
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();

        with_editor_context(&mut editor, || {
            exec_as(
                &engine,
                "test-plugin",
                r#"
                float = helix.floats.create({
                    title = "Owned",
                    placement = { type = "centered", width = 20, height = 5 },
                    content = { { text = "owned" } },
                })
                float:update({ placement = { type = "centered", width = 30, height = 6 } })
                "#,
            )
        })
        .unwrap();

        let entry = editor.model.floats.iter().next().unwrap().1;
        assert_eq!(entry.title.as_deref(), Some("Owned"));
        assert!(matches!(
            entry.placement,
            helix_view::model::Placement::Centered {
                width: 30,
                height: 6
            }
        ));

        let invalid = with_editor_context(&mut editor, || {
            exec_as(
                &engine,
                "test-plugin",
                r#"float:update({ placement = { type = "bogus" } })"#,
            )
        });
        let err = invalid.expect_err("invalid placement should be reported");
        assert!(err.to_string().contains("invalid placement type"));

        let entry = editor.model.floats.iter().next().unwrap().1;
        assert_eq!(entry.title.as_deref(), Some("Owned"));
    }

    #[test]
    fn test_plugin_float_create_without_editor_context_does_not_store_callback() {
        let engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let result: std::result::Result<(), mlua::Error> = engine
            .lua
            .load(
                r#"
                helix.floats.create({
                    placement = { type = "centered", width = 20, height = 5 },
                    render = function() end,
                })
                "#,
            )
            .exec();

        assert!(result.is_err());
        assert!(engine.ui_callbacks.read().is_empty());
    }

    #[test]
    fn test_event_registration() {
        struct TestEventHost;

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                Ok(crate::contract::SubscriptionHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn unsubscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _handle: crate::contract::SubscriptionHandle,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo> {
                vec![]
            }
        }

        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_event_host(Box::new(TestEventHost));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let result: std::result::Result<(), mlua::Error> = engine
            .lua
            .load(
                r#"
            local subscription = helix.events.subscribe("document_opened", function(event) end)
            assert(subscription ~= nil)
            assert(subscription:id() == 1)
            "#,
            )
            .exec();

        assert!(result.is_ok());
    }

    #[test]
    fn test_event_unsubscribe_removes_handler() {
        #[derive(Clone)]
        struct TestEventHost {
            unsubscribed: Arc<Mutex<Vec<crate::contract::SubscriptionHandle>>>,
        }

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                Ok(crate::contract::SubscriptionHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn unsubscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                handle: crate::contract::SubscriptionHandle,
            ) -> crate::contract::ContractResult<()> {
                self.unsubscribed.lock().push(handle);
                Ok(())
            }

            fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo> {
                vec![]
            }
        }

        let unsubscribed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_event_host(Box::new(TestEventHost {
            unsubscribed: Arc::clone(&unsubscribed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let result: std::result::Result<(), mlua::Error> = engine
            .lua
            .load(
                r#"
            local subscription = helix.events.subscribe("document_opened", function(event) end)
            helix.events.unsubscribe(subscription)
            "#,
            )
            .exec();

        assert!(result.is_ok());
        assert!(engine
            .contract_event_handlers
            .read()
            .get(&crate::contract::events::EventKind::DocumentOpened)
            .is_none());
        assert_eq!(unsubscribed.lock().len(), 1);
    }

    #[test]
    fn test_event_unsubscribe_failure_preserves_handler() {
        #[derive(Clone)]
        struct TestEventHost {
            unsubscribed: Arc<Mutex<Vec<crate::contract::SubscriptionHandle>>>,
        }

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                Ok(crate::contract::SubscriptionHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn unsubscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                handle: crate::contract::SubscriptionHandle,
            ) -> crate::contract::ContractResult<()> {
                self.unsubscribed.lock().push(handle);
                Err(crate::contract::ContractError::stale_handle(
                    handle.to_string(),
                ))
            }

            fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo> {
                vec![]
            }
        }

        let unsubscribed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_event_host(Box::new(TestEventHost {
            unsubscribed: Arc::clone(&unsubscribed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let result: std::result::Result<(), mlua::Error> = engine
            .lua
            .load(
                r#"
            local subscription = helix.events.subscribe("document_opened", function(event) end)
            helix.events.unsubscribe(subscription)
            "#,
            )
            .exec();

        assert!(result.is_err());
        let handlers = engine.contract_event_handlers.read();
        let entries = handlers
            .get(&crate::contract::events::EventKind::DocumentOpened)
            .expect("handler should remain registered after host rejection");
        assert_eq!(entries.len(), 1);
        assert_eq!(unsubscribed.lock().len(), 1);
    }

    #[test]
    fn test_event_unsubscribe_rejects_foreign_subscription() {
        #[derive(Clone)]
        struct TestEventHost {
            unsubscribed: Arc<Mutex<Vec<crate::contract::SubscriptionHandle>>>,
        }

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                Ok(crate::contract::SubscriptionHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn unsubscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                handle: crate::contract::SubscriptionHandle,
            ) -> crate::contract::ContractResult<()> {
                self.unsubscribed.lock().push(handle);
                Ok(())
            }

            fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo> {
                vec![]
            }
        }

        let unsubscribed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "owner-plugin", 1);
        register_loaded_plugin(&engine, "caller-plugin", 2);
        engine.set_event_host(Box::new(TestEventHost {
            unsubscribed: Arc::clone(&unsubscribed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "owner-plugin",
            r#"
            subscription = helix.events.subscribe("document_opened", function(event) end)
            "#,
        )
        .unwrap();

        let result = exec_as(
            &engine,
            "caller-plugin",
            r#"
            helix.events.unsubscribe(subscription)
            "#,
        );

        let err = result.expect_err("foreign subscription should be rejected");
        assert!(err.to_string().contains("permission denied"));
        let handlers = engine.contract_event_handlers.read();
        let entries = handlers
            .get(&crate::contract::events::EventKind::DocumentOpened)
            .expect("handler should remain registered after permission denial");
        assert_eq!(entries.len(), 1);
        assert!(unsubscribed.lock().is_empty());
    }

    #[test]
    fn test_reset_clears_event_subscriptions() {
        #[derive(Clone)]
        struct TestEventHost {
            unsubscribed: Arc<Mutex<Vec<crate::contract::SubscriptionHandle>>>,
        }

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                Ok(crate::contract::SubscriptionHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn unsubscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                handle: crate::contract::SubscriptionHandle,
            ) -> crate::contract::ContractResult<()> {
                self.unsubscribed.lock().push(handle);
                Ok(())
            }

            fn event_catalog(&self) -> Vec<crate::contract::metadata::EventKindInfo> {
                vec![]
            }
        }

        let unsubscribed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_event_host(Box::new(TestEventHost {
            unsubscribed: Arc::clone(&unsubscribed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        engine
            .lua
            .load(r#"helix.events.subscribe("document_opened", function(event) end)"#)
            .exec()
            .unwrap();

        engine.reset().unwrap();

        assert!(engine.contract_event_handlers.read().is_empty());
        assert_eq!(unsubscribed.lock().len(), 1);
    }

    #[test]
    fn test_command_update_and_remove() {
        #[derive(Clone)]
        struct TestCommandHost {
            registered: Arc<Mutex<Vec<crate::contract::requests::CommandDefinition>>>,
            updated: Arc<Mutex<Vec<crate::contract::requests::CommandUpdateRequest>>>,
            removed: Arc<Mutex<Vec<crate::contract::requests::CommandRemoveRequest>>>,
        }

        impl crate::contract::host::PluginCommandHost for TestCommandHost {
            fn register_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                def: crate::contract::requests::CommandDefinition,
            ) -> crate::contract::ContractResult<crate::contract::CommandHandle> {
                self.registered.lock().push(def);
                Ok(crate::contract::CommandHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::CommandUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                self.updated.lock().push(req);
                Ok(())
            }

            fn remove_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::CommandRemoveRequest,
            ) -> crate::contract::ContractResult<()> {
                self.removed.lock().push(req);
                Ok(())
            }

            fn run_command(
                &mut self,
                _req: crate::contract::requests::RunCommandRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }
        }

        let registered = Arc::new(Mutex::new(Vec::new()));
        let updated = Arc::new(Mutex::new(Vec::new()));
        let removed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_command_host(Box::new(TestCommandHost {
            registered: Arc::clone(&registered),
            updated: Arc::clone(&updated),
            removed: Arc::clone(&removed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        engine
            .lua
            .load(
                r#"
            command = helix.commands.register({
                name = "old_command",
                doc = "Old command",
                handler = function() end,
            })
            command:update({
                name = "new_command",
                doc = "New command",
                args = { "path" },
                handler = function() end,
            })
            "#,
            )
            .exec()
            .unwrap();

        let commands = engine.get_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "new_command");
        assert_eq!(commands[0].doc, "New command");
        assert_eq!(commands[0].args.as_deref(), Some("path"));
        assert_eq!(registered.lock().len(), 1);
        assert_eq!(updated.lock().len(), 1);

        engine
            .lua
            .load("helix.commands.remove(command)")
            .exec()
            .unwrap();

        assert!(engine.get_commands().is_empty());
        assert_eq!(removed.lock().len(), 1);
    }

    #[test]
    fn test_command_update_and_remove_reject_foreign_handles() {
        #[derive(Clone)]
        struct TestCommandHost {
            updated: Arc<Mutex<Vec<crate::contract::requests::CommandUpdateRequest>>>,
            removed: Arc<Mutex<Vec<crate::contract::requests::CommandRemoveRequest>>>,
        }

        impl crate::contract::host::PluginCommandHost for TestCommandHost {
            fn register_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                _def: crate::contract::requests::CommandDefinition,
            ) -> crate::contract::ContractResult<crate::contract::CommandHandle> {
                Ok(crate::contract::CommandHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::CommandUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                self.updated.lock().push(req);
                Ok(())
            }

            fn remove_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::CommandRemoveRequest,
            ) -> crate::contract::ContractResult<()> {
                self.removed.lock().push(req);
                Ok(())
            }

            fn run_command(
                &mut self,
                _req: crate::contract::requests::RunCommandRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }
        }

        let updated = Arc::new(Mutex::new(Vec::new()));
        let removed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "owner-plugin", 1);
        register_loaded_plugin(&engine, "caller-plugin", 2);
        engine.set_command_host(Box::new(TestCommandHost {
            updated: Arc::clone(&updated),
            removed: Arc::clone(&removed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "owner-plugin",
            r#"
            command = helix.commands.register({
                name = "owned_command",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let update_result = exec_as(
            &engine,
            "caller-plugin",
            "command:update({ doc = 'stolen' })",
        );
        let update_err = update_result.expect_err("foreign command update should be rejected");
        assert!(update_err.to_string().contains("permission denied"));

        let remove_result = exec_as(&engine, "caller-plugin", "helix.commands.remove(command)");
        let remove_err = remove_result.expect_err("foreign command remove should be rejected");
        assert!(remove_err.to_string().contains("permission denied"));

        assert!(updated.lock().is_empty());
        assert!(removed.lock().is_empty());
        assert_eq!(engine.get_commands().len(), 1);
    }

    #[test]
    fn test_command_execute_falls_back_to_registered_lua_command() {
        #[derive(Clone)]
        struct TestCommandHost {
            registered: Arc<Mutex<Vec<crate::contract::requests::CommandDefinition>>>,
            run: Arc<Mutex<Vec<crate::contract::requests::RunCommandRequest>>>,
        }

        impl crate::contract::host::PluginCommandHost for TestCommandHost {
            fn register_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                def: crate::contract::requests::CommandDefinition,
            ) -> crate::contract::ContractResult<crate::contract::CommandHandle> {
                self.registered.lock().push(def);
                Ok(crate::contract::CommandHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::CommandUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn remove_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::CommandRemoveRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn run_command(
                &mut self,
                req: crate::contract::requests::RunCommandRequest,
            ) -> crate::contract::ContractResult<()> {
                self.run.lock().push(req);
                Err(crate::contract::ContractError::not_found("command"))
            }
        }

        let registered = Arc::new(Mutex::new(Vec::new()));
        let run = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "owner-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        engine.plugin_registry.write().insert(
            "caller-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(2).unwrap()),
        );
        engine.set_command_host(Box::new(TestCommandHost {
            registered: Arc::clone(&registered),
            run: Arc::clone(&run),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "owner-plugin",
            r#"
            helix.commands.register({
                name = "owned_command",
                handler = function(args)
                    _G.executed_args = table.concat(args, ",")
                end,
            })
            "#,
        )
        .unwrap();

        exec_as(
            &engine,
            "caller-plugin",
            r#"
            helix.commands.execute("owned_command", { "one", "two" })
            "#,
        )
        .unwrap();

        assert_eq!(registered.lock().len(), 1);
        assert_eq!(run.lock().len(), 1);
        assert_eq!(run.lock()[0].name, "owned_command");
        assert_eq!(run.lock()[0].args, vec!["one", "two"]);

        let executed_args: String = engine.lua.globals().get("executed_args").unwrap();
        assert_eq!(executed_args, "one,two");
    }

    #[test]
    fn test_unknown_ui_callback_does_not_remove_persistent_render_callback() {
        let engine = LuaEngine::new().unwrap();
        let callback_id = UiCallbackId::new(1).unwrap();
        let callback_key = PluginCallbackKey::new("test-plugin".into(), callback_id);
        let callback_ref = engine
            .lua
            .create_registry_value(engine.lua.create_function(|_, _: LuaValue| Ok(())).unwrap())
            .unwrap();
        engine
            .ui_callbacks
            .write()
            .insert(callback_key.clone(), callback_ref);

        let mut editor = test_editor();
        engine
            .handle_ui_callback(
                &mut editor,
                callback_id,
                crate::contract::value::DynamicValue::Nil,
            )
            .unwrap();

        assert!(engine.ui_callbacks.read().contains_key(&callback_key));
    }

    #[test]
    fn test_ui_callback_tokens_reject_foreign_suspension() {
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "plugin-a", 1);
        register_loaded_plugin(&engine, "plugin-b", 2);
        engine.set_ui_host(Box::new(TestUiHost { next: 41 }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "plugin-a",
            r#"
            issued_callback = helix.ui._raw.start_prompt("name?")
            "#,
        )
        .unwrap();

        assert_eq!(engine.pending_ui_callbacks.read().len(), 1);
        let err = exec_as(
            &engine,
            "plugin-b",
            r#"
            local co = coroutine.create(function() end)
            helix.ui._raw.store_suspended(co, issued_callback)
            "#,
        )
        .expect_err("foreign plugin must not bind another plugin's UI callback");
        assert!(err.to_string().contains("does not own UI callback"));
        assert!(engine.suspended_coroutines.read().is_empty());
        assert_eq!(engine.pending_ui_callbacks.read().len(), 1);

        exec_as(
            &engine,
            "plugin-a",
            r#"
            local co = coroutine.create(function() end)
            helix.ui._raw.store_suspended(co, issued_callback)
            "#,
        )
        .unwrap();
        assert!(engine.pending_ui_callbacks.read().is_empty());
        assert_eq!(engine.suspended_coroutines.read().len(), 1);
    }

    #[test]
    fn test_ui_callback_tokens_must_be_pending_before_suspension() {
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "plugin-a", 1);
        engine.set_ui_host(Box::new(TestUiHost { next: 41 }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let unissued = engine
            .lua
            .create_userdata(UiCallbackToken::from_raw(NonZeroU64::new(999).unwrap()))
            .unwrap();
        engine
            .lua
            .globals()
            .set("unissued_callback", unissued)
            .unwrap();

        let err = exec_as(
            &engine,
            "plugin-a",
            r#"
            local co = coroutine.create(function() end)
            helix.ui._raw.store_suspended(co, unissued_callback)
            "#,
        )
        .expect_err("plugin must not bind a callback token the UI host never issued");
        assert!(err.to_string().contains("was not issued"));
        assert!(engine.suspended_coroutines.read().is_empty());
    }

    #[test]
    fn test_reset_clears_command_registrations() {
        #[derive(Clone)]
        struct TestCommandHost {
            removed: Arc<Mutex<Vec<crate::contract::requests::CommandRemoveRequest>>>,
        }

        impl crate::contract::host::PluginCommandHost for TestCommandHost {
            fn register_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                _def: crate::contract::requests::CommandDefinition,
            ) -> crate::contract::ContractResult<crate::contract::CommandHandle> {
                Ok(crate::contract::CommandHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                _req: crate::contract::requests::CommandUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }

            fn remove_command(
                &mut self,
                _plugin: crate::contract::PluginId,
                req: crate::contract::requests::CommandRemoveRequest,
            ) -> crate::contract::ContractResult<()> {
                self.removed.lock().push(req);
                Ok(())
            }

            fn run_command(
                &mut self,
                _req: crate::contract::requests::RunCommandRequest,
            ) -> crate::contract::ContractResult<()> {
                Ok(())
            }
        }

        let removed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        engine.plugin_registry.write().insert(
            "test-plugin".into(),
            PluginId::from_raw(NonZeroU64::new(1).unwrap()),
        );
        set_current_plugin(&engine, "test-plugin");
        engine.set_command_host(Box::new(TestCommandHost {
            removed: Arc::clone(&removed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        engine
            .lua
            .load(
                r#"
            helix.commands.register({
                name = "temporary_command",
                handler = function() end,
            })
            "#,
            )
            .exec()
            .unwrap();

        engine.reset().unwrap();

        assert!(engine.get_commands().is_empty());
        assert_eq!(removed.lock().len(), 1);
    }

    #[test]
    fn test_sandbox() {
        let engine = LuaEngine::new().unwrap();

        // These should fail due to sandboxing
        let result: std::result::Result<(), mlua::Error> =
            engine.lua.load("os.execute('ls')").exec();
        assert!(result.is_err());

        let result: std::result::Result<(), mlua::Error> =
            engine.lua.load("io.open('/etc/passwd')").exec();
        assert!(result.is_err());
    }

    #[test]
    fn test_full_api_availability() {
        let engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let code = r#"
            -- Test Workspace API
            assert(helix.workspace ~= nil)
            assert(helix.workspace.focused_document ~= nil)
            assert(helix.workspace.set_mode ~= nil)
            assert(helix.workspace.documents ~= nil)
            assert(helix.workspace.views ~= nil)
            assert(helix.workspace.editor_config ~= nil)

            -- Test UI API
            assert(helix.ui ~= nil)
            assert(helix.ui.notify ~= nil)
            assert(helix.ui.prompt ~= nil)
            assert(helix.ui.confirm ~= nil)
            assert(helix.ui.pick ~= nil)
            assert(helix.ui._raw ~= nil)

            -- Test Documents API
            assert(helix.documents ~= nil)
            assert(helix.documents.list ~= nil)
            assert(helix.documents.open ~= nil)

            -- Test Views API
            assert(helix.views ~= nil)
            assert(helix.views.list ~= nil)

            -- Test Events API
            assert(helix.events ~= nil)
            assert(helix.events.kind ~= nil)
            assert(helix.events.subscribe ~= nil)
            assert(helix.events.unsubscribe ~= nil)
            assert(helix.events.on == nil)

            -- Test Commands API
            assert(helix.commands ~= nil)
            assert(helix.commands.register ~= nil)
            assert(helix.commands.update ~= nil)
            assert(helix.commands.remove ~= nil)
            assert(helix.commands.execute ~= nil)

            -- Test Registers API
            assert(helix.registers ~= nil)
            assert(helix.registers.get ~= nil)
            assert(helix.registers.set ~= nil)

            -- Test helix.async
            assert(helix.async ~= nil)
            assert(type(helix.async) == "function")

            -- Test helix.config
            assert(helix.config ~= nil)

            -- Old modules should NOT exist
            assert(helix.editor == nil)
            assert(helix.get_config == nil)
            assert(helix.register_command == nil)
        "#;

        let result: std::result::Result<(), mlua::Error> = engine.lua.load(code).exec();
        assert!(result.is_ok(), "API check failed: {:?}", result.err());
    }
}
