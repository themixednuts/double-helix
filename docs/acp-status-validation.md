# ACP status line validation (claude-acp)

How to validate what the claude-acp CLI bridge returns for the agent panel
status line (agent, thinking, mode, etc.).

## Testing with mock agent

To test the ACP flow without API keys or network, use the mock echo agent.
Add to your `config.toml` (e.g. `~/.config/helix/config.toml` or project root):

```toml
[[editor.agents]]
name = "Mock Echo"
command = "node"
args = ["E:/helix/helix-fork/scripts/mock-acp-agent.js"]
# Use the absolute path to the script on your machine
```

Then run `hx`, press **Space A c** to connect, choose "Mock Echo", **Space A i** to
chat. Try these inputs:

**Mode (S-tab)** cycles test cases: echo, long, big, slow, error, refusal, max_tokens, plan, tool, config, mode. The selected mode determines behavior when you send a prompt. Or type a command prefix to override.

| Input | Response |
|-------|----------|
| `hello` | Uses current mode (e.g. echo if mode=echo) |
| `long hello` | Override: longer response, word chunks |
| `big test` | Override: large body, word chunks |
| `slow demo` | Override: streaming with delay (press x to cancel) |
| `error x` | JSON-RPC internal error |
| `refusal` | stopReason: refusal |
| `max_tokens` | stopReason: max_tokens |
| `plan foo` | Plan entries, then echo |
| `tool bar` | tool_call + tool_call_update, then echo |
| `config` | config_option_update (agent pushes new option) |
| `mode` | current_mode_update (agent switches mode) |
| `help` | List all commands |

The mock agent echoes via `session/update` notifications. If you see the response,
the prompt flow and streaming work.

**Debug logging:** `RUST_LOG=info hx` (or `helix`) shows transport messages.
Use `RUST_LOG=debug` for more detail.

## Commands to run claude-acp

**Windows (npm.cmd):**

```bash
cargo run -p helix-acp --example call_session_new -- npm.cmd exec --yes @zed-industries/claude-agent-acp@0.20.2
```

**With CLI installed globally:**

```bash
cargo run -p helix-acp --example call_session_new -- claude-agent-acp
```

**Required env:** `ANTHROPIC_API_KEY` must be set.

---

## What claude-acp returns (validated)

From `session/new`:

```json
{
  "sessionId": "2e0d075f-8595-4699-aaf9-57c1e9aa0a26",
  "configOptions": [
    {
      "id": "mode",
      "name": "Mode",
      "category": "mode",
      "type": "select",
      "currentValue": "default",
      "options": [
        { "value": "default", "name": "Default", "description": "..." },
        {
          "value": "acceptEdits",
          "name": "Accept Edits",
          "description": "..."
        },
        { "value": "plan", "name": "Plan Mode", "description": "..." },
        { "value": "dontAsk", "name": "Don't Ask", "description": "..." },
        {
          "value": "bypassPermissions",
          "name": "Bypass Permissions",
          "description": "..."
        }
      ]
    },
    {
      "id": "model",
      "name": "Model",
      "category": "model",
      "type": "select",
      "currentValue": "default",
      "options": [
        {
          "value": "default",
          "name": "Default (recommended)",
          "description": "Opus 4.6 · Most capable..."
        },
        {
          "value": "sonnet",
          "name": "Sonnet",
          "description": "Sonnet 4.6 · Best for everyday..."
        },
        {
          "value": "haiku",
          "name": "Haiku",
          "description": "Haiku 4.5 · Fastest..."
        }
      ]
    }
  ]
}
```

**Notes:**

- No `session_modes` – claude-acp uses `config_options` with `category: "mode"`
  instead.
- No `thinking` in session/new – may appear in later `session/update`
  notifications.
- Status line shows: `mode | model | thinking` when available.

---

## Helix handling

- **model:** `status_config_name("model")` – display name (e.g. "Sonnet", "Default (recommended)").
- **thinking:** `status_config_display("thinking")` – used when present.
- **mode:** `current_mode_name()` – uses `session_modes` when present, else
  `config_options` with `category: "mode"` (claude-acp).

### ACP status line

Shows **mode** (with background) and **model** with a gap between (no separator).
Theme scopes: ui.acp.mode, ui.acp.model. No agent name, thinking, or key hints
in the status bar.

### Auto-connect on open

When opening the ACP panel (Space A f, Space A i, or :acp-open) with no agent
connected, Helix auto-connects to the last used agent (`current_acp_agent_index`).
If never connected this session, uses the first configured agent.

### ACP key bindings (Space A)

All ACP actions are under **Space A**:

- **f** / **a**: focus panel
- **c**: connect agent
- **o**: open panel
- **x**: cancel
- **h**: history
- **i**: chat (focus + activate input)
- **q**: close panel
- **S-tab**: cycle mode
- **C-t**: cycle thinking
- **C-m**: cycle model

Cycle keys are configurable in `[editor.acp]` (cycle-mode, cycle-thinking,
cycle-model). When the panel is focused, those keys work
directly; from Space A use the defaults above.

Cycling uses optimistic updates: the status line updates immediately. If the
agent request fails (e.g. network error, no session), the UI reverts to the
previous value and shows an error.

### Agent-specific themes

Each agent can have a theme that applies when connected:

```toml
[[editor.agents]]
name = "Claude Agent"
command = "npm.cmd"
args = ["exec", "--yes", "@zed-industries/claude-agent-acp@0.20.2"]
theme = "claude"   # optional: theme name to apply when this agent is connected

[[editor.agents]]
name = "Gemini CLI"
command = "gemini"
args = ["--experimental-acp"]
theme = "gemini"   # optional
```

The theme applies **only to the ACP panel**, not the rest of Helix. When you
connect or cycle to an agent with a theme, the panel uses that theme. When you
close the panel or switch to an agent without a theme, the panel uses the global
theme again.

---

## Chat (Enter) – prompt flow

When you press Enter in the ACP input:

1. **Request:** Helix sends `session/prompt` with `{ sessionId, prompt: [{ type: "text", text: "..." }] }`.
   - Matches ACP spec. No serialization issues.
2. **Streaming:** The agent sends `session/update` notifications with
   `agent_message_chunk` as it generates. The update object uses `sessionUpdate` as
   the discriminator (not `type`). Helix receives these on the incoming channel
   and calls `panel.append_agent_text()` immediately. Text appears incrementally.
3. **Completion:** When the turn finishes, the agent responds to the prompt request
   with `stop_reason`. Helix marks the panel idle and shows "Done (completed)" etc.

---

## session/update notifications

- `config_option_update`: `{ config_options: [...] }` – updates
  model/thinking/mode when changed.
- `current_mode_update`: `{ mode_id: "..." }` – updates session mode (used with
  `mode_name_for_id` when `session_modes` is empty).
- `agent_message_chunk`: streaming text chunks – appended to the panel as they arrive.

---

## Future work: UI changes

**Chat input modal editing.** The ACP chat input currently uses a manual Normal/Insert mode implementation: we route vim-style keys (h, l, w, b, i, a, x, etc.) to the Prompt's primitives. Helix has no modal input abstraction—the Prompt is readline-style only, and the keymap is tied to documents. When tackling broader ACP UI changes, consider:

- Using a real document for the chat input and switching editor focus so the keymap applies (would require layout changes to render that doc in the panel area), or
- Extending the Prompt upstream to support a modal mode.
