use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "info",
        lua.create_function(|_lua, message: String| {
            log::info!("[plugin] {}", message);
            Ok(())
        })?,
    )?;
    m.set(
        "warn",
        lua.create_function(|_lua, message: String| {
            log::warn!("[plugin] {}", message);
            Ok(())
        })?,
    )?;
    m.set(
        "error",
        lua.create_function(|_lua, message: String| {
            log::error!("[plugin] {}", message);
            Ok(())
        })?,
    )?;
    m.set(
        "debug",
        lua.create_function(|_lua, message: String| {
            log::debug!("[plugin] {}", message);
            Ok(())
        })?,
    )?;
    m.set(
        "trace",
        lua.create_function(|_lua, message: String| {
            log::trace!("[plugin] {}", message);
            Ok(())
        })?,
    )?;

    helix_table.set("log", m)?;
    Ok(())
}
