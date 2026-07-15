# ACP event type validation

Validation of all JSON-RPC messages sent and received between Helix (client) and the mock agent.
The mock agent is a development fixture. Build/run from the repository root
with `--features mock-acp` to add it to ACP discovery before running these
checks. Builds without that feature, including default release builds, do not
compile it in.

## Client → Agent (Helix sends)

| Method | Params | Schema |
|--------|--------|--------|
| `initialize` | `protocolVersion`, `clientCapabilities`, `clientInfo` | InitializeRequest |
| `session/new` | `mcpServers`, `cwd` | NewSessionRequest |
| `session/prompt` | `sessionId`, `prompt` (ContentBlock[]) | PromptRequest |
| `session/set_config_option` | `sessionId`, `configId`, `value` | SetSessionConfigOptionRequest |
| `session/cancel` | `sessionId` | CancelNotification (no response) |

## Agent → Client (mock sends)

| Type | Method/Response | Schema |
|------|-----------------|--------|
| Response | `initialize` result | `protocolVersion`, `agentCapabilities`, `agentInfo` |
| Response | `session/new` result | `sessionId`, `configOptions` |
| Response | `session/prompt` result | `stopReason` (snake_case: end_turn, cancelled, etc.) |
| Response | `session/set_config_option` result | `configOptions` |
| Notification | `session/update` | `sessionId`, `update` with `sessionUpdate` discriminator |

## session/update variants (agent → client)

| sessionUpdate | Content |
|---------------|---------|
| `agent_message_chunk` | `content: { type: "text", text: "..." }` |
| `config_option_update` | `configOptions: [...]` |
| `current_mode_update` | `modeId: "..."` |
| `plan` | `entries: [...]` |
| `tool_call` | tool call info |
| `tool_call_update` | tool call status |

## Key schema details

- **InitializeResponse**: `agentCapabilities` (not `capabilities`), `agentInfo` (not `implementation`)
- **SessionUpdate**: discriminator is `sessionUpdate` (not `type`)
- **ContentBlock**: `{ type: "text", text: "..." }`
- **StopReason**: snake_case (`end_turn`, `cancelled`, `max_tokens`, etc.)
- **ConfigOption**: `currentValue`, `options` with `value` and `name`
- **SetSessionConfigOptionRequest**: param is `value` (not `valueId`)

## Testing cancel

1. Start Helix with mock agent.
2. Open ACP panel, send `slow hello` (streams with ~45ms delay per chunk).
3. Press `x` or run `:acp-cancel` while streaming.
4. Client sends `session/cancel` notification; mock stops streaming and responds with `stopReason: "cancelled"`.
