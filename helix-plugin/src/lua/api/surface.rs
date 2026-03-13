use mlua::prelude::*;

/// Convert a Lua table {x, y, width, height} to a Rect.
pub fn table_to_rect(t: &LuaTable) -> LuaResult<helix_view::graphics::Rect> {
    Ok(helix_view::graphics::Rect::new(
        t.get("x")?,
        t.get("y")?,
        t.get("width")?,
        t.get("height")?,
    ))
}

/// Convert a Rect to a Lua table.
pub fn rect_to_table(lua: &Lua, r: helix_view::graphics::Rect) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("x", r.x)?;
    t.set("y", r.y)?;
    t.set("width", r.width)?;
    t.set("height", r.height)?;
    Ok(t)
}

/// Lua userdata handle to the current render surface.
///
/// This is a unit struct — the actual surface lives in a thread-local set up
/// by `with_render_context`. Methods access it via `get_surface_mut()`.
pub struct LuaSurface;

impl LuaUserData for LuaSurface {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        // surface:set_string(x, y, text, style_scope)
        methods.add_method(
            "set_string",
            |_lua, _this, (x, y, text, scope): (u16, u16, String, String)| {
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.set_string(x, y, &text, style);
                Ok(())
            },
        );

        // surface:set_stringn(x, y, text, max_width, style_scope)
        methods.add_method(
            "set_stringn",
            |_lua, _this, (x, y, text, max_width, scope): (u16, u16, String, usize, String)| {
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.set_stringn(x, y, &text, max_width, style);
                Ok(())
            },
        );

        // surface:clear(area, style_scope)
        methods.add_method("clear", |_lua, _this, (area, scope): (LuaTable, String)| {
            let area = table_to_rect(&area)?;
            let style = crate::lua::resolve_style(&scope)?;
            let surface = crate::lua::get_surface_mut()?;
            surface.clear_with(area, style);
            Ok(())
        });

        // surface:set_style(area, style_scope)
        methods.add_method(
            "set_style",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.set_style(area, style);
                Ok(())
            },
        );

        // surface:header(area, title, style_scope)
        methods.add_method(
            "header",
            |_lua, _this, (area, title, scope): (LuaTable, String, String)| {
                let area = table_to_rect(&area)?;
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.header(area, &title, style);
                Ok(())
            },
        );

        // surface:header_with_counts(area, title, current, total, style_scope)
        methods.add_method("header_with_counts", |_lua, _this, (area, title, current, total, scope): (LuaTable, String, usize, usize, String)| {
            let area = table_to_rect(&area)?;
            let style = crate::lua::resolve_style(&scope)?;
            let surface = crate::lua::get_surface_mut()?;
            surface.header_with_counts(area, &title, current, total, style);
            Ok(())
        });

        // surface:hdivider(area, style_scope)
        methods.add_method(
            "hdivider",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.hdivider(area, style);
                Ok(())
            },
        );

        // surface:vdivider(area, style_scope)
        methods.add_method(
            "vdivider",
            |_lua, _this, (area, scope): (LuaTable, String)| {
                let area = table_to_rect(&area)?;
                let style = crate::lua::resolve_style(&scope)?;
                let surface = crate::lua::get_surface_mut()?;
                surface.vdivider(area, style);
                Ok(())
            },
        );

        // surface:text_input(area, text, cursor, style_scope, cursor_style_scope) -> {cursor_x, cursor_y}
        methods.add_method(
            "text_input",
            |lua,
             _this,
             (area, text, cursor, scope, cursor_scope): (
                LuaTable,
                String,
                usize,
                String,
                String,
            )| {
                let area = table_to_rect(&area)?;
                let style = crate::lua::resolve_style(&scope)?;
                let cursor_style = crate::lua::resolve_style(&cursor_scope)?;
                let surface = crate::lua::get_surface_mut()?;
                let (cx, cy) = surface.text_input(area, &text, cursor, style, cursor_style);
                let result = lua.create_table()?;
                result.set("cursor_x", cx)?;
                result.set("cursor_y", cy)?;
                Ok(result)
            },
        );

        // surface:scrollbar(area, {total, offset, visible, thumb_style, track_style})
        methods.add_method(
            "scrollbar",
            |_lua, _this, (area, opts): (LuaTable, LuaTable)| {
                let area = table_to_rect(&area)?;
                let total: usize = opts.get("total")?;
                let offset: usize = opts.get("offset")?;
                let visible: usize = opts.get("visible")?;
                let thumb_scope: String = opts.get("thumb_style")?;
                let track_scope: Option<String> = opts.get("track_style").ok();
                let thumb_style = crate::lua::resolve_style(&thumb_scope)?;
                let track_style = track_scope
                    .as_deref()
                    .map(crate::lua::resolve_style)
                    .transpose()?
                    .unwrap_or_default();
                let surface = crate::lua::get_surface_mut()?;
                surface.scrollbar(area, total, offset, visible, thumb_style, None, track_style);
                Ok(())
            },
        );
    }
}
