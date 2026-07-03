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
//! - `helix.pkg`        — package-manager backend registration
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
use std::num::NonZeroU64;

use crate::contract::bridge::{EditorMutationBridge, EditorQueryBridge};
use crate::contract::handles::{
    CommandHandle, DocumentHandle, FloatHandle, PanelHandle, RenderCallbackHandle,
    SubscriptionHandle, ThreadHandle, ViewHandle,
};
use crate::contract::host::{PluginFacadeMutationHost, PluginFacadeQueryHost, UiCallbackToken};
use crate::contract::requests;
use crate::contract::snapshots;
use crate::error::Result;
use crate::types::SurfaceRenderOp;

mod documents;
mod host;
mod layout;
mod logging;
mod lsp;
mod views;
mod workspace;
pub use documents::register as register_documents_module;
pub use host::register as register_host_module;
pub use layout::register as register_layout_module;
pub use logging::register as register_log_module;
pub use lsp::register as register_lsp_module;
pub use views::register as register_views_module;
pub use workspace::register as register_workspace_module;

fn with_editor<T>(f: impl FnOnce(&helix_view::Editor) -> LuaResult<T>) -> LuaResult<T> {
    crate::lua::with_current_editor(f)?
}

fn with_editor_mut<T>(f: impl FnOnce(&mut helix_view::Editor) -> LuaResult<T>) -> LuaResult<T> {
    crate::lua::with_current_editor_mut(f)?
}

fn with_query_bridge<T>(
    lua: &Lua,
    f: impl FnOnce(&dyn PluginFacadeQueryHost) -> LuaResult<T>,
) -> LuaResult<T> {
    if let Some(host) = lua
        .app_data_ref::<crate::lua::RemoteFacadeHostWrapper>()
        .map(|host| std::sync::Arc::clone(&host.0))
    {
        let host = host.lock();
        return f(host.query());
    }
    with_editor(|editor| {
        let bridge = EditorQueryBridge::new(editor);
        f(&bridge)
    })
}

fn with_mutation_bridge<T>(
    lua: &Lua,
    f: impl FnOnce(&mut dyn PluginFacadeMutationHost) -> LuaResult<T>,
) -> LuaResult<T> {
    if let Some(host) = lua
        .app_data_ref::<crate::lua::RemoteFacadeHostWrapper>()
        .map(|host| std::sync::Arc::clone(&host.0))
    {
        let mut host = host.lock();
        return f(host.mutation());
    }
    with_editor_mut(|editor| {
        let mut bridge = EditorMutationBridge::new(editor);
        f(&mut bridge)
    })
}

fn contract_error(err: crate::contract::ContractError) -> LuaError {
    let entity = err.entity().unwrap_or("");
    LuaError::RuntimeError(format!(
        "__helix_contract_error__\ncode={}\nmessage={}\nentity={}",
        err.code(),
        err,
        entity
    ))
}

