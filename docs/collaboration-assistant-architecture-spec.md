# Collaboration and Assistant Architecture Spec

Status: draft

This document defines the target architecture for collaboration, follow, assistant
sessions, and prompt-context attachments in the Helix fork.

It is intentionally broader than ACP. ACP is the first client of the design, not
the definition of it.

The goal is to build the best long-term APIs and patterns we can, while keeping
the implementation tractable and Rust-idiomatic. This document is therefore both:

- a product/interaction spec for follow, sessions, and context
- a systems/API spec for ownership, capability traits, registries, effects, and
  data structures

This spec describes the target architecture. Temporary migration bridges are not
part of that target and should not be treated as design endpoints.

## Why This Exists

The current ACP work proved several things:

- The message list was worth extracting into reusable widgets.
- The ACP panel currently owns too much durable state.
- Session identity exists in the ACP protocol, but not in our local architecture.
- Scratch buffers, session switching, and follow behavior expose architectural
  gaps, not just UI bugs.
- "Follow the agent" is not a panel trick; it is a collaboration primitive.

At the same time, a deep pass through Zed shows that:

- A generic collaboration/follow substrate is the right direction.
- Typed context references are the right direction.
- Registry-driven extensibility is the right direction.
- Mixing protocol serialization, follow logic, and panel-local session state into
  one abstraction creates long-term debt.

This spec uses those lessons to define a cleaner split for Helix.

## Scope

This spec covers:

- generic collaboration primitives
- follow and location tracking
- assistant sessions and tabbing
- prompt-context attachments for selection, symbol, and future kinds
- scratch/review surfaces as collaboration-aware editor surfaces
- the extension points needed for future human collaboration

This spec does not attempt to fully solve:

- concurrent shared editing over the network
- final remote transport protocols for generic collaboration
- final visual polish or keybindings

Those parts are influenced by this design, but they are not fully implemented by
this document.

## Primary Goals

1. Build a collaboration substrate in `helix-view` that is generic and reusable.
2. Build assistant session state on top of that substrate, not inside the ACP panel.
3. Support session tabs, follow, and context attachments without baking in ACP-only assumptions.
4. Keep APIs explicit, typed, and extensible without turning them into vague abstractions.
5. Preserve room for future human collaboration, shared notes, debugger follow,
   and other collaborative surfaces.

## Non-Goals

- Rebuilding Helix into a full multiplayer editor in one step.
- Designing network codecs for every future collaborative surface right now.
- Making every UI component collaboration-aware.
- Encoding every dynamic runtime state transition as typestate.

## Guiding Principles

### 1. Generic core, domain-specific clients

Collaboration is generic. Assistant threads are not.

The core collaboration layer should know about:

- participants
- locations
- presence
- surfaces
- follow

The assistant layer should know about:

- threads
- entries
- turns
- context attachments
- change provenance

### 2. State ownership belongs to `Editor`

`helix-view/src/editor.rs` is already the real ownership root for:

- tree views
- documents
- component-owned docs
- component-owned views
- the frontend model

The new collaboration and assistant stores should therefore live in `Editor`, not
in `helix-term/src/ui/acp.rs`.

### 3. Capability traits should stay small

Do not create one giant collaboration trait. Use small capability traits that can
be composed or registered independently.

### 4. Stable ids over indexes

No durable state should be keyed by row index or temporary panel ordering.

Use stable ids for:

- surfaces
- entries
- turns
- sessions
- participants

### 5. Protocol types stay at the edge

Generic collaboration traits should not be shaped around ACP JSON-RPC or around a
future multiplayer wire protocol.

Adapters can translate between transport formats and local typed state.

### 6. Extensibility should be explicit

For every abstraction in this spec, we should ask:

- is this a closed semantic category or an open extension point?
- should this be an enum, a trait, a registry, or a typestate builder?
- what happens if we add a debugger session, a shared note, or a non-editor
  collaborative panel later?

## What Zed Gets Right

The most important Zed files for this design are:

- `E:\zed\crates\workspace\src\workspace.rs`
- `E:\zed\crates\workspace\src\item.rs`
- `E:\zed\crates\project\src\project.rs`
- `E:\zed\crates\editor\src\editor.rs`
- `E:\zed\crates\agent_ui\src\agent_panel.rs`
- `E:\zed\crates\agent_ui\src\connection_view\thread_view.rs`
- `E:\zed\crates\acp_thread\src\mention.rs`
- `E:\zed\crates\agent_ui\src\mention_set.rs`
- `E:\zed\crates\assistant_slash_commands\src\selection_command.rs`

### Zed strengths we should copy

1. One collaboration/follow substrate for humans and the agent.
2. Opt-in capability layering instead of one universal item API.
3. Registries for followable/reconstructable/openable items.
4. Typed context references instead of pasted blobs.
5. A first-class location model for the agent.
6. Follow pause semantics on local user interaction.

### Zed tradeoffs we should avoid

1. `FollowableItem` mixes follow semantics with protocol conversion.
2. Agent session UI remains too panel-local.
3. Agent follow partly enters through `Project::set_agent_location(...)`, which is
   useful but still somewhat special-cased.
4. Assistant history/sidebar behavior is more custom than it would need to be if
   sessions were more first-class workspace items.

The main design correction for Helix is:

- collaboration core
- assistant domain
- transport adaptation

must be three separate concerns.

## What Helix Already Has

The most important Helix files for this design are:

- `helix-view/src/editor.rs`
- `helix-view/src/view.rs`
- `helix-view/src/traits.rs`
- `helix-view/src/model/mod.rs`
- `helix-term/src/compositor.rs`
- `helix-term/src/application.rs`
- `helix-term/src/ui/acp.rs`
- `helix-core/src/transaction.rs`

### Existing strengths

#### Editor-owned state root

`helix-view/src/editor.rs` already owns:

- `tree`
- `documents`
- `component_docs`
- `component_views`
- `model`

This is the right place for collaboration and assistant stores.

#### Existing heterogeneous surface seam

`helix-view/src/view.rs` and `helix-view/src/editor.rs` already support both:

- tree-backed editor views
- component-backed edit surfaces

This is a strong starting point for a generic surface model.

#### Existing capability-style traits

`helix-view/src/traits.rs` already defines small traits such as:

- `Focusable`
- `Scrollable`
- `Viewport`
- `TextViewport`

This is exactly the style we should continue.

#### Existing registry patterns

`helix-view/src/model/mod.rs` and `helix-modal/src/registry.rs` already show that
Helix is comfortable with:

- trait objects
- downcasting
- stable ids
- registration-based extensibility

### Existing constraints

#### ACP is too panel-owned

`helix-term/src/ui/acp.rs` currently owns durable state that should not be panel-owned:

- message/thread state
- plan state
- session-ish data
- scratch doc mappings

#### ACP update routing is too direct

`helix-term/src/application.rs` currently routes ACP updates straight into the
live panel. That is expedient, but it prevents session-aware state ownership and
clean tests.

#### Shared editing is not ready

`helix-core/src/transaction.rs:314` still has `ChangeSet::map()` unimplemented.

That means real concurrent remote editing is not phase-1 work. We should still
design for it, but not pretend it is immediately available.

## Target Layering

There should be two new layers in `helix-view`.

### 1. `collab`

This is the generic collaboration substrate.

It owns:

- participants
- locations
- presence
- follow state machinery
- collaboration effects
- surface capability and registry APIs

It does not own assistant sessions.

### 2. `assistant`

This is the assistant-specific domain.

It owns:

- threads/sessions
- entries
- turns
- change provenance
- prompt drafts
- context attachments
- per-thread follow preferences

ACP in `helix-term` becomes a UI over `assistant`, which itself consumes `collab`.

Both layers are expected to use the upcoming `helix-runtime` crate for:

- background work
- grouped child task spawning
- timers
- typed mailbox delivery back into the main loop

## Proposed Module Layout

### New generic collaboration layer

- `helix-view/src/collab/mod.rs`
- `helix-view/src/collab/ids.rs`
- `helix-view/src/collab/participant.rs`
- `helix-view/src/collab/location.rs`
- `helix-view/src/collab/presence.rs`
- `helix-view/src/collab/follow.rs`
- `helix-view/src/collab/surface.rs`
- `helix-view/src/collab/registry.rs`
- `helix-view/src/collab/effect.rs`

### New assistant layer

- `helix-view/src/assistant/mod.rs`
- `helix-view/src/assistant/store.rs`
- `helix-view/src/assistant/thread.rs`
- `helix-view/src/assistant/context.rs`
- `helix-view/src/assistant/change.rs`
- `helix-view/src/assistant/effect.rs`

### Integration points

- `helix-view/src/editor.rs`
- `helix-view/src/view.rs`
- `helix-view/src/model/mod.rs`
- `helix-term/src/application.rs`
- `helix-term/src/ui/acp.rs`

## Canonical Naming

This section defines the exact naming direction we should use.

The rule is:

- use module context aggressively
- keep local type names short
- avoid repeating context already carried by the module path
- alias module paths at the use-site when needed rather than inflating the type name

