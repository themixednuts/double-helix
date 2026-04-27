# Helix Lua Plugin System

A Lua-based plugin system for the Helix text editor, enabling users to extend functionality through custom scripts.

## Features

- **Event-driven architecture**: React to editor events (document open, save, mode change, etc.)
- **Handle-centric API**: Operate on document and view handles with method syntax
- **Coroutine-based async**: UI prompts/pickers suspend with `coroutine.yield`, no callbacks
- **Custom commands**: Register new commands accessible from the command palette
- **UI integration**: Panels, prompts, pickers, notifications
- **Host-agnostic contract**: Serializable types for future WASM/RPC transport
- **Safe sandboxing**: Plugins run in a sandboxed Lua 5.4 environment

## Quick Start

### 1. Enable Plugins

Add to your `~/.config/helix/config.toml`:

```toml
[plugins]
enabled = true
plugin-dir = "~/.config/helix/plugins"
```

### 2. Create Your First Plugin

```
~/.config/helix/plugins/
  my-plugin/
    plugin.toml    # Metadata (optional)
    init.lua       # Entry point (required)
```

**plugin.toml**:
```toml
name = "my-plugin"
version = "0.1.0"
description = "My first Helix plugin"
author = "Your Name"
```

**init.lua**:
```lua
-- Log when documents are opened
helix.events.subscribe("document_opened", function(event)
    helix.log.info("Opened: " .. (event.path or "untitled"))
end)

-- Register a custom command (runs as a coroutine, so it can yield)
helix.commands.register({
    name = "greet",
    doc = "Ask for a name and greet",
    handler = function()
        local name = helix.ui.prompt("Your name:")
        if name then
            helix.ui.info("Hello, " .. name .. "!")
        end
    end,
})
```

### 3. Restart Helix

Plugins are loaded on startup. Your plugin should now be active!

## API Reference

### `helix.workspace` — Workspace queries and state

```lua
local doc = helix.workspace.focused_document()  -- DocumentHandle or nil
local view = helix.workspace.focused_view()      -- ViewHandle or nil
local mode = helix.workspace.mode()              -- "normal" | "insert" | "select"
helix.workspace.set_mode("insert")               -- switch mode
local docs = helix.workspace.documents()          -- [DocumentHandle]
local views = helix.workspace.views()             -- [ViewHandle]
local snap = helix.workspace.snapshot()           -- full workspace snapshot table
local theme = helix.workspace.theme()             -- { name, bg?, fg? }
local cfg = helix.workspace.editor_config()       -- { scrolloff, mouse, ... }
```

### DocumentHandle methods

```lua
local doc = helix.workspace.focused_document()

-- Queries
local snap = doc:snapshot()     -- { path, language, is_modified, line_count, selections, ... }
local text = doc:text()         -- full text as string
local line = doc:line(0)        -- 0-based line
local diags = doc:diagnostics() -- { diagnostics = [...] }

-- Mutations
doc:edit({
    { start = {line=0, column=0}, finish = {line=0, column=0}, text = "hello" },
})
doc:save()                          -- save (no-op if unmodified)
doc:save({ force = true })          -- force save
doc:set_selections({
    { anchor = {line=0, column=0}, head = {line=0, column=5} },
})
doc:undo()                          -- returns true if undo succeeded
doc:redo()                          -- returns true if redo succeeded
doc:select_all()

-- Per-plugin virtual text annotations
doc:set_annotations({
    { line = 0, column = 0, text = " <- generated", fg = "#6f8f3d" },
    { line = 4, text = "Review this block", bg = { r = 40, g = 30, b = 20 }, is_line = true },
})
doc:clear_annotations()
```

### ViewHandle methods

```lua
local view = helix.workspace.focused_view()

local snap = view:snapshot()    -- { handle, document, cursor, viewport }
local pos = view:cursor()       -- { line, column }
view:focus()                    -- focus this view
view:close()                    -- close this view
```

### `helix.documents` — Document listing and opening

```lua
local docs = helix.documents.list()                     -- [DocumentHandle]
local doc = helix.documents.open("path/to/file.rs")     -- open, don't focus
local doc = helix.documents.open("file.rs", { focus = true })
```

### `helix.events` — Event subscription

```lua
local subscription = helix.events.subscribe("document_opened", function(event)
    -- event.document (DocumentHandle), event.path, event.language
end)

helix.events.unsubscribe(subscription)

-- Available event kinds (also as constants on helix.events.kind):
-- document_opened, document_changed, document_pre_save, document_saved,
-- document_closed, selection_changed, mode_changed, view_focused,
-- diagnostics_updated, lsp_attached, key_pressed, split_created,
-- split_closed, tab_opened, tab_closed, tab_focused, float_created,
-- float_closed, panel_toggled, assistant_thread_created,
-- assistant_thread_closed, assistant_run_started, assistant_run_completed,
-- assistant_message_received, assistant_context_changed, host_ready
```

### `helix.commands` — Register and execute commands

```lua
-- Register a plugin command (handler runs as a coroutine)
local command = helix.commands.register({
    name = "my_command",
    doc = "Does something useful",
    handler = function()
        -- Can use helix.ui.prompt(), helix.ui.confirm(), helix.ui.pick() here
        helix.ui.info("Done!")
    end,
})

-- Update/remove by typed CommandHandle
command:update({ doc = "Does something more useful" })
helix.commands.remove(command)

-- Execute a built-in editor command
helix.commands.execute("write")
helix.commands.execute("open", { "path/to/file.rs" })
```

