# Assistant

Helix can run an ACP-compatible agent in the assistant panel. ACP is the Agent Client Protocol used by Zed: JSON-RPC over stdio, with one JSON message per line.

Public ACP agents are available through the package manager. Open
`:assistant-agents` to browse, install, update, remove, and connect ACP agents.
The regular package manager also supports this through `:pkg` with
`kind:acp`.

Use `config.toml` for local agents, custom commands, or overrides:

```toml
[[editor.agents]]
id = "claude"
name = "Claude"
command = "npm"
args = ["exec", "--yes", "@agentclientprotocol/claude-agent-acp@0.56.0"]
theme = "default"
```

`id` is an optional stable logical identifier used for assistant history and backend
connections. When omitted, `name` is used. Give agents explicit, distinct IDs when
they share a launcher such as `node`, `npm`, or `npx`, and keep an ID unchanged when
renaming its display name.

Assistant completion notifications are enabled by default:

```toml
[editor.assistant]
notify-on-done = true
```

Profiles are optional named bundles for starting or switching assistant sessions:

```toml
[[editor.assistant.profiles]]
name = "review"
agent = "Claude"
mode = "review"
model = "claude-opus-4"
config = { thinking = "high" }
mcp-servers = ["fs", "git"]
```

`agent` selects an installed ACP package agent or configured `[[editor.agents]]` entry by name. If omitted, Helix uses the current agent when switching an active thread, or the first available agent when starting from `:assistant-profile`. `mode`, `model`, and `config` are applied through the normal ACP session mode/config paths. `mcp-servers` filters the selected agent's configured MCP servers by their `name`.

Open or connect with `:assistant-open` and `:assistant-connect`. With no arguments, `:assistant-connect` shows installed ACP package agents and configured agents; if none are available, Helix opens `:assistant-agents` so you can install one. In the ACP agents manager, press `Enter` or `i` to install and `c` to connect an installed agent. With arguments, `:assistant-connect` starts that command directly. Browse previous sessions with `:assistant-open-history`; press `d` on a selected session and then `d` again to confirm deletion.

## Panel Keys

The assistant is a docked panel with two focus modes: Input and Messages. Press `?` in Messages (or the auth card) to show the active layer's keys in the standard info popup; form and auth layers show it automatically when `editor.auto-info` is enabled. The panel footer shows only the layer badge and message position — never key hints.

### Input

Input focus is the prompt box. It uses Helix modal editing through the assistant EditRegion.

| Key | Action |
| --- | --- |
| `Tab` / `Ctrl-j` | Move to Messages |
| `Enter` | Send in normal mode; insert a newline in insert mode |
| `@` | Start mention completion |
| `/` | Start slash-command completion |
| `Ctrl-o` | Open the standard mode/model/config picker |
| `Ctrl-c` | Cancel pending assistant work |
| `Esc` | Leave insert mode; in normal mode follows the editor focused-component convention |

### Messages

Messages focus is a single transcript list. Cards are entries in that list; they do not own focus.

| Key | Action |
| --- | --- |
| `j` / `k` | Move between entries |
| `gg` / `G` | Move to the first/newest entry |
| `Enter` | Primary action: enter a pending card transient, jump to a subagent target, or open the selected entry |
| `Tab` | Expand or collapse the selected entry; tool calls are collapsed by default |
| `y` | Yank a pending request URL; otherwise yank the selected entry |
| `t` | Follow output or jump to a selected subagent target |
| `e` | Edit the selected user prompt |
| `r` | Retry the last user prompt after a failed or canceled run |
| `R` | Toggle write/review mode for the active thread |
| `+` / `-` | Toggle a local up/down rating for the thread |
| `n` | Add, edit, or clear a local thread note |
| `a` / `A` | Accept the selected/all pending review changes |
| `x` / `X` | Reject the selected/all pending review changes |
| `Esc` | Return to Input |
| `Ctrl-c` | Cancel pending assistant work |
| `Ctrl-o` | Open the standard mode/model/config picker |
| `?` | Toggle the key-help info popup |

