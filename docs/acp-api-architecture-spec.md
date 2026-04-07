# ACP API Architecture Spec

Status: draft

This document defines the ACP-specific architecture that sits on top of the
runtime and collaboration/assistant specs.

It answers a different question from the other two specs:

- not "how should generic collaboration work?"
- not "how should the runtime work?"
- but "how should ACP integrate cleanly so the UI can later move behind a plugin or alternate frontend?"

The goal is that ACP becomes:

- a transport/backend integration
- a clean domain-event producer
- a host-capability consumer

and *not*:

- the owner of UI state
- a second session model parallel to the assistant store
- a protocol-shaped API that leaks directly into frontends

This spec describes the target ACP architecture. Temporary UI coupling or adapter
bridges are not part of the intended end state.

## Relationship To Other Specs

This spec assumes and extends:

- `docs/runtime-executor-architecture-spec.md`
- `docs/collaboration-assistant-architecture-spec.md`
- `docs/runtime-collaboration-implementation-plan.md`

Layering summary:

1. `helix-runtime`
   - tasks, groups, cancellation, mailboxes, timers
2. `assistant` / `collab`
   - durable domain state, actions, events, models
3. `ACP`
   - one backend/driver that translates the ACP protocol into assistant-domain events
4. frontend/plugin UI
   - renders assistant models and dispatches assistant actions

ACP is therefore a backend integration, not the definition of the assistant domain.

## Goals

1. Keep `helix-acp` wire-only and UI-agnostic.
2. Avoid a dual source-of-truth thread/session model.
3. Make ACP one assistant backend implementation, not the shape of the whole assistant API.
4. Keep UI/plugin consumers fully decoupled from ACP wire types.
5. Provide typed host-service boundaries for file, terminal, permission, and future ACP host requests.
6. Make it easy to build a TUI panel now and a plugin UI later over the same assistant API.
7. Avoid Zed-style UI entities and scroll state leaking into ACP thread/session objects.

## Non-Goals

- Redesigning the ACP wire protocol itself.
- Solving all future non-ACP assistant backends now.
- Building the final plugin UI today.

## Problems In Current Helix ACP

The current Helix ACP stack has a few clear architectural problems.

### 1. Durable assistant state is split between `Editor` and `AcpPanel`

Current ownership is spread across:

- `helix-view/src/editor.rs`
- `helix-term/src/ui/acp.rs`
- `helix-view/src/model/models.rs`

The panel currently owns things that should not be panel-owned:

- draft input state
- plan state
- config/mode state
- tool call presentation state
- scratch-doc mappings
- selection/expanded state keyed to current rows

### 2. Application routes ACP updates directly into the panel

`helix-term/src/application.rs` currently converts ACP updates into concrete
`AcpPanel` mutations.

That makes the TUI panel the implicit domain owner.

### 3. ACP wire types leak too far upward

Current code still has UI/application logic shaped directly around:

- `helix_acp::types::SessionUpdate`
- `ToolCallStatus`
- prompt/config/mode wire structs

That prevents the assistant domain from being backend-neutral.

### 4. UI state is panel-specific

If the UI later moves into a plugin or another frontend, the current ACP panel API
does not give that frontend a clean command/model boundary.

### 5. Single-agent assumptions still leak through

Some paths still effectively behave like:

- "first connected agent wins"

which is incompatible with a clean backend/session architecture.

## Lessons From Zed

Zed gets several ACP design points right:

- typed ACP session/thread model
- a real thread store and history model
- typed context mentions/references
- protocol adaptation separated from some UI pieces

But it also shows real debt we should avoid:

- UI-facing `AcpThread` becomes a second thread source of truth beside native thread state
- UI entities, scroll position, and render concerns leak into ACP/session state
- protocol deficiencies get patched with `meta` hacks that bleed upward
- UI-oriented concepts like dropdown-vs-flat choice leak into capability/domain APIs

The key lesson is:

- do not create one ACP-specific thread model for UI and another for internal logic

Helix should keep one canonical assistant thread model and adapt ACP into it.

## Layered ACP Architecture

There should be four ACP-related layers.

### 1. `helix-acp`

Responsibility:

- wire types
- JSON-RPC framing
- process transport/client
- protocol serialization/deserialization

It should not know about:

- editor state
- assistant store
- panel/frontend models
- markdown rendering
- scroll state
- UI entities