### `helix.registers` — Read/write editor registers

```lua
local values = helix.registers.get("a")       -- [string]
helix.registers.set("a", { "hello", "world" })
```

### `helix.ui` — UI operations

```lua
-- Fire-and-forget notifications
helix.ui.notify("message")
helix.ui.notify("message", "error")   -- "info" | "warn" | "error"
helix.ui.info("info message")
helix.ui.warn("warning message")
helix.ui.error("error message")
helix.ui.set_status("status line text")

-- Coroutine-yielding (must be called from command handler or helix.async)
local answer = helix.ui.prompt("Enter name:", "default")  -- yields, returns string or nil
local yes = helix.ui.confirm("Are you sure?")             -- yields, returns bool
local item = helix.ui.pick({"a", "b", "c"}, "Choose:")   -- yields, returns string or nil

-- Panels
local panel = helix.ui.panel({
    title = "My Panel",
    side = "right",    -- "left" | "right"
    width = 30,
    render = function(surface, area)
        surface:set_string(area.x, area.y, "Hello", "ui.text")
    end,
    on_event = function(event) end,  -- optional
})
panel:focus()
panel:resize("fixed:40")    -- also "percent:30"
panel:toggle()
panel:close()

for _, entry in ipairs(helix.ui.panels()) do
    entry.handle:focus()    -- PanelHandle
end

-- Theme
local name = helix.ui.get_theme()
helix.ui.set_theme("gruvbox")

-- Terminal
local size = helix.ui.terminal_size()  -- { width, height }
helix.ui.redraw()
```

### `helix.splits` - View topology

```lua
local view = helix.workspace.focused_view()
local doc = helix.workspace.focused_document()

local right = helix.splits.split("right", { view = view, document = doc })
helix.splits.resize({ view = right, dimension = "width", amount = "grow:10" })
helix.splits.transpose(right)
helix.splits.focus_direction("left")

local tree = helix.splits.tree()
local views = helix.splits.list()
```

### `helix.tabs` - Per-view tab groups

Tab indexes are 0-based.

```lua
local view = helix.workspace.focused_view()
local doc = helix.workspace.focused_document()

helix.tabs.open(doc, { view = view, focus = true })
helix.tabs.focus(0, view)
helix.tabs.next(view)
helix.tabs.previous(view)

local tabs = helix.tabs.list(view)  -- { tabs = [...], active = index }
helix.tabs.close({ view = view, index = 0 })
```

### `helix.floats` - Floating windows

```lua
local float = helix.floats.create({
    title = "Preview",
    placement = { type = "centered", width = 60, height = 12 },
    content = {
        { text = "Hello from a float", style = "ui.text" },
    },
    focus = true,
    dismissible = true,
})

float:update({
    title = "Preview (updated)",
    placement = { type = "absolute", x = 4, y = 2, width = 50, height = 10 },
})
float:close()

for _, entry in ipairs(helix.floats.list()) do
    entry.handle:close()    -- FloatHandle
end
```

### `helix.assistant` - Assistant threads

```lua
local thread = helix.assistant.active_thread()  -- ThreadHandle or nil
if thread then
    local snap = helix.assistant.thread(thread)
    local entries = helix.assistant.entries(thread)
    local context = helix.assistant.context(thread)
    helix.assistant.submit(thread, "Continue from here")
else
    helix.assistant.submit(nil, "Start a new assistant request")
end

helix.assistant.cancel(thread)  -- nil cancels the active thread
```

### `helix.async(fn, ...)` — Launch a coroutine

```lua
-- Use from event handlers (which are synchronous) to call yielding APIs
helix.events.subscribe("document_opened", function(event)
    helix.async(function()
        local confirm = helix.ui.confirm("Format this file?")
        if confirm then
            helix.commands.execute("format")
        end
    end)
end)
```

### `helix.config()` — Per-plugin configuration

```lua
local cfg = helix.config()  -- returns table from config.toml or nil
if cfg then
    local delay = cfg.delay or 1000
end
```

### `helix.log` — Logging

```lua
helix.log.info("message")
helix.log.warn("message")
helix.log.error("message")
helix.log.debug("message")
helix.log.trace("message")
```

### `helix.lsp` — LSP queries

```lua
local clients = helix.lsp.get_clients()  -- [{ name, id }]
```

### `helix.layout` — Layout combinators

```lua
local rects = helix.layout.split_vertical(area, { "fill", "fixed:30" })
local rects = helix.layout.split_horizontal(area, { "percent:50", "fill" })
local rect = helix.layout.center(area, 40, 10)
```

## Configuration

### Global

```toml
[plugins]
enabled = true
plugin-dir = "~/.config/helix/plugins"

[[plugins.plugin]]
name = "auto-save"
enabled = true

[plugins.plugin.config]
delay = 1000
auto_format = true
```

### Accessing in Lua

```lua
local cfg = helix.config()
if cfg then
    local delay = cfg.delay or 1000
end
```

## Security

Plugins run in a sandboxed Lua environment:

- **Disabled**: `os.execute`, `os.exit`, `io.*`, `loadfile`, `dofile`
- **No network access** (currently)

## Development

```bash
cargo build --release          # Build
cargo test -p helix-plugin     # Test
RUST_LOG=helix_plugin=debug hx # Debug logging
```

## License

Licensed under the Mozilla Public License 2.0. See [LICENSE](../LICENSE) for details.
