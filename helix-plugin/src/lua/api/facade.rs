//! Contract-based Lua API facade.
//!
//! This is the sole Lua API surface for Helix plugins. All modules:
//!
//! - `helix.workspace`  — workspace queries (focused handles, mode, config, theme)
//! - `helix.documents`  — document listing and opening
//! - `helix.views`      — view listing
//! - `helix.host`       — API metadata and capability discovery
//! - `helix.events`     — typed event subscription (`subscribe` / `unsubscribe`)
//! - `helix.commands`   — register and execute commands
//! - `helix.registers`  — read/write editor registers
//! - `helix.ui`         — notifications, prompts, panels (toggle/focus/resize/list), theming
//! - `helix.splits`     — split/view topology (split, navigate, swap, resize, transpose, tree)
//! - `helix.tabs`       — per-view tab groups (open, close, focus, cycle, list)
//! - `helix.floats`     — floating windows (create, update, close, list)
//! - `helix.lsp`        — LSP client queries
//! - `helix.layout`     — layout combinators
//! - `helix.log`        — logging
//! - `helix.async(fn)`  — launch a coroutine from synchronous context
//! - `helix.config()`   — per-plugin config
//!
//! Handle-centric mutations live on the userdata methods:
//! - `doc:edit()`, `doc:save()`, `doc:set_selections()`, `doc:undo()`, `doc:redo()`, `doc:select_all()`
//! - `view:focus()`, `view:close()`, `view:cursor()`
//! - `panel:close()`, `panel:toggle()`, `panel:focus()`, `panel:resize()`

use mlua::prelude::*;

use crate::contract::handles::{
    CommandHandle, DocumentHandle, FloatHandle, PanelHandle, SubscriptionHandle, ThreadHandle,
    ViewHandle,
};
use crate::contract::host::{PluginFacadeMutationHost, PluginFacadeQueryHost};
use crate::contract::requests;
use crate::contract::snapshots;
use crate::contract::UiCallbackToken;
use crate::error::Result;
#[cfg(test)]
use helix_plugin_editor::bridge::{EditorMutationBridge, EditorQueryBridge};

mod documents;
mod host;
mod layout;
mod logging;
mod lsp;
mod syntax;
mod views;
mod workspace;
pub use documents::register as register_documents_module;
pub use host::register as register_host_module;
pub use layout::register as register_layout_module;
pub use logging::register as register_log_module;
pub use lsp::register as register_lsp_module;
pub use syntax::register as register_syntax_module;
pub use views::register as register_views_module;
pub use workspace::register as register_workspace_module;

fn with_query_bridge<T>(
    lua: &Lua,
    f: impl FnOnce(&dyn PluginFacadeQueryHost) -> LuaResult<T>,
) -> LuaResult<T> {
    if let Some(host) = lua
        .app_data_ref::<crate::lua::FacadeHostWrapper>()
        .map(|host| std::sync::Arc::clone(&host.0))
    {
        let host = host.lock();
        return f(host.query());
    }
    #[cfg(test)]
    {
        return crate::lua::with_current_editor(|editor| f(&EditorQueryBridge::new(editor)))?;
    }
    #[cfg(not(test))]
    Err(LuaError::RuntimeError(
        "plugin facade host unavailable".into(),
    ))
}

fn with_mutation_bridge<T>(
    lua: &Lua,
    f: impl FnOnce(&mut dyn PluginFacadeMutationHost) -> LuaResult<T>,
) -> LuaResult<T> {
    if let Some(host) = lua
        .app_data_ref::<crate::lua::FacadeHostWrapper>()
        .map(|host| std::sync::Arc::clone(&host.0))
    {
        let mut host = host.lock();
        return f(host.mutation());
    }
    #[cfg(test)]
    {
        return crate::lua::with_current_editor_mut(|editor| {
            let mut bridge = EditorMutationBridge::new(editor);
            f(&mut bridge)
        })?;
    }
    #[cfg(not(test))]
    Err(LuaError::RuntimeError(
        "plugin facade host unavailable".into(),
    ))
}

fn contract_error(err: crate::contract::ContractError) -> LuaError {
    LuaError::RuntimeError(contract_error_payload(&err))
}

pub(super) fn start_task(
    lua: &Lua,
    request: crate::contract::PluginTaskRequest,
) -> LuaResult<LuaValue> {
    let host = lua
        .app_data_ref::<crate::lua::TaskHostWrapper>()
        .map(|host| std::sync::Arc::clone(&host.0))
        .ok_or_else(|| LuaError::RuntimeError("plugin task host unavailable".into()))?;
    let plugin_name = current_plugin_name(lua)?;
    let operation = host
        .lock()
        .start(current_plugin_id(lua)?, request)
        .map_err(contract_error)?;
    lua.app_data_ref::<crate::lua::PendingOperationRegistry>()
        .ok_or_else(|| LuaError::RuntimeError("pending operation registry unavailable".into()))?
        .0
        .write()
        .insert(operation, plugin_name);
    lua.create_userdata(crate::lua::LuaPluginOperationToken::from(operation))?
        .into_lua(lua)
}

pub(super) fn dynamic_value_from_lua(value: LuaValue) -> LuaResult<crate::contract::DynamicValue> {
    fn convert(value: LuaValue, depth: usize) -> LuaResult<crate::contract::DynamicValue> {
        use crate::contract::DynamicValue;

        if depth >= 64 {
            return Err(LuaError::RuntimeError(
                "dynamic value exceeds the maximum nesting depth of 64".into(),
            ));
        }
        Ok(match value {
            LuaValue::Nil => DynamicValue::Nil,
            LuaValue::Boolean(value) => DynamicValue::Bool(value),
            LuaValue::Integer(value) => DynamicValue::Int(value),
            LuaValue::Number(value) if value.is_finite() => DynamicValue::Float(value),
            LuaValue::Number(_) => {
                return Err(LuaError::RuntimeError(
                    "dynamic values cannot contain non-finite numbers".into(),
                ))
            }
            LuaValue::String(value) => DynamicValue::String(value.to_str()?.to_owned()),
            LuaValue::Table(table) => {
                let mut entries = Vec::new();
                for pair in table.pairs::<LuaValue, LuaValue>() {
                    entries.push(pair?);
                }

                let is_array = entries
                    .iter()
                    .all(|(key, _)| matches!(key, LuaValue::Integer(index) if *index > 0));
                if is_array {
                    entries.sort_unstable_by_key(|(key, _)| match key {
                        LuaValue::Integer(index) => *index,
                        _ => unreachable!(),
                    });
                    for (offset, (key, _)) in entries.iter().enumerate() {
                        let LuaValue::Integer(index) = key else {
                            unreachable!();
                        };
                        if *index != offset as i64 + 1 {
                            return Err(LuaError::RuntimeError(
                                "dynamic value arrays must use contiguous 1-based indexes".into(),
                            ));
                        }
                    }
                    DynamicValue::Array(
                        entries
                            .into_iter()
                            .map(|(_, value)| convert(value, depth + 1))
                            .collect::<LuaResult<_>>()?,
                    )
                } else {
                    let mut values = std::collections::BTreeMap::new();
                    for (key, value) in entries {
                        let LuaValue::String(key) = key else {
                            return Err(LuaError::RuntimeError(
                                "dynamic value objects must use string keys".into(),
                            ));
                        };
                        values.insert(key.to_str()?.to_owned(), convert(value, depth + 1)?);
                    }
                    DynamicValue::Object(values)
                }
            }
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: other.type_name(),
                    to: "DynamicValue".into(),
                    message: Some("expected nil, boolean, number, string, array, or object".into()),
                })
            }
        })
    }

    convert(value, 0)
}

pub(crate) fn contract_error_payload(err: &crate::contract::ContractError) -> String {
    let entity = err.entity().unwrap_or("");
    format!(
        "__helix_contract_error__\ncode={}\nmessage={}\nentity={}",
        err.code(),
        err,
        entity
    )
}

// ---------------------------------------------------------------------------
// Handle userdata — opaque handles exposed to Lua as light userdata
// ---------------------------------------------------------------------------

/// Lua userdata wrapper for a `DocumentHandle`.
///
/// Query methods read through the bridge; mutations are handle methods.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LuaDocumentHandle(pub(crate) DocumentHandle);

impl FromLua for LuaDocumentHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "DocumentHandle".to_string(),
                message: Some("expected a DocumentHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaDocumentHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));

        // -- Query methods --

        methods.add_method("snapshot", |lua, this, ()| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.document_snapshot(this.0).map_err(contract_error)
            })?;
            snapshot_to_table(lua, &snap)
        });

        methods.add_method("text", |lua, this, ()| {
            with_query_bridge(lua, |bridge| {
                bridge.document_text(this.0).map_err(contract_error)
            })
        });

        methods.add_method("line", |lua, this, line: usize| {
            with_query_bridge(lua, |bridge| {
                bridge.document_line(this.0, line).map_err(contract_error)
            })
        });

        methods.add_method("diagnostics", |lua, this, ()| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.diagnostics(this.0).map_err(contract_error)
            })?;
            diagnostics_to_table(lua, &snap)
        });

        // -- Mutation methods --

        // doc:edit(edits) — apply text edits
        // Each edit: { start = {line=, column=}, finish = {line=, column=}, text = "..." }
        methods.add_method("edit", |lua, this, edits: Vec<LuaTable>| {
            let edits = edits
                .iter()
                .map(parse_text_edit)
                .collect::<LuaResult<Vec<_>>>()?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .apply_edit(requests::ApplyEditRequest {
                        document: this.0,
                        edits,
                    })
                    .map_err(contract_error)
            })
        });

        // doc:save(opts?) — save the document
        methods.add_method("save", |lua, this, opts: Option<LuaTable>| {
            let force = opts
                .and_then(|t| t.get::<Option<bool>>("force").ok().flatten())
                .unwrap_or(false);
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .save_document(requests::SaveDocumentRequest {
                        document: this.0,
                        force,
                    })
                    .map_err(contract_error)
            })
        });

        // doc:set_selections(selections, view?) — set selections on the document
        methods.add_method(
            "set_selections",
            |lua, this, (sels, view): (Vec<LuaTable>, Option<LuaViewHandle>)| {
                let selections = sels
                    .iter()
                    .map(parse_selection_range)
                    .collect::<LuaResult<Vec<_>>>()?;
                with_mutation_bridge(lua, |bridge| {
                    bridge
                        .set_selection(requests::SetSelectionRequest {
                            document: this.0,
                            view: view.map(|v| v.0),
                            selections,
                        })
                        .map_err(contract_error)
                })
            },
        );

        // doc:undo() — undo the last change, returns true if successful
        methods.add_method("undo", |lua, this, ()| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .undo(requests::UndoRequest { document: this.0 })
                    .map_err(contract_error)
            })
        });

        // doc:redo() — redo the last undone change, returns true if successful
        methods.add_method("redo", |lua, this, ()| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .redo(requests::RedoRequest { document: this.0 })
                    .map_err(contract_error)
            })
        });

        // doc:select_all() — select all text in the document
        methods.add_method("select_all", |lua, this, ()| {
            with_mutation_bridge(lua, |host| {
                host.select_all(requests::SelectAllRequest { document: this.0 })
                    .map_err(contract_error)
            })
        });

        // doc:set_annotations(annotations) — replace virtual text annotations
        // for the calling plugin on this document. Annotations are scoped by
        // plugin name so multiple plugins coexist.
        methods.add_method(
            "set_annotations",
            |lua, this, annotations: Vec<LuaTable>| {
                let parsed: Vec<requests::Annotation> = annotations
                    .iter()
                    .map(parse_annotation)
                    .collect::<LuaResult<_>>()?;
                let plugin = current_plugin_id(lua)?;
                with_mutation_bridge(lua, |host| {
                    host.set_annotations(requests::SetAnnotationsRequest {
                        document: this.0,
                        plugin,
                        annotations: parsed,
                    })
                    .map_err(contract_error)
                })
            },
        );

        // doc:clear_annotations() — remove all annotations registered by the
        // calling plugin on this document.
        methods.add_method("clear_annotations", |lua, this, ()| {
            let plugin = current_plugin_id(lua)?;
            with_mutation_bridge(lua, |host| {
                host.set_annotations(requests::SetAnnotationsRequest {
                    document: this.0,
                    plugin,
                    annotations: Vec::new(),
                })
                .map_err(contract_error)
            })
        });
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

/// Lua userdata wrapper for a `ViewHandle`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LuaViewHandle(pub(crate) ViewHandle);

impl FromLua for LuaViewHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "ViewHandle".to_string(),
                message: Some("expected a ViewHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaViewHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));

        methods.add_method("snapshot", |lua, this, ()| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.view_snapshot(this.0).map_err(contract_error)
            })?;
            view_snapshot_to_table(lua, &snap)
        });

        // view:cursor() — get cursor position as {line=, column=}
        methods.add_method("cursor", |lua, this, ()| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.view_snapshot(this.0).map_err(contract_error)
            })?;
            let t = lua.create_table()?;
            t.set("line", snap.cursor.line)?;
            t.set("column", snap.cursor.column)?;
            Ok(t)
        });

        // view:focus() — focus this view
        methods.add_method("focus", |lua, this, ()| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .focus_view(requests::FocusViewRequest { view: this.0 })
                    .map_err(contract_error)
            })
        });

        // view:close() — close this view
        methods.add_method("close", |lua, this, ()| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_view(requests::CloseViewRequest { view: this.0 })
                    .map_err(contract_error)
            })
        });
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

