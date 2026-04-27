# Host-Agnostic Plugin API

**Status:** Ready for task breakdown
**Effort:** XL
**Approved by:** user
**Date:** 2026-04-10

## Problem Statement

**Who:** Helix core/frontend/plugin maintainers, and future plugin authors targeting terminal, GUI, web, or remote-hosted Helix runtimes.

**What:** The current plugin system began as an embedded Lua bridge. It has been improved materially, but its public shape is still too coupled to the in-process runtime model, too string-driven in places, and too dependent on ambient host context.

**Why it matters:** If Helix wants a serious extension surface that can survive terminal, GUI, web, wasm, JSON/RPC, and possible remote-core deployments, the plugin API must be defined as a stable host contract rather than as “Lua code calling into the current Rust process.”

**Evidence:** Current `helix-plugin` still relies on ambient context setup, Lua registration names, and focused-buffer shortcuts. The workspace discussion explicitly identified GUI/web/JSON/wasm as plausible future directions. Neovim and WezTerm both show useful patterns here, but also show traps Helix should avoid.

## Proposed Solution

Helix should adopt a host-agnostic plugin contract with a hybrid model:

- opaque handles for identity and query
- explicit requests for most mutation and UI effects
- structured typed events
- explicit host capability objects instead of ambient global editor access
- thin Lua ergonomics layered on top of the host contract, not the other way around

The final plugin API should be designed as if it may eventually cross a transport boundary, even when running in-process today. That means the core contract must be compatible with serialization, async scheduling, capability separation, and stale-handle/error handling.

The target design is not “copy Neovim RPC” and not “copy WezTerm object methods.” It should instead combine:

- Neovim's strong opaque-handle and metadata discipline
- WezTerm's object-oriented discoverability and callback ergonomics
- Helix's own desire for stronger typing, cleaner crate ownership, and fewer string protocols

## Scope & Deliverables

| Deliverable | Effort | Depends On |
|-------------|--------|------------|
| Final host-agnostic plugin contract | L | - |
| Public handle/event/request type model | L | D1 |
| Capability object layout for hosts/frontends | L | D1 |
| Lua API facade shape over the host contract | M | D2, D3 |
| Migration plan from current Lua bridge | L | D2, D4 |

## Non-Goals (Explicit Exclusions)

- Do not define a concrete msgpack/json transport in this spec.
- Do not commit to wasm, RPC, or out-of-process plugins as the first implementation target.
- Do not preserve the current Lua API for compatibility if it conflicts with the better final contract.
- Do not expose raw `Editor`, `Document`, `View`, or frontend internals across the plugin boundary.
- Do not build a giant universal `PluginHost` interface that collapses all concerns into one trait.

## Discovery Summary

### Current Helix findings

- `helix-plugin` now has a typed `PluginEvent` model, request-based `UiHandler`, typed callback ids, and a partial query/mutation/render context split.
- `helix-term` acts as the frontend host and request router for plugin UI and runtime integration.
- `helix-view` is increasingly the owner of editor semantics and now exposes a growing set of semantic helper methods used by plugins indirectly.
- The current public Lua API still exposes too much “current focused editor” behavior instead of explicit resource-oriented behavior.

### Neovim findings worth borrowing

- opaque handle types are treated as distinct API-level entities
- API metadata is discoverable and versioned
- event and function contracts are structured and extensible
- fast contexts are explicitly restricted from mutating editor state

### Neovim traps to avoid

- giant flat API surface with many ad hoc calls
- too much stringly/event-name oriented API evolution
- transport model leaking into every API design decision
- weakly shaped dynamic tables as the long-term public contract

### WezTerm findings worth borrowing

- public object model is discoverable and pleasant to navigate
- object handles are passed into callbacks naturally
- modules are separated by concern (`window`, `pane`, `mux`, `wezterm`, etc.)
- event callbacks and object methods are reasonably ergonomic for Lua

### WezTerm traps to avoid

- too much direct object-method surface can blur identity/query versus mutation/effects
- callback-driven APIs can grow organically without enough contract discipline
- the public object model can become runtime-specific if not backed by a stable host contract

## Final Architecture Decision

## Decision

