# Runtime, Tasks, and Event Delivery Spec

Status: draft

This document defines the target async/runtime architecture for the Helix fork.

It is a separate spec from the collaboration and assistant architecture. This
document defines the runtime story those layers build on.

The goal is not to copy another editor's executor stack. The goal is to design
the right execution model for Helix itself: one that gives us clear thread
affinity, strong cancellation, typed event delivery, good timers, deterministic
tests, and a better replacement for the current jobs/callback/runtime-local mix.

This spec describes the target architecture. Temporary bridges and migration aids
are not part of that target and should not be treated as acceptable end state.

## Outcome

Helix should gain a dedicated runtime crate and a clear execution model built
around:

- a UI-affine execution domain
- an async background execution domain
- a blocking offload execution domain
- a first-class clock/timer service
- typed task handles
- structured cancellation
- task groups
- typed mailboxes for event delivery
- reusable orchestration helpers like latest/debounce/gate

The runtime should improve all async work in Helix, not just assistant or ACP.

## Goals

1. Make execution domains explicit in the type system.
2. Replace callback-heavy job re-entry with typed mailboxes and events.
3. Make cancellation first-class and composable.
4. Make timers a real runtime service instead of ad hoc sleeps and redraw nudges.
5. Preserve a single-writer main-loop mutation model for editor state.
6. Provide a deterministic test runtime with fake time and stepwise scheduling.
7. Avoid APIs that only behave one way in tests and another in production.
8. Avoid unsafe borrowed async scope as a foundational primitive.
9. Keep the public API compact, descriptive, and module-oriented.
10. Support current Helix workloads cleanly: editor UI, LSP, DAP, ACP, file watch,
    async previews, subprocesses, and future collaboration.

## Non-Goals

- Replacing Tokio outright.
- Turning the runtime into a general distributed systems framework.
- Making all state mutation async.
- Porting another editor's scheduler implementation line-for-line.

## Current Helix Problems This Must Solve

Today Helix spreads async work across several patterns:

- ambient `tokio::spawn`
- `helix-term::job::{Jobs, Job, Callback}`
- `helix-event::AsyncHook`
- editor-owned timers in `Editor`
- runtime-local globals for redraw, status, and job re-entry
- per-subsystem incoming streams for LSP, DAP, ACP, and file watching

This creates a few recurring problems:

- async work has no single ownership story
- cancellation is fragmented by feature
- UI-thread re-entry is callback-shaped instead of event-shaped
- timers are not a first-class shared primitive
- adding a new async subsystem means adding another fan-in path
- tests do not have one deterministic runtime model

The runtime spec should solve those problems directly, not work around them.

## What `RuntimeDispatch` Was Bundling

`helix-term/src/runtime/dispatch/` was an adapter to replace, not part of the
target architecture.

Originally it bundled three responsibilities:

1. scheduling background work through a `Work` handle
2. delivering async completions back to the main thread as typed ingress:
   - `RuntimeTaskEvent` (editor-side / neutral apply, e.g. via `effect.rs`), or
   - `UiCommand` (compositor / layers / widgets) as `RuntimeEvent::Ui`
3. tracking **must-finish-before-exit** work in `helix_runtime::WaitSet`, completion-order via `WaitSet::next` / `WaitSet::drain`

That bundling is what Phase 4+ should decompose.

In the current migration state, that decomposition is mostly complete:

- detached scheduling uses typed ingress helpers directly
- main-thread re-entry uses typed `UiCommand` / `RuntimeTaskEvent` ingress
- the remaining app-owned shutdown wait helpers live directly under `helix-term/src/runtime/mod.rs` and are just a thin app-owned sink around `WaitSet`

The target design splits those responsibilities apart:

- scheduling -> `Ui`, `Work`, `Block`
- main-thread ingress -> typed `UiCommand` / `RuntimeEvent` (no callback-shaped mailbox)
- shutdown waiting -> `WaitSet`

One remaining boundary to define more explicitly is redraw ownership below `helix-term`:

- `helix-term` async/runtime redraw producers can use typed `RuntimeEvent::Redraw`
- editor-owned `helix-view` redraw producers can use an explicit editor redraw queue
- shared `helix-view` code still has backend-agnostic redraw needs beyond that editor-owned seam
- the final typed seam for those shared view-layer redraw requests is narrower than the old global, but is not fully specified here yet

In the current code, direct calls into the redraw global are effectively reduced to the
shared primitive itself and its drop helper; app-owned and editor-owned redraw producers
now route through explicit runtime or editor seams.

The legacy name `TaskBridge` referred to the same bundle. The target architecture
does not preserve that bundle under another name.

## Design Principles

### 1. Model execution domains, not just executors

The right abstraction is not simply "foreground vs background".

Helix has at least four distinct runtime concerns:

- main-thread-affine work
- ordinary async background work
- blocking offload
- timers and time control

Those concerns should be explicit in the API. A single generic executor handle is
too vague. A raw foreground/background clone is too coarse.

### 2. Keep mutation authority synchronous

The editor should continue to mutate durable state through one synchronous main
loop path.

Async work should:

- compute
- wait
- read external state
- produce typed events

It should not become a second mutation pathway for editor state.

### 3. Root tasks and grouped tasks are different

The runtime must support both:

- root-owned long-lived tasks
- grouped child tasks with shared cancellation and lifecycle

Do not force everything into scoped spawn.

However, root spawn should be policy-restricted:

- top-level bootstrap code may use root spawn
- long-lived subsystem owners may use root spawn
- ordinary feature code should prefer group-owned spawn

So the architecture supports both, but the default style for feature work should
still be structured child tasks.

### 4. Cancellation must be first-class

"Drop the task and hope that means cancel" is not enough as the main model.

Cancellation should have:

- a dedicated token type
- explicit propagation
- clear task result semantics
- helper types for restartable/latest-only work

### 5. Timers must be centralized

Timers should not live as:

- random `tokio::sleep` calls
- editor-owned ad hoc fields
- frontends that only request redraws and hope something notices

Time should have one explicit service.

### 6. Event delivery must be typed

The right replacement for callback soup is:

- typed senders/receivers
- typed events
- typed effects

not more callback enums or runtime-local queues.

### 7. Public APIs should use the best stable Rust 1.94 surface

That means:

- native `async fn` in traits is available and should be used where appropriate
- async closures and `AsyncFn*` are available and should be used where they add
  semantic value
- but the runtime should not use a new feature just because it exists; it should
  use it where it improves the API shape

## Crate Layout

Introduce a new workspace crate:

- `helix-runtime`

Internal modules:

- `ui`
- `work`
- `block`
- `task`
- `cancel`
- `group`
- `clock`
- `mailbox`
- `wait`
- `latest`
- `debounce`
- `gate`
- `test`

Public crate-root re-exports should keep use-site naming short:

```rust
pub use block::Block;
pub use cancel::Token;
pub use clock::{Clock, TimerId};
pub use debounce::Debounce;
pub use gate::Gate;
pub use group::Group;
pub use group::Scope;
pub use latest::Latest;
pub use mailbox::{channel, Receiver, Sender};
pub use task::{Error as TaskError, Task};
pub use wait::Set as WaitSet;
pub use ui::Ui;
pub use work::Work;

pub struct Runtime {
    ui: Ui,
    work: Work,
    block: Block,
    clock: Clock,
}
```

This gives us clean call sites without long redundant paths.

## Canonical Naming

This runtime should follow the same naming rules as the collaboration spec.

The rule is:

- use module context aggressively
- keep local names short
- avoid repeating context already carried by the module path

### Modules

- `ui`
- `work`
- `block`
- `task`
- `cancel`
- `group`
- `clock`
- `mailbox`
- `wait`
- `latest`
- `debounce`
- `gate`
- `test`

### Types

- `Runtime`
- `Ui`
- `Work`
- `Block`
- `Task<T>`
- `task::Local<T>`
- `TaskError`
- `Token`
- `Group`
- `Clock`
- `TimerId`
- `Sender<T>`
- `Receiver<T>`
- `WaitSet`
- `Latest`
- `Debounce`
- `Gate<T>`

Avoid:

- `ForegroundExecutorHandle`
- `BackgroundExecutorService`
- `AsyncJobManager`
- `ThreadPoolRuntimeController`
- `TaskController` as the primary runtime name

### Method names

Prefer short verbs:

- `spawn`
- `spawn_in`
- `spawn_with`
- `blocking`
- `timer`
- `cancel`
- `detach`
- `join`
- `post`
- `send`
- `recv`
- `restart`
- `open`

Avoid:

- `spawn_background_task`
- `create_cancellation_token`
- `run_blocking_on_background_executor`
- `dispatch_async_job_callback`

### Task vs job terminology

Use `Task` for the runtime primitive.

Reason:

- `task` matches Rust async ecosystem expectations
- `job` in current Helix already implies callback-queue semantics we are trying to replace

Policy:

- new runtime APIs should use `task`, not `job`
- the old `job` terminology should be removed during the migration
- we should not intentionally keep both as live first-class terms in the new design

### Native trait policy

Be explicit about standard trait impls.

#### Handles and ids

- `TimerId`: `Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`
- lightweight handles like `Ui`, `Work`, `Block`, `Clock`, `Token`: `Clone`
- `Task<T>` / `task::Local<T>`: `Debug` if practical, but do not force `Clone`

#### Service handles

Handles stored in shared registries or passed broadly should be `Send + Sync` unless
they intentionally encode thread affinity.

Explicit exception:

- `Ui` should be `!Send`

#### `Default`

Do not derive `Default` for core runtime handles. They should be explicitly constructed.

Good `Default` candidates are helper state machines like `Latest` only if the zero
state is semantically real.

#### `#[must_use]`

Apply `#[must_use]` to:

- `Task<T>`
- `timer(...)`
- `spawn(...)`
- `spawn_in(...)`

Dropping a task immediately is often a bug unless the caller explicitly chooses `detach()`.

## Runtime Root

The runtime root should be a simple composition object.

```rust
impl Runtime {
    pub fn ui(&self) -> &Ui;
    pub fn work(&self) -> &Work;
    pub fn block(&self) -> &Block;
    pub fn clock(&self) -> &Clock;
    pub fn group(&self) -> Group;
}
```

The root should not be a giant scheduling DSL. It is just the owner of the four
execution domains.

## Execution Domains

## `Ui`

`Ui` is the main-thread-affine execution handle.

### Purpose

- represent UI-thread affinity in the type system
- spawn UI-affine tasks
- post typed events back into the main loop
- answer whether the current thread is the UI thread

### Type

```rust
#[derive(Clone)]
pub struct Ui {
    _not_send: PhantomData<Rc<()>>,
}
```

Making it `!Send` is correct. That is one of the strongest ideas worth keeping.

### API

```rust
impl Ui {
    pub fn is_current(&self) -> bool;

    #[must_use]
    pub fn spawn<F>(&self, future: F) -> task::Local<F::Output>
    where
        F: IntoFuture + 'static,
        F::IntoFuture: 'static,
        F::Output: 'static;

    #[must_use]
    pub fn spawn_in<F>(&self, scope: &Scope, future: F) -> Result<task::Local<F::Output>, group::SpawnError>
    where
        F: IntoFuture + 'static,
        F::IntoFuture: 'static,
        F::Output: 'static;
}
```

### Important rules

- no fake priority API on `Ui`
- `Ui` is an affinity/scheduling handle, not an async editor mutation context
- `Ui` may run async orchestration, but durable editor mutation still happens by
  sending typed events back into the main loop

### What belongs on `Ui`

- UI-only delayed state transitions
- caret blink / panel animation clocks once routed through typed timer events
- orchestration tasks that must stay on the main thread

### What does not belong on `Ui`

- arbitrary blocking work
- LSP/DAP/ACP transport loops
- editor state mutation across arbitrary `await` points

### Why `spawn` is synchronous

`Ui::spawn(...)` should be synchronous and return a `task::Local<T>` immediately.

Why:

- scheduling work is a local control operation, not async work itself
- callers often need the handle immediately for storage, replacement, or cancellation
- making `spawn` async would force "await to start" semantics that add no value
- synchronous spawn keeps the executor API small and unsurprising

The work remains asynchronous. Only the act of scheduling it is synchronous.

## `Work`

`Work` is the normal async background execution domain.

### Purpose

- run `Send + 'static` futures
- support grouped child tasks
- provide the normal async execution surface for background work

### API

```rust
#[derive(Clone)]
pub struct Work {
    // runtime handle
}

impl Work {
    #[must_use]
    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: IntoFuture + Send + 'static,
        F::IntoFuture: Send + 'static,
        F::Output: Send + 'static;

    #[must_use]
    pub fn spawn_in<F>(&self, scope: &Scope, future: F) -> Result<Task<F::Output>, group::SpawnError>
    where
        F: IntoFuture + Send + 'static,
        F::IntoFuture: Send + 'static,
        F::Output: Send + 'static;
}
```

