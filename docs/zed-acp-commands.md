# Zed ACP commands reference

From Zed docs/source (zed-industries/zed). Use this to align helix-fork agent config with Zed’s ACP UX.

## Zed UI commands (Command Palette)

| Command | Description |
|--------|-------------|
| `zed: extensions` | Open extensions; set filter to "Agent Servers" |
| `zed: acp registry` | Open ACP Registry in browser |
| `zed: open keymap file` | Edit keymap for agent keybindings |
| `dev: open acp logs` | Open ACP debug logs (messages to/from agent) |
| `agent: new thread` | New thread in agent panel (then pick agent from +) |

## New external agent thread (keymap)

In `keymap.json`, bind `agent::NewExternalAgentThread` with an agent name:

- **Claude Agent:** `{ "agent": { "custom": { "name": "claude-acp" } } }`
- **Gemini CLI:** `{ "agent": { "custom": { "name": "gemini" } } }`
- **Codex:** `{ "agent": { "custom": { "name": "codex-acp" } } }`

Example (e.g. cmd-alt-c for Claude):

```json
[
  {
    "bindings": {
      "cmd-alt-c": [
        "agent::NewExternalAgentThread",
        { "agent": { "custom": { "name": "claude-acp" } } }
      ]
    }
  }
]
```

## How Zed runs agents

- **Registry:** Agents are listed in the [ACP Registry](https://agentclientprotocol.com/registry). Zed fetches `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json` and uses each entry’s `distribution.binary` (per-platform `cmd` + `args`) or `distribution.npx` (`package` + `args`).
- **NPX agents:** Zed runs `npm exec --yes <package> [-- args...]` (see `agent_server_store.rs`).
- **Claude Agent:** Current ACP registry entries use `@agentclientprotocol/claude-agent-acp`; that adapter runs the Claude Code CLI.
- **Settings override:** Under `agent_servers` you can set `"type": "registry"` and use registry names `claude-acp`, `codex-acp`, `gemini` plus custom `env` (e.g. `CLAUDE_CODE_EXECUTABLE`).

## Custom agent (settings)

```json
{
  "agent_servers": {
    "My Agent": {
      "type": "custom",
      "command": "node",
      "args": ["~/projects/agent/index.js", "--acp"],
      "env": {}
    }
  }
}
```

## Agent server extension (legacy)

Extensions define agents in `extension.toml` with `[agent_servers.<id>]`, per-target `archive`, `cmd`, `args`, optional `sha256` and `env`. Prefer the ACP Registry from v0.221.x.

## Source references

- `zed-src/docs/src/ai/external-agents.md` – Claude, Codex, Gemini, registry, custom agents
- `zed-src/docs/src/extensions/agent-servers.md` – Agent Server extensions (deprecated in favor of registry)
- `zed-src/docs/src/reference/cli.md` – Zed CLI (open files, `--wait`, etc.; no ACP subcommands)
- `zed-src/crates/project/src/agent_server_store.rs` – `GEMINI_NAME`, `CLAUDE_AGENT_NAME`, `CODEX_NAME`; NPX command build
- `zed-src/crates/project/src/agent_registry_store.rs` – Registry fetch and binary/npx config