Helix should adopt a **host-agnostic hybrid plugin API**:

- **handles for identity and query**
- **requests for mutation and UI effects**
- **structured events for observation**
- **capability objects for host access**
- **Lua facade layered on top of the contract**

This is the final-state public API direction.

## Why this over the alternatives

### Over Lua-only

- avoids coupling the API to in-process shared-memory assumptions
- keeps GUI/web/remote/wasm options viable
- improves capability boundaries even for the embedded host

### Over fully handle-oriented mutation

- mutations and UI effects are easier to version, validate, schedule, and transport as requests
- avoids turning handles into semi-live mutable proxies to editor internals

### Over fully command-oriented mutation

- pure command APIs become too flat and hard to navigate
- handle-oriented queries are much more natural for authors

The hybrid model keeps the good parts of both.

## Final Public Model

## Top-level modules

The final plugin API should be organized by concern, not by “whatever is easy to expose from Rust.”

Top-level public modules:

- `helix.host`
- `helix.workspace`
- `helix.documents`
- `helix.views`
- `helix.panels`
- `helix.commands`
- `helix.ui`
- `helix.events`
- `helix.log`

### `helix.host`

Purpose:

- host/runtime identity and capabilities
- feature discovery
- API version/metadata
- scheduling and task utilities

Example responsibilities:

- `api_version()`
- `capabilities()`
- `schedule(task)`
- `current_frontend()`

### `helix.workspace`

Purpose:

- workspace/session/global queries
- current active handles
- config/theme/environment introspection

Example responsibilities:

- `current_document()` -> `DocumentHandle?`
- `current_view()` -> `ViewHandle?`
- `list_documents()` -> `[DocumentHandle]`
- `list_views()` -> `[ViewHandle]`
- `theme()` -> `ThemeSnapshot`
- `config()` -> `ConfigSnapshot`

### `helix.documents`

Purpose:

- document handle lookup/opening/query

Example responsibilities:

- `open(request)` -> `DocumentHandle`
- `by_handle(handle)` -> `Document?`
- `by_path(path)` -> `DocumentHandle?`

### `helix.views`

Purpose:

- view handle lookup/query/focus requests

Example responsibilities:

- `by_handle(handle)` -> `View?`
- `focus(request)`
- `for_document(document_handle)` -> `[ViewHandle]`

### `helix.panels`

Purpose:

- panel registration and lifecycle

Example responsibilities:

- `register(definition)` -> `PanelHandle`
- `close(handle)`
- `set_title(handle, title)`
- `update_state(handle, state_patch)`

### `helix.commands`

Purpose:

- command registration and invocation

Example responsibilities:

- `register(definition)` -> `CommandHandle`
- `update(handle, update)`
- `remove(handle)`
- `run(request)` -> `CommandResult`

### `helix.ui`

Purpose:

- notifications, prompts, pickers, confirms, clipboard, frontend-facing requests

Example responsibilities:

- `notify(request)`
- `confirm(request)` -> `Future<bool>` or callback token depending on host
- `prompt(request)` -> `Future<string?>`
- `picker(request)` -> `Future<Selection?>`

### `helix.events`

Purpose:

- typed event subscription
- lifecycle and editor observation

Example responsibilities:

- `subscribe(kind, handler)` -> `SubscriptionHandle`
- `unsubscribe(handle)`

### `helix.log`

Purpose:

- structured logging

Example responsibilities:

- `trace(...)`
- `debug(...)`
- `info(...)`
- `warn(...)`
- `error(...)`

## Handle model

Handles are opaque identities, not mutable stateful proxies.

Required handle types:

- `PluginId`
- `DocumentHandle`
- `ViewHandle`
- `PanelHandle`
- `CommandHandle`
- `SubscriptionHandle`

Optional later:

- `WorkspaceHandle`
- `DiagnosticCollectionHandle`
- `LanguageServerHandle`

### Handle rules

1. Handles are opaque and stable only within the current host session.
2. Handles may become stale; all APIs using them must define stale-handle errors.
3. Handles must be serializable as primitive identity values at the contract layer.
4. Handles are not themselves permission objects.
5. Handles should not expose raw editor internals or storage layout.

### Handle behavior