### 2. `assistant::backend`

Responsibility:

- generic backend command/update contract for assistant backends
- runtime-facing backend driver handles

This is the abstraction ACP should implement first.

### 3. `assistant::acp`

Responsibility:

- ACP-specific backend implementation
- translate ACP wire protocol into assistant-domain events
- translate assistant backend commands into ACP protocol requests
- normalize ACP protocol quirks into stable domain shapes

This layer is ACP-specific, but still not UI-specific.

### 4. frontend/plugin UI

Responsibility:

- render assistant models
- dispatch assistant actions
- own frontend-local scroll/selection/expanded state

It should not know about ACP wire types.

## Core Design Rule

The assistant domain owns the truth.

ACP does not own thread/session state.

ACP produces domain events and consumes domain/backend commands.

That means there should never be a second ACP-specific thread store equivalent to
Zed's `AcpThread` living beside the canonical assistant store.

## Canonical Module Layout

Recommended additions under `helix-view/src/assistant/`:

- `action.rs`
- `backend.rs`
- `host.rs`
- `permission.rs`
- `prompt.rs`
- `acp/mod.rs`
- `acp/id.rs`
- `acp/driver.rs`
- `acp/translate.rs`

Keep names short and module-oriented.

Do not create modules like:

- `assistant_acp_backend.rs`
- `assistant_acp_protocol_adapter.rs`
- `assistant_plugin_integration.rs`

The module path should carry the context.

## Canonical Naming

### `assistant::action`

- `Action`

### `assistant::backend`

- `Id`
- `Kind`
- `Connect`
- `Caps`
- `Remote`
- `Handle`
- `Driver`
- `Command`
- `Event`
- `Update`
- `Error`

### `assistant::host`

- `Fs`
- `Terminal`
- `Permission`
- `Host`
- `Error`

### `assistant::permission`

- `Request`
- `Choice`
- `Kind`
- `Decision`

### `assistant::mode`

- `Id`
- `Caps`
- `Set`
- `Item`
- `Selected`

### `assistant::config`

- `Id`
- `ValueId`
- `Caps`
- `State`
- `Item`
- `Selected`

### `assistant::terminal`

- `Id`
- `Terminal`
- `State`

### `assistant::prompt`

- `Part`
- `Role`
- `Builder`
- `Request`

### `assistant::history`

- `Caps`
- `Cursor`
- `Stub`
- `Record`

### `assistant::acp`

- `Id`
- `Session`
- `Driver`
- `Error`

This follows the same rule as the other specs:
- use module context heavily
- avoid repeating "Assistant", "Acp", "Backend" in every type name

## Frontend Contract

This is the most important design point for plugin-friendly UI.

Frontends should interact with the assistant system through exactly two surfaces:

1. a derived read model
2. a typed action enum

That means:

- frontend reads `assistant::model::*`
- frontend dispatches `assistant::action::Action`
- frontend never consumes ACP wire types directly

### `assistant::action::Action`

This is the canonical user/frontend command surface.

Suggested shape:

```rust
pub enum Action {
    NewThread,
    LoadThread { thread: thread::Id },
    Activate { thread: thread::Id },
    Close { thread: thread::Id },

    SetDraft {
        thread: thread::Id,
        text: String,
    },
    Submit { thread: thread::Id },
    Cancel { thread: thread::Id },

    AttachContext {
        thread: thread::Id,
        item: context::Kind,
    },
    DetachContext {
        thread: thread::Id,
        item: context::Id,
    },

    Follow { thread: thread::Id },
    Unfollow { thread: thread::Id },

    SetMode {
        thread: thread::Id,
        mode: mode::Id,
    },
    SetConfig {
        thread: thread::Id,
        option: config::Id,
        value: config::ValueId,
    },

    ResolvePermission {
        thread: thread::Id,
        request: permission::RequestId,
        decision: permission::Decision,
    },
}
```

This action surface is backend-neutral and frontend-neutral.

### Derived model

The frontend reads derived assistant models and nothing ACP-specific.

That model already exists directionally in the collaboration spec:

- `assistant::model::Panel`
- `assistant::model::Tab`
- `assistant::model::ThreadView`
- `assistant::model::EntryView`
- `assistant::model::Pill`

ACP-specific transport details should be absent from the model except where they
have already been normalized into domain concepts.