### What belongs on `Work`

- transport loops
- background parsing and indexing
- async symbol/diagnostic/context resolution
- file-watch event pipelines
- async persistence/history work

### What does not belong on `Work`

- blocking filesystem/process work that should be isolated explicitly
- main-thread-affine UI orchestration

### Why `spawn` is synchronous here too

The same rule applies to `Work`:

- task creation/scheduling should be immediate
- the caller gets the handle immediately
- the handle can be stored or canceled without another await boundary

This is especially important for patterns like restartable latest-only work.

## `Block`

`Block` is the blocking offload domain.

### Purpose

- run blocking closures explicitly
- make blocking offload visible at the call site
- keep blocking work out of the normal async worker surface

### API

```rust
#[derive(Clone)]
pub struct Block {
    // blocking pool handle
}

impl Block {
    #[must_use]
    pub fn spawn<T>(&self, f: impl FnOnce() -> T + Send + 'static) -> Task<T>
    where
        T: Send + 'static;

    #[must_use]
    pub fn spawn_in<T>(&self, scope: &Scope, f: impl FnOnce() -> T + Send + 'static) -> Result<Task<T>, group::SpawnError>
    where
        T: Send + 'static;
}
```

### Why this is separate

If a call site is doing blocking work, the API should say so explicitly.

This is cleaner and safer than hiding blocking offload as just another method on
the ordinary async background executor.

## `Clock`

`Clock` owns time and timer semantics.

### Purpose

- current time
- one-shot timers
- future fake-time behavior in tests

### API

```rust
#[derive(Clone)]
pub struct Clock {
    // production or test time source
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

impl Clock {
    pub fn now(&self) -> Instant;

    #[must_use]
    pub fn timer(&self, after: Duration) -> Task<()>;
}
```

### Rule

Timers should live on `Clock`, not be duplicated across `Ui` and `Work`.

That keeps:

- fake-time semantics centralized
- debounce/throttle helpers centralized
- timer behavior consistent across the runtime

## Frame Coordination

The complete target for foreground scheduling, immutable frame production, and
nonblocking terminal presentation is specified in
`responsive-application-architecture.md`. This section defines only the runtime
frame invalidation primitive.

The current redraw/render-lock mechanism in `helix-event/src/redraw.rs` is runtime-global.
That should become an explicit UI-facing primitive instead of a hidden side channel.

Suggested direction:

```rust
pub struct FrameGate {
    // owned by the application/runtime UI path
}

impl FrameGate {
    pub fn request_redraw(&self);
    pub fn lock(&self) -> FrameGuard;
    pub fn begin(&self);
}
```

This gives us a cleaner replacement for:

- `request_redraw()` globals
- `lock_frame()`
- `start_frame()`

The exact name can be refined, but it should become an explicit owned runtime/UI primitive.

## `Task<T>`

`Task<T>` is the join handle for `Send` tasks.

`task::Local<T>` is the join handle for UI-affine non-`Send` tasks.

### Design

The task result should make cancellation explicit.

```rust
pub enum TaskError {
    Canceled,
    Panic,
}

#[must_use]
pub struct Task<T> {
    // join state + cancellation handle
}

#[must_use]
pub struct Local<T> {
    // join state + cancellation handle + !Send marker
}

impl<T> Task<T> {
    pub fn cancel(&self);
    pub fn detach(self);
    pub fn is_finished(&self) -> bool;
}

impl<T> Local<T> {
    pub fn cancel(&self);
    pub fn detach(self);
    pub fn is_finished(&self) -> bool;
}

impl<T> Future for Task<T> {
    type Output = Result<T, TaskError>;
}

impl<T> Future for Local<T> {
    type Output = Result<T, TaskError>;
}
```

### Why this is better than cancellation-by-wrapper

- no second "fallible task" wrapper
- cancellation is explicit in the output type
- easier to reason about from call sites
- better fit for structured concurrency

### Why split `Task<T>` and `task::Local<T>`

This is a real API improvement, not just a naming preference.

It makes one important impossible state harder to represent:

- code should not casually treat a UI-affine join handle as an ordinary cross-thread task handle

Benefits:

- UI-affine task ownership is explicit at the type level
- `Work`/`Block` task handles remain plainly `Send`-oriented
- callers can tell from the return type whether the handle itself is thread-affine

This is the same kind of improvement as making `Ui` itself `!Send`.

### Handle ownership

The returned handle is initially owned by the caller.

The underlying task may also be associated with a `Group` if it was spawned with
`spawn_in(...)`.

So there are two related ownership questions:

- who owns the handle?
- which group, if any, owns the task as part of structured cancellation and join accounting?

That handle can then be:

- stored in editor or component state
- replaced when restarting work
- awaited by orchestration code
- detached when the caller no longer wants ownership

Typical ownership patterns:

- component field for restartable UI work (`task::Local<T>`)
- subsystem field for long-lived transports
- `Latest` owning one task
- `Group` owning a set of child tasks through its token subtree and join accounting

This is one of the main reasons the spawn APIs should be synchronous: the caller
gets lifecycle control immediately.

### No `spawn_detached` convenience

The runtime should not offer `spawn_detached(...)` on the primary executor APIs.

Why:

- it encourages fire-and-forget work by default
- it makes ownership invisible at the call site
- it weakens `#[must_use]` as a guardrail

If a caller truly wants a detached task, it should call:

```rust
work.spawn(async move { ... }).detach();
```

That keeps the ownership decision explicit.

### Drop behavior

Default policy:

- dropping a non-detached task cancels it
- detached tasks continue to completion

That is the most ergonomic and least surprising default for Helix.

### Detached task policy

Detached tasks are still real tasks. They simply no longer have a caller-owned
join handle.

Rules:

- detached tasks should normally report failures through typed events or another explicit sink
- detach should be rare in feature code
- detaching a grouped task should not remove it from the group's cancellation or join semantics

## `Token`

`Token` is the runtime's explicit cancellation primitive.

### API

```rust
#[derive(Clone, Debug)]
pub struct Token {
    // cancellation state
}

impl Token {
    pub fn child(&self) -> Self;
    pub fn cancel(&self);
    pub fn is_canceled(&self) -> bool;
    pub async fn canceled(&self);
}
```

### Why this must exist even if `Task` can be canceled

Task cancellation and cooperative cancellation are not the same thing.

We need `Token` for:

- task trees
- restartable work
- subprocess orchestration
- user-cancelable interactive operations
- long-lived loops that must stop cooperatively

