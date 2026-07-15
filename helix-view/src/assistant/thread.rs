use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::path::PathBuf;

use crate::collab::{FollowState, Location};
use crate::id::Id as StableId;
use crate::DocumentId;

use super::{auth, backend, change, config, context, mode, plan, profile, review, terminal, tool};

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
    pub auth: auth::State,
    pub review_mode: review::Mode,
    pub usage: Usage,
    pub commands: Vec<Command>,
    pub pending_elicitations: Vec<Elicitation>,
    pub caps: Option<helix_acp::AgentCaps>,
    pub profile: Option<profile::Defaults>,
    pub feedback: Feedback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: Id,
    origin: Origin,
    title: Option<String>,
    entries: Vec<Entry>,
    content_revision: u64,
    turns: Vec<Turn>,
    plan: Vec<plan::Item>,
    draft: String,
    context: Vec<context::Item>,
    terminals: Vec<terminal::Terminal>,
    auth: auth::State,
    review_mode: review::Mode,
    usage: Usage,
    commands: Vec<Command>,
    pending_elicitations: Vec<Elicitation>,
    caps: Option<helix_acp::AgentCaps>,
    profile: Option<profile::Active>,
    feedback: Feedback,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rating {
    #[default]
    None,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Feedback {
    pub rating: Rating,
    pub note: Option<String>,
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
    pub stream: Option<StreamId>,
    pub kind: EntryKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub turn: Option<TurnId>,
    pub stream: Option<StreamId>,
    pub kind: EntryKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamId {
    pub kind: StreamKind,
    pub id: String,
}

impl StreamId {
    #[must_use]
    pub fn user_prompt(id: impl Into<String>) -> Self {
        Self {
            kind: StreamKind::UserPrompt,
            id: id.into(),
        }
    }

    #[must_use]
    pub fn assistant_text(id: impl Into<String>) -> Self {
        Self {
            kind: StreamKind::AssistantText,
            id: id.into(),
        }
    }

    #[must_use]
    pub fn thought(id: impl Into<String>) -> Self {
        Self {
            kind: StreamKind::Thought,
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    UserPrompt,
    AssistantText,
    Thought,
}

#[allow(
    clippy::large_enum_variant,
    reason = "assistant entries are persisted and pattern-matched broadly; boxing ToolCall would create migration churn"
)]
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
    Auth(auth::Event),
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
    pub context_used_tokens: u64,
    pub context_window_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageUpdate {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub context_used_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
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
    Complete {
        id: String,
        status: ElicitationStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationValue {
    String(String),
    Integer(i64),
    Number(String),
    Boolean(bool),
    StringArray(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitationResponse {
    Accept(Vec<(String, ElicitationValue)>),
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Append(NewEntry),
    Stream(NewEntry),
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
            content_revision: 0,
            turns: Vec::new(),
            plan: Vec::new(),
            draft: String::new(),
            context: Vec::new(),
            terminals: Vec::new(),
            auth: auth::State::default(),
            review_mode: review::Mode::Write,
            usage: Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
            profile: None,
            feedback: Feedback::default(),
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

    pub fn auth(&self) -> &auth::State {
        &self.auth
    }

    pub fn auth_mut(&mut self) -> &mut auth::State {
        &mut self.auth
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

    #[must_use]
    pub fn profile(&self) -> Option<&profile::Active> {
        self.profile.as_ref()
    }

    #[must_use]
    pub fn profile_name(&self) -> Option<&str> {
        self.profile.as_ref().map(profile::Active::name)
    }

    pub fn set_profile(&mut self, profile: Option<profile::Defaults>) {
        self.profile = profile.map(profile::Active::new);
    }

    pub fn profile_mut(&mut self) -> Option<&mut profile::Active> {
        self.profile.as_mut()
    }

    #[must_use]
    pub fn feedback(&self) -> &Feedback {
        &self.feedback
    }

    pub fn set_rating(&mut self, rating: Rating) {
        self.feedback.rating = rating;
    }

    pub fn toggle_rating(&mut self, rating: Rating) {
        self.feedback.rating = if self.feedback.rating == rating {
            Rating::None
        } else {
            rating
        };
    }

    pub fn set_note(&mut self, note: Option<String>) {
        self.feedback.note = note.and_then(|note| {
            let note = note.trim().to_string();
            (!note.is_empty()).then_some(note)
        });
    }

    pub fn set_terminals(&mut self, terminals: Vec<terminal::Terminal>) {
        self.terminals = terminals;
    }

    pub fn set_auth(&mut self, auth: auth::State) {
        self.auth = auth;
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
        self.content_revision = self.content_revision.wrapping_add(1);
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
        self.auth = state.auth;
        self.review_mode = state.review_mode;
        self.usage = state.usage;
        self.commands = state.commands;
        self.pending_elicitations = state.pending_elicitations;
        self.caps = state.caps;
        self.profile = state.profile.map(profile::Active::restored);
        self.feedback = state.feedback;
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
    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    #[must_use]
    pub fn selected(&self) -> Option<&Entry> {
        self.selected_entry().and_then(|id| self.entry(id))
    }

    #[must_use]
    pub fn selected_user_prompt(&self) -> Option<(EntryId, String)> {
        let entry = self.selected()?;
        let EntryKind::UserPrompt { text } = &entry.kind else {
            return None;
        };
        Some((entry.id, text.clone()))
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
    pub fn fork_before(&mut self, entry: EntryId) -> bool {
        let Some(index) = self.entries.iter().position(|current| current.id == entry) else {
            return false;
        };
        let removed = self
            .entries
            .drain(index..)
            .map(|entry| entry.id)
            .collect::<HashSet<_>>();
        self.turns.retain(|turn| {
            turn.prompt != entry && !turn.entries.iter().any(|entry| removed.contains(entry))
        });
        self.view.folded.retain(|entry| !removed.contains(entry));
        self.view
            .opened_docs
            .retain(|entry, _| !removed.contains(entry));
        if self
            .view
            .selected
            .is_some_and(|entry| removed.contains(&entry))
        {
            self.view.selected = self.entries.last().map(|entry| entry.id);
        }
        self.content_revision = self.content_revision.wrapping_add(1);
        true
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
            Event::Content(content) => {
                self.apply_content(content);
                self.content_revision = self.content_revision.wrapping_add(1);
            }
            Event::Plan(plan::Event::Replace(items)) => self.set_plan(items),
            Event::Meta(Meta::Title(title)) => self.set_title(title),
            Event::Mode(mode) => self.set_mode(Some(mode)),
            Event::Config(config) => self.set_config(config),
            Event::Terminal(event) => self.apply_terminal(event),
            Event::Auth(event) => {
                let retry = self.auth.apply(event);
                if let Some(text) = retry {
                    self.draft = text;
                }
            }
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
            Content::Stream(entry) => {
                if entry.stream.is_none() {
                    if let Some(existing) = self.entries.last_mut() {
                        let merged = match (&mut existing.kind, &entry.kind) {
                            (
                                EntryKind::UserPrompt { text: existing },
                                EntryKind::UserPrompt { text },
                            )
                            | (
                                EntryKind::AssistantText { text: existing },
                                EntryKind::AssistantText { text },
                            )
                            | (
                                EntryKind::Thought { text: existing },
                                EntryKind::Thought { text },
                            ) => {
                                existing.push_str(text);
                                true
                            }
                            _ => false,
                        };
                        if merged {
                            existing.turn = entry.turn;
                            existing.locations.extend(normalize_locations(
                                existing.id,
                                entry.locations,
                            ));
                            return;
                        }
                    }
                }
                self.apply_content(Content::Append(entry));
            }
            Content::Append(entry) => match entry.kind {
                EntryKind::UserPrompt { text } => {
                    if let Some(existing) = entry
                        .stream
                        .as_ref()
                        .and_then(|stream| self.stream_entry_mut(stream))
                    {
                        if let EntryKind::UserPrompt {
                            text: existing_text,
                        } = &mut existing.kind
                        {
                            existing_text.push_str(&text);
                            existing.turn = entry.turn;
                            existing
                                .locations
                                .extend(normalize_locations(existing.id, entry.locations));
                            return;
                        }
                    }
                    let id = self.next_entry_id();
                    self.entries.push(Entry {
                        id,
                        turn: entry.turn,
                        stream: entry.stream,
                        kind: EntryKind::UserPrompt { text },
                        locations: normalize_locations(id, entry.locations),
                    });
                }
                EntryKind::AssistantText { text } => {
                    if let Some(existing) = entry
                        .stream
                        .as_ref()
                        .and_then(|stream| self.stream_entry_mut(stream))
                    {
                        if let EntryKind::AssistantText { text: existing_text } = &mut existing.kind
                        {
                            existing_text.push_str(&text);
                            existing.turn = entry.turn;
                            existing
                                .locations
                                .extend(normalize_locations(existing.id, entry.locations));
                            return;
                        }
                    }
                    let id = self.next_entry_id();
                    self.entries.push(Entry {
                        id,
                        turn: entry.turn,
                        stream: entry.stream,
                        kind: EntryKind::AssistantText { text },
                        locations: normalize_locations(id, entry.locations),
                    });
                }
                EntryKind::Thought { text } => {
                    if let Some(existing) = entry
                        .stream
                        .as_ref()
                        .and_then(|stream| self.stream_entry_mut(stream))
                    {
                        if let EntryKind::Thought { text: existing_text } = &mut existing.kind {
                            existing_text.push_str(&text);
                            existing.turn = entry.turn;
                            existing
                                .locations
                                .extend(normalize_locations(existing.id, entry.locations));
                            return;
                        }
                    }
                    let id = self.next_entry_id();
                    self.entries.push(Entry {
                        id,
                        turn: entry.turn,
                        stream: entry.stream,
                        kind: EntryKind::Thought { text },
                        locations: normalize_locations(id, entry.locations),
                    });
                    self.view.folded.insert(id);
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
                        existing.stream = entry.stream;
                        existing.kind = EntryKind::ToolCall(call);
                        existing.locations = normalize_locations(existing.id, entry.locations);
                    } else {
                        let id = self.next_entry_id();
                        self.entries.push(Entry {
                            id,
                            turn: entry.turn,
                            stream: entry.stream,
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
                        existing.stream = entry.stream;
                        existing.kind = EntryKind::ChangeSummary(summary);
                        existing.locations = normalize_locations(existing.id, entry.locations);
                    } else {
                        let id = self.next_entry_id();
                        self.entries.push(Entry {
                            id,
                            turn: entry.turn,
                            stream: entry.stream,
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
                        stream: entry.stream,
                        kind,
                        locations: normalize_locations(id, entry.locations),
                    });
                }
            },
            Content::Replace { id, entry } => {
                if let Some(existing) = self.entries.iter_mut().find(|existing| existing.id == id) {
                    existing.turn = entry.turn;
                    existing.stream = entry.stream;
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
                    stream: None,
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
        if let Some(value) = update.context_used_tokens {
            self.usage.context_used_tokens = value;
        }
        if let Some(value) = update.context_window_tokens {
            self.usage.context_window_tokens = value;
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

    fn stream_entry_mut(&mut self, stream: &StreamId) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.stream.as_ref() == Some(stream))
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
    if is_unspecified_tool_state(&next.state) {
        next.state.clone_from(&current.state);
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

fn is_unspecified_tool_state(state: &tool::State) -> bool {
    matches!(state, tool::State::Unknown(value) if value.as_ref() == "unspecified")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> Thread {
        Thread::new(
            Id::new(NonZeroU64::new(1).unwrap()),
            Origin::Local,
            Scope::new(std::path::PathBuf::from(".")),
        )
    }

    fn append(thread: &mut Thread, kind: EntryKind) -> EntryId {
        thread.apply(Event::Content(Content::Append(NewEntry {
            turn: None,
            stream: None,
            kind,
            locations: Vec::new(),
        })));
        thread.entries().last().expect("entry").id
    }

    fn append_stream(thread: &mut Thread, stream: StreamId, kind: EntryKind) -> EntryId {
        thread.apply(Event::Content(Content::Append(NewEntry {
            turn: None,
            stream: Some(stream),
            kind,
            locations: Vec::new(),
        })));
        thread.entries().last().expect("entry").id
    }

    fn append_legacy_stream(thread: &mut Thread, kind: EntryKind) -> EntryId {
        thread.apply(Event::Content(Content::Stream(NewEntry {
            turn: None,
            stream: None,
            kind,
            locations: Vec::new(),
        })));
        thread.entries().last().expect("entry").id
    }

    #[test]
    fn content_revision_changes_only_with_transcript_content() {
        let mut thread = thread();
        assert_eq!(thread.content_revision(), 0);

        let first = append(
            &mut thread,
            EntryKind::AssistantText {
                text: "first".into(),
            },
        );
        assert_eq!(thread.content_revision(), 1);

        thread.apply(Event::Meta(Meta::Title(Some("title".into()))));
        assert_eq!(thread.content_revision(), 1);

        assert!(thread.fork_before(first));
        assert_eq!(thread.content_revision(), 2);
    }

    #[test]
    fn fork_before_user_prompt_drops_target_and_later_entries() {
        let mut thread = thread();
        append(&mut thread, EntryKind::UserPrompt { text: "U0".into() });
        append(&mut thread, EntryKind::AssistantText { text: "A0".into() });
        let u1 = append(&mut thread, EntryKind::UserPrompt { text: "U1".into() });
        append(&mut thread, EntryKind::AssistantText { text: "A1".into() });
        append(&mut thread, EntryKind::UserPrompt { text: "U2".into() });

        assert!(thread.fork_before(u1));

        let texts = thread
            .entries()
            .iter()
            .map(|entry| match &entry.kind {
                EntryKind::UserPrompt { text } | EntryKind::AssistantText { text } => text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["U0", "A0"]);
    }

    #[test]
    fn streamed_assistant_text_chunks_merge_by_stream_id() {
        let mut thread = thread();
        let stream = StreamId::assistant_text("msg-agent");
        let first = append_stream(
            &mut thread,
            stream.clone(),
            EntryKind::AssistantText {
                text: "Rece".into(),
            },
        );
        append(
            &mut thread,
            EntryKind::Status {
                text: "between".into(),
            },
        );
        append_stream(
            &mut thread,
            stream,
            EntryKind::AssistantText {
                text: "ived.".into(),
            },
        );

        assert_eq!(thread.entries().len(), 2);
        assert_eq!(thread.entries()[0].id, first);
        assert!(matches!(
            &thread.entries()[0].kind,
            EntryKind::AssistantText { text } if text == "Received."
        ));
    }

    #[test]
    fn streamed_user_prompt_chunks_merge_by_stream_id() {
        let mut thread = thread();
        let stream = StreamId::user_prompt("msg-user");
        let first = append_stream(
            &mut thread,
            stream.clone(),
            EntryKind::UserPrompt { text: "hel".into() },
        );
        append_stream(
            &mut thread,
            stream,
            EntryKind::UserPrompt { text: "lo".into() },
        );

        assert_eq!(thread.entries().len(), 1);
        assert_eq!(thread.entries()[0].id, first);
        assert!(matches!(
            &thread.entries()[0].kind,
            EntryKind::UserPrompt { text } if text == "hello"
        ));
    }

    #[test]
    fn streamed_thought_chunks_merge_by_stream_id() {
        let mut thread = thread();
        let stream = StreamId::thought("msg-thought");
        append_stream(
            &mut thread,
            stream.clone(),
            EntryKind::Thought {
                text: "think".into(),
            },
        );
        append_stream(
            &mut thread,
            stream,
            EntryKind::Thought { text: "ing".into() },
        );

        assert_eq!(thread.entries().len(), 1);
        assert!(matches!(
            &thread.entries()[0].kind,
            EntryKind::Thought { text } if text == "thinking"
        ));
    }

    #[test]
    fn different_assistant_text_streams_create_distinct_entries() {
        let mut thread = thread();
        append_stream(
            &mut thread,
            StreamId::assistant_text("msg-a"),
            EntryKind::AssistantText {
                text: "Received.".into(),
            },
        );
        append_stream(
            &mut thread,
            StreamId::assistant_text("msg-b"),
            EntryKind::AssistantText {
                text: "Received.".into(),
            },
        );

        assert_eq!(thread.entries().len(), 2);
        assert_eq!(
            thread.entries()[0].stream,
            Some(StreamId::assistant_text("msg-a"))
        );
        assert_eq!(
            thread.entries()[1].stream,
            Some(StreamId::assistant_text("msg-b"))
        );
    }

    #[test]
    fn unstreamed_assistant_text_chunks_still_append_adjacently() {
        let mut thread = thread();
        append_legacy_stream(
            &mut thread,
            EntryKind::AssistantText {
                text: "Rece".into(),
            },
        );
        append_legacy_stream(
            &mut thread,
            EntryKind::AssistantText {
                text: "ived.".into(),
            },
        );

        assert_eq!(thread.entries().len(), 1);
        assert!(matches!(
            &thread.entries()[0].kind,
            EntryKind::AssistantText { text } if text == "Received."
        ));
    }

    #[test]
    fn tool_update_without_status_preserves_current_state() {
        let mut thread = thread();
        append(
            &mut thread,
            EntryKind::ToolCall(tool::Call {
                id: tool::Id::new("tool-1"),
                name: "Search".to_string(),
                state: tool::State::Running,
                output: String::new(),
                subagent: None,
                sandbox: None,
            }),
        );
        append(
            &mut thread,
            EntryKind::ToolCall(tool::Call {
                id: tool::Id::new("tool-1"),
                name: "tool".to_string(),
                state: tool::State::Unknown("unspecified".into()),
                output: "partial".to_string(),
                subagent: None,
                sandbox: None,
            }),
        );

        assert_eq!(thread.entries().len(), 1);
        assert!(matches!(
            &thread.entries()[0].kind,
            EntryKind::ToolCall(call)
                if call.name == "Search"
                    && call.state == tool::State::Running
                    && call.output == "partial"
        ));
    }
}