Handles may have query helpers in rich hosts like Lua userdata, but mutation should not primarily live as direct object mutation methods.

Good:

- `doc:metadata()`
- `doc:text_snapshot()`
- `doc:handle()`

Less good as the primary model:

- `doc:insert(...)`
- `doc:delete(...)`
- `doc:set_mode(...)`

Those should map to requests.

## Capability model

The public API should be backed by explicit capability objects, not one global editor bridge.

Core capability objects:

- `QueryContext`
- `MutationContext`
- `RenderContext`
- `UiContext`
- `CommandContext`

These may be represented differently in each language host, but the conceptual contract must remain stable.

### `QueryContext`

Purpose:

- read-only access to current session/editor/workspace state

Allowed:

- current handles
- handle lookups
- snapshots
- diagnostics/config/theme queries

Forbidden:

- document mutation
- layout mutation
- panel registration
- UI prompts

### `MutationContext`

Purpose:

- issue mutation requests against editor resources

Allowed:

- apply edits
- save documents
- focus views
- set selections
- set annotations
- schedule redraw-like effects through requests

Forbidden:

- direct mutable access to editor internals
- direct render operations

### `RenderContext`

Purpose:

- render to a host-provided surface with read-only query access

Allowed:

- drawing/text/layout primitives
- theme access
- read-only workspace/document/view queries

Forbidden:

- editor mutation
- prompting/pickers
- command execution with side effects

This must remain a hard boundary.

### `UiContext`

Purpose:

- prompt/confirm/picker/notification/clipboard requests

This is frontend-dependent and should be capability-gated.

### `CommandContext`

Purpose:

- command registration/invocation
- command argument parsing/validation at the host boundary

## Data model

## Snapshots vs live objects

The public contract should distinguish:

- **handles**: identity
- **snapshots**: immutable state data
- **requests**: mutations/effects
- **events**: observations

Examples:

- `DocumentHandle`
- `DocumentSnapshot`
- `ApplyEditRequest`
- `DocumentChangedEvent`

This is a critical rule. Do not collapse these into one general-purpose object.

## Required snapshot types

- `DocumentSnapshot`
- `ViewSnapshot`
- `WorkspaceSnapshot`
- `SelectionSnapshot`
- `ThemeSnapshot`
- `DiagnosticSnapshot`
- `PanelSnapshot`

Each snapshot should be fully serializable and contain only stable public fields.

## Mutation model

The final API uses the hybrid model.

### Query via handles/snapshots

Examples:

- `workspace.current_document()` -> `DocumentHandle?`
- `documents.snapshot(handle)` -> `DocumentSnapshot`
- `views.snapshot(handle)` -> `ViewSnapshot`

### Mutate via requests

Examples:

- `documents.apply_edit(ApplyEditRequest)`
- `documents.set_selection(SetSelectionRequest)`
- `documents.save(SaveDocumentRequest)`
- `views.focus(FocusViewRequest)`
- `panels.open(OpenPanelRequest)`
- `ui.prompt(PromptRequest)`

### Focused-context sugar

Allowed only as convenience wrappers over the request model:

- `workspace.current_document()`
- `workspace.current_view()`
- maybe `workspace.current_selection()`

Not allowed as the core mutation model:

- “all useful mutations only work on current buffer/view”

That is a trap of the current API and should be treated as transitional only.

## Request model

All side-effecting or frontend-dependent operations should be request-shaped.

Required request types:

- `OpenDocumentRequest`
- `ApplyEditRequest`
- `ReplaceRangeRequest`
- `SetSelectionRequest`
- `SaveDocumentRequest`
- `FocusViewRequest`
- `PromptRequest`
- `ConfirmRequest`
- `PickerRequest`
- `PanelRegistration`
- `PanelUpdateRequest`
- `PanelCloseRequest`
- `CommandUpdateRequest`
- `CommandRemoveRequest`
- `RunCommandRequest`

### Request rules

1. Requests must have named fields, not positional protocols.
2. Optional extensibility fields should be grouped into `opts` or explicit optional fields.
3. Requests must be transport-safe and serializable at the contract layer.
4. Requests should return structured results, not ad hoc strings.

## Event model

