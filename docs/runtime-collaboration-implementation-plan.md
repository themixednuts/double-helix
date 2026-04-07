# Runtime + Collaboration Implementation Plan

Status: draft

This document turns the two architecture specs into an execution plan:

- `docs/runtime-executor-architecture-spec.md`
- `docs/collaboration-assistant-architecture-spec.md`
- `docs/acp-api-architecture-spec.md`

It answers three practical questions:

1. what must land first
2. what can be built safely in parallel
3. what must be deleted or migrated so we do not end up with two ways of doing the same thing

This plan is deliberately strict about ownership boundaries and migration finish lines.
If a phase introduces a new primitive, that phase should also define the deletion
point for the old path. We do not optimize for compatibility shims or keeping both
systems alive longer than necessary.

This plan describes the target architecture and the required replacement order.
Intermediate implementation states are not design targets and should not be used
to justify keeping duplicate abstractions alive.

## Source Of Truth

Architecture specs:

- `docs/runtime-executor-architecture-spec.md`
- `docs/collaboration-assistant-architecture-spec.md`
- `docs/acp-api-architecture-spec.md`

Primary implementation roots:

- `helix-runtime` (new)
- `helix-view/src/editor.rs`
- `helix-view/src/view.rs`
- `helix-view/src/model/mod.rs`
- `helix-term/src/application.rs`
- `helix-term/src/ui/acp.rs`

Code to migrate or delete:

- exit-task helpers in `helix-term/src/runtime/mod.rs`
- `helix-term/src/runtime/tasks.rs`
- `helix-event/src/cancel.rs`
- `helix-event/src/debounce.rs`
- `helix-event/src/redraw.rs`
- `helix-event/src/status.rs`
- `helix-event/src/runtime.rs`

## Top-Level Outcomes

When this plan is complete, Helix should have:

- one canonical runtime story
- one canonical task/cancel/group/mailbox story
- one collaboration substrate in `helix-view`
- one assistant store in `helix-view`
- ACP as a UI over that store, not the owner of it
- explicit assistant tabs
- follow built on generic participant/location primitives
- typed context attachments built on clean capture/provider boundaries

And Helix should no longer have:

- `job` as a first-class runtime concept
- callback-queue async re-entry as the main pattern
- panel-owned assistant durable state
- row-index keyed assistant state
- duplicated runtime/task/timer abstractions in assistant/collab code

## Global Rules

These rules apply to every phase.

### 1. One canonical path per concern

By the end of a migration phase, there should be one canonical answer for each concern:

- task spawning
- cancellation
- timers
- debounce/latest
- typed event delivery
- assistant thread ownership
- follow state ownership

We should not leave permanent dual paths behind.

### 2. Editor remains the mutation authority

Even after runtime and collaboration work lands:

- async work happens outside the editor stores
- the editor/application loop remains the synchronous mutation path

### 3. Stable ids before UI migration

Anything that survives reordering must get a stable id before ACP UI migration:

- `surface::Id`
- `thread::Id`
- `thread::EntryId`
- `thread::TurnId`
- `change::Id`
- `context::Id`

### 4. Do not genericize too early

Use the exact abstractions from the specs.

Do not invent:

- a generic non-empty ordered map helper too early
- a generic async service abstraction that erases useful differences
- a giant collaboration trait
- a generic history/persistence trait family before one backend shows real pressure to split

### 5. Runtime first, but not runtime only

The runtime work should land early because assistant/collab async boundaries should
use it. But some collaboration-core work can proceed in parallel as long as it does
not invent a parallel runtime/task abstraction.

## Dependency Graph

Hard dependencies:

1. `helix-runtime` core primitives before assistant/collab async integration
2. `collab` ids + surface model before follow and typed scratch/doc reuse
3. `assistant` store before ACP tab migration
4. `assistant` thread ids and entry ids before scratch/follow/context refactor
5. `surface::Capture` + `context::Provider` before text-object prompt feature

Soft dependencies:

- assistant history backend can begin once thread/domain shapes are stable
- ACP tabs can begin once assistant store exists, even before full follow
- follow can begin once locations and tabs exist, even before richer context providers

## Remaining Design Blockers Before Full Parallelization

The major blockers that should be considered settled before broad callback/taskbridge
migration continues are:

1. typed main-thread UI ingress schema
   - grouped `UiCommand` families
   - typed layer ids/specs
   - clear split between `RuntimeTaskEvent` and `UiCommand`
