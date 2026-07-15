use crate::contract::{
    CommandHandle, KeymapHandle, PanelHandle, PluginId, PluginOperationToken, SubscriptionHandle,
    UiCallbackToken,
};
use crate::error::{PluginError, Result};
use crate::types::{PluginCallbackKey, UiCallbackId};
#[cfg(test)]
use helix_view::Editor;
use log::{error, warn};
use mlua::prelude::*;
use mlua::{HookTriggers, RegistryKey, VmState};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
mod context;
#[cfg(test)]
pub use context::{
    with_current_editor, with_current_editor_mut, with_editor_context, with_editor_context_ref,
};

pub(crate) struct CurrentPluginName(pub Arc<RwLock<Option<String>>>);

pub(crate) struct PluginRoots(pub Arc<RwLock<HashMap<String, PathBuf>>>);

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
        DynamicValue::Array(values) => {
            let table = lua.create_table().map_err(PluginError::LuaError)?;
            for (index, value) in values.iter().enumerate() {
                table
                    .set(index + 1, dynamic_value_to_lua(lua, value)?)
                    .map_err(PluginError::LuaError)?;
            }
            LuaValue::Table(table)
        }
        DynamicValue::Object(values) => {
            let table = lua.create_table().map_err(PluginError::LuaError)?;
            for (key, value) in values {
                table
                    .set(key.as_str(), dynamic_value_to_lua(lua, value)?)
                    .map_err(PluginError::LuaError)?;
            }
            LuaValue::Table(table)
        }
    })
}

fn task_result_to_lua(lua: &Lua, result: crate::contract::PluginTaskResult) -> Result<LuaValue> {
    Ok(match result {
        crate::contract::PluginTaskResult::Unit => LuaValue::Nil,
        crate::contract::PluginTaskResult::Document(handle) => LuaValue::UserData(
            lua.create_userdata(api::facade::LuaDocumentHandle(handle))
                .map_err(PluginError::LuaError)?,
        ),
        crate::contract::PluginTaskResult::Value(value) => dynamic_value_to_lua(lua, &value)?,
        crate::contract::PluginTaskResult::SyntaxCaptures(captures) => {
            let table = lua.create_table().map_err(PluginError::LuaError)?;
            for (index, capture) in captures.into_iter().enumerate() {
                let item = lua.create_table().map_err(PluginError::LuaError)?;
                item.set("name", capture.name)
                    .map_err(PluginError::LuaError)?;
                item.set("kind", capture.kind)
                    .map_err(PluginError::LuaError)?;
                for (field, position) in [("start", capture.start), ("end", capture.end)] {
                    let value = lua.create_table().map_err(PluginError::LuaError)?;
                    value
                        .set("line", position.line)
                        .map_err(PluginError::LuaError)?;
                    value
                        .set("column", position.column)
                        .map_err(PluginError::LuaError)?;
                    item.set(field, value).map_err(PluginError::LuaError)?;
                }
                table.set(index + 1, item).map_err(PluginError::LuaError)?;
            }
            LuaValue::Table(table)
        }
    })
}

pub mod api;
pub mod loader;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LuaUiCallbackToken(UiCallbackToken);

impl From<UiCallbackToken> for LuaUiCallbackToken {
    fn from(token: UiCallbackToken) -> Self {
        Self(token)
    }
}

impl FromLua for LuaUiCallbackToken {
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

impl LuaUserData for LuaUiCallbackToken {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LuaPluginOperationToken(PluginOperationToken);

impl From<PluginOperationToken> for LuaPluginOperationToken {
    fn from(token: PluginOperationToken) -> Self {
        Self(token)
    }
}

impl FromLua for LuaPluginOperationToken {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|token| *token),
            _ => Err(mlua::Error::FromLuaConversionError {
                from: value.type_name(),
                to: "PluginOperationToken".to_string(),
                message: Some("expected a PluginOperationToken userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaPluginOperationToken {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
    }
}

// ---------------------------------------------------------------------------
// Suspended coroutine tracking — for coroutine-based async UI
// ---------------------------------------------------------------------------

/// A Lua thread waiting for a typed host response.
pub(crate) struct SuspendedCoroutine {
    /// Registry key for the `mlua::Thread` so it survives GC.
    pub(crate) thread_key: RegistryKey,
    /// Which plugin owns this coroutine (for logging / error attribution).
    pub(crate) plugin_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AwaitKey {
    Ui(UiCallbackId),
    Operation(PluginOperationToken),
}

/// UI callback tokens issued by the frontend but not yet bound to a coroutine.
/// A token must be consumed by the same plugin before a suspended coroutine is
/// stored under it.
pub(crate) struct PendingUiCallbackRegistry(pub Arc<RwLock<HashMap<UiCallbackId, String>>>);
pub(crate) struct PendingOperationRegistry(pub Arc<RwLock<HashMap<PluginOperationToken, String>>>);

pub(crate) struct RegisteredCommand {
    pub(crate) handle: CommandHandle,
    pub(crate) plugin_name: String,
    pub(crate) metadata: crate::types::CommandMetadata,
    pub(crate) callback_ref: RegistryKey,
}

#[derive(Debug, Clone)]
pub struct RegisteredPanelCallbacks {
    pub plugin_name: String,
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

    pub(crate) fn get_by_handle(&self, handle: CommandHandle) -> Option<&RegisteredCommand> {
        self.by_handle.get(&handle)
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
    pub Arc<RwLock<HashMap<AwaitKey, SuspendedCoroutine>>>,
);

pub(crate) fn await_key_from_lua(value: LuaValue) -> LuaResult<AwaitKey> {
    if let LuaValue::UserData(userdata) = &value {
        if let Ok(token) = userdata.borrow::<LuaUiCallbackToken>() {
            let callback = UiCallbackId::new(token.0.raw().get()).ok_or_else(|| {
                LuaError::RuntimeError("coroutine yielded a zero await token".into())
            })?;
            return Ok(AwaitKey::Ui(callback));
        }
        if let Ok(token) = userdata.borrow::<LuaPluginOperationToken>() {
            return Ok(AwaitKey::Operation(token.0));
        }
    }
    Err(LuaError::RuntimeError(
        "coroutine yielded an invalid await token".into(),
    ))
}

pub(crate) fn suspend_coroutine(
    lua: &Lua,
    thread: &LuaThread,
    plugin_name: &str,
    key: AwaitKey,
) -> LuaResult<()> {
    let registry = lua
        .app_data_ref::<SuspendedCoroutineRegistry>()
        .ok_or_else(|| LuaError::RuntimeError("suspended coroutine registry unavailable".into()))?;
    if registry.0.read().contains_key(&key) {
        return Err(LuaError::RuntimeError(format!(
            "await token {key:?} is already bound to a coroutine"
        )));
    }
    let thread_key = lua.create_registry_value(thread.clone())?;

    match key {
        AwaitKey::Ui(callback) => claim_pending_ui_callback(lua, plugin_name, callback)?,
        AwaitKey::Operation(operation) => {
            let registry = lua
                .app_data_ref::<PendingOperationRegistry>()
                .ok_or_else(|| {
                    LuaError::RuntimeError("pending operation registry unavailable".into())
                })?;
            let mut pending = registry.0.write();
            match pending.get(&operation) {
                Some(owner) if owner == plugin_name => {
                    pending.remove(&operation);
                }
                Some(owner) => {
                    return Err(LuaError::RuntimeError(format!(
                        "permission denied: plugin '{plugin_name}' does not own operation {} (owner: '{owner}')",
                        operation.raw()
                    )))
                }
                None => {
                    return Err(LuaError::RuntimeError(format!(
                        "invalid request: operation {} was not issued to plugin '{plugin_name}'",
                        operation.raw()
                    )))
                }
            }
        }
    }
    registry.0.write().insert(
        key,
        SuspendedCoroutine {
            thread_key,
            plugin_name: plugin_name.to_owned(),
        },
    );
    Ok(())
}

pub(crate) fn suspend_coroutine_yield(
    lua: &Lua,
    thread: &LuaThread,
    plugin_name: &str,
    yielded: LuaMultiValue,
) -> LuaResult<()> {
    if thread.status() != LuaThreadStatus::Resumable {
        return Ok(());
    }
    let value = yielded
        .into_iter()
        .next()
        .ok_or_else(|| LuaError::RuntimeError("coroutine yielded without an await token".into()))?;
    let key = await_key_from_lua(value)?;
    suspend_coroutine(lua, thread, plugin_name, key)
}

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
    pub(crate) failures: u32,
}

type ContractEventHandlers =
    HashMap<crate::contract::events::EventKind, Vec<RegisteredEventHandler>>;

/// Wrapper to store contract event handlers in Lua app data.
pub(crate) struct ContractEventHandlersWrapper(pub Arc<RwLock<ContractEventHandlers>>);

pub(crate) struct UiHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginUiHost + Send + Sync>>>,
);

pub(crate) struct TaskHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginTaskHost + Send + Sync>>>,
);

pub(crate) struct PanelHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>>>,
);
pub(crate) struct CommandHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>>>,
);

