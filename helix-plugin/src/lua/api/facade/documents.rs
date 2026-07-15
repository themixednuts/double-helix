use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;
    let raw = lua.create_table()?;

    m.set(
        "list",
        lua.create_function(|lua, ()| {
            with_query_bridge(lua, |bridge| {
                Ok(bridge
                    .list_documents()
                    .into_iter()
                    .map(LuaDocumentHandle)
                    .collect::<Vec<_>>())
            })
        })?,
    )?;

    raw.set(
        "open",
        lua.create_function(|lua, (path, opts): (String, Option<LuaTable>)| {
            let focus = opts
                .and_then(|t| t.get::<Option<bool>>("focus").ok().flatten())
                .unwrap_or(false);
            let request = requests::OpenDocumentRequest { path, focus };
            start_task(
                lua,
                crate::contract::PluginTaskRequest::OpenDocument(request),
            )
        })?,
    )?;
    m.set("_raw", raw)?;

    helix_table.set("documents", m)?;
    Ok(())
}
