# Crate Abstractions, Types, and Event Bus Review

Status: historical review, superseded for plugin/runtime ownership by
`responsive-application-architecture.md` and `runtime-architecture.md`.

This document maps the current crate abstractions in the Helix fork, how the crates interact, and where the remaining API and ownership work still lives.

It is guided by:

- `docs/runtime-executor-architecture-spec.md`
- `docs/collaboration-assistant-architecture-spec.md`
- `docs/acp-api-architecture-spec.md`
- `docs/runtime-collaboration-implementation-plan.md`
- the Rust skill guidance on naming, API shape, traits, and typestate

The goal is not genericization for its own sake. The goal is to keep ownership explicit, remove weak abstraction seams, and model policy and lifecycle with semantic types instead of bool soup.

## Summary

The crate split is broadly right and materially improved from the earlier state.

- `helix-core` is the text and domain crate.
- `helix-runtime` is the execution and scheduling crate.
- `helix-view` is the durable editor-state and synchronous mutation-authority crate.
- `helix-term` is the terminal frontend and application/runtime orchestrator.

The old event story is no longer the main problem, because the global `helix-event` architecture is gone. The current design is now intentionally split by concern instead of forcing one universal bus.

The main remaining design work is now:

1. finishing `helix-view` ownership cleanup where `helix-term` still reaches through editor internals for rendering- or panel-adjacent logic
2. tightening the `helix-plugin` abstraction, which is now the weakest major crate boundary
3. continuing semantic API cleanup where plugin and UI surfaces still expose raw positional or string-heavy contracts

## Current Crate Map

### `helix-core`

Responsibility:

- pure editing and text-domain primitives
- rope/text, selections, transactions, movement, syntax, diagnostics, formatting data

Should own:

- domain algorithms and value types
- frontend-neutral and transport-neutral data

Should not own:

- runtime orchestration
- frontend concerns
- editor lifetime or mutation orchestration

Assessment:

- ownership: strong
- API shape: strong
- abstraction quality: strong

### `helix-runtime`

Responsibility:

- canonical execution-domain API
- `Runtime`, `Ui`, `Work`, `Block`, `Clock`
- cancellation, groups, mailboxes, timers, wait sets, debounce/latest/gate behavior

Should own:

- execution-domain distinctions in the type system
- task and lifecycle semantics
- typed mailbox primitives

Should not own:

- editor mutation
- frontend/compositor details
- feature-specific editor event domains

Assessment:

- ownership: strong
- API shape: strong
- abstraction quality: strong

### `helix-view`

Responsibility:

- canonical editor state and synchronous mutation authority
- `Editor`, documents, views, component docs/views, collaboration, assistant, derived model state
- editor-owned lifecycle subscribers and handler/query surfaces

Should own:

- durable editor state
- editor invariants
- editor-side semantic operations
- collaboration and assistant stores
- editor-local lifecycle dispatch

Should not own:

- terminal compositor/widgets
- terminal IO/event-loop policy
- protocol-specific transport adaptation that belongs at the edge

Assessment:

- ownership improved substantially and is now much closer to the intended shape
- this is now the real owner of editor semantics instead of `helix-term`

### `helix-term`

Responsibility:

- terminal frontend
- `Application` orchestration
- compositor and widgets
- typed runtime ingress application on the terminal side
- translation between runtime, editor, and compositor operations

Should own:

- app loop
- terminal-local state
- compositor-local UI application
- frontend-specific keymaps and integration wiring
- terminal-local synchronous post-command and post-input dispatch

Should not own:

- durable editor data model
- collaboration or assistant durable state
- generic runtime abstractions that belong in `helix-runtime`

Assessment:

- much better than earlier passes
- remaining debt is now narrower and mostly render-adjacent, assistant-panel-adjacent, or plugin-adjacent

### `helix-modal`

Responsibility:

- pluggable editing engines
- modal composition over shared editor command atoms

Assessment:

- already uses a good boundary style: small engine-facing surface with real behavioral variation behind it

### Support and edge crates

- `helix-lsp`, `helix-dap`, `helix-acp`: protocol and service edges
- `helix-plugin`: plugin host, scripting bridge, and plugin event delivery
- `helix-loader`, `helix-vcs`, `helix-stdx`, `helix-tui`: support crates

## Current Interaction Model

The intended interaction model is now:

1. `helix-core` provides domain primitives.
2. `helix-runtime` provides execution primitives.
3. `helix-view::Editor` owns durable editor mutation.
4. `helix-term::Application` receives typed runtime events and drives editor and compositor actions.
5. protocol crates and services feed typed results into that flow.

The dominant good seam is:

1. async or background work computes outside the editor
2. typed ingress delivers results back to the app loop
3. `helix-term` routes by concern
4. `helix-view::Editor` performs durable mutation

That matches the runtime specs and should remain the default pattern.

## Event Architecture

## `helix-event` is gone

The earlier review treated `helix-event` reduction as future work. That is now stale.

The global hook registry has been removed from the active architecture. The replacement is an explicit three-way split by concern.

## The current bus split

