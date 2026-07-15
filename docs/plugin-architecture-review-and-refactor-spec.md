# Plugin Architecture Review and Refactor Spec

Status: historical migration record. The current supervised host architecture is
documented in `book/src/plugins.md`, `docs/runtime-architecture.md`, and
`docs/responsive-application-architecture.md`.

Current code has moved the host-agnostic contract into `helix-plugin-api` and
`adapt.rs` / `bridge.rs` into the dedicated `helix-plugin-editor` integration
crate. References below to those modules under `helix-plugin/src/contract/`
describe the pre-migration layout.

This document turns the current `helix-plugin` review into a concrete target architecture and implementation sequence.

It is guided by:

- `docs/crate-abstractions-and-event-buses-review.md`
- `docs/runtime-executor-architecture-spec.md`
- the Rust skill guidance on traits, semantic types, and typestate

The goal is not to make the plugin system more abstract in the abstract. The goal is to make it safer, more structured, more extensible, and easier to evolve without widening crate boundary debt.

## Summary

At the time of this review, the plugin system still used an embedded Lua bridge.
That implementation has since been removed from the editor process.

The biggest remaining problems are:

1. plugin callbacks access editor and render state through thread-local raw pointers
2. plugin APIs are still only partially capability-oriented
3. some plugin-facing handles are still debug-string or raw-id shaped
4. event registration on the Lua side is still string-driven
5. some plugin buffer operations are still intentionally limited to the focused document rather than handle-oriented mutation

The right end state is a typed capability host:

- read-only contexts are read-only by construction
- mutation happens through explicit request APIs
- plugin-facing handles are opaque and stable
- UI requests use semantic structs instead of long positional method calls
- event delivery stays typed on the Rust side and structured at the Lua boundary

## Current Architecture

Primary files:

- `helix-plugin/src/lib.rs`
- `helix-plugin/src/types.rs`
- `helix-plugin/src/lua/mod.rs`
- `helix-plugin/src/lua/api/facade.rs` — sole Lua API surface (16 modules, ~2500 lines)
- `helix-plugin/src/contract/` — host-agnostic plugin contract (handles, snapshots, requests, events, errors, metadata, host traits, adapt, bridge, codec)
- `helix-term/src/plugin_registry.rs`
- `helix-term/src/runtime/ui/plugin.rs`
- `helix-term/src/ui/plugin_panel.rs`
- `helix-term/src/effect/plugin.rs`

Note: old per-module files (`editor.rs`, `buffer.rs`, `ui.rs`, `window.rs`, `lsp.rs`, `layout.rs`, `surface.rs`, `log.rs`) have been deleted — all API lives in `facade.rs`.

Current strengths:

- the former `PluginManager` owned plugin load, event fire, command lookup, and UI callback delivery
- `PluginEvent` (contract) is the single typed Rust event enum with 20 event kinds and structured payloads
- `DrawSurface` keeps `helix-plugin` independent of `helix-tui`
- plugin panels use a lightweight model/compositor split via `ContentModel` trait
- read-only and mutating editor access are split (`EditorQueryBridge` vs `EditorMutationBridge`)
- 12 host traits define explicit capability boundaries (query, mutation, UI, panels, commands, events, splits, tabs, floats, workspace detail, assistant query, assistant mutation)
- all Lua API through a single facade with 17 modules
- typed event subscription via `EventKind` enum and `helix.events.kind` constants table
- `UiHandler` uses request structs, callback ids wrapped in `UiCallbackId`
- trait upcasting eliminates all `as_any()` boilerplate in model and compositor

Current weaknesses:

- ambient mutable editor access via `get_editor_mut()` (thread-local raw pointer with RAII guard)
- thread-local context for editor, surface, and theme requires one lifetime-erasure transmute
- some plugin APIs are still narrower than their names suggest, especially focused-buffer mutation paths
- frontend host traits (`PluginUiHost`, `PluginPanelHost`, `PluginCommandHost`, `PluginEventHost`) still use the legacy `UiHandler`/callback system rather than the contract traits
- panel registration still goes through the legacy callback mechanism rather than `PluginPanelHost`