2. shutdown wait semantics
   - `WaitSet::next()` / `drain()` completion-order contract
   - explicit registration policy for must-finish tasks
3. neutral apply extraction pattern
   - editor effects go to neutral apply modules
   - UI construction goes through typed `apply.rs` modules
   - no handlers <-> runtime cycles

Once these are accepted, the callback-heavy migrations can safely proceed in parallel.

## Parallel Work Model

This plan is split into phases, and each phase may contain several tracks.

Tracks are "parallel-safe" only if they meet all of these conditions:

- they do not create a second canonical path for the same concern
- they touch distinct ownership roots or only converge through stable agreed types
- they can merge in either order without semantic conflict

For each phase below:

- `serial` means it should land before the next phase starts
- `parallel-safe` means tracks can proceed concurrently once the phase contract is agreed

## Phase 0 - Lock The Contracts

Goal:

- freeze the core shapes so implementation work does not churn names or boundaries

Deliverables:

- specs accepted as the design baseline
- any final naming corrections applied
- any unresolved contradictions between specs removed

Exit criteria:

- the runtime spec and collaboration spec no longer contradict each other
- the implementation plan is accepted as the migration order

Parallel-safe tracks:

- none; this is the agreement phase

## Phase 1 - `helix-runtime` Core

Goal:

- introduce the runtime crate and make its core primitives real without yet migrating all consumers

Primary files:

- `helix-runtime/Cargo.toml`
- `helix-runtime/src/lib.rs`
- `helix-runtime/src/ui.rs`
- `helix-runtime/src/work.rs`
- `helix-runtime/src/block.rs`
- `helix-runtime/src/task.rs`
- `helix-runtime/src/cancel.rs`
- `helix-runtime/src/group.rs`
- `helix-runtime/src/clock.rs`
- `helix-runtime/src/mailbox.rs`
- `helix-runtime/src/latest.rs`
- `helix-runtime/src/debounce.rs`
- `helix-runtime/src/gate.rs`
- `helix-runtime/src/test.rs`

Required APIs:

- `Runtime`
- `Ui`
- `Work`
- `Block`
- `Clock`
- `Task<T>`
- `task::Local<T>`
- `Token`
- `Group`
- `group::Scope`
- `Sender<T>` / `Receiver<T>`
- `WaitSet`
- `Latest`
- `Debounce`
- `Gate<T>`

Required tests:

- task cancellation semantics
- `task::Local<T>` vs `Task<T>` type behavior
- group child accounting and `join()` semantics
- closed-scope `spawn_in(...)` returning `SpawnError`
- bounded mailbox semantics
- fake clock and deterministic timer progression

Parallel-safe tracks:

### Track 1A - Core handles and task types

- `task`, `cancel`, `group`, `ui`, `work`, `block`

### Track 1B - Time and orchestration helpers

- `clock`, `latest`, `debounce`, `gate`

### Track 1C - Mailboxes and test runtime

- `mailbox`, `test`

### Track 1D - Shutdown wait set

- `wait`

Merge condition:

- all public types exist
- core tests pass
- no feature crate depends on ambient Tokio APIs from new code added after this point

## Phase 2 - Runtime Ownership In `helix-term`

Goal:

- make the application own a `Runtime`
- establish typed mailbox ingress
- start replacing global/callback runtime glue

Primary files:

- `helix-term/src/main.rs`
- `helix-term/src/application.rs`
- `helix-term/src/host.rs`
- `helix-view/src/editor.rs`

Required work:

- construct `Runtime` in `main`
- pass runtime handles into application/editor ownership roots
- define typed app/runtime ingress events
- establish one application-owned runtime mailbox receiver path
- replace redraw/status globals with owned runtime-facing primitives or adapters

Parallel-safe tracks:

### Track 2A - Runtime root wiring

- `main.rs`
- `Application::new(...)`
- runtime ownership plumbing

### Track 2B - Typed ingress enums and mailbox receiver ownership

- `AppEvent`
- `RuntimeEvent`
- application fan-in cleanup

### Track 2C - Frame/timer integration

- connect `Clock`
- replace ad hoc timer ownership where feasible

Exit criteria:

- application owns runtime handles and runtime mailbox receivers
- new feature code can re-enter main-loop mutation through typed mailbox events
- no new runtime-local global queues are introduced

## Phase 3 - Typed UI/Main-Thread Ingress

Goal:

- define the typed compositor-facing ingress that replaces `Callback::EditorCompositor`

Primary files:

- `helix-term/src/application.rs`
- `helix-term/src/compositor.rs`
- `helix-term/src/runtime/ui/` (`command.rs`, `apply.rs`, `mod.rs`)
- `helix-term/src/runtime/ingress.rs` (events + ingress sender helpers), `tasks.rs`
- `helix-term/src/effect.rs` (editor-only apply logic shared from `runtime::tasks`; keeps `handlers` from depending on `runtime::tasks` in a cycle)
- `helix-term/src/ui/*` (call sites; new UI commands live under `runtime/ui/`)

Required work:

- define `UiCommand` families grouped by domain
- define layer/picker/prompt/completion/signature command specs where needed
- centralize application of UI commands on the main thread
- define shutdown wait semantics using `WaitSet`
- decompose `RuntimeDispatch` responsibilities into:
  - runtime spawning
  - typed UI/runtime ingress
  - explicit shutdown waiting

Parallel-safe tracks:

### Track 3A - UI command schema

- top-level `UiCommand`
- grouped sub-command enums/specs

### Track 3B - Main-thread application path

- application/compositor integration for applying typed commands

### Track 3C - Shutdown/wait path

- replace `wait_futures` style logic with completion-order `WaitSet::next()` / `drain()`

Exit criteria:

- there is a typed replacement for compositor-touching callbacks
- there is an explicit replacement for wait-on-exit semantics
- `RuntimeDispatch` is no longer the only place where those concerns are glued together

Current note:

- detached scheduling and typed ingress have already moved out of `RuntimeDispatch`
- the remaining exit-task helpers in `helix-term/src/runtime/mod.rs` are now an app-owned wait-on-exit sink around `WaitSet`

## Phase 4 - Remove `job` As A First-Class Runtime Concept

Goal:

- remove `helix-term::job` as a canonical path

Primary files:

- exit-task helpers in `helix-term/src/runtime/mod.rs`
- `helix-term/src/runtime/tasks.rs`
- all call sites using `Jobs`, `Job`, `Callback`

Required work:

- port every `Jobs` call site to:
  - `Task<T>` / `task::Local<T>`
  - `Sender<T>` / `Receiver<T>`
  - typed events/effects
- delete the old `helix-term/src/runtime/dispatch/` adapter layer
- delete the `Callback` / `RuntimeEvent::Callback` path once command/LSP/DAP/ACP jobs return only `UiCommand` / `RuntimeTaskEvent`
- delete or fold any remaining detached spawn compatibility helpers and `tasks.rs` into the new runtime/app ingress path

Parallel-safe tracks:

### Track 3A - Inventory + conversion map

- enumerate all `Jobs` call sites and classify them:
  - root task
  - grouped task
  - latest-only task
  - blocking work
  - mailbox re-entry

### Track 3B - Port root/background services

- long-lived background tasks and service loops

### Track 3C - Port callback-style UI re-entry

- convert callback result delivery to typed events

Exit criteria:

- `RuntimeDispatch`/`Callback` bridge removed in favor of `Task<T>` + typed mailbox events
- no `helix-term/src/runtime/{bridge,callback}` path remains in the architecture
- `job` no longer appears as a runtime primitive in code paths under active development

Current note:

- the callback/deferred bridge portion of `RuntimeDispatch` is already gone
- the remaining work is to decide whether the wait-on-exit helpers in `runtime/mod.rs` should stay there or be folded even further into a smaller shutdown-only surface
- the old `runtime/spawn.rs` helper has already been folded into `runtime/mod.rs`

## Phase 5 - Retire Old Async Helper Paths

Goal:

- collapse duplicate async helper patterns into the runtime crate

Current note:

- `helix-term` detached spawn policy now lives at `helix-term/src/runtime/mod.rs`
- `helix-term` async UI/task future helpers report failures through typed runtime status ingress rather than the old status-global path
- straightforward `helix-term` redraw producers now send typed `RuntimeEvent::Redraw` through runtime ingress instead of poking the redraw global directly
- runtime task application no longer requests redraw directly; task ingress callers render after apply
- notification popup state sync no longer requests redraw directly during render preparation
- remaining redraw-global use is now concentrated in explicit main-loop render throttling policy and shared `helix-view` / `helix-event` infrastructure
- `helix-view` notification timeout redraw scheduling now requires the editor runtime handle and runs on `helix_runtime::Work`
- `helix-term` marquee redraw scheduling now uses one runtime-native path instead of split fallback behavior
- shared `helix-view` tests that exercise runtime-backed editor behavior now install an editor runtime handle instead of relying on no-runtime construction
- the main unresolved redraw question is now the shared view-layer seam: code like `helix-view::handlers::diagnostics` can request redraw without depending on `helix-term`, but the target typed replacement for that backend-agnostic path is not yet defined in the plan
- editor-owned `helix-view` redraw producers now have an explicit redraw queue (`Editor::request_redraw`) instead of reaching straight for the redraw global; the remaining global path is the shared external redraw signal consumed in `Editor::wait_event`
- diagnostics and VCS diff updates now use that editor-owned redraw queue instead of calling the redraw global directly
- `helix-term::Application` redraw policy now also goes through `Editor::request_redraw()`, so direct redraw-global calls are down to the shared primitive itself and its drop helper
- assistant model sync, assistant thread creation, and plugin `helix.ui.redraw()` now also use the editor-owned redraw queue instead of setting redraw flags directly
- assistant follow/location publication now flows through store effects and assistant effect draining, rather than being published directly from the application update handler
- assistant participant creation on backend bind now also flows through store effects instead of direct application-side collab mutation
- assistant follow activation now also relies on a store-emitted participant effect instead of pre-joining from the toggle helper
- dead assistant side-effect plumbing like `MarkUnread` has been removed where unread state is already carried by the store/model path
- assistant cancel/mode/config backend sends now flow through store-emitted backend-command effects instead of panel/command helpers sending backend handles directly
- assistant prompt submission now also flows through `Action::Submit` + backend-command effects instead of panel/command helpers appending entries and sending backend handles directly
- assistant permission resolution now returns to the main loop through typed runtime ingress, then flows through `Action::ResolvePermission` + backend-command effects instead of sending backend handles directly from the popup task
- assistant close/load-thread backend sends now also flow through store/effect routing instead of command/UI helpers sending backend handles directly
- assistant new-thread creation now also flows through `Action::NewThread` + backend-command effects once backend bootstrap exists
- assistant participant leave on thread close now also flows through effects instead of direct command-side collab mutation
- creating a new assistant thread now updates canonical active-thread state in the store at the same time the backend `NewThread` command is emitted
- assistant history refresh status now also flows through assistant effects instead of an application-local status update after history replacement
- assistant panel/controller code no longer resolves backend handles for send paths; backend-handle lookup is centralized in assistant effect draining after thread origin checks
- assistant scratch-detail rendering now reads canonical thread entries by stable `EntryId` instead of deriving details from the panel-local chat projection
- restoring a history thread now goes through `Action::LoadThread` rather than a direct store helper, keeping thread-load state changes on the main action path
- the derived assistant panel model now carries selected message state as stable `EntryId` rather than a row index
- the derived assistant panel entry list now also carries stable `EntryId` and location counts, bringing it closer to canonical `assistant::model::EntryView`
- assistant scratch-doc mappings are now cleaned up on document close through `Action::UntrackDoc`, instead of waiting for stale-doc detection on the next reopen
- the panel output cache now also uses the stable-id derived assistant entry struct instead of a panel-local chat-entry enum, reducing duplicate message shaping in the controller
- assistant panel output syncing now derives from canonical `assistant::model::Panel` / `ThreadView` data instead of re-reading raw thread entries directly
- panel output scroll is now also resynchronized from canonical per-thread view state during panel sync, keeping controller-local viewport state aligned with durable thread scroll state
- scratch-doc reuse now reads opened-doc mappings from canonical `assistant::model::ThreadView`, instead of reading raw thread view state directly
- panel-local selection/fold/scroll reads now resolve through canonical derived `ThreadView` data for message navigation, instead of querying raw thread view state directly
- panel display reads like mode/model/follow/plan/context now also resolve through canonical derived `assistant::model::ThreadView`, shrinking raw-thread reads further
- assistant message-detail rendering now also uses canonical derived `EntryView` data instead of reading raw thread entries directly
- panel checks for backend-backed active threads now also use canonical `ThreadView` state instead of reading raw thread origin directly
- the assistant panel controller no longer keeps a raw active-thread helper; durable reads now route through canonical derived `Panel` / `ThreadView` data
- scratch-doc open/reuse orchestration now lives in `crate::assistant::open_entry_scratch`, so the panel no longer mutates scratch-doc tracking state directly
- command-side scratch open now also reads canonical selected-entry state through `crate::assistant::open_selected_entry_scratch`, rather than requiring panel instance access
- prompt request construction now happens in the assistant store from canonical context state, instead of being assembled in panel/command helpers before dispatch
- the typed `assistant-open-entry-scratch` command now uses the same centralized scratch-doc helper as the panel path, removing another duplicate entry-to-doc conversion path
- canonical derived `EntryView` now carries revealable locations too, so reveal-entry commands can avoid re-reading raw thread entry locations for the common case
- turn/thread change scratch opening now also routes through centralized assistant helpers instead of duplicating summary extraction in typed command handlers
- follow reveal/open execution is now triggered by a store-emitted `RevealLocation` effect, so the application no longer recomputes follow reveal policy after applying location updates
- panel-side scratch opening now also dispatches `Action::OpenEntryDoc` through the shared assistant helper instead of bypassing the action/effect path
- assistant backend bootstrap/lookup is now centralized in `crate::assistant::{spawn_backend, ensure_backend}` and reused by effect draining, rather than being split across command helpers