/// Lua userdata wrapper for a `PanelHandle`.
#[derive(Debug, Clone, Copy)]
struct LuaPanelHandle(PanelHandle);

impl FromLua for LuaPanelHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "PanelHandle".to_string(),
                message: Some("expected a PanelHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaPanelHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));

        methods.add_method("close", |lua, this, ()| {
            ensure_panel_owner(lua, this.0)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            handler
                .0
                .lock()
                .close_panel(plugin_id, requests::PanelCloseRequest { panel: this.0 })
                .map_err(contract_error)?;
            clear_panel_callbacks(lua, this.0)?;
            Ok(())
        });

        methods.add_method("toggle", |lua, this, ()| {
            ensure_panel_owner(lua, this.0)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            handler
                .0
                .lock()
                .toggle_panel(plugin_id, requests::TogglePanelRequest { panel: this.0 })
                .map_err(contract_error)?;
            Ok(())
        });

        methods.add_method("focus", |lua, this, ()| {
            ensure_panel_owner(lua, this.0)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            handler
                .0
                .lock()
                .focus_panel(plugin_id, requests::FocusPanelRequest { panel: this.0 })
                .map_err(contract_error)?;
            Ok(())
        });

        methods.add_method("resize", |lua, this, size_str: String| {
            ensure_panel_owner(lua, this.0)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            handler
                .0
                .lock()
                .resize_panel(
                    plugin_id,
                    requests::ResizePanelRequest {
                        panel: this.0,
                        size: parse_panel_size_spec(&size_str)?,
                    },
                )
                .map_err(contract_error)?;
            Ok(())
        });

        methods.add_method("update", |lua, this, options: LuaTable| {
            ensure_panel_owner(lua, this.0)?;
            let title = options.get::<Option<String>>("title")?;
            let content = options
                .contains_key("content")?
                .then(|| parse_panel_content(&options))
                .transpose()?;
            panel_host(lua)?
                .0
                .lock()
                .update_panel(
                    current_plugin_id(lua)?,
                    requests::PanelUpdateRequest {
                        panel: this.0,
                        title,
                        content,
                    },
                )
                .map_err(contract_error)
        });
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

/// Lua userdata wrapper for a `CommandHandle`.
#[derive(Debug, Clone, Copy)]
struct LuaCommandHandle(CommandHandle);

impl FromLua for LuaCommandHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "CommandHandle".to_string(),
                message: Some("expected a CommandHandle userdata".into()),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LuaKeymapHandle(crate::contract::KeymapHandle);

impl FromLua for LuaKeymapHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|handle| *handle),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "KeymapHandle".to_string(),
                message: Some("expected a KeymapHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaKeymapHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
        methods.add_method("update", |lua, this, definition: LuaTable| {
            update_lua_keymap(lua, this.0, definition)
        });
        methods.add_method("remove", |lua, this, ()| remove_lua_keymap(lua, this.0));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

impl LuaUserData for LuaCommandHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
        methods.add_method("update", |lua, this, table: LuaTable| {
            update_lua_command(lua, this.0, table)
        });
        methods.add_method("remove", |lua, this, ()| remove_lua_command(lua, this.0));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

/// Lua userdata wrapper for a `SubscriptionHandle`.
#[derive(Debug, Clone, Copy)]
struct LuaSubscriptionHandle(SubscriptionHandle);

impl FromLua for LuaSubscriptionHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "SubscriptionHandle".to_string(),
                message: Some("expected a SubscriptionHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaSubscriptionHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

/// Lua userdata wrapper for an assistant `ThreadHandle`.
#[derive(Debug, Clone, Copy)]
struct LuaThreadHandle(ThreadHandle);

impl FromLua for LuaThreadHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "ThreadHandle".to_string(),
                message: Some("expected a ThreadHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaThreadHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

// ---------------------------------------------------------------------------
// helix.events - event subscription
// ---------------------------------------------------------------------------

fn current_plugin_name(lua: &Lua) -> LuaResult<String> {
    crate::lua::current_plugin_name(lua)
}

fn current_plugin_id(lua: &Lua) -> LuaResult<crate::contract::PluginId> {
    let plugin_name = current_plugin_name(lua)?;
    let registry = lua
        .app_data_ref::<crate::lua::LoadedPluginRegistryWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("plugin registry not available".into()))?;
    let plugin_id = registry
        .0
        .read()
        .id_for_name(&plugin_name)
        .ok_or_else(|| LuaError::RuntimeError(format!("plugin not loaded: {plugin_name}")))?;
    Ok(plugin_id)
}

fn ui_host(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, crate::lua::UiHostWrapper>> {
    lua.app_data_ref::<crate::lua::UiHostWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("UI host not available".into()))
}

fn panel_host(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, crate::lua::PanelHostWrapper>> {
    lua.app_data_ref::<crate::lua::PanelHostWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("panel host not available".into()))
}

fn command_host(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, crate::lua::CommandHostWrapper>> {
    lua.app_data_ref::<crate::lua::CommandHostWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("command host not available".into()))
}

fn keymap_host(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, crate::lua::KeymapHostWrapper>> {
    lua.app_data_ref::<crate::lua::KeymapHostWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("keymap host not available".into()))
}

fn keymap_registry(
    lua: &Lua,
) -> LuaResult<mlua::AppDataRef<'_, crate::lua::KeymapRegistryWrapper>> {
    lua.app_data_ref::<crate::lua::KeymapRegistryWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("keymap registry not available".into()))
}

fn command_registry(
    lua: &Lua,
) -> LuaResult<mlua::AppDataRef<'_, crate::lua::CommandRegistryWrapper>> {
    lua.app_data_ref::<crate::lua::CommandRegistryWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("command registry not available".into()))
}

fn stale_handle_error(handle: impl std::fmt::Display) -> LuaError {
    contract_error(crate::contract::ContractError::stale_handle(
        handle.to_string(),
    ))
}

fn permission_denied_error(reason: impl Into<String>) -> LuaError {
    contract_error(crate::contract::ContractError::permission_denied(reason))
}

fn event_host(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, crate::lua::EventHostWrapper>> {
    lua.app_data_ref::<crate::lua::EventHostWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("event host not available".into()))
}

fn remove_ui_callback(
    lua: &Lua,
    plugin_name: String,
    callback_id: crate::types::UiCallbackId,
) -> LuaResult<()> {
    let registry_key = {
        let Some(ui_callbacks) = lua.app_data_ref::<crate::types::UiCallbackRegistry>() else {
            return Ok(());
        };
        let key = crate::types::PluginCallbackKey::new(plugin_name, callback_id);
        let mut callbacks = ui_callbacks.0.write();
        callbacks.remove(&key)
    };

    if let Some(registry_key) = registry_key {
        lua.remove_registry_value(registry_key)?;
    }
    Ok(())
}

fn ensure_panel_owner(lua: &Lua, panel: PanelHandle) -> LuaResult<()> {
    let plugin_name = current_plugin_name(lua)?;
    let owner = {
        let registry = lua
            .app_data_ref::<crate::lua::PanelCallbackRegistry>()
            .ok_or_else(|| LuaError::RuntimeError("panel callback registry unavailable".into()))?;
        let callbacks = registry.0.read();
        callbacks
            .get(&panel)
            .map(|callbacks| callbacks.plugin_name.clone())
    };

    match owner {
        Some(owner) if owner == plugin_name => Ok(()),
        Some(_) => Err(permission_denied_error(format!(
            "plugin '{plugin_name}' does not own {panel}"
        ))),
        None => Err(stale_handle_error(panel)),
    }
}

fn current_plugin_panel_handles(lua: &Lua) -> LuaResult<std::collections::HashSet<PanelHandle>> {
    let plugin_name = current_plugin_name(lua)?;
    let handles = {
        let registry = lua
            .app_data_ref::<crate::lua::PanelCallbackRegistry>()
            .ok_or_else(|| LuaError::RuntimeError("panel callback registry unavailable".into()))?;
        let callbacks = registry.0.read();
        callbacks
            .iter()
            .filter_map(|(&handle, callbacks)| {
                (callbacks.plugin_name == plugin_name).then_some(handle)
            })
            .collect()
    };
    Ok(handles)
}

fn clear_panel_callbacks(lua: &Lua, panel: PanelHandle) -> LuaResult<()> {
    let callbacks = {
        let Some(registry) = lua.app_data_ref::<crate::lua::PanelCallbackRegistry>() else {
            return Ok(());
        };
        let removed = registry.0.write().remove(&panel);
        removed
    };

    let Some(callbacks) = callbacks else {
        return Ok(());
    };

    if let Some(event_id) = callbacks.event_callback_id {
        remove_ui_callback(lua, callbacks.plugin_name, event_id)?;
    }
    Ok(())
}

fn clear_event_subscription(lua: &Lua, handle: SubscriptionHandle) -> LuaResult<()> {
    let handlers = lua
        .app_data_ref::<crate::lua::ContractEventHandlersWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("contract event handlers not initialized".into()))?;

    let removed = {
        let mut handlers = handlers.0.write();
        let mut removed = None;
        let mut empty_kind = None;

        for (&kind, entries) in handlers.iter_mut() {
            if let Some(index) = entries.iter().position(|entry| entry.handle == handle) {
                removed = Some(entries.remove(index));
                if entries.is_empty() {
                    empty_kind = Some(kind);
                }
                break;
            }
        }

        if let Some(kind) = empty_kind {
            handlers.remove(&kind);
        }

        removed
    };

    let entry = removed.ok_or_else(|| {
        LuaError::RuntimeError(
            crate::contract::ContractError::stale_handle(handle.to_string()).to_string(),
        )
    })?;
    lua.remove_registry_value(entry.callback_ref)?;
    Ok(())
}

fn ensure_event_subscription_owner(lua: &Lua, handle: SubscriptionHandle) -> LuaResult<()> {
    let plugin_name = current_plugin_name(lua)?;
    let handlers = lua
        .app_data_ref::<crate::lua::ContractEventHandlersWrapper>()
        .ok_or_else(|| LuaError::RuntimeError("contract event handlers not initialized".into()))?;

    let owner = {
        let handlers = handlers.0.read();
        handlers
            .values()
            .flat_map(|entries| entries.iter())
            .find(|entry| entry.handle == handle)
            .map(|entry| entry.plugin_name.clone())
    };

    match owner {
        Some(owner) if owner == plugin_name => Ok(()),
        Some(_) => Err(permission_denied_error(format!(
            "plugin '{plugin_name}' does not own {handle}"
        ))),
        None => Err(stale_handle_error(handle)),
    }
}

