use super::*;

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "api_metadata",
        lua.create_function(|lua, ()| {
            let meta = lua
                .app_data_ref::<crate::lua::HostApiMetadata>()
                .ok_or_else(|| LuaError::RuntimeError("host metadata unavailable".into()))?
                .0
                .clone();

            let table = lua.create_table()?;
            table.set("version", meta.version)?;

            let caps = lua.create_table()?;
            for (i, cap) in meta.capabilities.iter().enumerate() {
                caps.set(i + 1, cap.as_str())?;
            }
            table.set("capabilities", caps)?;

            let caps_clone: Vec<String> = meta
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect();
            table.set(
                "has_capability",
                lua.create_function(move |_lua, (_self_table, name): (LuaTable, String)| {
                    Ok(caps_clone.contains(&name.to_lowercase()))
                })?,
            )?;

            let catalog = lua.create_table()?;
            for (i, info) in meta.event_catalog.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("kind", info.kind.as_str())?;
                entry.set("description", info.description.as_str())?;
                catalog.set(i + 1, entry)?;
            }
            table.set("event_catalog", catalog)?;

            Ok(table)
        })?,
    )?;

    helix_table.set("host", m)?;
    Ok(())
}