pub(crate) struct KeymapHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginKeymapHost + Send + Sync>>>,
);

pub(crate) struct KeymapRegistryWrapper(pub Arc<RwLock<HashMap<KeymapHandle, String>>>);

pub(crate) struct EventHostWrapper(
    pub Arc<Mutex<Box<dyn crate::contract::host::PluginEventHost + Send + Sync>>>,
);

pub(crate) trait FacadeHost: Send + Sync {
    fn query(&self) -> &dyn crate::contract::host::PluginFacadeQueryHost;
    fn mutation(&mut self) -> &mut dyn crate::contract::host::PluginFacadeMutationHost;
}

impl<T> FacadeHost for T
where
    T: crate::contract::host::PluginFacadeQueryHost
        + crate::contract::host::PluginFacadeMutationHost
        + Send
        + Sync
        + 'static,
{
    fn query(&self) -> &dyn crate::contract::host::PluginFacadeQueryHost {
        self
    }

    fn mutation(&mut self) -> &mut dyn crate::contract::host::PluginFacadeMutationHost {
        self
    }
}

pub(crate) struct FacadeHostWrapper(pub Arc<Mutex<Box<dyn FacadeHost>>>);

pub(crate) struct HostApiMetadata(pub crate::contract::metadata::ApiMetadata);

pub(crate) struct CommandRegistryWrapper(pub Arc<RwLock<CommandRegistry>>);

pub(crate) struct LoadedPluginRegistryWrapper(pub Arc<RwLock<LoadedPluginRegistry>>);

pub(crate) struct PanelCallbackRegistry(
    pub Arc<RwLock<HashMap<PanelHandle, RegisteredPanelCallbacks>>>,
);
const WATCHDOG_STEP: u32 = 10_000;
const EVENT_FAILURE_LIMIT: u32 = 10;

fn setup_sandbox(
    lua: &Lua,
    config: &crate::types::PluginConfig,
    plugin_roots: Arc<RwLock<HashMap<String, PathBuf>>>,
) -> Result<()> {
    lua.set_memory_limit(config.max_memory).map_err(|e| {
        PluginError::InitializationFailed(format!("Failed to set memory limit: {e}"))
    })?;
    lua.set_app_data(PluginRoots(plugin_roots));

    lua.load(
        r#"
        os.execute = nil
        os.exit = nil
        io = nil
        package = nil
        load = nil
        loadstring = nil
        loadfile = nil
        dofile = nil
        "#,
    )
    .exec()
    .map_err(|e| PluginError::InitializationFailed(format!("Failed to setup sandbox: {e}")))?;

    let require = lua.create_function(scoped_require).map_err(|e| {
        PluginError::InitializationFailed(format!("Failed to install require: {e}"))
    })?;
    lua.globals().set("require", require).map_err(|e| {
        PluginError::InitializationFailed(format!("Failed to install require: {e}"))
    })?;
    Ok(())
}

fn scoped_require(lua: &Lua, module: String) -> LuaResult<LuaValue> {
    if module.is_empty()
        || module.contains("..")
        || module.contains('/')
        || module.contains('\\')
        || module.contains(':')
    {
        return Err(LuaError::RuntimeError(format!(
            "module '{module}' is outside the plugin directory"
        )));
    }

    let plugin_name = current_plugin_name(lua)?;
    let root = {
        let roots = lua
            .app_data_ref::<PluginRoots>()
            .ok_or_else(|| LuaError::RuntimeError("plugin roots unavailable".into()))?;
        let root = roots.0.read().get(&plugin_name).cloned().ok_or_else(|| {
            LuaError::RuntimeError(format!("plugin root not found: {plugin_name}"))
        })?;
        root
    };

    let relative = module.replace('.', std::path::MAIN_SEPARATOR_STR);
    let candidate = root.join(relative).with_extension("lua");
    let canonical = candidate
        .canonicalize()
        .map_err(|_| LuaError::RuntimeError(format!("module '{module}' not found")))?;
    if !canonical.starts_with(&root) {
        return Err(LuaError::RuntimeError(format!(
            "module '{module}' is outside the plugin directory"
        )));
    }

    let code = std::fs::read_to_string(&canonical).map_err(LuaError::external)?;
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(module.as_str());
    let value: LuaValue = lua.load(&code).set_name(name).eval()?;
    Ok(match value {
        LuaValue::Nil => LuaValue::Boolean(true),
        value => value,
    })
}

fn canonical_root(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(PluginError::IoError)
}

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
    keymaps: Arc<RwLock<HashMap<KeymapHandle, String>>>,
    /// Loaded plugin handles keyed by plugin name and reverse lookup by id.
    plugin_registry: Arc<RwLock<LoadedPluginRegistry>>,
    /// Next available plugin handle.
    next_plugin_handle: Arc<std::sync::atomic::AtomicU64>,
    /// Rust-owned current plugin context for callbacks and API ownership checks.
    current_plugin_name: Arc<RwLock<Option<String>>>,
    /// Canonical plugin root directories keyed by plugin name.
    plugin_roots: Arc<RwLock<HashMap<String, PathBuf>>>,
    /// UI callbacks: (plugin_name, callback_id) -> callback_ref
    ui_callbacks: Arc<RwLock<HashMap<PluginCallbackKey, RegistryKey>>>,
    /// Panel render/event callback metadata keyed by panel handle.
    panel_callbacks: Arc<RwLock<HashMap<PanelHandle, RegisteredPanelCallbacks>>>,
    /// Next available UI callback identity.
    next_ui_callback_id: Arc<std::sync::atomic::AtomicU64>,
    /// Frontend UI host.
    ui_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginUiHost + Send + Sync>>>>,
    task_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginTaskHost + Send + Sync>>>>,
    /// Frontend panel host.
    panel_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>>>>,
    resource_host:
        Option<Arc<Mutex<Box<dyn crate::contract::host::PluginResourceHost + Send + Sync>>>>,
    /// Frontend command host.
    command_host:
        Option<Arc<Mutex<Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>>>>,
    keymap_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginKeymapHost + Send + Sync>>>>,
    /// Frontend event host.
    event_host: Option<Arc<Mutex<Box<dyn crate::contract::host::PluginEventHost + Send + Sync>>>>,
    /// Suspended coroutines waiting for UI responses, keyed by callback token identity.
    suspended_coroutines: Arc<RwLock<HashMap<AwaitKey, SuspendedCoroutine>>>,
    /// UI callback tokens issued by the frontend and awaiting coroutine binding.
    pending_ui_callbacks: Arc<RwLock<HashMap<UiCallbackId, String>>>,
    pending_operations: Arc<RwLock<HashMap<PluginOperationToken, String>>>,
    api_metadata_override: Option<crate::contract::metadata::ApiMetadata>,
}

