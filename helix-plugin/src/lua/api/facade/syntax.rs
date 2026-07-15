use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let module = lua.create_table()?;
    let raw = lua.create_table()?;
    raw.set(
        "query",
        lua.create_function(
            |lua, (document, query, options): (LuaDocumentHandle, String, Option<LuaTable>)| {
                let position = |name: &str| -> LuaResult<Option<snapshots::Position>> {
                    let Some(table) = options
                        .as_ref()
                        .and_then(|options| options.get::<Option<LuaTable>>(name).ok())
                        .flatten()
                    else {
                        return Ok(None);
                    };
                    Ok(Some(snapshots::Position {
                        line: table.get("line")?,
                        column: table.get("column")?,
                    }))
                };
                let request = crate::contract::SyntaxQueryRequest {
                    document: document.0,
                    query,
                    start: position("start")?,
                    end: position("end")?,
                    max_captures: options
                        .as_ref()
                        .and_then(|options| options.get::<Option<usize>>("max_captures").ok())
                        .flatten()
                        .unwrap_or(10_000),
                };
                start_task(
                    lua,
                    crate::contract::PluginTaskRequest::SyntaxQuery(request),
                )
            },
        )?,
    )?;
    module.set("_raw", raw)?;
    helix_table.set("syntax", module)?;
    Ok(())
}
