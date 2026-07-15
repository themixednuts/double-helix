use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::{Duration, Instant},
};

use crate::runtime::{
    ui::{PromptCommand, PromptCompletionResult},
    RuntimeIngress, UiCommand,
};
use crate::ui::prompt::Completion;

const FILE_INDEX_FRESHNESS: Duration = Duration::from_secs(1);
const FILE_INDEX_CAPACITY: usize = 8;
const THEME_INDEX_FRESHNESS: Duration = Duration::from_secs(2);
const PROGRAM_INDEX_FRESHNESS: Duration = Duration::from_secs(30);

static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);
static NAME_INDEX: OnceLock<Mutex<NameIndex>> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PromptId(u64);

impl PromptId {
    pub(crate) fn next() -> Self {
        Self(NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileIndexKey {
    pub(crate) directory: PathBuf,
    pub(crate) git_ignore: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FileIndexEntry {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) is_symlink: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CompletionWorkKey {
    Themes,
    Programs,
    Files(FileIndexKey),
    #[cfg(test)]
    Test(String),
}

#[derive(Clone)]
pub(crate) enum CompletionWorkOutput {
    Themes(Arc<[String]>),
    Programs {
        path: Option<OsString>,
        names: Arc<[String]>,
    },
    Files(Arc<[FileIndexEntry]>),
    #[cfg(test)]
    Test {
        key: String,
        values: Arc<[String]>,
    },
}

impl std::fmt::Debug for CompletionWorkOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Themes(names) => formatter.debug_tuple("Themes").field(&names.len()).finish(),
            Self::Programs { names, .. } => formatter
                .debug_tuple("Programs")
                .field(&names.len())
                .finish(),
            Self::Files(entries) => formatter
                .debug_tuple("Files")
                .field(&entries.len())
                .finish(),
            #[cfg(test)]
            Self::Test { key, values } => formatter
                .debug_struct("Test")
                .field("key", key)
                .field("values", &values.len())
                .finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PromptCompletionPayload {
    pub(crate) prompt_id: PromptId,
    pub(crate) generation: u64,
    pub(crate) query: Arc<str>,
    pub(crate) completions: Vec<Completion>,
}

pub struct CompletionRequest {
    evaluate: Box<dyn FnMut() -> Vec<Completion> + Send>,
}

impl CompletionRequest {
    pub fn new(evaluate: impl FnMut() -> Vec<Completion> + Send + 'static) -> Self {
        Self {
            evaluate: Box::new(evaluate),
        }
    }

    fn evaluate(&mut self) -> Vec<Completion> {
        (self.evaluate)()
    }
}

#[derive(Clone)]
struct EvaluationScope {
    cache: Arc<Mutex<CompletionCache>>,
    requests: Arc<Mutex<Vec<CompletionWorkKey>>>,
}

thread_local! {
    static EVALUATION_SCOPE: RefCell<Option<EvaluationScope>> = const { RefCell::new(None) };
}

struct ScopeGuard(Option<EvaluationScope>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        EVALUATION_SCOPE.with(|scope| {
            scope.replace(self.0.take());
        });
    }
}

#[derive(Clone)]
struct CachedFiles {
    entries: Arc<[FileIndexEntry]>,
    loaded_at: Instant,
    used_at: u64,
}

#[derive(Clone)]
struct CachedNames {
    names: Arc<[String]>,
    loaded_at: Instant,
}

#[derive(Clone)]
struct CachedPrograms {
    path: Option<OsString>,
    names: Arc<[String]>,
    loaded_at: Instant,
}

#[derive(Default)]
struct NameIndex {
    themes: Option<CachedNames>,
    programs: Option<CachedPrograms>,
}

fn name_index() -> &'static Mutex<NameIndex> {
    NAME_INDEX.get_or_init(|| Mutex::new(NameIndex::default()))
}

#[derive(Default)]
struct CompletionCache {
    files: HashMap<FileIndexKey, CachedFiles>,
    clock: u64,
    #[cfg(test)]
    tests: HashMap<String, Arc<[String]>>,
}

#[derive(Clone, Default)]
pub(crate) struct CompletionSession {
    cache: Arc<Mutex<CompletionCache>>,
}

impl CompletionSession {
    pub(crate) fn evaluate<T>(&self, evaluate: impl FnOnce() -> T) -> (T, Vec<CompletionWorkKey>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let scope = EvaluationScope {
            cache: self.cache.clone(),
            requests: requests.clone(),
        };
        let previous = EVALUATION_SCOPE.with(|current| current.replace(Some(scope)));
        let _guard = ScopeGuard(previous);
        let value = evaluate();
        let requests = std::mem::take(&mut *lock(&requests));
        (value, requests)
    }

    pub(crate) fn insert(&self, key: CompletionWorkKey, output: CompletionWorkOutput) {
        let mut cache = lock(&self.cache);
        match (key, output) {
            (CompletionWorkKey::Themes, CompletionWorkOutput::Themes(names)) => {
                lock(name_index()).themes = Some(CachedNames {
                    names,
                    loaded_at: Instant::now(),
                });
            }
            (CompletionWorkKey::Programs, CompletionWorkOutput::Programs { path, names }) => {
                lock(name_index()).programs = Some(CachedPrograms {
                    path,
                    names,
                    loaded_at: Instant::now(),
                });
            }
            (CompletionWorkKey::Files(key), CompletionWorkOutput::Files(entries)) => {
                cache.clock = cache.clock.wrapping_add(1);
                let used_at = cache.clock;
                cache.files.insert(
                    key,
                    CachedFiles {
                        entries,
                        loaded_at: Instant::now(),
                        used_at,
                    },
                );
                trim_file_cache(&mut cache);
            }
            #[cfg(test)]
            (CompletionWorkKey::Test(expected), CompletionWorkOutput::Test { key, values })
                if expected == key =>
            {
                cache.tests.insert(key, values);
            }
            (key, output) => {
                log::warn!(
                    "prompt_completion phase=discard_mismatched_output key={key:?} output={output:?}"
                );
            }
        }
    }
}

fn trim_file_cache(cache: &mut CompletionCache) {
    while cache.files.len() > FILE_INDEX_CAPACITY {
        let Some(oldest) = cache
            .files
            .iter()
            .min_by_key(|(_, cached)| cached.used_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        cache.files.remove(&oldest);
    }
}

fn with_scope<T>(f: impl FnOnce(&EvaluationScope) -> T) -> Option<T> {
    EVALUATION_SCOPE.with(|scope| scope.borrow().as_ref().map(f))
}

fn request(scope: &EvaluationScope, key: CompletionWorkKey) {
    let mut requests = lock(&scope.requests);
    if !requests.contains(&key) {
        requests.push(key);
    }
}

pub(crate) fn theme_names() -> Option<Arc<[String]>> {
    with_scope(|scope| {
        let now = Instant::now();
        let cached = lock(name_index()).themes.clone();
        if cached
            .as_ref()
            .is_none_or(|cached| now.duration_since(cached.loaded_at) >= THEME_INDEX_FRESHNESS)
        {
            request(scope, CompletionWorkKey::Themes);
        }
        cached.map(|cached| cached.names)
    })
    .flatten()
}

pub(crate) fn program_names() -> Option<Arc<[String]>> {
    with_scope(|scope| {
        let now = Instant::now();
        let path = std::env::var_os("PATH");
        let cached = lock(name_index()).programs.clone();
        if cached.as_ref().is_none_or(|cached| {
            cached.path != path || now.duration_since(cached.loaded_at) >= PROGRAM_INDEX_FRESHNESS
        }) {
            request(scope, CompletionWorkKey::Programs);
        }
        cached.map(|cached| cached.names)
    })
    .flatten()
}

pub(crate) fn file_entries(key: FileIndexKey) -> Option<Arc<[FileIndexEntry]>> {
    with_scope(|scope| {
        let now = Instant::now();
        let mut cache = lock(&scope.cache);
        cache.clock = cache.clock.wrapping_add(1);
        let used_at = cache.clock;
        let mut cached = cache.files.get_mut(&key);
        let entries = cached.as_mut().map(|cached| {
            cached.used_at = used_at;
            cached.entries.clone()
        });
        let needs_refresh = cached
            .as_ref()
            .is_none_or(|cached| now.duration_since(cached.loaded_at) >= FILE_INDEX_FRESHNESS);
        drop(cache);
        if needs_refresh {
            request(scope, CompletionWorkKey::Files(key));
        }
        entries
    })
    .flatten()
}

#[cfg(test)]
pub(crate) fn test_values(key: &str) -> Option<Arc<[String]>> {
    with_scope(|scope| {
        let values = lock(&scope.cache).tests.get(key).cloned();
        if values.is_none() {
            request(scope, CompletionWorkKey::Test(key.to_owned()));
        }
        values
    })
    .flatten()
}

struct CompletionJob {
    prompt_id: PromptId,
    generation: u64,
    query: Arc<str>,
    request: CompletionRequest,
    session: CompletionSession,
}

#[derive(Default)]
struct PipelineState {
    closed: bool,
    latest: Option<(PromptId, u64)>,
    pending: Option<CompletionJob>,
}

pub(crate) struct CompletionCancellation {
    state: Arc<Mutex<PipelineState>>,
    prompt_id: PromptId,
    generation: u64,
}

impl CompletionCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        let state = lock(&self.state);
        state.closed || state.latest != Some((self.prompt_id, self.generation))
    }
}

pub(crate) type CompletionLoader = Arc<
    dyn Fn(&CompletionWorkKey, &CompletionCancellation) -> Option<CompletionWorkOutput>
        + Send
        + Sync,
>;

pub(crate) struct CompletionPipeline {
    state: Arc<Mutex<PipelineState>>,
    wake: Option<tokio::sync::mpsc::Sender<()>>,
    loader: CompletionLoader,
}

impl Default for CompletionPipeline {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(PipelineState::default())),
            wake: None,
            loader: Arc::new(load_completion),
        }
    }
}