pub fn register_events_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.events.kind table — stable event kind constants
    let kind = lua.create_table()?;
    for &ek in crate::contract::events::EventKind::ALL {
        kind.set(snake_to_pascal(ek.as_str()), ek.as_str())?;
    }
    m.set("kind", kind)?;

    // helix.events.subscribe(kind_str, handler) -> SubscriptionHandle
    m.set(
        "subscribe",
        lua.create_function(|lua, (kind_str, handler): (String, LuaFunction)| {
            let event_kind = parse_event_kind(&kind_str)
                .map_err(|_| LuaError::RuntimeError(format!("unknown event kind: {kind_str}")))?;
            let plugin_name = current_plugin_name(lua)?;
            let plugin_id = current_plugin_id(lua)?;
            let wrapper = lua
                .app_data_ref::<crate::lua::ContractEventHandlersWrapper>()
                .ok_or_else(|| {
                    LuaError::RuntimeError("contract event handlers not initialized".into())
                })?;
            let callback_ref = lua.create_registry_value(handler)?;
            let handle = match event_host(lua)?.0.lock().subscribe(plugin_id, event_kind) {
                Ok(handle) => handle,
                Err(err) => {
                    lua.remove_registry_value(callback_ref)?;
                    return Err(LuaError::RuntimeError(err.to_string()));
                }
            };
            wrapper.0.write().entry(event_kind).or_default().push(
                crate::lua::RegisteredEventHandler {
                    handle,
                    plugin_name,
                    callback_ref,
                    failures: 0,
                },
            );

            lua.create_userdata(LuaSubscriptionHandle(handle))
        })?,
    )?;

    // helix.events.unsubscribe(handle)
    m.set(
        "unsubscribe",
        lua.create_function(|lua, handle: LuaSubscriptionHandle| {
            ensure_event_subscription_owner(lua, handle.0)?;
            let plugin_id = current_plugin_id(lua)?;
            event_host(lua)?
                .0
                .lock()
                .unsubscribe(plugin_id, handle.0)
                .map_err(contract_error)?;
            clear_event_subscription(lua, handle.0)?;
            Ok(())
        })?,
    )?;

    helix_table.set("events", m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.commands — register and execute commands
// ---------------------------------------------------------------------------

fn command_args_from_value(
    value: LuaValue,
    keep_empty: bool,
) -> LuaResult<(Option<Vec<String>>, Option<String>)> {
    match value {
        LuaValue::Nil => Ok((None, None)),
        LuaValue::String(arg) => {
            let arg = arg.to_str()?.to_string();
            Ok((Some(vec![arg.clone()]), Some(arg)))
        }
        LuaValue::Table(args_table) => {
            let mut args = Vec::new();
            for arg in args_table.sequence_values::<String>() {
                args.push(arg?);
            }
            let metadata = (!args.is_empty()).then(|| args.join(" "));
            let args = (keep_empty || !args.is_empty()).then_some(args);
            Ok((args, metadata))
        }
        other => Err(LuaError::FromLuaConversionError {
            from: other.type_name(),
            to: "command args".to_string(),
            message: Some("expected nil, string, or string array".into()),
        }),
    }
}

fn optional_command_handler(table: &LuaTable) -> LuaResult<Option<LuaFunction>> {
    match table.get::<LuaValue>("handler")? {
        LuaValue::Nil => Ok(None),
        LuaValue::Function(handler) => Ok(Some(handler)),
        other => Err(LuaError::FromLuaConversionError {
            from: other.type_name(),
            to: "function".to_string(),
            message: Some("expected command handler function".into()),
        }),
    }
}

fn command_definition_from_table(
    table: &LuaTable,
) -> LuaResult<(requests::CommandDefinition, crate::types::CommandMetadata)> {
    let name: String = table
        .get("name")
        .map_err(|_| LuaError::RuntimeError("Command name required".into()))?;
    let doc: String = table.get::<Option<String>>("doc")?.unwrap_or_default();
    let (definition_args, metadata_args) =
        command_args_from_value(table.get::<LuaValue>("args")?, false)?;

    Ok((
        requests::CommandDefinition {
            name: name.clone(),
            doc: (!doc.is_empty()).then_some(doc.clone()),
            args: definition_args,
        },
        crate::types::CommandMetadata {
            name,
            doc,
            args: metadata_args,
        },
    ))
}

fn command_update_from_table(
    command: CommandHandle,
    table: LuaTable,
) -> LuaResult<(requests::CommandUpdateRequest, Option<LuaFunction>)> {
    let name = table.get::<Option<String>>("name")?;
    let doc = table.get::<Option<String>>("doc")?;
    let (args, _) = command_args_from_value(table.get::<LuaValue>("args")?, true)?;
    let handler = optional_command_handler(&table)?;

    Ok((
        requests::CommandUpdateRequest {
            command,
            name,
            doc,
            args,
        },
        handler,
    ))
}

fn ensure_command_owner(lua: &Lua, command: CommandHandle) -> LuaResult<()> {
    let plugin_name = current_plugin_name(lua)?;
    let owner = {
        let registry = command_registry(lua)?;
        let commands = registry.0.read();
        commands.owner_for_handle(command).map(str::to_string)
    };

    match owner {
        Some(owner) if owner == plugin_name => Ok(()),
        Some(_) => Err(permission_denied_error(format!(
            "plugin '{plugin_name}' does not own {command}"
        ))),
        None => Err(stale_handle_error(command)),
    }
}

fn update_lua_command(lua: &Lua, command: CommandHandle, table: LuaTable) -> LuaResult<()> {
    ensure_command_owner(lua, command)?;
    let plugin_id = current_plugin_id(lua)?;
    let (req, handler) = command_update_from_table(command, table)?;
    let new_callback_ref = handler
        .map(|handler| lua.create_registry_value(handler))
        .transpose()?;

    let metadata = {
        let registry = command_registry(lua)?;
        let metadata = registry
            .0
            .read()
            .metadata_for_update(&req)
            .map_err(LuaError::RuntimeError)?;
        metadata
    };

    if let Err(err) = command_host(lua)?
        .0
        .lock()
        .update_command(plugin_id, req.clone())
    {
        if let Some(new_callback_ref) = new_callback_ref {
            lua.remove_registry_value(new_callback_ref)?;
        }
        return Err(LuaError::RuntimeError(err.to_string()));
    }

    let old_callback_ref = {
        let registry = command_registry(lua)?;
        let old_callback_ref = registry
            .0
            .write()
            .update(command, metadata, new_callback_ref)
            .map_err(LuaError::RuntimeError)?;
        old_callback_ref
    };

    if let Some(old_callback_ref) = old_callback_ref {
        lua.remove_registry_value(old_callback_ref)?;
    }
    Ok(())
}

fn remove_lua_command(lua: &Lua, command: CommandHandle) -> LuaResult<()> {
    ensure_command_owner(lua, command)?;
    let plugin_id = current_plugin_id(lua)?;

    command_host(lua)?
        .0
        .lock()
        .remove_command(plugin_id, requests::CommandRemoveRequest { command })
        .map_err(contract_error)?;

    let removed = {
        let registry = command_registry(lua)?;
        let removed = registry.0.write().remove(command);
        removed
    }
    .ok_or_else(|| {
        LuaError::RuntimeError(
            crate::contract::ContractError::stale_handle(command.to_string()).to_string(),
        )
    })?;

    lua.remove_registry_value(removed.callback_ref)?;
    Ok(())
}

fn store_suspended_command_thread(
    lua: &Lua,
    thread: &LuaThread,
    plugin_name: &str,
    yielded: LuaMultiValue,
) -> LuaResult<()> {
    crate::lua::suspend_coroutine_yield(lua, thread, plugin_name, yielded)
}

fn execute_registered_lua_command(lua: &Lua, name: &str, args: &[String]) -> LuaResult<bool> {
    let Some((plugin_name, callback)) = ({
        let registry = command_registry(lua)?;
        let commands = registry.0.read();
        commands.get_by_name(name).map(|command| {
            let callback = lua.registry_value::<LuaFunction>(&command.callback_ref)?;
            Ok::<_, LuaError>((command.plugin_name.clone(), callback))
        })
    })
    .transpose()?
    else {
        return Ok(false);
    };

    let thread = lua.create_thread(callback)?;
    let lua_args = lua.create_sequence_from(args.iter().cloned())?;
    crate::lua::with_current_plugin_name(lua, &plugin_name, || {
        let result: LuaMultiValue = thread.resume(lua_args)?;
        store_suspended_command_thread(lua, &thread, &plugin_name, result)
    })?;
    Ok(true)
}

fn command_descriptor_to_table(
    lua: &Lua,
    command: &crate::contract::CommandDescriptor,
) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;
    table.set("name", command.name.as_str())?;
    table.set(
        "aliases",
        lua.create_sequence_from(command.aliases.clone())?,
    )?;
    table.set("doc", command.doc.as_str())?;
    table.set(
        "arguments",
        lua.create_sequence_from(command.arguments.clone())?,
    )?;
    table.set(
        "kind",
        match command.kind {
            crate::contract::CommandKind::Typable => "typable",
            crate::contract::CommandKind::Static => "static",
            crate::contract::CommandKind::Plugin => "plugin",
        },
    )?;
    table.set(
        "scope",
        match command.scope {
            crate::contract::CommandScope::Viewport => "viewport",
            crate::contract::CommandScope::Tree => "tree",
            crate::contract::CommandScope::Frontend => "frontend",
        },
    )?;

    if let Some(signature) = &command.signature {
        let signature_table = lua.create_table()?;
        signature_table.set("min_positionals", signature.min_positionals)?;
        signature_table.set("max_positionals", signature.max_positionals)?;
        signature_table.set("raw_after", signature.raw_after)?;
        let flags = lua.create_table()?;
        for (index, flag) in signature.flags.iter().enumerate() {
            let flag_table = lua.create_table()?;
            flag_table.set("name", flag.name.as_str())?;
            flag_table.set("alias", flag.alias.map(|alias| alias.to_string()))?;
            flag_table.set("doc", flag.doc.as_str())?;
            flag_table.set("takes_value", flag.takes_value)?;
            flag_table.set("values", lua.create_sequence_from(flag.values.clone())?)?;
            flags.set(index + 1, flag_table)?;
        }
        signature_table.set("flags", flags)?;
        table.set("signature", signature_table)?;
    }

    Ok(table)
}

fn register_commands_module(
    lua: &Lua,
    helix_table: &LuaTable,
    commands: std::sync::Arc<parking_lot::RwLock<crate::lua::CommandRegistry>>,
) -> Result<()> {
    lua.set_app_data(crate::lua::CommandRegistryWrapper(commands.clone()));
    let m = lua.create_table()?;
    let raw = lua.create_table()?;

    // helix.commands.register({ name=, doc=, handler= }) -> CommandHandle
    let cmds = commands.clone();
    m.set(
        "register",
        lua.create_function(move |lua, table: LuaTable| {
            let (definition, meta) = command_definition_from_table(&table)?;
            let handler = optional_command_handler(&table)?.ok_or_else(|| {
                LuaError::RuntimeError("Command handler function required".into())
            })?;
            if !cmds.read().name_available(&meta.name, None) {
                return Err(LuaError::RuntimeError(format!(
                    "command already registered: {}",
                    meta.name
                )));
            }

            let plugin_name = current_plugin_name(lua)?;
            let plugin_id = current_plugin_id(lua)?;
            let callback_ref = lua.create_registry_value(handler)?;
            let handle = match command_host(lua)?
                .0
                .lock()
                .register_command(plugin_id, definition)
            {
                Ok(handle) => handle,
                Err(err) => {
                    lua.remove_registry_value(callback_ref)?;
                    return Err(LuaError::RuntimeError(err.to_string()));
                }
            };
            let registered = crate::lua::RegisteredCommand {
                handle,
                plugin_name,
                metadata: meta,
                callback_ref,
            };
            if let Err(err) = cmds.write().insert(registered) {
                let (registered, err) = *err;
                let _ = command_host(lua)?.0.lock().remove_command(
                    plugin_id,
                    requests::CommandRemoveRequest { command: handle },
                );
                lua.remove_registry_value(registered.callback_ref)?;
                return Err(LuaError::RuntimeError(err));
            }
            let handle = lua.create_userdata(LuaCommandHandle(handle))?;
            Ok(LuaValue::UserData(handle))
        })?,
    )?;

    // helix.commands.list() -> [{ name, aliases, doc, arguments, signature, kind, scope }]
    m.set(
        "list",
        lua.create_function(|lua, ()| {
            let catalog = command_host(lua)?.0.lock().command_catalog();
            let result = lua.create_table()?;
            for (index, command) in catalog.iter().enumerate() {
                result.set(index + 1, command_descriptor_to_table(lua, command)?)?;
            }
            Ok(result)
        })?,
    )?;

    // helix.commands.get(name) -> command metadata or nil
    m.set(
        "get",
        lua.create_function(|lua, name: String| {
            let command = command_host(lua)?
                .0
                .lock()
                .command_catalog()
                .into_iter()
                .find(|command| {
                    command.name == name || command.aliases.iter().any(|alias| alias == &name)
                });
            command
                .as_ref()
                .map(|command| command_descriptor_to_table(lua, command))
                .transpose()
        })?,
    )?;

    // helix.commands.update(handle, { name?, doc?, args?, handler? })
    m.set(
        "update",
        lua.create_function(|lua, (handle, table): (LuaCommandHandle, LuaTable)| {
            update_lua_command(lua, handle.0, table)
        })?,
    )?;

    // helix.commands.remove(handle)
    m.set(
        "remove",
        lua.create_function(|lua, handle: LuaCommandHandle| remove_lua_command(lua, handle.0))?,
    )?;

    raw.set(
        "execute_local",
        lua.create_function(|lua, (cmd, args): (String, Option<Vec<String>>)| {
            let args = args.unwrap_or_default();
            execute_registered_lua_command(lua, &cmd, &args)
        })?,
    )?;
    raw.set(
        "execute_host",
        lua.create_function(|lua, (name, args): (String, Option<Vec<String>>)| {
            start_task(
                lua,
                crate::contract::PluginTaskRequest::RunCommand(requests::RunCommandRequest {
                    name,
                    args: args.unwrap_or_default(),
                }),
            )
        })?,
    )?;
    m.set("_raw", raw.clone())?;

    helix_table.set("commands", m)?;
    Ok(())
}

fn keymap_definition_from_table(table: LuaTable) -> LuaResult<crate::contract::KeymapDefinition> {
    let mode = match table
        .get::<Option<String>>("mode")?
        .as_deref()
        .unwrap_or("normal")
    {
        "normal" => crate::contract::KeymapMode::Normal,
        "insert" => crate::contract::KeymapMode::Insert,
        "select" => crate::contract::KeymapMode::Select,
        mode => {
            return Err(LuaError::RuntimeError(format!(
                "unknown keymap mode: {mode}"
            )))
        }
    };
    let scope = match table.get::<Option<LuaTable>>("scope")? {
        Some(scope) => {
            if scope.contains_key("component")? {
                return Err(LuaError::RuntimeError(
                    "plugin keymaps target the editor; `scope.component` is not supported".into(),
                ));
            }
            crate::contract::KeymapScope {
                language: scope.get("language")?,
                path_prefix: scope.get("path_prefix")?,
            }
        }
        None => crate::contract::KeymapScope::default(),
    };
    let mut bindings = Vec::new();
    let binding_tables: LuaTable = table.get("bindings")?;
    for binding in binding_tables.sequence_values::<LuaTable>() {
        let binding = binding?;
        let keys = binding
            .get::<LuaTable>("keys")?
            .sequence_values::<String>()
            .collect::<LuaResult<Vec<_>>>()?;
        let commands = match binding.get::<LuaValue>("commands")? {
            LuaValue::Nil => vec![binding.get::<String>("command")?],
            LuaValue::String(command) => vec![command.to_str()?.to_string()],
            LuaValue::Table(commands) => commands
                .sequence_values::<String>()
                .collect::<LuaResult<Vec<_>>>()?,
            value => {
                return Err(LuaError::FromLuaConversionError {
                    from: value.type_name(),
                    to: "command string or array".into(),
                    message: None,
                })
            }
        };
        bindings.push(crate::contract::KeymapBinding { keys, commands });
    }
    Ok(crate::contract::KeymapDefinition {
        mode,
        scope,
        bindings,
    })
}

fn ensure_keymap_owner(lua: &Lua, keymap: crate::contract::KeymapHandle) -> LuaResult<()> {
    let plugin_name = current_plugin_name(lua)?;
    match keymap_registry(lua)?.0.read().get(&keymap) {
        Some(owner) if owner == &plugin_name => Ok(()),
        Some(_) => Err(permission_denied_error(format!(
            "plugin '{plugin_name}' does not own {keymap}"
        ))),
        None => Err(stale_handle_error(keymap)),
    }
}

