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
| `R` | Toggle write/review mode for the active thread |
| `a` / `A` | Accept the selected/all pending review changes |
| `x` / `X` | Reject the selected/all pending review changes |
| `G` / `End` | Jump back to the live tail |
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

Use `:assistant-permissions-reset` to clear stored rules.

## Review Mode

Assistant writes have two modes:

| Mode | Behavior |
| --- | --- |
| `write` | Writes land immediately. The panel shows an informational per-file diff card. |
| `review` | Writes are staged in an overlay. The agent's later reads see staged content, and you accept or reject the files from the panel. |

Toggle the active thread with `R` in message focus or `:assistant-toggle-review-mode`.

Review cards use unified diff coloring and expand/collapse with `Tab`. In review mode, use `a` to accept the selected file, `A` to accept all, `x` to reject the selected file, and `X` to reject all.

When an accepted file is open in a buffer, Helix applies the accepted text as a normal document transaction so undo history and language-server state stay in sync. Clean buffers are saved after the transaction. Dirty buffers receive the transaction but remain unsaved, and the status line notes that the accepted edit was not saved. Files that are not open fall back to direct filesystem writes.

## Scrolling

The assistant output follows new output only while the viewport is already at the bottom. Scrolling up pauses live follow. `G` or `End` returns to the newest output.

## Context

Attached context appears as chips above the input. Context can be attached with `:assistant-attach-file`, `:assistant-attach-diagnostics`, `:assistant-attach-diff`, and removed with `:assistant-detach-context`.

Type `@` in the assistant input to open inline context completion. The popup lists workspace files, open buffers, and fixed entries for `@selection`, `@diagnostics`, and `@diff`.

Filter by typing after `@`. Use `C-n`/`C-p` or `Up`/`Down` to move, `Enter` or `Tab` to insert the selected mention, and `Esc` to dismiss. Accepted mentions insert `@relative/path` or the fixed token and attach the matching context as a chip. Removing the mention token from the draft detaches that mention-owned context.

## Markdown

Agent messages render Markdown incrementally while streaming. Supported formatting includes headings, bold, italic, strikethrough, inline code, fenced code blocks with tree-sitter highlighting, ordered and unordered lists with nesting, links, blockquotes, and horizontal rules.

Tool calls render as collapsible cards. Running and successful tools stay collapsed by default; failed tools expand automatically. Expanded tool output uses diff-style colors for patch-like lines.