impl CompletionPipeline {
    pub(crate) fn submit(
        &mut self,
        prompt_id: PromptId,
        generation: u64,
        query: Arc<str>,
        request: CompletionRequest,
        session: CompletionSession,
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        ingress: RuntimeIngress,
    ) {
        if self.wake.is_none() {
            let (wake, receiver) = tokio::sync::mpsc::channel(1);
            self.wake = Some(wake);
            let state = self.state.clone();
            let loader = self.loader.clone();
            work.spawn(run_worker(state, receiver, loader, block, ingress))
                .detach();
        }

        let mut state = lock(&self.state);
        state.latest = Some((prompt_id, generation));
        state.pending = Some(CompletionJob {
            prompt_id,
            generation,
            query,
            request,
            session,
        });
        drop(state);

        if let Some(wake) = &self.wake {
            match wake.try_send(()) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                    self.wake = None;
                    log::warn!("prompt_completion phase=worker_closed");
                }
            }
        }
    }

    pub(crate) fn cancel(&mut self) {
        let mut state = lock(&self.state);
        state.latest = None;
        state.pending = None;
    }

    #[cfg(test)]
    pub(crate) fn set_loader(&mut self, loader: CompletionLoader) {
        assert!(
            self.wake.is_none(),
            "test loader must be set before first work"
        );
        self.loader = loader;
    }
}