Primary files:

- `helix-event/src/cancel.rs`
- `helix-event/src/debounce.rs`
- `helix-event/src/redraw.rs`
- `helix-event/src/status.rs`
- `helix-event/src/runtime.rs`

Required work:

- move restartable/latest-only feature code to `Latest`
- move debounce logic to `Debounce`
- replace ad hoc init barriers with `Gate<T>` where appropriate
- retire runtime-local redraw/status runtime globals from the normal architecture

Parallel-safe tracks:

### Track 4A - Cancellation/latest migration

- move `TaskController`-style call sites onto `Token` + `Latest`

### Track 4B - Debounce migration

- move `AsyncHook`-style call sites onto `Debounce`

### Track 4C - Global queue/runtime-local cleanup

- redraw/status/runtime globals

Exit criteria:

- no duplicate "official" debounce/latest/cancel path remains
- old helpers are deleted or fully demoted out of the main architecture

## Phase 6 - Collaboration Core In `helix-view`

Goal:

- add the generic `collab` substrate in `helix-view`

Primary files:

- `helix-view/src/collab/mod.rs`
- `helix-view/src/collab/ids.rs`
- `helix-view/src/collab/participant.rs`
- `helix-view/src/collab/location.rs`
- `helix-view/src/collab/presence.rs`
- `helix-view/src/collab/follow.rs`
- `helix-view/src/collab/surface.rs`
- `helix-view/src/collab/registry.rs`
- `helix-view/src/collab/effect.rs`
- `helix-view/src/editor.rs`
- `helix-view/src/view.rs`

Required work:

- define `participant`, `location`, `presence`, `follow`, `surface`, `effect`
- add `surface::Id`, `surface::Ref`, `surface::Mut`
- add `surface::Factory`, `surface::Registry`, `surface::Role`, `surface::Target`, `surface::Capture`
- add editor-owned `collab::Store`
- add `Editor::with_surface(...)` / `with_surface_mut(...)`

Parallel-safe tracks:

### Track 5A - Ids, participants, locations, presence

- pure domain types and store

### Track 5B - Surface model and registry

- surface ids, refs/muts, factory/registry, typed open/capture errors

### Track 5C - Editor integration seam

- `Editor` fields and surface visitor helpers

Exit criteria:

- generic collaboration types compile and are test-covered
- editor can resolve surfaces by `surface::Id`
- no assistant-specific types leak into `collab`

## Phase 7 - Assistant Core In `helix-view`

Goal:

- move assistant domain state into `helix-view`

Primary files:

- `helix-view/src/assistant/mod.rs`
- `helix-view/src/assistant/action.rs`
- `helix-view/src/assistant/backend.rs`
- `helix-view/src/assistant/store.rs`
- `helix-view/src/assistant/thread.rs`
- `helix-view/src/assistant/context.rs`
- `helix-view/src/assistant/host.rs`
- `helix-view/src/assistant/permission.rs`
- `helix-view/src/assistant/prompt.rs`
- `helix-view/src/assistant/config.rs`
- `helix-view/src/assistant/plan.rs`
- `helix-view/src/assistant/tool.rs`
- `helix-view/src/assistant/change.rs`
- `helix-view/src/assistant/history.rs`
- `helix-view/src/assistant/event.rs`
- `helix-view/src/assistant/effect.rs`
- `helix-view/src/editor.rs`