### Module naming

Use these module names:

- `collab`
- `collab::participant`
- `collab::location`
- `collab::presence`
- `collab::surface`
- `collab::follow`
- `collab::effect`
- `assistant`
- `assistant::thread`
- `assistant::context`
- `assistant::config`
- `assistant::plan`
- `assistant::tool`
- `assistant::change`
- `assistant::history`
- `assistant::effect`
- `assistant::event`
- `assistant::model`

Do not create modules like:

- `assistant_session_store`
- `collaboration_surface_registry`
- `assistant_follow_state`

The module path already carries that meaning.

### Type naming inside modules

Use short names inside each module.

#### `collab::participant`

- `Id`
- `Kind`
- `Access`
- `Participant`

#### `collab::location`

- `Location`
- `Source`

#### `collab::surface`

- `Id`
- `Kind`
- `Role`
- `Ref`
- `Mut`
- `Open`
- `Capture`
- `Factory`
- `Registry`

#### `collab::follow`

- `State`
- `Mode`
- `Pause`

Use `Pause`, not `PauseReason`, because the module already provides context.

#### `collab::effect`

- `Effect`

#### `assistant`

- `Store`

#### `assistant::thread`

- `Id`
- `EntryId`
- `TurnId`
- `Origin`
- `Run`
- `Event`
- `Content`
- `Meta`
- `Thread`
- `Entry`
- `EntryKind`
- `Turn`
- `Scope`
- `Snapshot`
- `ViewState`

#### `assistant::context`

- `Id`
- `Item`
- `Kind`
- `Key`
- `Selection`
- `Symbol`
- `File`
- `Diagnostics`
- `Diff`
- `Provider`
- `Registry`

#### `assistant::config`

- `State`

#### `assistant::plan`

- `Item`
- `Status`
- `Event`

#### `assistant::tool`

- `Id`
- `Call`
- `State`

#### `assistant::change`

- `Id`
- `Summary`
- `File`

#### `assistant::history`

- `Backend`
- `Record`
- `Stub`

#### `assistant::event`

- `Event`

#### `assistant::effect`

- `Effect`

#### `assistant::model`

- `Panel`
- `Tab`
- `Follow`
- `ThreadView`
- `EntryView`
- `Pill`

This is preferable to names like `AssistantPanelModel` when the module path is
already `assistant::model`.

### Exact import style

Prefer importing modules, not flattening many repeated short type names into one scope.

Good:

```rust
use crate::assistant::{context, effect as assistant_effect, event as assistant_event, thread};
use crate::collab::{follow, location, participant, surface};

fn activate(thread_id: thread::Id, on: surface::Id) {
    // ...
}
```

Also good in narrower scopes:

```rust
use crate::assistant::thread::Id as ThreadId;
use crate::collab::surface::Id as SurfaceId;
```

Avoid flattening a dozen short names into one scope where `Id`, `State`, and
`Effect` all collide invisibly.

### Method naming

Use short verbs where the receiver already provides context.

Good:

- `with_surface`
- `with_surface_mut`
- `set_location`
- `set_presence`
- `apply`
- `active`
- `thread`
- `thread_mut`
- `activate`
- `close`
- `attach`
- `detach`
- `pause`
- `resume`

Avoid:

- `get_active_thread`
- `apply_assistant_event_to_store`
- `set_follow_pause_reason`
- `open_collaboration_surface`

### Why this naming scheme is better

- it matches Rust module-oriented naming style
- it avoids repetitive prefixes/suffixes
- it keeps call sites readable
- it scales better as the architecture grows
- it avoids long names that still fail to communicate ownership boundaries

## Data Structure Strategy

This section is explicit because the data structure choices directly affect API
quality, id stability, ordering, and future extensibility.

### Stable ids: `slotmap`

Use `slotmap` ids for internal durable identifiers that must survive vector
reordering and map mutation.

Recommended:

- `surface::Id`
- `thread::EntryId`
- `thread::TurnId`
- `change::Id`

Why:

- stable under insertion/removal
- cheap to copy
- already used in our codebase
- avoids row-index coupling

Recommended derive policy for id newtypes:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
```

If an id cannot be `Copy`, it should still derive:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
```

### Ordered stores: `IndexMap`

Use `IndexMap` for ordered, keyed collections where user-visible order matters.

Recommended:

- assistant thread tabs: `IndexMap<thread::Id, thread::Thread>`
- recent session metadata
- maybe ordered context refs if we want dedup + stable order

Why:

- deterministic order
- O(1)-ish keyed lookup
- no need to manually synchronize `Vec` + `HashMap`

Use `IndexMap` when users care about order *and* we need keyed lookup.
Use `Vec` when only order matters and stable ids already live on elements.

Do not use `BTreeMap` here unless lexical ordering is the point.

### Small collections: `SmallVec`

Use `SmallVec` for short collections that are very often tiny.

Good candidates:

- `Entry.locations`
- `Turn.entries`
- `context::Kind` lists produced by one command
- small path stacks / breadcrumbs

Why:

- many of these will be 0, 1, or 2 items
- avoids heap churn in hot paths

Do not use `SmallVec` for long-lived top-level stores.

### Mutable root stores: plain owned maps, not `ArcSwap` or `left-right`

The primary collaboration and assistant stores should be plain editor-owned
mutable structs.

Why:

- `Editor` is already the single mutable authority
- most updates must mutate multiple related structures together
- transactional consistency matters more than lock-free reads
- deterministic tests are easier with ordinary owned data

#### `ArcSwap`

`ArcSwap` is already used heavily in the codebase for read-mostly shared state,
for example syntax/config loaders.

That is appropriate for:

- shared config snapshots
- frontend-readable derived snapshots
- replacing whole immutable snapshots cheaply

It is not the right primary storage for collaboration or assistant state because:

- writes are frequent and incremental
- updates touch several coupled maps and ids at once
- we want one editor-owned mutable source of truth

Where `ArcSwap` *could* be used later:

- publishing a derived read-only collaboration snapshot for non-owner readers
- asynchronously feeding model/frontends if that ever becomes a bottleneck

If we do add a snapshot layer later, it should be explicitly named and isolated,
for example:

```rust
pub struct SnapshotStore {
    current: ArcSwap<Snapshot>,
}
```

That snapshot layer should never become the mutation authority.

#### `left-right`

We do not currently use `left-right` in the repo for editor state, and it is not
the right default here.

Why not:

- collaboration updates are not just append-only or single-map mutations
- many operations need consistent updates across sessions, entries, locations,
  presence, unread state, and UI-derived state
- left-right works best when reads dominate and writes can be batched over a
  structurally simple state graph
- our immediate problem is clean ownership and API shape, not lock-free read scaling

If later we identify a real read-contention problem for derived snapshots, we can
revisit that with a dedicated snapshot layer. It should not shape the primary API.

### `DashMap`

Do not use `DashMap` for the primary collaboration or assistant stores.

Why:

- ordering matters for tabs, sessions, and history
- editor mutations need coherent multi-structure updates
- lock-sharded maps encourage piecemeal mutation and muddier invariants

### Newtype ids and typed wrappers

Follow Rust type-safety rules aggressively here.

Good:

```rust
pub struct PeerId(u64);
// in assistant::thread
pub struct Id(Arc<str>);
// in collab::surface
pub struct Kind(Cow<'static, str>);
```

Avoid:

- passing raw `u64` or `String` where a semantic id exists
- reusing one id type for unrelated spaces

### Open tags and owned string wrappers

For open-ended registry tags, prefer an owned cheap-clone wrapper rather than a
large central enum.