## Concrete Findings

## 1. The current editor-context bridge was the main safety problem

`helix-plugin/src/lua/mod.rs` stores editor, surface, and theme in thread-local raw pointers.

That first-phase problem has now been partially fixed.

Current behavior:

- `with_editor_context(&mut Editor, f)` installs a mutable editor pointer for callback duration
- `with_editor_context_ref(&Editor, f)` now installs a read-only query context
- `get_editor()` provides immutable access
- `get_editor_mut()` is rejected from query-only contexts

That removes the worst render-time mutation hazard, even though the bridge still relies on thread-local context rather than explicit borrowed host objects.

This is especially visible in `helix-term/src/ui/plugin_panel.rs`, where render callbacks run through:

1. `with_render_context(...)`
2. `with_editor_context_ref(cx.editor, ...)`
3. Lua callback execution

This work is now landed. The next step is to replace more ambient context usage with explicit host/context objects over time.

## 2. The plugin API surface is not capability-oriented yet

Examples:

- `helix-plugin/src/lua/api/buffer.rs` still exposes focused-buffer-only mutation semantics
- `helix-plugin/src/lua/api/window.rs` and `editor.rs` still expose ids as strings in several places
- `helix-plugin/src/lua/api/lsp.rs` still offers a very thin client-list surface
- the plugin Lua layer still constructs many capabilities by reading globals and app data ad hoc

Important cleanup already landed here:

- direct plugin writes to `editor.mode` were replaced by editor-owned mode methods
- direct plugin writes to `editor.tree.focus` were replaced by `Editor::focus(view_id)`
- many direct reads of `tree`, `documents`, `language_servers`, and `registers` were replaced by editor-owned helpers
- focused selection, cursor, undo/redo, and close paths now go through editor-owned methods

## 3. UI requests were structurally weak

This problem is now addressed in the first refactor pass.

`UiHandler` now uses named request structs instead of positional methods, including:

- `PromptRequest`
- `ConfirmRequest`
- `PickerRequest`
- `PanelRegistration`
- `PanelRemoval`

This is a materially better API boundary and should stay.

## 4. Plugin callback routing is serviceable but too string and id heavy

Current patterns:

- event handlers are stored as `HashMap<EventType, Vec<(String, RegistryKey)>>`
- UI callbacks are stored as `HashMap<PluginCallbackKey, RegistryKey>`
- panels are tracked by `plugin_name`, `panel_id`, `render_callback_id`, and `event_callback_id`

This is workable as an internal registry, but it should not shape the public plugin abstraction more than necessary.

## 5. Some APIs are not real semantic operations yet

Examples in `helix-plugin/src/lua/api/editor.rs` before cleanup:

- `move_cursor` is a TODO stub
- `save`, `save_all`, `quit`, and `focus` were not yet integrated as real semantic host operations

Current status:

- `save`, `save_all`, and `quit` now route through the real builtin command registry
- `focus` now reports the actual focused view id
- `move_cursor` now fails honestly instead of pretending to succeed

## Comparison: Neovim

Neovim is useful as a reference point, not as a blueprint.

Relevant properties from stable:

1. remote plugins run out of process over msgpack-RPC
2. plugin hosts are language-specific and loaded lazily
3. plugin declarations are materialized through a manifest
4. the public API uses typed opaque handles such as `Buffer`, `Window`, and `Tabpage`
5. API metadata is discoverable and versioned
6. buffer event subscriptions are explicit and structured
7. fast callback contexts are explicitly restricted from mutating editor state directly

Helix should not copy the remote-host model, but it should borrow these lessons:

- host boundaries should be explicit
- plugin-facing resources should use typed handles
- read-only and mutating contexts should be separated by API design
- event and request contracts should be structured and extensible
- callback context restrictions should be enforced by architecture, not comments