## Backend Layer

ACP should not be the assistant domain API. ACP should be one backend driver.

### `assistant::backend::Driver`

Suggested shape:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kind(Cow<'static, str>);

pub mod kind {
    pub const ACP: Kind = Kind(Cow::Borrowed("acp"));
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Remote(Arc<str>);

pub struct ContextServer {
    pub id: Arc<str>,
    pub transport: ContextTransport,
}

pub enum ContextTransport {
    Http(Url),
    Sse(Url),
    Stdio {
        command: PathBuf,
        args: Vec<String>,
    },
}

pub struct Connect {
    pub scope: thread::Scope,
    pub context_servers: Vec<ContextServer>,
}

pub struct Caps {
    pub load_thread: bool,
    pub close_thread: bool,
    pub history: Option<history::Caps>,
    pub mode: Option<mode::Caps>,
    pub config: Option<config::Caps>,
    pub prompt: prompt::Caps,
    pub host: host::Caps,
}

pub trait Driver: Send + Sync {
    fn kind(&self) -> Kind;

    fn spawn(
        &self,
        runtime: &helix_runtime::Runtime,
        host: host::Set,
        tx: helix_runtime::Sender<Update>,
        connect: Connect,
    ) -> Result<Handle, Error>;
}
```

### Why capabilities stay data-driven

One of the cleaner improvements over Zed should be avoiding a family of optional
capability traits like:

- `supports_load_session()`
- `session_modes()` returning `Option<dyn Trait>`
- `session_config_options()` returning `Option<dyn Trait>`

Instead:

- capability presence is described by `backend::Caps`
- commands remain part of one stable `backend::Command` surface
- unsupported commands fail cleanly at the backend boundary if a caller violates capability checks

This is a better API because:

- fewer trait surfaces
- less `Option<Rc<dyn Trait>>` plumbing
- easier frontend capability checks
- cleaner mocking and testing

### `assistant::backend::Handle`

Suggested shape:

```rust
pub struct Handle {
    pub id: Id,
    tx: helix_runtime::Sender<Command>,
}

impl Handle {
    pub async fn send(&self, cmd: Command) -> Result<(), helix_runtime::Closed<Command>>;
    pub fn try_send(&self, cmd: Command) -> Result<(), helix_runtime::TrySend<Command>>;
}
```

`Handle` should not expose its sender directly. That keeps the backend boundary
intentional and leaves room for validation, tracing, or metrics later.

### `assistant::backend::Command`

This is what the assistant store/application sends to the backend runtime actor.

Suggested shape:

```rust
pub enum Command {
    NewThread {
        thread: thread::Id,
        scope: thread::Scope,
    },
    ListThreads {
        scope: thread::Scope,
        cursor: Option<history::Cursor>,
    },
    LoadThread {
        thread: thread::Id,
        remote: backend::Remote,
    },
    CloseThread {
        thread: thread::Id,
    },
    Submit {
        thread: thread::Id,
        prompt: prompt::Request,
    },
    Cancel {
        thread: thread::Id,
    },
    SetMode {
        thread: thread::Id,
        mode: mode::Id,
    },
    SetConfig {
        thread: thread::Id,
        option: config::Id,
        value: config::ValueId,
    },
    ResolvePermission {
        thread: thread::Id,
        request: permission::RequestId,
        decision: permission::Decision,
    },
}
```

### `assistant::backend::Update`

This is what the backend driver emits back into the runtime/application layer.

Suggested shape:

```rust
pub enum Event {
    Ready { caps: Caps },
    Stopped,
}

pub enum Update {
    Backend {
        backend: Id,
        event: Event,
    },
    Thread {
        thread: thread::Id,
        event: thread::Event,
    },
    History {
        scope: thread::Scope,
        entries: Vec<history::Stub>,
        next: Option<history::Cursor>,
    },
    Terminal {
        thread: thread::Id,
        event: terminal::Event,
    },
    Permission {
        thread: thread::Id,
        request: permission::Request,
    },
    Error {
        at: Target,
        error: Error,
    },
}

pub enum Target {
    Backend(Id),
    Thread(thread::Id),
}
```

This is the right separation because:

- backend command/update contracts are generic
- ACP becomes one implementation
- the UI never depends on ACP protocol types directly

### Make impossible states hard to represent

This ACP API should lean on types to eliminate avoidable ambiguity.

Key examples in this design:

- `backend::Target` instead of `Option<thread::Id>` on backend errors
- `backend::Remote` instead of raw remote-id strings in assistant-facing APIs
- `thread::Origin::Backend { backend, remote }` instead of ACP-specific identity fields
- `mode::Selected` and `config::Selected` instead of loose current/pending fields
- `permission::Request { default: Option<ChoiceId> }` instead of per-choice `default: bool`
- `host::Set` instead of one giant trait forcing unsupported capabilities to exist

Whenever an ACP-facing API starts drifting toward:

- raw strings
- optional field bags
- backend-specific enums in generic layers
- per-choice booleans that should be one selected id

we should stop and fix the type shape before implementation spreads it further.

The addition of `terminal::Event` resolves the terminal pressure point.

Terminal output should become a first-class assistant-domain stream/event model
instead of remaining only an opaque host side effect.

### History/listing boundary

ACP session history should normalize into generic assistant history shapes as soon
as it enters the assistant layer.

Decision:

- backend drivers may implement listing/loading however they need internally
- once exposed upward, ACP session list entries should become `assistant::history::Stub`
- full persisted/restored session content should become `assistant::history::Record`

That means ACP-specific listing metadata should not leak into frontend or assistant
domain APIs unless it becomes a consciously promoted generic concept later.

## ACP Driver Layer

### `assistant::acp::Driver`

ACP should implement `assistant::backend::Driver`.

Responsibilities:

- own ACP process/client/transport actor tasks
- translate backend `Command` into ACP protocol calls
- translate ACP notifications/host requests into backend `Update`
- normalize ACP-specific protocol oddities locally

### Connection identity

The assistant domain may be tempted to use an ACP-specific origin shape directly,
but that would still bake ACP transport identity directly into domain state.

This spec tightens that further.

Recommended direction:

```rust
pub enum thread::Origin {
    Backend {
        backend: backend::Id,
        remote: backend::Remote,
    },
    Local,
}
```

Why:

- local backend identity matters independently of remote thread/session id
- avoids the old "first connected agent wins" mindset
- multiple ACP backends or multiple connections to the same backend become coherent

### ACP-local ids

In `assistant::acp`:

```rust
pub struct Id(Arc<str>);
pub struct Session(Arc<str>);
```

ACP wire types may still use raw `String` aliases, but ACP adapter code should
normalize them immediately.

At the backend boundary, ACP session identity should be lowered to `backend::Remote`
before it enters the generic assistant store.

This is another place where the API should guard and clarify instead of relying on
convention.

## Prompt Content

Current Helix ACP code is still shaped too directly around ACP `ContentBlock`.

That is not the right assistant API if we want backend neutrality and plugin UI.

### Domain prompt content

Introduce a backend-neutral prompt content model:

```rust
pub enum prompt::Part {
    Text(String),
    Image(prompt::Image),
    Audio(prompt::Audio),
    Link(prompt::Link),
    Resource(prompt::Resource),
}

pub struct prompt::Caps {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

pub struct prompt::Request {
    pub thread: thread::Id,
    pub role: prompt::Role,
    pub parts: Vec<prompt::Part>,
}
```

Then:

- frontend actions build `prompt::Request`
- ACP adapter translates `prompt::Request` -> `helix_acp::types::ContentBlock`

This is much cleaner than building ACP `ContentBlock` directly in generic assistant code.

## Host Capability Layer

ACP host requests must not be hardwired to `Application` or to TUI popups.

### Narrow host traits

```rust
pub struct host::Caps {
    pub fs: FsCaps,
    pub terminal: Option<TerminalCaps>,
    pub permission: PermissionCaps,
}

pub struct FsCaps {
    pub read_text: bool,
    pub write_text: bool,
}

pub struct TerminalCaps;
pub struct PermissionCaps;

pub trait Fs: Send + Sync {
    async fn read_text(&self, path: &Path) -> Result<String, host::Error>;
    async fn write_text(&self, req: host::Write) -> Result<(), host::Error>;
}

pub trait Terminal: Send + Sync {
    async fn create(&self, req: host::CreateTerminal) -> Result<host::TerminalId, host::Error>;
    async fn output(&self, id: &host::TerminalId) -> Result<String, host::Error>;
    async fn wait(&self, id: &host::TerminalId) -> Result<host::ExitStatus, host::Error>;
    async fn kill(&self, id: &host::TerminalId) -> Result<(), host::Error>;
    async fn release(&self, id: &host::TerminalId) -> Result<(), host::Error>;
}

pub trait Permission: Send + Sync {
    async fn request(&self, req: permission::Request) -> Result<permission::Decision, host::Error>;
}

pub struct Set {
    pub fs: Arc<dyn Fs>,
    pub terminal: Option<Arc<dyn Terminal>>,
    pub permission: Arc<dyn Permission>,
}
```

This is better than one giant host trait because:

- narrower capabilities
- easier testing
- easier future optionality

`host::Set` is preferable to a single `dyn Host` supertrait because it can encode
optional capabilities like terminal support directly in the type.

## History Boundary

ACP supports backend-owned session history/listing. The assistant API should expose
that generically when present.

```rust
pub mod history {
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Cursor(Arc<str>);

    pub struct Caps {
        pub list: bool,
        pub load: bool,
        pub close: bool,
        pub resume: bool,
    }
}
```

This lets frontends ask for history/load/close capability without depending on ACP-specific traits.

## Permission Model

Do not expose ACP wire permission types to frontends.

### Domain permission types

```rust
pub struct permission::Request {
    pub id: permission::RequestId,
    pub thread: thread::Id,
    pub title: String,
    pub body: String,
    pub default: Option<permission::ChoiceId>,
    pub choices: Vec<permission::Choice>,
}

pub struct permission::Choice {
    pub id: permission::ChoiceId,
    pub label: String,
    pub kind: permission::Kind,
}

pub enum permission::Kind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Custom(Cow<'static, str>),
}

pub enum permission::Decision {
    Choose(permission::ChoiceId),
    Dismiss,
}
```

Why:

- frontend/plugin UIs should render semantic permission data, not ACP wire structs
- this also avoids UI-specific concepts like dropdown-vs-flat leaking into domain APIs
- storing the default on the request avoids the impossible state of multiple choices
  all claiming to be default

## Modes And Config

Current ACP flows still have optimistic local config mutation footguns.

We should model mode/config state as domain state with explicit pending values.

This also resolves the question of whether mode/config belongs on the generic
backend command surface.

Decision:

- `SetMode` and `SetConfig` stay in `assistant::backend::Command`
- support is advertised through `backend::Caps`
- frontends and application logic should gate those actions on capabilities

Why this is better than ACP-only capability traits:

- keeps the action/command surface stable across backends
- capability checks remain data-driven instead of trait-driven
- avoids a proliferation of backend-specific optional command traits

### Suggested shape

```rust
pub mod mode {
    pub struct Id(Arc<str>);

    pub struct Caps;

    pub struct Set {
        items: IndexMap<Id, Item>,
        selected: Selected,
    }

    pub enum Selected {
        Current(Id),
        Pending {
            current: Id,
            next: Id,
        },
    }

    pub struct Item {
        pub id: Id,
        pub name: String,
        pub description: Option<String>,
    }

    impl Set {
        pub fn selected(&self) -> &Selected;
        pub fn item(&self, id: &Id) -> Option<&Item>;
        pub fn items(&self) -> impl Iterator<Item = &Item>;
    }
}

pub mod config {
    pub struct Id(Arc<str>);
    pub struct ValueId(Arc<str>);

    pub struct Caps;

    pub struct State {
        items: IndexMap<Id, Item>,
    }

    pub struct Item {
        pub id: Id,
        pub name: String,
        pub selected: Selected,
        pub values: Vec<Value>,
    }

    pub enum Selected {
        Current(ValueId),
        Pending {
            current: ValueId,
            next: ValueId,
        },
    }

    impl State {
        pub fn item(&self, id: &Id) -> Option<&Item>;
        pub fn items(&self) -> impl Iterator<Item = &Item>;
    }
}
```

This makes optimistic-update state explicit instead of burying it in panel logic.

## Terminal Domain

This resolves the ACP terminal pressure point.

Terminal work should not remain just a host capability side effect. It should have
an assistant-domain representation so UI/plugin frontends can render it cleanly.

Suggested shape:

```rust
pub mod terminal {
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Id(Arc<str>);

    pub struct Terminal {
        pub id: Id,
        pub title: Option<String>,
        pub state: State,
    }

    pub enum State {
        Running,
        Exited { code: i32 },
        Failed { message: String },
    }

    pub enum Event {
        Open(Terminal),
        Output {
            id: Id,
            chunk: String,
        },
        Exit {
            id: Id,
            state: State,
        },
    }
}
```

The UI can still choose whether to render terminal output inline, in a scratch
buffer, or in a dedicated panel. But the domain event and state should be typed
and frontend-neutral.

## UI Plugin Boundary

If the ACP UI later moves into a plugin, that plugin should need only:

- `assistant::model::*`
- `assistant::action::Action`

It should *not* need:

- `helix_acp::types::*`
- `assistant::acp::*`
- `Application` internals
- `AcpPanel` internals
- host permission/file/terminal services directly

That is the clean plugin boundary.

## What Must Never Happen

These are the ACP-specific footguns we should explicitly forbid.

### 1. Dual thread stores

Do not build a second ACP-shaped thread store beside the assistant store.

### 2. UI state in ACP/domain objects

Do not store:

- scroll positions
- expanded rows
- markdown/render entities
- widget handles

inside ACP/session/thread domain types.

### 3. ACP wire types in frontend APIs

Do not make frontend models/actions depend on `helix_acp::types::*`.

### 4. Meta hacks outside ACP adapter layer

If ACP protocol deficiencies require normalization or extension metadata, that logic
must stay inside `assistant::acp`, not scatter upward.

### 5. Application mutating ACP panel directly

Application should mutate assistant state through actions/events/effects, not call
panel methods as the source of truth.

### 6. Host-service UI coupling

Permission/file/terminal host operations must not directly instantiate TUI widgets
or hardwire to `PermissionPopup`.

Those belong behind `host::*` traits and typed permission/domain models.

### 7. Index-keyed ACP UI state

Durable assistant behavior must never key on row index.

Everything durable should key on:

- `thread::Id`
- `thread::EntryId`
- `context::Id`
- `permission::RequestId`

## ACP-Specific Open Questions

The main ACP-specific pressure points are now resolved in this spec:

- mode/config stay on the generic backend command surface and are gated by `backend::Caps`
- terminal streaming has a typed assistant-domain `terminal::{Terminal, State, Event}` model
- ACP history/listing normalizes into `assistant::history::{Stub, Record}`

The remaining thing to watch during implementation is simply whether the generic
backend boundary stays cohesive as a second real backend appears.

## Definition Of Done

This ACP design is in the right shape when:

- ACP is one backend implementation over the assistant domain
- ACP wire types stop at the adapter boundary
- the assistant store is the only thread/session source of truth
- frontends/plugins use only assistant actions and models
- host services are abstracted behind typed host traits
- no ACP UI state is stored in protocol/domain objects

At that point ACP is cleanly integrated into the broader runtime and collaboration
architecture, and the UI can later move behind a plugin without re-architecting ACP itself.

## ACP Implementation Order

This is the ACP-specific sequence that best fits the broader runtime/collaboration plan.

### 1. Backend boundary first

Implement first:

- `assistant::backend::{Kind, Connect, Caps, Driver, Handle, Command, Update}`
- `assistant::host::{Fs, Terminal, Permission, Host}`

Reason:

- this locks the contract ACP must satisfy before any UI migration starts

### 2. ACP adapter second

Implement next:

- `assistant::acp::{Id, Session, Driver, Error}`
- ACP id normalization
- ACP wire -> backend update translation
- backend command -> ACP request translation

Reason:

- this establishes ACP as one backend implementation instead of a special panel path

### 3. Assistant store routing third

Implement next:

- `assistant::event::Event::Thread { .. }` routing from ACP backend updates
- `thread::Origin::Backend { backend, remote }`
- `thread::Event` application

Reason:

- once ACP updates are entering the store correctly, UI de-ownership can proceed safely

### 4. Frontend/plugin contract fourth

Implement next:

- `assistant::action::Action`
- derived `assistant::model::*`
- permission/mode/config/domain state surfaces

Reason:

- this creates the stable frontend contract that both TUI and future plugin UIs use

### 5. ACP TUI migration last

Only after the layers above are in place:

- reduce `AcpPanel` to a renderer/controller
- remove draft/session/tool state ownership from the panel
- key panel-local state by stable ids only

Reason:

- otherwise the TUI panel remains the hidden domain owner