Required work:

- define `assistant::Store::{Empty, Ready(Threads)}`
- define frontend-facing `assistant::action::Action`
- define backend-facing `assistant::backend::{Kind, Connect, Caps, Remote, Handle, Driver, Command, Event, Update}`
- define `thread::{Id, EntryId, TurnId, Origin, Run, Thread, Entry, Turn, Event, Snapshot, ViewState}`
- define `context::{Id, Item, Kind, Selection, Symbol, Provider, Registry}`
- define `host::{Fs, Terminal, Permission, Set}`
- define `permission::{Request, Choice, Kind, Decision}`
- define `prompt::{Part, Role, Request, Builder}`
- define `plan`, `config`, `tool`, `terminal`, `change`, `history`
- add editor-owned `assistant::Store`

Parallel-safe tracks:

### Track 6A - Thread/store core

- thread ids, thread state, store, effects

### Track 6B - Context model and provider registry

- context ids/items/kinds/providers

### Track 6C - Backend/host/action boundary

- backend commands/updates
- host capabilities
- frontend action surface

### Track 6D - History backend and record/stub shapes

- `history::{Record, Stub, Backend}`

### Track 6E - Change/provenance model

- `change::{Id, Summary, File, Hunk}`

Exit criteria:

- assistant durable state exists outside ACP UI
- ACP wire ids are converted at the boundary into assistant domain ids/origin
- no panel-owned durable thread data remains as a required source of truth

## Phase 8 - Route ACP Through Assistant Store

Goal:

- stop mutating ACP durable state directly in the panel/application

Primary files:

- `helix-view/src/assistant/acp/mod.rs`
- `helix-view/src/assistant/acp/id.rs`
- `helix-view/src/assistant/acp/driver.rs`
- `helix-view/src/assistant/acp/translate.rs`
- `helix-term/src/application.rs`
- `helix-acp/src/client.rs`
- `helix-term/src/ui/acp.rs`

Required work:

- implement ACP as one `assistant::backend::Driver`
- normalize ACP ids into `assistant::acp::Id`, `assistant::backend::Remote`, and `thread::Origin`
- define `assistant::backend::{Handle, Command, Event, Update}` concretely enough to replace panel-direct ACP control flow
- define `assistant::host::{Fs, Terminal, Permission, Set}` adapters at the application boundary
- translate ACP wire updates into:
  - `thread::Id`
  - `thread::Event`
  - `assistant::event::Event`
- apply through `editor.assistant.apply(...)`
- drain assistant/collab effects
- sync derived model state

Parallel-safe tracks:

### Track 7A - Application event translation

- wire-event -> domain-event conversion

### Track 7B - ACP backend driver

- `assistant::acp::{id,driver,translate}`
- backend command/update wiring

### Track 7C - Host capability adapters

- file, terminal, permission host bridges

### Track 7D - Assistant effect draining

- publish location, unread, save/delete, model sync

### Track 7E - ACP panel de-ownership

- reduce ACP panel to view/controller state only

Exit criteria:

- application no longer directly mutates ACP durable thread state
- assistant store is the source of truth

## Phase 9 - ACP Tabs And View-State Migration

Goal:

- make ACP tabs/session switching real and move panel state onto stable ids

Primary files:

- `helix-term/src/ui/acp.rs`
- `helix-view/src/model/mod.rs`
- `helix-view/src/model/models.rs`

Required work:

- explicit session tabs
- per-thread `thread::ViewState`
- scratch docs keyed by `thread::EntryId`
- derived panel model updated to thread/tab model

Parallel-safe tracks:

### Track 8A - Derived panel model

- `assistant::model::{Panel, Tab, ThreadView, EntryView, Pill}`

### Track 8B - ACP tab strip and per-thread selection/scroll state

- panel rendering/controller work

### Track 8C - Scratch/doc/view-state migration

- stable id keyed scratch docs and expanded rows

Exit criteria:

- tabs are real
- background threads are real
- ACP panel no longer keys durable UI state by row index

## Phase 10 - Follow

Goal:

- implement follow on top of generic participant/location primitives and assistant thread state

Primary files:

- `helix-view/src/collab/follow.rs`
- `helix-term/src/application.rs`
- `helix-term/src/ui/acp.rs`
- `helix-view/src/editor.rs`

Required work:

- publish locations from ACP read/write/tool activity
- thread-local `follow::State`
- auto-open/auto-switch/reveal behavior
- pause on local movement/scroll/edit/buffer switch

