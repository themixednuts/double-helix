use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "get_clients",
        lua.create_function(|lua, ()| {
            let clients = lua.create_table()?;
            with_editor(|editor| {
                for (i, (name, id)) in editor
                    .language_server_client_names()
                    .zip(editor.language_server_client_ids())
                    .enumerate()
                {
                    let c = lua.create_table()?;
                    c.set("name", name)?;
                    c.set("id", id)?;
                    clients.set(i + 1, c)?;
                }
                Ok(clients)
            })
        })?,
    )?;

    helix_table.set("lsp", m)?;
    Ok(())
}