impl LuaEngine {
    /// Create a new Lua engine
    pub fn new() -> Result<Self> {
        let lua = Lua::new();

        let contract_event_handlers = Arc::new(RwLock::new(HashMap::new()));
        let commands = Arc::new(RwLock::new(CommandRegistry::default()));
        let keymaps = Arc::new(RwLock::new(HashMap::new()));
        let plugin_registry = Arc::new(RwLock::new(LoadedPluginRegistry::default()));
        let next_plugin_handle = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let current_plugin_name = Arc::new(RwLock::new(None));
        let plugin_roots = Arc::new(RwLock::new(HashMap::new()));
        lua.set_app_data(CurrentPluginName(Arc::clone(&current_plugin_name)));
        setup_sandbox(
            &lua,
            &crate::types::PluginConfig::default(),
            Arc::clone(&plugin_roots),
        )?;
        let ui_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let panel_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let next_ui_callback_id = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let suspended_coroutines = Arc::new(RwLock::new(HashMap::new()));
        let pending_ui_callbacks = Arc::new(RwLock::new(HashMap::new()));
        let pending_operations = Arc::new(RwLock::new(HashMap::new()));
        lua.set_app_data(PendingUiCallbackRegistry(Arc::clone(&pending_ui_callbacks)));
        lua.set_app_data(PendingOperationRegistry(Arc::clone(&pending_operations)));

        Ok(Self {
            lua,
            contract_event_handlers,
            plugins: HashMap::new(),
            commands,
            keymaps,
            plugin_registry,
            next_plugin_handle,
            current_plugin_name,
            plugin_roots,
            ui_callbacks,
            panel_callbacks,
            next_ui_callback_id,
            ui_host: None,
            task_host: None,
            panel_host: None,
            resource_host: None,
            command_host: None,
            keymap_host: None,
            event_host: None,
            suspended_coroutines,
            pending_ui_callbacks,
            pending_operations,
            api_metadata_override: None,
        })
    }

    /// Reset the Lua engine, clearing all state and plugins
    pub fn reset(&mut self) -> Result<()> {
        let lua = Lua::new();

        self.cancel_pending_operations();
        self.clear_event_subscriptions()?;
        self.clear_command_registrations()?;
        self.clear_keymap_registrations()?;
        self.release_plugin_resources()?;
        self.ui_callbacks.write().clear();
        self.panel_callbacks.write().clear();
        self.suspended_coroutines.write().clear();
        self.pending_ui_callbacks.write().clear();
        self.pending_operations.write().clear();
        self.current_plugin_name.write().take();
        self.lua = lua;
        self.lua
            .set_app_data(CurrentPluginName(Arc::clone(&self.current_plugin_name)));
        setup_sandbox(
            &self.lua,
            &crate::types::PluginConfig::default(),
            Arc::clone(&self.plugin_roots),
        )?;
        self.lua.set_app_data(PendingUiCallbackRegistry(Arc::clone(
            &self.pending_ui_callbacks,
        )));
        self.lua.set_app_data(PendingOperationRegistry(Arc::clone(
            &self.pending_operations,
        )));
        self.plugins.clear();
        self.plugin_registry.write().clear();
        self.plugin_roots.write().clear();

        Ok(())
    }

