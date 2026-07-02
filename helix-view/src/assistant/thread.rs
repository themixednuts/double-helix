use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::path::PathBuf;

use crate::collab::{FollowState, Location};
use crate::id::Id as StableId;
use crate::DocumentId;

use super::{backend, change, config, context, mode, plan, review, terminal, tool};

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

impl Run {
    #[allow(non_upper_case_globals)]
    pub const Canceling: Self = Self::Waiting;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedState {
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    pub turns: Vec<Turn>,
    pub plan: Vec<plan::Item>,
    pub draft: String,
    pub context: Vec<context::Item>,
    pub follow: FollowState,
    pub run: Run,
    pub unread: bool,
    pub mode: Option<mode::Set>,
    pub config: config::State,
    pub terminals: Vec<terminal::Terminal>,
    pub review_mode: review::Mode,
    pub usage: Usage,
    pub commands: Vec<Command>,
    pub pending_elicitations: Vec<Elicitation>,
    pub caps: Option<helix_acp::AgentCaps>,
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
    review_mode: review::Mode,
    usage: Usage,
    commands: Vec<Command>,
    pending_elicitations: Vec<Elicitation>,
    caps: Option<helix_acp::AgentCaps>,
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
    Thought { text: String },
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
    Review(review::Event),
    Usage(UsageUpdate),
    Commands(Vec<Command>),
    Elicitation(ElicitationEvent),
    Caps(helix_acp::AgentCaps),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageUpdate {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: Option<String>,
    pub category: CommandCategory,
    pub arguments: Vec<CommandArgument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCategory {
    Native,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elicitation {
    pub id: String,
    pub status: ElicitationStatus,
    pub mode: ElicitationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationStatus {
    Pending,
    Completed,
    Declined,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationMode {
    Form {
        message: String,
        fields: Vec<ElicitationField>,
    },
    Url {
        message: String,
        url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationField {
    pub name: String,
    pub field_type: ElicitationFieldType,
    pub label: Option<String>,
    pub required: bool,
    pub options: Vec<ElicitationOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationFieldType {
    Text,
    Select,
    Bool,
    Textarea,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationEvent {
    Request(Elicitation),
    Complete { id: String, status: ElicitationStatus },
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
            review_mode: review::Mode::Write,
            usage: Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
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

    #[must_use]
    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
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

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    pub fn pending_elicitations(&self) -> &[Elicitation] {
        &self.pending_elicitations
    }

    pub fn caps(&self) -> Option<&helix_acp::AgentCaps> {
        self.caps.as_ref()
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

    #[must_use]
    pub fn is_backend(&self) -> bool {
        matches!(self.origin, Origin::Backend { .. })
    }

    #[must_use]
    pub fn backend_id(&self) -> Option<backend::Id> {
        match &self.origin {
            Origin::Backend { backend, .. } => Some(backend.clone()),
            Origin::Local => None,
        }
    }

    pub fn cycle_config(&self, category: &str) -> Option<(config::Id, config::ValueId)> {
        self.config.cycle(category)
    }

    pub fn next_mode(&self) -> Option<mode::Id> {
        let mode = self.mode.as_ref()?;
        match mode.selected() {
            mode::Selected::Current(current) => {
                let ids: Vec<_> = mode.items().map(|item| item.id.clone()).collect();
                if ids.is_empty() {
                    return None;
                }
                let idx = ids.iter().position(|id| id == current).unwrap_or(0);
                Some(ids[(idx + 1) % ids.len()].clone())
            }
            mode::Selected::Pending { next, .. } => Some(next.clone()),
        }
    }

    pub fn restore_persisted_state(&mut self, state: PersistedState) {
        self.title = state.title;
        self.entries = state.entries;
        self.turns = state.turns;
        self.plan = state.plan;
        self.draft = state.draft;
        self.context = state.context;
        self.follow = state.follow;
        self.run = state.run;
        self.unread = state.unread;
        self.mode = state.mode;
        self.config = state.config;
        self.terminals = state.terminals;
        self.review_mode = state.review_mode;
        self.usage = state.usage;
        self.commands = state.commands;
        self.pending_elicitations = state.pending_elicitations;
        self.caps = state.caps;
    }

    #[must_use]
    pub fn review_mode(&self) -> review::Mode {
        self.review_mode
    }

    pub fn set_review_mode(&mut self, mode: review::Mode) {
        self.review_mode = mode;
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

    #[must_use]
    pub fn selected(&self) -> Option<&Entry> {
        self.selected_entry().and_then(|id| self.entry(id))
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
            Event::Review(event) => self.apply_review(event),
            Event::Usage(update) => self.apply_usage(update),
            Event::Commands(commands) => self.commands = commands,
            Event::Elicitation(event) => self.apply_elicitation(event),
            Event::Caps(caps) => self.caps = Some(caps),
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
                EntryKind::Thought { text } => {
                    if let Some(Entry {
                        id,
                        kind: EntryKind::Thought { text: existing },
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
                            kind: EntryKind::Thought { text },
                            locations: normalize_locations(id, entry.locations),
                        });
                        self.view.folded.insert(id);
                    }
                }
                EntryKind::ToolCall(call) => {
                    if let Some(existing) = self.entries.iter_mut().rev().find(|entry| {
                        matches!(&entry.kind, EntryKind::ToolCall(current) if current.id == call.id)
                    }) {
                        let call = match &existing.kind {
                            EntryKind::ToolCall(current) => merge_tool_call(current, call),
                            _ => call,
                        };
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
                EntryKind::ChangeSummary(summary) if summary.files.iter().any(|file| file.review.is_some()) => {
                    if let Some(existing) = self.entries.iter_mut().rev().find(|entry| {
                        matches!(
                            &entry.kind,
                            EntryKind::ChangeSummary(current)
                                if same_review_files(current, &summary)
                        )
                    }) {
                        existing.turn = entry.turn;
                        existing.kind = EntryKind::ChangeSummary(summary);
                        existing.locations = normalize_locations(existing.id, entry.locations);
                    } else {
                        let id = self.next_entry_id();
                        self.entries.push(Entry {
                            id,
                            turn: entry.turn,
                            kind: EntryKind::ChangeSummary(summary),
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

    fn apply_review(&mut self, event: review::Event) {
        match event {
            review::Event::Mode(mode) => self.set_review_mode(mode),
            review::Event::Stage { file, mode } => {
                self.set_review_mode(mode);
                let locations = vec![Location::new(
                    file.path.clone(),
                    crate::collab::location::Source::Write,
                )];
                self.apply_content(Content::Append(NewEntry {
                    turn: None,
                    kind: EntryKind::ChangeSummary(change::Summary {
                        files: vec![change::File {
                            path: file.path.clone(),
                            hunks: Vec::new(),
                            review: Some(file),
                        }],
                    }),
                    locations,
                }));
            }
            review::Event::Resolve { target, decision } => {
                for entry in &mut self.entries {
                    let EntryKind::ChangeSummary(summary) = &mut entry.kind else {
                        continue;
                    };
                    for file in &mut summary.files {
                        let Some(review) = &mut file.review else {
                            continue;
                        };
                        let matches_target = match &target {
                            review::Target::All => true,
                            review::Target::File(path) => &review.path == path,
                        };
                        if matches_target && review.status.is_pending() {
                            review.resolve(decision);
                        }
                    }
                }
            }
        }
    }

    fn apply_usage(&mut self, update: UsageUpdate) {
        if let Some(value) = update.input_tokens {
            self.usage.input_tokens = value;
            self.usage.total_input_tokens = self.usage.total_input_tokens.saturating_add(value);
        }
        if let Some(value) = update.output_tokens {
            self.usage.output_tokens = value;
            self.usage.total_output_tokens = self.usage.total_output_tokens.saturating_add(value);
        }
        if let Some(value) = update.total_input_tokens {
            self.usage.total_input_tokens = value;
        }
        if let Some(value) = update.total_output_tokens {
            self.usage.total_output_tokens = value;
        }
        if let Some(value) = update.cache_creation_input_tokens {
            self.usage.cache_creation_input_tokens = value;
        }
        if let Some(value) = update.cache_read_input_tokens {
            self.usage.cache_read_input_tokens = value;
        }
    }

    fn apply_elicitation(&mut self, event: ElicitationEvent) {
        match event {
            ElicitationEvent::Request(elicitation) => {
                if let Some(existing) = self
                    .pending_elicitations
                    .iter_mut()
                    .find(|existing| existing.id == elicitation.id)
                {
                    *existing = elicitation;
                } else {
                    self.pending_elicitations.push(elicitation);
                }
            }
            ElicitationEvent::Complete { id, status } => {
                if let Some(existing) = self
                    .pending_elicitations
                    .iter_mut()
                    .find(|existing| existing.id == id)
                {
                    existing.status = status;
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

    #[must_use]
    pub fn change_summary(&self) -> Option<change::Summary> {
        collect_change_summaries(self.entries.iter().filter_map(Entry::change_summary))
    }

    #[must_use]
    pub fn change_summary_for(&self, entry: &Entry) -> Option<change::Summary> {
        if let Some(summary) = entry.change_summary() {
            return Some(summary);
        }

        let turn = entry.turn?;
        collect_change_summaries(
            self.entries
                .iter()
                .filter(|candidate| candidate.turn == Some(turn))
                .filter_map(Entry::change_summary),
        )
    }

    #[must_use]
    pub fn selected_change_summary(&self) -> Option<change::Summary> {
        self.selected()
            .and_then(|entry| self.change_summary_for(entry))
    }
}

impl Entry {
    #[must_use]
    pub fn change_summary(&self) -> Option<change::Summary> {
        match &self.kind {
            EntryKind::ChangeSummary(summary) => Some(summary.clone()),
            _ => None,
        }
    }
}

fn merge_tool_call(current: &tool::Call, mut next: tool::Call) -> tool::Call {
    if next.name == "tool" && current.name != "tool" {
        next.name.clone_from(&current.name);
    }
    if next.output.is_empty() {
        next.output.clone_from(&current.output);
    } else if !current.output.is_empty() {
        let mut output = current.output.clone();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&next.output);
        next.output = output;
    }
    if next.subagent.is_none() {
        next.subagent.clone_from(&current.subagent);
    }
    if next.sandbox.is_none() {
        next.sandbox.clone_from(&current.sandbox);
    }
    next
}

fn normalize_locations(entry: EntryId, locations: Vec<Location>) -> Vec<Location> {
    locations
        .into_iter()
        .map(|location| location.for_entry(entry))
        .collect()
}

fn same_review_files(current: &change::Summary, next: &change::Summary) -> bool {
    current.files.len() == next.files.len()
        && current
            .files
            .iter()
            .zip(&next.files)
            .all(|(current, next)| current.path == next.path)
}

fn collect_change_summaries(
    summaries: impl Iterator<Item = change::Summary>,
) -> Option<change::Summary> {
    let files = summaries
        .flat_map(|summary| summary.files)
        .collect::<Vec<_>>();
    (!files.is_empty()).then_some(change::Summary { files })
}
