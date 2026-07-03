use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Backend, Record, Stub, View};
use crate::assistant::{
    change, config, context, mode, plan, profile, review, terminal, thread, tool,
};
use crate::collab::{self, location};

pub fn local_backend() -> Backend {
    Backend::Local(LocalHistory {
        root: helix_loader::cache_dir().join("assistant").join("history"),
        store_paths: None,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct LocalHistory {
    root: PathBuf,
    store_paths: Option<helix_store::StorePaths>,
}

impl LocalHistory {
    fn path(&self, id: thread::Id) -> PathBuf {
        self.root.join(format!("{}.json", id.value().get()))
    }

    async fn entries(&self) -> anyhow::Result<Vec<PersistedThread>> {
        let mut out = Vec::new();
        let Ok(mut dir) = tokio::fs::read_dir(&self.root).await else {
            return Ok(out);
        };

        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(raw) => match serde_json::from_str::<PersistedThread>(&raw) {
                    Ok(item) => out.push(item),
                    Err(err) => log::warn!("assistant history decode failed {:?}: {}", path, err),
                },
                Err(err) => log::warn!("assistant history read failed {:?}: {}", path, err),
            }
        }

        out.sort_by_key(|item| item.stub.id);
        Ok(out)
    }

    fn store_paths(&self) -> helix_store::StorePaths {
        self.store_paths
            .clone()
            .unwrap_or_else(helix_store::StorePaths::default_paths)
    }

    fn open_imported_store(&self) -> anyhow::Result<helix_store::Store> {
        let paths = self.store_paths();
        import_legacy_from_paths(
            &self.root,
            crate::assistant::layout::layout_path(),
            crate::assistant::permission::Rules::path(),
            paths.clone(),
        )?;
        Ok(helix_store::Store::open(paths)?)
    }
}

impl LocalHistory {
    pub async fn load_scope(&self, scope: &thread::Scope) -> anyhow::Result<Vec<Stub>> {
        let store_scope = crate::assistant::layout::scope_key(scope)?;
        let store_backend = self.clone();
        match tokio::task::spawn_blocking(move || {
            let mut store = store_backend.open_imported_store()?;
            let rows = store.threads().list_by_scope(&store_scope)?;
            let entries = rows
                .into_iter()
                .filter_map(|row| match persisted_thread_from_store(row) {
                    Ok(thread) => Some(thread.stub.into_domain()),
                    Err(err) => {
                        log::warn!("assistant history store decode failed: {err}");
                        None
                    }
                })
                .collect::<Vec<_>>();
            Ok::<_, anyhow::Error>(entries)
        })
        .await
        {
            Ok(Ok(entries)) => Ok(entries),
            Ok(Err(err)) => {
                log::warn!("assistant history store load failed, falling back to JSON: {err}");
                self.load_scope_from_files(scope).await
            }
            Err(err) => {
                log::warn!("assistant history store load task failed, falling back to JSON: {err}");
                self.load_scope_from_files(scope).await
            }
        }
    }

    pub async fn load(&self, id: thread::Id) -> anyhow::Result<Option<Record>> {
        let store_backend = self.clone();
        let store_id = id.value().get().to_string();
        match tokio::task::spawn_blocking(move || {
            let mut store = store_backend.open_imported_store()?;
            store
                .threads()
                .get(&store_id)?
                .map(persisted_thread_from_store)
                .transpose()?
                .map(|thread| thread.record.into_domain())
                .transpose()
        })
        .await
        {
            Ok(Ok(record)) => return Ok(record),
            Ok(Err(err)) => {
                log::warn!("assistant history store get failed, falling back to JSON: {err}");
            }
            Err(err) => {
                log::warn!("assistant history store get task failed, falling back to JSON: {err}");
            }
        }

        let path = self.path(id);
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let entry = match serde_json::from_str::<PersistedThread>(&raw) {
            Ok(entry) => entry,
            Err(err) => {
                log::warn!("assistant history decode failed {:?}: {}", path, err);
                return Ok(None);
            }
        };
        Ok(Some(entry.record.into_domain()?))
    }

    pub async fn save(&self, record: Record) -> anyhow::Result<()> {
        let store_backend = self.clone();
        let store_thread = store_thread_from_record(&record)?;
        match tokio::task::spawn_blocking(move || {
            let mut store = store_backend.open_imported_store()?;
            store.threads().upsert(store_thread)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                log::warn!("assistant history store save failed, falling back to JSON: {err}");
            }
            Err(err) => {
                log::warn!("assistant history store save task failed, falling back to JSON: {err}");
            }
        }

        let path = self.path(record.id);
        let payload = match PersistedThread::from_domain(&record) {
            Ok(payload) => payload,
            Err(err) => return Err(err),
        };
        crate::assistant::layout::atomic_write(&path, &serde_json::to_vec_pretty(&payload)?)
            .await?;
        Ok(())
    }

    pub async fn delete(&self, id: thread::Id) -> anyhow::Result<()> {
        let store_backend = self.clone();
        let store_id = id.value().get().to_string();
        match tokio::task::spawn_blocking(move || {
            let mut store = store_backend.open_imported_store()?;
            store.threads().delete(&store_id)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                log::warn!("assistant history store delete failed, falling back to JSON: {err}");
            }
            Err(err) => {
                log::warn!(
                    "assistant history store delete task failed, falling back to JSON: {err}"
                );
            }
        }

        let path = self.path(id);
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    async fn load_scope_from_files(&self, scope: &thread::Scope) -> anyhow::Result<Vec<Stub>> {
        let scope = scope.clone();
        Ok(self
            .entries()
            .await?
            .into_iter()
            .filter(|entry| entry.stub.scope == PersistedScope::from(&scope))
            .map(|entry| entry.stub.into_domain())
            .collect())
    }
}

pub(super) fn import_legacy_if_needed_blocking() -> anyhow::Result<()> {
    import_legacy_from_paths(
        helix_loader::cache_dir().join("assistant").join("history"),
        crate::assistant::layout::layout_path(),
        crate::assistant::permission::Rules::path(),
        helix_store::StorePaths::default_paths(),
    )
}

fn import_legacy_from_paths(
    history_root: impl AsRef<Path>,
    layout_path: impl AsRef<Path>,
    permissions_path: impl AsRef<Path>,
    store_paths: helix_store::StorePaths,
) -> anyhow::Result<()> {
    let mut store = helix_store::Store::open(store_paths)?;
    if store.threads().has_assistant_import_marker()? {
        return Ok(());
    }

    let threads = legacy_threads_from_root(history_root)?;
    let layouts = crate::assistant::layout::legacy_layouts_from_path(layout_path)?;
    let permissions = crate::assistant::permission::legacy_permissions_from_path(permissions_path)?;
    store
        .threads()
        .import_assistant_state_once(threads, layouts, permissions)?;
    Ok(())
}

fn legacy_threads_from_root(
    root: impl AsRef<Path>,
) -> anyhow::Result<Vec<helix_store::AssistantThread>> {
    let root = root.as_ref();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(err) => {
                log::warn!("assistant history directory entry read failed: {err}");
                continue;
            }
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                log::warn!("assistant history read failed {:?}: {}", path, err);
                continue;
            }
        };
        match serde_json::from_str::<PersistedThread>(&raw).and_then(persisted_thread_into_store) {
            Ok(thread) => out.push(thread),
            Err(err) => log::warn!("assistant history decode failed {:?}: {}", path, err),
        }
    }
    Ok(out)
}

