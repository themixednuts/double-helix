use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;
    let raw = lua.create_table()?;

    m.set(
        "get_clients",
        lua.create_function(|lua, ()| {
            let clients = lua.create_table()?;
            with_query_bridge(lua, |host| {
                let servers = host.language_servers().map_err(contract_error)?;
                for (i, client) in servers.into_iter().enumerate() {
                    let c = lua.create_table()?;
                    c.set("name", client.name)?;
                    c.set("id", client.id)?;
                    clients.set(i + 1, c)?;
                }
                Ok(clients)
            })
        })?,
    )?;

    raw.set(
        "call",
        lua.create_function(
            |lua,
             (document, method, params, options): (
                LuaDocumentHandle,
                String,
                LuaValue,
                Option<LuaTable>,
            )| {
                if method.is_empty() {
                    return Err(LuaError::RuntimeError(
                        "LSP method must not be empty".into(),
                    ));
                }
                let server = options
                    .as_ref()
                    .map(|options| options.get::<Option<String>>("server"))
                    .transpose()?
                    .flatten();
                start_task(
                    lua,
                    crate::contract::PluginTaskRequest::LspCall(crate::contract::LspCallRequest {
                        document: document.0,
                        server,
                        method,
                        params: dynamic_value_from_lua(params)?,
                    }),
                )
            },
        )?,
    )?;
    m.set("_raw", raw)?;

    helix_table.set("lsp", m)?;
    Ok(())
}