Parallel-safe tracks:

### Track 9A - location publication

- ACP activity -> `collab::Store::publish_location(...)`

### Track 9B - editor follow execution

- reveal/open/switch behavior using `surface::Role`

### Track 9C - thread follow policy and ACP UI badges

- tab badges, thread follow state, pause/resume controls

Exit criteria:

- follow works across files
- follow pause semantics are deterministic
- inactive threads do not steal focus just because they are busy

## Phase 11 - Context Attachments

Goal:

- add typed context attachment UX and request building

Primary files:

- `helix-view/src/assistant/context.rs`
- `helix-term/src/ui/acp.rs`
- `helix-term/src/commands.rs`

Required work:

- local capture via `surface::Capture`
- richer providers via `context::Provider` / `Registry`
- pills in composer
- `PromptBuilder` over `context::Kind`

Parallel-safe tracks:

### Track 10A - local capture

- selection and symbol from current surface

### Track 10B - provider registry

- diagnostics/file/diff or future richer providers

### Track 10C - UI and commands

- attach/detach/show/send behavior

Exit criteria:

- text-object prompting is built on typed context refs, not prompt string hacks

## Phase 12 - Persistence

Goal:

- wire assistant history backend and persistence effects through runtime

Primary files:

- `helix-view/src/assistant/history.rs`
- `helix-term/src/application.rs`
- runtime-backed persistence worker code

Required work:

- `Save` and `Delete` effects
- debounced saves through runtime
- immediate flush on close/shutdown
- lazy restore by `Stub` first, `Record` on demand where appropriate

Parallel-safe tracks:

### Track 11A - backend implementation

- local persistence backend implementation

### Track 11B - effect wiring

- application/runtime mailbox and debounced persistence worker

### Track 11C - restore path

- startup restore, tab restore, active thread restore

Exit criteria:

- assistant persistence is fully runtime-driven
- no assistant store method performs persistence I/O directly

## Phase 13 - Cleanup And Deletion Pass

Goal:

- remove legacy paths that should not survive

Required deletions or severe reductions:

- dedicated exit-task wrapper/module after full migration
- primary `TaskController` usage paths
- primary `AsyncHook` usage paths
- redraw/status/runtime globals as canonical paths
- ACP panel as durable assistant state owner

Exit criteria:

- only one canonical runtime path
- only one canonical collaboration state path
- only one canonical assistant state path

## Parallel-Safe Work Summary

The largest safely parallel buckets are:

### Runtime track

- `helix-runtime` core
- mailbox/test runtime
- helper primitives (`Latest`, `Debounce`, `Gate`)

### Collaboration-core track

- ids/participants/locations/presence
- surface model and editor visitor seams

### Assistant-core track

- thread/store/event/effect model
- context model/provider registry
- history record/stub/backend
- change/provenance model

### Frontend track

- ACP tabs
- panel model derivation
- per-thread view state

These tracks can run in parallel once the relevant phase contracts are frozen.

## Things That Must Not Be Parallelized Prematurely

These are the risky merges that should stay ordered:

- deleting the `Job`/`Jobs` bridge before typed runtime ingress exists
- ACP tab migration before assistant ids/store are in place
- follow implementation before locations and per-thread state are stable
- persistence worker wiring before the assistant record/backend shape is frozen
- context pills UI before the context model and detach semantics are stable

## Commit Strategy

Recommended commit slices should follow intent and ownership boundaries, for example:

1. `wip: add runtime task and cancellation primitives`
2. `wip: add runtime mailbox and clock services`
3. `wip: delete job queue in favor of runtime tasks`
4. `wip: add collaboration surface and location store`
5. `wip: add assistant thread store and events`
6. `wip: route ACP updates through assistant store`
7. `wip: add assistant tabs and per-thread view state`
8. `wip: add follow on collaboration locations`
9. `wip: add typed assistant context attachments`
10. `wip: wire assistant persistence through runtime`

## Definition Of Done

This implementation plan is complete when:

- `helix-runtime` is the only canonical runtime path
- `job` is gone as an architecture concept
- collaboration state is editor-owned and generic
- assistant state is editor-owned and session-aware
- ACP is a UI/controller, not the durable owner
- follow is built on generic participant/location primitives
- typed context attachment flow is real
- persistence is effect-driven and runtime-backed
- no old duplicate path remains as a peer abstraction
