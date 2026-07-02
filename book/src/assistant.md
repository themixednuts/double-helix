# Assistant

Helix can run an ACP-compatible agent in the assistant panel. ACP is the Agent Client Protocol used by Zed: JSON-RPC over stdio, with one JSON message per line.

Configure agents in `config.toml`:

```toml
[[editor.agents]]
name = "Claude"
command = "npx"
args = ["@zed-industries/claude-agent-acp"]
theme = "default"
```

Open or connect with `:assistant-open` and `:assistant-connect`. With no arguments, `:assistant-connect` shows configured agents. With arguments, it starts that command directly.

## Panel Keys

In message focus:

| Key | Action |
| --- | --- |
| `j` / `k` | Move between entries |
| `Enter` | Open the selected entry in a scratch buffer |
| `Tab` | Expand or collapse the selected entry; tool calls are collapsed by default |
| `y` | Yank the selected entry |
| `t` | Toggle follow mode |
| `Esc` | Interrupt a running agent; otherwise return to input |
| `Ctrl-c` | Interrupt a running agent |

In input focus, type the prompt and submit normally. `Esc` leaves insert mode.

## Permissions

When an agent asks for permission, Helix shows a popup with the tool name, request body, available choices, shortcut keys, and the default choice when the request provides one.

Choices such as allow always or reject always are stored in the assistant permission rules file:

```text
<cache-dir>/assistant/permissions.toml
```

Future matching requests for the same agent and tool are answered automatically, and the thread shows a transient status such as `auto-allowed shell (always)` or `auto-rejected shell (always)`.

Remove that file to clear stored rules.

## Markdown

Agent messages render Markdown incrementally while streaming. Supported formatting includes headings, bold, italic, strikethrough, inline code, fenced code blocks with tree-sitter highlighting, ordered and unordered lists with nesting, links, blockquotes, and horizontal rules.

Tool calls render as collapsible cards. Running and successful tools stay collapsed by default; failed tools expand automatically. Expanded tool output uses diff-style colors for patch-like lines.