### 1. Typed async ingress for domain-crossing delivery

Primary types:

- `RuntimeEvent`
- `RuntimeTaskEvent`
- `UiCommand`
- `helix_runtime::Sender` / receiver pairs

Primary home:

- `helix-term/src/runtime/ingress.rs`

Use this for:

- background completions
- timers
- async service results
- task completion and status delivery
- compositor commands that must re-enter through the app loop

This is the right replacement for callback-shaped async re-entry.

### 2. Terminal-local synchronous dispatcher for command and input follow-up

Primary home:

- `helix-term/src/handlers/local.rs`

Use this for:

- post-command completion cleanup
- post-insert-char completion/signature-help behavior
- mode-switch follow-up work such as auto-save, auto-reload, signature-help cancellation, and redraw requests
- plugin mode-change notifications emitted from the frontend side

This is intentionally local. These are terminal-orchestration concerns, not generic editor lifecycle events.

### 3. Editor-owned synchronous lifecycle bus

Primary homes:

- `helix-view/src/events.rs`
- `helix-view/src/editor/hooks.rs`

Current shape:

- typed event structs such as `DocumentDidOpen`, `DocumentDidChange`, `DiagnosticsDidChange`, `LanguageServerInitialized`, and `ConfigDidChange`
- `LifecycleBus` stored on the editor side
- typed registration methods such as `on_document_open`, `on_document_change`, `on_selection_change`, and `on_config_change`
- synchronous dispatch methods owned by the editor boundary

This is the correct home for editor-local lifecycle reactions. It is typed, non-global, and aligned with real ownership.

## Event architecture conclusion

There should not be one universal event bus.

The current three-part split is the right end state:

1. typed mailboxes for async and domain-crossing delivery
2. terminal-local synchronous dispatch for frontend-local follow-up
3. editor-owned synchronous lifecycle subscriptions for editor mutations

No global fallback bus should be reintroduced.

## Type and API Cleanup Already Landed

The document previously listed several cleanup targets as future work. Many of those have now landed.

### Policy enums and request structs now present

Confirmed examples:

- `SavePolicy`
- `ClosePolicy`
- `Activation`
- `PanelBehavior`
- `ThreadSelectPolicy`
- `FrameSelection`
- `IdleRender`
- `PendingFormatWrite`
- `ShowDocumentRequest`
- `BenchActionUpdate`
- `BenchFrameUpdate`

This is the right direction.

- policy decisions are now explicit at call sites
- tuple payloads are being replaced with named structures
- cross-crate APIs read more like semantic operations and less like hidden boolean protocols

## Typestate guidance

The earlier guidance still stands.

Typestate is appropriate when all of these hold:

- the state is owned
- the transitions are staged or linear
- invalid states should be unrepresentable at the API boundary
- the type change makes the call site simpler or safer

Typestate is not the right default tool for long-lived dynamic editor state. Those usually want enums, not staged wrapper types.

Use this rule of thumb:

- staged registration or setup phases: typestate can help
- dynamic runtime or editor state: prefer enums
- policy booleans: prefer small enums
- multiple genuine implementations: use traits

## Remaining Crate Findings

## `helix-view` and `helix-term`

This seam improved substantially.

Recent cleanup moved more semantics into `helix-view`, including editor-owned helpers for:

- runtime work access
- bench lifecycle and reporting
- focus semantics
- diagnostics and language-server queries
- handler senders and request-state helpers
- assistant thread, model, and snapshot queries

Remaining obvious debt is narrower now:

- panel-local and rendering-local behavior in `helix-term/src/ui/assistant.rs`
- some render-adjacent direct `tree` and document access
- plugin integration surfaces
- support-crate boundary quality in `helix-plugin` and `helix-tui`

## Crate Grade Snapshot

Ordered as performance / API / abstractions-patterns:

| Crate | Grade |
| --- | --- |
| `helix-core` | A / A- / A- |
| `helix-runtime` | A / A / A |
| `helix-view` | A- / A / A- |
| `helix-term` | A- / A- / A- |
| `helix-plugin` | A- / B+ / B+ |
| `helix-tui` | A- / B+ / B+ |

The remaining B+ crates are now mainly `helix-plugin` and `helix-tui`.

## Plugin System Review

## Current `helix-plugin` shape

Primary homes:

- `helix-plugin/src/lib.rs`
- `helix-plugin/src/types.rs`
- `helix-plugin/src/lua/mod.rs`
- `helix-plugin/src/lua/api/*.rs`
- `helix-term/src/plugin_registry.rs`
- `helix-term/src/runtime/ui/plugin.rs`

Current strengths:

- the former `PluginManager` provided one ownership root for plugin loading and event delivery
- Rust-side plugin events now use a single typed `PluginEvent` enum, with `EventType` retained for registration and naming
- `helix-plugin` defines a `DrawSurface` abstraction so the plugin crate does not depend directly on `helix-tui`
- command execution and UI bridging are already separated into host traits (`EditorCommandRegistry`, `UiHandler`)
- `UiHandler` now uses request structs instead of positional calls
- callback ids are now typed with `UiCallbackId`

