# Plugins

Helix plugins are Lua 5.4 scripts loaded from plugin directories. The host API version is `2`.

## Layout

```text
plugins/
  my-plugin/
    plugin.toml
    init.lua
    helper.lua
```

`init.lua` is the entry point unless `entry` is set in `plugin.toml`.

```toml
name = "my-plugin"
version = "0.1.0"
description = "Optional"
author = "Optional"
entry = "init.lua"
min_api_version = 2
capabilities = ["query", "mutation", "ui", "events"]
```

`min_api_version` and `capabilities` are optional. Loading is refused if `min_api_version` is greater than the host API version or if a capability name is unknown. Capability names are `query`, `mutation`, `ui`, `panels`, `commands`, `events`, `render`, `splits`, `tabs`, and `floats`.

## Errors

Host contract failures use stable machine codes: `not_found`, `stale_handle`, `invalid_request`, `permission_denied`, `unsupported_capability`, `busy`, and `internal_error`. When caught with `pcall`, wrapped Helix API functions return a table:

```lua
local ok, err = pcall(function()
  helix.tabs.list(stale_view)
end)

if not ok and type(err) == "table" then
  helix.log.warn(err.code .. ": " .. err.message)
end
```

The table fields are `code`, `message`, and optional `entity`. Treat `code` as stable and `message` as diagnostic.

## Remote Hosts

Plugins can run out of process in `helix-plugin-host`. The editor and child process communicate over stdout/stdin using length-prefixed msgpack `helix_plugin::rpc::Frame` messages that carry the same contract as the in-process Lua facade. The child process discovers plugins on its own filesystem.

```toml
[[plugins.hosts]]
name = "local"
command = "helix-plugin-host"
plugin_dirs = ["/home/me/.config/helix/plugins"]
```

Remote execution over SSH uses the same host binary and needs no SSH-specific Helix code:

```toml
[[plugins.hosts]]
name = "remote-box"
command = "ssh"
args = ["box", "helix-plugin-host"]
plugin_dirs = ["/srv/helix/plugins"]
```

Remote mode supports contract calls that can be represented as synchronous query/mutation requests. Capabilities that depend on editor-local rendering or callbacks may be reported as unsupported by a remote host until that transport path is implemented. Plugins should check `helix.host.api_metadata().has_capability(name)` before using optional capability families.

Security follows the configured command. `command = "ssh"` grants the plugin host whatever access that SSH account has on the remote machine. Editor-side contract bridges still enforce the same handle ownership, permissions, and capability checks as local plugins.

## Sandbox

The Lua sandbox removes `os.execute`, `os.exit`, `io`, `package`, `load`, `loadstring`, `loadfile`, and `dofile`. `require(name)` is scoped to the current plugin directory only. Module names cannot be absolute, contain path separators, contain `:`, or contain `..`.

Default limits are `max_memory = 268435456` bytes and `max_instructions = 5000000` VM instructions per plugin dispatch. Setting either value to `0` disables that limit.

## API

`helix.workspace`: `focused_document()`, `focused_view()`, `mode()`, `set_mode(mode)`, `documents()`, `views()`, `snapshot()`, `theme()`, `editor_config()`.

`DocumentHandle`: `id()`, `snapshot()`, `text()`, `line(index)`, `diagnostics()`, `edit(edits)`, `save(opts?)`, `set_selections(selections, view?)`, `undo()`, `redo()`, `select_all()`, `set_annotations(annotations)`, `clear_annotations()`.

`ViewHandle`: `id()`, `snapshot()`, `cursor()`, `focus()`, `close()`.

`helix.documents`: `list()`, `open(path, opts?)`.

`helix.views`: `list()`.

`helix.host`: `api_metadata()`.

`helix.events`: `kind`, `subscribe(kind, handler)`, `unsubscribe(handle)`.

Event kinds are `host_ready`, `document_opened`, `document_changed`, `document_pre_save`, `document_saved`, `document_closed`, `selection_changed`, `mode_changed`, `view_focused`, `diagnostics_updated`, `lsp_attached`, `key_pressed`, `split_created`, `split_closed`, `tab_opened`, `tab_closed`, `tab_focused`, `float_created`, `float_closed`, `panel_toggled`, `assistant_thread_created`, `assistant_thread_closed`, `assistant_run_started`, `assistant_run_completed`, `assistant_message_received`, and `assistant_context_changed`.

`helix.commands`: `register(spec)`, `update(handle, spec)`, `remove(handle)`, `execute(name, args?)`. `CommandHandle` has `id()`, `update(spec)`, and `remove()`.

`helix.registers`: `get(name)`, `set(name, values)`.

`helix.ui`: `notify(message, level?)`, `info(message)`, `warn(message)`, `error(message)`, `set_status(message)`, `prompt(message, default?)`, `confirm(message)`, `pick(items, prompt?)`, `panel(spec)`, `toggle_panel(handle)`, `focus_panel(handle)`, `resize_panel(handle, size)`, `panels()`, `get_theme()`, `set_theme(name)`, `terminal_size()`, `redraw()`.

`PanelHandle`: `id()`, `close()`, `toggle()`, `focus()`, `resize(size)`.

`helix.splits`: `split(direction, opts?)`, `focus_direction(direction)`, `swap(direction)`, `transpose(view?)`, `resize(opts)`, `tree()`, `list()`.

`helix.tabs`: `open(document, opts?)`, `close(index_or_opts?)`, `focus(index, view?)`, `next(view?)`, `previous(view?)`, `list(view?)`.

`helix.floats`: `create(opts)`, `close(handle)`, `list()`. `FloatHandle` has `id()`, `close()`, and `update(opts)`.

`helix.assistant`: `snapshot()`, `thread(thread)`, `entries(thread)`, `context(thread)`, `is_ready()`, `active_thread()`, `thread_count()`, `submit(thread_or_nil, text)`, `cancel(thread_or_nil)`.

`helix.lsp`: `get_clients()`.

`helix.layout`: `split_vertical(area, constraints)`, `split_horizontal(area, constraints)`, `center(area, width, height)`.

`helix.log`: `trace(message)`, `debug(message)`, `info(message)`, `warn(message)`, `error(message)`.

`helix.config()` returns the current plugin's config table or `nil`. `helix.async(fn, ...)` starts a coroutine from synchronous contexts such as event handlers.