    fn cancel_pending_operations(&self) {
        let mut operations = self.pending_operations.read().clone();
        for (key, entry) in self.suspended_coroutines.read().iter() {
            if let AwaitKey::Operation(operation) = key {
                operations
                    .entry(*operation)
                    .or_insert_with(|| entry.plugin_name.clone());
            }
        }

        let Some(host) = &self.task_host else {
            return;
        };
        let registry = self.plugin_registry.read();
        let mut host = host.lock();
        for (operation, plugin_name) in operations {
            let Some(plugin) = registry.id_for_name(&plugin_name) else {
                warn!(
                    "cannot cancel plugin operation {}: plugin '{}' is no longer registered",
                    operation.raw(),
                    plugin_name
                );
                continue;
            };
            host.cancel(plugin, operation);
        }
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

    fn clear_keymap_registrations(&self) -> Result<()> {
        let handles = self
            .keymaps
            .read()
            .iter()
            .map(|(handle, plugin)| (*handle, plugin.clone()))
            .collect::<Vec<_>>();
        if let Some(host) = &self.keymap_host {
            let mut host = host.lock();
            for (keymap, plugin_name) in handles {
                let plugin = self.plugin_id_for_name(&plugin_name)?;
                match host.remove_keymap(plugin, crate::contract::KeymapRemoveRequest { keymap }) {
                    Ok(()) | Err(crate::contract::ContractError::StaleHandle { .. }) => {}
                    Err(error) => {
                        return Err(PluginError::InitializationFailed(format!(
                            "Failed to clear keymap contribution {keymap}: {error}"
                        )));
                    }
                }
            }
        }
        self.keymaps.write().clear();
        Ok(())
    }

    fn release_plugin_resources(&self) -> Result<()> {
        let Some(host) = &self.resource_host else {
            return Ok(());
        };
        let plugins = self
            .plugin_registry
            .read()
            .by_id
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut host = host.lock();
        let mut failures = Vec::new();
        for plugin in plugins {
            if let Err(error) = host.release_plugin_resources(plugin) {
                failures.push(format!("{plugin}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(PluginError::InitializationFailed(format!(
                "Failed to release plugin resources: {}",
                failures.join("; ")
            )))
        }
    }

    fn plugin_id_for_name(&self, plugin_name: &str) -> Result<PluginId> {
        self.plugin_registry
            .read()
            .id_for_name(plugin_name)
            .ok_or_else(|| {
                PluginError::InitializationFailed(format!("Plugin not registered: {plugin_name}"))
            })
    }

    fn with_facade_host<H, T>(&self, host: &H, f: impl FnOnce() -> Result<T>) -> Result<T>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        struct Guard<'lua> {
            lua: &'lua Lua,
            previous: Option<FacadeHostWrapper>,
        }

        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(previous) => {
                        self.lua.set_app_data(previous);
                    }
                    None => {
                        self.lua.remove_app_data::<FacadeHostWrapper>();
                    }
                }
            }
        }

        let wrapper = FacadeHostWrapper(Arc::new(Mutex::new(
            Box::new(host.clone()) as Box<dyn FacadeHost>
        )));
        let previous = self.lua.set_app_data(wrapper);
        let _guard = Guard {
            lua: &self.lua,
            previous,
        };
        f()
    }

    fn with_watchdog<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let max_instructions = self
            .lua
            .app_data_ref::<crate::types::PluginConfig>()
            .map(|config| config.max_instructions)
            .unwrap_or_else(|| crate::types::PluginConfig::default().max_instructions);

        if max_instructions == 0 {
            return f();
        }

        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let hook_count = Arc::clone(&count);
        self.lua.set_hook(
            HookTriggers::new().every_nth_instruction(WATCHDOG_STEP),
            move |_lua, _debug| {
                let executed = hook_count.fetch_add(
                    u64::from(WATCHDOG_STEP),
                    std::sync::atomic::Ordering::Relaxed,
                ) + u64::from(WATCHDOG_STEP);
                if executed > max_instructions {
                    return Err(LuaError::RuntimeError(format!(
                        "instruction watchdog exceeded {max_instructions} instructions"
                    )));
                }
                Ok(VmState::Continue)
            },
        );

        let result = f();
        self.lua.remove_hook();
        result
    }

    pub fn set_ui_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginUiHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(UiHostWrapper(Arc::clone(&host)));
        self.ui_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_task_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginTaskHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(TaskHostWrapper(Arc::clone(&host)));
        self.task_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_panel_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginPanelHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(PanelHostWrapper(Arc::clone(&host)));
        self.panel_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_resource_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginResourceHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.resource_host = Some(host);
    }

    pub fn set_command_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginCommandHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(CommandHostWrapper(Arc::clone(&host)));
        self.command_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_keymap_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginKeymapHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(KeymapHostWrapper(Arc::clone(&host)));
        self.keymap_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_event_host(
        &mut self,
        host: Box<dyn crate::contract::host::PluginEventHost + Send + Sync>,
    ) {
        let host = Arc::new(Mutex::new(host));
        self.lua.set_app_data(EventHostWrapper(Arc::clone(&host)));
        self.event_host = Some(host);
        self.publish_api_metadata();
    }

    pub fn set_api_metadata(&mut self, metadata: crate::contract::metadata::ApiMetadata) {
        self.api_metadata_override = Some(metadata);
        self.publish_api_metadata();
    }

    fn api_metadata(&self) -> crate::contract::metadata::ApiMetadata {
        use crate::contract::metadata::{ApiMetadata, Capability};

        if let Some(metadata) = &self.api_metadata_override {
            return metadata.clone();
        }

        let mut capabilities = vec![
            Capability::Query,
            Capability::Mutation,
            Capability::Splits,
            Capability::Tabs,
            Capability::Floats,
            Capability::Assistant,
        ];
        if self.ui_host.is_some() {
            capabilities.push(Capability::Ui);
        }
        if self.panel_host.is_some() {
            capabilities.push(Capability::Panels);
        }
        if self.command_host.is_some() {
            capabilities.push(Capability::Commands);
        }
        if self.keymap_host.is_some() {
            capabilities.push(Capability::Keymaps);
        }
        if self.event_host.is_some() {
            capabilities.push(Capability::Events);
        }
        if self.task_host.is_some() {
            capabilities.extend([
                Capability::Tasks,
                Capability::Syntax,
                Capability::Lsp,
                Capability::Themes,
            ]);
        }
        ApiMetadata::from_capabilities(capabilities)
    }

    fn publish_api_metadata(&self) {
        self.lua.set_app_data(HostApiMetadata(self.api_metadata()));
    }

    fn validate_plugin_capabilities(&self, plugin: &crate::types::Plugin) -> Result<()> {
        use crate::contract::metadata::Capability;
        use std::str::FromStr;

        let metadata = self.api_metadata();
        for name in &plugin.metadata.capabilities {
            let capability = Capability::from_str(name).map_err(PluginError::ConfigError)?;
            if !metadata.has_capability(capability) {
                return Err(PluginError::ConfigError(format!(
                    "plugin '{}' requires unavailable host capability '{name}'",
                    plugin.metadata.name
                )));
            }
        }
        Ok(())
    }

    /// Register the Helix API with Lua
    pub fn register_api(&self, config: crate::types::PluginConfig) -> Result<()> {
        let globals = self.lua.globals();
        self.lua.set_memory_limit(config.max_memory).map_err(|e| {
            PluginError::InitializationFailed(format!("Failed to set memory limit: {e}"))
        })?;

        if let Some(ref host) = self.ui_host {
            self.lua.set_app_data(UiHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.task_host {
            self.lua.set_app_data(TaskHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.panel_host {
            self.lua.set_app_data(PanelHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.command_host {
            self.lua.set_app_data(CommandHostWrapper(Arc::clone(host)));
        }

        if let Some(ref host) = self.keymap_host {
            self.lua.set_app_data(KeymapHostWrapper(Arc::clone(host)));
        }
        self.lua
            .set_app_data(KeymapRegistryWrapper(Arc::clone(&self.keymaps)));

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
            .set_app_data(PluginRoots(Arc::clone(&self.plugin_roots)));
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
        self.lua.set_app_data(PendingOperationRegistry(Arc::clone(
            &self.pending_operations,
        )));
        self.lua.set_app_data(config);
        self.publish_api_metadata();

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
    #[cfg(test)]
    pub fn execute_command_with_editor(
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
                self.with_watchdog(|| {
                    let result: LuaMultiValue = thread.resume(lua_args).map_err(|e| {
                        PluginError::CommandExecutionFailed(format!("Execution failed: {}", e))
                    })?;
                    self.handle_coroutine_yield(&thread, &plugin_name, result)
                })
            })
        })?;

        Ok(())
    }

    pub fn execute_command<H>(&self, host: &mut H, name: &str, args: Vec<String>) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
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

        self.with_facade_host(host, || {
            with_current_plugin_name(&self.lua, &plugin_name, || {
                self.with_watchdog(|| {
                    let result: LuaMultiValue = thread.resume(lua_args).map_err(|e| {
                        PluginError::CommandExecutionFailed(format!("Execution failed: {}", e))
                    })?;
                    self.handle_coroutine_yield(&thread, &plugin_name, result)
                })
            })
        })?;

        Ok(())
    }

    /// Execute a command by the exact handle returned at registration.
    pub fn execute_command_handle<H>(
        &self,
        host: &mut H,
        command: CommandHandle,
        args: Vec<String>,
    ) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let (plugin_name, callback) = {
            let commands = self.commands.read();
            let command = commands.get_by_handle(command).ok_or_else(|| {
                PluginError::CommandExecutionFailed(format!("Command not found: {command}"))
            })?;
            let callback = self
                .lua
                .registry_value(&command.callback_ref)
                .map_err(|error| {
                    PluginError::CommandExecutionFailed(format!(
                        "Failed to retrieve callback: {error}"
                    ))
                })?;
            (command.plugin_name.clone(), callback)
        };

        let thread = self.lua.create_thread(callback).map_err(|error| {
            PluginError::CommandExecutionFailed(format!("Failed to create coroutine: {error}"))
        })?;
        let lua_args = self
            .lua
            .create_sequence_from(args)
            .map_err(PluginError::LuaError)?;

        self.with_facade_host(host, || {
            with_current_plugin_name(&self.lua, &plugin_name, || {
                self.with_watchdog(|| {
                    let result: LuaMultiValue = thread.resume(lua_args).map_err(|error| {
                        PluginError::CommandExecutionFailed(format!("Execution failed: {error}"))
                    })?;
                    self.handle_coroutine_yield(&thread, &plugin_name, result)
                })
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
        suspend_coroutine_yield(&self.lua, thread, plugin_name, yielded)
            .map_err(PluginError::LuaError)
    }

    /// Get all registered commands metadata
    pub fn get_commands(&self) -> Vec<crate::types::CommandMetadata> {
        self.commands.read().metadata()
    }
    /// Handle a UI callback from the editor (prompt response, picker selection, etc.).
    ///
    /// If a coroutine yielded this callback token, resume it with the response value.
    /// Unknown callback tokens are ignored; persistent panel event callbacks use a
    /// separate registry and must not be consumed by UI responses.
    ///
    /// If the resumed coroutine yields *again* (chained async ops), it is re-stored
    /// under the new callback token identity.
    #[cfg(test)]
    pub fn handle_ui_callback_with_editor(
        &self,
        editor: &mut Editor,
        callback_id: UiCallbackId,
        value: crate::contract::value::DynamicValue,
    ) -> Result<()> {
        let suspended = self
            .suspended_coroutines
            .write()
            .remove(&AwaitKey::Ui(callback_id));
        if let Some(entry) = suspended {
            let thread: LuaThread = self
                .lua
                .registry_value(&entry.thread_key)
                .map_err(PluginError::LuaError)?;

            let lua_value = dynamic_value_to_lua(&self.lua, &value)?;

            with_editor_context(editor, || {
                with_current_plugin_name(&self.lua, &entry.plugin_name, || {
                    self.with_watchdog(|| {
                        let result: LuaMultiValue = thread.resume(lua_value).map_err(|e| {
                            PluginError::CommandExecutionFailed(format!(
                                "coroutine resume failed (plugin: {}): {}",
                                entry.plugin_name, e
                            ))
                        })?;
                        // Handle re-yield for chained operations.
                        self.handle_coroutine_yield(&thread, &entry.plugin_name, result)
                    })
                })
            })?;

            return Ok(());
        }
        self.pending_ui_callbacks.write().remove(&callback_id);
        Ok(())
    }

    /// Resume a UI coroutine against the facade host.
    pub fn handle_ui_callback<H>(
        &self,
        host: &mut H,
        callback_id: UiCallbackId,
        value: crate::contract::value::DynamicValue,
    ) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let suspended = self
            .suspended_coroutines
            .write()
            .remove(&AwaitKey::Ui(callback_id));
        if let Some(entry) = suspended {
            let thread: LuaThread = self
                .lua
                .registry_value(&entry.thread_key)
                .map_err(PluginError::LuaError)?;
            let lua_value = dynamic_value_to_lua(&self.lua, &value)?;

            self.with_facade_host(host, || {
                with_current_plugin_name(&self.lua, &entry.plugin_name, || {
                    self.with_watchdog(|| {
                        let result: LuaMultiValue = thread.resume(lua_value).map_err(|error| {
                            PluginError::CommandExecutionFailed(format!(
                                "coroutine resume failed (plugin: {}): {error}",
                                entry.plugin_name
                            ))
                        })?;
                        self.handle_coroutine_yield(&thread, &entry.plugin_name, result)
                    })
                })
            })?;
            return Ok(());
        }

        self.pending_ui_callbacks.write().remove(&callback_id);
        Ok(())
    }

    #[cfg(test)]
    pub fn handle_task_completion_with_editor(
        &self,
        editor: &mut Editor,
        operation: PluginOperationToken,
        result: crate::contract::ContractResult<crate::contract::PluginTaskResult>,
    ) -> Result<()> {
        with_editor_context(editor, || self.resume_task_completion(operation, result))?;
        Ok(())
    }

    pub fn handle_task_completion<H>(
        &self,
        host: &mut H,
        operation: PluginOperationToken,
        result: crate::contract::ContractResult<crate::contract::PluginTaskResult>,
    ) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        self.with_facade_host(host, || self.resume_task_completion(operation, result))
    }

    fn resume_task_completion(
        &self,
        operation: PluginOperationToken,
        result: crate::contract::ContractResult<crate::contract::PluginTaskResult>,
    ) -> Result<()> {
        let suspended = self
            .suspended_coroutines
            .write()
            .remove(&AwaitKey::Operation(operation));
        let Some(entry) = suspended else {
            self.pending_operations.write().remove(&operation);
            return Ok(());
        };
        let thread: LuaThread = self
            .lua
            .registry_value(&entry.thread_key)
            .map_err(PluginError::LuaError)?;

        with_current_plugin_name(&self.lua, &entry.plugin_name, || {
            self.with_watchdog(|| {
                let resumed: LuaMultiValue = match result {
                    Ok(result) => thread.resume((true, task_result_to_lua(&self.lua, result)?)),
                    Err(error) => {
                        thread.resume((false, api::facade::contract_error_payload(&error)))
                    }
                }
                .map_err(|error| {
                    PluginError::CommandExecutionFailed(format!(
                        "task coroutine resume failed (plugin: {}): {error}",
                        entry.plugin_name
                    ))
                })?;
                self.handle_coroutine_yield(&thread, &entry.plugin_name, resumed)
            })
        })
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

    #[cfg(test)]
    pub fn load_plugin_with_editor(
        &mut self,
        editor: &mut Editor,
        plugin: crate::types::Plugin,
    ) -> Result<()> {
        self.validate_plugin_capabilities(&plugin)?;
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
        let root = canonical_root(&plugin.path)?;
        self.plugin_roots
            .write()
            .insert(plugin.metadata.name.clone(), root);
        with_editor_context(editor, || {
            with_current_plugin_name(&self.lua, &plugin.metadata.name, || {
                self.with_watchdog(|| {
                    self.lua
                        .load(&code)
                        .set_name(&plugin.metadata.name)
                        .exec()
                        .map_err(PluginError::LuaError)
                })
            })
        })?;

        self.plugins.insert(plugin.metadata.name.clone(), plugin);

        Ok(())
    }

    /// Load a plugin into this host process.
    pub fn load_plugin<H>(&mut self, host: &mut H, plugin: crate::types::Plugin) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        self.validate_plugin_capabilities(&plugin)?;
        let entry_file = plugin
            .path
            .join(plugin.metadata.entry.as_deref().unwrap_or("init.lua"));

        if !entry_file.exists() {
            return Err(PluginError::InvalidPluginStructure(format!(
                "Entry file not found: {:?}",
                entry_file
            )));
        }

        let code = std::fs::read_to_string(&entry_file)?;
        self.ensure_plugin_id(&plugin.metadata.name);
        let root = canonical_root(&plugin.path)?;
        self.plugin_roots
            .write()
            .insert(plugin.metadata.name.clone(), root);
        self.with_facade_host(host, || {
            with_current_plugin_name(&self.lua, &plugin.metadata.name, || {
                self.with_watchdog(|| {
                    self.lua
                        .load(&code)
                        .set_name(&plugin.metadata.name)
                        .exec()
                        .map_err(PluginError::LuaError)
                })
            })
        })?;

        self.plugins.insert(plugin.metadata.name.clone(), plugin);

        Ok(())
    }

    /// Dispatch a contract event to all subscribed plugin handlers.
    #[cfg(test)]
    pub fn call_event_handlers_with_editor(
        &self,
        editor: &mut Editor,
        event: &crate::contract::events::PluginEvent,
    ) -> Result<()> {
        let kind = event.kind();

        let targets: Vec<_> = self
            .contract_event_handlers
            .read()
            .get(&kind)
            .map(|callbacks| {
                callbacks
                    .iter()
                    .map(|entry| (entry.handle, entry.plugin_name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        if targets.is_empty() {
            return Ok(());
        }

        let event_data = with_editor_context_ref(editor, || {
            api::facade::contract_event_to_table(&self.lua, event).map_err(PluginError::LuaError)
        })?;

        for (handle, plugin_name) in targets {
            let callback = {
                let handlers = self.contract_event_handlers.read();
                let Some(entry) = handlers
                    .get(&kind)
                    .and_then(|entries| entries.iter().find(|entry| entry.handle == handle))
                else {
                    continue;
                };
                self.lua
                    .registry_value::<LuaFunction>(&entry.callback_ref)
                    .map_err(|e| PluginError::EventHandlerError {
                        plugin: entry.plugin_name.clone(),
                        error: format!("Failed to retrieve callback: {e}"),
                    })
            };

            let result = match callback {
                Ok(callback) => with_editor_context(editor, || {
                    with_current_plugin_name(&self.lua, &plugin_name, || {
                        self.with_watchdog(|| {
                            callback
                                .call::<()>(event_data.clone())
                                .map_err(PluginError::LuaError)
                        })
                    })
                }),
                Err(err) => Err(err),
            };

            match result {
                Ok(()) => self.reset_event_failure(kind, handle),
                Err(err) => {
                    error!(
                        "Plugin event handler failed: plugin='{}' event='{}' handle='{}': {}",
                        plugin_name, kind, handle, err
                    );
                    self.record_event_failure(kind, handle, &plugin_name);
                }
            }
        }

        Ok(())
    }

    /// Dispatch a contract event through the facade host.
    pub fn call_event_handlers<H>(
        &self,
        host: &mut H,
        event: &crate::contract::events::PluginEvent,
    ) -> Result<()>
    where
        H: crate::contract::host::PluginFacadeQueryHost
            + crate::contract::host::PluginFacadeMutationHost
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let kind = event.kind();

        let targets: Vec<_> = self
            .contract_event_handlers
            .read()
            .get(&kind)
            .map(|callbacks| {
                callbacks
                    .iter()
                    .map(|entry| (entry.handle, entry.plugin_name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        if targets.is_empty() {
            return Ok(());
        }

        let event_data = api::facade::contract_event_to_table(&self.lua, event)
            .map_err(PluginError::LuaError)?;

        for (handle, plugin_name) in targets {
            let callback = {
                let handlers = self.contract_event_handlers.read();
                let Some(entry) = handlers
                    .get(&kind)
                    .and_then(|entries| entries.iter().find(|entry| entry.handle == handle))
                else {
                    continue;
                };
                self.lua
                    .registry_value::<LuaFunction>(&entry.callback_ref)
                    .map_err(|e| PluginError::EventHandlerError {
                        plugin: entry.plugin_name.clone(),
                        error: format!("Failed to retrieve callback: {e}"),
                    })
            };

            let result = match callback {
                Ok(callback) => self.with_facade_host(host, || {
                    with_current_plugin_name(&self.lua, &plugin_name, || {
                        self.with_watchdog(|| {
                            callback
                                .call::<()>(event_data.clone())
                                .map_err(PluginError::LuaError)
                        })
                    })
                }),
                Err(err) => Err(err),
            };

            match result {
                Ok(()) => self.reset_event_failure(kind, handle),
                Err(err) => {
                    error!(
                        "Plugin event handler failed: plugin='{}' event='{}' handle='{}': {}",
                        plugin_name, kind, handle, err
                    );
                    self.record_event_failure(kind, handle, &plugin_name);
                }
            }
        }

        Ok(())
    }

    fn reset_event_failure(
        &self,
        kind: crate::contract::events::EventKind,
        handle: SubscriptionHandle,
    ) {
        if let Some(entry) = self
            .contract_event_handlers
            .write()
            .get_mut(&kind)
            .and_then(|entries| entries.iter_mut().find(|entry| entry.handle == handle))
        {
            entry.failures = 0;
        }
    }

    fn record_event_failure(
        &self,
        kind: crate::contract::events::EventKind,
        handle: SubscriptionHandle,
        plugin_name: &str,
    ) {
        let removed = {
            let mut handlers = self.contract_event_handlers.write();
            let Some(entries) = handlers.get_mut(&kind) else {
                return;
            };
            let Some(entry) = entries.iter_mut().find(|entry| entry.handle == handle) else {
                return;
            };
            entry.failures += 1;
            if entry.failures < EVENT_FAILURE_LIMIT {
                return;
            }

            warn!(
                "Unsubscribing plugin event handler after {} consecutive failures: plugin='{}' event='{}' handle='{}'",
                EVENT_FAILURE_LIMIT,
                plugin_name,
                kind,
                handle
            );

            let Some(index) = entries.iter().position(|entry| entry.handle == handle) else {
                return;
            };
            let removed = entries.remove(index);
            if entries.is_empty() {
                handlers.remove(&kind);
            }
            removed
        };

        if let Some(host) = &self.event_host {
            match self.plugin_id_for_name(plugin_name).and_then(|plugin| {
                host.lock().unsubscribe(plugin, handle).map_err(|err| {
                    PluginError::EventHandlerError {
                        plugin: plugin_name.to_string(),
                        error: err.to_string(),
                    }
                })
            }) {
                Ok(()) | Err(PluginError::EventHandlerError { .. }) => {}
                Err(err) => warn!("Failed to notify host about event unsubscribe: {err}"),
            }
        }

        if let Err(err) = self.lua.remove_registry_value(removed.callback_ref) {
            warn!("Failed to remove event handler registry value: {err}");
        }
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

    pub fn has_panel_event_callback(&self, panel: PanelHandle) -> bool {
        self.panel_callbacks
            .read()
            .get(&panel)
            .is_some_and(|callbacks| callbacks.event_callback_id.is_some())
    }

    /// Run a retained panel key callback without borrowing frontend state.
    ///
    /// Panel callbacks must use the configured contract hosts for editor
    /// queries and mutations. Legacy direct-editor access is intentionally not
    /// available here so the callback can execute outside the UI foreground.
    pub fn handle_panel_key(&self, panel: PanelHandle, key_text: &str) -> Result<bool> {
        let (plugin_name, callback_id) = {
            let callbacks = self.panel_callbacks.read();
            let Some(callbacks) = callbacks.get(&panel) else {
                return Ok(false);
            };
            let Some(callback_id) = callbacks.event_callback_id else {
                return Ok(false);
            };
            (callbacks.plugin_name.clone(), callback_id)
        };
        let callback_key = PluginCallbackKey::new(plugin_name.clone(), callback_id);
        let callback = {
            let callbacks = self.ui_callbacks.read();
            let Some(callback_ref) = callbacks.get(&callback_key) else {
                return Ok(false);
            };
            self.lua
                .registry_value::<LuaFunction>(callback_ref)
                .map_err(PluginError::LuaError)?
        };
        let event = self.lua.create_table().map_err(PluginError::LuaError)?;
        event.set("key", key_text).map_err(PluginError::LuaError)?;
        with_current_plugin_name(&self.lua, &plugin_name, || {
            self.with_watchdog(|| {
                callback
                    .call::<Option<bool>>(event)
                    .map(|consumed| consumed.unwrap_or(false))
                    .map_err(PluginError::LuaError)
            })
        })
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

    #[test]
    fn plugin_requirements_are_checked_against_active_host_metadata() {
        let mut engine = LuaEngine::new().unwrap();
        let plugin = |capability: &str| crate::types::Plugin {
            metadata: crate::types::PluginMetadata {
                name: format!("requires-{capability}"),
                capabilities: vec![capability.to_owned()],
                ..Default::default()
            },
            path: PathBuf::new(),
            enabled: true,
        };

        assert!(engine
            .validate_plugin_capabilities(&plugin("query"))
            .is_ok());
        assert!(engine.validate_plugin_capabilities(&plugin("ui")).is_err());

        engine.set_api_metadata(crate::contract::metadata::ApiMetadata::from_capabilities([
            crate::contract::metadata::Capability::Query,
            crate::contract::metadata::Capability::Ui,
        ]));
        assert!(engine.validate_plugin_capabilities(&plugin("ui")).is_ok());
        assert!(engine
            .validate_plugin_capabilities(&plugin("mutation"))
            .is_err());
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

    #[derive(Clone)]
    struct TestTaskHost {
        operation: PluginOperationToken,
        opened: Arc<Mutex<Vec<(PluginId, crate::contract::requests::OpenDocumentRequest)>>>,
        canceled: Arc<Mutex<Vec<(PluginId, PluginOperationToken)>>>,
    }

    impl crate::contract::host::PluginTaskHost for TestTaskHost {
        fn start(
            &mut self,
            plugin: PluginId,
            request: crate::contract::PluginTaskRequest,
        ) -> crate::contract::ContractResult<PluginOperationToken> {
            let crate::contract::PluginTaskRequest::OpenDocument(request) = request else {
                return Err(crate::contract::ContractError::unsupported("test task"));
            };
            self.opened.lock().push((plugin, request));
            Ok(self.operation)
        }

        fn cancel(&mut self, plugin: PluginId, operation: PluginOperationToken) {
            self.canceled.lock().push((plugin, operation));
        }
    }

    struct TestUiHost {
        next: u64,
    }

    impl TestUiHost {
        fn next_token(&mut self) -> crate::contract::UiCallbackToken {
            let token =
                crate::contract::UiCallbackToken::from_raw(NonZeroU64::new(self.next).unwrap());
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
        ) -> crate::contract::ContractResult<crate::contract::UiCallbackToken> {
            Ok(self.next_token())
        }

        fn confirm(
            &mut self,
            _plugin: crate::contract::PluginId,
            _req: crate::contract::requests::ConfirmRequest,
        ) -> crate::contract::ContractResult<crate::contract::UiCallbackToken> {
            Ok(self.next_token())
        }

        fn picker(
            &mut self,
            _plugin: crate::contract::PluginId,
            _req: crate::contract::requests::PickerRequest,
        ) -> crate::contract::ContractResult<crate::contract::UiCallbackToken> {
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
                ..Default::default()
            },
            path: dir.path().to_path_buf(),
            enabled: true,
        };

        engine.load_plugin_with_editor(&mut editor, plugin).unwrap();

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
    fn test_panel_registration_failure_clears_event_callbacks() {
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
                    content = "Rejected",
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
                content = {
                    { x = 1, y = 2, text = "Owned", style = "ui.text.focus" },
                },
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
    fn test_plugin_float_rejects_render_callbacks() {
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

        let result = with_editor_context(&mut editor, || {
            engine
                .lua
                .load(
                    r#"
                    helix.floats.create({
                        placement = { type = "centered", width = 20, height = 5 },
                        render = function() end,
                    })
                    "#,
                )
                .exec()
        });

        let err = result.expect_err("render callbacks must be rejected");
        assert!(err.to_string().contains("retained `content`"));
        assert!(engine.ui_callbacks.read().is_empty());
        assert!(editor.model.floats.is_empty());
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
    fn test_plugin_float_create_without_editor_context_leaves_no_state() {
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

        exec_as(
            &engine,
            "caller-plugin",
            r#"
            local ok, err = pcall(function()
                helix.events.unsubscribe(subscription)
            end)
            assert(not ok)
            assert(type(err) == "table", type(err) .. ":" .. tostring(err))
            unsubscribe_err_code = err.code
            "#,
        )
        .unwrap();

        assert_eq!(
            engine
                .lua
                .globals()
                .get::<String>("unsubscribe_err_code")
                .unwrap(),
            "permission_denied"
        );
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
            fn command_catalog(&self) -> Vec<crate::contract::CommandDescriptor> {
                Vec::new()
            }

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
            fn command_catalog(&self) -> Vec<crate::contract::CommandDescriptor> {
                Vec::new()
            }

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

        exec_as(
            &engine,
            "caller-plugin",
            r#"
            local ok, err = pcall(function()
                command:update({ doc = 'stolen' })
            end)
            assert(not ok)
            if type(err) == "table" then
                update_err_code = err.code
            else
                update_err_code = tostring(err):match("code=([%w_]+)")
            end
            "#,
        )
        .unwrap();
        assert_eq!(
            engine
                .lua
                .globals()
                .get::<String>("update_err_code")
                .unwrap(),
            "permission_denied"
        );

        exec_as(
            &engine,
            "caller-plugin",
            r#"
            local ok, err = pcall(function()
                helix.commands.remove(command)
            end)
            assert(not ok)
            if type(err) == "table" then
                remove_err_code = err.code
            else
                remove_err_code = tostring(err):match("code=([%w_]+)")
            end
            "#,
        )
        .unwrap();
        assert_eq!(
            engine
                .lua
                .globals()
                .get::<String>("remove_err_code")
                .unwrap(),
            "permission_denied"
        );

        assert!(updated.lock().is_empty());
        assert!(removed.lock().is_empty());
        assert_eq!(engine.get_commands().len(), 1);
    }

    #[test]
    fn test_command_discovery_and_local_execution() {
        #[derive(Clone)]
        struct TestCommandHost {
            registered: Arc<Mutex<Vec<crate::contract::requests::CommandDefinition>>>,
        }

        impl crate::contract::host::PluginCommandHost for TestCommandHost {
            fn command_catalog(&self) -> Vec<crate::contract::CommandDescriptor> {
                vec![crate::contract::CommandDescriptor {
                    name: "write".into(),
                    aliases: vec!["w".into()],
                    doc: "Write the current document".into(),
                    arguments: Vec::new(),
                    signature: Some(crate::contract::CommandSignatureDescriptor {
                        min_positionals: 0,
                        max_positionals: Some(1),
                        raw_after: None,
                        flags: vec![crate::contract::CommandFlagDescriptor {
                            name: "force".into(),
                            alias: Some('f'),
                            doc: "Force the write".into(),
                            takes_value: false,
                            values: Vec::new(),
                        }],
                    }),
                    kind: crate::contract::CommandKind::Typable,
                    scope: crate::contract::CommandScope::Frontend,
                }]
            }

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
        }

        let registered = Arc::new(Mutex::new(Vec::new()));
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
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "caller-plugin",
            r#"
            local commands = helix.commands.list()
            assert(#commands == 1)
            assert(commands[1].name == "write")
            assert(commands[1].kind == "typable")
            assert(commands[1].scope == "frontend")
            assert(commands[1].signature.max_positionals == 1)
            assert(commands[1].signature.flags[1].alias == "f")
            assert(helix.commands.get("w").name == "write")
            assert(helix.commands.get("missing") == nil)
            "#,
        )
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
            .handle_ui_callback_with_editor(
                &mut editor,
                callback_id,
                crate::contract::value::DynamicValue::Nil,
            )
            .unwrap();

        assert!(engine.ui_callbacks.read().contains_key(&callback_key));
    }

    #[test]
    fn stale_document_handle_returns_contract_error_table() {
        let engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();
        let stale = crate::contract::ViewHandle::from_raw(NonZeroU64::new(999).unwrap());

        let (code, entity): (String, String) = with_editor_context(&mut editor, || {
            engine.lua.globals().set(
                "view",
                engine
                    .lua
                    .create_userdata(api::facade::LuaViewHandle(stale))?,
            )?;
            engine
                .lua
                .load(
                    r#"
                    local ok, err = pcall(function()
                        return helix.tabs.list(view)
                    end)
                    assert(not ok)
                    assert(type(err) == "table", type(err) .. ":" .. tostring(err))
                    return err.code, err.entity
                    "#,
                )
                .eval()
                .map_err(PluginError::LuaError)
        })
        .unwrap();

        assert_eq!(code, "stale_handle");
        assert!(entity.contains("ViewHandle"));
    }

    #[test]
    fn event_handler_failure_isolated_and_unsubscribed_after_threshold() {
        #[derive(Clone)]
        struct TestEventHost {
            next: Arc<Mutex<u64>>,
            unsubscribed: Arc<Mutex<Vec<crate::contract::SubscriptionHandle>>>,
        }

        impl crate::contract::host::PluginEventHost for TestEventHost {
            fn subscribe(
                &mut self,
                _plugin: crate::contract::PluginId,
                _kind: crate::contract::events::EventKind,
            ) -> crate::contract::ContractResult<crate::contract::SubscriptionHandle> {
                let mut next = self.next.lock();
                let handle =
                    crate::contract::SubscriptionHandle::from_raw(NonZeroU64::new(*next).unwrap());
                *next += 1;
                Ok(handle)
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

        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "test-plugin", 1);
        set_current_plugin(&engine, "test-plugin");
        let unsubscribed = Arc::new(Mutex::new(Vec::new()));
        engine.set_event_host(Box::new(TestEventHost {
            next: Arc::new(Mutex::new(1)),
            unsubscribed: Arc::clone(&unsubscribed),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "test-plugin",
            r#"
            helix.events.subscribe("host_ready", function(event)
                error("boom")
            end)
            helix.events.subscribe("host_ready", function(event)
                _G.second_handler_runs = (_G.second_handler_runs or 0) + 1
            end)
            "#,
        )
        .unwrap();

        let mut editor = test_editor();
        let event = crate::contract::events::PluginEvent::HostReady(
            crate::contract::events::HostReadyEvent {
                api_version: crate::contract::metadata::API_VERSION,
            },
        );
        for _ in 0..EVENT_FAILURE_LIMIT {
            engine
                .call_event_handlers_with_editor(&mut editor, &event)
                .unwrap();
        }

        let second_runs: u32 = engine.lua.globals().get("second_handler_runs").unwrap();
        assert_eq!(second_runs, EVENT_FAILURE_LIMIT);
        assert_eq!(unsubscribed.lock().len(), 1);
        assert_eq!(
            engine
                .contract_event_handlers
                .read()
                .get(&crate::contract::events::EventKind::HostReady)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn sandbox_scopes_require_to_plugin_directory() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("scoped");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("helper.lua"), "return { value = 42 }").unwrap();
        std::fs::write(
            plugin_dir.join("init.lua"),
            r#"
            local helper = require("helper")
            _G.helper_value = helper.value
            local ok = pcall(function()
                require("io")
            end)
            _G.system_require_failed = not ok
            _G.package_removed = package == nil
            "#,
        )
        .unwrap();

        let mut engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();
        let mut editor = test_editor();
        engine
            .load_plugin_with_editor(
                &mut editor,
                crate::types::Plugin {
                    metadata: crate::types::PluginMetadata {
                        name: "scoped".into(),
                        ..Default::default()
                    },
                    path: plugin_dir,
                    enabled: true,
                },
            )
            .unwrap();

        assert_eq!(engine.lua.globals().get::<u32>("helper_value").unwrap(), 42);
        assert!(engine
            .lua
            .globals()
            .get::<bool>("system_require_failed")
            .unwrap());
        assert!(engine.lua.globals().get::<bool>("package_removed").unwrap());
    }

    #[test]
    fn instruction_watchdog_aborts_runaway_plugin() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("runaway");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("init.lua"), "while true do end").unwrap();

        let mut engine = LuaEngine::new().unwrap();
        engine
            .register_api(crate::types::PluginConfig {
                max_instructions: 20_000,
                ..Default::default()
            })
            .unwrap();
        let mut editor = test_editor();
        let result = engine.load_plugin_with_editor(
            &mut editor,
            crate::types::Plugin {
                metadata: crate::types::PluginMetadata {
                    name: "runaway".into(),
                    ..Default::default()
                },
                path: plugin_dir,
                enabled: true,
            },
        );

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("instruction watchdog exceeded"));
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
            .create_userdata(LuaUiCallbackToken::from(UiCallbackToken::from_raw(
                NonZeroU64::new(999).unwrap(),
            )))
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
            fn command_catalog(&self) -> Vec<crate::contract::CommandDescriptor> {
                Vec::new()
            }

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
    fn keymap_lifecycle_is_owned_and_reset_removes_contribution() {
        #[derive(Clone)]
        struct TestKeymapHost {
            registered: Arc<Mutex<Vec<crate::contract::KeymapDefinition>>>,
            updated: Arc<Mutex<Vec<crate::contract::KeymapUpdateRequest>>>,
            removed: Arc<Mutex<Vec<crate::contract::KeymapRemoveRequest>>>,
        }

        impl crate::contract::host::PluginKeymapHost for TestKeymapHost {
            fn register_keymap(
                &mut self,
                _plugin: PluginId,
                definition: crate::contract::KeymapDefinition,
            ) -> crate::contract::ContractResult<crate::contract::KeymapHandle> {
                self.registered.lock().push(definition);
                Ok(crate::contract::KeymapHandle::from_raw(
                    NonZeroU64::new(1).unwrap(),
                ))
            }

            fn update_keymap(
                &mut self,
                _plugin: PluginId,
                request: crate::contract::KeymapUpdateRequest,
            ) -> crate::contract::ContractResult<()> {
                self.updated.lock().push(request);
                Ok(())
            }

            fn remove_keymap(
                &mut self,
                _plugin: PluginId,
                request: crate::contract::KeymapRemoveRequest,
            ) -> crate::contract::ContractResult<()> {
                self.removed.lock().push(request);
                Ok(())
            }
        }

        let registered = Arc::new(Mutex::new(Vec::new()));
        let updated = Arc::new(Mutex::new(Vec::new()));
        let removed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = LuaEngine::new().unwrap();
        register_loaded_plugin(&engine, "test-plugin", 1);
        set_current_plugin(&engine, "test-plugin");
        engine.set_keymap_host(Box::new(TestKeymapHost {
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
                keymap = helix.keymaps.register({
                    mode = "normal",
                    scope = { language = "rust" },
                    bindings = {
                        { keys = { "space", "t" }, command = ":write" },
                    },
                })
                keymap:update({
                    mode = "normal",
                    bindings = {
                        { keys = { "F24" }, commands = { ":write", ":reload" } },
                    },
                })
                "#,
            )
            .exec()
            .unwrap();

        assert_eq!(registered.lock().len(), 1);
        assert_eq!(updated.lock().len(), 1);
        assert_eq!(registered.lock()[0].scope.language.as_deref(), Some("rust"));
        assert_eq!(updated.lock()[0].definition.bindings[0].commands.len(), 2);

        engine.reset().unwrap();
        assert_eq!(removed.lock().len(), 1);
    }

    #[test]
    fn reset_cancels_pending_plugin_operations() {
        let mut engine = LuaEngine::new().unwrap();
        let plugin = PluginId::from_raw(NonZeroU64::new(1).unwrap());
        let pending_operation = PluginOperationToken::from_raw(NonZeroU64::new(7).unwrap());
        let suspended_operation = PluginOperationToken::from_raw(NonZeroU64::new(8).unwrap());
        engine
            .plugin_registry
            .write()
            .insert("test-plugin".into(), plugin);
        engine
            .pending_operations
            .write()
            .insert(pending_operation, "test-plugin".into());
        let thread = engine
            .lua
            .create_thread(engine.lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        let thread_key = engine.lua.create_registry_value(thread).unwrap();
        engine.suspended_coroutines.write().insert(
            AwaitKey::Operation(suspended_operation),
            SuspendedCoroutine {
                thread_key,
                plugin_name: "test-plugin".into(),
            },
        );
        let canceled = Arc::new(Mutex::new(Vec::new()));
        engine.set_task_host(Box::new(TestTaskHost {
            operation: pending_operation,
            opened: Arc::new(Mutex::new(Vec::new())),
            canceled: Arc::clone(&canceled),
        }));

        engine.reset().unwrap();

        let mut canceled = canceled.lock().clone();
        canceled.sort_by_key(|(_, operation)| operation.raw());
        assert_eq!(
            canceled,
            vec![(plugin, pending_operation), (plugin, suspended_operation),]
        );
        assert!(engine.pending_operations.read().is_empty());
        assert!(engine.suspended_coroutines.read().is_empty());
    }

    #[test]
    fn reset_releases_resources_for_every_loaded_plugin() {
        struct TestResourceHost {
            released: Arc<Mutex<Vec<PluginId>>>,
        }

        impl crate::contract::host::PluginResourceHost for TestResourceHost {
            fn release_plugin_resources(
                &mut self,
                plugin: PluginId,
            ) -> crate::contract::ContractResult<()> {
                self.released.lock().push(plugin);
                Ok(())
            }
        }

        let mut engine = LuaEngine::new().unwrap();
        let first = PluginId::from_raw(NonZeroU64::new(1).unwrap());
        let second = PluginId::from_raw(NonZeroU64::new(2).unwrap());
        engine.plugin_registry.write().insert("first".into(), first);
        engine
            .plugin_registry
            .write()
            .insert("second".into(), second);
        let released = Arc::new(Mutex::new(Vec::new()));
        engine.set_resource_host(Box::new(TestResourceHost {
            released: Arc::clone(&released),
        }));

        engine.reset().unwrap();

        let mut released = released.lock().clone();
        released.sort_by_key(|plugin| plugin.raw());
        assert_eq!(released, vec![first, second]);
    }

    #[test]
    fn async_document_open_resumes_with_real_document_handle() {
        let mut engine = LuaEngine::new().unwrap();
        let plugin = PluginId::from_raw(NonZeroU64::new(1).unwrap());
        let operation = PluginOperationToken::from_raw(NonZeroU64::new(7).unwrap());
        register_loaded_plugin(&engine, "test-plugin", 1);
        let opened = Arc::new(Mutex::new(Vec::new()));
        engine.set_task_host(Box::new(TestTaskHost {
            operation,
            opened: Arc::clone(&opened),
            canceled: Arc::new(Mutex::new(Vec::new())),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        exec_as(
            &engine,
            "test-plugin",
            r#"
            helix.async(function()
                local document = helix.documents.open("target.txt", { focus = true })
                _G.opened_document_id = document:id()
            end)
            "#,
        )
        .unwrap();

        assert_eq!(opened.lock().len(), 1);
        assert_eq!(opened.lock()[0].0, plugin);
        assert_eq!(opened.lock()[0].1.path, "target.txt");
        assert!(opened.lock()[0].1.focus);
        assert!(engine.pending_operations.read().is_empty());
        assert!(engine
            .suspended_coroutines
            .read()
            .contains_key(&AwaitKey::Operation(operation)));

        let mut editor = test_editor();
        editor.new_file(Action::VerticalSplit);
        let handle = helix_plugin_editor::adapt::document_handle(editor.focused_document_id());
        engine
            .handle_task_completion_with_editor(
                &mut editor,
                operation,
                Ok(crate::contract::PluginTaskResult::Document(handle)),
            )
            .unwrap();

        let opened_document_id: u64 = engine.lua.globals().get("opened_document_id").unwrap();
        assert_eq!(opened_document_id, handle.raw().get());
        assert!(engine.suspended_coroutines.read().is_empty());
    }

    #[test]
    fn synchronous_document_open_is_rejected_before_host_work_is_issued() {
        let mut engine = LuaEngine::new().unwrap();
        let operation = PluginOperationToken::from_raw(NonZeroU64::new(7).unwrap());
        register_loaded_plugin(&engine, "test-plugin", 1);
        let opened = Arc::new(Mutex::new(Vec::new()));
        engine.set_task_host(Box::new(TestTaskHost {
            operation,
            opened: Arc::clone(&opened),
            canceled: Arc::new(Mutex::new(Vec::new())),
        }));
        engine
            .register_api(crate::types::PluginConfig::default())
            .unwrap();

        let error = exec_as(
            &engine,
            "test-plugin",
            r#"helix.documents.open("target.txt", { focus = true })"#,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("must be called from a coroutine"));
        assert!(opened.lock().is_empty());
        assert!(engine.pending_operations.read().is_empty());
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
