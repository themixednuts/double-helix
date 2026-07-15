# Plugins

Helix plugins are Lua 5.4 scripts loaded from plugin directories. The host API version is `1`.

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
api_version = 1
capabilities = ["query", "mutation", "ui", "events"]
```

`api_version` is exact. Loading is refused if it differs from the host contract or if a capability name is unknown. Capability names are `query`, `mutation`, `ui`, `panels`, `commands`, `keymaps`, `events`, `splits`, `tabs`, `floats`, `tasks`, `syntax`, `lsp`, `themes`, and `assistant`.

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

## Plugin Hosts

Plugins always run outside the editor process. When plugins are enabled, the editor starts and supervises its private `dhx --plugin-host` mode, using length-prefixed msgpack `helix_plugin::rpc::Frame` messages over stdin/stdout. A crash or protocol failure releases that host generation's editor resources and restarts the process with bounded exponential backoff.

The local host discovers plugins from `plugin_dirs`, or from the default plugin directories when the list is empty:

```toml
[plugins]
enabled = true
plugin_dirs = ["/home/me/.config/helix/plugins"]
max_memory = 268435456
max_instructions = 5000000
```

Additional hosts use the same protocol. For example, remote execution over SSH needs only a remote `dhx` binary:

```toml
[[plugins.hosts]]
name = "remote-box"
command = "ssh"
args = ["box", "dhx", "--plugin-host"]
plugin_dirs = ["/srv/helix/plugins"]
```

Every host uses the same typed contract, retained rendering, task completion, generation routing, and ownership cleanup. Contract types have one independent owner in `helix-plugin-api`; Rust consumers import that crate directly. The framed protocol and Lua runtime live in `helix-plugin`; editor conversions live separately in `helix-plugin-editor` and execute only on the editor thread. Foreground host requests use nonblocking admission, and generation cleanup returns through runtime ingress. A config refresh replaces hosts only when `[plugins]` changes. Plugins should check `helix.host.api_metadata().has_capability(name)` before using optional capability families.

Security follows the configured command. `command = "ssh"` grants the plugin host whatever access that SSH account has on the remote machine. Editor-side contract bridges enforce handle ownership, permissions, generation validity, and capability checks for every host.

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

Event kinds are `host_ready`, `document_opened`, `document_changed`, `document_saved`, `document_closed`, `selection_changed`, `mode_changed`, `view_focused`, `diagnostics_updated`, `key_pressed`, `assistant_thread_created`, `assistant_thread_closed`, `assistant_run_started`, `assistant_run_completed`, `assistant_message_received`, and `assistant_context_changed`.

`helix.commands`: `register(spec)`, `update(handle, spec)`, `remove(handle)`, `execute(name, args?)`. `CommandHandle` has `id()`, `update(spec)`, and `remove()`.

`helix.keymaps`: `register(definition)`, `update(handle, definition)`, `remove(handle)`.

`helix.registers`: `get(name)`, `set(name, values)`.

`helix.ui`: `notify(message, level?)`, `info(message)`, `warn(message)`, `error(message)`, `set_status(message)`, `prompt(message, default?)`, `confirm(message)`, `pick(items, prompt?)`, `panel(spec)`, `toggle_panel(handle)`, `focus_panel(handle)`, `resize_panel(handle, size)`, `panels()`, `get_theme()`, `set_theme(name)`, `terminal_size()`, `redraw()`.

`PanelHandle`: `id()`, `close()`, `toggle()`, `focus()`, `resize(size)`.

`helix.splits`: `split(direction, opts?)`, `focus_direction(direction)`, `swap(direction)`, `transpose(view?)`, `resize(opts)`, `tree()`, `list()`.

`helix.tabs`: `open(document, opts?)`, `close(index_or_opts?)`, `focus(index, view?)`, `next(view?)`, `previous(view?)`, `list(view?)`.

`helix.floats`: `create(opts)`, `close(handle)`, `list()`. `FloatHandle` has `id()`, `close()`, and `update(opts)`.

`helix.assistant`: `snapshot()`, `thread(thread)`, `entries(thread)`, `context(thread)`, `is_ready()`, `active_thread()`, `thread_count()`, `submit(thread_or_nil, text)`, `cancel(thread_or_nil)`.

`helix.lsp`: `get_clients()`, `_raw.call(document, method, params, options?)`.

`helix.syntax`: `_raw.query(document, query, options?)`.

`helix.layout`: `split_vertical(area, constraints)`, `split_horizontal(area, constraints)`, `center(area, width, height)`.

`helix.log`: `trace(message)`, `debug(message)`, `info(message)`, `warn(message)`, `error(message)`.

`helix.config()` returns the current plugin's config table or `nil`. `helix.async(fn, ...)` starts a coroutine from synchronous contexts such as event handlers.
