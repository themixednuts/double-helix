use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "focused_document",
        lua.create_function(|_lua, ()| {
            with_query_bridge(|bridge| Ok(bridge.focused_document().map(LuaDocumentHandle)))
        })?,
    )?;

    m.set(
        "focused_view",
        lua.create_function(|_lua, ()| {
            with_query_bridge(|bridge| Ok(bridge.focused_view().map(LuaViewHandle)))
        })?,
    )?;

    m.set(
        "mode",
        lua.create_function(|_lua, ()| {
            with_query_bridge(|bridge| {
                let snap = bridge.workspace_snapshot();
                Ok(mode_to_string(snap.mode))
            })
        })?,
    )?;

    m.set(
        "set_mode",
        lua.create_function(|_lua, mode_str: String| {
            let mode = parse_edit_mode(&mode_str)?;
            with_mutation_bridge(|mut bridge| {
                bridge
                    .set_mode(requests::SetModeRequest { mode })
                    .map_err(|e| LuaError::RuntimeError(e.to_string()))
            })
        })?,
    )?;

    m.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            let snap = with_query_bridge(|bridge| Ok(bridge.workspace_snapshot()))?;
            let table = lua.create_table()?;
            table.set(
                "focused_document",
                snap.focused_document.map(LuaDocumentHandle),
            )?;
            table.set("focused_view", snap.focused_view.map(LuaViewHandle))?;
            table.set("mode", mode_to_string(snap.mode))?;
            let docs = lua.create_table()?;
            for (i, h) in snap.documents.iter().enumerate() {
                docs.set(i + 1, LuaDocumentHandle(*h))?;
            }
            table.set("documents", docs)?;
            let views = lua.create_table()?;
            for (i, h) in snap.views.iter().enumerate() {
                views.set(i + 1, LuaViewHandle(*h))?;
            }
            table.set("views", views)?;
            Ok(table)
        })?,
    )?;

    m.set(
        "theme",
        lua.create_function(|lua, ()| {
            let snap = with_query_bridge(|bridge| Ok(bridge.theme_snapshot()))?;
            let table = lua.create_table()?;
            table.set("name", snap.name)?;
            if let Some(c) = snap.bg {
                table.set("bg", format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b))?;
            }
            if let Some(c) = snap.fg {
                table.set("fg", format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b))?;
            }
            Ok(table)
        })?,
    )?;

    m.set(
        "documents",
        lua.create_function(|_lua, ()| {
            with_query_bridge(|bridge| {
                Ok(bridge
                    .list_documents()
                    .into_iter()
                    .map(LuaDocumentHandle)
                    .collect::<Vec<_>>())
            })
        })?,
    )?;

    m.set(
        "views",
        lua.create_function(|_lua, ()| {
            with_query_bridge(|bridge| {
                Ok(bridge
                    .list_views()
                    .into_iter()
                    .map(LuaViewHandle)
                    .collect::<Vec<_>>())
            })
        })?,
    )?;

    m.set(
        "editor_config",
        lua.create_function(|lua, ()| {
            let config = with_editor(|editor| Ok(editor.config()))?;
            let table = lua.create_table()?;
            table.set("scrolloff", config.scrolloff)?;
            table.set("mouse", config.mouse)?;
            table.set("cursorline", config.cursorline)?;
            table.set("cursorcolumn", config.cursorcolumn)?;
            table.set("auto_format", config.auto_format)?;
            table.set("auto_completion", config.auto_completion)?;
            table.set("auto_info", config.auto_info)?;
            table.set(
                "line_number",
                match config.line_number {
                    helix_view::editor::LineNumber::Absolute => "absolute",
                    helix_view::editor::LineNumber::Relative => "relative",
                },
            )?;
            Ok(table)
        })?,
    )?;

    helix_table.set("workspace", m)?;
    Ok(())
}