fn update_lua_keymap(
    lua: &Lua,
    keymap: crate::contract::KeymapHandle,
    table: LuaTable,
) -> LuaResult<()> {
    ensure_keymap_owner(lua, keymap)?;
    let plugin = current_plugin_id(lua)?;
    let definition = keymap_definition_from_table(table)?;
    keymap_host(lua)?
        .0
        .lock()
        .update_keymap(
            plugin,
            crate::contract::KeymapUpdateRequest { keymap, definition },
        )
        .map_err(contract_error)
}

fn remove_lua_keymap(lua: &Lua, keymap: crate::contract::KeymapHandle) -> LuaResult<()> {
    ensure_keymap_owner(lua, keymap)?;
    let plugin = current_plugin_id(lua)?;
    keymap_host(lua)?
        .0
        .lock()
        .remove_keymap(plugin, crate::contract::KeymapRemoveRequest { keymap })
        .map_err(contract_error)?;
    keymap_registry(lua)?.0.write().remove(&keymap);
    Ok(())
}

fn register_keymaps_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let module = lua.create_table()?;
    module.set(
        "register",
        lua.create_function(|lua, table: LuaTable| {
            let plugin_name = current_plugin_name(lua)?;
            let plugin = current_plugin_id(lua)?;
            let definition = keymap_definition_from_table(table)?;
            let keymap = keymap_host(lua)?
                .0
                .lock()
                .register_keymap(plugin, definition)
                .map_err(contract_error)?;
            keymap_registry(lua)?.0.write().insert(keymap, plugin_name);
            lua.create_userdata(LuaKeymapHandle(keymap))
        })?,
    )?;
    module.set(
        "update",
        lua.create_function(|lua, (keymap, table): (LuaKeymapHandle, LuaTable)| {
            update_lua_keymap(lua, keymap.0, table)
        })?,
    )?;
    module.set(
        "remove",
        lua.create_function(|lua, keymap: LuaKeymapHandle| remove_lua_keymap(lua, keymap.0))?,
    )?;
    helix_table.set("keymaps", module)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.registers — read/write editor registers
// ---------------------------------------------------------------------------

