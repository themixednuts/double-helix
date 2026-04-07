use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::path::PathBuf;

use crate::collab::{FollowState, Location};
use crate::id::Id as StableId;
use crate::DocumentId;

use super::{backend, change, config, context, mode, plan, terminal, tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadKind {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKindId {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnKind {}

pub type Id = StableId<ThreadKind, NonZeroU64>;
pub type EntryId = StableId<EntryKindId, NonZeroU64>;
pub type TurnId = StableId<TurnKind, NonZeroU64>;

#[must_use]
pub fn participant(id: Id) -> crate::collab::ParticipantId {
    crate::collab::ParticipantId::new(id.value())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub cwd: PathBuf,
    pub worktrees: Vec<PathBuf>,
}

impl Scope {
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            worktrees: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Backend {
        backend: backend::Id,
        remote: backend::Remote,
    },
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    Idle,
    Running,
    Waiting,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: Id,
    origin: Origin,
    title: Option<String>,
    entries: Vec<Entry>,
    turns: Vec<Turn>,
    plan: Vec<plan::Item>,
    draft: String,
    context: Vec<context::Item>,
    terminals: Vec<terminal::Terminal>,
    pub follow: FollowState,
    mode: Option<mode::Set>,
    config: config::State,
    run: Run,
    unread: bool,
    scope: Scope,
    view: ViewState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: Id,
    pub draft: String,
    pub context: Vec<context::Item>,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ViewState {
    pub focus: Focus,
    pub selected: Option<EntryId>,
    pub folded: HashSet<EntryId>,
    pub content_scroll: usize,
    pub opened_docs: HashMap<EntryId, DocumentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Input,
    Messages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub turn: Option<TurnId>,
    pub kind: EntryKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub turn: Option<TurnId>,
    pub kind: EntryKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    UserPrompt { text: String },
    AssistantText { text: String },
    ToolCall(tool::Call),
    Status { text: String },
    ChangeSummary(change::Summary),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: TurnId,
    pub prompt: EntryId,
    pub entries: Vec<EntryId>,
    pub changes: Vec<change::Id>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Content(Content),
    Plan(plan::Event),
    Meta(Meta),
    Mode(mode::Set),
    Config(config::State),
    Terminal(terminal::Event),
    Run(Run),
    Follow(Location),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Append(NewEntry),
    Replace { id: EntryId, entry: NewEntry },
    Remove { id: EntryId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Meta {
    Title(Option<String>),
}

impl Thread {
    #[must_use]
    pub fn new(id: Id, origin: Origin, scope: Scope) -> Self {
        Self {
            id,
            origin,
            title: None,
            entries: Vec::new(),
            turns: Vec::new(),
            plan: Vec::new(),
            draft: String::new(),
            context: Vec::new(),
            terminals: Vec::new(),
            follow: FollowState::Off,
            mode: None,
            config: config::State::new(Vec::new()),
            run: Run::Idle,
            unread: false,
            scope,
            view: ViewState::default(),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            id: self.id,
            draft: self.draft.clone(),
            context: self.context.clone(),
            scope: self.scope.clone(),
        }
    }

    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn set_origin(&mut self, origin: Origin) {
        self.origin = origin;
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn plan(&self) -> &[plan::Item] {
        &self.plan
    }

    pub fn set_plan(&mut self, plan: Vec<plan::Item>) {
        self.plan = plan;
    }

    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    pub fn set_draft(&mut self, draft: String) {
        self.draft = draft;
    }

    pub fn context_items(&self) -> &[context::Item] {
        &self.context
    }

    pub fn set_context_items(&mut self, context: Vec<context::Item>) {
        self.context = context;
    }

    pub fn push_context_item(&mut self, item: context::Item) {
        self.context.push(item);
    }

    pub fn retain_context_items(&mut self, mut keep: impl FnMut(&context::Item) -> bool) {
        self.context.retain(|item| keep(item));
    }

    pub fn terminals(&self) -> &[terminal::Terminal] {
        &self.terminals
    }

    pub fn set_terminals(&mut self, terminals: Vec<terminal::Terminal>) {
        self.terminals = terminals;
    }

    #[must_use]
    pub fn mode(&self) -> Option<&mode::Set> {
        self.mode.as_ref()
    }

    pub fn set_mode(&mut self, mode: Option<mode::Set>) {
        self.mode = mode;
    }

    pub fn mode_mut(&mut self) -> Option<&mut mode::Set> {
        self.mode.as_mut()
    }

    #[must_use]
    pub fn config(&self) -> &config::State {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut config::State {
        &mut self.config
    }

    pub fn set_config(&mut self, config: config::State) {
        self.config = config;
    }

    #[must_use]
    pub fn run(&self) -> &Run {
        &self.run
    }

    pub fn set_run(&mut self, run: Run) {
        self.run = run;
    }

    #[must_use]
    pub fn unread(&self) -> bool {
        self.unread
    }

    pub fn set_unread(&mut self, unread: bool) {
        self.unread = unread;
    }

    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn clone_scope(&self) -> Scope {
        self.scope.clone()
    }

    pub fn restore_persisted_state(
        &mut self,
        title: Option<String>,
        entries: Vec<Entry>,
        turns: Vec<Turn>,
        plan: Vec<plan::Item>,
        draft: String,
        context: Vec<context::Item>,
        follow: FollowState,
        run: Run,
        unread: bool,
        mode: Option<mode::Set>,
        config: config::State,
        terminals: Vec<terminal::Terminal>,
    ) {
        self.title = title;
        self.entries = entries;
        self.turns = turns;
        self.plan = plan;
        self.draft = draft;
        self.context = context;
        self.follow = follow;
        self.run = run;
        self.unread = unread;
        self.mode = mode;
        self.config = config;
        self.terminals = terminals;
    }

    #[must_use]
    pub fn follow(&self) -> &FollowState {
        &self.follow
    }

    pub fn set_follow(&mut self, follow: FollowState) {
        self.follow = follow;
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        self.view.focus
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<EntryId> {
        self.view.selected
    }

    pub fn folded_entries(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.view.folded.iter().copied()
    }

    #[must_use]
    pub fn is_folded(&self, entry: EntryId) -> bool {
        self.view.folded.contains(&entry)
    }

    #[must_use]
    pub fn content_scroll(&self) -> usize {
        self.view.content_scroll
    }

    #[must_use]
    pub fn opened_doc(&self, entry: EntryId) -> Option<DocumentId> {
        self.view.opened_docs.get(&entry).copied()
    }

    #[must_use]
    pub fn opened_docs(&self) -> &HashMap<EntryId, DocumentId> {
        &self.view.opened_docs
    }

    pub fn restore_transient_view<I>(
        &mut self,
        focus: Focus,
        selected: Option<EntryId>,
        folded: I,
        content_scroll: usize,
    ) where
        I: IntoIterator<Item = EntryId>,
    {
        self.restore_view(focus, selected, folded, content_scroll);
    }

    pub fn set_focus(&mut self, focus: Focus) {
        self.view.focus = focus;
    }

    pub fn set_selected_entry(&mut self, entry: Option<EntryId>) {
        self.view.selected = entry;
    }

    pub fn set_content_scroll(&mut self, content_scroll: usize) {
        self.view.content_scroll = content_scroll;
    }

    pub fn set_folded(&mut self, entry: EntryId, folded: bool) {
        if folded {
            self.view.folded.insert(entry);
        } else {
            self.view.folded.remove(&entry);
        }
    }

    pub fn track_opened_doc(&mut self, entry: EntryId, doc: DocumentId) {
        self.view.opened_docs.insert(entry, doc);
    }

    pub fn untrack_opened_doc(&mut self, entry: EntryId) {
        self.view.opened_docs.remove(&entry);
    }

    #[must_use]
    pub fn untrack_document(&mut self, doc: DocumentId) -> bool {
        let before = self.view.opened_docs.len();
        self.view.opened_docs.retain(|_, current| current != &doc);
        self.view.opened_docs.len() != before
    }

    pub fn restore_view<I>(
        &mut self,
        focus: Focus,
        selected: Option<EntryId>,
        folded: I,
        content_scroll: usize,
    ) where
        I: IntoIterator<Item = EntryId>,
    {
        self.view.focus = focus;
        self.view.selected = selected;
        self.view.folded = folded.into_iter().collect();
        self.view.content_scroll = content_scroll;
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Content(content) => self.apply_content(content),
            Event::Plan(plan::Event::Replace(items)) => self.set_plan(items),
            Event::Meta(Meta::Title(title)) => self.set_title(title),
            Event::Mode(mode) => self.set_mode(Some(mode)),
            Event::Config(config) => self.set_config(config),
            Event::Terminal(event) => self.apply_terminal(event),
            Event::Run(run) => self.set_run(run),
            Event::Follow(location) => match &mut self.follow {
                FollowState::Off => {}
                FollowState::On { last, .. } | FollowState::Paused { last, .. } => {
                    *last = Some(location);
                }
            },
        }
    }

    fn apply_content(&mut self, content: Content) {
        match content {
            Content::Append(entry) => match entry.kind {
                EntryKind::AssistantText { text } => {
                    if let Some(Entry {
                        id,
                        kind: EntryKind::AssistantText { text: existing },
                        locations,
                        ..
                    }) = self.entries.last_mut()
                    {
                        existing.push_str(&text);
                        locations.extend(normalize_locations(*id, entry.locations));
                    } else {
                        let id = self.next_entry_id();
                        self.entries.push(Entry {
                            id,
                            turn: entry.turn,
                            kind: EntryKind::AssistantText { text },
                            locations: normalize_locations(id, entry.locations),
                        });
                    }
                }
                EntryKind::ToolCall(call) => {
                    if let Some(existing) = self.entries.iter_mut().rev().find(|entry| {
                        matches!(&entry.kind, EntryKind::ToolCall(current) if current.id == call.id)
                    }) {
                        existing.turn = entry.turn;
                        existing.kind = EntryKind::ToolCall(call);
                        existing.locations = normalize_locations(existing.id, entry.locations);
                    } else {
                        let id = self.next_entry_id();
                        self.entries.push(Entry {
                            id,
                            turn: entry.turn,
                            kind: EntryKind::ToolCall(call),
                            locations: normalize_locations(id, entry.locations),
                        });
                    }
                }
                kind => {
                    let id = self.next_entry_id();
                    self.entries.push(Entry {
                        id,
                        turn: entry.turn,
                        kind,
                        locations: normalize_locations(id, entry.locations),
                    });
                }
            },
            Content::Replace { id, entry } => {
                if let Some(existing) = self.entries.iter_mut().find(|existing| existing.id == id) {
                    existing.turn = entry.turn;
                    existing.kind = entry.kind;
                    existing.locations = normalize_locations(existing.id, entry.locations);
                }
            }
            Content::Remove { id } => {
                self.entries.retain(|entry| entry.id != id);
                self.view.folded.remove(&id);
                self.view.opened_docs.remove(&id);
                if self.view.selected == Some(id) {
                    self.view.selected = self.entries.last().map(|entry| entry.id);
                }
            }
        }
    }

    fn apply_terminal(&mut self, event: terminal::Event) {
        match event {
            terminal::Event::Open(terminal) => {
                if let Some(existing) = self
                    .terminals
                    .iter_mut()
                    .find(|item| item.id == terminal.id)
                {
                    *existing = terminal;
                } else {
                    self.terminals.push(terminal);
                }
            }
            terminal::Event::Output { id, chunk } => {
                if let Some(existing) = self.terminals.iter_mut().find(|item| item.id == id) {
                    existing.output.push_str(&chunk);
                } else {
                    self.terminals.push(terminal::Terminal {
                        id,
                        title: None,
                        state: terminal::State::Running,
                        output: chunk,
                    });
                }
            }
            terminal::Event::Exit { id, state } => {
                if let Some(existing) = self.terminals.iter_mut().find(|item| item.id == id) {
                    existing.state = state;
                } else {
                    self.terminals.push(terminal::Terminal {
                        id,
                        title: None,
                        state,
                        output: String::new(),
                    });
                }
            }
        }
    }

    fn next_entry_id(&self) -> EntryId {
        EntryId::new(NonZeroU64::new(self.entries.len() as u64 + 1).unwrap())
    }
}

fn normalize_locations(entry: EntryId, locations: Vec<Location>) -> Vec<Location> {
    locations
        .into_iter()
        .map(|location| location.for_entry(entry))
        .collect()
}