impl Drop for CompletionPipeline {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.latest = None;
        state.pending = None;
    }
}

async fn run_worker(
    state: Arc<Mutex<PipelineState>>,
    mut wake: tokio::sync::mpsc::Receiver<()>,
    loader: CompletionLoader,
    block: helix_runtime::Block,
    ingress: RuntimeIngress,
) {
    while wake.recv().await.is_some() {
        loop {
            let Some(mut job) = lock(&state).pending.take() else {
                break;
            };
            let load_state = state.clone();
            let load = loader.clone();
            let prompt_id = job.prompt_id;
            let generation = job.generation;
            let started = Instant::now();
            let evaluated = block
                .spawn(move || {
                    let cancellation = CompletionCancellation {
                        state: load_state,
                        prompt_id,
                        generation,
                    };
                    loop {
                        if cancellation.is_cancelled() {
                            return None;
                        }
                        let (completions, mut keys) =
                            job.session.evaluate(|| job.request.evaluate());
                        keys.dedup();
                        if keys.is_empty() {
                            return Some((job, completions));
                        }
                        for key in keys {
                            if cancellation.is_cancelled() {
                                return None;
                            }
                            let output = load(&key, &cancellation)?;
                            job.session.insert(key, output);
                        }
                    }
                })
                .await;

            let (job, completions) = match evaluated {
                Ok(Some(result)) => result,
                Ok(None) => {
                    if lock(&state).pending.is_none() {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    log::warn!(
                        "prompt_completion phase=worker_join_error error={error} elapsed_us={}",
                        started.elapsed().as_micros()
                    );
                    continue;
                }
            };

            let dispatch = {
                let state = lock(&state);
                !state.closed && state.latest == Some((job.prompt_id, job.generation))
            };

            if dispatch {
                log::debug!(
                    "prompt_completion phase=evaluate_done prompt={:?} generation={} completions={} elapsed_us={}",
                    job.prompt_id,
                    job.generation,
                    completions.len(),
                    started.elapsed().as_micros()
                );
                let _ = ingress
                    .send_ui(UiCommand::Prompt(PromptCommand::CompletionReady(
                        PromptCompletionResult(PromptCompletionPayload {
                            prompt_id: job.prompt_id,
                            generation: job.generation,
                            query: job.query,
                            completions,
                        }),
                    )))
                    .await;
            }

            if lock(&state).pending.is_none() {
                break;
            }
        }
    }
}

fn load_completion(
    key: &CompletionWorkKey,
    cancellation: &CompletionCancellation,
) -> Option<CompletionWorkOutput> {
    match key {
        CompletionWorkKey::Themes => load_themes(cancellation).map(CompletionWorkOutput::Themes),
        CompletionWorkKey::Programs => load_programs(cancellation),
        CompletionWorkKey::Files(key) => {
            load_files(key, cancellation).map(CompletionWorkOutput::Files)
        }
        #[cfg(test)]
        CompletionWorkKey::Test(key) => Some(CompletionWorkOutput::Test {
            key: key.clone(),
            values: Arc::from([format!("{key}-result")]),
        }),
    }
}

fn load_themes(cancellation: &CompletionCancellation) -> Option<Arc<[String]>> {
    let loader = helix_view::theme::Loader::new(&[helix_loader::config_dir()]);
    let loader = match helix_loader::runtime_assets_if_initialized() {
        Some(runtime_assets) => loader.with_runtime_assets(runtime_assets.clone()),
        None => loader,
    };
    if cancellation.is_cancelled() {
        return None;
    }
    let names = loader.names().ok()?;
    Some(names.into())
}

fn load_programs(cancellation: &CompletionCancellation) -> Option<CompletionWorkOutput> {
    let path = std::env::var_os("PATH");
    let mut programs = std::collections::BTreeSet::new();
    if let Some(path) = path.as_ref() {
        for directory in std::env::split_paths(path) {
            if cancellation.is_cancelled() {
                return None;
            }
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries {
                if cancellation.is_cancelled() {
                    return None;
                }
                let Ok(entry) = entry else {
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_file() {
                    if let Ok(name) = entry.file_name().into_string() {
                        programs.insert(name);
                    }
                }
            }
        }
    }
    Some(CompletionWorkOutput::Programs {
        path,
        names: programs.into_iter().collect::<Vec<_>>().into(),
    })
}

fn load_files(
    key: &FileIndexKey,
    cancellation: &CompletionCancellation,
) -> Option<Arc<[FileIndexEntry]>> {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(&key.directory)
        .hidden(false)
        .follow_links(false)
        .git_ignore(key.git_ignore)
        .parents(false)
        .max_depth(Some(1))
        .build();

    for entry in walker {
        if cancellation.is_cancelled() {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let file_type = entry.file_type();
        let path = entry.into_path();
        let is_symlink = file_type.is_some_and(|file_type| file_type.is_symlink());
        let is_dir = file_type.map_or_else(
            || path.is_dir(),
            |file_type| {
                if file_type.is_symlink() {
                    path.is_dir()
                } else {
                    file_type.is_dir()
                }
            },
        );
        entries.push(FileIndexEntry {
            path,
            is_dir,
            is_symlink,
        });
    }
    Some(entries.into())
}