fn register_registers_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.registers.get(name) -> [string]
    m.set(
        "get",
        lua.create_function(|lua, name_str: String| {
            let mut chars = name_str.chars();
            let name = chars
                .next()
                .filter(|_| chars.next().is_none())
                .ok_or_else(|| {
                    LuaError::RuntimeError("Register name must be exactly one character".into())
                })?;
            let table = lua.create_table()?;
            with_query_bridge(lua, |host| {
                let values = host.read_register(name).map_err(contract_error)?;
                for (i, value) in values.into_iter().enumerate() {
                    table.set(i + 1, value)?;
                }
                Ok(table)
            })
        })?,
    )?;

    // helix.registers.set(name, values)
    m.set(
        "set",
        lua.create_function(|lua, (name_str, values): (String, Vec<String>)| {
            let mut chars = name_str.chars();
            let name = chars
                .next()
                .filter(|_| chars.next().is_none())
                .ok_or_else(|| {
                    LuaError::RuntimeError("Register name must be exactly one character".into())
                })?;
            with_mutation_bridge(lua, |host| {
                host.write_register(name, values).map_err(contract_error)
            })
        })?,
    )?;

    helix_table.set("registers", m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// UI callback helper — shared by _raw.start_prompt / start_confirm / start_pick
// ---------------------------------------------------------------------------

/// Allocate a callback token and register the request with the UI handler.
/// The Lua coroutine yields this opaque token until the UI response arrives.
fn allocate_ui_callback(
    lua: &Lua,
) -> LuaResult<(
    mlua::AppDataRef<'_, crate::lua::UiHostWrapper>,
    crate::contract::PluginId,
    String,
)> {
    let plugin_name = current_plugin_name(lua)?;
    let plugin_id = current_plugin_id(lua)?;
    Ok((ui_host(lua)?, plugin_id, plugin_name))
}

fn register_pending_ui_callback(
    lua: &Lua,
    plugin_name: &str,
    token: UiCallbackToken,
) -> LuaResult<LuaValue> {
    let raw = token.raw().get();
    let callback_id = crate::types::UiCallbackId::new(raw)
        .ok_or_else(|| LuaError::RuntimeError("invalid UI callback token (zero)".into()))?;
    crate::lua::remember_pending_ui_callback(lua, plugin_name.to_string(), callback_id)?;
    Ok(LuaValue::UserData(lua.create_userdata(
        crate::lua::LuaUiCallbackToken::from(token),
    )?))
}

// ---------------------------------------------------------------------------
// helix.ui — notifications, prompts (coroutine-yielding), panels, theming
// ---------------------------------------------------------------------------

pub fn register_ui_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // -- Fire-and-forget notifications --

    m.set(
        "notify",
        lua.create_function(|lua, (message, level): (String, Option<String>)| {
            let level = match level.as_deref() {
                Some("error") => requests::NotifyLevel::Error,
                Some("warn" | "warning") => requests::NotifyLevel::Warn,
                _ => requests::NotifyLevel::Info,
            };
            ui_host(lua)?
                .0
                .lock()
                .notify(requests::NotifyRequest { message, level })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "info",
        lua.create_function(|lua, message: String| {
            ui_host(lua)?
                .0
                .lock()
                .notify(requests::NotifyRequest {
                    message,
                    level: requests::NotifyLevel::Info,
                })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "warn",
        lua.create_function(|lua, message: String| {
            ui_host(lua)?
                .0
                .lock()
                .notify(requests::NotifyRequest {
                    message,
                    level: requests::NotifyLevel::Warn,
                })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "error",
        lua.create_function(|lua, message: String| {
            ui_host(lua)?
                .0
                .lock()
                .notify(requests::NotifyRequest {
                    message,
                    level: requests::NotifyLevel::Error,
                })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "set_status",
        lua.create_function(|lua, message: String| {
            ui_host(lua)?
                .0
                .lock()
                .notify(requests::NotifyRequest {
                    message,
                    level: requests::NotifyLevel::Info,
                })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    // -- Coroutine-yielding UI operations --
    // These return a callback token; the Lua wrapper yields it so the coroutine
    // gets resumed with the response.

    // _raw.start_prompt(message, default?) -> UiCallbackToken
    let raw = lua.create_table()?;

    raw.set(
        "start_prompt",
        lua.create_function(|lua, (message, default): (String, Option<String>)| {
            let (handler, plugin_id, plugin_name) = allocate_ui_callback(lua)?;
            let token = handler
                .0
                .lock()
                .prompt(plugin_id, requests::PromptRequest { message, default })
                .map_err(contract_error)?;
            register_pending_ui_callback(lua, &plugin_name, token)
        })?,
    )?;

    // _raw.start_confirm(message) -> UiCallbackToken
    raw.set(
        "start_confirm",
        lua.create_function(|lua, message: String| {
            let (handler, plugin_id, plugin_name) = allocate_ui_callback(lua)?;
            let token = handler
                .0
                .lock()
                .confirm(plugin_id, requests::ConfirmRequest { message })
                .map_err(contract_error)?;
            register_pending_ui_callback(lua, &plugin_name, token)
        })?,
    )?;

    // _raw.start_pick(items, prompt?) -> UiCallbackToken
    raw.set(
        "start_pick",
        lua.create_function(|lua, (items, prompt): (Vec<String>, Option<String>)| {
            let (handler, plugin_id, plugin_name) = allocate_ui_callback(lua)?;
            let token = handler
                .0
                .lock()
                .picker(plugin_id, requests::PickerRequest { items, prompt })
                .map_err(contract_error)?;
            register_pending_ui_callback(lua, &plugin_name, token)
        })?,
    )?;

    m.set("_raw", raw.clone())?;

    // -- Panel --

    m.set(
        "panel",
        lua.create_function(|lua, opts: LuaTable| {
            let title: String = opts.get("title")?;
            let side: String = opts
                .get::<Option<String>>("side")?
                .unwrap_or_else(|| "right".into());
            let width: u16 = opts.get::<Option<u16>>("width")?.unwrap_or(30);
            if opts.contains_key("render")? {
                return Err(LuaError::RuntimeError(
                    "panel render callbacks are unsupported; use retained content nodes".into(),
                ));
            }
            let content = parse_panel_content(&opts)?;
            let event_fn: Option<LuaFunction> = opts.get("on_event").ok();

            let plugin_name = current_plugin_name(lua)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            let event_id = if let Some(ef) = event_fn {
                let callback_reg = lua
                    .app_data_ref::<crate::types::UiCallbackRegistry>()
                    .ok_or_else(|| {
                        LuaError::RuntimeError("UI callback registry unavailable".into())
                    })?;
                let counter = lua
                    .app_data_ref::<crate::types::UiCallbackCounter>()
                    .ok_or_else(|| {
                        LuaError::RuntimeError("UI callback counter unavailable".into())
                    })?;
                let eid = counter.next();
                let event_ref = lua.create_registry_value(ef)?;
                callback_reg.0.write().insert(
                    crate::types::PluginCallbackKey::new(plugin_name.clone(), eid),
                    event_ref,
                );
                Some(eid)
            } else {
                None
            };

            let panel = match handler.0.lock().register_panel(
                plugin_id,
                requests::PanelRegistration {
                    title,
                    side: parse_panel_side(&side)?,
                    size: Some(requests::PanelSizeSpec::Fixed(width)),
                    hidden: false,
                    content,
                },
            ) {
                Ok(panel) => panel,
                Err(err) => {
                    if let Some(event_id) = event_id {
                        remove_ui_callback(lua, plugin_name.clone(), event_id)?;
                    }
                    return Err(LuaError::RuntimeError(err.to_string()));
                }
            };
            let Some(panel_callbacks) = lua.app_data_ref::<crate::lua::PanelCallbackRegistry>()
            else {
                if let Some(event_id) = event_id {
                    remove_ui_callback(lua, plugin_name.clone(), event_id)?;
                }
                let _ = handler
                    .0
                    .lock()
                    .close_panel(plugin_id, requests::PanelCloseRequest { panel });
                return Err(LuaError::RuntimeError(
                    "panel callback registry unavailable".into(),
                ));
            };
            panel_callbacks.0.write().insert(
                panel,
                crate::lua::RegisteredPanelCallbacks {
                    plugin_name,
                    event_callback_id: event_id,
                },
            );
            let panel = lua.create_userdata(LuaPanelHandle(panel))?;
            Ok(LuaValue::UserData(panel))
        })?,
    )?;

    // -- Panel toggle / focus / resize / list --

    m.set(
        "toggle_panel",
        lua.create_function(|lua, panel: LuaPanelHandle| {
            ensure_panel_owner(lua, panel.0)?;
            let handler = panel_host(lua)?;
            let plugin_id = current_plugin_id(lua)?;
            {
                let mut host = handler.0.lock();
                host.toggle_panel(plugin_id, requests::TogglePanelRequest { panel: panel.0 })
                    .map_err(contract_error)?;
            }
            let visible = handler
                .0
                .lock()
                .list_panels()
                .into_iter()
                .find(|snapshot| snapshot.handle == panel.0)
                .map(|snapshot| snapshot.visible)
                .ok_or_else(|| LuaError::RuntimeError(format!("panel not found: {}", panel.0)))?;
            Ok(visible)
        })?,
    )?;

    m.set(
        "focus_panel",
        lua.create_function(|lua, panel: LuaPanelHandle| {
            ensure_panel_owner(lua, panel.0)?;
            let plugin_id = current_plugin_id(lua)?;
            panel_host(lua)?
                .0
                .lock()
                .focus_panel(plugin_id, requests::FocusPanelRequest { panel: panel.0 })
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "resize_panel",
        lua.create_function(|lua, (panel, size_str): (LuaPanelHandle, String)| {
            ensure_panel_owner(lua, panel.0)?;
            let plugin_id = current_plugin_id(lua)?;
            panel_host(lua)?
                .0
                .lock()
                .resize_panel(
                    plugin_id,
                    requests::ResizePanelRequest {
                        panel: panel.0,
                        size: parse_panel_size_spec(&size_str)?,
                    },
                )
                .map_err(contract_error)?;
            Ok(())
        })?,
    )?;

    m.set(
        "panels",
        lua.create_function(|lua, ()| {
            let current_plugin_panels = current_plugin_panel_handles(lua)?;
            let snapshots = panel_host(lua)?.0.lock().list_panels();
            let result = lua.create_table()?;
            for (i, panel) in snapshots
                .iter()
                .filter(|panel| current_plugin_panels.contains(&panel.handle))
                .enumerate()
            {
                let t = lua.create_table()?;
                t.set("handle", LuaPanelHandle(panel.handle))?;
                t.set("title", panel.title.as_str())?;
                t.set("tag", "plugin_panel")?;
                t.set(
                    "side",
                    match panel.side {
                        requests::PanelSide::Left => "left",
                        requests::PanelSide::Right => "right",
                        requests::PanelSide::Bottom => "bottom",
                    },
                )?;
                t.set("visible", panel.visible)?;
                result.set(i + 1, t)?;
            }
            Ok(result)
        })?,
    )?;

    // -- Theme --

    m.set(
        "get_theme",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |host| Ok(host.theme_snapshot().name))
        })?,
    )?;

    raw.set(
        "set_theme",
        lua.create_function(|lua, name: String| {
            start_task(lua, crate::contract::PluginTaskRequest::SetTheme(name))
        })?,
    )?;

    // -- Terminal size --

    m.set(
        "terminal_size",
        lua.create_function(|lua, ()| {
            let snapshot =
                with_query_bridge(lua, |host| host.terminal_size().map_err(contract_error))?;
            let size = lua.create_table()?;
            size.set("width", snapshot.width)?;
            size.set("height", snapshot.height)?;
            Ok(size)
        })?,
    )?;

    // -- Redraw --

    m.set(
        "redraw",
        lua.create_function(|lua, ()| {
            with_mutation_bridge(lua, |host| {
                host.request_redraw();
                Ok(())
            })
        })?,
    )?;

    helix_table.set("ui", m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.layout — layout combinators
// ---------------------------------------------------------------------------

fn parse_panel_side(s: &str) -> LuaResult<requests::PanelSide> {
    match s {
        "left" => Ok(requests::PanelSide::Left),
        "right" => Ok(requests::PanelSide::Right),
        "bottom" => Ok(requests::PanelSide::Bottom),
        _ => Err(LuaError::RuntimeError(format!("invalid panel side: {s}"))),
    }
}

fn parse_panel_content(options: &LuaTable) -> LuaResult<Vec<requests::UiRenderNode>> {
    let parse_node = |table: LuaTable| parse_ui_render_node(&table);

    match options.get::<LuaValue>("content")? {
        LuaValue::Nil => Ok(Vec::new()),
        LuaValue::String(text) => Ok(vec![requests::UiRenderNode::text(
            text.to_str()?.to_owned(),
        )]),
        LuaValue::Table(table) if table.contains_key("text")? => Ok(vec![parse_node(table)?]),
        LuaValue::Table(table) => table
            .sequence_values::<LuaTable>()
            .map(|node| parse_node(node?))
            .collect(),
        value => Err(LuaError::FromLuaConversionError {
            from: value.type_name(),
            to: "PanelContent".into(),
            message: Some("expected a string, text node, or array of text nodes".into()),
        }),
    }
}

fn parse_ui_render_node(table: &LuaTable) -> LuaResult<requests::UiRenderNode> {
    let kind = table
        .get::<Option<String>>("kind")?
        .unwrap_or_else(|| "text".into());
    let style = |default: &str| -> LuaResult<String> {
        Ok(table
            .get::<Option<String>>("style")?
            .unwrap_or_else(|| default.into()))
    };

    match kind.as_str() {
        "text" => Ok(requests::UiRenderNode::Text {
            x: table.get::<Option<u16>>("x")?.unwrap_or(0),
            y: table.get::<Option<u16>>("y")?.unwrap_or(0),
            text: table.get("text")?,
            style: style("ui.text")?,
            max_width: table.get("max_width")?,
        }),
        "fill" => Ok(requests::UiRenderNode::Fill {
            area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
            style: style("ui.text")?,
        }),
        "header" => {
            let current = table.get::<Option<usize>>("current")?;
            let total = table.get::<Option<usize>>("total")?;
            if current.is_some() != total.is_some() {
                return Err(LuaError::RuntimeError(
                    "header current and total must be provided together".into(),
                ));
            }
            Ok(requests::UiRenderNode::Header {
                area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
                title: table.get("title")?,
                current,
                total,
                style: style("ui.text")?,
            })
        }
        "horizontal_divider" => Ok(requests::UiRenderNode::HorizontalDivider {
            area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
            style: style("ui.text")?,
        }),
        "vertical_divider" => Ok(requests::UiRenderNode::VerticalDivider {
            area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
            style: style("ui.text")?,
        }),
        "text_input" => Ok(requests::UiRenderNode::TextInput {
            area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
            text: table.get("text")?,
            cursor: table.get("cursor")?,
            style: style("ui.text")?,
            cursor_style: table
                .get::<Option<String>>("cursor_style")?
                .unwrap_or_else(|| "ui.cursor".into()),
        }),
        "scrollbar" => Ok(requests::UiRenderNode::Scrollbar {
            area: parse_ui_rect(&table.get::<LuaTable>("area")?)?,
            total: table.get("total")?,
            offset: table.get("offset")?,
            visible: table.get("visible")?,
            thumb_style: table
                .get::<Option<String>>("thumb_style")?
                .unwrap_or_else(|| "ui.menu.scroll".into()),
            track_symbol: table.get("track_symbol")?,
            track_style: table
                .get::<Option<String>>("track_style")?
                .unwrap_or_else(|| "ui.background".into()),
        }),
        _ => Err(LuaError::RuntimeError(format!(
            "invalid retained UI node kind: {kind}"
        ))),
    }
}

fn parse_ui_rect(table: &LuaTable) -> LuaResult<requests::UiRect> {
    Ok(requests::UiRect {
        x: table.get::<Option<u16>>("x")?.unwrap_or(0),
        y: table.get::<Option<u16>>("y")?.unwrap_or(0),
        width: table.get("width")?,
        height: table.get("height")?,
    })
}

fn parse_panel_size_spec(s: &str) -> LuaResult<requests::PanelSizeSpec> {
    if let Some(n) = s.strip_prefix("fixed:") {
        let value: u16 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid fixed panel size: {s}")))?;
        return Ok(requests::PanelSizeSpec::Fixed(value));
    }
    if let Some(n) = s.strip_prefix("percent:") {
        let value: u8 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid percent panel size: {s}")))?;
        return Ok(requests::PanelSizeSpec::Percent(value));
    }
    Err(LuaError::RuntimeError(format!(
        "panel size must be fixed:N or percent:N, got: {s}"
    )))
}

// ---------------------------------------------------------------------------
// Rect helpers (for layout/surface)
// ---------------------------------------------------------------------------

/// Convert a Lua table {x, y, width, height} to contract geometry.
pub fn table_to_rect(t: &LuaTable) -> LuaResult<requests::UiRect> {
    Ok(requests::UiRect {
        x: t.get("x")?,
        y: t.get("y")?,
        width: t.get("width")?,
        height: t.get("height")?,
    })
}

/// Convert contract geometry to a Lua table.
pub fn rect_to_table(lua: &Lua, r: requests::UiRect) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("x", r.x)?;
    t.set("y", r.y)?;
    t.set("width", r.width)?;
    t.set("height", r.height)?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// helix.splits — split/view topology management
// ---------------------------------------------------------------------------

fn register_splits_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.splits.split(direction, opts?) -> ViewHandle
    m.set(
        "split",
        lua.create_function(|lua, (direction, opts): (String, Option<LuaTable>)| {
            let dir = parse_split_direction(&direction)?;
            let document = opts
                .as_ref()
                .and_then(|t| {
                    t.get::<Option<LuaDocumentHandle>>("document")
                        .ok()
                        .flatten()
                })
                .map(|d| d.0);
            let view = opts
                .as_ref()
                .and_then(|t| t.get::<Option<LuaViewHandle>>("view").ok().flatten())
                .map(|v| v.0);
            let view_handle = with_mutation_bridge(lua, |bridge| {
                bridge
                    .split_view(requests::SplitViewRequest {
                        view,
                        direction: dir,
                        document,
                    })
                    .map_err(contract_error)
            })?;
            Ok(LuaViewHandle(view_handle))
        })?,
    )?;

    // helix.splits.focus_direction(direction)
    m.set(
        "focus_direction",
        lua.create_function(|lua, direction: String| {
            let dir = parse_split_direction(&direction)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .focus_direction(requests::FocusDirectionRequest { direction: dir })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.splits.swap(direction)
    m.set(
        "swap",
        lua.create_function(|lua, direction: String| {
            let dir = parse_split_direction(&direction)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .swap_split(requests::SwapSplitRequest { direction: dir })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.splits.transpose(view?)
    m.set(
        "transpose",
        lua.create_function(|lua, view: Option<LuaViewHandle>| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .transpose(requests::TransposeSplitRequest {
                        view: view.map(|v| v.0),
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.splits.resize(opts) — { dimension = "width"|"height", amount = "grow:N"|"shrink:N" }
    m.set(
        "resize",
        lua.create_function(|lua, opts: LuaTable| {
            let dim_str: String = opts.get("dimension")?;
            let amount_str: String = opts.get("amount")?;
            let view = opts.get::<Option<LuaViewHandle>>("view")?.map(|v| v.0);
            let dimension = match dim_str.as_str() {
                "width" => requests::ResizeDimension::Width,
                "height" => requests::ResizeDimension::Height,
                _ => {
                    return Err(LuaError::RuntimeError(
                        "dimension must be 'width' or 'height'".into(),
                    ))
                }
            };
            let amount = parse_resize_amount(&amount_str)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .resize_split(requests::ResizeSplitRequest {
                        view,
                        dimension,
                        amount,
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.splits.tree() -> recursive table
    m.set(
        "tree",
        lua.create_function(|lua, ()| {
            let snap = with_query_bridge(lua, |bridge| Ok(bridge.split_tree()))?;
            split_node_to_table(lua, &snap.root)
        })?,
    )?;

    // helix.splits.list() -> array of ViewHandles
    m.set(
        "list",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |bridge| {
                Ok(bridge
                    .list_views()
                    .into_iter()
                    .map(LuaViewHandle)
                    .collect::<Vec<_>>())
            })
        })?,
    )?;

    helix_table.set("splits", m)?;
    Ok(())
}

fn parse_split_direction(s: &str) -> LuaResult<requests::SplitDirection> {
    match s {
        "right" => Ok(requests::SplitDirection::Right),
        "down" => Ok(requests::SplitDirection::Down),
        "left" => Ok(requests::SplitDirection::Left),
        "up" => Ok(requests::SplitDirection::Up),
        _ => Err(LuaError::RuntimeError(format!(
            "invalid split direction: {s} (expected right/down/left/up)"
        ))),
    }
}

fn parse_resize_amount(s: &str) -> LuaResult<requests::ResizeAmount> {
    if let Some(n) = s.strip_prefix("grow:") {
        let n: u16 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid grow amount: {s}")))?;
        Ok(requests::ResizeAmount::Grow(n))
    } else if let Some(n) = s.strip_prefix("shrink:") {
        let n: u16 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid shrink amount: {s}")))?;
        Ok(requests::ResizeAmount::Shrink(n))
    } else {
        Err(LuaError::RuntimeError(format!(
            "invalid resize amount: {s} (expected 'grow:N' or 'shrink:N')"
        )))
    }
}

fn split_node_to_table(lua: &Lua, node: &snapshots::SplitNodeSnapshot) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    match node {
        snapshots::SplitNodeSnapshot::Leaf { view } => {
            t.set("type", "leaf")?;
            t.set("view", LuaViewHandle(*view))?;
        }
        snapshots::SplitNodeSnapshot::Container {
            direction,
            children,
        } => {
            t.set("type", "container")?;
            t.set(
                "direction",
                match direction {
                    snapshots::SplitLayoutDirection::Horizontal => "horizontal",
                    snapshots::SplitLayoutDirection::Vertical => "vertical",
                },
            )?;
            let child_table = lua.create_table()?;
            for (i, child) in children.iter().enumerate() {
                child_table.set(i + 1, split_node_to_table(lua, child)?)?;
            }
            t.set("children", child_table)?;
        }
    }
    Ok(t)
}

fn parse_tab_close_arg(arg: LuaValue) -> LuaResult<(Option<ViewHandle>, Option<usize>)> {
    match arg {
        LuaValue::Nil => Ok((None, None)),
        LuaValue::Integer(index) if index >= 0 => Ok((None, Some(index as usize))),
        LuaValue::Number(index) if index >= 0.0 && index.fract() == 0.0 => {
            Ok((None, Some(index as usize)))
        }
        LuaValue::Table(table) => {
            let view = table
                .get::<Option<LuaViewHandle>>("view")?
                .map(|view| view.0);
            let index = table.get::<Option<usize>>("index")?;
            Ok((view, index))
        }
        other => Err(LuaError::FromLuaConversionError {
            from: other.type_name(),
            to: "tab close argument".to_string(),
            message: Some("expected nil, tab index, or { view?, index? }".into()),
        }),
    }
}

// ---------------------------------------------------------------------------
// helix.tabs — per-view tab group management
// ---------------------------------------------------------------------------

fn register_tabs_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.tabs.open(doc, opts?) — open a document as a new tab
    m.set(
        "open",
        lua.create_function(|lua, (doc, opts): (LuaDocumentHandle, Option<LuaTable>)| {
            let focus = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("focus").ok().flatten())
                .unwrap_or(true);
            let view = opts
                .as_ref()
                .and_then(|t| t.get::<Option<LuaViewHandle>>("view").ok().flatten())
                .map(|v| v.0);
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .open_tab(requests::OpenTabRequest {
                        view,
                        document: doc.0,
                        focus,
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.tabs.close(index_or_opts?)
    m.set(
        "close",
        lua.create_function(|lua, arg: LuaValue| {
            let (view, index) = parse_tab_close_arg(arg)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_tab(requests::CloseTabRequest { view, index })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.tabs.focus(index, view?)
    m.set(
        "focus",
        lua.create_function(|lua, (index, view): (usize, Option<LuaViewHandle>)| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .focus_tab(requests::FocusTabRequest {
                        view: view.map(|v| v.0),
                        index,
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.tabs.next(view?)
    m.set(
        "next",
        lua.create_function(|lua, view: Option<LuaViewHandle>| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .cycle_tab(requests::CycleTabRequest {
                        view: view.map(|v| v.0),
                        direction: requests::TabCycleDirection::Next,
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.tabs.previous(view?)
    m.set(
        "previous",
        lua.create_function(|lua, view: Option<LuaViewHandle>| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .cycle_tab(requests::CycleTabRequest {
                        view: view.map(|v| v.0),
                        direction: requests::TabCycleDirection::Previous,
                    })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.tabs.list(view?) -> { tabs = [...], active = N }
    m.set(
        "list",
        lua.create_function(|lua, view: Option<LuaViewHandle>| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.list_tabs(view.map(|v| v.0)).map_err(contract_error)
            })?;
            let t = lua.create_table()?;
            let tabs = lua.create_table()?;
            for (i, tab) in snap.tabs.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("document", LuaDocumentHandle(tab.document))?;
                entry.set("title", tab.title.as_str())?;
                entry.set("is_modified", tab.is_modified)?;
                tabs.set(i + 1, entry)?;
            }
            t.set("tabs", tabs)?;
            t.set("active", snap.active)?;
            Ok(t)
        })?,
    )?;

    helix_table.set("tabs", m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.floats — floating window management
// ---------------------------------------------------------------------------

fn register_floats_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.floats.create(opts) -> FloatHandle
    m.set(
        "create",
        lua.create_function(|lua, opts: LuaTable| {
            let title: Option<String> = opts.get("title").ok();
            let dismissible: bool = opts.get::<Option<bool>>("dismissible")?.unwrap_or(false);
            let focus: bool = opts.get::<Option<bool>>("focus")?.unwrap_or(true);
            let plugin_id = current_plugin_id(lua)?;

            let placement = parse_float_placement(&opts.get::<LuaTable>("placement")?)?;
            let content = parse_float_content(&opts)?;
            let float = with_mutation_bridge(lua, |bridge| {
                bridge
                    .create_float(
                        plugin_id,
                        requests::CreateFloatRequest {
                            title,
                            placement,
                            content,
                            focus,
                            dismissible,
                        },
                    )
                    .map_err(contract_error)
            })?;

            Ok(LuaFloatHandle(float))
        })?,
    )?;

    // helix.floats.close(float)
    m.set(
        "close",
        lua.create_function(|lua, float: LuaFloatHandle| {
            let plugin = current_plugin_id(lua)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_float(plugin, requests::CloseFloatRequest { float: float.0 })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.floats.list() -> array of { handle, title, is_focused }
    m.set(
        "list",
        lua.create_function(|lua, ()| {
            let plugin = current_plugin_id(lua)?;
            let result = lua.create_table()?;
            with_mutation_bridge(lua, |bridge| {
                for (i, entry) in bridge.list_floats(plugin).into_iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("handle", LuaFloatHandle(entry.handle))?;
                    t.set("title", entry.title)?;
                    t.set("is_focused", entry.is_focused)?;
                    result.set(i + 1, t)?;
                }
                Ok(result)
            })
        })?,
    )?;

    helix_table.set("floats", m)?;
    Ok(())
}

/// Lua userdata wrapper for a `FloatHandle`.
#[derive(Debug, Clone, Copy)]
struct LuaFloatHandle(FloatHandle);

impl FromLua for LuaFloatHandle {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(ud) => ud.borrow::<Self>().map(|h| *h),
            _ => Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "FloatHandle".to_string(),
                message: Some("expected a FloatHandle userdata".into()),
            }),
        }
    }
}

impl LuaUserData for LuaFloatHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("id", |_lua, this, ()| Ok(this.0.raw().get()));

        // float:close()
        methods.add_method("close", |lua, this, ()| {
            let plugin = current_plugin_id(lua)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_float(plugin, requests::CloseFloatRequest { float: this.0 })
                    .map_err(contract_error)?;
                Ok(())
            })
        });

        // float:update(opts) — update title, placement
        methods.add_method("update", |lua, this, opts: LuaTable| {
            let plugin = current_plugin_id(lua)?;
            let title = if opts.contains_key("title")? {
                Some(opts.get::<Option<String>>("title")?)
            } else {
                None
            };
            let placement = if opts.contains_key("placement")? {
                Some(parse_float_placement(&opts.get::<LuaTable>("placement")?)?)
            } else {
                None
            };
            let content = if opts.contains_key("content")? {
                Some(parse_float_content(&opts)?)
            } else {
                None
            };

            with_mutation_bridge(lua, |bridge| {
                bridge
                    .update_float(
                        plugin,
                        requests::UpdateFloatRequest {
                            float: this.0,
                            title,
                            placement,
                            content,
                        },
                    )
                    .map_err(contract_error)
            })
        });
    }

    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("handle", |_lua, this| Ok(this.0.raw().get()));
    }
}

fn parse_float_placement(t: &LuaTable) -> LuaResult<requests::FloatPlacement> {
    let ptype: String = t.get("type")?;
    match ptype.as_str() {
        "centered" => {
            let width: u16 = t.get("width")?;
            let height: u16 = t.get("height")?;
            Ok(requests::FloatPlacement::Centered { width, height })
        }
        "absolute" => {
            let x: u16 = t.get("x")?;
            let y: u16 = t.get("y")?;
            let width: u16 = t.get("width")?;
            let height: u16 = t.get("height")?;
            Ok(requests::FloatPlacement::Absolute {
                x,
                y,
                width,
                height,
            })
        }
        "anchored" => {
            let view: Option<LuaViewHandle> = t.get("view")?;
            let line: usize = t.get("line")?;
            let column: usize = t.get("column")?;
            let width: u16 = t.get("width")?;
            let height: u16 = t.get("height")?;
            let prefer = t
                .get::<Option<String>>("prefer")?
                .unwrap_or_else(|| "below".into());
            let prefer = match prefer.as_str() {
                "below" => requests::AnchorPreference::Below,
                "above" => requests::AnchorPreference::Above,
                _ => {
                    return Err(LuaError::RuntimeError(format!(
                        "invalid anchor preference: {prefer} (expected 'below' or 'above')"
                    )));
                }
            };
            Ok(requests::FloatPlacement::Anchored {
                view: view.map(|view| view.0),
                line,
                column,
                width,
                height,
                prefer,
            })
        }
        _ => Err(LuaError::RuntimeError(format!(
            "invalid placement type: {ptype} (expected 'centered', 'absolute', or 'anchored')"
        ))),
    }
}

fn parse_float_content(opts: &LuaTable) -> LuaResult<requests::FloatContent> {
    if opts.contains_key("render")? {
        return Err(LuaError::RuntimeError(
            "float render callbacks are not supported; provide retained `content`".into(),
        ));
    }

    let parse_block = |table: LuaTable| {
        Ok(requests::FloatBlock {
            text: table.get("text")?,
            style: table.get("style")?,
        })
    };

    let blocks = match opts.get::<LuaValue>("content")? {
        LuaValue::Nil => Vec::new(),
        LuaValue::String(text) => vec![requests::FloatBlock {
            text: text.to_str()?.to_owned(),
            style: None,
        }],
        LuaValue::Table(table) if table.contains_key("text")? => vec![parse_block(table)?],
        LuaValue::Table(table) => table
            .sequence_values::<LuaTable>()
            .map(|block| parse_block(block?))
            .collect::<LuaResult<Vec<_>>>()?,
        value => {
            return Err(LuaError::FromLuaConversionError {
                from: value.type_name(),
                to: "FloatContent".into(),
                message: Some("expected a string, text block, or array of text blocks".into()),
            });
        }
    };
    Ok(requests::FloatContent::Blocks(blocks))
}

// ---------------------------------------------------------------------------
// helix.assistant — assistant/AI system queries and mutations
// ---------------------------------------------------------------------------

fn register_assistant_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    // helix.assistant.snapshot() -> table
    m.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            let snap = with_query_bridge(lua, |bridge| Ok(bridge.assistant_snapshot()))?;
            let table = lua.create_table()?;
            table.set("active_thread", snap.active_thread.map(LuaThreadHandle))?;
            table.set("is_ready", snap.is_ready)?;
            let threads = lua.create_table()?;
            for (i, t) in snap.threads.iter().enumerate() {
                let tt = lua.create_table()?;
                tt.set("handle", LuaThreadHandle(t.handle))?;
                tt.set("title", t.title.as_deref())?;
                tt.set("run", format!("{:?}", t.run).to_lowercase())?;
                tt.set("entry_count", t.entry_count)?;
                tt.set("has_context", t.has_context)?;
                tt.set("is_active", t.is_active)?;
                tt.set("scope_cwd", t.scope_cwd.as_str())?;
                tt.set("follow", format!("{:?}", t.follow).to_lowercase())?;
                threads.set(i + 1, tt)?;
            }
            table.set("threads", threads)?;
            Ok(table)
        })?,
    )?;

    // helix.assistant.thread(thread) -> table
    m.set(
        "thread",
        lua.create_function(|lua, thread: LuaThreadHandle| {
            let snap = with_query_bridge(lua, |bridge| {
                bridge.thread_snapshot(thread.0).map_err(contract_error)
            })?;
            let table = lua.create_table()?;
            table.set("handle", LuaThreadHandle(snap.handle))?;
            table.set("title", snap.title.as_deref())?;
            table.set("run", format!("{:?}", snap.run).to_lowercase())?;
            table.set("entry_count", snap.entry_count)?;
            table.set("has_context", snap.has_context)?;
            table.set("is_active", snap.is_active)?;
            table.set("scope_cwd", snap.scope_cwd.as_str())?;
            table.set("follow", format!("{:?}", snap.follow).to_lowercase())?;
            Ok(table)
        })?,
    )?;

    // helix.assistant.entries(thread) -> [table]
    m.set(
        "entries",
        lua.create_function(|lua, thread: LuaThreadHandle| {
            let entries = with_query_bridge(lua, |bridge| {
                bridge.thread_entries(thread.0).map_err(contract_error)
            })?;
            let result = lua.create_table()?;
            for (i, entry) in entries.iter().enumerate() {
                let t = lua.create_table()?;
                t.set("id", entry.id)?;
                t.set("kind", entry.kind.as_str())?;
                t.set("text", entry.text.as_deref())?;
                t.set("location_count", entry.location_count)?;
                result.set(i + 1, t)?;
            }
            Ok(result)
        })?,
    )?;

    // helix.assistant.context(thread) -> [table]
    m.set(
        "context",
        lua.create_function(|lua, thread: LuaThreadHandle| {
            let items = with_query_bridge(lua, |bridge| {
                bridge.thread_context(thread.0).map_err(contract_error)
            })?;
            let result = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                let t = lua.create_table()?;
                t.set("id", item.id.as_str())?;
                t.set("kind", item.kind.as_str())?;
                t.set("label", item.label.as_str())?;
                result.set(i + 1, t)?;
            }
            Ok(result)
        })?,
    )?;

    // helix.assistant.is_ready() -> bool
    m.set(
        "is_ready",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |host| Ok(host.assistant_snapshot().is_ready))
        })?,
    )?;

    // helix.assistant.active_thread() -> ThreadHandle?
    m.set(
        "active_thread",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |host| {
                Ok(host.assistant_snapshot().active_thread.map(LuaThreadHandle))
            })
        })?,
    )?;

    // helix.assistant.thread_count() -> number
    m.set(
        "thread_count",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |host| Ok(host.assistant_snapshot().threads.len()))
        })?,
    )?;

    // helix.assistant.submit(thread_or_nil, text) — submit a prompt
    m.set(
        "submit",
        lua.create_function(|lua, (thread, text): (Option<LuaThreadHandle>, String)| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .submit_prompt(thread.map(|thread| thread.0), text)
                    .map_err(contract_error)
            })
        })?,
    )?;

    // helix.assistant.cancel(thread_or_nil) — cancel the active run
    m.set(
        "cancel",
        lua.create_function(|lua, thread: Option<LuaThreadHandle>| {
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .cancel_thread(thread.map(|thread| thread.0))
                    .map_err(contract_error)
            })
        })?,
    )?;

    helix_table.set("assistant", m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level registration
// ---------------------------------------------------------------------------

/// Register all contract-based facade modules as the Lua API.
pub(crate) fn register_facade(
    lua: &Lua,
    helix_table: &LuaTable,
    commands: std::sync::Arc<parking_lot::RwLock<crate::lua::CommandRegistry>>,
) -> Result<()> {
    register_workspace_module(lua, helix_table)?;
    register_documents_module(lua, helix_table)?;
    register_views_module(lua, helix_table)?;
    register_host_module(lua, helix_table)?;
    register_events_module(lua, helix_table)?;
    register_commands_module(lua, helix_table, commands)?;
    register_keymaps_module(lua, helix_table)?;
    register_registers_module(lua, helix_table)?;
    register_ui_module(lua, helix_table)?;
    register_splits_module(lua, helix_table)?;
    register_tabs_module(lua, helix_table)?;
    register_floats_module(lua, helix_table)?;
    register_assistant_module(lua, helix_table)?;
    register_lsp_module(lua, helix_table)?;
    register_syntax_module(lua, helix_table)?;
    register_layout_module(lua, helix_table)?;
    register_log_module(lua, helix_table)?;
    register_config_api(lua, helix_table)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.config() — per-plugin config
// ---------------------------------------------------------------------------

fn register_config_api(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let get_config = lua.create_function(move |lua, ()| {
        let plugin_name = current_plugin_name(lua)?;

        if let Some(config) = lua.app_data_ref::<crate::types::PluginConfig>() {
            if let Some(plugin_config) = config.plugins.iter().find(|p| p.name == plugin_name) {
                let val = match &plugin_config.config {
                    serde_json::Value::Object(map) => {
                        let table = lua.create_table()?;
                        for (k, v) in map {
                            match v {
                                serde_json::Value::String(s) => table.set(k.clone(), s.clone())?,
                                serde_json::Value::Number(n) => {
                                    table.set(k.clone(), n.as_f64().unwrap_or(0.0))?
                                }
                                serde_json::Value::Bool(b) => table.set(k.clone(), *b)?,
                                _ => {}
                            }
                        }
                        Some(table)
                    }
                    _ => None,
                };
                return Ok(val);
            }
        }
        Ok(None)
    })?;
    helix_table.set("config", get_config)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helix.async(fn) — launch a coroutine from synchronous context
// ---------------------------------------------------------------------------

/// Register the typed `_raw.store_suspended` bridge used by `helix.async`.
/// Must be called before the Lua wrappers are injected.
fn register_store_suspended(lua: &Lua, raw_table: &LuaTable) -> Result<()> {
    raw_table.set(
        "store_suspended",
        lua.create_function(|lua, (thread, token): (LuaThread, LuaValue)| {
            let plugin_name = current_plugin_name(lua)?;
            let key = crate::lua::await_key_from_lua(token)?;
            crate::lua::suspend_coroutine(lua, &thread, &plugin_name, key)
        })?,
    )?;
    Ok(())
}

/// Inject Lua wrappers that depend on the `helix` global being set.
///
/// Must be called AFTER `globals.set("helix", helix_table)`.
pub fn inject_lua_wrappers(lua: &Lua) -> Result<()> {
    // Register store_suspended on the raw table first
    let raw: LuaTable = lua.load("helix.ui._raw").eval()?;
    register_store_suspended(lua, &raw)?;

    // Coroutine-yielding UI wrappers + helix.async
    lua.load(
        r#"
local function __helix_contract_error(err)
    local marker = "__helix_contract_error__"
    if type(err) ~= "string" then
        local rendered = tostring(err)
        if rendered:find(marker, 1, true) == nil then
            return err
        end
        err = rendered
    end
    local start = err:find(marker, 1, true)
    if start == nil then
        return err
    end
    err = err:sub(start)
    local out = {}
    for line in err:gmatch("[^\n]+") do
        local k, v = line:match("^([%w_]+)=(.*)$")
        if k ~= nil then
            out[k] = v
        end
    end
    return {
        code = out.code or "internal_error",
        message = out.message or err,
        entity = out.entity ~= "" and out.entity or nil,
    }
end

local function __helix_wrap(fn)
    return function(...)
        local result = table.pack(pcall(fn, ...))
        if not result[1] then
            error(__helix_contract_error(result[2]), 2)
        end
        return table.unpack(result, 2, result.n)
    end
end

local function __helix_wrap_table(t, seen)
    if seen[t] then
        return
    end
    seen[t] = true
    for k, v in pairs(t) do
        if type(v) == "function" then
            t[k] = __helix_wrap(v)
        elseif type(v) == "table" then
            __helix_wrap_table(v, seen)
        end
    end
end

-- Coroutine-yielding UI wrappers.
-- Must be called from a coroutine context (command handler or helix.async).

function helix.documents.open(path, opts)
    if not coroutine.isyieldable() then
        error("helix.documents.open() must be called from a coroutine when document loading is asynchronous", 2)
    end
    local operation = helix.documents._raw.open(path, opts)
    local ok, result = coroutine.yield(operation)
    if not ok then
        error(result, 2)
    end
    return result
end

function helix.syntax.query(document, query, opts)
    if not coroutine.isyieldable() then
        error("helix.syntax.query() must be called from a coroutine", 2)
    end
    local operation = helix.syntax._raw.query(document, query, opts)
    local ok, result = coroutine.yield(operation)
    if not ok then
        error(result, 2)
    end
    return result
end

function helix.lsp.call(document, method, params, opts)
    local running, is_main = coroutine.running()
    if not running or is_main then
        error("helix.lsp.call() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local operation = helix.lsp._raw.call(document, method, params, opts)
    local ok, result = coroutine.yield(operation)
    if not ok then error(result, 2) end
    return result
end

function helix.commands.execute(name, args)
    args = args or {}
    if helix.commands._raw.execute_local(name, args) then return end
    local running, is_main = coroutine.running()
    if not running or is_main then
        error("helix.commands.execute() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local operation = helix.commands._raw.execute_host(name, args)
    local ok, result = coroutine.yield(operation)
    if not ok then error(result, 2) end
    return result
end

function helix.ui.set_theme(name)
    local running, is_main = coroutine.running()
    if not running or is_main then
        error("helix.ui.set_theme() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local operation = helix.ui._raw.set_theme(name)
    local ok, result = coroutine.yield(operation)
    if not ok then error(result, 2) end
    return result
end

function helix.ui.prompt(message, default)
    if not coroutine.isyieldable() then
        error("helix.ui.prompt() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local token = helix.ui._raw.start_prompt(message, default)
    return coroutine.yield(token)
end

function helix.ui.confirm(message)
    if not coroutine.isyieldable() then
        error("helix.ui.confirm() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local token = helix.ui._raw.start_confirm(message)
    return coroutine.yield(token)
end

function helix.ui.pick(items, prompt)
    if not coroutine.isyieldable() then
        error("helix.ui.pick() must be called from a coroutine (command handler or helix.async())", 2)
    end
    local token = helix.ui._raw.start_pick(items, prompt)
    return coroutine.yield(token)
end

function helix.async(fn, ...)
    local args = {...}
    local co = coroutine.create(fn)
    local ok, val = coroutine.resume(co, table.unpack(args))
    if not ok then
        error(val, 2)
    end
    if coroutine.status(co) == "suspended" then
        helix.ui._raw.store_suspended(co, val)
    end
end

__helix_wrap_table(helix, {})
"#,
    )
    .exec()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers — Lua table conversion
// ---------------------------------------------------------------------------

fn mode_to_string(mode: snapshots::EditMode) -> &'static str {
    match mode {
        snapshots::EditMode::Normal => "normal",
        snapshots::EditMode::Insert => "insert",
        snapshots::EditMode::Select => "select",
    }
}

fn parse_edit_mode(s: &str) -> LuaResult<snapshots::EditMode> {
    match s {
        "normal" => Ok(snapshots::EditMode::Normal),
        "insert" => Ok(snapshots::EditMode::Insert),
        "select" => Ok(snapshots::EditMode::Select),
        _ => Err(LuaError::RuntimeError(format!(
            "unknown mode: {s}. Expected: \"normal\", \"insert\", or \"select\""
        ))),
    }
}

fn snapshot_to_table(lua: &Lua, snap: &snapshots::DocumentSnapshot) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;
    table.set("handle", LuaDocumentHandle(snap.handle))?;
    table.set("path", snap.path.clone())?;
    table.set("language", snap.language.clone())?;
    table.set("is_modified", snap.is_modified)?;
    table.set("line_count", snap.line_count)?;
    table.set("mode", mode_to_string(snap.mode))?;

    let sels = lua.create_table()?;
    for (i, sel) in snap.selections.iter().enumerate() {
        let s = lua.create_table()?;
        let anchor = lua.create_table()?;
        anchor.set("line", sel.anchor.line)?;
        anchor.set("column", sel.anchor.column)?;
        let head = lua.create_table()?;
        head.set("line", sel.head.line)?;
        head.set("column", sel.head.column)?;
        s.set("anchor", anchor)?;
        s.set("head", head)?;
        sels.set(i + 1, s)?;
    }
    table.set("selections", sels)?;
    Ok(table)
}

fn view_snapshot_to_table(lua: &Lua, snap: &snapshots::ViewSnapshot) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;
    table.set("handle", LuaViewHandle(snap.handle))?;
    table.set("document", LuaDocumentHandle(snap.document))?;
    let cursor = lua.create_table()?;
    cursor.set("line", snap.cursor.line)?;
    cursor.set("column", snap.cursor.column)?;
    table.set("cursor", cursor)?;
    let vp = lua.create_table()?;
    vp.set("first_visible_line", snap.viewport.first_visible_line)?;
    vp.set("height", snap.viewport.height)?;
    vp.set("width", snap.viewport.width)?;
    table.set("viewport", vp)?;
    Ok(table)
}

fn diagnostics_to_table(lua: &Lua, snap: &snapshots::DiagnosticSnapshot) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;
    table.set("document", LuaDocumentHandle(snap.document))?;
    let diags = lua.create_table()?;
    for (i, d) in snap.diagnostics.iter().enumerate() {
        let entry = lua.create_table()?;
        let range = lua.create_table()?;
        let start = lua.create_table()?;
        start.set("line", d.start.line)?;
        start.set("column", d.start.column)?;
        let end = lua.create_table()?;
        end.set("line", d.end.line)?;
        end.set("column", d.end.column)?;
        range.set(1, start)?;
        range.set(2, end)?;
        entry.set("range", range)?;
        entry.set("message", d.message.as_str())?;
        entry.set(
            "severity",
            match d.severity {
                snapshots::DiagnosticSeverity::Error => "error",
                snapshots::DiagnosticSeverity::Warning => "warning",
                snapshots::DiagnosticSeverity::Info => "info",
                snapshots::DiagnosticSeverity::Hint => "hint",
            },
        )?;
        diags.set(i + 1, entry)?;
    }
    table.set("diagnostics", diags)?;
    Ok(table)
}

fn parse_text_edit(table: &LuaTable) -> LuaResult<requests::TextEdit> {
    let start = parse_position(&table.get::<LuaTable>("start")?)?;
    // Accept both "end" and "finish" for the end position (end is a Lua keyword).
    let end = table
        .get::<Option<LuaTable>>("finish")?
        .or(table.get::<Option<LuaTable>>("end")?)
        .ok_or_else(|| {
            LuaError::RuntimeError("text edit must have 'finish' or 'end' field".into())
        })?;
    let end = parse_position(&end)?;
    let new_text: String = table.get::<Option<String>>("text")?.unwrap_or_default();
    Ok(requests::TextEdit {
        start,
        end,
        new_text,
    })
}

fn parse_position(table: &LuaTable) -> LuaResult<snapshots::Position> {
    Ok(snapshots::Position {
        line: table.get("line")?,
        column: table.get("column")?,
    })
}

fn parse_selection_range(table: &LuaTable) -> LuaResult<snapshots::SelectionRange> {
    let anchor = parse_position(&table.get::<LuaTable>("anchor")?)?;
    let head = parse_position(&table.get::<LuaTable>("head")?)?;
    Ok(snapshots::SelectionRange { anchor, head })
}

/// Parse a color expressed as `"#rrggbb"` hex, `{r, g, b}` integer array,
/// or `{r = .., g = .., b = ..}` keyed table.
fn parse_color(value: LuaValue) -> LuaResult<snapshots::Color> {
    match value {
        LuaValue::String(s) => {
            let value = s.to_str()?;
            let hex = value
                .strip_prefix('#')
                .ok_or_else(|| LuaError::RuntimeError("colors must use #rrggbb syntax".into()))?;
            if hex.len() != 6 {
                return Err(LuaError::RuntimeError(
                    "colors must use #rrggbb syntax".into(),
                ));
            }
            let channel = |range: std::ops::Range<usize>| {
                u8::from_str_radix(&hex[range], 16)
                    .map_err(|_| LuaError::RuntimeError("invalid hex color".into()))
            };
            Ok(snapshots::Color {
                r: channel(0..2)?,
                g: channel(2..4)?,
                b: channel(4..6)?,
            })
        }
        LuaValue::Table(t) => {
            let r: u8 = t.get("r").or_else(|_| t.get(1))?;
            let g: u8 = t.get("g").or_else(|_| t.get(2))?;
            let b: u8 = t.get("b").or_else(|_| t.get(3))?;
            Ok(snapshots::Color { r, g, b })
        }
        other => Err(LuaError::FromLuaConversionError {
            from: other.type_name(),
            to: "Color".to_string(),
            message: Some("expected hex string or rgb table".into()),
        }),
    }
}

/// Parse a single annotation from a Lua table.
///
/// Expected shape:
/// ```lua
/// { line = 0, column = 0, text = "...", fg = "#rrggbb", bg = {r,g,b}, is_line = false }
/// ```
fn parse_annotation(table: &LuaTable) -> LuaResult<requests::Annotation> {
    let line: usize = table.get("line")?;
    let column: usize = table.get::<Option<usize>>("column")?.unwrap_or(0);
    let text: String = table.get("text")?;
    let is_line: bool = table.get::<Option<bool>>("is_line")?.unwrap_or(false);
    let offset: u16 = table.get::<Option<u16>>("offset")?.unwrap_or(0);
    let fg = match table.get::<LuaValue>("fg")? {
        LuaValue::Nil => None,
        v => Some(parse_color(v)?),
    };
    let bg = match table.get::<LuaValue>("bg")? {
        LuaValue::Nil => None,
        v => Some(parse_color(v)?),
    };

    Ok(requests::Annotation {
        position: snapshots::Position { line, column },
        text,
        style: requests::AnnotationStyle { fg, bg },
        offset,
        is_line,
        virtual_line: table.get::<Option<u16>>("virt_line_idx")?,
        dropped_text: table.get::<Option<String>>("dropped_text")?,
    })
}

/// Convert a contract `PluginEvent` to a Lua table for handler dispatch.
pub fn contract_event_to_table(
    lua: &Lua,
    event: &crate::contract::events::PluginEvent,
) -> LuaResult<LuaTable> {
    use crate::contract::events::PluginEvent as E;
    let t = lua.create_table()?;
    t.set("kind", event.kind().as_str())?;

    match event {
        E::HostReady(e) => {
            t.set("api_version", e.api_version)?;
        }
        E::DocumentOpened(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("path", e.path.clone())?;
            t.set("language", e.language.clone())?;
        }
        E::DocumentChanged(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
        }
        E::DocumentPreSave(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("path", e.path.clone())?;
        }
        E::DocumentSaved(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("path", e.path.clone())?;
            t.set("success", e.success)?;
        }
        E::DocumentClosed(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
        }
        E::SelectionChanged(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("view", LuaViewHandle(e.view))?;
            let cursor = lua.create_table()?;
            cursor.set("line", e.primary_cursor.line)?;
            cursor.set("column", e.primary_cursor.column)?;
            t.set("cursor", cursor)?;
        }
        E::ModeChanged(e) => {
            t.set("old", mode_to_string(e.old))?;
            t.set("new", mode_to_string(e.new))?;
        }
        E::ViewFocused(e) => {
            t.set("view", LuaViewHandle(e.view))?;
            t.set("document", LuaDocumentHandle(e.document))?;
        }
        E::DiagnosticsUpdated(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("count", e.count)?;
        }
        E::LspAttached(e) => {
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("server_name", e.server_name.as_str())?;
        }
        E::KeyPressed(e) => {
            t.set("key", e.key.as_str())?;
            t.set("mode", mode_to_string(e.mode))?;
        }
        E::SplitCreated(e) => {
            t.set("new_view", LuaViewHandle(e.new_view))?;
            t.set("source_view", LuaViewHandle(e.source_view))?;
        }
        E::SplitClosed(e) => {
            t.set("view", LuaViewHandle(e.view))?;
        }
        E::TabOpened(e) => {
            t.set("view", LuaViewHandle(e.view))?;
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("index", e.index)?;
        }
        E::TabClosed(e) => {
            t.set("view", LuaViewHandle(e.view))?;
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("index", e.index)?;
        }
        E::TabFocused(e) => {
            t.set("view", LuaViewHandle(e.view))?;
            t.set("document", LuaDocumentHandle(e.document))?;
            t.set("index", e.index)?;
        }
        E::FloatCreated(e) => {
            t.set("float", LuaFloatHandle(e.float))?;
        }
        E::FloatClosed(e) => {
            t.set("float", LuaFloatHandle(e.float))?;
        }
        E::PanelToggled(e) => {
            t.set("panel", LuaPanelHandle(e.panel))?;
            t.set("visible", e.visible)?;
        }
        E::AssistantThreadCreated(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
            t.set("title", e.title.clone())?;
            t.set("scope_cwd", e.scope_cwd.as_str())?;
        }
        E::AssistantThreadClosed(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
        }
        E::AssistantRunStarted(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
        }
        E::AssistantRunCompleted(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
            t.set("success", e.success)?;
            t.set("error", e.error.clone())?;
        }
        E::AssistantMessageReceived(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
            t.set("entry_id", e.entry_id)?;
            t.set("kind", e.kind.as_str())?;
        }
        E::AssistantContextChanged(e) => {
            t.set("thread", LuaThreadHandle(e.thread))?;
            t.set("attached", e.attached)?;
            t.set("context_kind", e.context_kind.as_str())?;
        }
    }
    Ok(t)
}

/// Convert a snake_case event kind string to PascalCase for Lua constant names.
fn snake_to_pascal(s: &str) -> String {
    use heck::ToPascalCase;
    s.to_pascal_case()
}

fn parse_event_kind(s: &str) -> std::result::Result<crate::contract::events::EventKind, ()> {
    crate::contract::events::EventKind::from_id(s).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_to_pascal_conversion() {
        assert_eq!(snake_to_pascal("document_opened"), "DocumentOpened");
        assert_eq!(snake_to_pascal("host_ready"), "HostReady");
        assert_eq!(snake_to_pascal("key_pressed"), "KeyPressed");
    }

    #[test]
    fn parse_event_kind_underscore() {
        use crate::contract::events::EventKind;
        assert_eq!(
            parse_event_kind("document_opened"),
            Ok(EventKind::DocumentOpened)
        );
        assert_eq!(parse_event_kind("mode_changed"), Ok(EventKind::ModeChanged));
        assert_eq!(parse_event_kind("host_ready"), Ok(EventKind::HostReady));
    }

    #[test]
    fn parse_event_kind_unknown() {
        assert!(parse_event_kind("nonexistent_event").is_err());
        assert!(parse_event_kind("DocumentOpened").is_err());
    }

    #[test]
    fn facade_registration() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_host_module(&lua, &helix_table).unwrap();

        let host: LuaTable = helix_table.get("host").unwrap();
        assert!(host.contains_key("api_metadata").unwrap());
    }

    #[test]
    fn event_kind_constants_registered() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();

        register_events_module(&lua, &helix_table).unwrap();

        let events: LuaTable = helix_table.get("events").unwrap();
        let kind: LuaTable = events.get("kind").unwrap();

        let doc_opened: String = kind.get("DocumentOpened").unwrap();
        assert_eq!(doc_opened, "document_opened");

        let mode_changed: String = kind.get("ModeChanged").unwrap();
        assert_eq!(mode_changed, "mode_changed");
    }

    #[test]
    fn workspace_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_workspace_module(&lua, &helix_table).unwrap();

        let ws: LuaTable = helix_table.get("workspace").unwrap();
        assert!(ws.contains_key("focused_document").unwrap());
        assert!(ws.contains_key("focused_view").unwrap());
        assert!(ws.contains_key("mode").unwrap());
        assert!(ws.contains_key("set_mode").unwrap());
        assert!(ws.contains_key("snapshot").unwrap());
        assert!(ws.contains_key("theme").unwrap());
        assert!(ws.contains_key("documents").unwrap());
        assert!(ws.contains_key("views").unwrap());
        assert!(ws.contains_key("editor_config").unwrap());
    }

    #[test]
    fn documents_raw_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_documents_module(&lua, &helix_table).unwrap();

        let docs: LuaTable = helix_table.get("documents").unwrap();
        assert!(docs.contains_key("list").unwrap());
        let raw: LuaTable = docs.get("_raw").unwrap();
        assert!(raw.contains_key("open").unwrap());
    }

    #[test]
    fn views_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_views_module(&lua, &helix_table).unwrap();

        let views: LuaTable = helix_table.get("views").unwrap();
        assert!(views.contains_key("list").unwrap());
    }

    #[test]
    fn commands_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        let commands = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::lua::CommandRegistry::default(),
        ));
        register_commands_module(&lua, &helix_table, commands).unwrap();

        let cmds: LuaTable = helix_table.get("commands").unwrap();
        assert!(cmds.contains_key("register").unwrap());
        assert!(cmds.contains_key("update").unwrap());
        assert!(cmds.contains_key("remove").unwrap());
        let raw: LuaTable = cmds.get("_raw").unwrap();
        assert!(raw.contains_key("execute_local").unwrap());
        assert!(raw.contains_key("execute_host").unwrap());
    }

    #[test]
    fn registers_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_registers_module(&lua, &helix_table).unwrap();

        let regs: LuaTable = helix_table.get("registers").unwrap();
        assert!(regs.contains_key("get").unwrap());
        assert!(regs.contains_key("set").unwrap());
    }

    #[test]
    fn events_use_typed_subscription_api() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_events_module(&lua, &helix_table).unwrap();

        let events: LuaTable = helix_table.get("events").unwrap();
        assert!(events.contains_key("subscribe").unwrap());
        assert!(events.contains_key("unsubscribe").unwrap());
        assert!(!events.contains_key("on").unwrap());
    }

    #[test]
    fn config_uses_new_name() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_config_api(&lua, &helix_table).unwrap();

        assert!(helix_table.contains_key("config").unwrap());
        // "get_config" should NOT exist
        assert!(!helix_table.contains_key("get_config").unwrap());
    }

    #[test]
    fn splits_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_splits_module(&lua, &helix_table).unwrap();

        let splits: LuaTable = helix_table.get("splits").unwrap();
        assert!(splits.contains_key("split").unwrap());
        assert!(splits.contains_key("focus_direction").unwrap());
        assert!(splits.contains_key("swap").unwrap());
        assert!(splits.contains_key("transpose").unwrap());
        assert!(splits.contains_key("resize").unwrap());
        assert!(splits.contains_key("tree").unwrap());
        assert!(splits.contains_key("list").unwrap());
    }

    #[test]
    fn tabs_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_tabs_module(&lua, &helix_table).unwrap();

        let tabs: LuaTable = helix_table.get("tabs").unwrap();
        assert!(tabs.contains_key("open").unwrap());
        assert!(tabs.contains_key("close").unwrap());
        assert!(tabs.contains_key("focus").unwrap());
        assert!(tabs.contains_key("next").unwrap());
        assert!(tabs.contains_key("previous").unwrap());
        assert!(tabs.contains_key("list").unwrap());
    }

    #[test]
    fn floats_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_floats_module(&lua, &helix_table).unwrap();

        let floats: LuaTable = helix_table.get("floats").unwrap();
        assert!(floats.contains_key("create").unwrap());
        assert!(floats.contains_key("close").unwrap());
        assert!(floats.contains_key("list").unwrap());
    }

    #[test]
    fn ui_module_has_enhanced_panel_api() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_ui_module(&lua, &helix_table).unwrap();

        let ui: LuaTable = helix_table.get("ui").unwrap();
        // Original methods
        assert!(ui.contains_key("notify").unwrap());
        assert!(ui.contains_key("info").unwrap());
        assert!(ui.contains_key("warn").unwrap());
        assert!(ui.contains_key("error").unwrap());
        assert!(ui.contains_key("set_status").unwrap());
        assert!(ui.contains_key("panel").unwrap());
        assert!(ui.contains_key("get_theme").unwrap());
        let raw: LuaTable = ui.get("_raw").unwrap();
        assert!(raw.contains_key("set_theme").unwrap());
        assert!(ui.contains_key("terminal_size").unwrap());
        assert!(ui.contains_key("redraw").unwrap());
        // Enhanced panel API
        assert!(ui.contains_key("toggle_panel").unwrap());
        assert!(ui.contains_key("focus_panel").unwrap());
        assert!(ui.contains_key("resize_panel").unwrap());
        assert!(ui.contains_key("panels").unwrap());
    }

    #[test]
    fn lua_panel_handle_exposes_id_and_handle_field() {
        let lua = Lua::new();
        let handle = PanelHandle::from_raw(std::num::NonZeroU64::new(7).unwrap());
        let userdata = lua.create_userdata(LuaPanelHandle(handle)).unwrap();
        lua.globals().set("panel", userdata).unwrap();

        let id: u64 = lua.load("return panel:id()").eval().unwrap();
        let field: u64 = lua.load("return panel.handle").eval().unwrap();

        assert_eq!(id, 7);
        assert_eq!(field, 7);
    }

    #[test]
    fn lua_command_handle_exposes_id_and_handle_field() {
        let lua = Lua::new();
        let handle = CommandHandle::from_raw(std::num::NonZeroU64::new(9).unwrap());
        let userdata = lua.create_userdata(LuaCommandHandle(handle)).unwrap();
        lua.globals().set("command", userdata).unwrap();

        let id: u64 = lua.load("return command:id()").eval().unwrap();
        let field: u64 = lua.load("return command.handle").eval().unwrap();

        assert_eq!(id, 9);
        assert_eq!(field, 9);
    }

    #[test]
    fn lua_subscription_handle_exposes_id_and_handle_field() {
        let lua = Lua::new();
        let handle = SubscriptionHandle::from_raw(std::num::NonZeroU64::new(11).unwrap());
        let userdata = lua.create_userdata(LuaSubscriptionHandle(handle)).unwrap();
        lua.globals().set("subscription", userdata).unwrap();

        let id: u64 = lua.load("return subscription:id()").eval().unwrap();
        let field: u64 = lua.load("return subscription.handle").eval().unwrap();

        assert_eq!(id, 11);
        assert_eq!(field, 11);
    }

    #[test]
    fn parse_panel_side_valid() {
        assert!(matches!(
            parse_panel_side("left").unwrap(),
            requests::PanelSide::Left
        ));
        assert!(matches!(
            parse_panel_side("right").unwrap(),
            requests::PanelSide::Right
        ));
        assert!(matches!(
            parse_panel_side("bottom").unwrap(),
            requests::PanelSide::Bottom
        ));
    }

    #[test]
    fn parse_panel_side_invalid() {
        assert!(parse_panel_side("top").is_err());
    }

    #[test]
    fn parse_split_direction_valid() {
        assert!(matches!(
            parse_split_direction("right").unwrap(),
            requests::SplitDirection::Right
        ));
        assert!(matches!(
            parse_split_direction("left").unwrap(),
            requests::SplitDirection::Left
        ));
        assert!(matches!(
            parse_split_direction("up").unwrap(),
            requests::SplitDirection::Up
        ));
        assert!(matches!(
            parse_split_direction("down").unwrap(),
            requests::SplitDirection::Down
        ));
    }

    #[test]
    fn parse_split_direction_invalid() {
        assert!(parse_split_direction("diagonal").is_err());
    }

    #[test]
    fn parse_resize_amount_valid() {
        assert!(matches!(
            parse_resize_amount("grow:5").unwrap(),
            requests::ResizeAmount::Grow(5)
        ));
        assert!(matches!(
            parse_resize_amount("shrink:3").unwrap(),
            requests::ResizeAmount::Shrink(3)
        ));
    }

    #[test]
    fn parse_resize_amount_invalid() {
        assert!(parse_resize_amount("expand:5").is_err());
        assert!(parse_resize_amount("grow:abc").is_err());
    }
}