fn persisted_thread_from_store(
    row: helix_store::AssistantThread,
) -> anyhow::Result<PersistedThread> {
    Ok(serde_json::from_str(&row.record_json)?)
}

fn persisted_thread_into_store(
    thread: PersistedThread,
) -> Result<helix_store::AssistantThread, serde_json::Error> {
    let id = thread.stub.id;
    let feedback = thread.stub.feedback.clone();
    let scope = serde_json::to_string(&thread.stub.scope)?;
    Ok(helix_store::AssistantThread {
        id: id.to_string(),
        scope,
        title: thread.stub.title.clone(),
        created_at: timestamp_from_id(id),
        updated_at: timestamp_from_id(id),
        rating: rating_column(feedback.rating).map(ToOwned::to_owned),
        has_feedback: feedback.rating != PersistedRating::None || feedback.note.is_some(),
        record_json: serde_json::to_string_pretty(&thread)?,
    })
}

fn store_thread_from_record(record: &Record) -> anyhow::Result<helix_store::AssistantThread> {
    let thread = PersistedThread::from_domain(record)?;
    Ok(persisted_thread_into_store(thread)?)
}

fn rating_column(rating: PersistedRating) -> Option<&'static str> {
    match rating {
        PersistedRating::None => None,
        PersistedRating::Up => Some("up"),
        PersistedRating::Down => Some("down"),
    }
}