## `Group`

`Group` is the structured-concurrency primitive.

### Purpose

- own a cancellation subtree
- spawn related child tasks
- wait for the group to join

### API

```rust
pub struct Group {
    token: Token,
    // shared child task accounting state
}

#[derive(Clone)]
pub struct Scope {
    // spawn authority for one group
}

pub enum SpawnError {
    Closed,
}

impl Group {
    pub fn token(&self) -> &Token;
    pub fn scope(&self) -> Scope;
    pub fn child(&self) -> Group;

    pub fn cancel(&self);
    pub async fn join(self);
}
```

### Join semantics

`join()` must not be vague. It should mean:

- no tracked child tasks remain running
- all completion accounting for this group has settled

That implies `Group` cannot just be a token wrapper. It also needs shared task
accounting state.

So the intended implementation model is:

- every `spawn_in(...)` registers one child with the group
- child completion or cancellation decrements the group's live task count
- `join()` waits until that count reaches zero

This is an important design correction. Without explicit accounting, `join()` would
be underspecified and easy to misuse.

`join()` should not return a bespoke error if its only job is waiting for group
settling. Child task failures belong to child task/result paths, not to `Group::join()`.

### Why not make every spawn scoped?

Because Helix has many root-owned long-lived tasks:

- LSP transport loops
- ACP transport loops
- file watchers
- background subsystems

If every spawn required a scope, we would either invent fake global scopes or
thread scope references everywhere. Both are worse than a clean split between:

- root spawn
- grouped child spawn

### Grouped spawn API

The executor handles should expose `spawn_in(...)` directly.

```rust
impl Work {
    #[must_use]
    pub fn spawn_in<F>(
        &self,
        scope: &Scope,
        future: F,
    ) -> Result<Task<F::Output>, SpawnError>
    where
        F: IntoFuture + Send + 'static,
        F::IntoFuture: Send + 'static,
        F::Output: Send + 'static;
}

impl Ui {
    #[must_use]
    pub fn spawn_in<F>(&self, scope: &Scope, future: F) -> Result<task::Local<F::Output>, SpawnError>
    where
        F: IntoFuture + 'static,
        F::IntoFuture: 'static,
        F::Output: 'static;
}

impl Block {
    #[must_use]
    pub fn spawn_in<T>(&self, scope: &Scope, f: impl FnOnce() -> T + Send + 'static) -> Result<Task<T>, SpawnError>
    where
        T: Send + 'static;
}
```

`Group` owns cancellation and join semantics. `Scope` owns spawn authority.

That is better than passing `&Group` everywhere because it makes it harder to
accidentally hand full lifecycle authority to helper code that only needs spawn access.

### Closed-scope semantics

Spawning into a closed or fully joined group should not silently succeed and should
not silently no-op.

That is why `spawn_in(...)` returns `Result<_, SpawnError>`.

This is another place where the API should guard callers instead of relying on docs.

### Ownership tree

The intended ownership model is:

- `Application` owns a small number of root groups
- subsystem owners own child groups beneath those roots
- feature-local restartable work owns `Latest`/`Debounce` or a small child group

Example hierarchy:

- app root group
  - editor services group
    - file watch group
    - diagnostics group
    - completion group
  - transport group
    - lsp client group(s)
    - dap client group(s)
    - acp agent group(s)
  - ui helper group

This keeps lifecycle ownership explicit without requiring lexical borrowed scopes.

### No borrowed async scope as the foundation

We should not build the runtime around borrowed lexical async scopes that require:

- unsafe lifetime extension
- blocking `Drop`

If we ever add a lexical convenience wrapper, it should compile down to owned
group semantics, not to borrowed async scope magic.

## `Sender<T>` / `Receiver<T>`

The runtime needs typed mailboxes.

### API

```rust
pub fn channel<T>(bound: usize) -> (Sender<T>, Receiver<T>);

pub enum Closed<T> {
    Closed(T),
}

pub enum TrySend<T> {
    Full(T),
    Closed(T),
}

impl<T> Sender<T> {
    pub async fn send(&self, value: T) -> Result<(), Closed<T>>;
    pub fn try_send(&self, value: T) -> Result<(), TrySend<T>>;
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T>;
}
```

### Mailbox rules

- default mailboxes should be bounded
- unbounded mailboxes should be rare and justified explicitly
- the core mailbox API should not offer a blocking send
- synchronous boundary adapters should use `try_send` and an explicit policy for
  full queues (drop, coalesce, status, or retry through a dedicated helper)
- normal runtime code should prefer `send` or `try_send`

This avoids one of the easiest runtime footguns: hidden thread blocking through a
convenience send that looks harmless.

### Why mailboxes matter

They are the correct replacement direction for:

- callback-based `Jobs`
- runtime-local global queues
- ad hoc event handoff channels hidden in subsystems

Background or UI tasks should do work and send typed events. The application or
editor loop should own the receiving side.

## High-Level Helpers

These helpers should live in the runtime layer because they are runtime semantics,
not feature-local inventions.

## `Latest`

For restartable latest-only work.

```rust
pub struct Latest {
    token: Token,
    task: Option<Task<()>>,
}

impl Latest {
    pub fn restart<F>(&mut self, work: &Work, future: F)
    where
        F: IntoFuture<Output = ()> + Send + 'static,
        F::IntoFuture: Send + 'static;

    pub fn restart_with<F>(&mut self, work: &Work, f: F)
    where
        F: AsyncFnOnce(Token) -> () + Send + 'static;
    pub fn cancel(&mut self);
    pub fn is_running(&self) -> bool;
}
```

Use for:

- completion refresh
- signature help
- diagnostics pulls
- document colors

## `Debounce`

For delayed latest-only work.

```rust
pub struct Debounce {
    delay: Duration,
    latest: Latest,
}

impl Debounce {
    pub fn new(delay: Duration) -> Self;

    pub fn restart<F>(
        &mut self,
        work: &Work,
        clock: &Clock,
        future: F,
    )
    where
        F: IntoFuture<Output = ()> + Send + 'static,
        F::IntoFuture: Send + 'static;
}
```

## `Gate<T>`

For readiness barriers and queued work.

```rust
pub enum Push<T> {
    Buffered,
    Ready(T),
}

pub struct Gate<T> {
    // closed/open state plus buffered items
}

impl<T> Gate<T> {
    pub fn push(&mut self, item: T) -> Push<T>;
    pub fn open(&mut self) -> Vec<T>;
    pub fn is_open(&self) -> bool;
}
```

