use crate::error::Result;
use mlua::prelude::*;

/// Register UI API in the Helix Lua global table
pub fn register_ui_api(lua: &Lua, helix_table: &LuaTable) -> Result<()> {
    let ui_module = lua.create_table()?;

    // helix.ui.notify(message, level) - Show notification
    let notify = lua.create_function(|_lua, (message, _level): (String, Option<String>)| {
        if let Ok(editor) = crate::lua::get_editor_mut() {
            editor.set_status(message);
        }
        Ok(())
    })?;
    ui_module.set("notify", notify)?;

    // helix.ui.info(message) - Show info message
    let info = lua.create_function(|_lua, message: String| {
        if let Ok(editor) = crate::lua::get_editor_mut() {
            editor.set_status(message);
        }
        Ok(())
    })?;
    ui_module.set("info", info)?;

    // helix.ui.warn(message) - Show warning message
    let warn = lua.create_function(|_lua, message: String| {
        if let Ok(editor) = crate::lua::get_editor_mut() {
            editor.set_status(format!("Warning: {}", message));
        }
        Ok(())
    })?;
    ui_module.set("warn", warn)?;

    // helix.ui.error(message) - Show error message
    let error = lua.create_function(|_lua, message: String| {
        if let Ok(editor) = crate::lua::get_editor_mut() {
            editor.set_error(message);
        }
        Ok(())
    })?;
    ui_module.set("error", error)?;

    // helix.ui.prompt(message, default, callback) - Show input prompt
    let prompt = lua.create_function(
        |lua, (message, default, callback): (String, Option<String>, LuaFunction)| {
            let editor = match crate::lua::get_editor_mut() {
                Ok(e) => e,
                Err(_) => return Ok(()),
            };
            let plugin_name = lua
                .globals()
                .get::<String>("_current_plugin_name")
                .unwrap_or_else(|_| "unknown".to_string());

            let handler = match lua.app_data_ref::<crate::types::UiHandlerWrapper>() {
                Some(h) => h,
                None => return Ok(()),
            };

            let callback_reg = match lua.app_data_ref::<crate::types::UiCallbackRegistry>() {
                Some(r) => r,
                None => return Ok(()),
            };

            let counter = match lua.app_data_ref::<crate::types::UiCallbackCounter>() {
                Some(c) => c,
                None => return Ok(()),
            };

            let callback_id = counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let callback_ref = lua.create_registry_value(callback)?;

            callback_reg
                .0
                .write()
                .insert((plugin_name.clone(), callback_id), callback_ref);

            handler
                .0
                .prompt(editor, message, default, plugin_name, callback_id);

            Ok(())
        },
    )?;
    ui_module.set("prompt", prompt)?;

    // helix.ui.confirm(message, callback) - Show confirmation dialog
    let confirm = lua.create_function(|lua, (message, callback): (String, LuaFunction)| {
        let editor = match crate::lua::get_editor_mut() {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let plugin_name = lua
            .globals()
            .get::<String>("_current_plugin_name")
            .unwrap_or_else(|_| "unknown".to_string());

        let handler = match lua.app_data_ref::<crate::types::UiHandlerWrapper>() {
            Some(h) => h,
            None => return Ok(()),
        };

        let callback_reg = match lua.app_data_ref::<crate::types::UiCallbackRegistry>() {
            Some(r) => r,
            None => return Ok(()),
        };

        let counter = match lua.app_data_ref::<crate::types::UiCallbackCounter>() {
            Some(c) => c,
            None => return Ok(()),
        };

        let callback_id = counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let callback_ref = lua.create_registry_value(callback)?;

        callback_reg
            .0
            .write()
            .insert((plugin_name.clone(), callback_id), callback_ref);

        handler.0.confirm(editor, message, plugin_name, callback_id);

        Ok(())
    })?;
    ui_module.set("confirm", confirm)?;

    // helix.ui.picker(options) - Show picker/menu
    let picker = lua.create_function(|lua, options: LuaTable| {
        let editor = match crate::lua::get_editor_mut() {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let plugin_name = lua
            .globals()
            .get::<String>("_current_plugin_name")
            .unwrap_or_else(|_| "unknown".to_string());

        let handler = match lua.app_data_ref::<crate::types::UiHandlerWrapper>() {
            Some(h) => h,
            None => return Ok(()),
        };

        let callback_reg = match lua.app_data_ref::<crate::types::UiCallbackRegistry>() {
            Some(r) => r,
            None => return Ok(()),
        };

        let counter = match lua.app_data_ref::<crate::types::UiCallbackCounter>() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Extract picker options
        let items: Vec<String> = options
            .get::<Option<Vec<String>>>("items")?
            .unwrap_or_default();
        let prompt_text: String = options
            .get::<Option<String>>("prompt")?
            .unwrap_or_else(|| "Select:".to_string());
        let callback: LuaFunction = options
            .get("on_select")
            .map_err(|_| LuaError::RuntimeError("on_select callback required".into()))?;

        let callback_id = counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let callback_ref = lua.create_registry_value(callback)?;

        callback_reg
            .0
            .write()
            .insert((plugin_name.clone(), callback_id), callback_ref);

        handler
            .0
            .picker(editor, items, prompt_text, plugin_name, callback_id);

        Ok(())
    })?;
    ui_module.set("picker", picker)?;

    // helix.ui.panel(opts) - Register a custom panel
    //
    // opts: { title, side?, width?, render, on_event? }
    // render(area, surface) is called each frame
    // on_event(event) is called for key events when panel is focused
    let panel_fn = lua.create_function(|lua, opts: LuaTable| {
        let title: String = opts.get("title")?;
        let side: String = opts
            .get::<Option<String>>("side")?
            .unwrap_or_else(|| "right".into());
        let width: u16 = opts.get::<Option<u16>>("width")?.unwrap_or(30);
        let render_fn: LuaFunction = opts.get("render")?;
        let event_fn: Option<LuaFunction> = opts.get("on_event").ok();

        let plugin_name = lua
            .globals()
            .get::<String>("_current_plugin_name")
            .unwrap_or_else(|_| "unknown".to_string());
        let panel_id = format!("{}:{}", plugin_name, title);

        let handler = match lua.app_data_ref::<crate::types::UiHandlerWrapper>() {
            Some(h) => h,
            None => return Ok(LuaNil),
        };
        let callback_reg = match lua.app_data_ref::<crate::types::UiCallbackRegistry>() {
            Some(r) => r,
            None => return Ok(LuaNil),
        };
        let counter = match lua.app_data_ref::<crate::types::UiCallbackCounter>() {
            Some(c) => c,
            None => return Ok(LuaNil),
        };

        // Store render callback
        let render_id = counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let render_ref = lua.create_registry_value(render_fn)?;
        callback_reg
            .0
            .write()
            .insert((plugin_name.clone(), render_id), render_ref);

        // Store event callback (optional)
        let event_id = if let Some(ef) = event_fn {
            let eid = counter.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let event_ref = lua.create_registry_value(ef)?;
            callback_reg
                .0
                .write()
                .insert((plugin_name.clone(), eid), event_ref);
            Some(eid)
        } else {
            None
        };

        let editor = crate::lua::get_editor_mut()?;
        handler.0.register_panel(
            editor,
            plugin_name.clone(),
            panel_id.clone(),
            title,
            side,
            width,
            render_id,
            event_id,
        );

        // Return a handle table so the plugin can close the panel later
        let pn = plugin_name.clone();
        let pid = panel_id.clone();
        let handle = lua.create_table()?;
        let close_fn = lua.create_function(move |_lua, ()| {
            if let Ok(editor) = crate::lua::get_editor_mut() {
                if let Some(handler) = _lua.app_data_ref::<crate::types::UiHandlerWrapper>() {
                    handler.0.remove_panel(editor, pn.clone(), pid.clone());
                }
            }
            Ok(())
        })?;
        handle.set("close", close_fn)?;
        handle.set("id", panel_id)?;
        Ok(LuaValue::Table(handle))
    })?;
    ui_module.set("panel", panel_fn)?;

    // helix.ui.menu(items, callback) - Show menu
    let menu = lua.create_function(
        |_lua, (items, _callback): (Vec<String>, Option<LuaFunction>)| {
            // TODO: Implement actual menu
            Ok(format!("Would show menu with {} items", items.len()))
        },
    )?;
    ui_module.set("menu", menu)?;

    // helix.ui.set_status(message) - Set status line message
    let set_status = lua.create_function(|_lua, message: String| {
        let editor = crate::lua::get_editor_mut()?;
        editor.set_status(message);
        Ok(())
    })?;
    ui_module.set("set_status", set_status)?;

    // helix.ui.get_theme() - Get current theme name
    let get_theme = lua.create_function(|_lua, ()| {
        let editor = crate::lua::get_editor_mut()?;
        Ok(editor.theme.name().to_string())
    })?;
    ui_module.set("get_theme", get_theme)?;

    // helix.ui.set_theme(name) - Set theme
    let set_theme = lua.create_function(|_lua, name: String| {
        let editor = crate::lua::get_editor_mut()?;
        match editor.theme_loader.load(&name) {
            Ok(theme) => {
                editor.set_theme(theme);
                Ok(())
            }
            Err(e) => Err(LuaError::RuntimeError(format!(
                "Failed to load theme {}: {}",
                name, e
            ))),
        }
    })?;
    ui_module.set("set_theme", set_theme)?;

    // helix.ui.get_terminal_size() - Get terminal dimensions
    let get_terminal_size = lua.create_function(|_lua, ()| {
        let editor = crate::lua::get_editor_mut()?;
        let area = editor.tree.area();
        let size = _lua.create_table()?;
        size.set("width", area.width)?;
        size.set("height", area.height)?;
        Ok(size)
    })?;
    ui_module.set("get_terminal_size", get_terminal_size)?;

    // helix.ui.redraw() - Force redraw
    let redraw = lua.create_function(|_lua, ()| {
        let editor = crate::lua::get_editor_mut()?;
        editor.request_redraw();
        Ok(())
    })?;
    ui_module.set("redraw", redraw)?;

    // helix.ui.show_help(topic) - Show help
    let show_help = lua.create_function(|_lua, topic: Option<String>| {
        // TODO: Implement actual help display
        Ok(format!("Would show help for: {:?}", topic))
    })?;
    ui_module.set("show_help", show_help)?;

    helix_table.set("ui", ui_module)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_api_registration() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();

        let result = register_ui_api(&lua, &helix_table);
        assert!(result.is_ok());

        // Verify ui module exists with expected functions
        let ui_module: LuaTable = helix_table.get("ui").unwrap();
        assert!(ui_module.contains_key("notify").unwrap());
        assert!(ui_module.contains_key("info").unwrap());
        assert!(ui_module.contains_key("warn").unwrap());
        assert!(ui_module.contains_key("error").unwrap());
        assert!(ui_module.contains_key("prompt").unwrap());
        assert!(ui_module.contains_key("picker").unwrap());
        assert!(ui_module.contains_key("menu").unwrap());
    }

    #[test]
    fn test_ui_notification_functions() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_ui_api(&lua, &helix_table).unwrap();

        // Test notification functions
        let code = r#"
            helix.ui.info("Test message")
            helix.ui.warn("Warning!")
            helix.ui.error("Error!")
        "#;

        lua.globals().set("helix", helix_table).unwrap();
        lua.load(code).exec().unwrap();
    }

    #[test]
    fn test_ui_picker() {
        let lua = Lua::new();
        let helix_table = lua.create_table().unwrap();
        register_ui_api(&lua, &helix_table).unwrap();

        // Test picker with options
        let code = r#"
            local result = helix.ui.picker({
                items = {"item1", "item2", "item3"},
                prompt = "Choose:",
                on_select = function(item)
                    print("Selected: " .. item)
                end
            })
            assert(result == nil)
        "#;

        lua.globals().set("helix", helix_table).unwrap();
        lua.load(code).exec().unwrap();
    }
}