fn timestamp_from_id(id: u64) -> i64 {
    i64::try_from(id).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedThread {
    stub: PersistedStub,
    record: PersistedRecord,
}

impl PersistedThread {
    fn from_domain(record: &Record) -> anyhow::Result<Self> {
        Ok(Self {
            stub: PersistedStub::from_domain(&Stub {
                id: record.id,
                origin: Some(record.origin.clone()),
                title: record.title.clone(),
                scope: record.scope.clone(),
                unread: record.unread,
                run: record.run.clone(),
                feedback: record.feedback.clone(),
            }),
            record: PersistedRecord::from_domain(record)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedStub {
    id: u64,
    title: Option<String>,
    scope: PersistedScope,
    unread: bool,
    run: PersistedRun,
    #[serde(default)]
    feedback: PersistedFeedback,
}

impl PersistedStub {
    fn from_domain(stub: &Stub) -> Self {
        Self {
            id: stub.id.value().get(),
            title: stub.title.clone(),
            scope: PersistedScope::from(&stub.scope),
            unread: stub.unread,
            run: PersistedRun::from(&stub.run),
            feedback: PersistedFeedback::from_domain(&stub.feedback),
        }
    }

    fn into_domain(self) -> Stub {
        Stub {
            id: thread_id(self.id),
            origin: None,
            title: self.title,
            scope: self.scope.into_domain(),
            unread: self.unread,
            run: self.run.into_domain(),
            feedback: self.feedback.into_domain(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedRecord {
    id: u64,
    origin: PersistedOrigin,
    title: Option<String>,
    entries: Vec<PersistedEntry>,
    turns: Vec<PersistedTurn>,
    plan: Vec<PersistedPlanItem>,
    draft: String,
    context: Vec<PersistedContextItem>,
    follow: PersistedFollow,
    run: PersistedRun,
    unread: bool,
    mode: Option<PersistedModeSet>,
    config: PersistedConfigState,
    #[serde(default)]
    profile: Option<PersistedProfile>,
    #[serde(default)]
    feedback: PersistedFeedback,
    #[serde(default)]
    review_mode: review::Mode,
    scope: PersistedScope,
    view: PersistedView,
    terminals: Vec<PersistedTerminal>,
}

impl PersistedRecord {
    fn from_domain(record: &Record) -> anyhow::Result<Self> {
        Ok(Self {
            id: record.id.value().get(),
            origin: PersistedOrigin::from(&record.origin),
            title: record.title.clone(),
            entries: record
                .entries
                .iter()
                .map(PersistedEntry::from_domain)
                .collect(),
            turns: record
                .turns
                .iter()
                .map(PersistedTurn::from_domain)
                .collect(),
            plan: record
                .plan
                .iter()
                .map(PersistedPlanItem::from_domain)
                .collect(),
            draft: record.draft.clone(),
            context: record
                .context
                .iter()
                .map(PersistedContextItem::from_domain)
                .collect(),
            follow: PersistedFollow::from(&record.follow),
            run: PersistedRun::from(&record.run),
            unread: record.unread,
            mode: record.mode.as_ref().map(PersistedModeSet::from_domain),
            config: PersistedConfigState::from_domain(&record.config),
            profile: record.profile.as_ref().map(PersistedProfile::from_domain),
            feedback: PersistedFeedback::from_domain(&record.feedback),
            review_mode: record.review_mode,
            scope: PersistedScope::from(&record.scope),
            view: PersistedView::from_domain(&record.view),
            terminals: record
                .terminals
                .iter()
                .map(PersistedTerminal::from_domain)
                .collect(),
        })
    }

    fn into_domain(self) -> anyhow::Result<Record> {
        Ok(Record {
            id: thread_id(self.id),
            origin: self.origin.into_domain(),
            title: self.title,
            entries: self
                .entries
                .into_iter()
                .map(PersistedEntry::into_domain)
                .collect(),
            turns: self
                .turns
                .into_iter()
                .map(PersistedTurn::into_domain)
                .collect(),
            plan: self
                .plan
                .into_iter()
                .map(PersistedPlanItem::into_domain)
                .collect(),
            draft: self.draft,
            context: self
                .context
                .into_iter()
                .map(PersistedContextItem::into_domain)
                .collect(),
            follow: self.follow.into_domain(),
            run: self.run.into_domain(),
            unread: self.unread,
            mode: self.mode.map(PersistedModeSet::into_domain).transpose()?,
            config: self.config.into_domain()?,
            profile: self.profile.map(PersistedProfile::into_domain),
            feedback: self.feedback.into_domain(),
            review_mode: self.review_mode,
            usage: thread::Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
            scope: self.scope.into_domain(),
            view: self.view.into_domain(),
            terminals: self
                .terminals
                .into_iter()
                .map(PersistedTerminal::into_domain)
                .collect(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedView {
    focus: PersistedFocus,
    selected: Option<u64>,
    folded: Vec<u64>,
    content_scroll: usize,
}

impl PersistedView {
    fn from_domain(view: &View) -> Self {
        Self {
            focus: PersistedFocus::from(view.focus),
            selected: view.selected.map(|id| id.value().get()),
            folded: view.folded.iter().map(|id| id.value().get()).collect(),
            content_scroll: view.content_scroll,
        }
    }

    fn into_domain(self) -> View {
        View {
            focus: self.focus.into_domain(),
            selected: self.selected.map(entry_id),
            folded: self.folded.into_iter().map(entry_id).collect(),
            content_scroll: self.content_scroll,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedFocus {
    Input,
    Messages,
}

impl From<thread::Focus> for PersistedFocus {
    fn from(focus: thread::Focus) -> Self {
        match focus {
            thread::Focus::Input => Self::Input,
            thread::Focus::Messages => Self::Messages,
        }
    }
}

impl PersistedFocus {
    fn into_domain(self) -> thread::Focus {
        match self {
            Self::Input => thread::Focus::Input,
            Self::Messages => thread::Focus::Messages,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedTerminal {
    id: String,
    title: Option<String>,
    state: PersistedTerminalState,
    output: String,
}

impl PersistedTerminal {
    fn from_domain(terminal: &terminal::Terminal) -> Self {
        Self {
            id: terminal.id.to_string(),
            title: terminal.title.clone(),
            state: PersistedTerminalState::from(&terminal.state),
            output: terminal.output.clone(),
        }
    }

    fn into_domain(self) -> terminal::Terminal {
        terminal::Terminal {
            id: terminal::Id::new(self.id),
            title: self.title,
            state: self.state.into_domain(),
            output: self.output,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedTerminalState {
    Running,
    Exited { code: i32 },
    Failed { message: String },
}

impl From<&terminal::State> for PersistedTerminalState {
    fn from(state: &terminal::State) -> Self {
        match state {
            terminal::State::Running => Self::Running,
            terminal::State::Exited { code } => Self::Exited { code: *code },
            terminal::State::Failed { message } => Self::Failed {
                message: message.clone(),
            },
        }
    }
}

impl PersistedTerminalState {
    fn into_domain(self) -> terminal::State {
        match self {
            Self::Running => terminal::State::Running,
            Self::Exited { code } => terminal::State::Exited { code },
            Self::Failed { message } => terminal::State::Failed { message },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedScope {
    cwd: PathBuf,
    worktrees: Vec<PathBuf>,
}

impl From<&thread::Scope> for PersistedScope {
    fn from(scope: &thread::Scope) -> Self {
        Self {
            cwd: scope.cwd.clone(),
            worktrees: scope.worktrees.clone(),
        }
    }
}

impl PersistedScope {
    fn into_domain(self) -> thread::Scope {
        thread::Scope {
            cwd: self.cwd,
            worktrees: self.worktrees,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedOrigin {
    Backend { backend: String, remote: String },
    Local,
}

impl From<&thread::Origin> for PersistedOrigin {
    fn from(origin: &thread::Origin) -> Self {
        match origin {
            thread::Origin::Backend { backend, remote } => Self::Backend {
                backend: backend.to_string(),
                remote: remote.to_string(),
            },
            thread::Origin::Local => Self::Local,
        }
    }
}

impl PersistedOrigin {
    fn into_domain(self) -> thread::Origin {
        match self {
            Self::Backend { backend, remote } => thread::Origin::Backend {
                backend: crate::assistant::backend::Id::new(backend),
                remote: crate::assistant::backend::Remote::new(remote),
            },
            Self::Local => thread::Origin::Local,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedRun {
    Idle,
    Running,
    Waiting,
    Failed { message: String },
}

impl From<&thread::Run> for PersistedRun {
    fn from(run: &thread::Run) -> Self {
        match run {
            thread::Run::Idle => Self::Idle,
            thread::Run::Running => Self::Running,
            thread::Run::Waiting => Self::Waiting,
            thread::Run::Failed { message } => Self::Failed {
                message: message.clone(),
            },
        }
    }
}

impl PersistedRun {
    fn into_domain(self) -> thread::Run {
        match self {
            Self::Idle => thread::Run::Idle,
            Self::Running => thread::Run::Running,
            Self::Waiting => thread::Run::Waiting,
            Self::Failed { message } => thread::Run::Failed { message },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedEntry {
    id: u64,
    turn: Option<u64>,
    kind: PersistedEntryKind,
    locations: Vec<PersistedLocation>,
}

impl PersistedEntry {
    fn from_domain(entry: &thread::Entry) -> Self {
        Self {
            id: entry.id.value().get(),
            turn: entry.turn.map(|turn| turn.value().get()),
            kind: PersistedEntryKind::from(&entry.kind),
            locations: entry
                .locations
                .iter()
                .map(PersistedLocation::from)
                .collect(),
        }
    }

    fn into_domain(self) -> thread::Entry {
        thread::Entry {
            id: entry_id(self.id),
            turn: self.turn.map(turn_id),
            kind: self.kind.into_domain(),
            locations: self
                .locations
                .into_iter()
                .map(PersistedLocation::into_domain)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedEntryKind {
    UserPrompt { text: String },
    AssistantText { text: String },
    Thought { text: String },
    ToolCall(PersistedToolCall),
    Status { text: String },
    ChangeSummary { files: Vec<PersistedChangeFile> },
}

impl From<&thread::EntryKind> for PersistedEntryKind {
    fn from(kind: &thread::EntryKind) -> Self {
        match kind {
            thread::EntryKind::UserPrompt { text } => Self::UserPrompt { text: text.clone() },
            thread::EntryKind::AssistantText { text } => Self::AssistantText { text: text.clone() },
            thread::EntryKind::Thought { text } => Self::Thought { text: text.clone() },
            thread::EntryKind::ToolCall(call) => Self::ToolCall(PersistedToolCall::from(call)),
            thread::EntryKind::Status { text } => Self::Status { text: text.clone() },
            thread::EntryKind::ChangeSummary(summary) => Self::ChangeSummary {
                files: summary
                    .files
                    .iter()
                    .map(PersistedChangeFile::from)
                    .collect(),
            },
        }
    }
}

impl PersistedEntryKind {
    fn into_domain(self) -> thread::EntryKind {
        match self {
            Self::UserPrompt { text } => thread::EntryKind::UserPrompt { text },
            Self::AssistantText { text } => thread::EntryKind::AssistantText { text },
            Self::Thought { text } => thread::EntryKind::Thought { text },
            Self::ToolCall(call) => thread::EntryKind::ToolCall(call.into_domain()),
            Self::Status { text } => thread::EntryKind::Status { text },
            Self::ChangeSummary { files } => thread::EntryKind::ChangeSummary(change::Summary {
                files: files
                    .into_iter()
                    .map(PersistedChangeFile::into_domain)
                    .collect(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedToolCall {
    id: String,
    name: String,
    state: PersistedToolState,
    #[serde(default)]
    output: String,
}

impl From<&tool::Call> for PersistedToolCall {
    fn from(call: &tool::Call) -> Self {
        Self {
            id: call.id.to_string(),
            name: call.name.clone(),
            state: PersistedToolState::from(&call.state),
            output: call.output.clone(),
        }
    }
}

impl PersistedToolCall {
    fn into_domain(self) -> tool::Call {
        tool::Call {
            id: tool::Id::new(self.id),
            name: self.name,
            state: self.state.into_domain(),
            output: self.output,
            subagent: None,
            sandbox: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedProfile {
    name: String,
    mode: Option<String>,
    config: Vec<PersistedProfileConfig>,
}

impl PersistedProfile {
    fn from_domain(profile: &profile::Defaults) -> Self {
        Self {
            name: profile.name.clone(),
            mode: profile.mode.as_ref().map(ToString::to_string),
            config: profile
                .config
                .iter()
                .map(|(option, value)| PersistedProfileConfig {
                    option: option.to_string(),
                    value: value.to_string(),
                })
                .collect(),
        }
    }

    fn into_domain(self) -> profile::Defaults {
        profile::Defaults {
            name: self.name,
            mode: self.mode.map(mode::Id::new),
            config: self
                .config
                .into_iter()
                .map(|item| {
                    (
                        config::Id::new(item.option),
                        config::ValueId::new(item.value),
                    )
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedProfileConfig {
    option: String,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
struct PersistedFeedback {
    #[serde(default)]
    rating: PersistedRating,
    #[serde(default)]
    note: Option<String>,
}

impl PersistedFeedback {
    fn from_domain(feedback: &thread::Feedback) -> Self {
        Self {
            rating: PersistedRating::from(feedback.rating),
            note: feedback.note.clone(),
        }
    }

    fn into_domain(self) -> thread::Feedback {
        thread::Feedback {
            rating: self.rating.into_domain(),
            note: self.note,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
enum PersistedRating {
    #[default]
    None,
    Up,
    Down,
}

impl From<thread::Rating> for PersistedRating {
    fn from(rating: thread::Rating) -> Self {
        match rating {
            thread::Rating::None => Self::None,
            thread::Rating::Up => Self::Up,
            thread::Rating::Down => Self::Down,
        }
    }
}

impl PersistedRating {
    fn into_domain(self) -> thread::Rating {
        match self {
            Self::None => thread::Rating::None,
            Self::Up => thread::Rating::Up,
            Self::Down => thread::Rating::Down,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedToolState {
    Pending,
    Running,
    Completed,
    Failed { message: Option<String> },
    Canceled,
    Unknown(String),
}

impl From<&tool::State> for PersistedToolState {
    fn from(state: &tool::State) -> Self {
        match state {
            tool::State::Pending => Self::Pending,
            tool::State::Running => Self::Running,
            tool::State::Completed => Self::Completed,
            tool::State::Failed { message } => Self::Failed {
                message: message.clone(),
            },
            tool::State::Canceled => Self::Canceled,
            tool::State::Unknown(value) => Self::Unknown(value.to_string()),
        }
    }
}

impl PersistedToolState {
    fn into_domain(self) -> tool::State {
        match self {
            Self::Pending => tool::State::Pending,
            Self::Running => tool::State::Running,
            Self::Completed => tool::State::Completed,
            Self::Failed { message } => tool::State::Failed { message },
            Self::Canceled => tool::State::Canceled,
            Self::Unknown(value) => tool::State::Unknown(value.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedChangeFile {
    path: PathBuf,
    hunks: Vec<PersistedHunk>,
    #[serde(default)]
    review: Option<PersistedReviewFile>,
}

impl From<&change::File> for PersistedChangeFile {
    fn from(file: &change::File) -> Self {
        Self {
            path: file.path.clone(),
            hunks: file.hunks.iter().map(PersistedHunk::from).collect(),
            review: file.review.as_ref().map(PersistedReviewFile::from),
        }
    }
}

impl PersistedChangeFile {
    fn into_domain(self) -> change::File {
        change::File {
            path: self.path,
            hunks: self
                .hunks
                .into_iter()
                .map(PersistedHunk::into_domain)
                .collect(),
            review: self.review.map(PersistedReviewFile::into_domain),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedReviewFile {
    path: PathBuf,
    before: String,
    after: String,
    diff: String,
    status: review::Status,
}

impl From<&review::File> for PersistedReviewFile {
    fn from(file: &review::File) -> Self {
        Self {
            path: file.path.clone(),
            before: file.before.clone(),
            after: file.after.clone(),
            diff: file.diff.clone(),
            status: file.status,
        }
    }
}

impl PersistedReviewFile {
    fn into_domain(self) -> review::File {
        review::File {
            path: self.path,
            before: self.before,
            after: self.after,
            diff: self.diff,
            status: self.status,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedHunk {
    range: Option<PersistedLocation>,
    summary: String,
}

impl From<&change::Hunk> for PersistedHunk {
    fn from(hunk: &change::Hunk) -> Self {
        Self {
            range: hunk.range.as_ref().map(PersistedLocation::from),
            summary: hunk.summary.clone(),
        }
    }
}

impl PersistedHunk {
    fn into_domain(self) -> change::Hunk {
        change::Hunk {
            range: self.range.map(PersistedLocation::into_domain),
            summary: self.summary,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedLocation {
    path: PathBuf,
    #[serde(default)]
    range: Option<collab::RangeAnchor>,
    #[serde(default = "default_location_source")]
    source: location::Source,
    #[serde(default)]
    surface: Option<u64>,
    #[serde(default)]
    entry: Option<u64>,
}

impl From<&collab::Location> for PersistedLocation {
    fn from(location: &collab::Location) -> Self {
        Self {
            path: location.path.clone(),
            range: location.range,
            source: location.source,
            surface: location.surface.map(|id| id.value().get()),
            entry: location.entry.map(|id| id.value().get()),
        }
    }
}

impl PersistedLocation {
    fn into_domain(self) -> collab::Location {
        let mut location = collab::Location::new(self.path, self.source);
        location.range = self.range;
        location.surface = self.surface.map(surface_id);
        location.entry = self.entry.map(entry_id);
        location
    }
}

fn default_location_source() -> location::Source {
    location::Source::Tool
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedTurn {
    id: u64,
    prompt: u64,
    entries: Vec<u64>,
    changes: Vec<u64>,
}

impl PersistedTurn {
    fn from_domain(turn: &thread::Turn) -> Self {
        Self {
            id: turn.id.value().get(),
            prompt: turn.prompt.value().get(),
            entries: turn
                .entries
                .iter()
                .map(|entry| entry.value().get())
                .collect(),
            changes: Vec::new(),
        }
    }

    fn into_domain(self) -> thread::Turn {
        thread::Turn {
            id: turn_id(self.id),
            prompt: entry_id(self.prompt),
            entries: self.entries.into_iter().map(entry_id).collect(),
            changes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedPlanItem {
    content: String,
    status: PersistedPlanStatus,
}

impl PersistedPlanItem {
    fn from_domain(item: &plan::Item) -> Self {
        Self {
            content: item.content.clone(),
            status: PersistedPlanStatus::from(item.status),
        }
    }

    fn into_domain(self) -> plan::Item {
        plan::Item {
            content: self.content,
            status: self.status.into_domain(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedPlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl From<plan::Status> for PersistedPlanStatus {
    fn from(status: plan::Status) -> Self {
        match status {
            plan::Status::Pending => Self::Pending,
            plan::Status::InProgress => Self::InProgress,
            plan::Status::Completed => Self::Completed,
            plan::Status::Failed => Self::Failed,
        }
    }
}

impl PersistedPlanStatus {
    fn into_domain(self) -> plan::Status {
        match self {
            Self::Pending => plan::Status::Pending,
            Self::InProgress => plan::Status::InProgress,
            Self::Completed => plan::Status::Completed,
            Self::Failed => plan::Status::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedContextItem {
    id: String,
    kind: PersistedContextKind,
}

impl PersistedContextItem {
    fn from_domain(item: &context::Item) -> Self {
        Self {
            id: item.id.to_string(),
            kind: PersistedContextKind::from(&item.kind),
        }
    }

    fn into_domain(self) -> context::Item {
        context::Item::new(context::Id::new(self.id), self.kind.into_domain())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedContextKind {
    Selection {
        path: PathBuf,
        range: Option<PersistedLocation>,
        text: String,
        label: Option<String>,
    },
    Symbol {
        path: PathBuf,
        name: String,
        kind: String,
        range: Option<PersistedLocation>,
        text: String,
        breadcrumb: Vec<String>,
    },
    File {
        path: PathBuf,
    },
    Diagnostics {
        path: PathBuf,
        items: Vec<String>,
    },
    Diff {
        path: PathBuf,
        summary: String,
    },
}

impl From<&context::Kind> for PersistedContextKind {
    fn from(kind: &context::Kind) -> Self {
        match kind {
            context::Kind::Selection(selection) => Self::Selection {
                path: selection.path.clone(),
                range: selection.range.as_ref().map(PersistedLocation::from),
                text: selection.text.clone(),
                label: selection.label.clone(),
            },
            context::Kind::Symbol(symbol) => Self::Symbol {
                path: symbol.path.clone(),
                name: symbol.name.clone(),
                kind: symbol.kind.to_string(),
                range: symbol.range.as_ref().map(PersistedLocation::from),
                text: symbol.text.clone(),
                breadcrumb: symbol.breadcrumb.clone(),
            },
            context::Kind::File(file) => Self::File {
                path: file.path.clone(),
            },
            context::Kind::Diagnostics(diag) => Self::Diagnostics {
                path: diag.path.clone(),
                items: diag.items.clone(),
            },
            context::Kind::Diff(diff) => Self::Diff {
                path: diff.path.clone(),
                summary: diff.summary.clone(),
            },
        }
    }
}

impl PersistedContextKind {
    fn into_domain(self) -> context::Kind {
        match self {
            Self::Selection {
                path,
                range,
                text,
                label,
            } => context::Kind::Selection(context::Selection {
                path,
                range: range.map(PersistedLocation::into_domain),
                text,
                label,
            }),
            Self::Symbol {
                path,
                name,
                kind,
                range,
                text,
                breadcrumb,
            } => context::Kind::Symbol(context::Symbol {
                path,
                name,
                kind: kind.into(),
                range: range.map(PersistedLocation::into_domain),
                text,
                breadcrumb,
            }),
            Self::File { path } => context::Kind::File(context::File { path }),
            Self::Diagnostics { path, items } => {
                context::Kind::Diagnostics(context::Diagnostics { path, items })
            }
            Self::Diff { path, summary } => context::Kind::Diff(context::Diff { path, summary }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedFollow {
    Off,
    On {
        participant: u64,
        last: Option<PersistedLocation>,
    },
    Paused {
        participant: u64,
        last: Option<PersistedLocation>,
        reason: PersistedPause,
    },
}

impl From<&collab::FollowState> for PersistedFollow {
    fn from(state: &collab::FollowState) -> Self {
        match state {
            collab::FollowState::Off => Self::Off,
            collab::FollowState::On {
                participant, last, ..
            } => Self::On {
                participant: participant.value().get(),
                last: last.as_ref().map(PersistedLocation::from),
            },
            collab::FollowState::Paused {
                participant,
                last,
                reason,
                ..
            } => Self::Paused {
                participant: participant.value().get(),
                last: last.as_ref().map(PersistedLocation::from),
                reason: PersistedPause::from(*reason),
            },
        }
    }
}

impl PersistedFollow {
    fn into_domain(self) -> collab::FollowState {
        match self {
            Self::Off => collab::FollowState::Off,
            Self::On { participant, last } => collab::FollowState::On {
                mode: collab::follow::Mode::AutoSwitchAndReveal,
                participant: participant_id(participant),
                last: last.map(PersistedLocation::into_domain),
            },
            Self::Paused {
                participant,
                last,
                reason,
            } => collab::FollowState::Paused {
                mode: collab::follow::Mode::AutoSwitchAndReveal,
                participant: participant_id(participant),
                last: last.map(PersistedLocation::into_domain),
                reason: reason.into_domain(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedPause {
    LocalMove,
    LocalScroll,
    LocalEdit,
    BufferSwitch,
    Explicit,
}

impl From<collab::follow::Pause> for PersistedPause {
    fn from(reason: collab::follow::Pause) -> Self {
        match reason {
            collab::follow::Pause::LocalMove => Self::LocalMove,
            collab::follow::Pause::LocalScroll => Self::LocalScroll,
            collab::follow::Pause::LocalEdit => Self::LocalEdit,
            collab::follow::Pause::BufferSwitch => Self::BufferSwitch,
            collab::follow::Pause::Explicit => Self::Explicit,
        }
    }
}

impl PersistedPause {
    fn into_domain(self) -> collab::follow::Pause {
        match self {
            Self::LocalMove => collab::follow::Pause::LocalMove,
            Self::LocalScroll => collab::follow::Pause::LocalScroll,
            Self::LocalEdit => collab::follow::Pause::LocalEdit,
            Self::BufferSwitch => collab::follow::Pause::BufferSwitch,
            Self::Explicit => collab::follow::Pause::Explicit,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedModeSet {
    items: Vec<PersistedModeItem>,
    selected: PersistedSelected,
}

impl PersistedModeSet {
    fn from_domain(set: &mode::Set) -> Self {
        Self {
            items: set.items().map(PersistedModeItem::from_domain).collect(),
            selected: PersistedSelected::from_mode(set.selected()),
        }
    }

    fn into_domain(self) -> anyhow::Result<mode::Set> {
        mode::Set::new(
            self.items
                .into_iter()
                .map(PersistedModeItem::into_domain)
                .collect(),
            self.selected.into_mode(),
        )
        .map_err(Into::into)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedModeItem {
    id: String,
    name: String,
    description: Option<String>,
}

impl PersistedModeItem {
    fn from_domain(item: &mode::Item) -> Self {
        Self {
            id: item.id.to_string(),
            name: item.name.clone(),
            description: item.description.clone(),
        }
    }

    fn into_domain(self) -> mode::Item {
        mode::Item {
            id: mode::Id::new(self.id),
            name: self.name,
            description: self.description,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedConfigState {
    items: Vec<PersistedConfigItem>,
}

impl PersistedConfigState {
    fn from_domain(state: &config::State) -> Self {
        Self {
            items: state
                .items()
                .map(PersistedConfigItem::from_domain)
                .collect(),
        }
    }

    fn into_domain(self) -> anyhow::Result<config::State> {
        let mut items = Vec::with_capacity(self.items.len());
        for item in self.items {
            items.push(item.into_domain()?);
        }
        Ok(config::State::new(items))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedConfigItem {
    id: String,
    name: String,
    category: Option<String>,
    selected: PersistedSelected,
    values: Vec<PersistedConfigValue>,
}

impl PersistedConfigItem {
    fn from_domain(item: &config::Item) -> Self {
        Self {
            id: item.id.to_string(),
            name: item.name.clone(),
            category: item.category.clone(),
            selected: PersistedSelected::from_config(&item.selected),
            values: item
                .values
                .iter()
                .map(PersistedConfigValue::from_domain)
                .collect(),
        }
    }

    fn into_domain(self) -> anyhow::Result<config::Item> {
        config::Item::new(
            config::Id::new(self.id),
            self.name,
            self.category,
            self.selected.into_config(),
            self.values
                .into_iter()
                .map(PersistedConfigValue::into_domain)
                .collect(),
        )
        .map_err(Into::into)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedConfigValue {
    id: String,
    label: String,
    description: Option<String>,
}

impl PersistedConfigValue {
    fn from_domain(value: &config::Value) -> Self {
        Self {
            id: value.id.to_string(),
            label: value.label.clone(),
            description: value.description.clone(),
        }
    }

    fn into_domain(self) -> config::Value {
        config::Value {
            id: config::ValueId::new(self.id),
            label: self.label,
            description: self.description,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum PersistedSelected {
    Current(String),
    Pending { current: String, next: String },
}

impl PersistedSelected {
    fn from_mode(selected: &mode::Selected) -> Self {
        match selected {
            mode::Selected::Current(id) => Self::Current(id.to_string()),
            mode::Selected::Pending { current, next } => Self::Pending {
                current: current.to_string(),
                next: next.to_string(),
            },
        }
    }

    fn into_mode(self) -> mode::Selected {
        match self {
            Self::Current(id) => mode::Selected::Current(mode::Id::new(id)),
            Self::Pending { current, next } => mode::Selected::Pending {
                current: mode::Id::new(current),
                next: mode::Id::new(next),
            },
        }
    }

    fn from_config(selected: &config::Selected) -> Self {
        match selected {
            config::Selected::Current(id) => Self::Current(id.to_string()),
            config::Selected::Pending { current, next } => Self::Pending {
                current: current.to_string(),
                next: next.to_string(),
            },
        }
    }

    fn into_config(self) -> config::Selected {
        match self {
            Self::Current(id) => config::Selected::Current(config::ValueId::new(id)),
            Self::Pending { current, next } => config::Selected::Pending {
                current: config::ValueId::new(current),
                next: config::ValueId::new(next),
            },
        }
    }
}

fn thread_id(raw: u64) -> thread::Id {
    thread::Id::new(NonZeroU64::new(raw).expect("thread id"))
}

fn entry_id(raw: u64) -> thread::EntryId {
    thread::EntryId::new(NonZeroU64::new(raw).expect("entry id"))
}

fn turn_id(raw: u64) -> thread::TurnId {
    thread::TurnId::new(NonZeroU64::new(raw).expect("turn id"))
}

fn participant_id(raw: u64) -> collab::ParticipantId {
    collab::ParticipantId::new(NonZeroU64::new(raw).expect("participant id"))
}

fn surface_id(raw: u64) -> collab::SurfaceId {
    collab::SurfaceId::new(NonZeroU64::new(raw).expect("surface id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> Record {
        let id = thread::Id::new(NonZeroU64::new(1).unwrap());
        Record {
            id,
            origin: thread::Origin::Backend {
                backend: crate::assistant::backend::Id::new("acp:test"),
                remote: crate::assistant::backend::Remote::new("session-1"),
            },
            title: Some("Thread".to_string()),
            entries: vec![thread::Entry {
                id: thread::EntryId::new(NonZeroU64::new(1).unwrap()),
                turn: None,
                kind: thread::EntryKind::AssistantText {
                    text: "hello".to_string(),
                },
                locations: vec![collab::Location {
                    path: PathBuf::from("src/main.rs"),
                    range: None,
                    source: location::Source::Tool,
                    surface: None,
                    entry: None,
                }],
            }],
            turns: Vec::new(),
            plan: vec![plan::Item {
                content: "do thing".to_string(),
                status: plan::Status::Pending,
            }],
            draft: "draft".to_string(),
            context: vec![context::Item::new(
                context::Id::new("ctx-1"),
                context::Kind::File(context::File {
                    path: PathBuf::from("Cargo.toml"),
                }),
            )],
            follow: collab::FollowState::Off,
            run: thread::Run::Idle,
            unread: false,
            mode: None,
            config: config::State::new(Vec::new()),
            review_mode: review::Mode::Write,
            usage: thread::Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
            profile: Some(profile::Defaults {
                name: "review".to_string(),
                mode: Some(mode::Id::new("review")),
                config: vec![(config::Id::new("thinking"), config::ValueId::new("high"))],
            }),
            feedback: thread::Feedback {
                rating: thread::Rating::Up,
                note: Some("useful review".to_string()),
            },
            scope: thread::Scope::new(PathBuf::from(".")),
            view: View {
                focus: thread::Focus::Messages,
                selected: Some(thread::EntryId::new(NonZeroU64::new(1).unwrap())),
                folded: Vec::new(),
                content_scroll: 3,
            },
            terminals: vec![terminal::Terminal {
                id: terminal::Id::new("term-1"),
                title: Some("cargo test".to_string()),
                state: terminal::State::Exited { code: 0 },
                output: "ok".to_string(),
            }],
        }
    }

    fn store_paths(root: &Path) -> helix_store::StorePaths {
        helix_store::StorePaths::new(root.join("state.sqlite3"), root.join("cache.sqlite3"))
    }

    fn write_record(path: &Path, record: &Record) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let persisted = PersistedThread::from_domain(record).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();
    }

    #[test]
    fn local_history_round_trips_record_and_scope_listing() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let backend = LocalHistory {
                root: dir.path().join("assistant-history"),
                store_paths: Some(store_paths(dir.path())),
            };
            let record = sample_record();

            backend.save(record.clone()).await.unwrap();

            let listed = backend.load_scope(&record.scope).await.unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, record.id);
            assert_eq!(listed[0].title, record.title);
            assert_eq!(listed[0].feedback, record.feedback);

            let loaded = backend.load(record.id).await.unwrap().unwrap();
            assert_eq!(loaded, record);
        });
    }

    #[test]
    fn store_round_trip_populates_indexed_feedback_columns() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let backend = LocalHistory {
                root: dir.path().join("assistant-history"),
                store_paths: Some(store_paths(dir.path())),
            };
            let record = sample_record();

            backend.save(record.clone()).await.unwrap();

            let mut store = helix_store::Store::open(store_paths(dir.path())).unwrap();
            let scope = crate::assistant::layout::scope_key(&record.scope).unwrap();
            let rows = store.threads().list_by_scope(&scope).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, record.id.value().get().to_string());
            assert_eq!(rows[0].title, record.title);
            assert_eq!(rows[0].rating.as_deref(), Some("up"));
            assert!(rows[0].has_feedback);

            let up = store
                .threads()
                .list_by_scope_filtered(&scope, Some("up"), Some(true))
                .unwrap();
            let down = store
                .threads()
                .list_by_scope_filtered(&scope, Some("down"), Some(true))
                .unwrap();
            assert_eq!(up.len(), 1);
            assert!(down.is_empty());
        });
    }

    #[test]
    fn one_time_import_preserves_legacy_files_and_sets_marker() {
        let dir = tempfile::tempdir().unwrap();
        let history_root = dir.path().join("assistant").join("history");
        let layout_path = dir.path().join("assistant").join("layout.json");
        let permissions_path = dir.path().join("assistant").join("permissions.toml");
        let record = sample_record();
        let mut other = sample_record();
        other.id = thread::Id::new(NonZeroU64::new(2).unwrap());
        other.scope = thread::Scope::new(PathBuf::from("other"));

        write_record(&history_root.join("1.json"), &record);
        write_record(&history_root.join("2.json"), &other);
        std::fs::write(
            &layout_path,
            br#"{"scopes":[{"scope":{"cwd":".","worktrees":[]},"open":[1],"active":1}]}"#,
        )
        .unwrap();
        std::fs::write(
            &permissions_path,
            r#"[[rules]]
agent = "agent"
tool = "shell"
choice = "allow-always"
"#,
        )
        .unwrap();

        let paths = store_paths(dir.path());
        import_legacy_from_paths(
            &history_root,
            &layout_path,
            &permissions_path,
            paths.clone(),
        )
        .unwrap();

        let mut store = helix_store::Store::open(paths.clone()).unwrap();
        assert!(store.threads().has_assistant_import_marker().unwrap());
        let scope = crate::assistant::layout::scope_key(&record.scope).unwrap();
        assert_eq!(store.threads().list_by_scope(&scope).unwrap().len(), 1);
        assert!(store.layout().get(&scope).unwrap().is_some());
        assert_eq!(store.permissions().all().unwrap().len(), 1);
        assert!(history_root.join("1.json").exists());
        assert!(layout_path.exists());
        assert!(permissions_path.exists());

        let mut later = sample_record();
        later.id = thread::Id::new(NonZeroU64::new(3).unwrap());
        write_record(&history_root.join("3.json"), &later);
        import_legacy_from_paths(
            &history_root,
            &layout_path,
            &permissions_path,
            paths.clone(),
        )
        .unwrap();
        let mut store = helix_store::Store::open(paths).unwrap();
        assert_eq!(store.threads().list_by_scope(&scope).unwrap().len(), 1);
    }

    #[test]
    fn db_open_failure_falls_back_to_json_without_panic() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let bad_state = dir.path().join("state-as-directory");
            std::fs::create_dir_all(&bad_state).unwrap();
            let backend = LocalHistory {
                root: dir.path().join("assistant-history"),
                store_paths: Some(helix_store::StorePaths::new(
                    bad_state,
                    dir.path().join("cache.sqlite3"),
                )),
            };
            let record = sample_record();

            backend.save(record.clone()).await.unwrap();
            tokio::fs::write(backend.root.join("2.json"), b"{\"stub\":null} trailing")
                .await
                .unwrap();

            let listed = backend.load_scope(&record.scope).await.unwrap();
            let loaded = backend.load(record.id).await.unwrap().unwrap();

            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, record.id);
            assert_eq!(loaded, record);
        });
    }
}
