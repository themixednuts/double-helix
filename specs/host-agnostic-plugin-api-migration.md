# Host-Agnostic Plugin API Migration

**Status:** Drafted from accepted final-state API direction
**Effort:** XL
**Date:** 2026-04-10

## Purpose

This document maps the current `helix-plugin` system to the final-state host-agnostic plugin API defined in `specs/host-agnostic-plugin-api.md`.

It answers:

- how to move from the current embedded Lua bridge to the target contract
- what crates should own each part of the new design
- what should be built first versus deferred
- what compatibility shims are acceptable temporarily
- what traps to avoid during migration

## Current to Target Mapping

| Current | Target | Notes |
|---|---|---|
| `helix.editor.*` | `helix.workspace`, `helix.documents`, `helix.views`, `helix.commands` | Split by concern; keep only thin convenience wrappers in `editor` if needed during migration |
| `LuaBuffer` | `DocumentHandle` + `DocumentSnapshot` + document requests | Query and mutation must stop being bundled together |
| `LuaWindow` | `ViewHandle` + `ViewSnapshot` + view requests | Same split as documents |
| `UiHandler` request structs | `PluginUiHost` capability | Already close; keep and generalize |
| `PluginEvent` enum | final typed event catalog | Keep as source of truth and expand toward metadata-backed event definitions |
| callback registry ids | internal registration/runtime detail | Keep internal, but do not let them define public API shape |
| focused-buffer mutation helpers | convenience-only sugar | Not the core model in the final contract |
| thread-local context bridge | host-specific implementation detail | Replace as the primary model with capability/context objects |

## Ownership Model by Crate

### `helix-plugin`

Should own:

- public plugin contract types
- handles, snapshots, requests, events, errors
- host capability traits
- language-host adapters such as Lua
- contract metadata definition and exposure

Should not own:

- editor internals
- terminal compositor logic
- frontend-specific panel implementation details

### `helix-view`

Should own:

- canonical editor semantics
- editor/query/mutation operations used to implement plugin requests
- conversion from internal editor state to public snapshots

Should not own:

- plugin runtime concerns
- plugin registration
- transport/runtime metadata concerns

### `helix-term`

Should own:

- frontend host implementations for UI/panels/rendering
- routing plugin UI requests into terminal runtime/application flow
- terminal-specific capability implementations

Should not own:

- plugin contract type definitions
- editor semantic ownership

## Migration Principles

1. The host-agnostic contract becomes the canonical model before new public API growth happens.
2. The Lua facade is an adapter over the contract, not the design authority.
3. Handles are identity only.
4. Requests are the primary mutation mechanism.
5. Snapshots are immutable and serializable.
6. Event registration must become metadata-backed, not free-form string-first.
7. Frontend-specific features remain capability-gated.

## Phased Migration

## Phase 1: Freeze the contract surface direction

Goal:

- stop adding new public plugin APIs in the old style

Deliverables:

- adopt `specs/host-agnostic-plugin-api.md` as the design authority
- mark the current Lua API as transitional
- add links from plugin docs/spec docs to the host-agnostic spec

Exit criteria:

- new plugin API work references the host-agnostic contract
- no new `helix.editor.*` style catch-all growth

## Phase 2: Define the public contract types in Rust

Goal:

- create the canonical contract layer inside `helix-plugin`

Deliverables:

- `handles.rs`
- `snapshots.rs`
- `requests.rs`
- `events.rs`
- `errors.rs`
- `metadata.rs`

Required public type families:

- `DocumentHandle`, `ViewHandle`, `PanelHandle`, `CommandHandle`, `SubscriptionHandle`
- `DocumentSnapshot`, `ViewSnapshot`, `WorkspaceSnapshot`, `ThemeSnapshot`, `DiagnosticSnapshot`
- `ApplyEditRequest`, `SaveDocumentRequest`, `FocusViewRequest`, `PromptRequest`, `PanelRegistration`, `RunCommandRequest`
- typed event structs/enums
- structured error enum
- metadata schema

Exit criteria:

- contract types can be explained independently of Lua or terminal implementation details
- type definitions do not depend on `helix-term`

## Phase 3: Introduce capability traits as the actual host boundary

Goal:

- replace “ambient editor bridge” as the conceptual host model

Deliverables:

- `PluginQueryHost`
- `PluginMutationHost`
- `PluginUiHost`
- `PluginPanelHost`
- `PluginCommandHost`
- `PluginEventHost`

Implementation notes:

- keep traits narrow and concern-specific
- do not introduce one giant `PluginHost`
- capability objects may be combined in concrete host impls, but not at the public trait boundary

Exit criteria:

- `helix-term` can implement frontend-dependent capabilities cleanly
- `helix-view` semantics are consumed via editor-owned methods, not field reach-through

## Phase 4: Build snapshot adapters from `helix-view`

Goal:

- convert internal editor state into stable public snapshot types

Deliverables:

- snapshot builders/adapters in `helix-plugin` or a tightly scoped adapter module
- editor-owned methods in `helix-view` to support the needed conversions

Rules:

- snapshot builders must not leak internal types
- snapshots are transport-safe and immutable
- focused-document/view helpers remain convenience-only

Exit criteria:

- documents/views/workspace/theme/diagnostics can be surfaced without exposing internal editor structs

## Phase 5: Migrate the Lua facade to the new contract

Goal:

- make Lua a facade over the final contract rather than a separate design

Deliverables:

- `require('helix')` facade shaped around:
  - `workspace`
  - `documents`
  - `views`
  - `panels`
  - `commands`
  - `ui`
  - `events`
  - `log`
- temporary compatibility layer for old `helix.editor`, `helix.buffer`, `helix.window` entry points if needed

Migration notes:

- handle-returning APIs should become normal
- request-based mutation APIs should become primary
- old object-mutation style should be deprecated

Exit criteria:

- a plugin can be written primarily against the new facade without using the old bridge shape

## Phase 6: Replace string-first event registration

Goal:

- event registration should use stable event kinds backed by metadata

Deliverables:

- event kind catalog
- metadata exposure for supported events
- Lua registration helpers that map ergonomic names to stable event identifiers

Good target shape:

- `helix.events.subscribe(helix.events.kind.DocumentChanged, handler)`

Transitional allowance:

- string aliases may continue to work temporarily, but should be wrappers over the stable catalog

Exit criteria:

- core event registration no longer depends on undocumented free-form strings

## Phase 7: Replace focused-buffer-only mutation as the primary story

Goal:

- move from “current focused buffer” semantics to explicit handle-based requests

Deliverables:

- `documents.apply_edit { document = handle, ... }`
- `documents.set_selection { document = handle, view = optional_view, ... }`
- `documents.annotations { document = handle, ... }`

Keep as sugar only:

- `workspace.current_document()`
- maybe `workspace.current_view()`

Exit criteria:

- important document mutations can target explicit handles, not only the active editor focus

## Phase 8: Metadata and versioning

Goal:

- make the plugin API discoverable and versionable

Deliverables:

- API version
- compatibility level
- supported capabilities
- request catalog
- event catalog
- deprecation markers

Borrow strongly from Neovim here.

Exit criteria:

- a future external host can inspect the supported contract rather than guessing

## Phase 9: Remove the old bridge as the design center

Goal:

- old helper shims stop being the primary architecture

Deliverables:

- deprecate old Lua modules/shapes that conflict with the final contract
- remove old compatibility paths when migration is complete

Exit criteria:

- the final public API is the host-agnostic contract, not the embedded bridge with some wrappers

## Public API Migration Map

| Current API | Final API |
|---|---|
| `helix.editor.mode()` | `helix.workspace.mode()` or `workspace_snapshot.mode` |
| `helix.editor.get_cursor()` | `helix.documents.cursor(document_handle, view_handle?)` or snapshot query |
| `helix.editor.set_cursor(...)` | `helix.documents.set_selection(SetSelectionRequest)` |
| `helix.editor.open(...)` | `helix.documents.open(OpenDocumentRequest)` |
| `helix.editor.save()` | `helix.documents.save(SaveDocumentRequest)` |
| `helix.window.get_current()` | `helix.workspace.current_view()` |
| `helix.buffer.get_current()` | `helix.workspace.current_document()` |
| `buffer:insert(...)` | `helix.documents.apply_edit(ApplyEditRequest)` |
| `buffer:delete(...)` | `helix.documents.apply_edit(ApplyEditRequest)` |
| `buffer:set_annotations(...)` | `helix.documents.set_annotations(SetAnnotationsRequest)` |
| `helix.ui.prompt(...)` | `helix.ui.prompt(PromptRequest)` |
| `helix.ui.panel(...)` | `helix.panels.register(PanelRegistration)` |
| `helix.on("buffer_open", ...)` style registration | `helix.events.subscribe(EventKind.DocumentOpened, ...)` |

## Traps to Avoid

## Neovim traps

- do not let event/request signatures become giant dynamic tables without stronger schema discipline
- do not let a flat function namespace become the only shape of the API
- do not make transport details the public mental model

## WezTerm traps

- do not let object methods become mutable live proxies to host state everywhere
- do not make callback shape the only organizing principle of the API
- do not let frontend/runtime-specific object models define the core contract

## Helix-specific traps

- do not keep “focused document only” as the implicit mutation model
- do not let Lua convenience dictate the contract type system
- do not let temporary compatibility shims become permanent architecture
- do not hide capability restrictions behind runtime errors when they can be modeled explicitly

## Recommended Immediate Next Deliverables

1. carve out the canonical contract modules in `helix-plugin`
2. define the first stable handle/snapshot/request/event/error types
3. define capability trait boundaries
4. sketch the `require('helix')` final Lua facade against those types

## Acceptance Criteria

- [ ] The migration path is ordered and each phase has a clear exit condition.
- [ ] The target contract can be implemented by terminal, GUI, or web hosts without changing the semantic API.
- [ ] The final API no longer depends on focused-buffer-only semantics as the core model.
- [ ] Compatibility shims are explicitly transitional, not the target design.
- [ ] The contract modules and capability traits have clear crate ownership.

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Migration stretches too long and old/new APIs both linger | High | High | Time-box compatibility periods and mark deprecated APIs clearly |
| Too much abstraction lands before enough real APIs are shaped | Medium | High | Define public contract types and 2-3 real end-to-end flows first |
| Frontend-specific features distort the core contract | Medium | Medium | Gate them as optional capabilities |
| Plugin authors lose ergonomics during transition | Medium | Medium | Build the Lua facade early over the final contract |

## Success Metrics

- A new plugin can be authored against the final facade without using old bridge-shaped APIs.
- The plugin contract can be documented without mentioning thread-local editor context.
- At least one future non-terminal host could implement the semantic contract in principle.