fn with_surface<T>(
    f: impl FnOnce(&mut crate::types::SurfaceRenderOps, &helix_view::Theme) -> LuaResult<T>,
) -> LuaResult<T> {
    crate::lua::with_current_render_context(f)?
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
        methods.add_method("select_all", |_lua, this, ()| {
            with_editor_mut(|editor| {
                let doc_id = crate::contract::adapt::resolve_document(editor, this.0)
                    .map_err(contract_error)?;
                let view_id = editor
                    .tree
                    .try_get(editor.tree.focus)
                    .filter(|view| view.doc == doc_id)
                    .map(|view| view.id)
                    .or_else(|| {
                        editor
                            .tree
                            .views()
                            .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
                    })
                    .or_else(|| editor.tree.views().next().map(|(view, _)| view.id))
                    .ok_or_else(|| LuaError::RuntimeError("view not found".into()))?;
                let doc = editor
                    .documents
                    .get(&doc_id)
                    .ok_or_else(|| LuaError::RuntimeError("document not found".into()))?;
                let text = doc.text();
                let len = text.len_chars();
                if len == 0 {
                    Ok(())
                } else {
                    let selection = helix_core::Selection::single(0, len);
                    let doc = editor.documents.get_mut(&doc_id).ok_or_else(|| {
                        contract_error(crate::contract::ContractError::stale_handle(
                            this.0.to_string(),
                        ))
                    })?;
                    doc.ensure_view_init(view_id);
                    doc.set_selection(view_id, selection);
                    Ok(())
                }
            })
        });

        // doc:set_annotations(annotations) — replace virtual text annotations
        // for the calling plugin on this document. Annotations are scoped by
        // plugin name so multiple plugins coexist.
        methods.add_method(
            "set_annotations",
            |lua, this, annotations: Vec<LuaTable>| {
                let plugin_name = current_plugin_name(lua)?;

                // Parse first (before borrowing editor) so errors surface cleanly.
                let parsed: Vec<ParsedAnnotation> = annotations
                    .iter()
                    .map(parse_annotation)
                    .collect::<LuaResult<_>>()?;

                with_editor_mut(|editor| {
                    let doc_id = crate::contract::adapt::resolve_document(editor, this.0)
                        .map_err(contract_error)?;

                    // Resolve positions against the document text.
                    let doc = editor.documents.get(&doc_id).ok_or_else(|| {
                        contract_error(crate::contract::ContractError::stale_handle(
                            this.0.to_string(),
                        ))
                    })?;
                    let text = doc.text();
                    let converted: Vec<helix_view::document::PluginAnnotation> = parsed
                        .into_iter()
                        .map(|p| helix_view::document::PluginAnnotation {
                            char_idx: crate::contract::adapt::position_to_char(text, p.position),
                            ..p.annot
                        })
                        .collect();

                    // Find all views showing this document.
                    let view_ids: Vec<helix_view::ViewId> = editor
                        .tree
                        .views()
                        .filter_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
                        .collect();

                    if view_ids.is_empty() {
                        return Ok(());
                    }

                    let doc = editor.documents.get_mut(&doc_id).ok_or_else(|| {
                        contract_error(crate::contract::ContractError::stale_handle(
                            this.0.to_string(),
                        ))
                    })?;
                    let mut iter = view_ids.into_iter();
                    let Some(first) = iter.next() else {
                        return Ok(());
                    };
                    for view_id in iter {
                        doc.set_plugin_annotations(view_id, plugin_name.clone(), converted.clone());
                    }
                    doc.set_plugin_annotations(first, plugin_name, converted);
                    Ok(())
                })
            },
        );

        // doc:clear_annotations() — remove all annotations registered by the
        // calling plugin on this document.
        methods.add_method("clear_annotations", |lua, this, ()| {
            let plugin_name = current_plugin_name(lua)?;
            with_editor_mut(|editor| {
                let doc_id = crate::contract::adapt::resolve_document(editor, this.0)
                    .map_err(contract_error)?;
                let doc = editor.documents.get_mut(&doc_id).ok_or_else(|| {
                    contract_error(crate::contract::ContractError::stale_handle(
                        this.0.to_string(),
                    ))
                })?;
                doc.clear_plugin_annotations(&plugin_name);
                Ok(())
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

    remove_ui_callback(
        lua,
        callbacks.plugin_name.clone(),
        callbacks.render_callback_id,
    )?;
    if let Some(event_id) = callbacks.event_callback_id {
        remove_ui_callback(lua, callbacks.plugin_name, event_id)?;
    }
    Ok(())
}

fn float_render_callback(
    float: FloatHandle,
) -> LuaResult<Option<(String, crate::types::UiCallbackId)>> {
    with_editor(|editor| {
        let float_id =
            crate::contract::adapt::resolve_float(&editor.model, float).map_err(contract_error)?;
        Ok(editor.model.float(float_id).and_then(|entry| {
            entry
                .content
                .downcast_ref::<helix_view::model::PluginFloatModel>()
                .and_then(|model| {
                    crate::types::UiCallbackId::new(model.render_callback_id)
                        .map(|id| (model.plugin_name.clone(), id))
                })
        }))
    })
}

fn remove_float_render_callback(lua: &Lua, float: FloatHandle) -> LuaResult<()> {
    if let Some((plugin_name, callback_id)) = float_render_callback(float)? {
        remove_ui_callback(lua, plugin_name, callback_id)?;
    }
    Ok(())
}

fn set_lua_float_owner(float: FloatHandle, plugin_name: &str) -> LuaResult<()> {
    with_editor_mut(|editor| {
        let float_id =
            crate::contract::adapt::resolve_float(&editor.model, float).map_err(contract_error)?;
        let entry = editor
            .model
            .float_mut(float_id)
            .ok_or_else(|| stale_handle_error(float))?;
        entry.owner = Some(plugin_name.to_string());
        if let Some(model) = entry
            .content
            .downcast_mut::<helix_view::model::PluginFloatModel>()
        {
            model.plugin_name = plugin_name.to_string();
        }
        Ok(())
    })
}

fn ensure_float_owner(lua: &Lua, float: FloatHandle) -> LuaResult<()> {
    let plugin_name = current_plugin_name(lua)?;
    let owner = with_editor(|editor| {
        let float_id =
            crate::contract::adapt::resolve_float(&editor.model, float).map_err(contract_error)?;
        Ok(editor
            .model
            .float(float_id)
            .map(|entry| entry.owner.clone()))
    })?;

    match owner {
        Some(Some(owner)) if owner == plugin_name => Ok(()),
        Some(_) => Err(permission_denied_error(format!(
            "plugin '{plugin_name}' does not own float"
        ))),
        None => Err(stale_handle_error(float)),
    }
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
    if thread.status() != LuaThreadStatus::Resumable {
        return Ok(());
    }

    let id_val = yielded.into_iter().next().ok_or_else(|| {
        LuaError::RuntimeError("command coroutine yielded without a UI callback token".into())
    })?;
    let token: UiCallbackToken = lua.unpack(id_val)?;
    let callback_id = crate::types::UiCallbackId::new(token.raw().get()).ok_or_else(|| {
        LuaError::RuntimeError("command coroutine yielded zero UI callback token".into())
    })?;
    crate::lua::claim_pending_ui_callback(lua, plugin_name, callback_id)?;
    let registry = lua
        .app_data_ref::<crate::lua::SuspendedCoroutineRegistry>()
        .ok_or_else(|| {
            LuaError::RuntimeError("suspended coroutine registry not available".into())
        })?;
    let thread_key = lua.create_registry_value(thread.clone())?;
    let mut suspended = registry.0.write();
    if suspended.contains_key(&callback_id) {
        return Err(LuaError::RuntimeError(format!(
            "UI callback {} is already bound to a coroutine",
            callback_id.get()
        )));
    }
    suspended.insert(
        callback_id,
        crate::lua::SuspendedCoroutine {
            thread_key,
            plugin_name: plugin_name.to_string(),
        },
    );
    Ok(())
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

fn register_commands_module(
    lua: &Lua,
    helix_table: &LuaTable,
    commands: std::sync::Arc<parking_lot::RwLock<crate::lua::CommandRegistry>>,
) -> Result<()> {
    lua.set_app_data(crate::lua::CommandRegistryWrapper(commands.clone()));
    let m = lua.create_table()?;

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

    // helix.commands.execute(name, args?)
    m.set(
        "execute",
        lua.create_function(|lua, (cmd, args): (String, Option<Vec<String>>)| {
            let args = args.unwrap_or_default();
            let result = command_host(lua)?
                .0
                .lock()
                .run_command(requests::RunCommandRequest {
                    name: cmd.clone(),
                    args: args.clone(),
                });
            match result {
                Ok(()) => Ok(()),
                Err(err @ crate::contract::ContractError::NotFound { .. }) => {
                    if execute_registered_lua_command(lua, &cmd, &args)? {
                        Ok(())
                    } else {
                        Err(LuaError::RuntimeError(format!(
                            "Command '{cmd}' failed: {err}"
                        )))
                    }
                }
                Err(err) => Err(LuaError::RuntimeError(format!(
                    "Command '{cmd}' failed: {err}"
                ))),
            }
        })?,
    )?;

    helix_table.set("commands", m)?;
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
            let name = name_str.chars().next().ok_or_else(|| {
                LuaError::RuntimeError("Register name must be a single character".into())
            })?;
            let table = lua.create_table()?;
            with_editor(|editor| {
                if let Some(values) = editor.read_register(name) {
                    for (i, val) in values.enumerate() {
                        table.set(i + 1, val.to_string())?;
                    }
                }
                Ok(table)
            })
        })?,
    )?;

    // helix.registers.set(name, values)
    m.set(
        "set",
        lua.create_function(|_lua, (name_str, values): (String, Vec<String>)| {
            let name = name_str.chars().next().ok_or_else(|| {
                LuaError::RuntimeError("Register name must be a single character".into())
            })?;
            with_editor_mut(|editor| {
                editor.write_register(name, values).map_err(|e| {
                    LuaError::RuntimeError(format!("Failed to write to register {name}: {e}"))
                })
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
    Ok(LuaValue::UserData(lua.create_userdata(token)?))
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

    m.set("_raw", raw)?;

    // -- Panel --

    m.set(
        "panel",
        lua.create_function(|lua, opts: LuaTable| {
            let title: String = opts.get("title")?;
            let side: String = opts
                .get::<Option<String>>("side")?
                .unwrap_or_else(|| "right".into());
            let width: u16 = opts.get::<Option<u16>>("width")?.unwrap_or(30);
            let render_fn: LuaFunction = opts.get("render")?;
            let event_fn: Option<LuaFunction> = opts.get("on_event").ok();

            let plugin_name = current_plugin_name(lua)?;
            let plugin_id = current_plugin_id(lua)?;
            let handler = panel_host(lua)?;
            let Some(callback_reg) = lua.app_data_ref::<crate::types::UiCallbackRegistry>() else {
                return Ok(LuaNil);
            };
            let Some(counter) = lua.app_data_ref::<crate::types::UiCallbackCounter>() else {
                return Ok(LuaNil);
            };

            let render_id = counter.next();
            let render_ref = lua.create_registry_value(render_fn)?;
            callback_reg.0.write().insert(
                crate::types::PluginCallbackKey::new(plugin_name.clone(), render_id),
                render_ref,
            );

            let event_id = if let Some(ef) = event_fn {
                let eid = counter.next();
                let event_ref = match lua.create_registry_value(ef) {
                    Ok(event_ref) => event_ref,
                    Err(err) => {
                        remove_ui_callback(lua, plugin_name.clone(), render_id)?;
                        return Err(err);
                    }
                };
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
                },
            ) {
                Ok(panel) => panel,
                Err(err) => {
                    remove_ui_callback(lua, plugin_name.clone(), render_id)?;
                    if let Some(event_id) = event_id {
                        remove_ui_callback(lua, plugin_name.clone(), event_id)?;
                    }
                    return Err(LuaError::RuntimeError(err.to_string()));
                }
            };
            let Some(panel_callbacks) = lua.app_data_ref::<crate::lua::PanelCallbackRegistry>()
            else {
                remove_ui_callback(lua, plugin_name.clone(), render_id)?;
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
                    render_callback_id: render_id,
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
        lua.create_function(|_lua, ()| with_editor(|editor| Ok(editor.theme.name().to_string())))?,
    )?;

    m.set(
        "set_theme",
        lua.create_function(|_lua, name: String| {
            with_editor_mut(|editor| match editor.theme_loader.load(&name) {
                Ok(theme) => {
                    editor.set_theme(theme);
                    Ok(())
                }
                Err(e) => Err(LuaError::RuntimeError(format!(
                    "Failed to load theme {name}: {e}"
                ))),
            })
        })?,
    )?;

    // -- Terminal size --

    m.set(
        "terminal_size",
        lua.create_function(|lua, ()| {
            let size = lua.create_table()?;
            with_editor(|editor| {
                let area = editor.tree.area();
                size.set("width", area.width)?;
                size.set("height", area.height)?;
                Ok(size)
            })
        })?,
    )?;

    // -- Redraw --

    m.set(
        "redraw",
        lua.create_function(|_lua, ()| {
            with_editor_mut(|editor| {
                editor.request_redraw();
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
// helix.surface — render surface userdata
// ---------------------------------------------------------------------------

/// Lua userdata handle to the current render surface.
pub struct LuaSurface;

impl LuaUserData for LuaSurface {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "set_string",
            |_lua, _this, (x, y, text, scope): (u16, u16, String, String)| {
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::SetString { x, y, text, style });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "set_stringn",
            |_lua, _this, (x, y, text, max_width, scope): (u16, u16, String, usize, String)| {
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::SetStringN {
                        x,
                        y,
                        text,
                        max_width,
                        style,
                    });
                    Ok(())
                })
            },
        );

        methods.add_method("clear", |_lua, _this, (area, scope): (LuaTable, String)| {
            let area = table_to_rect(&area)?;
            with_surface(|ops, theme| {
                let style = theme.get(&scope);
                ops.push(SurfaceRenderOp::Clear { area, style });
                Ok(())
            })
        });

        methods.add_method(
            "set_style",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::SetStyle { area, style });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "header",
            |_lua, _this, (area, title, scope): (LuaTable, String, String)| {
                let area = table_to_rect(&area)?;
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::Header { area, title, style });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "header_with_counts",
            |_lua,
             _this,
             (area, title, current, total, scope): (LuaTable, String, usize, usize, String)| {
                let area = table_to_rect(&area)?;
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::HeaderWithCounts {
                        area,
                        title,
                        current,
                        total,
                        style,
                    });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "hdivider",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::HDivider { area, style });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "vdivider",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    ops.push(SurfaceRenderOp::VDivider { area, style });
                    Ok(())
                })
            },
        );

        methods.add_method(
            "text_input",
            |lua,
             _this,
             (area, text, cursor, scope, cursor_scope): (
                LuaTable,
                String,
                usize,
                String,
                String,
            )| {
                let area = table_to_rect(&area)?;
                let state = helix_view::layout::text_input_layout(area, &text, cursor);
                with_surface(|ops, theme| {
                    let style = theme.get(&scope);
                    let cursor_style = theme.get(&cursor_scope);
                    ops.push(SurfaceRenderOp::TextInput {
                        area,
                        text,
                        cursor,
                        style,
                        cursor_style,
                    });
                    let result = lua.create_table()?;
                    result.set("cursor_x", state.cursor_x)?;
                    result.set("cursor_y", state.cursor_y)?;
                    Ok(result)
                })
            },
        );

        methods.add_method(
            "scrollbar",
            |_lua, _this, (area, opts): (LuaTable, LuaTable)| {
                let area = table_to_rect(&area)?;
                let total: usize = opts.get("total")?;
                let offset: usize = opts.get("offset")?;
                let visible: usize = opts.get("visible")?;
                let thumb_scope: String = opts.get("thumb_style")?;
                let track_scope: Option<String> = opts.get("track_style").ok();
                let track_symbol: Option<String> = opts.get("track_symbol").ok();
                with_surface(|ops, theme| {
                    let thumb_style = theme.get(&thumb_scope);
                    let track_style = track_scope
                        .as_deref()
                        .map(|scope| theme.get(scope))
                        .unwrap_or_default();
                    ops.push(SurfaceRenderOp::Scrollbar {
                        area,
                        total,
                        offset,
                        visible,
                        thumb_style,
                        track_symbol,
                        track_style,
                    });
                    Ok(())
                })
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Rect helpers (for layout/surface)
// ---------------------------------------------------------------------------

/// Convert a Lua table {x, y, width, height} to a Rect.
pub fn table_to_rect(t: &LuaTable) -> LuaResult<helix_view::graphics::Rect> {
    Ok(helix_view::graphics::Rect::new(
        t.get("x")?,
        t.get("y")?,
        t.get("width")?,
        t.get("height")?,
    ))
}

/// Convert a Rect to a Lua table.
pub fn rect_to_table(lua: &Lua, r: helix_view::graphics::Rect) -> LuaResult<LuaTable> {
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
            let plugin_name = current_plugin_name(lua)?;
            let plugin_id = current_plugin_id(lua)?;

            let placement = parse_float_placement(&opts.get::<LuaTable>("placement")?)?;
            let (content, render_callback) = parse_float_content(lua, &opts, &plugin_name)?;
            let float = match with_mutation_bridge(lua, |bridge| {
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
            }) {
                Ok(float) => float,
                Err(err) => {
                    if let Some(callback_id) = render_callback {
                        remove_ui_callback(lua, plugin_name, callback_id)?;
                    }
                    return Err(err);
                }
            };

            if let Err(err) = set_lua_float_owner(float, &plugin_name) {
                if let Some(callback_id) = render_callback {
                    remove_ui_callback(lua, plugin_name, callback_id)?;
                }
                let _ = with_mutation_bridge(lua, |bridge| {
                    bridge
                        .close_float(requests::CloseFloatRequest { float })
                        .map_err(contract_error)?;
                    Ok(())
                });
                return Err(err);
            }

            Ok(LuaFloatHandle(float))
        })?,
    )?;

    // helix.floats.close(float)
    m.set(
        "close",
        lua.create_function(|lua, float: LuaFloatHandle| {
            ensure_float_owner(lua, float.0)?;
            remove_float_render_callback(lua, float.0)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_float(requests::CloseFloatRequest { float: float.0 })
                    .map_err(contract_error)?;
                Ok(())
            })
        })?,
    )?;

    // helix.floats.list() -> array of { handle, title, is_focused }
    m.set(
        "list",
        lua.create_function(|lua, ()| {
            let plugin_name = current_plugin_name(lua)?;
            let result = lua.create_table()?;
            with_editor(|editor| {
                for (i, (id, entry)) in editor
                    .model
                    .floats
                    .iter()
                    .filter(|(_, entry)| entry.owner.as_deref() == Some(plugin_name.as_str()))
                    .enumerate()
                {
                    let t = lua.create_table()?;
                    let handle = crate::contract::adapt::float_handle(id);
                    t.set("handle", LuaFloatHandle(handle))?;
                    t.set("title", entry.title.clone())?;
                    t.set(
                        "is_focused",
                        editor.model.focus == helix_view::model::FocusTarget::Float(id),
                    )?;
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
            ensure_float_owner(lua, this.0)?;
            remove_float_render_callback(lua, this.0)?;
            with_mutation_bridge(lua, |bridge| {
                bridge
                    .close_float(requests::CloseFloatRequest { float: this.0 })
                    .map_err(contract_error)?;
                Ok(())
            })
        });

        // float:update(opts) — update title, placement
        methods.add_method("update", |lua, this, opts: LuaTable| {
            ensure_float_owner(lua, this.0)?;
            let plugin_name = current_plugin_name(lua)?;
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
            let (content, new_render_callback) =
                if opts.contains_key("render")? || opts.contains_key("content")? {
                    let (content, callback) = parse_float_content(lua, &opts, &plugin_name)?;
                    (Some(content), callback)
                } else {
                    (None, None)
                };
            let old_render_callback = if content.is_some() {
                float_render_callback(this.0)?
            } else {
                None
            };

            if let Err(err) = with_mutation_bridge(lua, |bridge| {
                bridge
                    .update_float(requests::UpdateFloatRequest {
                        float: this.0,
                        title,
                        placement,
                        content,
                    })
                    .map_err(contract_error)
            }) {
                if let Some(callback_id) = new_render_callback {
                    remove_ui_callback(lua, plugin_name, callback_id)?;
                }
                return Err(err);
            }

            if let Some((plugin_name, callback_id)) = old_render_callback {
                remove_ui_callback(lua, plugin_name, callback_id)?;
            }
            Ok(())
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

fn parse_float_content(
    lua: &Lua,
    opts: &LuaTable,
    plugin_name: &str,
) -> LuaResult<(requests::FloatContent, Option<crate::types::UiCallbackId>)> {
    if let Some(render_fn) = opts.get::<Option<LuaFunction>>("render")? {
        let callback_reg = lua
            .app_data_ref::<crate::types::UiCallbackRegistry>()
            .ok_or_else(|| LuaError::RuntimeError("UI callback registry not available".into()))?;
        let counter = lua
            .app_data_ref::<crate::types::UiCallbackCounter>()
            .ok_or_else(|| LuaError::RuntimeError("UI callback counter not available".into()))?;

        let render_id = counter.next();
        let render_ref = lua.create_registry_value(render_fn)?;
        callback_reg.0.write().insert(
            crate::types::PluginCallbackKey::new(plugin_name.to_string(), render_id),
            render_ref,
        );

        let callback = RenderCallbackHandle::from_raw(
            NonZeroU64::new(render_id.get()).expect("UI callback IDs are non-zero"),
        );
        return Ok((
            requests::FloatContent::PluginRender { callback },
            Some(render_id),
        ));
    }

    // Check for content as array of blocks (default)
    if let Ok(content_table) = opts.get::<Vec<LuaTable>>("content") {
        let blocks: Vec<requests::FloatBlock> = content_table
            .iter()
            .map(|block| {
                let text: String = block.get("text")?;
                let style = block.get::<Option<String>>("style")?;
                Ok(requests::FloatBlock { text, style })
            })
            .collect::<LuaResult<Vec<_>>>()?;
        Ok((requests::FloatContent::Blocks(blocks), None))
    } else {
        // Default: empty text float
        Ok((requests::FloatContent::Blocks(Vec::new()), None))
    }
}

// ---------------------------------------------------------------------------
// helix.pkg — package-manager backend registration
// ---------------------------------------------------------------------------

fn register_pkg_module(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;
    let backends = lua.create_table()?;
    m.set("_backends", backends.clone())?;
    m.set(
        "register_backend",
        lua.create_function(move |_lua, spec: LuaTable| {
            let name: String = spec.get("name")?;
            if name.trim().is_empty() {
                return Err(LuaError::RuntimeError(
                    "package backend name must not be empty".into(),
                ));
            }
            for key in ["probe", "resolve", "install", "remove", "doctor"] {
                let value: LuaValue = spec.get(key)?;
                if !matches!(value, LuaValue::Function(_)) {
                    return Err(LuaError::RuntimeError(format!(
                        "package backend {name} requires function field {key}"
                    )));
                }
            }
            backends.set(name, spec)?;
            Ok(())
        })?,
    )?;
    helix_table.set("pkg", m)?;
    Ok(())
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
        lua.create_function(|_lua, ()| with_editor(|editor| Ok(!editor.assistant.is_empty())))?,
    )?;

    // helix.assistant.active_thread() -> ThreadHandle?
    m.set(
        "active_thread",
        lua.create_function(|_lua, ()| {
            with_editor(|editor| {
                Ok(editor
                    .assistant
                    .active()
                    .map(crate::contract::adapt::thread_handle)
                    .map(LuaThreadHandle))
            })
        })?,
    )?;

    // helix.assistant.thread_count() -> number
    m.set(
        "thread_count",
        lua.create_function(|_lua, ()| {
            with_editor(|editor| Ok(editor.assistant.threads().count()))
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
    register_registers_module(lua, helix_table)?;
    register_ui_module(lua, helix_table)?;
    register_splits_module(lua, helix_table)?;
    register_tabs_module(lua, helix_table)?;
    register_floats_module(lua, helix_table)?;
    register_pkg_module(lua, helix_table)?;
    register_assistant_module(lua, helix_table)?;
    register_lsp_module(lua, helix_table)?;
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

/// Register the `_raw.store_suspended` Rust function on the `helix.ui._raw` table.
/// Must be called before the Lua wrappers are injected.
fn register_store_suspended(lua: &Lua, raw_table: &LuaTable) -> Result<()> {
    raw_table.set(
        "store_suspended",
        lua.create_function(|lua, (thread, token): (LuaThread, UiCallbackToken)| {
            let plugin_name = current_plugin_name(lua)?;

            let cb_id = crate::types::UiCallbackId::new(token.raw().get())
                .ok_or_else(|| LuaError::RuntimeError("invalid UI callback token (zero)".into()))?;
            crate::lua::claim_pending_ui_callback(lua, &plugin_name, cb_id)?;

            let registry = lua
                .app_data_ref::<crate::lua::SuspendedCoroutineRegistry>()
                .ok_or_else(|| {
                    LuaError::RuntimeError("suspended coroutine registry not available".into())
                })?;

            let thread_key = lua.create_registry_value(thread)?;
            let mut suspended = registry.0.write();
            if suspended.contains_key(&cb_id) {
                return Err(LuaError::RuntimeError(format!(
                    "UI callback {} is already bound to a coroutine",
                    cb_id.get()
                )));
            }
            suspended.insert(
                cb_id,
                crate::lua::SuspendedCoroutine {
                    thread_key,
                    plugin_name,
                },
            );

            Ok(())
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
fn parse_color(value: LuaValue) -> LuaResult<String> {
    match value {
        LuaValue::String(s) => Ok(s.to_str()?.to_string()),
        LuaValue::Table(t) => {
            let r: u8 = t.get("r").or_else(|_| t.get(1))?;
            let g: u8 = t.get("g").or_else(|_| t.get(2))?;
            let b: u8 = t.get("b").or_else(|_| t.get(3))?;
            Ok(format!("#{r:02x}{g:02x}{b:02x}"))
        }
        other => Err(LuaError::FromLuaConversionError {
            from: other.type_name(),
            to: "Color".to_string(),
            message: Some("expected hex string or rgb table".into()),
        }),
    }
}

/// Parsed annotation from Lua before char-index resolution.
struct ParsedAnnotation {
    position: snapshots::Position,
    annot: helix_view::document::PluginAnnotation,
}

/// Parse a single annotation from a Lua table.
///
/// Expected shape:
/// ```lua
/// { line = 0, column = 0, text = "...", fg = "#rrggbb", bg = {r,g,b}, is_line = false }
/// ```
fn parse_annotation(table: &LuaTable) -> LuaResult<ParsedAnnotation> {
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

    Ok(ParsedAnnotation {
        position: snapshots::Position { line, column },
        annot: helix_view::document::PluginAnnotation {
            char_idx: 0, // resolved at apply time against document text
            text,
            style: None,
            fg,
            bg,
            offset,
            is_line,
            virt_line_idx: table.get::<Option<u16>>("virt_line_idx")?,
            dropped_text: table.get::<Option<String>>("dropped_text")?,
        },
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

/// Parse an event kind string into an `EventKind`.
///
/// Accepts the canonical underscore form (e.g. `"document_opened"`) as well as
/// the PascalCase constant name (e.g. `"DocumentOpened"`).
fn parse_event_kind(s: &str) -> std::result::Result<crate::contract::events::EventKind, ()> {
    use crate::contract::events::EventKind;
    // Try canonical underscore form first, then PascalCase.
    for &kind in EventKind::ALL {
        if kind.as_str() == s {
            return Ok(kind);
        }
    }
    // PascalCase fallback
    for &kind in EventKind::ALL {
        if snake_to_pascal(kind.as_str()) == s {
            return Ok(kind);
        }
    }
    Err(())
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
    fn parse_event_kind_pascal() {
        use crate::contract::events::EventKind;
        assert_eq!(
            parse_event_kind("DocumentOpened"),
            Ok(EventKind::DocumentOpened)
        );
        assert_eq!(parse_event_kind("KeyPressed"), Ok(EventKind::KeyPressed));
    }

    #[test]
    fn parse_event_kind_unknown() {
        assert!(parse_event_kind("nonexistent_event").is_err());
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
    fn documents_module_structure() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_documents_module(&lua, &helix_table).unwrap();

        let docs: LuaTable = helix_table.get("documents").unwrap();
        assert!(docs.contains_key("list").unwrap());
        assert!(docs.contains_key("open").unwrap());
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
        assert!(cmds.contains_key("execute").unwrap());
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
    fn pkg_module_registers_backend() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_pkg_module(&lua, &helix_table).unwrap();
        lua.globals().set("helix", helix_table).unwrap();

        lua.load(
            r#"
            helix.pkg.register_backend({
                name = "fixture",
                probe = function() return true end,
                resolve = function() return { version = "1" } end,
                install = function(_staging, _progress) return true end,
                remove = function() return true end,
                doctor = function() return true end,
            })
            "#,
        )
        .exec()
        .unwrap();

        let helix: LuaTable = lua.globals().get("helix").unwrap();
        let pkg: LuaTable = helix.get("pkg").unwrap();
        let backends: LuaTable = pkg.get("_backends").unwrap();
        assert!(backends.contains_key("fixture").unwrap());
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
        assert!(ui.contains_key("set_theme").unwrap());
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
