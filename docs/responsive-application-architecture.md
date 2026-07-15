# Responsive Application Architecture

Status: target architecture and clean-break migration contract.

This document is canonical for application event admission, foreground state
application, frame production, terminal presentation, and responsiveness. The
executor and task primitives remain defined by
`runtime-executor-architecture-spec.md`; revisioned render dependencies remain
defined by `render-pipeline-revision-plan.md`.

## Decision

Double Helix will use five explicit stages:

1. service actors own external protocols and expensive work
2. domain adapters normalize and semantically reduce updates
3. one foreground owner applies small synchronous state transactions
4. a render actor turns immutable snapshots into complete cell buffers
5. a presenter actor diffs and flushes only the newest complete frame

Input is checked between every foreground transaction. No transport, parser,
filesystem operation, plugin callback, render pass, or terminal flush may run on
the foreground owner.

There is no frame-rate cap, debounce, periodic tick, or fixed frame budget.
Dirty state is submitted whenever a renderer slot is available. Animations
request absolute deadlines and receive elapsed time. Superseded frames are
replaced, not delayed to satisfy a target interval.

## Why The Existing Foundations Are Not Enough

The earlier runtime refactor established the correct low-level pieces:

- explicit `Ui`, `Work`, `Block`, and `Clock` execution domains
- typed bounded mailboxes
- structured cancellation and latest-only work
- application-owned frame invalidation and animation deadlines
- revisioned text, syntax, annotation, layout, and paint dependencies
- typed runtime and compositor commands

The current application still has four systemic gaps:

1. Raw LSP and DAP streams enter `Application` beside the typed runtime ingress.
2. Protocol payload parsing and state preparation can still happen while the
   foreground owner is unavailable for input.
3. A bounded runtime mailbox spills into an unbounded reliable overflow queue.
4. Compositor rendering and terminal flushing execute synchronously in the same
   application loop that accepts input.

The rust-analyzer trace exposed the combined result: thousands of ready protocol
messages became thousands of foreground turns and render opportunities, while a
single terminal flush could hold the application owner for hundreds of
milliseconds.

## Research Basis

The design follows several production patterns, adapted to a terminal editor:

- Tokio documents that `select!` branches run concurrently on one task, not in
  parallel, and that one blocking branch prevents the others from advancing.
  It also makes bounded MPSC capacity the mechanism for backpressure.
  [Tokio select](https://docs.rs/tokio/latest/tokio/macro.select.html),
  [Tokio MPSC](https://docs.rs/tokio/latest/tokio/sync/mpsc/)
- Zed's GPUI keeps application state on a non-`Send` foreground executor and
  directs ordinary computation to a background executor. This validates the
  single-writer model but also shows that foreground scheduling alone does not
  make foreground work cheap.
  [Zed async architecture](https://zed.dev/blog/zed-decoded-async-rust),
  [Zed development glossary](https://zed.dev/docs/development/glossary)
- Neovim processes asynchronous events through one predictable editor loop and
  postpones screen updates until a coherent command or redraw batch is complete.
  Its UI protocol uses an explicit `flush` boundary so intermediate states are
  never presented.
  [Neovim event-loop architecture](https://neovim.io/doc/user/dev_arch.html),
  [Neovim UI events](https://neovim.io/doc/user/api-ui-events/)
- Ratatui's pipeline already distinguishes building the current buffer, diffing
  it against the previous buffer, swapping buffers, and flushing the backend.
  Those phases can be assigned to separate owners without changing widget
  semantics.
  [Ratatui terminal pipeline](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html)
- VS Code isolates extension execution from the UI process and permits only API
  messages across that boundary. Plugin isolation is the only reliable defense
  against arbitrary plugin code monopolizing editor input.
  [VS Code extension host](https://code.visualstudio.com/api/advanced-topics/extension-host),
  [VS Code process sandboxing](https://code.visualstudio.com/blogs/2022/11/28/vscode-sandbox)
- LSP itself provides the identities needed for semantic reduction. Published
  diagnostics replace the previous set for a URI and may carry a document
  version; progress is a state machine keyed by a progress token.
  [LSP 3.18 diagnostics and progress](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
- Apple separates application state updates from final display composition and
  treats main-thread work as the source of both input hangs and animation
  hitches. The useful principle here is separation, not adopting a display-rate
  scheduler for a terminal.
  [UI responsiveness](https://developer.apple.com/documentation/xcode/understanding-user-interface-responsiveness)

## Target Flow

```text
 external process / filesystem / plugin / timer
                    |
                    v
       +---------------------------+
       | service actor             |
       | I/O, parse, compute,      |
       | cancellation, supervision |
       +---------------------------+
                    |
                    v
       +---------------------------+
       | domain adapter            |
       | validate, version, reduce |
       | reliable/latest/fold/pulse|
       +---------------------------+
                    |
                    v
 terminal input -> foreground scheduler <- presenter/render acknowledgements
                    |
                    v
       +---------------------------+
       | foreground owner          |
       | apply one transaction     |
       | emit typed effects        |
       | advance revisions         |
       +---------------------------+
                    |
             immutable snapshot
                    v
       +---------------------------+
       | render actor              |
       | layout + paint to Buffer  |
       | latest generation wins    |
       +---------------------------+
                    |
             complete FramePacket
                    v
       +---------------------------+
       | presenter actor           |
       | diff from last presented  |
       | terminal I/O + cursor     |
       +---------------------------+
```

## Required Invariants

### Foreground ownership

- `Editor`, compositor interaction state, command state, and focus have one
  mutation owner.
- A foreground transaction is synchronous and contains no `.await`.
- A transaction never waits on a lock that background work can hold.
- A transaction does not perform filesystem I/O, process I/O, JSON decoding,
  repository scans, general sorting, regex search, plugin execution, or terminal
  output.
- Background results carry the source revision or generation needed to reject
  stale work before mutation.
- Expensive preparation produces an immutable replacement snapshot whenever
  possible, making foreground application a validation plus pointer swap.
- Work that cannot be reduced to a pointer swap is represented as a resumable,
  domain-specific foreground job with semantic step boundaries. Input is checked
  between steps. There is no generic closure queue.

### Queueing

- Every queue has finite memory.
- Queue-full behavior is part of the event type's semantics, not a call-site
  choice and not an integer priority.
- Reliable messages apply backpressure to their asynchronous producer.
- Replaceable state keeps one latest value per semantic key.
- Foldable streams retain a compact state machine per semantic key.
- Pulses retain presence, not occurrence count.
- Telemetry uses a bounded ring and cannot delay foreground state.
- There is no unbounded spill queue behind a bounded mailbox.

### Rendering

- A frame is associated with one coherent application generation.
- Render code reads immutable frame models and cannot mutate editor state.
- At most one frame is rendering and one newer render snapshot is pending.
- At most one frame is presenting and one newer completed frame is pending.
- The presenter computes its diff against the last successfully presented
  buffer, so dropping intermediate frames cannot desynchronize the terminal.
- A presenter failure or size mismatch requests one full redraw from the newest
  application generation.
- Terminal I/O never blocks input acceptance or editor state mutation.

### Time

- Immediate invalidation means "render as soon as the pipeline can accept work."
- Animation invalidation carries an absolute `Instant` deadline.
- Animation state derives from elapsed time, not frame count.
- No runtime constant represents 60 Hz, 120 Hz, or another refresh cap.
- The 8.33 ms value may be used as a 120 Hz-class benchmark assertion, but never
  as a scheduler delay or work budget.

## One Ingress Abstraction, Multiple Semantic Stores

"One ingress" means one public ownership and routing API. It does not mean one
physical FIFO. One FIFO would recreate head-of-line blocking between input,
responses, diagnostics, logs, and redraw pulses.

`ForegroundIngress` owns four private stores:

```rust
pub struct ForegroundIngress {
    control: ReliableMailbox<ControlEvent>,
    state: StateMailbox,
    telemetry: TelemetryRing,
    wake: PulseHandle,
}
```

Feature code never receives these fields and never selects a lane. It receives a
domain producer such as `LspIngress`, `AssistantIngress`, or `FileIngress`. The
producer's typed methods encode the only valid admission policy.

`helix-runtime` supplies policy-free primitives:

```rust
Reliable<T>             // bounded FIFO, async backpressure
Latest<K, V>            // one replacement value per key
Fold<K, S, U>           // compact state plus typed update reducer
Pulse<K>                // at least one occurrence per key
Ring<T>                 // bounded diagnostic history
```

Protocol crates own the keys and reducers. The runtime crate must not know what a
URI, progress token, assistant message, debugger thread, or file-watch event is.

## Admission Semantics

| Event family | Policy | Key or ordering rule |
| --- | --- | --- |
| Key presses, text paste, mouse buttons | Reliable | Host order |
| Mouse movement and resize | Latest | Device or surface |
| Shutdown, permission prompts, server requests | Reliable control | Source order |
| Request responses | Reliable until matched | Server and request ID |
| Diagnostics | Latest | Server and normalized URI |
| Progress | Fold | Server and progress token |
| Feature refresh requests | Pulse | Server and feature |
| Search/preview result | Latest | Surface and query generation |
| File watcher changes | Fold | Normalized path |
| Status state | Latest | Status owner |
| Log and telemetry records | Ring | Source |
| Animation wakeup | Latest deadline | Component frame source |
| Completed render | Latest | Surface |

Search and automatic preview are explicitly latest-only and cancellable, not
debounced. Every input query starts immediately. A newer generation cancels or
invalidates older work, and stale results are rejected on arrival.

## LSP Contract

Raw `helix_lsp::Call` values must stop at the LSP adapter. `helix-term` must not
parse protocol methods or `serde_json::Value` payloads.

The adapter produces typed domain events:

```rust
pub enum LspControl {
    ServerRequest(PreparedServerRequest),
    Response(PreparedResponse),
    ServerExited(ServerExit),
}

pub enum LspStateUpdate {
    Diagnostics(Arc<DiagnosticSnapshot>),
    Progress(ProgressUpdate),
    FeatureRefresh(LspFeature),
    Capability(CapabilityUpdate),
}
```

Required policies:

- `publishDiagnostics`: parse off foreground, normalize URI once, key by
  `(server, uri)`, keep only the newest publication, and reject a version older
  than the open document. An empty newest set must be preserved because it
  clears diagnostics.
- `$/progress`: fold `begin`, `report`, and `end` by `(server, token)`. Reports
  may replace reports; `begin` establishes identity; `end` is terminal and must
  not be lost. Partial-result progress is routed to the owning request reducer,
  not treated as work-done UI progress.
- server requests: preserve order and response obligation. Their parameters are
  decoded before admission; foreground application creates a typed effect that
  eventually sends the response.
- responses: match and remove the pending request in the service actor. Feature
  results carry request and document generations; stale results are discarded
  before foreground admission.
- log and telemetry notifications: append to a bounded service-owned ring. They
  wake the foreground only when a visible surface subscribes.
- refresh notifications: coalesce as a pulse per server and feature.

This policy is protocol-correct: LSP 3.18 states that a new diagnostic
publication replaces the old set for its URI, and that progress is keyed by a
token with explicit begin/end states.

## Outbound Protocol Contract

Inbound reduction is insufficient if an external service can make an outbound
FIFO grow forever. The current LSP normal-message `VecDeque` must also be
replaced.

Document synchronization is reconstructable state, not an arbitrary reliable
message stream. For each `(server, document)`, the LSP service actor owns:

```rust
pub struct DocumentSyncState {
    pub lifecycle: SyncLifecycle,
    pub written: Arc<TextSnapshot>,
    pub desired: Arc<TextSnapshot>,
    pub desired_version: i32,
}
```

Only one unsent desired state is retained per document. When the writer is ready:

- full-sync servers receive the newest complete content
- incremental-sync servers receive a change from the last sent snapshot to the
  newest snapshot; multiple pending edits may be represented as one replacement
  of the old full range, encoded in the server's position encoding
- versions may skip intermediate numbers because LSP requires increasing, not
  consecutive, document versions
- open, change, save, and close transitions are emitted in a valid lifecycle
  order

Requests that depend on document state carry a required document version. The
service actor establishes a causal barrier: it writes synchronization through
that version before writing the request. This preserves correctness without one
unbounded global FIFO.

Other outbound classes use explicit semantics:

- request responses and shutdown control use bounded reliable mailboxes
- capacity for a required response is reserved when the matching inbound request
  is admitted
- cancellation is a keyed pulse until written
- configuration and workspace-folder state are latest snapshots
- user operations that are neither reducible nor pre-reserved fail admission
  immediately with typed overload state; they never block the foreground and are
  never hidden in an overflow queue

The same rule applies to DAP, ACP, plugin IPC, persistence, and package operations:
model reconstructable state as state, reserve capacity for protocol obligations,
and bound genuinely ordered commands.

## Foreground Scheduler

The scheduler is work-conserving and input-preemptible. It has no periodic loop
and no duration budget.

Conceptually:

```rust
loop {
    drain_ready_input(&mut app);

    if let Some(control) = ingress.try_control() {
        apply_one(&mut app, control);
        continue;
    }

    if let Some(update) = ingress.try_reduced_state() {
        apply_one(&mut app, update);
        continue;
    }

    if renderer.is_ready() && app.is_dirty() {
        renderer.replace(app.render_snapshot());
        continue;
    }

    await_next_wakeup();
}
```

The actual implementation must avoid background starvation without weakening
input semantics:

- Input is drained first at every transaction boundary.
- One reliable control event is applied before checking input again.
- Reduced state stores are drained by key, not by replaying superseded events.
- A render snapshot is submitted when the renderer is ready and the latest dirty
  generation has not already been submitted.
- If rendering is already in flight, further invalidations only advance the
  dirty generation. They do not build or queue more frames.
- A due animation deadline is another input to the scheduler, not a fixed tick.

Tokio's random `select!` fairness is not the foreground policy. The explicit
ready checks establish the policy, and `select!` is used only to sleep until any
source becomes ready.

## Apply And Effect API

Foreground application returns data, never an arbitrary callback:

```rust
pub struct Applied {
    pub damage: Damage,
    pub effects: SmallVec<[Effect; 4]>,
}

pub trait Apply<E> {
    fn apply(&mut self, event: E) -> Applied;
}
```

`Damage` identifies revisioned state that changed. `Effect` is a typed request to
a service actor, such as sending an LSP response, reading a file, starting a
search, or opening a declarative UI layer. Effects contain owned snapshots and
generation IDs; service code cannot borrow `Editor` or `Compositor`.

Domain apply functions remain concrete and statically dispatched. The generic
trait describes the contract; it is not a boxed event bus.

## Immutable Render Boundary

`Application` first synchronizes mutable component models, then creates a cheap
`RenderSnapshot` from revisioned, shared data:

```rust
pub struct RenderSnapshot {
    pub generation: Revision,
    pub surface: SurfaceSpec,
    pub editor: Arc<EditorFrameModel>,
    pub layers: Arc<[LayerFrameModel]>,
    pub theme: Arc<ThemeTokens>,
    pub cursor: CursorModel,
    pub deadlines: Arc<[(FrameSource, Instant)]>,
}
```

Frame models contain no mutable widget, editor, protocol client, runtime handle,
or callback. Expensive derived data is prepared in background services and
shared through `Arc` snapshots. Visible-range layout remains demand-driven and
bounded by the surface.

The render actor owns render caches and produces:

```rust
pub struct FramePacket {
    pub generation: Revision,
    pub area: Rect,
    pub cells: Arc<Buffer>,
    pub cursor: CursorModel,
}
```

The pending render mailbox retains only the newest `RenderSnapshot`. The actor
checks for replacement between major regions and may abandon a superseded frame.
Only complete packets are published.

## Terminal Presenter

The presenter owns:

- the terminal backend and session lifecycle
- the last successfully presented cell buffer
- cursor visibility, shape, and position
- resize validation
- synchronized-update begin/end commands where supported

Its mailbox retains one newest complete `FramePacket`. After a slow flush, it
discards any superseded pending packet and diffs the newest packet directly from
the last successfully presented buffer.

The terminal presenter uses a dedicated host thread, not the ordinary async work
pool or a fire-and-forget `spawn_blocking` task. Terminal writes may block for an
unbounded external duration, while Tokio explicitly notes that started blocking
tasks cannot be aborted.
[Tokio blocking work](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)

This is the crucial latency boundary. A terminal may still take 500 ms to accept
output, so visual feedback cannot physically appear during that interval. The
editor must nevertheless continue accepting input, updating state, and replacing
the pending frame. When output becomes writable, the user sees the latest
coherent state rather than a replay of every intermediate frame.

Input reading remains independently owned. Crossterm exposes `EventStream` as a
`Send` stream, and Windows exposes distinct console input and screen-buffer
handles. Backend session configuration and restoration remain coordinated by the
host so input and output modes cannot race.

## Zero-Copy And Allocation Policy

Zero-copy is used where ownership permits it; borrowed data must never cross an
unsafe lifetime boundary merely to avoid a measured-small allocation.

- Read protocol bytes into reusable transport buffers.
- Deserialize each protocol payload exactly once into its typed owner.
- Store large immutable arrays as `Arc<[T]>`, text as `Arc<str>` or rope
  snapshots, and byte payloads as shared byte buffers.
- Normalize URI and path identity once at the adapter boundary.
- Carry revisioned `Arc` snapshots through foreground and render stages.
- Reuse render buffers inside the serial render and presenter actors.
- Do not clone `serde_json::Value` into foreground events.
- Do not expose pointers into transport buffers after the next read.

Performance changes remain measurement-driven. `Arc` and persistent snapshots
are ownership tools, not automatic wins for small values.

## Plugin Isolation

Arbitrary plugin code cannot share the foreground execution domain.

The plugin architecture uses supervised plugin host processes with generation
routing, restart backoff, memory limits, instruction watchdogs, and owned
resource cleanup. Plugins receive immutable snapshots and return typed editor
effects or declarative UI models. They never receive mutable editor/compositor
access and never render directly into the terminal buffer. There is no supported
in-process plugin execution path.

## Other Subsystem Policies

| Subsystem | Background authority | Foreground payload |
| --- | --- | --- |
| DAP | protocol parse, output buffering, thread snapshots | ordered stop/control plus revisioned snapshots |
| ACP | transport, message stream fold by schema identity | thread/message deltas and ordered permissions |
| File explorer | scan, watch, search, preview load | generation-checked tree/search/preview snapshots |
| Picker | filtering and preview preparation | latest result snapshot and selection identity |
| Syntax | parse and query computation | version-checked syntax snapshot |
| VCS | repository status and diff computation | latest repository/file snapshot |
| Package/runtime health | install processes and probing | operation state keyed by operation ID |
| Persistence | serialization and durable writes | completion/error acknowledgement |
| Plugins | isolated execution | typed effects and declarative UI specs |

ACP stream reducers must use protocol IDs and terminal states, not adjacent-value
deduplication. File search and preview remain immediate latest-generation work;
no debounce is introduced. File watcher overflow becomes a rescan request rather
than an ever-growing path queue.

## Self-Healing Rules

| Failure | Recovery |
| --- | --- |
| Stale generation arrives | Discard before foreground mutation |
| Replace/fold store saturates | Compact by semantic key; memory stays bounded |
| Reliable mailbox saturates | Backpressure producer and record queue wait |
| File watcher reports overflow | Replace pending changes with full rescan generation |
| Render is superseded | Abandon at the next region checkpoint |
| Terminal diff or flush fails | Mark presenter desynchronized and request full newest frame |
| Service actor exits unexpectedly | Supervisor restarts and rehydrates from authoritative snapshots |
| LSP/DAP/ACP exits | Publish typed lifecycle state and reject pending requests cleanly |
| Plugin hangs or crashes | Terminate isolated host, preserve editor, report scoped failure |
| Telemetry consumer is slow | Overwrite oldest ring entries and retain dropped count |

Recovery must be idempotent and generation-based. Retrying arbitrary callbacks is
not allowed.

## Observability Contract

Replace disconnected stopwatch logs with structured spans and stable IDs. The
`tracing` model is appropriate because spans preserve causality across multiplexed
async tasks.

Every external event carries:

- source instance and event kind
- protocol/request/message identity where available
- source generation and foreground generation
- observed, normalized, admitted, applied, rendered, and presented timestamps
- admission policy and whether an older value was coalesced

Required metrics:

- queue depth, memory estimate, high-water mark, and producer wait by store
- coalesced, stale, rejected, and recovered event counts
- normalize, queue-wait, foreground-apply, snapshot, render, diff, and flush time
- input-to-apply, input-to-frame-submit, and input-to-present latency
- dirty, submitted, rendered, and presented generations
- service restart and presenter resynchronization counts

Verbose logs should make one event traceable end to end without logging full
document contents or protocol payloads.

## Crate Ownership

### `helix-runtime`

Own execution domains, tasks, cancellation, clocks, bounded reliable mailboxes,
latest/fold/pulse/ring primitives, deterministic tests, and generic supervisors.
It owns no editor or protocol types.

### `helix-lsp`, `helix-dap`, `helix-acp`

Own transport actors, protocol parsing, protocol identities, admission reducers,
request matching, and typed prepared events. Raw wire payloads do not escape.

### `helix-view`

Own authoritative editor state, revision validation, synchronous domain apply,
backend-neutral frame models, and immutable document/presentation snapshots.

### `helix-tui`

Own pure frame rendering, reusable cell buffers, terminal presentation, buffer
diffing, cursor output, resize validation, and backend recovery.

### `helix-term`

Own composition: the foreground scheduler, mapping input to commands, applying
typed events, dispatching effects, building top-level render snapshots, and
wiring and supervising plugin hosts. It does not perform plugin execution or
terminal output.

### `helix-plugin-api`

Own the host-agnostic handles, requests, snapshots, capability traits, errors,
events, codec, and pure retained-UI geometry. It is the single type owner shared
by plugin processes and frontend adapters.

### `helix-plugin`

Own the framed process protocol, Lua runtime, capability checks, loader, and
sandbox. It has no production dependency on editor or terminal crates.

### `helix-plugin-editor`

Own the explicit `helix-view::Editor` snapshot, handle, and mutation adapters
for the plugin contract. These adapters run only on the editor thread; the Lua
host process never links or accesses editor state.

Plugin-host generations are supervised off the editor thread. Foreground
request service never waits for shared host state: contention returns typed
`Busy` backpressure, task completion routes from immutable responder state, and
command discovery reads a lock-free published snapshot. Generation teardown
submits resource cleanup through `RuntimeIngress`; owner-thread-only foreground
transactions are reserved for requests already being serviced by the editor.

## APIs Removed By The Clean Break

No compatibility shims are required before release. The migration removes:

- `RuntimeOverflow` and every unbounded reliable spill queue
- unbounded protocol output FIFOs and the unbounded shutdown channel
- raw `SelectAll<Receiver<(LanguageServerId, Call)>>` in `Application`
- raw DAP payload streams in `Application`
- protocol parsing in `helix-term::application`
- the placeholder one-variant `AppEvent`
- direct terminal ownership and `terminal.draw()` from `Application`
- mutable component rendering through broad editor/compositor contexts
- arbitrary foreground callbacks and direct plugin editor access
- feature-selected queue priorities

The replacement API must land before each old path is deleted; both paths must
not remain as supported peers.

## Migration Sequence

### Phase 1: Admission and measurement

1. Add keyed `Latest`, `Fold`, and bounded `Ring` primitives to `helix-runtime`.
2. Introduce structured event identity and end-to-end latency spans.
3. Replace `RuntimeOverflow` with typed per-domain full-queue policy.
4. Add burst, blocked-presenter, and stale-generation test fixtures.

### Phase 2: LSP vertical slice

1. Move all inbound LSP decoding and request matching behind an LSP service actor.
2. Implement diagnostics, progress, refresh, response, and log reducers.
3. Deliver only prepared `LspControl` and reduced `LspStateUpdate` events.
4. Remove the raw LSP `SelectAll` branch and foreground protocol parser.

This phase proves correctness against the observed rust-analyzer burst before
generalizing the path.

### Phase 3: Foreground transaction scheduler

1. Replace the placeholder `AppEvent` with the private `ForegroundIngress` facade.
2. Make all application event application synchronous and effect-returning.
3. Add explicit input checks between transactions and foreground-job steps.
4. Route DAP, ACP, config, package, plugin, and file events through domain
   producers with encoded policies.

### Phase 4: Immutable render models

1. Finish immutable component frame models after `sync`.
2. Remove broad mutable query-bag access from render code.
3. Move layout/paint caches into a serial render actor.
4. Publish only complete, generation-tagged `FramePacket` values.

### Phase 5: Presenter isolation

1. Move terminal backend/session ownership into the presenter actor.
2. Diff newest complete frames from the last successful presentation.
3. Add resize handshake, synchronized updates, full-redraw recovery, and acks.
4. Remove terminal flush and buffer ownership from `Application`.

### Phase 6: Isolation and enforcement

1. Keep plugin execution behind the supervised host boundary and enforce it in CI.
2. Add crate-layer architecture tests and forbidden-import checks.
3. Remove transitional APIs and raw Tokio calls from feature code.
4. Make saturation, stale-result, restart, and shutdown tests mandatory in CI.

## Validation Gates

The architecture is complete only when all of these pass:

- A synthetic 100,000-message LSP burst cannot grow memory without bound.
- The burst preserves the newest diagnostics per URI, every required response,
  and each progress terminal state.
- Ordered input is lossless while the burst is active.
- Input-to-state and input-to-frame-submit remain 120 Hz-class in release CI
  when the renderer is available. This is a measured SLO, not a frame cap.
- A presenter deliberately blocked for two seconds does not block input, state
  mutation, LSP responses, cancellation, or shutdown initiation.
- After the presenter unblocks, it displays the newest coherent generation
  without replaying intermediate frames.
- Search and preview begin on every query change without debounce; stale results
  never replace the current generation.
- A file watcher overflow converges through one rescan.
- A hung plugin cannot make the foreground scheduler unresponsive.
- Deterministic runtime tests cover all queue-full and cancellation paths.
- No application-facing queue is unbounded.
- No foreground apply or render function contains filesystem/process I/O or
  arbitrary protocol deserialization.

## Final Architecture Rule

Background systems publish state, not work for the UI to finish. The foreground
owner validates and commits that state. Rendering consumes immutable committed
state. Presentation consumes only complete frames. Each boundary is bounded,
typed, observable, cancellable, and recoverable.