## Target Architecture

## Design goals

The refactor should produce these properties:

1. no ambient mutable editor access from plugin render callbacks
2. read-only versus mutating plugin operations are separated
3. plugin-facing APIs use semantic editor methods, not raw field access
4. UI requests use named structs and typed handles where appropriate
5. internal registries may still use ids, but ids should not dominate the public shape
6. the design remains frontend-aware without baking in terminal-only assumptions where unnecessary

## Rust API design rules

This design should follow a few explicit Rust rules.

### Use newtypes for plugin-facing handles and ids

Do not keep relying on raw `u64`, debug strings, or ad hoc `(String, u64)` pairs as the conceptual API.

Preferred shape:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginDocumentHandle(helix_view::DocumentId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginViewHandle(helix_view::ViewId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginPanelHandle(u64);
```

These are cheap, `Copy`, and remove type confusion with zero meaningful runtime cost.

### Prefer enums over parallel discriminant-plus-payload structs

This change is now landed.

Preferred shape:

```rust
#[derive(Debug, Clone)]
pub enum PluginEvent {
    Init,
    Ready,
    BufferOpen(BufferOpenEvent),
    BufferPreSave(BufferSaveEvent),
    BufferPostSave(BufferSaveEvent),
    BufferClose(BufferCloseEvent),
    BufferChanged(BufferChangedEvent),
    ModeChange(ModeChangeEvent),
    KeyPress(KeyPressEvent),
    LspAttach(LspAttachEvent),
    LspDiagnostic(LspDiagnosticEvent),
    SelectionChange(SelectionChangeEvent),
    ViewChange(ViewChangeEvent),
}
```

The current code now follows this direction using a single `PluginEvent` enum with typed variants. `EventType` remains as the registration key and Lua-facing name mapping, not as a parallel payload discriminator.

If API evolution pressure is high, nested payload structs or `#[non_exhaustive]` can be used on public event structs or enums where appropriate.

### Use traits only at real capability boundaries

Traits are appropriate for host capabilities because there are real implementation boundaries between `helix-plugin` and frontend crates.

Traits are not needed for everything inside the plugin engine.

Good trait use:

- `PluginUiHost`
- `PluginCommandHost`
- `PluginQueryHost`
- `PluginMutationHost`
- `PluginPanelHost`

Bad trait use:

- genericizing `LuaEngine` over every host concern
- introducing one giant `PluginHost` trait with dozens of methods
- parameterizing editor-facing value types over traits just to avoid concrete names

### Use typestate only for staged APIs

Typestate is appropriate for:

- query versus mutate context access
- builder-style registration with required fields
- panel or command handles that move from registering to registered states

Typestate is not appropriate for:

- long-lived plugin runtime state
- general editor state
- event payload categories

Those should stay enum- or request-based.

### Keep generic bounds local

If builders or context wrappers use generics, put bounds on impl blocks and methods, not on storage structs unless the storage really requires them.

That keeps the types easier to read and prevents generic noise from spreading through the whole crate.

## Core split: contexts and capabilities

The host should be structured around explicit contexts.

The most Rust-idiomatic version of this is likely one shared context type with an access-state marker, plus a separate render wrapper.

Sketch:

```rust
mod access {
    pub trait Mode: sealed::Sealed {}
    pub struct Query;
    pub struct Mutate;

    mod sealed {
        pub trait Sealed {}
        impl Sealed for super::Query {}
        impl Sealed for super::Mutate {}
    }

    impl Mode for Query {}
    impl Mode for Mutate {}
}

pub struct PluginContext<'a, M: access::Mode> {
    plugin: PluginId,
    _mode: PhantomData<M>,
    // borrowed host pieces live here
}
```

This gives zero-cost compile-time separation without turning the whole engine into a generic maze.

### A. `PluginQueryContext`

Purpose:

- read-only access to editor state
- document, view, and panel lookup
- config, theme, and layout inspection

Rules:

- no mutation methods
- safe to expose during render callbacks

This can be modeled either as its own concrete type or as `PluginContext<'a, Query>`.

### B. `PluginMutationContext`

Purpose:

- semantic editor mutations
- command execution
- focus changes
- document edits through explicit host methods

Rules:

- only available in mutating callback paths
- should call semantic editor APIs, not expose direct field mutation

This can be modeled either as its own concrete type or as `PluginContext<'a, Mutate>`.

### C. `PluginRenderContext`

Purpose:

- draw on a provided `DrawSurface`
- inspect theme and panel geometry
- optionally inspect read-only editor snapshot through query context

Rules:

- no access to mutation APIs
- no ambient `get_editor_mut()` compatibility path

Recommended shape:

```rust
pub struct PluginRenderContext<'a> {
    pub area: helix_view::graphics::Rect,
    pub surface: &'a mut dyn DrawSurface,
    pub query: PluginContext<'a, access::Query>,
}
```

This is a good typestate use because render-versus-mutate is a real staged distinction with clear API consequences.

### D. `PluginUiHost`

Purpose:

- prompts
- confirms
- pickers
- panel registration and removal

Rules:

- request-oriented
- frontend implementation lives in `helix-term`

## Plugin-facing handles

Plugin-facing objects should remain opaque and typed.

Good candidates:

- `PluginDocumentHandle`
- `PluginViewHandle`
- `PluginPanelHandle`
- `PluginCommandHandle`

Internal-only ids can still exist for callback routing, but they should also be newtypes:

- `RenderCallbackId`
- `UiCallbackId`
- `EventCallbackId`

These do not need to become complex wrappers immediately. The important part is that plugins stop reasoning about raw editor internals or debug-string ids as the main API.

## Semantic host requests

UI and mutation paths should move toward named request types.

Recommended initial request structs:

- `PromptRequest`
- `ConfirmRequest`
- `PickerRequest`
- `PanelRegistration`
- `PanelRemoval`
- `OpenDocumentRequest`
- `SaveDocumentRequest`
- `SetModeRequest` only if mode switching remains exposed

This aligns with the broader codebase cleanup away from positional protocols.

For request types with many optional fields, use a normal builder first. Only move to typestate builders where required fields are few, clear, and semantically important.

Good candidate:

- `PanelRegistrationBuilder<Title, Render>` if panel registration grows beyond a handful of required fields

Not necessary initially:

- typestate builders for every prompt or picker request

## Event model

The Rust side now uses a closed `PluginEvent` enum.

Target properties:

- one Rust source of truth for event kinds
- Lua receives structured event tables generated from typed Rust event data
- string event names can remain as Lua-facing aliases, but they should not become the authoritative model

Implemented direction:

1. `PluginEvent` is the single typed event enum carrying payloads
2. `PluginEvent::event_type()` and `PluginEvent::name()` bridge to registration and Lua-facing names
3. emitters now construct typed event variants directly

This already improved correctness and makes event additions easier to review.

## Recommended Trait Shape

The current `UiHandler` and `EditorCommandRegistry` are a start, but the capability split should be clearer.

Recommended host traits:

- `PluginCommandHost`
- `PluginUiHost`
- `PluginPanelHost`
- `PluginQueryHost`
- `PluginMutationHost`

One practical shape is to keep `PluginContext<'a, M>` as the primary API object and use traits for the host services it delegates to internally.

That gives:

- object-safe host boundaries at crate seams
- concrete, borrow-based context types at call sites
- no need to genericize the entire `LuaEngine`

These can still be implemented by a smaller number of concrete host structs in `helix-term`, but the trait boundaries should reflect actual responsibilities.

Do not over-genericize this. The goal is capability separation, not trait proliferation.

## Where typestate helps

Typestate should be used sparingly.

Good candidates:

- panel registration builder transitioning to a registered panel handle
- command registration builder transitioning to installed command metadata
- explicit read-only render context versus mutating callback context

Good use of sealed state markers:

- `PluginContext<'a, Query>`
- `PluginContext<'a, Mutate>`

Bad candidates:

- long-lived plugin runtime state
- general editor state exposed to plugins

Those should remain request- or enum-based.

## Phased Implementation Plan

## Phase 1: eliminate the unsafe render-mutation bridge

Status: mostly completed

Goal:

- render callbacks must not be able to call generic mutation accessors

Concrete changes:

1. split `get_editor_mut()` into distinct query and mutation access paths
2. stop using a mutable editor pointer for `with_editor_context_ref`
3. introduce newtype ids for callback and panel routing that are currently raw integers or string pairs
4. make read-only APIs in Lua use a query context instead of the mutation accessor where possible

Success criteria:

- plugin render callbacks cannot obtain a mutable editor reference
- the UB caveat in `with_editor_context_ref` disappears
- render and callback routing types stop depending on raw primitive ids in public-facing code

Current status:

- completed for `get_editor()` / `get_editor_mut()` split
- completed for `with_editor_context_ref`
- completed for `UiCallbackId` and `PluginCallbackKey`
- completed for many read-only Lua APIs
- still not fully modeled as explicit borrowed context objects

## Phase 2: move editor API calls behind semantic editor methods

Status: partially completed

Goal:

- no plugin API writes raw editor fields directly

Concrete changes:

1. replace `editor.mode = ...` with semantic mode-setting helpers in `helix-view`
2. replace direct `editor.tree.focus` writes with semantic focus helpers
3. replace raw language-server traversal or document assumptions with editor-owned query helpers
4. split read-only buffer/window APIs from mutation-oriented APIs in the Lua bridge

Success criteria:

- plugin Lua APIs stop depending on editor field layout
- plugin query paths no longer require mutable editor access

Current status:

- completed for mode and focus operations
- completed for many window/list/register/lsp query paths
- completed for focused cursor/selection/undo/redo/select-all helpers
- completed for focused-buffer mutation checks being moved into editor-owned helpers
- still incomplete for a truly handle-oriented document/view mutation API

## Phase 3: convert UI host methods to request structs

Status: completed

Goal:

- make UI calls extensible and self-documenting

Concrete changes:

1. replace positional `UiHandler` methods with named request structs
2. update `helix-term/src/plugin_registry.rs` to translate those requests into ingress events
3. update `helix-term/src/runtime/ui/plugin.rs` and `effect/plugin.rs` accordingly
4. add builders only where request construction becomes materially noisy

Success criteria:

- panel, prompt, confirm, and picker requests have semantic types

## Phase 3.5: contract API and Lua facade expansion

Status: completed

Goal:

- extend the contract and Lua facade with split, tab, float, and panel topology APIs
- provide bridges from contract host traits to editor internals
- unify model traits and eliminate unnecessary `as_any()` boilerplate

Concrete changes:

1. contract types: `handles.rs` (6 handle types), `snapshots.rs` (split tree, tab group, float, panel, focus target, workspace detail), `requests.rs` (split/tab/float/panel/resize requests), `events.rs` (20 event kinds including split/tab/float/panel events)
2. host traits: `PluginSplitHost`, `PluginTabHost`, `PluginFloatHost`, `PluginPanelHost`, `PluginWorkspaceQueryHost` — all defined with full method signatures
3. bridge implementations: `EditorQueryBridge` (split tree, tab listing, workspace detail), `EditorMutationBridge` (split/tab/float operations)
4. Lua facade modules: `helix.splits` (split/focus_direction/swap/transpose/resize/tree/list), `helix.tabs` (open/close/focus/next/previous/list), `helix.floats` (create/close/list, LuaFloatId userdata with close/update methods), enhanced `helix.ui` panel API (toggle_panel/focus_panel/resize_panel/panels)
5. model unification: `LayerModel`/`PanelModel`/`FloatModel` → single `ContentModel` trait, `impl_content_model!` macro, trait-upcasting-based downcasting (no `as_any()`)
6. compositor: `AnyComponent` trait removed, `Component: Any + Send` uses trait upcasting directly
7. unsafe reduction: `RawSurfacePtr` decompose/reconstruct eliminated, reduced to single lifetime-erasure transmute in thread-local surface storage

Primary files:

- `helix-plugin/src/contract/` — all submodules (handles, snapshots, requests, events, errors, metadata, host, adapt, bridge, codec)
- `helix-plugin/src/lua/api/facade.rs` — sole Lua API surface (16 modules)
- `helix-view/src/model/mod.rs` — `ContentModel` trait with trait upcasting
- `helix-term/src/compositor.rs` — `impl dyn Component` with trait upcasting
- `helix-plugin/src/lua/mod.rs` — simplified unsafe surface storage

Success criteria:

- all 9 host traits defined with complete method signatures
- bridges compile and connect contract to editor internals
- Lua facade exposes all 16 modules with structure tests (68 tests total)
- zero `as_any()`/`AnyComponent` definitions remain in the codebase
- single `ContentModel` trait replaces three identical model traits

## Phase 4: narrow the public plugin surface

Goal:

- prefer fewer strong APIs over many weak ones

Concrete changes:

1. either implement or remove stubby APIs such as `move_cursor`, `save_all`, `quit`, and `focus`
2. keep only operations that can be backed by semantic host behavior
3. make document and window handles more explicit and less dependent on the current focused item

Success criteria:

- the public plugin API surface matches real supported semantics

## Phase 5: improve typed registration and callback routing

Status: partially completed

Goal:

- reduce callback-id protocol leakage in the public design

Concrete changes:

1. keep internal callback registries if useful, but wrap them behind typed registration helpers
2. consider typed panel and command handles returned from registration
3. migrate event storage toward a single typed `PluginEvent` model
4. make event subscription registration more explicit in Rust, while preserving ergonomic Lua entry points

Success criteria:

- ids remain an internal implementation detail more than a public mental model
- invalid event kind and payload combinations become unrepresentable

Current status:

- completed for `PluginEvent` as the typed event model (26 event kinds including 6 assistant events)
- completed for typed callback ids internally
- completed for contract event subscription model (`EventKind` enum + `PluginEventHost` trait)
- Lua event registration uses typed `EventKind` constants via `helix.events.kind` table
- not yet completed for typed registration handles returned to plugins

## Performance notes

This design should not regress runtime behavior just to look cleaner.

Guidelines:

- prefer `Copy` newtypes over heap-owning handle wrappers
- typestate markers should remain zero-sized via `PhantomData`
- keep context objects borrowed and stack-local
- use trait objects at crate seams, not in every internal helper path
- keep hot-path dispatch data concrete where possible

Potential low-cost improvements during refactor:

- `SmallVec` for per-event handler lists if most events have few subscribers
- newtyped callback ids backed by `NonZeroU64` if we want denser `Option` storage
- fewer temporary strings in event and id plumbing once typed handles replace debug formatting

The target is better correctness and API quality at near-zero or negligible runtime overhead.

## Non-goals

This refactor should not try to do these all at once:

- replace Lua with a different plugin language
- adopt Neovim's remote-plugin transport model wholesale
- invent a global host abstraction for hypothetical GUI frontends before there is a concrete need
- genericize `Editor` or `Application`

## Phase 3.6: assistant plugin API

Status: completed

Goal:

- expose the assistant/AI system to plugins through the contract and Lua facade
- add assistant events to the plugin event system
- add command aliases for daily-use assistant commands

Concrete changes:

1. contract events: 6 new `EventKind` variants (`AssistantThreadCreated`, `AssistantThreadClosed`, `AssistantRunStarted`, `AssistantRunCompleted`, `AssistantMessageReceived`, `AssistantContextChanged`) with structured payloads — event system now has 26 kinds
2. contract handles: `ThreadHandle` added via `define_handle!` macro
3. contract snapshots: `AssistantSnapshot`, `AssistantThreadSnapshot`, `AssistantEntrySnapshot`, `AssistantContextSnapshot`, `AssistantRunState`, `AssistantFollowState`
4. host traits: `PluginAssistantQueryHost` (assistant_snapshot, thread_snapshot, thread_entries, thread_context) and `PluginAssistantMutationHost` (submit_prompt, cancel_thread)
5. adapt converters: `thread_id_to_raw`, `resolve_thread_id`, `assistant_snapshot`, `assistant_thread_snapshot`, `assistant_entries_snapshot`, `assistant_context_snapshot`, `run_to_contract`, `follow_to_contract`, `entry_kind_to_contract`, `context_kind_to_contract`
6. bridge: `PluginAssistantQueryHost` implemented on `EditorQueryBridge`
7. Lua facade: `helix.assistant` module with 7 functions (snapshot, thread, entries, context, is_ready, active_thread, thread_count)
8. command aliases: 12 new aliases for assistant commands (e.g., `:follow`, `:scratch`, `:attach-sel`, `:attach-file`, `:detach`, `:new-thread`, `:close-thread`, etc.)

Primary files:

- `helix-plugin/src/contract/events.rs` — 6 new event kinds and payloads
- `helix-plugin/src/contract/handles.rs` — `ThreadHandle`
- `helix-plugin/src/contract/snapshots.rs` — assistant snapshot types
- `helix-plugin/src/contract/host.rs` — `PluginAssistantQueryHost`, `PluginAssistantMutationHost`
- `helix-plugin/src/contract/adapt.rs` — assistant type converters
- `helix-plugin/src/contract/bridge.rs` — query bridge impl
- `helix-plugin/src/lua/api/facade.rs` — `helix.assistant` module (17th module)
- `helix-term/src/commands/typed.rs` — 12 new command aliases

Success criteria:

- plugins can observe assistant state: list threads, read entries, check run state
- plugins can subscribe to assistant events: thread creation, run start/complete, message received, context changes
- all 68+ tests pass
- command aliases reduce typing for common assistant operations

## Recommended Next Slice

The next meaningful step is Phase 4: narrowing the public plugin surface.

Why:

- Phases 1–3.6 are complete — the safety bridge, semantic methods, request structs, contract types, host traits, bridge implementations, full Lua facade, and assistant plugin API are all landed
- the remaining stubs and focused-only mutation paths are the main gap between "the API exists" and "the API is correct and complete"
- typed registration handles (Phase 5) will follow naturally once the surface is clean

## Bottom Line

`helix-plugin` has progressed from the weakest crate boundary to a well-structured capability-oriented architecture:

What's landed:

- 12 host traits with full method signatures (query, mutation, UI, panels, commands, events, splits, tabs, floats, workspace detail, assistant query, assistant mutation)
- contract types: 8 handle types, 25+ request structs, 17+ snapshot types, 26 event kinds with structured payloads
- bridges connecting all editor-side contract traits to editor internals
- single `ContentModel` trait with trait-upcasting-based downcasting (no `as_any()`)
- Lua facade: 17 modules as the sole API surface, with 68+ tests
- assistant system fully observable from plugins (threads, entries, context, events)
- read-only and mutating contexts separated by API design
- thread-local unsafe reduced to a single lifetime-erasure transmute

What remains:

- narrowing the public surface (Phase 4) — remove stubs, complete handle-oriented mutation
- typed registration handles for plugins (Phase 5) — panels/commands return opaque handles
- frontend host trait implementations in `helix-term` (PluginUiHost, PluginPanelHost, PluginCommandHost, PluginEventHost) — currently these use the legacy `UiHandler`/callback system
- assistant mutation bridge in `helix-term` (PluginAssistantMutationHost needs runtime ingress access to submit prompts/cancel)