The public event model should remain a closed typed enum in Rust and a structured event contract in other hosts.

Required event families:

- host lifecycle events
- document lifecycle events
- selection/focus/view events
- diagnostics/language-server events
- panel/UI events
- command events

Example final Rust shape:

```rust
pub enum PluginEvent {
    HostReady(HostReadyEvent),
    DocumentOpened(DocumentOpenedEvent),
    DocumentChanged(DocumentChangedEvent),
    DocumentSaved(DocumentSavedEvent),
    SelectionChanged(SelectionChangedEvent),
    ViewFocused(ViewFocusedEvent),
    DiagnosticsUpdated(DiagnosticsUpdatedEvent),
    PanelInput(PanelInputEvent),
}
```

### Event rules

1. Event kind is part of the type, not a string field plus a payload blob.
2. Event payloads must be structured and versionable.
3. Events may be extended compatibly by adding optional fields or new variants.
4. Lua-facing event names may exist, but are aliases over structured event definitions.

## Registration model

The final system should not use free-form string registration as the primary model.

Final direction:

- predefined event kinds are registered via stable identifiers
- commands are registered via typed definitions
- panels are registered via typed definitions
- custom plugin-defined events, if supported, live in a separate namespace and do not collide with core events

### Good final registration examples

- `events.subscribe(EventKind.DocumentChanged, handler)`
- `commands.register(CommandDefinition { ... })`
- `panels.register(PanelDefinition { ... })`

### Avoid

- `on("buffer_open", ...)` as the core system model
- event names that are just undocumented strings
- panel registration that relies on title-derived ids

## Lua API shape

Lua should remain ergonomic, but it should be a facade over the host-agnostic contract.

### Final Lua style

Preferred shape:

```lua
local helix = require('helix')

local doc = helix.workspace.current_document()
local snapshot = helix.documents.snapshot(doc)

helix.documents.apply_edit {
  document = doc,
  edits = {
    { start = { line = 10, column = 4 }, finish = { line = 10, column = 7 }, text = "foo" },
  },
}
```

Not preferred as the final primary model:

```lua
helix.editor.current_buffer():insert(...)
```

### Lua handle ergonomics

Use mixed ergonomics:

- handles may be opaque userdata or opaque ids
- rich convenience methods are allowed for query/discovery
- mutation should still route through request-shaped functions

That means Lua can still feel pleasant without the contract becoming runtime-specific.

## API/Interface Contract

## Core interfaces

At the Rust host boundary, use capability traits rather than a monolith.

Required host traits:

- `PluginQueryHost`
- `PluginMutationHost`
- `PluginUiHost`
- `PluginPanelHost`
- `PluginCommandHost`
- `PluginEventHost`

### `PluginQueryHost`

Responsibilities:

- current handles
- snapshot retrieval
- handle lookup/listing
- theme/config/workspace metadata

### `PluginMutationHost`

Responsibilities:

- document/view mutation requests
- save/open/focus operations
- annotation/selection updates

### `PluginUiHost`

Responsibilities:

- notifications
- prompt/confirm/picker requests
- clipboard and frontend-dependent UI capabilities

### `PluginPanelHost`

Responsibilities:

- panel registration
- panel lifecycle
- render context provisioning

### `PluginCommandHost`

Responsibilities:

- command registration
- command update/removal
- command invocation

### `PluginEventHost`

Responsibilities:

- subscribe/unsubscribe
- host-defined event catalog and metadata

## Error model

The final public contract must use structured errors.

Required error categories:

- `NotFound`
- `StaleHandle`
- `InvalidRequest`
- `PermissionDenied`
- `UnsupportedCapability`
- `Busy` or `Conflict`
- `InternalHostError`

Avoid:

- generic runtime string errors as the primary public contract
- silent no-ops for unsupported operations

## Versioning and metadata

The host-agnostic contract must have explicit metadata discovery.

Required metadata:

- API version
- compatible version floor
- capability list
- event catalog
- request support matrix
- optional deprecation metadata

Helix should borrow this strongly from Neovim.

## Acceptance Criteria