Use for:

- LSP init barriers
- DAP init barriers
- any feature that must queue requests until a phase completes

## `Throttle`

For rate-limited publication or redraw-like work.

The exact API can follow `Debounce` later once we implement it.

## `AsyncFnOnce` vs `Future`

Stable Rust 1.94 gives us both.

### Rule

- use `impl Future` for core spawn APIs
- use `impl AsyncFnOnce` or `impl AsyncFn` for higher-level helpers that inject
  arguments, snapshots, or scoped context

### `for<'a>` and borrowed async closures

Stable Rust lets us express higher-ranked bounds like `for<'a>` and use async
closures more ergonomically, but we should still be careful about borrowed async
inputs.

Rule:

- do not design core runtime helpers around borrowed inputs that must live across
  `await` points unless the borrowing semantics are the main point of the API
- prefer owned cloneable handles (`Token`, snapshots, senders) over borrowed
  handles in async helper signatures

Good:

```rust
pub fn spawn_with<F, T>(&self, f: F) -> Task<T>
where
    F: AsyncFnOnce(Token) -> T + Send + 'static,
    T: Send + 'static;
```

Risky unless truly necessary:

```rust
pub fn spawn_with<F, T>(&self, f: F) -> Task<T>
where
    F: for<'a> AsyncFnOnce(&'a Token) -> T + Send + 'static;
```

Why:

- owned arguments reduce lifetime coupling
- borrowed async inputs are one of the easiest ways to make APIs hard to compose
- this avoids sliding back toward borrowed scoped-task semantics

### Why `impl Future` for core spawn is still right

The core runtime primitives do not need to know how a future was constructed.

So these should stay like:

```rust
ui.spawn(async move { ... });
work.spawn(async move { ... });
```

That is the simplest and most composable shape.

### Where `AsyncFnOnce` is better

Use it for orchestration helpers that supply input into the async work.

Example:

```rust
pub fn spawn_with<T>(
    &self,
    f: impl AsyncFnOnce(Token) -> T + Send + 'static,
) -> Task<T>
where
    T: Send + 'static;
```

This is appropriate for helpers that inject:

- a cancellation token
- a snapshot
- a per-task scoped helper

### What not to do

Do not rewrite every spawn-like API to take `AsyncFnOnce` just because the trait exists.

That would make normal task spawning more awkward without adding semantic value.

## Typestate And Generics

Rust 1.94 gives us enough expressive power to use typestate and generics where
they truly clarify the API. We should use them selectively, not everywhere.

### Good typestate candidates

#### Runtime builder

If runtime construction becomes multi-step across platforms/tests, a typestate
builder is a good fit.

Example:

```rust
pub struct Builder<U, W, B, C> {
    ui: U,
    work: W,
    block: B,
    clock: C,
}

pub struct Missing;
pub struct Ready<T>(T);
```

This is useful only if runtime construction is actually staged. If construction is
simple, a plain constructor is better.

#### Spawn-with helpers

Helpers that inject a token, snapshot, or mailbox can use generics and
`AsyncFnOnce` cleanly.

These are good candidates for generic helper functions because they are short-lived
and their lifecycles are explicit.

### Poor typestate candidates

Do *not* use typestate for:

- `Task<T>` detach/cancel state
- `Group` runtime lifecycle
- `Gate<T>` open/closed state
- long-lived runtime handles like `Ui`, `Work`, `Block`, or `Clock`

Why:

- those states are dynamic at runtime
- typestate there would make the API harder to store and compose
- ordinary methods are clearer for long-lived service objects

### Generic API guidance

Use generics where they reduce ceremony and improve composability.

Good:

- `spawn<F: IntoFuture>(...)`
- `spawn_with<F: AsyncFnOnce(...)>(...)`
- typed mailboxes `Sender<T>` / `Receiver<T>`
- `Gate<T>`

Avoid generics when they turn the primary API surface into type algebra.

The runtime should feel easy to use at the call site.

### Make impossible states hard to represent

This runtime should lean on types where they remove ambiguity.

Good examples in this spec:

- `TaskError` instead of silently treating cancellation like a normal success path
- `TrySend<T>` with `Full(T)` vs `Closed(T)` instead of a bool-like failure
- `gate::Push<T>` with `Buffered` vs `Ready(T)` instead of `Option<T>`
- separate execution-domain handles (`Ui`, `Work`, `Block`, `Clock`) instead of one vague executor type

Where we should *not* force this principle too far:

- dynamic service state like group open/closed/canceled lifecycles
- long-lived handles whose state changes at runtime

In those places, ordinary methods and explicit enums are clearer than typestate.

## Async Traits

Native async trait methods are stable and should be used where they make the API
cleaner.

### Good async trait boundaries

- persistence backends
- context/symbol resolvers
- remote collaboration backends
- long-lived subsystem backends that are naturally async

### Example

```rust
pub trait History: Send + Sync {
    async fn load_scope(&self, scope: &assistant::thread::Scope) -> anyhow::Result<Vec<assistant::history::Stub>>;
    async fn save_thread(&self, thread: &assistant::thread::Thread) -> anyhow::Result<()>;
}
```

### What should stay synchronous

- runtime-owned task spawning APIs
- main-loop state mutation
- event/effect application
- local scheduling handles

The runtime is not anti-async. It is explicit about where async belongs.

## UI Mutation Model

Even with a dedicated `Ui` handle, Helix should keep one central mutation path.

The preferred model is:

- async work emits typed events
- `Application` and `Editor` receive those events
- state mutation happens synchronously in the main loop

This is a deliberate design choice. It keeps Helix simpler than a system where
UI-affine async contexts directly mutate app state across awaits.

## Unified Event Ingress

The single public ingress facade and its multiple private semantic stores are
specified in `responsive-application-architecture.md`. In particular, one source
of truth does not require one physical FIFO; reliable, latest, fold, pulse, and
telemetry traffic have different backpressure semantics.

Long term, `Application` should own one semantic ingress facade for
runtime-originated events. As specified in
`responsive-application-architecture.md`, that facade uses multiple private
stores with explicit reliable/latest/fold/pulse/ring behavior rather than one
head-of-line-blocking FIFO.

Example shape:

```rust
pub enum AppEvent {
    Term(TermEvent),
    Editor(EditorEvent),
    Runtime(RuntimeEvent),
    Shutdown,
}

pub enum RuntimeEvent {
    Status(StatusMessage),
    Redraw,
    Timer(TimerId),
    Task(TaskEvent),
}
```

This is cleaner than today's split between:

- `Application::event_loop_until_idle`
- `Editor::wait_event`
- runtime-local queues
- per-subsystem incoming streams

We do not need to collapse everything into one enum immediately, but this should
be the target shape.

## Main-Thread UI Ingress

This is the missing piece behind the current `Callback` / pre-typed-ingress blocker.

The runtime plan is not complete unless it also defines how async work requests
main-thread UI/compositor actions without closures.

### Core rule

Arbitrary `FnOnce(&mut Editor, &mut Compositor)` callbacks are not the final API.

They should be replaced with a typed command set that is:

- data-oriented
- domain-grouped
- applied centrally on the main thread

### Target shape

```rust
pub enum AppEvent {
    Term(TermEvent),
    Editor(EditorEvent),
    Runtime(RuntimeEvent),
    Shutdown,
}

pub enum RuntimeEvent {
    Status(StatusMessage),
    Redraw,
    Timer(TimerId),
    Task(RuntimeTaskEvent),
    Ui(ui::Command),
}

pub mod ui {
    pub enum Command {
        Layer(layer::Command),
        Completion(completion::Command),
        Signature(signature::Command),
        Picker(picker::Command),
        Prompt(prompt::Command),
        Info(info::Command),
    }
}
```

`UiCommand` should travel inside `RuntimeEvent::Ui`, not as a separate top-level
`AppEvent` variant. It is still runtime-originated main-thread ingress.

This is the correct replacement for `Callback::EditorCompositor`.

### Why this is better than one giant enum

One giant flat enum would work, but it would grow without structure.

The grouped shape is better because:

- it keeps each UI domain locally coherent
- it avoids one monolithic variant list
- it keeps matching and testing easier
- it still gives us one canonical ingress path

### Why this is better than a boxed main-thread closure

Because closures keep the semantics implicit.

Typed commands give us:

- clear ownership of data
- a testable and inspectable ingress schema
- easier evolution of the command surface
- no hidden borrow/capture behavior

### Layer command direction

The most sensitive area is layer/popup management.

Instead of pushing arbitrary components through callbacks, we should use typed specs.

Example:

```rust
pub mod layer {
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Id(Cow<'static, str>);

    pub enum Command {
        Open {
            id: Id,
            open: Open,
        },
        Replace {
            id: Id,
            open: Open,
        },
        Close {
            id: Id,
        },
    }

    pub enum Open {
        Picker(picker::Spec),
        Prompt(prompt::Spec),
        Popup(popup::Spec),
        Signature(signature::Spec),
        Completion(completion::Spec),
    }
}
```

This is an important API correction.

The command layer should not use raw strings for runtime-facing identity if we can
avoid it. Even a light newtype gives us:

- one place to document the identity rules
- less stringly-typed main-thread ingress
- easier future refinement if ids later need namespaces or stronger typing

The main-thread application/compositor code then owns the actual component
construction from those typed specs.

That is the cleanest replacement for ad hoc compositor-touching closures.

### Concrete UI ingress module layout

The runtime-facing UI ingress should use path-oriented modules with short names.

Recommended layout:

- `helix-term/src/runtime/ui/mod.rs`
- `helix-term/src/runtime/ui/command.rs`
- `helix-term/src/runtime/ui/apply.rs`
- `helix-term/src/runtime/ui/layer.rs`
- `helix-term/src/runtime/ui/completion.rs`
- `helix-term/src/runtime/ui/signature.rs`
- `helix-term/src/runtime/ui/picker.rs`
- `helix-term/src/runtime/ui/prompt.rs`
- `helix-term/src/runtime/ui/info.rs`

Responsibilities:

- `command.rs` defines the top-level grouped `ui::Command`
- domain modules define their own `Command` and `Spec` types
- `apply.rs` is the central main-thread dispatcher over those domain commands

Do not create flat files like:

- `runtime/ui_command.rs`
- `runtime/ui_apply.rs`

The module tree should carry the context.

### Domain command rule

Each UI command domain should have exactly two public concepts:

- `Command`
- `Spec`

For example:

```rust
pub mod picker {
    pub enum Command {
        Open { id: layer::Id, spec: Spec },
        Replace { id: layer::Id, spec: Spec },
        Close { id: layer::Id },
    }

    pub struct Spec {
        pub title: String,
        pub source: Source,
    }
}
```

This keeps the schema regular and predictable across domains.

### Policy

When a new async workflow needs editor+compositor mutation, the default answer
should be:

- define or extend a typed `ui::Command` variant family
- send it through the mailbox
- apply it centrally on the main thread

Not:

- add another callback path
- capture editor/compositor in a closure
- invent a subsystem-local main-thread bridge

### `RuntimeTaskEvent` vs `UiCommand`

This split must stay crisp or we will recreate ambiguity.

Use:

- `RuntimeTaskEvent` for editor-side effects that do not require compositor-owned
  widget/layer construction
- `UiCommand` for compositor/layer/widget commands

Examples:

- apply transaction if current -> `RuntimeTaskEvent`
- attach document colors -> `RuntimeTaskEvent`
- open picker/popup/prompt/completion/signature layer -> `UiCommand`

Rule:

If the action needs `Compositor` to create, replace, or close a UI surface, it is
`UiCommand`. Otherwise it should bias toward a typed task/editor event.

This distinction should be explicit enough that feature authors rarely need to ask
which path to use.

### Neutral apply modules

When a typed ingress path needs logic that currently lives in a handler or UI module,
extract the pure apply logic into a neutral module rather than crossing modules in a cycle.

Examples:

- editor-only apply logic -> `effect.rs`
- UI-spec application -> `runtime/ui/<domain>/apply.rs`

Rule:

- handlers may produce typed ingress events
- runtime ingress may apply neutral effects
- handlers and runtime modules should not depend on each other in circles

## Runtime Requirements From The Current Backlog

The current Helix fork backlog points to several missing runtime primitives.

This runtime must support:

- non-blocking document open and other file I/O offload
- UI prompts that pause and later resume workflows
- cancellable subprocess-backed work
- restartable latest-only preview/annotation work
- explicit cancellation of pending callbacks or deferred actions
- init/readiness barriers for transports
- cancellation-safe coordination primitives
- streaming partial updates
- consistent timeout policy for subprocesses and external commands

Those requirements are why the runtime spec includes:

- `Block`
- `Token`
- `Group`
- `wait::Set`
- `Latest`
- `Debounce`
- `Gate`
- typed mailboxes

## Shutdown And Wait Semantics

This is the other missing piece behind the old `RuntimeDispatch` bundle.