Current weaknesses:

1. The bridge relies on thread-local raw pointers for editor, surface, and theme context.
2. The bridge still uses ambient thread-local context instead of explicit borrowed host/context objects.
3. Large parts of the plugin API still rely on `get_editor_mut()` rather than a clearer capability-oriented surface.
4. The Lua event surface is still string-driven even though Rust-side events are now fully typed.
5. UI interactions are still callback- and registry-key driven internally, even if the public surface is better typed.
6. Some plugin APIs are still narrower than their names suggest, especially focused-buffer mutation behavior.
7. Some plugin-facing ids are still exposed as formatted strings rather than stronger typed handles.

The current system is better than a raw scripting bridge, but it is not yet a full typed capability host.

## Neovim comparison

Neovim should not be copied directly, but it is a useful reference for plugin-host architecture.

Relevant properties from Neovim stable:

1. Remote plugins run out of process over msgpack-RPC.
2. Hosts are language-specific and lazily loaded.
3. Plugin registration is manifest-driven via `:UpdateRemotePlugins` and `remote#host#RegisterPlugin`.
4. The public API exposes stable opaque handle types such as `Buffer`, `Window`, and `Tabpage` instead of one giant mutable editor pointer.
5. API metadata is discoverable through `nvim_get_api_info()`.
6. Buffer subscriptions are explicit via `nvim_buf_attach()` and emit structured update events.
7. Fast-event contexts are explicitly constrained: code may inspect state, but editor mutation must be scheduled back onto the main loop.

The important lessons are conceptual, not architectural cargo culting:

- keep host boundaries explicit
- expose typed handles instead of raw host internals
- separate read-only contexts from mutating operations
- make event contracts explicit and extensible
- distinguish immediate callback contexts from scheduled mutation contexts

## What Helix should borrow conceptually

Helix does not need Neovim's exact remote-host model, manifest flow, or RPC-first architecture.

It should borrow these ideas instead:

1. a typed capability host surface rather than ambient mutable editor access
2. opaque plugin-facing handles for documents, views, panels, and maybe language-server sessions
3. a clear split between read-only snapshot access and mutation requests
4. explicit event contracts rather than string-heavy dynamic registration as the primary design
5. scheduled mutation for contexts that are logically render-only or callback-only

## Recommended `helix-plugin` direction

### 1. Remove ambient mutable editor access

The raw thread-local editor bridge was the highest-value first fix, and the worst part of it has now been reduced.

Target direction:

- stop exposing generic `get_editor_mut()` as the universal plugin escape hatch
- introduce explicit host/context objects for each call path
- make render callbacks read-only by construction
- make mutation happen through explicit command or request methods

Current status:

- `get_editor()` and `get_editor_mut()` are now split
- `with_editor_context_ref(...)` is now truly read-only
- the next step is to replace more ambient access with explicit context objects rather than relying on thread-local lookup

### 2. Split plugin capabilities by concern

Instead of one fuzzy surface, break the host into smaller semantic groups:

- editor snapshot and queries
- editor mutation requests
- UI interaction requests
- render surface access
- command registration and invocation
- event subscription

Traits are appropriate here because these are real host capabilities, not just naming exercises.

### 3. Replace positional UI methods with request structs

This change has now landed.

`UiHandler` now uses semantic requests such as:

- `PromptRequest`
- `ConfirmRequest`
- `PickerRequest`
- `PanelRegistration`
- `PanelRemoval`

This will improve naming, call-site readability, and future extensibility.

### 4. Keep events typed end to end

This change has also landed on the Rust side.

Target direction:

- preserve a closed Rust event enum
- give Lua a stable, structured representation of those events
- avoid making string event names the primary source of truth

Current status:

- `PluginEvent` is now the typed source of truth
- `EventType` remains useful as the subscription key and Lua-facing name mapping
- Lua registration is still stringly and remains the next obvious event-model improvement

### 5. Use typestate only where it genuinely helps

Typestate may help for staged plugin registration or panel lifecycle handles.

Good candidates:

- panel registration builder to registered handle
- command builder to installed command
- read-only render context vs scheduled mutation handle

Bad candidate:

- general editor runtime state exposed to plugins

That should stay enum- and request-based.

## Bottom Line

The core crate architecture is now mostly right.

The big architectural shift since the earlier review is complete:

- `helix-event` is no longer the story
- event flow is now split correctly by concern
- `helix-view` is much closer to being the true owner of editor semantics
- policy enums and request structs are replacing weak boolean and tuple APIs

The highest-value remaining abstraction problem is `helix-plugin`.

The right end state for the plugin system is not a larger dynamic bridge. It is:

- explicit host ownership
- typed handles and typed events
- read-only versus mutating capability separation
- request-oriented UI surfaces
- no ambient unsafe editor access during render or callback phases

The initial implementation pass has already moved in that direction:

- invalid event kind and payload combinations are now unrepresentable
- request-oriented UI surfaces are in place
- read-only render/editor contexts are enforced more strongly
- more plugin-facing operations now go through editor-owned semantic helpers
