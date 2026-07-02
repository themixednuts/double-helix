use super::*;

fn parse_size(s: &str) -> LuaResult<helix_view::layout::Size> {
    use helix_view::layout::Size;
    if s == "fill" {
        return Ok(Size::Fill);
    }
    if let Some(n) = s.strip_prefix("fixed:") {
        let v: u16 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid fixed size: {s}")))?;
        return Ok(Size::fixed(v));
    }
    if let Some(n) = s.strip_prefix("percent:") {
        let v: u8 = n
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid percent size: {s}")))?;
        return Ok(Size::Percent(v));
    }
    if let Some(rest) = s.strip_prefix("constrained:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() != 2 {
            return Err(LuaError::RuntimeError(format!(
                "constrained size needs min:max, got: {s}"
            )));
        }
        let min: u16 = parts[0]
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid constrained min: {s}")))?;
        let max: u16 = parts[1]
            .parse()
            .map_err(|_| LuaError::RuntimeError(format!("invalid constrained max: {s}")))?;
        return Ok(Size::constrained(min, max));
    }
    if let Ok(v) = s.parse::<u16>() {
        return Ok(Size::fixed(v));
    }
    Err(LuaError::RuntimeError(format!(
        "unknown size format: {s}. Expected: \"fill\", \"fixed:N\", \"percent:N\", \"constrained:MIN:MAX\", or a number"
    )))
}

fn parse_sizes(list: &[String]) -> LuaResult<Vec<helix_view::layout::Size>> {
    list.iter().map(|s| parse_size(s)).collect()
}

pub fn register(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let m = lua.create_table()?;

    m.set(
        "split_vertical",
        lua.create_function(|lua, (area_table, sizes_list): (LuaTable, Vec<String>)| {
            let area = table_to_rect(&area_table)?;
            let sizes = parse_sizes(&sizes_list)?;
            let rects = helix_view::layout::split_vertical(area, &sizes);
            let result = lua.create_table()?;
            for (i, r) in rects.iter().enumerate() {
                result.set(i + 1, rect_to_table(lua, *r)?)?;
            }
            Ok(result)
        })?,
    )?;

    m.set(
        "split_horizontal",
        lua.create_function(|lua, (area_table, sizes_list): (LuaTable, Vec<String>)| {
            let area = table_to_rect(&area_table)?;
            let sizes = parse_sizes(&sizes_list)?;
            let rects = helix_view::layout::split_horizontal(area, &sizes);
            let result = lua.create_table()?;
            for (i, r) in rects.iter().enumerate() {
                result.set(i + 1, rect_to_table(lua, *r)?)?;
            }
            Ok(result)
        })?,
    )?;

    m.set(
        "center",
        lua.create_function(|lua, (area_table, width, height): (LuaTable, u16, u16)| {
            let area = table_to_rect(&area_table)?;
            let r = helix_view::layout::center(area, width, height);
            rect_to_table(lua, r)
        })?,
    )?;

    helix_table.set("layout", m)?;
    Ok(())
}