- [ ] The final plugin API can be described without reference to embedded Lua or direct Rust object access.
- [ ] All public plugin-facing identities are expressed as opaque handles, not debug strings or raw editor structs.
- [ ] Most mutations and all UI effects are represented as explicit request types.
- [ ] Query, mutation, and render capabilities are clearly separated.
- [ ] Event contracts are structured and typed, with registration not primarily based on free-form strings.
- [ ] The API shape remains pleasant enough to expose as a Lua facade without leaking host internals.
- [ ] The contract is compatible with future JSON/msgpack/wasm transport layers without redesigning the semantic model.

## Test Strategy

| Layer | What | How |
|-------|------|-----|
| Unit | Handle/request/event type behavior | Rust unit tests for ids, requests, event conversion, error mapping |
| Integration | Host capability boundaries | Integration tests across `helix-plugin`, `helix-term`, `helix-view` |
| Contract | Metadata/API discovery | Snapshot tests for exported API metadata and event catalogs |
| Lua facade | Public ergonomics | Lua integration tests using the facade against in-process host |
| Future transport | Serialization assumptions | Contract tests that serialize handles/snapshots/requests/events |

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Overdesigning for hypothetical remote hosts | Medium | High | Keep the semantic contract host-agnostic, but defer concrete transport/runtime work |
| Command/request APIs become too flat or verbose | Medium | Medium | Use modules plus handles for discovery/query, not one giant request bag |
| Handle APIs drift back into mutable proxy objects | Medium | High | Enforce the rule: handles are identity, requests are mutation |
| Lua ergonomics become clumsy | Medium | Medium | Keep Lua-specific sugar layered on top of the contract |
| Compatibility pressure drags the design back toward current weak APIs | High | Medium | Accept redesign breakage intentionally and provide migration docs rather than preserving bad shapes |

## Trade-offs Made

| Chose | Over | Because |
|-------|------|---------|
| Host-agnostic contract | Lua-only host API | GUI/web/remote/wasm futures need a stable boundary |
| Hybrid handle + request model | Pure handle mutation | Requests are better for transport, capability, validation, and async side effects |
| Capability traits | Single giant host trait | Responsibilities are real and separable |
| Structured metadata/versioning | Ad hoc dynamic discovery | Public APIs need compatibility discipline |
| Lua facade over contract | Lua as the contract | Keeps API portable and better typed |

## Final Recommendation

Helix should redesign the plugin system around a host-agnostic contract with:

- opaque handles for resources
- snapshots for state
- explicit requests for mutation/effects
- typed events for observation
- explicit capability objects for host access
- Lua as one frontend over that contract, not the definition of it

This is the cleanest path to a plugin API that can remain pleasant locally while still surviving GUI, web, SSH-remote, JSON/RPC, and wasm futures.

## Open Questions

- [ ] Should panel rendering stay as a special capability family or be generalized into a host “view/widget” API? → Owner: maintainers
- [ ] Should command registration be part of the plugin contract core or an optional host capability? → Owner: maintainers
- [ ] Which public handles need stable cross-session identity, if any? → Owner: maintainers

## Success Metrics

- The plugin API can be documented independently of the current in-process Lua runtime.
- A future non-Lua host could implement the same semantic contract without changing the public model.
- The number of plugin APIs that require “current focused buffer/view only” semantics drops substantially.
- Plugin-facing errors and events become easier to version and test.

---

Phase: DONE
Type: architecture decision / feature plan
Effort: XL
Status: Ready for task breakdown

Discovery:

- Explored: `helix-plugin`, `helix-term` plugin host wiring, `helix-view` editor ownership helpers, Neovim API docs, WezTerm Lua docs
- Key findings: Helix already improved internal safety and typing, but still needs a final public contract that is host-agnostic and capability-based

Recommendation:

- adopt a host-agnostic hybrid plugin API: handles for identity/query, requests for mutations/effects, typed events, capability objects, Lua facade on top

Key Trade-offs:

- choosing a stronger final contract over preserving current Lua bridge semantics
- accepting some verbosity in exchange for portability, safety, and versioning discipline

Deliverables (Ordered):

1. final host-agnostic contract and metadata model
2. handle/snapshot/request/event public types
3. capability host interfaces
4. Lua facade design over the contract
5. migration plan from the current embedded bridge