The runtime story is not complete unless we define how the application waits for
specific tasks before exit.

### Requirement

Some tasks are:

- long-lived services that should be canceled on shutdown
- one-shot operations that should be awaited before shutdown completes

Those are different responsibilities and should not be conflated.

### Proposed split

- `Group` handles service/task-tree cancellation and join semantics
- `wait::Set` handles explicitly awaited one-shot tasks

### API

```rust
pub mod wait {
    pub struct Set<T> {
        tasks: FuturesUnordered<Task<T>>,
    }

    impl<T> Set<T> {
        pub fn push(&mut self, task: Task<T>);
        pub async fn next(&mut self) -> Option<Result<T, TaskError>>;
        pub async fn drain(self) -> Vec<Result<T, TaskError>>;
        pub fn is_empty(&self) -> bool;
    }
}
```

`drain()` should preserve completion order, not registration order.

That means `wait::Set<T>` should internally behave like a completion set, not a
plain `Vec<Task<T>>` that is awaited sequentially.

### Rule

If the application must wait for a specific task before exit, it should register
it in `wait::Set` explicitly.

Do not rely on:

- hidden callback queues
- implicit runtime shutdown ordering
- unrelated root task handles that happen to still exist

This is the clean replacement for the old `wait_futures` responsibility that used
to be hidden inside `RuntimeDispatch`.

## Integration With Current Helix

## Concrete Refactor Map

This section makes the "no two ways" rule explicit at the file level.

### `helix-term/src/job.rs`

Current role:

- pseudo-executor
- callback queue
- async-to-main-loop bridge

Target:

- fully removed from the target architecture
- replaced by `Task<T>` + typed `mailbox` events

What must be removed as part of the runtime migration:

- `Callback::{Editor, EditorCompositor}` as the primary re-entry mechanism
- global `JOB_QUEUE` as the main app inbox for async work

End state:

- this module is deleted
- the public architecture uses `Task`, `Token`, `Group`, and `mailbox` only

There should not be a second first-class "job" concept once the new runtime lands.

### `helix-event/src/cancel.rs`

Current role:

- generation-based cancellation for one restartable subtask

Target:

- port restartable call sites onto `Token` + `Latest`
- delete or drastically shrink the old helper in the same migration window

What should not happen:

- leaving both `TaskController` and `Token` as equal first-class cancellation stories forever

### `helix-event/src/debounce.rs`

Current role:

- feature-specific debounced hook loop helper

Target:

- `Debounce` / `Latest` become the canonical primitives
- existing hook-specific debounce loops are ported and the old helper is retired

### `helix-event/src/redraw.rs`

Current role:

- runtime-local redraw notify
- runtime-local render lock

Target:

- owned `FrameGate` or equivalent UI/runtime primitive
- no redraw globals in the final architecture

### `helix-event/src/status.rs`

Current role:

- runtime-local status channel setup and reporting

Target:

- status messages travel through typed mailboxes/runtime events
- no separate status-global subsystem in the final architecture

### `helix-event/src/runtime.rs`

Current role:

- runtime-local globals for integration-test isolation

Target:

- explicit runtime-owned state instead of runtime-local statics
- integration tests should use the test runtime, not runtime-local shadow globals

### `helix-term/src/host.rs`

Current role:

- `UiHost::request_timer` just spawns a sleep and requests redraw

Target:

- timers come from `Clock`
- timer expiry enters the app via typed timer events

### `helix-view/src/editor.rs`

Current role:

- owns `idle_timer`, `redraw_timer`, and a second async fan-in loop in `wait_event()`

Target:

- editor no longer owns ad hoc timer primitives directly
- editor consumes typed events from the application/runtime ingress path

### `helix-term/src/application.rs`

Current role:

- top-level select loop over terminal, jobs, status, UI requests, editor events

Target:

- one clearer runtime/app ingress story with typed mailboxes
- still the central mutation path, but with cleaner event sources

### `helix-term/src/handlers.rs`

Current role:

- composition root for many `AsyncHook` workers

Target:

- composition root for runtime-native helpers and workers
- feature workers should stop hand-rolling debounce/restart/cancel patterns

### `helix-lsp`, `helix-dap`, `helix-acp`

Current role:

- each transport starts several Tokio tasks with slightly different conventions

Target:

- shared runtime conventions for subprocess/task ownership, cancellation, and event delivery
- protocol differences stay local, runtime patterns do not

## One Runtime Way To Do Each Thing

The runtime refactor should remove duplicate async patterns instead of adding a
second way to do the same work.

This table is the migration contract.

| Current pattern | Target pattern | Notes |
|-----------------|----------------|-------|
| `tokio::spawn(async move { ... })` from feature code | `runtime.work().spawn(...)` or `runtime.ui().spawn(...)` | Ambient Tokio spawn should stop being the default feature API |
| `tokio::task::spawn_blocking(...)` or ad hoc blocking wrappers | `runtime.block().spawn(...)` | Blocking work becomes explicit at the call site |
| `helix-term::job::{Job, Jobs, Callback}` | `Task<T>` + typed `mailbox` events | The old job queue is removed, not retained as a peer API |
| `helix-event::TaskController` | `Token` + `Latest` | Keep generation/restart semantics as a helper, not the primary primitive |
| `helix-event::AsyncHook` | `Debounce` / `Latest` / typed mailbox loops | Hook-specific debounce loops should converge on shared helpers |
| `helix_event::request_redraw()` globals | typed `RuntimeEvent::Redraw` through mailbox | Avoid runtime-local redraw globals as the long-term path |
| status message globals | typed runtime/app events | Same reason as redraw |
| editor-local ad hoc timers | `clock::Clock` + `TimerId` + typed timer events | One timer service only |
| fake null-key cancellation hacks | explicit pending-action cancellation primitive | Do not model cancellation as fake input |
| transport-specific init barriers | `Gate<T>` | LSP/DAP/ACP should not all reinvent readiness queues |

If a new feature proposes another async pattern outside this table, the burden is
on that design to justify why the existing runtime primitives are insufficient.

### Raw Tokio policy

Once `helix-runtime` exists, ordinary feature code should not call:

- `tokio::spawn`
- `tokio::task::spawn_blocking`
- ad hoc `tokio::time::sleep`

directly.

Those calls should be confined to:

- the runtime crate itself
- low-level transport/process internals that have not been migrated yet

This keeps the architecture from regressing back into ambient async patterns.

## `helix-term/src/main.rs`

This should become the explicit runtime construction point.

