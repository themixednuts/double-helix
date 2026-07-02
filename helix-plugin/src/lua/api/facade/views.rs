use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "list",
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

    helix_table.set("views", m)?;
    Ok(())
}
