# Plugin API parity

This note compares the host-agnostic `helix-plugin` contract with the Steel
plugin prototype tracked by helix-editor/helix#8675. The audited Steel source is
`mattwparas/helix@0522d519fd5227f77ecef387a87e51b732907562` on
`steel-event-system`.

Primary references:

- https://github.com/helix-editor/helix/pull/8675
- https://github.com/mattwparas/helix/tree/0522d519fd5227f77ecef387a87e51b732907562
- https://raw.githubusercontent.com/mattwparas/helix/0522d519fd5227f77ecef387a87e51b732907562/STEEL.md

## Current strengths

The local contract is stronger where stable extension boundaries matter:

- typed, serializable handles, requests, snapshots, errors, and events;
- one framed contract shared by every supervised Lua host;
- capability and API-version discovery;
- owned command, keymap, event, panel, float, and subscription handles with
  reload and host-generation cleanup;
- watchdog, memory limit, per-plugin module roots, reload cleanup, and process
  isolation;
- nonblocking document-open operations with cancellation, invocation-view
  targeting, real-handle completion, and structured errors;
- retained host-rendered panels and floats that never invoke plugin code during
  a frame, plus splits, per-view tabs, and assistant APIs;
- immutable cancellable syntax queries and cancellable typed custom LSP calls.

## Steel-only or broader surfaces

| Surface | Steel prototype | Local status | Direction |
| --- | --- | --- | --- |
| Builtin commands | Automatically exports typable and static commands with docs | Typable and static commands are discoverable with kind, scope, aliases, docs, parser signatures, and flags through the framed host contract | Keep execution queued through host-owned command dispatch without exposing editor context |
| Keymaps | Direct key-event interception plus global, extension, and labelled-buffer maps | Owned editor keymaps support mode plus language and path-prefix scopes through compiled immutable snapshots | Do not advertise component scopes until non-editor components consume the same contribution model |
| Configuration | Programmable editor, file-picker, language, and LSP configuration | Read-only editor config snapshot and per-plugin config | Add validated override layers owned by plugin/package identity |
| Themes | Define, list, register, and select themes | Read-only snapshots plus cancellable off-thread activation | Route definitions and catalog discovery through the runtime asset/provider abstraction |
| Tree-sitter | Tree, node, layer, query, capture, and byte-range APIs | Immutable rope/tree/grammar snapshots with cancellable background queries, byte ranges, and capture limits | Add higher-level node traversal only when it can preserve snapshot ownership and cancellation |
| LSP extension | Raw notification/call handlers, custom requests, inlay hints | Typed client discovery and cancellable custom requests with document/server identity and recursive typed values | Add notifications and handler registration only with explicit ownership and lifecycle semantics |
| Components | Custom compositor components, buffers, events, styles, widgets, and status elements | Host-owned panels/floats with serializable retained text, fill, header, divider, input, and scrollbar nodes | Add new retained nodes through the shared renderer; never expose compositor references or frame callbacks |
| Hooks | Mode, insert, command, terminal focus, document, selection, save, and close hooks | Typed catalog is broader, but only fully emitted events are now advertised | Add missing events only together with real emitters and transport tests |
| Async | Futures, native threads, and main-thread callback scheduling | Typed coroutine awaitables and runtime queues | Continue the typed operation model; never expose an editor lock or blocking main-thread callback |
| Native extension | Steel can load ABI-compatible dynamic libraries | Out-of-process framed hosts | Prefer process isolation and declared capabilities; native in-process loading requires a separate trust policy |

## Contract priorities

1. Unify plugin/package/runtime contributions as validated asset providers for
   languages, themes, grammars, keymaps, and configuration layers.
2. Complete typed LSP notifications and owned handler registration.
3. Add validated configuration override layers owned by plugin/package identity.
4. Extend retained nodes only for concrete component needs, preserving clipping,
   immutable frame reads, and transport serialization.

Package backends are intentionally not exposed by the plugin contract yet. The
package engine has a transport adapter, but plugin callbacks cannot be invoked
safely from blocking package workers until an owned out-of-process provider
transport exists end to end.

The Steel prototype intentionally gives Scheme direct access to a live Helix
context. This fork should not copy that boundary: plugin work must continue to
use snapshots, owned handles, typed operations, and queued UI-thread apply.