Prefer:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kind(Cow<'static, str>);
```

over:

```rust
pub enum Kind {
    Editor,
    AssistantThread,
    Debugger,
    SharedNote,
    // keeps growing forever
}
```

This is one of the most important anti-enum-bloat decisions in the design.

### Native trait policy

This architecture will introduce many new public-within-workspace types. We should
be deliberate about standard trait impls.

#### Id types

Ids should derive eagerly:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
```

If an id cannot be `Copy`, derive:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
```

#### Semantic enums

Small semantic enums should typically derive:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
```

Examples:

- `participant::Kind`
- `Access`
- `location::Source`
- `follow::Mode`
- `follow::Pause`

#### Value objects

Pure value objects should derive `Debug` and `Clone` eagerly, and `PartialEq`/`Eq`
when equality has real meaning.

Examples:

- `Location`
- `context::Selection`
- `context::Symbol`
- `model::Tab`

#### Stores

Stores should generally derive `Debug`, but not `Clone` or `Default` unless there
is a truly sensible default and clone semantics are actually useful.

#### `Default`

Only derive or implement `Default` where the zero/default state is semantically
real, not merely convenient.

Good:

- empty per-thread view state
- empty collaboration store

Questionable:

- fully formed `Participant`
- fully formed `Location`
- fully formed `Thread`

#### `Display`

Implement `Display` only for types that have a real user-facing string identity.

Good:

- ids that appear in logs or statuslines
- compact context labels

Bad:

- large state structs where `Debug` is more appropriate

#### `Send + Sync`

Anything stored behind editor-wide registries or shared service handles should be
explicitly expected to be `Send + Sync` unless there is a strong reason otherwise.

Good candidates:

- adapter registries
- async resolver services
- persistence backends
- collaboration transports

Editor-owned stores themselves do not need a special marker in the type
definition; they inherit thread-safety from ownership structure rather than from
being trait objects.

#### `#[must_use]`

Apply `#[must_use]` to:

- builders
- effect-returning pure update functions if dropped results would be a bug

#### `#[non_exhaustive]`

Use sparingly.

Because these are cross-crate but still in one workspace, prefer normal enums for
ergonomic matching unless we have a concrete reason to preserve forward-compatibility
at the type boundary.

#### Field visibility

Default to private fields for stores and domain types that need invariants.

Good candidates for private fields:

- `assistant::Store`
- `collab::Store`
- `thread::Thread`
- `context::Item`

Expose mutation through small methods instead of wide `pub` fields when the type
has invariants we care about.

## Generic Collaboration Model

### Participant

```rust
// in collab::participant
pub enum Id {
    Agent(helix_acp::AgentId),
    Peer(PeerId),
}

pub enum Kind {
    Agent,
    Human,
}

pub enum Access {
    Observe,
    Read,
    Write,
}

pub struct Participant {
    pub id: Id,
    pub kind: Kind,
    pub name: String,
    pub access: Access,
}
```

Why:

- this is generic enough for agent and human collaboration
- `Access` gives us a central capability primitive instead of ad hoc read-only
  logic scattered across future features
- `participant::Kind` is a small closed enum with stable semantics
- `participant::Kind` is a small closed enum with stable semantics

### Location

```rust
// in collab::location
pub struct Location {
    pub path: PathBuf,
    pub range: Option<RangeAnchor>,
    pub source: Source,
    pub surface: Option<surface::Id>,
    pub entry: Option<thread::EntryId>,
}

pub enum Source {
    Read,
    Write,
    Tool,
    Change,
    Cursor,
}
```

Why:

- follow needs a canonical target
- tool rows need jump targets
- change review needs reveal targets
- `surface` is optional because not every location resolves to an already-open surface
- `entry` is optional because not every collaboration location comes from an assistant entry

### Presence

```rust
// in collab::presence
pub struct Presence {
    pub participant: participant::Id,
    pub surface: surface::Id,
    pub cursor: Option<RangeAnchor>,
    pub selection: Option<RangeAnchor>,
    pub viewport: Option<ViewportAnchor>,
}
```

Why:

- if we do not establish a generic presence type now, we will special-case agent
  cursor overlays later
- presence is generic collaboration state, not assistant state

### Generic collaboration store

Keep the generic store small and factual.

```rust
pub struct Store {
    participants: HashMap<participant::Id, participant::Participant>,
    locations: HashMap<participant::Id, location::Location>,
    presence: HashMap<surface::Id, Vec<presence::Presence>>,
}
```

This store should *not* own assistant thread data.

### Exact generic store API

The generic collaboration store should stay small and explicit.

```rust
impl Store {
    #[must_use]
    pub fn join(
        &mut self,
        participant: participant::Participant,
    ) -> SmallVec<[effect::Effect; 1]>;

    #[must_use]
    pub fn leave(
        &mut self,
        participant: participant::Id,
    ) -> SmallVec<[effect::Effect; 1]>;

    #[must_use]
    pub fn publish_location(
        &mut self,
        participant: participant::Id,
        location: location::Location,
    ) -> Result<SmallVec<[effect::Effect; 1]>, MissingParticipant>;

    #[must_use]
    pub fn show_presence(
        &mut self,
        surface: surface::Id,
        presence: Vec<presence::Presence>,
    ) -> SmallVec<[effect::Effect; 1]>;

    #[must_use]
    pub fn clear_location(
        &mut self,
        participant: participant::Id,
    ) -> Result<SmallVec<[effect::Effect; 1]>, MissingParticipant>;

    #[must_use]
    pub fn clear_presence(
        &mut self,
        surface: surface::Id,
    ) -> SmallVec<[effect::Effect; 1]>;

    pub fn participant(&self, id: participant::Id) -> Option<&participant::Participant>;
    pub fn location(&self, id: participant::Id) -> Option<&location::Location>;
    pub fn presence(&self, id: surface::Id) -> Option<&[presence::Presence]>;
}

pub struct MissingParticipant {
    pub id: participant::Id,
}
```

Design notes:

 - use specific verbs, not a giant `apply` catch-all for the generic store
 - avoid `Option<T>` inputs where separate methods encode meaning more clearly
 - keep the number of mutators intentionally small
- reserve an event enum for domains like assistant where event fan-in is much larger

## Surface Model

This is the most important extensibility boundary.

We need a model that supports:

- tree-backed editor views
- component-backed edit surfaces
- future shared notes/debugger/editor-like surfaces
- future plugin or external collaboration surfaces

without forcing everything into one enum or one giant trait.

### Surface identity

Do not use `ViewId` as the generic collaboration surface id.

Reason:

- `ViewId` is a local viewport-state identity today
- collaboration surfaces are a broader concept
- we want the option to map multiple local view-like things onto one semantic surface,
  or vice versa, without rewriting public APIs

Use:

```rust
// in collab::surface
slotmap::new_key_type! {
    pub struct Id;
}
```

Then map `surface::Id` to local editor/component surfaces in `Editor`.

### Visitor-like local surface access

Formalize the heterogeneous access seam that already exists in our code.

```rust
// in collab::surface
pub enum Ref<'a> {
    Tree {
        view: &'a View,
        doc: &'a Document,
    },
    Component {
        view: &'a ComponentViewState,
        doc: &'a Document,
    },
}

pub enum Mut<'a> {
    Tree {
        view: &'a mut View,
        doc: &'a mut Document,
    },
    Component {
        view: &'a mut ComponentViewState,
        doc: &'a mut Document,
    },
}
```

And in `Editor`:

```rust
impl Editor {
    pub fn with_surface<R>(
        &self,
        id: surface::Id,
        f: impl FnOnce(surface::Ref<'_>) -> R,
    ) -> Result<R, surface::Missing>;

    pub fn with_surface_mut<R>(
        &mut self,
        id: surface::Id,
        f: impl FnOnce(surface::Mut<'_>) -> R,
    ) -> Result<R, surface::Missing>;
}
```

Why this pattern:

- it is explicit
- it avoids scattering downcasts everywhere
- it works with our current tree/component split
- it is easier to evolve than forcing a universal trait over all surfaces immediately

This is the main visitor-like pattern we should lean on.

### Open requests and context queries

The registry should speak in small typed request objects rather than many loosely
related parameters.

```rust
// in collab::surface
pub struct Open {
    pub target: Target,
}

pub enum Target {
    New,
    Path(PathBuf),
    Location(location::Location),
}

pub enum Capture {
    Selection,
    Symbol,
}
```

This is preferable to a bag of optional fields because it avoids representing
invalid request shapes like "path and location are both absent" or "both are present".

The surface kind should be selected by the registry call itself, not duplicated
inside `surface::Open`.

`surface::Capture` should stay intentionally small and surface-local.

It should cover only capture kinds that are naturally derived from one local
surface, such as:

- selection
- symbol

Richer attachment kinds like diagnostics, diff, or multi-file context should not
be forced through `surface::Capture`. They belong in assistant-domain providers or
commands built on top of the assistant layer.

### Richer context providers

This resolves the first pressure point.

The intended split is:

- `surface::Capture` stays small and closed
- `assistant::context::Provider` and `assistant::context::Registry` handle richer,
  potentially async, multi-surface, or service-backed attachments

Suggested shape:

```rust
// in assistant::context
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(Cow<'static, str>);

pub enum Error {
    Unavailable,
    Resolve(anyhow::Error),
}

pub trait Provider: Send + Sync {
    fn key(&self) -> Key;

    async fn provide(
        &self,
        editor: &Editor,
        thread: &thread::Snapshot,
        surface: Option<surface::Id>,
    ) -> Result<Kind, Error>;
}

pub struct Registry {
    providers: HashMap<Key, Arc<dyn Provider>>,
}
```

Rule:

- if the attachment is naturally local to one surface, use `surface::Capture`
- if the attachment needs richer async/service-backed logic, use a provider
- do not keep expanding `surface::Capture` into a kitchen sink

### Capability traits

Do not create one giant `CollaborativeSurface` trait.

Use narrow traits:

```rust
pub trait Reveal {
    fn reveal(
        &mut self,
        editor: &mut Editor,
        location: &location::Location,
    ) -> anyhow::Result<()>;
}

pub trait PauseFollow {
    fn pause(&self, event: &EditorEvent) -> Option<follow::Pause>;
}

pub trait ShowPresence {
    fn show_presence(
        &mut self,
        editor: &mut Editor,
        presence: &[presence::Presence],
    );
}

pub trait Context {
    fn capture(&self, editor: &Editor, capture: Capture) -> Option<context::Kind>;
}
```

These should live in `helix-view/src/collab/surface.rs`, not in
`helix-view/src/traits.rs`, because they are collaboration capabilities, not
general UI traits.

### Why not one trait?

Because the moment one trait needs:

- follow event handling
- remote reconstruction
- context extraction
- presence updates
- serialization hooks

it becomes hard to read, hard to evolve, and hard to implement partially.

Zed shows this pressure in `FollowableItem`.

### Registries

We need open registration for collaboration-aware surface creation.

Use a factory registry rather than a central enum or a bag of raw function pointers.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kind(Cow<'static, str>);

impl Kind {
    pub const fn core(name: &'static str) -> Self;
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self;
}

pub mod kind {
    pub const EDITOR: Kind = Kind::core("editor");
    pub const ASSISTANT_THREAD: Kind = Kind::core("assistant.thread");
}

pub enum Role {
    Editor,
    Auxiliary,
}

pub trait Factory: Send + Sync {
    fn kind(&self) -> Kind;
    fn role(&self) -> Role;
    fn open(&self, editor: &mut Editor, open: Open) -> Result<Id, Error>;
}

pub struct Registry {
    factories: HashMap<Kind, Arc<dyn Factory>>,
}

pub enum Error {
    Open(anyhow::Error),
}

pub enum OpenError {
    UnknownKind(Kind),
    Factory(Error),
}

pub struct Missing {
    pub id: Id,
}

impl Registry {
    pub fn open(
        &self,
        editor: &mut Editor,
        kind: &Kind,
        open: Open,
    ) -> Result<Id, OpenError>;
}
```

Why this shape is better:

- surface types are open-ended
- a new debugger/session/note surface should register itself
- we should not have to touch a central enum for every future extension
- creation concerns are separate from local surface capabilities
- local reveal/context/presence behavior stays on the surface side instead of being
  duplicated in a registry trait

This split is cleaner than a single growing adapter trait.

`surface::Role` resolves the docked-panel pressure point.

Generic collaboration should know only a small semantic role such as:

- `Editor`
- `Auxiliary`

It should not know about concrete docking layout like left/right/bottom placement.
Layout remains a frontend concern.

This gives us enough information for policies like "follow should prefer editor
surfaces" without leaking panel layout into the collaboration core.

`surface::Kind` should therefore stay open-ended *and* expose a few built-in core
constants for discoverability. That hybrid is the best DX without reintroducing enum bloat.

### Extension decision matrix

Use this decision table whenever a new collaboration or assistant abstraction is proposed.

| Need | Preferred shape | Why |
|------|-----------------|-----|
| Stable semantic category with few variants | enum | Exhaustive and easy to match |
| Open-ended pluggable kind | registry + tag newtype | Avoids enum growth and central churn |
| Optional behavior on heterogeneous surfaces | small trait | Keeps impl surface narrow |
| Short-lived lifecycle invariant | typestate builder | Good compile-time guardrails |
| Long-lived dynamic runtime state | store struct | Easier to mutate, persist, and inspect |

Examples:

- `location::Source` -> enum
- `participant::Kind` -> enum
- `surface::Kind` -> registry tag
- `surface::Context` -> trait
- prompt send preconditions -> typestate builder if we want one
- assistant threads -> ordinary store structs

### Closures vs traits

We should also be explicit about when a closure-shaped API is better than a trait
or registry.

#### Use `impl FnOnce` / `impl FnMut` for scoped, local inversion of control

Closures are the right tool when:

- the callback is invoked immediately
- the callback is not stored
- the callback is there to help with borrowing or traversal
- the semantics are local and obvious from the function name

Good examples in this architecture:

```rust
impl Editor {
    pub fn with_surface<R>(
        &self,
        id: surface::Id,
        f: impl FnOnce(surface::Ref<'_>) -> R,
    ) -> Option<R>;

    pub fn with_surface_mut<R>(
        &mut self,
        id: surface::Id,
        f: impl FnOnce(surface::Mut<'_>) -> R,
    ) -> Option<R>;
}
```

Why this is good:

- the call site is concise
- it avoids leaking internal borrow structure
- it reads as a scoped visitor without needing a dedicated trait

Use `FnMut` when the callback may be invoked multiple times during iteration.
Use `FnOnce` when the callback is one-shot and should freely move captured state.

#### Do not use closures as the primary open-ended extension mechanism

Closures are the wrong primary abstraction when:

- the behavior must be registered and stored
- the behavior needs a stable semantic contract
- the behavior should be mockable and documentable as an API surface
- multiple related methods belong to one conceptual capability

That is why the open-ended surface registration mechanism should be a named trait,
not a map of closures.

Traits win there because they give us:

- a named contract
- room for docs and default methods
- cleaner test doubles
- clearer grouping of related behavior

#### Use function/closure parameters for local customization, not core ownership

Good:

- `with_surface(...)`
- `with_thread_mut(...)`
- local sorting/filtering/predicate helpers

Bad:

- making the collaboration store itself callback-driven
- storing unstructured closure bags in place of capability traits
- representing durable workflow transitions as callback chains instead of explicit effects

### Sealing rules

When a capability trait is meant to define an internal collaboration contract,
seal it.

Example shape:

```rust
mod sealed {
    pub trait Sealed {}
}

pub trait Reveal: sealed::Sealed {
    fn reveal(
        &mut self,
        editor: &mut Editor,
        location: &location::Location,
    ) -> anyhow::Result<()>;
}
```

Why:

- keeps invariants local while the API is evolving
- avoids accidental external impl sprawl
- lets us refine capability boundaries before stabilizing them as a public contract

### Open vs closed categories

Closed enums are appropriate for:

- `participant::Kind`
- `Access`
- `location::Source`
- probably `ContextKind`

Registries/newtype tags are appropriate for:

- surface kinds
- context providers
- future persistence codecs
- future collaboration backends

This is the main anti-enum-bloat rule.

## Transport And Codec Separation

One of the main things we should improve over Zed is the separation between local
collaboration APIs and wire-format concerns.

### Core rule

- `collab` traits speak in local typed state
- transport adapters convert local state to and from wire formats

Do not put methods like `to_proto`, `from_proto`, `apply_wire_delta`, or ACP JSON
concerns directly on the generic collaboration traits.

### Future codec split

If we later need networked collaboration over generic surfaces, add a separate
codec layer.

Example shape:

```rust
pub trait SurfaceCodec {
    type Snapshot;
    type Delta;

    fn snapshot(editor: &Editor, id: surface::Id) -> anyhow::Result<Self::Snapshot>;
    fn diff(previous: &Self::Snapshot, next: &Self::Snapshot) -> Option<Self::Delta>;
    fn apply(editor: &mut Editor, id: surface::Id, delta: Self::Delta) -> anyhow::Result<()>;
}
```

This keeps transport/reconstruction separate from local reveal/follow/context
capabilities.

## Assistant Domain Model

This is assistant-specific. ACP is the first UI client of it, but the domain
model should not assume ACP is the only assistant-facing surface forever.

### Thread store

```rust
// in assistant
pub enum Store {
    Empty,
    Ready(Threads),
}

pub struct Threads {
    active: thread::Id,
    threads: IndexMap<thread::Id, thread::Thread>,
}
```

Use `IndexMap` because tab order is user-visible and stable ordering matters.

This is one place where an enum is preferable to `Option<thread::Id>` because it
removes an invalid state: "store has threads, but no active thread".

The `Threads` wrapper is there for the same reason:

- it can enforce that `Ready` means non-empty threads
- it can enforce that `active` is always a member of `threads`

That is strictly better than exposing `Ready { threads, active }` directly.

This resolves the second pressure point.

`assistant::Threads` should stay assistant-specific and private.

Why not extract a generic non-empty ordered map now:

- the invariants here are assistant-domain specific
- a generic wrapper would likely need customization hooks immediately
- premature generic extraction would make the API more abstract and less clear

Rule:

- keep `Threads` private to `assistant`
- if a second real domain needs the same shape later, extract a shared helper then
- do not genericize this preemptively

### Thread identity

The assistant domain should use a local `thread::Id`, not ACP transport ids directly.

```rust
// in assistant::thread
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Id(Arc<str>);

slotmap::new_key_type! {
    pub struct EntryId;
    pub struct TurnId;
}

impl From<helix_acp::types::SessionId> for Id {
    fn from(value: helix_acp::types::SessionId) -> Self;
}
```

Why:

- keeps the assistant domain from being shaped directly by ACP transport types
- gives us room for local-only draft threads or future non-ACP assistant thread kinds
- keeps the transport boundary explicit

### Exact assistant store API

The assistant store is a larger domain and should use a typed event entry point.

```rust
// in assistant::event
pub enum Event {
    Thread {
        thread: thread::Id,
        event: thread::Event,
    },
    Activate(thread::Id),
    Close(thread::Id),
    SetDraft {
        thread: thread::Id,
        text: String,
    },
    AttachContext {
        thread: thread::Id,
        item: context::Kind,
    },
    DetachContext {
        thread: thread::Id,
        item: context::Id,
    },
    Follow(thread::Id),
    Unfollow(thread::Id),
    PauseFollow {
        thread: thread::Id,
        reason: follow::Pause,
    },
    ResumeFollow(thread::Id),
}

impl Store {
    #[must_use]
    pub fn apply(&mut self, event: event::Event) -> SmallVec<[effect::Effect; 4]>;

    pub fn is_empty(&self) -> bool;
    pub fn active(&self) -> Option<thread::Id>;
    pub fn thread(&self, id: thread::Id) -> Option<&thread::Thread>;
    pub fn thread_mut(&mut self, id: thread::Id) -> Option<&mut thread::Thread>;
    pub fn threads(&self) -> impl Iterator<Item = &thread::Thread>;
}

impl Threads {
    pub fn active(&self) -> thread::Id;
    pub fn thread(&self, id: thread::Id) -> Option<&thread::Thread>;
    pub fn thread_mut(&mut self, id: thread::Id) -> Option<&mut thread::Thread>;
    pub fn threads(&self) -> impl Iterator<Item = &thread::Thread>;

    pub fn activate(&mut self, id: thread::Id) -> Result<(), MissingThread>;
    pub fn insert(&mut self, thread: thread::Thread);
    pub fn close(&mut self, id: thread::Id) -> Close;
}

pub struct MissingThread {
    pub id: thread::Id,
}

pub enum Close {
    Empty {
        thread: thread::Thread,
    },
    Remaining {
        thread: thread::Thread,
        active: thread::Id,
    },
    Missing(MissingThread),
}
```

Design notes:

- `assistant::event::Event` is the domain-level mutation boundary
- transport-specific ids and ACP wire updates are converted immediately at the edge
- thread-local imperative helpers should stay small and private where possible

### Thread

```rust
// in assistant::thread
pub struct Thread {
    pub id: Id,
    pub origin: Origin,
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    pub turns: Vec<Turn>,
    pub plan: Vec<plan::Item>,
    pub draft: String,
    pub queue: Vec<String>,
    pub context: Vec<context::Item>,
    pub follow: follow::State,
    pub config: config::State,
    pub run: Run,
    pub unread: bool,
    pub scope: Scope,
}

pub enum Origin {
    Acp {
        session: helix_acp::types::SessionId,
        agent: helix_acp::AgentId,
    },
    Local,
}

pub enum Run {
    Idle,
    Running,
    Waiting,
    Failed { message: String },
}

pub enum Event {
    Content(Content),
    Plan(plan::Event),
    Meta(Meta),
    Run(Run),
    Follow(location::Location),
}

pub enum Content {
    Append(Entry),
    Replace {
        id: EntryId,
        entry: Entry,
    },
    Remove {
        id: EntryId,
    },
}

pub enum Meta {
    Title(Option<String>),
}
```

In the real implementation, default these fields to private and expose focused
query/mutation methods unless a field is truly just inert data.

If we need a detached immutable projection for async orchestration, define it in
the same module.

```rust
// in assistant::thread
pub struct Snapshot {
    pub id: Id,
    pub draft: String,
    pub context: Vec<context::Item>,
    pub scope: Scope,
}
```

This is the durable domain state. It should not live in the ACP panel.

### Scope and workspace correlation

Each thread should carry explicit scope metadata so that workspace restoration,
history grouping, and future multi-root behavior are not reconstructed later from
UI heuristics.

```rust
pub struct Scope {
    pub cwd: PathBuf,
    pub worktrees: SmallVec<[PathBuf; 2]>,
}
```

Why:

- Zed's sidebar/history code has to infer workspace correlation from stored path
  lists and active workspace context
- we should encode that relation as domain state from the beginning
- this also gives us a stable path for later project-aware session lists

### Plan and config state

These should be assistant-domain types, not UI-only snapshots.

```rust
// in assistant::plan
pub struct Item {
    pub content: String,
    pub status: Status,
}

pub enum Event {
    Replace(Vec<Item>),
}

pub enum Status {
    Pending,
    InProgress,
    Completed,
    Failed,
}

// in assistant::config
pub struct State {
    pub options: Vec<helix_acp::types::ConfigOption>,
    pub modes: Vec<helix_acp::types::SessionMode>,
}
```

### Tool calls

```rust
// in assistant::tool
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Id(Arc<str>);

pub struct Call {
    pub id: Id,
    pub name: String,
    pub state: State,
}

pub enum State {
    Pending,
    Running,
    Completed,
    Failed { message: Option<String> },
    Canceled,
    Unknown(Arc<str>),
}
```

Location data should live on `thread::Entry.locations`, not be duplicated inside
`tool::Call`. That avoids inconsistent "tool path vs entry locations" states.

### Entry

```rust
// in assistant::thread
pub struct Entry {
    pub id: EntryId,
    pub turn: Option<TurnId>,
    pub kind: EntryKind,
    pub locations: SmallVec<[location::Location; 2]>,
}
```

### Entry kinds

```rust
// in assistant::thread
pub enum EntryKind {
    UserPrompt { text: String },
    AssistantText { text: String },
    ToolCall(tool::Call),
    Status { text: String },
    ChangeSummary(change::Summary),
}
```

Current `ChatEntry` in `helix-term/src/ui/acp.rs` should evolve toward this.
The key difference is that this is a domain model, not a render model.

### Turn

```rust
// in assistant::thread
pub struct Turn {
    pub id: TurnId,
    pub prompt: EntryId,
    pub entries: SmallVec<[EntryId; 4]>,
    pub changes: SmallVec<[change::Id; 2]>,
}
```

Turns are required to avoid ad hoc provenance logic later.

### Change summary

```rust
// in assistant::change
slotmap::new_key_type! {
    pub struct Id;
}

pub struct Summary {
    pub files: Vec<File>,
}

pub struct File {
    pub path: PathBuf,
    pub hunks: Vec<Hunk>,
}

pub struct Hunk {
    pub range: RangeAnchor,
    pub summary: String,
}
```

We do not need full accept/reject yet, but we do need stable provenance now.

## Follow Model

The generic collaboration store publishes participant locations. Assistant threads
consume those locations through a per-thread follow preference.

### Follow state

```rust
// in collab::follow
pub enum Mode {
    AutoSwitchAndReveal,
}

pub enum Pause {
    LocalMove,
    LocalScroll,
    LocalEdit,
    BufferSwitch,
    Explicit,
}

pub enum State {
    Off,
    On {
        mode: Mode,
        participant: participant::Id,
        last: Option<location::Location>,
    },
    Paused {
        mode: Mode,
        participant: participant::Id,
        last: Option<location::Location>,
        reason: Pause,
    },
}
```

Why this belongs in the thread, not generic collab:

- location is a collaboration fact
- follow is a consumer preference
- different consumers could follow the same participant differently later
- the enum shape avoids impossible states like "disabled but paused"

### Required follow behavior

The requested behavior is explicit:

- follow must auto-open buffers
- follow must auto-switch buffers
- follow should reveal the changed/current range when known

Follow pause should occur on:

- local cursor move
- local scroll
- local edit
- explicit user buffer switch

The explicit `follow::Pause` is worth keeping because it improves logs, future UI,
and policy decisions around auto-resume.

This should be the default assistant behavior.

### First location producers

Phase-1 location producers are:

- `helix-term/src/application.rs::handle_acp_read_file`
- `helix-term/src/application.rs::handle_acp_write_file`
- reliable ACP tool/session update data when available

This gives us:

- file-level follow immediately
- range-level follow for writes next
- richer tool-call location tracking later

## Context Attachments

Do not start with inline serialized mention links.

Start with typed context pills rendered above the composer.

### Context kinds

```rust
// in assistant::context
slotmap::new_key_type! {
    pub struct Id;
}

pub struct Item {
    pub id: Id,
    pub kind: Kind,
}

pub enum Kind {
    Selection(Selection),
    Symbol(Symbol),
    File(File),
    Diagnostics(Diagnostics),
    Diff(Diff),
}
```

### Selection

```rust
pub struct Selection {
    pub path: PathBuf,
    pub range: RangeAnchor,
    pub text: String,
    pub label: Option<String>,
}
```

### Symbol

```rust
pub struct Symbol {
    pub path: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub range: RangeAnchor,
    pub text: String,
    pub breadcrumb: Vec<String>,
}
```

### Why pills first

- our current composer is not a rich mention editor
- pills keep the domain model independent of text-format rendering details
- we can add inline serialized mention syntax later if we want clipboard-stable
  text representations

### Mapping text objects

Text objects should resolve into `context::Kind`, not directly into ACP requests.

Examples:

- function -> `context::Symbol` when symbol data exists, else `context::Selection`
- block -> `context::Selection`
- variable -> `context::Symbol` when semantic info exists, else `context::Selection`
- paragraph/comment/doc block -> `context::Selection`

This is the correct extensibility boundary.

### Prompt builder

The request-building side is a good place to use a small typestate builder if we
want compile-time guardrails around sendability.

Example:

```rust
pub struct PromptBuilder<S> {
    text: String,
    context: Vec<context::Kind>,
    _state: PhantomData<S>,
}

pub struct Empty;
pub struct Ready;

impl PromptBuilder<Empty> {
    pub fn new() -> Self;
    pub fn text(self, text: impl Into<String>) -> PromptBuilder<Ready>;
    pub fn context(self, item: context::Kind) -> PromptBuilder<Ready>;
}

impl PromptBuilder<Ready> {
    pub fn push_context(mut self, item: context::Kind) -> Self;
    pub fn build(self) -> Vec<helix_acp::ContentBlock>;
}
```

We should only do this if the invariant is truly useful. The important part is
that the request builder is the right place for typestate, not the long-lived
stores.

## Effects And Event Flow

We should move away from direct panel mutation toward explicit effects.

### Generic collab effects

```rust
// in collab::effect
pub enum Effect {
    Open {
        participant: participant::Id,
        location: location::Location,
    },
    Reveal {
        participant: participant::Id,
        location: location::Location,
    },
    ShowPresence {
        surface: surface::Id,
        presence: Vec<presence::Presence>,
    },
    ClearPresence {
        participant: participant::Id,
    },
}
```

### Assistant effects

```rust
// in assistant::effect
pub enum Effect {
    PublishLocation {
        participant: participant::Id,
        location: location::Location,
    },
    MarkUnread { thread: thread::Id },
    Save { thread: thread::Id },
    Delete { thread: thread::Id },
    SyncModel,
}
```

### Why effects matter

- makes state updates pure and testable
- lets `application.rs` coordinate side effects centrally
- avoids panel ownership confusion
- provides a natural extension point for future backends and surfaces

## UI Ownership And View State

`helix-term/src/ui/acp.rs` should become a render/controller layer over the
assistant store.

### What should stay in the panel

- panel focus
- current tab focus
- per-thread message cursor
- per-thread expanded rows
- per-thread scratch docs
- local animation state

### What should move out of the panel

- thread entries
- turns
- draft text
- follow state
- queue state
- config state
- plan state
- unread/running state
- context attachments

### Per-thread view state

```rust
// in assistant::thread
pub struct ViewState {
    pub cursor: MessageCursor,
    pub layout: MessageListState,
    pub focus: FocusTarget,
    pub expanded: HashSet<thread::EntryId>,
    pub opened_docs: HashMap<thread::EntryId, DocumentId>,
}
```

This replaces panel-global state like:

- one global `message_cursor`
- one global `expanded_message`
- one global `opened_message_docs`

### Derived panel model

The existing `AcpModel` in `helix-view/src/model/models.rs` is currently a direct
singleton panel snapshot. That was fine for the current ACP panel, but it is too
narrow for the target architecture.

It should evolve into a derived model that includes:

- session tabs
- active thread state
- per-thread unread/running/follow badges
- current composer draft and context pills

Suggested shape:

```rust
// in assistant::model
pub struct Panel {
    pub tabs: Vec<Tab>,
    pub active: Option<ThreadView>,
    pub focused: bool,
}

pub struct Tab {
    pub id: thread::Id,
    pub title: String,
    pub run: thread::Run,
    pub unread: bool,
    pub follow: Follow,
}

pub enum Follow {
    Off,
    On,
    Paused,
}

pub struct ThreadView {
    pub id: thread::Id,
    pub entries: Vec<EntryView>,
    pub draft: String,
    pub context: Vec<Pill>,
}
```

Whether this replaces `AcpModel` or sits beside it is an implementation detail.
The important rule is that the panel model remains derived and disposable.

## Session Tabs

Unlike Zed, we should make session tabs explicit early.

Why:

- session multiplicity should be discoverable
- background-running sessions should be visible
- session-local follow state should be visible
- we should not hide core thread switching behind history menus first

### Tab metadata

Each tab should expose:

- title
- running spinner
- unread indicator
- follow badge

### Tab behavior

- switching tabs never cancels a running session
- inactive sessions continue receiving updates
- inactive updates set `unread = true`
- active tab clear unread
- per-tab draft/scroll/selection state persists across switches

### Background behavior

Thread tabs should behave like this:

- switching away from a running thread does not cancel it
- inactive running tabs keep receiving updates
- updates to inactive tabs mark them unread
- inactive tabs may still publish locations into `collab`, but only the followed
  active thread should drive auto-open/auto-switch behavior in the editor

This prevents a background session from stealing the editor simply because it is
still producing activity.

## Typestate And Generics

### Typestate: where it helps

Use typestate in short-lived builders and lifecycle-restricted APIs, not in the
long-lived stores.

Good candidates:

- prompt request builder
- attachment collection pipeline
- session bootstrap/configuration sequence

Example:

```rust
pub struct Draft<S> {
    text: String,
    context: Vec<context::Kind>,
    _state: PhantomData<S>,
}

pub struct Empty;
pub struct Ready;
```

This can be nice if we want to enforce a send invariant.

### Typestate: where it hurts

Do not use typestate for:

- `Editor` collaboration stores
- assistant thread stores
- per-thread runtime state

Those are dynamic and persisted. Typestate usually adds more friction than value there.

### Generics: where they help

Use generics for:

- small builders
- registry descriptors
- capability adapters
- typed event/effect helpers

Use trait objects for:

- open-ended surface registries
- heterogeneous UI/model/plugin surfaces

### Make impossible states hard to represent

The collaboration and assistant APIs should use types to eliminate avoidable ambiguity.

Good examples in this spec:

- `follow::State` as an enum instead of `enabled: bool` plus `paused: Option<_>`
- `surface::Open { target: Target }` instead of a bag of optional fields
- `context::Item { id, kind }` plus `context::Kind` instead of detach-by-index
- `thread::Origin` instead of mandatory transport session fields on every thread
- `thread::Run` instead of a single `running: bool`
- `tool::State` instead of freeform status strings

When an API currently needs `Option<T>` or `bool` to express two or three very
different semantic states, we should prefer a dedicated enum if that removes
invalid combinations.

Do not force this principle so far that every dynamic runtime condition becomes a
typestate maze. Use it where it removes real ambiguity.

## Async Boundaries

We should be explicit here because async creep is one of the easiest ways for the
APIs to become muddy.

### Core rule

The core `collab` and `assistant` stores should be synchronous, effect-driven, and
owned by `Editor`.

That means:

- store `apply(...)` methods are synchronous
- capability traits on local surfaces are synchronous
- async work happens through `helix-runtime` at the boundary and re-enters the stores as events/results

This is *not* because async is undesirable. It is because the stores are the
single-writer state machines for editor collaboration state. Synchronous mutation
is the cleanest ownership model there.

### Why

- easier to test deterministically
- easier to reason about ownership and ordering
- keeps the single-writer mutation path obvious
- keeps local surface capabilities focused on local state changes, not transport

### Good async boundaries

- ACP transport
- background file/symbol/context resolution
- future network collaboration transport
- future persistence I/O if it becomes decoupled from immediate state mutation
- history/archive/session backends if they become independently loaded

### Recommended shape

- boundary layer starts async work through `Runtime`, `Work`, `Ui`, `group::Scope`, and typed mailboxes
- boundary layer emits a typed event when the work completes
- store applies that event synchronously and returns effects

Concretely, the collaboration and assistant layers should *use* the runtime spec's
primitives. They should not define parallel task/mailbox abstractions of their own.

Example:

```rust
pub enum event::Event {
    Thread { ... },
    ContextResolved { thread: thread::Id, item: context::Kind },
    ContextResolveFailed { thread: thread::Id, error: String },
}
```

### Native async trait methods are allowed

We are on stable Rust with native `async fn` in traits. We should use that where
it genuinely improves the API.

Good places for native async trait methods:

- transport/service boundaries
- persistence backends
- context resolvers that naturally perform async work
- future collaboration remotes/backends

### `AsyncFnOnce` / async closures

Stable async closures and the `AsyncFn*` family are useful, but only in the right
places.

Use them for:

- one-shot async helpers
- boundary-layer orchestration helpers
- spawn helpers that capture a prepared snapshot and then perform async work

Good shape:

```rust
pub fn spawn_thread_task(
    work: &Work,
    scope: &group::Scope,
    tx: mailbox::Sender<event::Event>,
    snapshot: thread::Snapshot,
    f: impl AsyncFnOnce(thread::Snapshot) -> anyhow::Result<event::Event> + Send + 'static,
) -> Result<task::Task<()>, group::SpawnError>;
```

In practice this helper should be built on the runtime spec's primitives:

- `work::Work`
- `group::Scope`
- `task::Task`
- typed `mailbox::Sender<event::Event>`

Do not use `AsyncFnOnce` to hold `&mut Editor`, `&mut Store`, or `&mut Thread`
across `await` points. That would blur the single-writer mutation boundary and
turn borrow management into the architecture.

So the rule is:

- sync borrow and snapshot first
- async work after the snapshot
- typed event/effect reintegration afterward

This is the cleanest way to use async closures without compromising the ownership model.

Example:

```rust
// in assistant::context
pub trait Provider: Send + Sync {
    async fn resolve(
        &self,
        editor: &Editor,
        surface: surface::Id,
        query: surface::Capture,
    ) -> anyhow::Result<Option<context::Kind>>;
}
```

### Async trait object caveat

Native async trait methods are great, but they are not a reason to turn every
registry trait into an async dyn interface.

We should split based on usage:

- use native async trait methods for service interfaces that are naturally async
- keep local surface capability traits synchronous and object-safe
- if a dyn-dispatched registry genuinely needs async, either:
  - box the future explicitly in that adapter boundary, or
  - move the async step into a resolver service behind the registry

### Best tool for the job

If a service is semantically async, use async. Do not avoid it just because it is
more complex.

If a service is fundamentally local and synchronous, keep it synchronous. Do not
introduce futures just to make the design look uniform.

The preferred split is:

- synchronous stores and local surface capabilities
- async service boundaries with native async trait methods when they clarify the API
- explicit event/effect reintegration at the editor-owned mutation path

The collaboration and assistant layers should not invent their own task, timer,
or mailbox abstractions on top of that.

### No direct Tokio in collaboration/assistant code

Once `helix-runtime` lands, ordinary code in `collab` and `assistant` should not
call:

- `tokio::spawn`
- `tokio::task::spawn_blocking`
- ad hoc `tokio::time::sleep`

directly.

Those crates should depend on runtime abstractions, not ambient Tokio calls.

### What should stay synchronous

- assistant store apply/update methods
- collaboration store updates
- local surface reveal/follow/presence application
- model synchronization

### What can and should be async

- ACP/network transport
- symbol resolution if it depends on async index/LSP work
- file/diagnostic/diff context loading when expensive
- session history/archive loading
- future remote collaboration backends

### Recommended async service traits

These are the kinds of boundaries where native async trait methods are a good fit.

```rust
// in assistant::context
pub trait Provider: Send + Sync {
    async fn resolve(
        &self,
        editor: &Editor,
        surface: surface::Id,
        query: surface::Capture,
    ) -> anyhow::Result<Option<context::Kind>>;
}

// in assistant::history
pub struct Record {
    pub id: thread::Id,
    pub origin: thread::Origin,
    pub title: Option<String>,
    pub entries: Vec<thread::Entry>,
    pub turns: Vec<thread::Turn>,
    pub plan: Vec<plan::Item>,
    pub draft: String,
    pub context: Vec<context::Item>,
    pub follow: follow::State,
    pub config: config::State,
    pub scope: thread::Scope,
}

pub struct Stub {
    pub id: thread::Id,
    pub title: Option<String>,
    pub scope: thread::Scope,
    pub unread: bool,
    pub run: thread::Run,
}

pub trait Backend: Send + Sync {
    async fn load_scope(&self, scope: &thread::Scope) -> anyhow::Result<Vec<Stub>>;
    async fn load(&self, id: thread::Id) -> anyhow::Result<Option<Record>>;
    async fn save(&self, record: Record) -> anyhow::Result<()>;
    async fn delete(&self, id: thread::Id) -> anyhow::Result<()>;
}

// in collab::remote
pub trait Backend: Send + Sync {
    async fn publish_location(
        &self,
        participant: participant::Id,
        location: &location::Location,
    ) -> anyhow::Result<()>;

    async fn fetch_presence(
        &self,
        participant: participant::Id,
    ) -> anyhow::Result<Vec<presence::Presence>>;
}
```

These belong at the edge. They should not own mutation authority.

### Ordering

If multiple async results may target the same thread or participant, apply them in
explicit arrival order through the same editor-owned mutation path.

That keeps the state machine single-writer even if the work that produced the
event was asynchronous.

## Integration Points

### `helix-view/src/editor.rs`

Add editor-owned fields:

- `collab: collab::Store`
- `assistant: assistant::Store`
- `surface_registry: collab::surface::Registry`

Add:

- `with_surface(...)`
- `with_surface_mut(...)`

Likely additional helpers:

- `publish_location(...)`
- `apply_presence(...)`
- `active_thread(...)`
- `active_thread_mut(...)`

This is the main insertion point because `Editor` already owns the right state.

### `helix-view/src/view.rs`

Formalize the tree/component surface bridge.

### `helix-view/src/model/mod.rs`

Keep it derived.

The model should reflect collaboration and assistant state, but it should not own it.

Use it for:

- tab strip model
- unread/running/follow badges
- context pill model if needed

### `helix-term/src/application.rs`

Stop mutating the ACP panel directly.

Target flow:

1. receive ACP wire event
2. translate it into domain types such as `thread::Id` and `thread::Event`
3. build an `assistant::event::Event`
4. apply into `editor.assistant`
5. drain assistant/collab effects
6. sync derived model state

This file should become the event-translation boundary, not the assistant state owner.

### `helix-term/src/ui/acp.rs`

Reduce this file to:

- panel rendering
- panel-local focus/scroll/animation state
- dispatching commands into the assistant store

It should no longer be the source of truth for assistant sessions.

## Detailed File Responsibilities

This section makes the target ownership boundary explicit.

### `helix-view/src/editor.rs`

Responsibilities after the refactor:

- own collaboration and assistant stores
- own the surface registry
- provide heterogeneous surface accessors
- apply collaboration and assistant effects
- remain the single mutable authority over documents and views

It should not:

- encode ACP panel policy
- format assistant rows
- own assistant tab rendering concerns

### `helix-view/src/view.rs`

Responsibilities after the refactor:

- expose the tree-backed side of the surface bridge
- remain the owner of tree-view-specific viewport mechanics
- support the visitor-style access path to surfaces

It should not:

- know about ACP sessions
- know about collaboration transports

### `helix-view/src/model/mod.rs`

Responsibilities after the refactor:

- hold derived frontend models only
- provide stable ids for layers/panels
- downcast panel/layer models for rendering

It should not:

- become a second durable store for collaboration or assistant state

### `helix-term/src/application.rs`

Responsibilities after the refactor:

- translate ACP wire events into assistant-domain events
- apply assistant updates to the store
- apply collaboration effects to the editor
- own the runtime mailbox receivers that deliver assistant/collaboration async results
- trigger model synchronization

It should not:

- directly mutate the ACP panel for durable state updates

### `helix-term/src/ui/acp.rs`

Responsibilities after the refactor:

- render tabs, entries, composer, and context pills
- maintain only per-thread UI view state
- dispatch commands into the assistant store
- open/reuse scratch docs keyed by `thread::EntryId`

It should not:

- own thread data
- own follow state
- own plan/config/unread/running as durable data

### `helix-term/src/commands.rs`

Responsibilities after the refactor:

- expose commands over the generic collaboration layer and assistant layer
- target active session/thread state in the assistant store
- avoid reaching into panel-owned singleton state

This file should gain commands that reflect the architecture, not just today's ACP UI.

## Persistence And Restoration

We should define persistence semantics now so the data model does not drift later.

### Persist thread metadata

Persist per thread:

- `thread::Id`
- remote ACP `SessionId`
- title
- scope
- draft
- context refs
- follow state
- unread and `thread::Run`

### Persist per-thread view state separately

Persist view state separately from durable thread content:

- scroll position
- selected entry id
- expanded entry ids

Scratch docs are *not* persistence. They are ephemeral projections.

### Backend shape

This resolves the persistence pressure point.

Persistence should be behind `assistant::history::Backend` from the start, but the
assistant store should remain pure and backend-agnostic.

That means:

- `assistant::Store` does not own a backend trait object
- persistence is driven by effects and runtime work at the application boundary
- the persisted shape is `history::Record`, not `thread::Thread` directly

This is cleaner because it keeps:

- domain mutation in the store
- I/O in the runtime/application layer
- persistence shape explicit instead of implicit

### Save timing

The default save policy should be:

- debounce ordinary mutations through `helix-runtime::Debounce`
- flush immediately on explicit thread close
- flush immediately on application shutdown

Suggested effect direction:

```rust
// in assistant::effect
pub enum Effect {
    PublishLocation { ... },
    MarkUnread { thread: thread::Id },
    Save { thread: thread::Id },
    Delete { thread: thread::Id },
    SyncModel,
}
```

The application layer then owns the persistence worker and coalesces `Save` effects.

### Restore rules

- restore open tabs only if scope still makes sense
- restore active tab
- restore follow preference, but not active follow if the participant/session is absent
- restore draft and context refs before rendering the panel
- load stubs first, then full records lazily on activation/open when possible

## API Footguns To Avoid

This section is the explicit checklist for collaboration/assistant API hazards.

### 1. Index-keyed durable state

Do not key durable state by row index, tab index, or panel ordering.

Rule:

- use `surface::Id`, `thread::Id`, `thread::EntryId`, `thread::TurnId`, and `change::Id`

### 2. Store state with invalid active selection

The assistant store should not be able to represent "there are threads, but no
active thread".

Rule:

- use `assistant::Store::{Empty, Ready { .. }}` instead of `Option<thread::Id>` beside a thread map

### 3. Invalid optional request shapes

APIs should not rely on bags of optional fields where a sum type removes ambiguity.

Rule:

- use enums like `surface::Target` instead of `Option<PathBuf>` plus `Option<Location>`

### 4. Mixed transport and domain identity

Do not use ACP transport ids as the primary assistant domain id.

Rule:

- local identity is `thread::Id`
- transport identity lives in `thread::Origin`

### 5. Bool-plus-option state shapes

If a state shape naturally has mutually exclusive modes, use an enum.

Rule:

- `follow::State`, not `enabled: bool` + `paused: Option<_>`
- `thread::Run`, not `running: bool`
- `tool::State`, not status strings

### 6. Detach-by-index APIs

Context attachments should not be removed by list index.

Rule:

- store context as `context::Item { id, kind }`
- detach by `context::Id`

### 7. Registry overreach

If `surface::Registry` starts absorbing reveal/context/presence policy, it has become too broad.

Rule:

- registry opens/locates surfaces
- capabilities stay on the surface side or in dedicated service traits

### 8. Assistant leakage into generic collaboration

If a generic collaboration type starts carrying assistant-thread concepts, that is
an architectural bug.

Rule:

- keep `collab` generic
- keep assistant session/thread concepts in `assistant`

## Review Against Goals

This section is the explicit self-check for the architecture.

### Extensibility

The architecture holds up well on extensibility because:

- generic collaboration state is separate from assistant domain state
- open-ended surface kinds use registry tags instead of a growing central enum
- optional capabilities are modeled as small traits instead of one giant trait
- transport concerns are separated from local core traits
- stable ids avoid index-coupled view state and scratch associations

The main extensibility pressure points to watch are:

- whether `surface::Factory` stays narrowly focused on creation
- whether `assistant::context::Registry` is enough for richer attachment kinds
- whether assistant history/archive stays satisfied with one backend trait or later
  wants finer-grained read/write/archive capabilities

If the registry starts attracting non-creation behavior, that is a sign the
capability belongs on the surface side or in a dedicated service trait instead.

### API cleanliness

The architecture holds up well on API cleanliness because:

- `Editor` remains the ownership root
- stores are explicit structs, not hidden behind callbacks or panel-local state
- effects are explicit
- durable state vs view state is a hard boundary
- domain-specific types stay out of the generic collaboration core

The main cleanliness risk is accidental leakage of assistant-specific concepts back
into `collab`. That should be treated as an architectural bug.

### Enum bloat risk

The current design addresses enum bloat by:

- keeping semantic enums small and closed
- using registry tags for open-ended surface kinds
- using trait layering for optional capabilities

The main enums that are expected to remain small are:

- `participant::Kind`
- `Access`
- `location::Source`
- `follow::Mode`
- `follow::Pause`

If a proposed enum starts listing many feature-specific surface or provider kinds,
that is a sign it should become a registry instead.

### Rust API guideline fit

This architecture follows the Rust skill reasonably well:

- newtype ids instead of raw primitive ids
- small traits over giant traits
- bounds and async kept where they are used
- typestate only for short-lived lifecycle/build APIs
- no `get_*`-style accessor naming
- explicit ownership boundaries
- enum state models where they remove invalid combinations

It is intentionally conservative about `Default`, `Clone`, and `Display` so we do
not derive them just because it is convenient.

### Async fit

The architecture now assumes modern stable Rust with native async trait methods
available.

It uses that in a disciplined way:

- synchronous editor-owned stores for mutation authority
- async service boundaries where the job is naturally async
- explicit reintegration of async results through events/effects

It now also aligns with the runtime spec by:

- using `helix-runtime` primitives instead of ad hoc task/mailbox abstractions
- treating grouped async work as `group::Scope`-based runtime work
- keeping store mutation synchronous even when the surrounding workflow is async

This is not an anti-async design. It is a single-writer-state-machine design with
async boundaries placed where they actually belong.

### Collaboration-wide fitness

The design is not ACP-narrow anymore.

It should scale to:

- agent follow
- agent tabs/history/context
- future human presence/follow
- shared notes or debugger-follow surfaces
- future remote replicated editing once the core text-mapping layer exists

That is the key reason the spec is worth following even before human collaboration lands.

## Open Questions And Pressure Points

This pass resolves most of the earlier pressure points. The main one still worth
watching during implementation is:

1. Does `assistant::history::Backend` stay cohesive enough, or later need to split into
   separate read/write/archive capabilities?

## Command Surface

These are architecture-level commands, not final keybindings.

### Collaboration commands

- `collab.follow.participant`
- `collab.unfollow`
- `collab.reveal.location`
- `collab.show.presence`

### Assistant session commands

- `assistant.new_thread`
- `assistant.close_thread`
- `assistant.next_thread`
- `assistant.prev_thread`
- `assistant.open_history`
- `assistant.toggle_follow`

### Assistant context commands

- `assistant.attach.selection`
- `assistant.attach.symbol`
- `assistant.attach.file`
- `assistant.detach.context`

### Assistant review commands

- `assistant.open_entry_scratch`
- `assistant.reveal_entry_location`
- `assistant.open_turn_changes`
- `assistant.open_thread_changes`

The exact names can change, but the command surface should already reflect the
split between generic collaboration primitives and assistant-specific workflow.

## Transport Adaptation

The current ACP protocol has session ids already.

We should route by `notif.session_id` and stop treating the panel as a singleton session.

### Current issue

`helix-acp/src/client.rs` stores a current session id on the agent.

That is useful as a transport convenience, but it should not be the UI source of truth.

### Target

The assistant store is the source of truth for open/active threads.

Transport helpers can still cache the last session id, but they should not define
the local architecture.

## Human Collaboration Extension

This architecture is deliberately shaped so that human collaboration can later use:

- `participant::Id::Peer(...)`
- the same generic location model
- the same generic presence model
- the same surface registry and reveal/open patterns

What phase 1 does *not* solve yet is shared editing.

That later work should sit below the collaboration substrate as a replicated text
layer once `ChangeSet::map()` or an alternative mapping/rebasing primitive exists.

## Phased Execution

This phase plan preserves the final architecture from the start. It is not a
"we will simplify now and redo later" plan.

### Phase 1: collaboration substrate

- add `collab` module in `helix-view`
- add ids, participants, location, presence, effects, surface registry
- add `surface::Ref` / `surface::Mut` editor APIs

### Phase 2: assistant store extraction

- add `assistant` module in `helix-view`
- move thread/session/entry/turn/context state there
- route ACP updates by `session_id`

### Phase 3: ACP tabs and per-thread view state

- add explicit session tabs
- move expanded/scratch/cursor state to `thread::EntryId` + per-thread view state

### Phase 4: follow

- publish locations from ACP activity
- auto-open/auto-switch/reveal
- pause on local navigation/edit

### Phase 5: typed context attachments

- selection and symbol first
- pills in composer
- request builder over typed refs

### Phase 6: change provenance and review substrate

- full turn/change relationships
- prep diff/review surfaces

### Phase 7: broader collaboration

- human participant presence
- shared follow over same substrate
- later replicated editing

## Tests

### Collaboration core

- participant location updates store latest location
- surface registry can open and reveal registered surfaces
- local navigation pauses follow

### Assistant domain

- ACP updates route by `session_id`
- inactive thread updates mark unread
- running sessions remain active in background
- `thread::EntryId` scratch reuse is stable
- follow emits open/reveal effects on new locations
- context refs serialize with path/range/text metadata

### ACP UI

- tab switching preserves per-thread draft/scroll/selection
- badges render correctly
- scratch docs remain associated with the right entry/thread

### Width/render regressions

Keep adding narrow, deterministic rendering tests where layout bugs appear.

## Rejected Alternatives

### 1. Keep building from `AcpPanel` outward

Rejected because it keeps durable state panel-owned and session-blind.

### 2. Make one giant collaboration trait

Rejected because it causes capability bloat and vague APIs.

### 3. Couple follow traits to wire protocol serialization

Rejected because it repeats one of Zed's main debts.

### 4. Use `ArcSwap` or `left-right` as the primary collaboration store

Rejected because the editor already has a single mutable authority and the main need
is clean ownership and coherent updates, not lock-free reads.

### 5. Start with inline mention-link syntax instead of typed pills

Rejected because it front-loads composer complexity before the domain model is stable.

## Definition Of Done For The Architecture

The architecture is in the right shape when:

- collaboration state is editor-owned, not panel-owned
- assistant state is session-aware and routed by `session_id`
- follow consumes generic participant locations
- surfaces expose follow/context/presence capabilities through narrow traits
- new collaborative surfaces can register without touching a giant central enum
- assistant tabs, follow, and context attachments are clean clients of the same substrate

At that point ACP is no longer a special one-off panel. It is the first serious use
case of a real collaboration architecture.