Instead of relying only on ambient `#[tokio::main]`, the application should create
and receive a `Runtime` explicitly.

## `helix-term/src/application.rs`

This should become the main runtime consumer.

Responsibilities:

- own top-level mailbox receivers
- fan in terminal input and runtime-originated events
- translate runtime-originated events into synchronous editor/application mutation

## `helix-view/src/editor.rs`

`Editor::wait_event()` should shrink or disappear as a second fan-in hub.

The long-term direction is one clearer application-owned event ingress path.

## `helix-term/src/job.rs`

This module should be removed as part of the migration.

The new runtime should not ship with both `job` and `task` as first-class terms.

## `helix-event`

Long term:

- `cancel.rs` should be folded into the runtime or deleted as part of the migration
- `debounce.rs` should be replaced by runtime-native `Debounce` / `Latest`
- runtime-local globals for redraw/status/job queue should be retired

## Testing Model

The runtime must have a proper test story.

### Required capabilities

- fake clock
- deterministic draining
- visibility into pending tasks and timers
- stepwise scheduling
- optional randomized background ordering in tests

### Crucial rule

The test runtime must preserve the same observable executor contract as production.

It may be more inspectable and more controllable, but it should not introduce fake
semantics that the production runtime does not honor.

## API Footguns To Avoid

This section is the explicit checklist for runtime API hazards.

### 1. Detached task leaks

`detach()` is necessary, but it is also a footgun if used casually.

Rules:

- `Task<T>` is `#[must_use]`
- detach should be explicit and named
- long-lived detached tasks should normally belong to a subsystem or group owner,
  not to random call sites
- no `spawn_detached(...)` convenience on the main runtime handles

### 2. Silent backpressure failure

If mailboxes are unbounded or if send paths silently drop too much, the runtime
becomes impossible to reason about.

Rules:

- bounded by default
- `try_send` failure is visible to the caller
- no blocking send in the core mailbox API
- synchronous edge adapters must choose an explicit full-queue policy

### 3. Hidden blocking work on `Work`

If we allow blocking work to creep onto `Work`, the API loses its semantic value.

Rule:

- blocking work goes through `Block`

### 4. Root-spawn sprawl

If feature code uses root spawn everywhere, grouped structure becomes meaningless.

Rule:

- root spawn for top-level owners
- grouped spawn for ordinary feature work

### 5. Group authority leakage

If helpers receive full `Group` values when they only need spawn authority, they
can accidentally cancel or join work they should not own.

Rule:

- pass `group::Scope` for spawn authority
- keep `Group` for code that owns cancellation and join semantics

### 6. Ambiguous group completion semantics

`finish` is vague. `join` and `cancel` are clearer.

Rules:

- `cancel()` means request cancellation
- `join()` means wait for the group to settle

### 7. Duplicate timer APIs

If `Ui`, `Work`, and `Clock` all grow their own timer APIs, we recreate drift.

Rule:

- one timer service: `Clock`

### 8. Cross-thread handle confusion

If UI-affine task handles and ordinary background task handles share exactly the
same type, callers can accidentally treat them as interchangeable.

Rule:

- `Ui` returns `task::Local<T>`
- `Work` and `Block` return `Task<T>`

### 9. Async mutation creep

If too many APIs directly mutate editor state across `await`, the single-writer
model erodes.

Rule:

- async work emits events
- main loop applies them synchronously

### 10. Priority without semantics

Do not expose priority or lane APIs unless all backends and tests honor them.

### 11. Feature-local reinvention

If features keep inventing their own debounce/latest/barrier/task wrappers, the
runtime layer has failed.

Rule:

- shared helper belongs in the runtime crate once it appears in more than one subsystem

## Migration Plan

### Phase 1

- add `helix-runtime`
- add `Runtime`, `Ui`, `Work`, `Block`, `Clock`, `Task`, `Token`, `Group`, `mailbox`
- add `FrameGate`, `Latest`, `Debounce`, and `Gate`
- wire `Application` to own a runtime instance

### Phase 2

- port `helix-term::job` call sites to `Task<T>` + typed mailboxes
- delete `helix-term/src/job.rs`
- remove `JOB_QUEUE`
- remove callback-based async re-entry as a primary mechanism

### Phase 3

- move debounce/cancel helpers onto runtime primitives
- remove `helix-event::AsyncHook` and `TaskController` from primary use sites
- retire or sharply reduce `helix-event::{cancel,debounce}`

### Phase 4

- replace callback-heavy job re-entry with typed mailboxes
- replace redraw/status runtime-local globals with owned runtime primitives
- remove `helix-event::runtime` globals from the normal architecture

### Phase 5

- migrate LSP/DAP/ACP transport runners onto shared runtime conventions
- keep protocol-specific logic local to each subsystem

### Phase 6

- add deterministic runtime testing and fake time
- migrate async-heavy tests gradually

## Rejected Designs

### 1. Raw Zed-style two-executor port

Rejected because it is too coarse for Helix and hides the blocking/timer story.

### 2. One generic executor handle

Rejected because it erases execution-domain semantics that matter at call sites.

### 3. Scope-only spawn APIs

Rejected because Helix has many root-owned long-lived tasks and would end up with
fake ambient scopes.

### 4. Borrowed async scoped tasks as the foundation

Rejected because they pressure the design toward unsafe lifetime extension and
blocking `Drop`.

### 5. Callback queues as the primary re-entry mechanism

Rejected because typed mailboxes and events are cleaner, more testable, and more
extensible.

## Defaults

If implementation started from this spec now, the default answers should be:

- keep `Ui`, `Work`, `Block`, and `Clock` as separate execution domains
- no foreground priority API
- no lane/priority API until semantics are real in production and tests
- `Task<T>` returns `Result<T, TaskError>`
- dropping a non-detached task cancels it
- `Token` is the primary cancellation primitive
- `Group` is the structured child-task primitive
- `Clock` is the only timer service
- root spawn and grouped spawn both exist
- `impl Future` for core spawn APIs
- `impl AsyncFnOnce` for orchestration helpers that inject a token or snapshot
- typed mailboxes replace callback soup as part of the migration

## Definition Of Done

The runtime architecture is in the right shape when:

- execution domains are explicit
- task handles are first-class
- cancellation is structured
- timers are centralized
- event delivery is typed
- the editor still mutates state through one synchronous main loop path
- feature code stops reinventing debounce/restart/cancel logic
- tests can run time- and task-sensitive code deterministically

At that point Helix has a real runtime architecture, not just a pile of async helpers.