Editing is available only for user prompts. Press `e` on a selected user prompt to load that text into Input mode. The header shows `editing message · esc cancels`, the footer badge shows `EDIT`, and the target message remains highlighted in the transcript.

While editing, `Esc` cancels the edit, restores the draft that was in the input before editing, and returns to the previous focus without changing the thread. The normal send action resubmits the edit: Helix locally forks at that prompt by removing the edited prompt and every later entry, then sends the edited text as the replacement prompt.

When the ACP agent advertises `session/fork`, Helix forks the remote session before sending so the agent context matches the local transcript. If the agent does not support session forking, Helix still updates the local transcript and sends the edited text, but shows a status note that earlier agent context may be retained.

### Card Transients

Authentication method choice and elicitation form editing are transient layers entered from Messages.

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | Move between fields or methods |
| `h` / `l` / `Space` | Change select and boolean fields |
| `Enter` | Submit or confirm |
| `Esc` | Pop back to Messages |
| `Ctrl-c` | Cancel pending assistant work |

The panel header shows the active thread title, focus mode, current profile, current mode/model when the agent provides them, local rating/note state, compact token usage, and run state. The first token number is cumulative thread usage, and `last` is the most recent turn.

Ratings and notes are local history metadata only. They are persisted with the thread record and shown in `:assistant-open-history`, where the picker can be filtered by `up`, `down`, or `note`; they are not sent to the agent or any telemetry service.

## Permissions

When an agent asks for permission, Helix shows a standard picker with the request choices. The picker uses normal picker keys; selecting a row sends that choice to the agent.

Choices such as allow always or reject always are stored in the assistant permission rules file:

```text
<data-dir>/double-helix/state.sqlite3
```

The legacy `<cache-dir>/double-helix/assistant/permissions.toml` file is still
imported or used as a fallback if the SQLite store cannot be opened.

Future matching requests for the same agent and tool are answered automatically, and the thread shows a transient status such as `auto-allowed shell (always)` or `auto-rejected shell (always)`.

Use `:assistant-permissions-reset` to clear stored rules.

## Storage

Generated assistant state is stored in SQLite through `helix-store`:

```text
<data-dir>/double-helix/state.sqlite3
```

That database holds assistant threads, history metadata, layout, permissions,
and package receipts. The rebuildable file-picker frecency and query cache lives
in:

```text
<cache-dir>/double-helix/cache.sqlite3
```

Legacy assistant files under `<cache-dir>/double-helix/assistant/` are preserved
as one-release import or fallback sources. Log output remains a plain text log
file, defaulting to `<cache-dir>/double-helix/double-helix.log` unless `--log`
specifies another path.

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

Type `/` at the start of the input to open slash-command completion. Commands come from the active ACP session. Accepting a completion inserts the command text; unknown slash commands are still sent as normal prompt text.

## Agent Requests

ACP elicitations render as request cards in the thread. Form cards list text, textarea, select, and boolean fields with required markers. Press `Enter` from Messages to edit a form as a transient layer. URL cards show the URL; press `y` to yank it.

The selector opened with `Ctrl-o` lists configured profiles first, then session modes and config options such as model and thought level. Selecting a profile sets the active thread profile and applies its mode/config defaults through the active ACP session; pending values show in the header while the agent confirms them. `:assistant-profile` opens the profile picker directly and can start a profiled session when no assistant thread is active.

Thought entries render as dim `thinking...` blocks and are folded by default. `Tab` expands or collapses the selected thought like other foldable entries.

Agent-spawned terminals render as cards with a running/exited/failed status badge and the latest output tail.

Tool calls that refer to a subagent session show a subagent marker in the row and include the subagent session id in the entry details. Press `Enter` or `t` on that tool call to jump to the subagent session; when the backend supports session loading, unloaded subagent sessions are loaded first.

## Markdown

Agent messages render Markdown incrementally while streaming. Supported formatting includes headings, bold, italic, strikethrough, inline code, fenced code blocks with tree-sitter highlighting, ordered and unordered lists with nesting, links, blockquotes, and horizontal rules.

Tool calls render as collapsible cards. Running and successful tools stay collapsed by default; failed tools expand automatically. Expanded tool output uses diff-style colors for patch-like lines.
