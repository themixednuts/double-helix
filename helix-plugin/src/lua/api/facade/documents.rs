use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "list",
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
        "open",
        lua.create_function(|_lua, (path, opts): (String, Option<LuaTable>)| {
            let focus = opts
                .and_then(|t| t.get::<Option<bool>>("focus").ok().flatten())
                .unwrap_or(false);
            let handle = with_mutation_bridge(|mut bridge| {
                bridge
                    .open_document(requests::OpenDocumentRequest { path, focus })
                    .map_err(|e| LuaError::RuntimeError(e.to_string()))
            })?;
            Ok(LuaDocumentHandle(handle))
        })?,
    )?;

    helix_table.set("documents", m)?;
    Ok(())
}
