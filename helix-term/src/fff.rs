use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use fff_search::{
    ContentOverlay, FFFMode, FilePicker, FilePickerOptions, FilePickerScanOptions,
    FileSearchConfig, FrecencyRecord, FrecencyStore, FrecencyTracker, FuzzySearchOptions,
    GrepConfig, GrepMode, GrepSearchOptions, PaginationArgs, QueryHistoryKind, QueryMatchEntry,
    QueryParser, QueryTracker, QueryTrackerStore, SharedFrecency, SharedPicker, SharedQueryTracker,
};
use heed::types::{Bytes, SerdeBincode};
use heed::{Database, EnvOpenOptions};
use helix_store::{CacheStore, FrecencyEntry, QueryHistory};
use helix_view::editor::{FileExplorerConfig, FilePickerConfig};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const FILE_SEARCH_LIMIT: usize = 1_000;
const SCAN_WAIT: Duration = Duration::from_millis(20);
const FIRST_RESULTS_WAIT: Duration = Duration::from_secs(2);
const INITIAL_SCAN_WAIT: Duration = Duration::from_secs(30);
const GREP_SEARCH_LIMIT: usize = 2_000;
const GREP_SCAN_WAIT: Duration = Duration::from_millis(250);
const PICKER_TRACE_TARGET: &str = crate::ui::picker::PICKER_TRACE_TARGET;
const FFF_CACHE_IMPORT_MARKER: &str = "fff-cache-v1";
const WORKSPACE_CACHE_CAPACITY: usize = 4;

static WORKSPACES: OnceLock<Mutex<FffWorkspaceCache>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct FileMatch {
    pub(crate) path: PathBuf,
    pub(crate) query: Arc<str>,
}

#[derive(Debug, Clone)]
pub(crate) struct GrepMatch {
    pub(crate) path: PathBuf,
    pub(crate) line_num: usize,
}

#[derive(Debug)]
struct FffWorkspace {
    root: PathBuf,
    picker: SharedPicker,
    frecency: SharedFrecency,
    query_tracker: SharedQueryTracker,
}

#[derive(Default)]
struct FffWorkspaceCache {
    slots: VecDeque<Arc<FffWorkspaceSlot>>,
}

struct FffWorkspaceSlot {
    root: PathBuf,
    config: FilePickerConfig,
    workspace: OnceCell<Arc<FffWorkspace>>,
}

impl FffWorkspaceSlot {
    fn new(root: PathBuf, config: FilePickerConfig) -> Self {
        Self {
            root,
            config,
            workspace: OnceCell::new(),
        }
    }
}

impl FffWorkspaceCache {
    fn slot_for(
        &mut self,
        root: PathBuf,
        config: &FilePickerConfig,
    ) -> (Arc<FffWorkspaceSlot>, bool) {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.root == root && same_file_index_config(&slot.config, config))
        {
            let slot = self
                .slots
                .remove(index)
                .expect("workspace slot index came from this cache");
            self.slots.push_back(slot.clone());
            return (slot, true);
        }

        let slot = Arc::new(FffWorkspaceSlot::new(root, config.clone()));
        self.slots.push_back(slot.clone());
        self.trim();
        (slot, false)
    }

    fn ready(&mut self, root: &Path, config: &FilePickerConfig) -> Option<Arc<FffWorkspace>> {
        let index = self
            .slots
            .iter()
            .position(|slot| slot.root == root && same_file_index_config(&slot.config, config))?;
        let slot = self
            .slots
            .remove(index)
            .expect("workspace slot index came from this cache");
        let workspace = slot.workspace.get().cloned();
        self.slots.push_back(slot);
        workspace
    }

    fn trim(&mut self) {
        while self.slots.len() > WORKSPACE_CACHE_CAPACITY {
            let Some(index) = self
                .slots
                .iter()
                .position(|slot| Arc::strong_count(slot) == 1)
            else {
                break;
            };
            self.slots.remove(index);
        }
    }
}

pub(crate) fn search_files(
    root: &Path,
    query: &str,
    current_file: Option<&Path>,
    config: &FilePickerConfig,
) -> anyhow::Result<Vec<FileMatch>> {
    search_files_with_scan_wait(root, query, current_file, config, SCAN_WAIT)
}

pub(crate) fn search_files_available(
    root: &Path,
    query: &str,
    current_file: Option<&Path>,
    config: &FilePickerConfig,
) -> anyhow::Result<Vec<FileMatch>> {
    let total_start = std::time::Instant::now();
    let Some(workspace) = workspace_if_ready(root, config)? else {
        return Ok(Vec::new());
    };
    search_workspace_files(&workspace, query, current_file, Duration::ZERO, total_start)
}

pub(crate) fn search_file_explorer_available_cancellable(
    root: &Path,
    query: &str,
    config: &FileExplorerConfig,
    abort_signal: Option<&std::sync::atomic::AtomicBool>,
) -> anyhow::Result<Vec<PathBuf>> {
    let config = file_explorer_picker_config(config);
    let total_start = std::time::Instant::now();
    let workspace = workspace_for_root(root, &config)?;
    search_workspace_files_cancellable(
        &workspace,
        query,
        None,
        Duration::ZERO,
        total_start,
        abort_signal,
    )
    .map(|matches| {
        matches
            .into_iter()
            .map(|file_match| file_match.path)
            .collect()
    })
}

pub(crate) fn wait_for_initial_file_scan(
    root: &Path,
    config: &FilePickerConfig,
) -> anyhow::Result<bool> {
    let workspace = workspace_for_root(root, config)?;
    Ok(workspace.picker.wait_for_scan(INITIAL_SCAN_WAIT))
}

pub(crate) fn wait_for_initial_file_results(
    root: &Path,
    config: &FilePickerConfig,
) -> anyhow::Result<bool> {
    let workspace = workspace_for_root(root, config)?;
    let start = std::time::Instant::now();
    while start.elapsed() < FIRST_RESULTS_WAIT {
        {
            let picker_guard = workspace.picker.read()?;
            if picker_guard
                .as_ref()
                .is_some_and(|picker| !picker.get_files().is_empty())
            {
                return Ok(true);
            }
        }
        if workspace.picker.wait_for_scan(Duration::ZERO) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(false)
}

pub(crate) fn prewarm(root: &Path, config: &FilePickerConfig) {
    if let Err(err) = workspace_for_root(root, config) {
        log::debug!(
            "failed to prewarm FFF workspace for {}: {err:#}",
            root.display()
        );
    }
}

pub(crate) fn prewarm_file_explorer(root: &Path, config: &FileExplorerConfig) {
    let config = file_explorer_picker_config(config);
    if let Err(err) = workspace_for_root(root, &config) {
        log::debug!(
            "failed to prewarm FFF file explorer workspace for {}: {err:#}",
            root.display()
        );
    }
}

fn search_files_with_scan_wait(
    root: &Path,
    query: &str,
    current_file: Option<&Path>,
    config: &FilePickerConfig,
    scan_wait: Duration,
) -> anyhow::Result<Vec<FileMatch>> {
    let total_start = std::time::Instant::now();
    let workspace = workspace_for_root(root, config)?;
    search_workspace_files(&workspace, query, current_file, scan_wait, total_start)
}

fn search_workspace_files(
    workspace: &FffWorkspace,
    query: &str,
    current_file: Option<&Path>,
    scan_wait: Duration,
    total_start: std::time::Instant,
) -> anyhow::Result<Vec<FileMatch>> {
    search_workspace_files_cancellable(workspace, query, current_file, scan_wait, total_start, None)
}

fn search_workspace_files_cancellable(
    workspace: &FffWorkspace,
    query: &str,
    current_file: Option<&Path>,
    scan_wait: Duration,
    total_start: std::time::Instant,
    abort_signal: Option<&std::sync::atomic::AtomicBool>,
) -> anyhow::Result<Vec<FileMatch>> {
    let wait_start = std::time::Instant::now();
    let scan_ready = workspace.picker.wait_for_scan(scan_wait);
    let wait_elapsed = wait_start.elapsed();

    let picker_guard = workspace.picker.read()?;
    let Some(picker) = picker_guard.as_ref() else {
        anyhow::bail!("FFF picker is not initialized");
    };

    if query.trim().is_empty() {
        let query: Arc<str> = Arc::from(query);
        let matches = picker
            .get_files()
            .iter()
            .filter(|file| {
                !file.is_deleted()
                    && !abort_signal
                        .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::Relaxed))
            })
            .take(FILE_SEARCH_LIMIT)
            .map(|file| FileMatch {
                path: file.absolute_path(picker, picker.base_path()),
                query: query.clone(),
            })
            .collect::<Vec<_>>();

        log::info!(
            target: PICKER_TRACE_TARGET,
            "FFF file search query={query:?} mode=path_order scan_ready={scan_ready} wait={wait_elapsed:?} total={:?} results={} files={}",
            total_start.elapsed(),
            matches.len(),
            picker.get_files().len(),
        );

        return Ok(matches);
    }

    let query_tracker_guard = workspace.query_tracker.read().ok();
    let query_tracker = query_tracker_guard
        .as_ref()
        .and_then(|guard| guard.as_ref());
    let parser: QueryParser<FileSearchConfig> = QueryParser::default();
    let parsed = parser.parse(query);
    let current_file = current_file.and_then(|path| relative_path(&workspace.root, path));
    let search_threads = responsive_search_threads();

    let search_start = std::time::Instant::now();
    let results = picker.fuzzy_search(
        &parsed,
        query_tracker,
        FuzzySearchOptions {
            max_threads: search_threads,
            abort_signal,
            current_file: current_file.as_deref(),
            project_path: Some(&workspace.root),
            combo_boost_score_multiplier: 20_000,
            min_combo_count: 2,
            pagination: PaginationArgs {
                offset: 0,
                limit: FILE_SEARCH_LIMIT,
            },
        },
    );
    let search_elapsed = search_start.elapsed();

    let query: Arc<str> = Arc::from(query);
    let matches: Vec<FileMatch> = results
        .items
        .into_iter()
        .map(|file| {
            let path = file.absolute_path(picker, picker.base_path());
            FileMatch {
                path,
                query: query.clone(),
            }
        })
        .collect();

    log::info!(
        target: PICKER_TRACE_TARGET,
        "FFF file search query={query:?} scan_ready={scan_ready} threads={search_threads} wait={wait_elapsed:?} search={search_elapsed:?} total={:?} results={} matched={} files={}",
        total_start.elapsed(),
        matches.len(),
        results.total_matched,
        results.total_files,
    );

    Ok(matches)
}

fn responsive_search_threads() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    responsive_search_threads_for(available)
}

fn responsive_search_threads_for(available: usize) -> usize {
    available.saturating_sub((available / 4).max(1)).max(1)
}

pub(crate) fn grep_files(
    root: &Path,
    query: &str,
    smart_case: bool,
    config: &FilePickerConfig,
    content_overlays: &[ContentOverlay],
) -> anyhow::Result<Vec<GrepMatch>> {
    let workspace = workspace_for_root(root, config)?;
    if !workspace.picker.wait_for_scan(GREP_SCAN_WAIT) {
        anyhow::bail!("FFF scan is not ready");
    }

    let picker_guard = workspace.picker.read()?;
    let Some(picker) = picker_guard.as_ref() else {
        anyhow::bail!("FFF picker is not initialized");
    };

    let parser = QueryParser::new(GrepConfig);
    let parsed = parser.parse(query);
    let results = picker.grep_owned(
        &parsed,
        &GrepSearchOptions {
            smart_case,
            mode: GrepMode::Regex,
            page_limit: GREP_SEARCH_LIMIT,
            time_budget_ms: 250,
            ..GrepSearchOptions::default()
        },
        content_overlays,
    );

    if let Some(err) = results.regex_fallback_error {
        anyhow::bail!("failed to compile regex: {err}");
    }

    let matches = results
        .matches
        .into_iter()
        .map(|item| GrepMatch {
            path: item.path,
            line_num: item.line_number.saturating_sub(1) as usize,
        })
        .collect();

    Ok(matches)
}

pub(crate) fn record_file_open(root: &Path, config: &FilePickerConfig, query: &str, path: &Path) {
    let Ok(workspace) = workspace_for_root(root, config) else {
        return;
    };

    if let Ok(mut frecency_guard) = workspace.frecency.write() {
        if let Some(frecency) = frecency_guard.as_mut() {
            if let Err(err) = frecency.track_access(path) {
                log::debug!("failed to track FFF frecency for {}: {err}", path.display());
            }

            if let Ok(mut picker_guard) = workspace.picker.write() {
                if let Some(picker) = picker_guard.as_mut() {
                    if let Err(err) = picker.update_single_file_frecency(path, frecency) {
                        log::debug!(
                            "failed to refresh FFF frecency for {}: {err}",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    if query.is_empty() {
        return;
    }

    if let Ok(mut tracker_guard) = workspace.query_tracker.write() {
        if let Some(tracker) = tracker_guard.as_mut() {
            if let Err(err) = tracker.track_query_completion(query, &workspace.root, path) {
                log::debug!(
                    "failed to track FFF query completion for {}: {err}",
                    path.display()
                );
            }
        }
    };
}

fn workspace_for_root(root: &Path, config: &FilePickerConfig) -> anyhow::Result<Arc<FffWorkspace>> {
    let start = std::time::Instant::now();
    let root = helix_stdx::path::normalize(root);
    let (slot, cached) = {
        let cache = WORKSPACES.get_or_init(|| Mutex::new(FffWorkspaceCache::default()));
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("FFF workspace cache lock was poisoned"))?;
        cache.slot_for(root, config)
    };

    if let Some(workspace) = slot.workspace.get() {
        log::info!(
            target: PICKER_TRACE_TARGET,
            "phase=fff_workspace root={} state=reused elapsed_us={}",
            workspace.root.display(),
            start.elapsed().as_micros(),
        );
        return Ok(workspace.clone());
    }

    log::info!(
        target: PICKER_TRACE_TARGET,
        "phase=fff_workspace root={} state={}create_start",
        slot.root.display(),
        if cached { "wait_or_" } else { "" },
    );
    let workspace = slot
        .workspace
        .get_or_try_init(|| {
            FffWorkspace::new(slot.root.clone(), slot.config.clone()).map(Arc::new)
        })?
        .clone();
    if let Some(cache) = WORKSPACES.get() {
        cache
            .lock()
            .map_err(|_| anyhow::anyhow!("FFF workspace cache lock was poisoned"))?
            .trim();
    }
    log::info!(
        target: PICKER_TRACE_TARGET,
        "phase=fff_workspace root={} state=create_done elapsed_us={}",
        workspace.root.display(),
        start.elapsed().as_micros(),
    );
    Ok(workspace)
}

fn workspace_if_ready(
    root: &Path,
    config: &FilePickerConfig,
) -> anyhow::Result<Option<Arc<FffWorkspace>>> {
    let root = helix_stdx::path::normalize(root);
    let Some(cache) = WORKSPACES.get() else {
        return Ok(None);
    };
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("FFF workspace cache lock was poisoned"))?;
    Ok(cache.ready(&root, config))
}

impl FffWorkspace {
    fn new(root: PathBuf, config: FilePickerConfig) -> anyhow::Result<Self> {
        let picker = SharedPicker::default();
        let workspace = stable_path_hash(&root);
        let store = match CacheStore::open_default() {
            Ok(store) => Some(Arc::new(Mutex::new(store))),
            Err(err) => {
                log::debug!(
                    "disabling FFF persistent cache for {}: {err}",
                    root.display()
                );
                None
            }
        };
        if let Some(store) = &store {
            import_legacy_fff_cache(&root, &workspace, store);
        }
        let frecency = init_frecency(&root, &workspace, store.clone());
        let query_tracker = init_query_tracker(&root, &workspace, store.clone());
        let scan = scan_options(&config);

        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                enable_mmap_cache: false,
                enable_content_indexing: false,
                mode: FFFMode::Neovim,
                cache_budget: None,
                watch: true,
                follow_symlinks: scan.follow_links,
                enable_fs_root_scanning: true,
                enable_home_dir_scanning: true,
                scan,
            },
        )?;

        Ok(Self {
            root,
            picker,
            frecency,
            query_tracker,
        })
    }
}

fn same_file_index_config(left: &FilePickerConfig, right: &FilePickerConfig) -> bool {
    left.hidden == right.hidden
        && left.follow_symlinks == right.follow_symlinks
        && left.deduplicate_links == right.deduplicate_links
        && left.parents == right.parents
        && left.ignore == right.ignore
        && left.git_ignore == right.git_ignore
        && left.git_global == right.git_global
        && left.git_exclude == right.git_exclude
        && left.max_depth == right.max_depth
}

fn scan_options(config: &FilePickerConfig) -> FilePickerScanOptions {
    FilePickerScanOptions {
        hidden: config.hidden,
        parents: config.parents,
        ignore: config.ignore,
        git_ignore: config.git_ignore,
        git_global: config.git_global,
        git_exclude: config.git_exclude,
        follow_links: config.follow_symlinks,
        max_depth: config.max_depth,
        custom_ignore_files: Box::from([
            helix_loader::config_dir().join("ignore"),
            helix_loader::workspace_ignore_file_name().into(),
        ]),
        deduplicate_links: config.deduplicate_links,
    }
}

fn file_explorer_picker_config(config: &FileExplorerConfig) -> FilePickerConfig {
    FilePickerConfig {
        hidden: config.hidden,
        follow_symlinks: config.follow_symlinks,
        deduplicate_links: true,
        parents: config.parents,
        ignore: config.ignore,
        git_ignore: config.git_ignore,
        git_global: config.git_global,
        git_exclude: config.git_exclude,
        max_depth: None,
        hide_preview: true,
    }
}

fn init_frecency(
    root: &Path,
    workspace: &str,
    store: Option<Arc<Mutex<CacheStore>>>,
) -> SharedFrecency {
    let shared = SharedFrecency::default();
    let Some(store) = store else {
        return SharedFrecency::noop();
    };
    let tracker_store = HelixFrecencyStore {
        store,
        workspace: workspace.to_owned(),
    };
    match FrecencyTracker::new(tracker_store).and_then(|tracker| shared.init(tracker)) {
        Ok(()) => shared,
        Err(err) => {
            log::debug!(
                "disabling FFF frecency for {} in cache.sqlite3: {err}",
                root.display(),
            );
            SharedFrecency::noop()
        }
    }
}

fn init_query_tracker(
    root: &Path,
    workspace: &str,
    store: Option<Arc<Mutex<CacheStore>>>,
) -> SharedQueryTracker {
    let shared = SharedQueryTracker::default();
    let Some(store) = store else {
        return SharedQueryTracker::noop();
    };
    let tracker = QueryTracker::new(HelixQueryTrackerStore {
        store,
        workspace: workspace.to_owned(),
    });
    match shared.init(tracker) {
        Ok(()) => shared,
        Err(err) => {
            log::debug!(
                "disabling FFF query tracking for {} in cache.sqlite3: {err}",
                root.display(),
            );
            SharedQueryTracker::noop()
        }
    }
}

#[derive(Clone)]
struct HelixFrecencyStore {
    store: Arc<Mutex<CacheStore>>,
    workspace: String,
}

impl std::fmt::Debug for HelixFrecencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelixFrecencyStore")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl FrecencyStore for HelixFrecencyStore {
    fn load_all(&self) -> fff_search::frecency::FrecencyPersistenceResult<Vec<FrecencyRecord>> {
        let mut store = self.store.lock().map_err(|_| "helix store lock poisoned")?;
        let entries = store.frecency().list_by_workspace(&self.workspace)?;
        entries
            .into_iter()
            .map(|entry| {
                Ok(FrecencyRecord {
                    path_hash: hex_to_hash(&entry.path_hash)?,
                    accesses: serde_json::from_str(&entry.timestamps_json)?,
                })
            })
            .collect()
    }

    fn save(
        &self,
        path_hash: &[u8; 32],
        accesses: &VecDeque<u64>,
    ) -> fff_search::frecency::FrecencyPersistenceResult<()> {
        let timestamps_json = serde_json::to_string(accesses)?;
        let first_accessed_at = accesses.front().copied().unwrap_or_default() as i64;
        let last_accessed_at = accesses.back().copied().unwrap_or_default() as i64;
        let entry = FrecencyEntry {
            workspace: self.workspace.clone(),
            path_hash: hash_to_hex(path_hash),
            first_accessed_at,
            last_accessed_at,
            access_count: accesses.len() as i64,
            timestamps_json,
        };
        self.store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .frecency()
            .upsert(entry)?;
        Ok(())
    }

    fn delete(&self, path_hash: &[u8; 32]) -> fff_search::frecency::FrecencyPersistenceResult<()> {
        self.store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .frecency()
            .delete(&self.workspace, &hash_to_hex(path_hash))?;
        Ok(())
    }

    fn entry_count(&self) -> fff_search::frecency::FrecencyPersistenceResult<u64> {
        Ok(self
            .store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .frecency()
            .list_by_workspace(&self.workspace)?
            .len() as u64)
    }

    fn location(&self) -> String {
        "cache.sqlite3:frecency".to_string()
    }
}

#[derive(Clone)]
struct HelixQueryTrackerStore {
    store: Arc<Mutex<CacheStore>>,
    workspace: String,
}

impl std::fmt::Debug for HelixQueryTrackerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelixQueryTrackerStore")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl QueryTrackerStore for HelixQueryTrackerStore {
    fn load_match(
        &self,
        _project_path: &Path,
        query: &str,
    ) -> fff_search::query_tracker::QueryPersistenceResult<Option<QueryMatchEntry>> {
        let payload = self
            .store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .query_history()
            .load_query_match(&self.workspace, query)?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    fn save_match(
        &self,
        _project_path: &Path,
        query: &str,
        entry: &QueryMatchEntry,
    ) -> fff_search::query_tracker::QueryPersistenceResult<()> {
        let payload = serde_json::to_string(entry)?;
        self.store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .query_history()
            .save_query_match(&self.workspace, query, &payload, entry.last_opened as i64)?;
        Ok(())
    }

    fn append_history(
        &self,
        _project_path: &Path,
        kind: QueryHistoryKind,
        query: &str,
        timestamp: u64,
    ) -> fff_search::query_tracker::QueryPersistenceResult<()> {
        self.store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .query_history()
            .append_bounded_history(
                &self.workspace,
                query_history_kind(kind),
                query,
                timestamp as i64,
            )?;
        Ok(())
    }

    fn history_at(
        &self,
        _project_path: &Path,
        kind: QueryHistoryKind,
        offset: usize,
    ) -> fff_search::query_tracker::QueryPersistenceResult<Option<String>> {
        Ok(self
            .store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .query_history()
            .history_at(&self.workspace, query_history_kind(kind), offset)?)
    }

    fn entry_counts(
        &self,
    ) -> fff_search::query_tracker::QueryPersistenceResult<Vec<(&'static str, u64)>> {
        let rows = self
            .store
            .lock()
            .map_err(|_| "helix store lock poisoned")?
            .query_history()
            .list_by_workspace(&self.workspace)?;
        let associations = rows
            .iter()
            .filter(|row| row.id.starts_with("fff:assoc:"))
            .count() as u64;
        let file_history = rows
            .iter()
            .filter(|row| row.opened_path == "fff:history:file")
            .count() as u64;
        let grep_history = rows
            .iter()
            .filter(|row| row.opened_path == "fff:history:grep")
            .count() as u64;

        Ok(vec![
            ("query_file_entries", associations),
            ("query_history_entries", file_history),
            ("grep_query_history_entries", grep_history),
        ])
    }

    fn location(&self) -> String {
        "cache.sqlite3:query_history".to_string()
    }
}

fn import_legacy_fff_cache(root: &Path, workspace: &str, store: &Arc<Mutex<CacheStore>>) {
    let legacy_dir = db_dir(root);
    if !legacy_dir.join("frecency").join("data.mdb").exists()
        && !legacy_dir.join("queries").join("data.mdb").exists()
    {
        return;
    }

    let already_imported = store
        .lock()
        .ok()
        .and_then(|mut store| {
            store
                .frecency()
                .import_marker_exists(FFF_CACHE_IMPORT_MARKER)
                .ok()
        })
        .unwrap_or(false);
    if already_imported {
        return;
    }

    match read_legacy_fff_cache(root, workspace) {
        Ok((frecency_entries, query_entries)) => {
            let result = store
                .lock()
                .map_err(|_| anyhow::anyhow!("helix store lock poisoned"))
                .and_then(|mut store| {
                    store
                        .frecency()
                        .import_fff_cache_once(
                            FFF_CACHE_IMPORT_MARKER,
                            &frecency_entries,
                            &query_entries,
                        )
                        .map_err(Into::into)
                });
            match result {
                Ok(true) => log::debug!(
                    "imported FFF legacy cache for {}: frecency={} query_rows={}",
                    root.display(),
                    frecency_entries.len(),
                    query_entries.len()
                ),
                Ok(false) => {}
                Err(err) => log::debug!(
                    "failed to import FFF legacy cache for {}; starting empty: {err:#}",
                    root.display()
                ),
            }
        }
        Err(err) => log::debug!(
            "failed to read FFF legacy cache for {}; starting empty: {err:#}",
            root.display()
        ),
    }
}

fn read_legacy_fff_cache(
    root: &Path,
    workspace: &str,
) -> anyhow::Result<(Vec<FrecencyEntry>, Vec<QueryHistory>)> {
    let legacy_dir = db_dir(root);
    let frecency_entries = read_legacy_frecency(&legacy_dir.join("frecency"), workspace)
        .unwrap_or_else(|err| {
            log::debug!(
                "skipping legacy frecency import for {}: {err:#}",
                root.display()
            );
            Vec::new()
        });
    let query_entries = read_legacy_queries(&legacy_dir.join("queries"), root, workspace)
        .unwrap_or_else(|err| {
            log::debug!(
                "skipping legacy query-history import for {}: {err:#}",
                root.display()
            );
            Vec::new()
        });

    Ok((frecency_entries, query_entries))
}

fn read_legacy_frecency(path: &Path, workspace: &str) -> anyhow::Result<Vec<FrecencyEntry>> {
    if !path.join("data.mdb").exists() {
        return Ok(Vec::new());
    }

    let env = unsafe { EnvOpenOptions::new().open(path)? };
    let rtxn = env.read_txn()?;
    let Some(db): Option<Database<Bytes, SerdeBincode<VecDeque<u64>>>> =
        env.open_database(&rtxn, None)?
    else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for item in db.iter(&rtxn)? {
        let (key, accesses) = item?;
        if key.len() != 32 || accesses.is_empty() {
            continue;
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(key);
        entries.push(FrecencyEntry {
            workspace: workspace.to_owned(),
            path_hash: hash_to_hex(&hash),
            first_accessed_at: accesses.front().copied().unwrap_or_default() as i64,
            last_accessed_at: accesses.back().copied().unwrap_or_default() as i64,
            access_count: accesses.len() as i64,
            timestamps_json: serde_json::to_string(&accesses)?,
        });
    }
    Ok(entries)
}

fn read_legacy_queries(
    path: &Path,
    root: &Path,
    workspace: &str,
) -> anyhow::Result<Vec<QueryHistory>> {
    if !path.join("data.mdb").exists() {
        return Ok(Vec::new());
    }

    let env = unsafe {
        let mut opts = EnvOpenOptions::new();
        opts.max_dbs(16);
        opts.open(path)?
    };
    let rtxn = env.read_txn()?;
    let Some(query_file_db): Option<Database<Bytes, SerdeBincode<QueryMatchEntry>>> =
        env.open_database(&rtxn, Some("query_file_associations"))?
    else {
        return Ok(Vec::new());
    };
    let query_history_db: Option<Database<Bytes, SerdeBincode<VecDeque<LegacyHistoryEntry>>>> =
        env.open_database(&rtxn, Some("query_history"))?;
    let grep_query_history_db: Option<Database<Bytes, SerdeBincode<VecDeque<LegacyHistoryEntry>>>> =
        env.open_database(&rtxn, Some("grep_query_history"))?;

    let mut rows = Vec::new();
    if let Some(db) = query_history_db {
        let project_key = legacy_project_key(root)?;
        if let Some(history) = db.get(&rtxn, &project_key)? {
            for entry in history.iter().rev().take(128).rev() {
                rows.push(history_row(
                    workspace,
                    "file",
                    &entry.query,
                    entry.timestamp,
                ));
                let query_key = legacy_query_key(root, &entry.query)?;
                if let Some(match_entry) = query_file_db.get(&rtxn, &query_key)? {
                    rows.push(query_match_row(
                        workspace,
                        &entry.query,
                        &serde_json::to_string(&match_entry)?,
                        match_entry.last_opened,
                    ));
                }
            }
        }
    }
    if let Some(db) = grep_query_history_db {
        let project_key = legacy_project_key(root)?;
        if let Some(history) = db.get(&rtxn, &project_key)? {
            for entry in history.iter().rev().take(128).rev() {
                rows.push(history_row(
                    workspace,
                    "grep",
                    &entry.query,
                    entry.timestamp,
                ));
            }
        }
    }

    Ok(rows)
}

#[derive(Debug, Deserialize, Serialize)]
struct LegacyHistoryEntry {
    query: String,
    timestamp: u64,
}

fn query_history_kind(kind: QueryHistoryKind) -> &'static str {
    match kind {
        QueryHistoryKind::File => "file",
        QueryHistoryKind::Grep => "grep",
    }
}

fn query_match_row(workspace: &str, query: &str, payload_json: &str, ts: u64) -> QueryHistory {
    QueryHistory {
        id: query_match_id(workspace, query),
        workspace: workspace.to_owned(),
        query: query.to_owned(),
        opened_path: payload_json.to_owned(),
        ts: ts as i64,
    }
}

fn history_row(workspace: &str, kind: &str, query: &str, ts: u64) -> QueryHistory {
    QueryHistory {
        id: query_history_id(workspace, kind, query, ts as i64),
        workspace: workspace.to_owned(),
        query: query.to_owned(),
        opened_path: format!("fff:history:{kind}"),
        ts: ts as i64,
    }
}

fn legacy_project_key(project_path: &Path) -> anyhow::Result<[u8; 32]> {
    let project_str = project_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path {}", project_path.display()))?;
    Ok(*blake3::hash(project_str.as_bytes()).as_bytes())
}

fn legacy_query_key(project_path: &Path, query: &str) -> anyhow::Result<[u8; 32]> {
    let project_str = project_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid path {}", project_path.display()))?;
    let mut hasher = blake3::Hasher::default();
    hasher.update(project_str.as_bytes());
    hasher.update(b"::");
    hasher.update(query.as_bytes());
    Ok(*hasher.finalize().as_bytes())
}

fn hash_to_hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn hex_to_hash(hex: &str) -> anyhow::Result<[u8; 32]> {
    if hex.len() != 64 {
        anyhow::bail!("invalid frecency hash length {}", hex.len());
    }
    let mut hash = [0; 32];
    for (idx, byte) in hash.iter_mut().enumerate() {
        let start = idx * 2;
        *byte = u8::from_str_radix(&hex[start..start + 2], 16)?;
    }
    Ok(hash)
}

fn query_match_id(workspace: &str, query: &str) -> String {
    format!("fff:assoc:{:016x}", stable_hash_parts(&[workspace, query]))
}

fn query_history_id(workspace: &str, kind: &str, query: &str, ts: i64) -> String {
    let ts_string = ts.to_string();
    format!(
        "fff:history:{kind}:{ts}:{:016x}",
        stable_hash_parts(&[workspace, kind, query, &ts_string])
    )
}

fn db_dir(root: &Path) -> PathBuf {
    helix_loader::cache_dir()
        .join("fff")
        .join(stable_path_hash(root))
}

fn stable_path_hash(path: &Path) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

fn stable_hash_parts(parts: &[&str]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;

    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_search_threads_reserve_capacity_without_disabling_parallelism() {
        assert_eq!(responsive_search_threads_for(1), 1);
        assert_eq!(responsive_search_threads_for(2), 1);
        assert_eq!(responsive_search_threads_for(4), 3);
        assert_eq!(responsive_search_threads_for(8), 6);
        assert_eq!(responsive_search_threads_for(16), 12);
    }

    #[test]
    fn stable_path_hash_is_deterministic() {
        let path = PathBuf::from("workspace").join("src");

        assert_eq!(stable_path_hash(&path), stable_path_hash(&path));
    }

    #[test]
    fn scan_options_preserve_picker_config() {
        let config = FilePickerConfig {
            ignore: false,
            git_ignore: false,
            max_depth: Some(2),
            ..FilePickerConfig::default()
        };
        let scan = scan_options(&config);

        assert!(!scan.ignore);
        assert!(!scan.git_ignore);
        assert_eq!(scan.max_depth, Some(2));
    }

    #[test]
    fn explorer_picker_config_preserves_explorer_scan_semantics() {
        let config = FileExplorerConfig {
            hidden: false,
            follow_symlinks: false,
            parents: true,
            ignore: false,
            git_ignore: false,
            git_global: false,
            git_exclude: false,
            ..FileExplorerConfig::default()
        };
        let picker_config = file_explorer_picker_config(&config);
        let scan = scan_options(&picker_config);

        assert_eq!(scan.hidden, config.hidden);
        assert_eq!(scan.follow_links, config.follow_symlinks);
        assert_eq!(scan.parents, config.parents);
        assert_eq!(scan.ignore, config.ignore);
        assert_eq!(scan.git_ignore, config.git_ignore);
        assert_eq!(scan.git_global, config.git_global);
        assert_eq!(scan.git_exclude, config.git_exclude);
        assert_eq!(scan.max_depth, None);
    }

    #[test]
    fn workspace_cache_keeps_picker_and_explorer_indexes_separate_and_reusable() {
        let root = PathBuf::from("workspace");
        let picker_config = FilePickerConfig::default();
        let explorer_config = file_explorer_picker_config(&FileExplorerConfig::default());
        assert!(!same_file_index_config(&picker_config, &explorer_config));

        let mut cache = FffWorkspaceCache::default();
        let (picker, picker_cached) = cache.slot_for(root.clone(), &picker_config);
        let (explorer, explorer_cached) = cache.slot_for(root.clone(), &explorer_config);
        let (picker_again, picker_again_cached) = cache.slot_for(root, &picker_config);

        assert!(!picker_cached);
        assert!(!explorer_cached);
        assert!(picker_again_cached);
        assert!(Arc::ptr_eq(&picker, &picker_again));
        assert!(!Arc::ptr_eq(&picker, &explorer));
        assert_eq!(cache.slots.len(), 2);
    }

    #[test]
    fn workspace_cache_bounds_live_indexes_and_evicts_least_recently_used() {
        let config = FilePickerConfig::default();
        let mut cache = FffWorkspaceCache::default();
        let oldest_root = PathBuf::from("workspace-0");
        cache.slot_for(oldest_root.clone(), &config);

        for index in 1..=WORKSPACE_CACHE_CAPACITY {
            cache.slot_for(PathBuf::from(format!("workspace-{index}")), &config);
        }

        assert_eq!(cache.slots.len(), WORKSPACE_CACHE_CAPACITY);
        assert!(cache.slots.iter().all(|slot| slot.root != oldest_root));
    }

    #[test]
    fn file_search_is_not_limited_to_initial_batch_size() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..320 {
            std::fs::write(temp.path().join(format!("file-{index:03}.txt")), "contents")
                .expect("write file");
        }

        let config = FilePickerConfig::default();
        let workspace = workspace_for_root(temp.path(), &config).expect("workspace");
        assert!(workspace.picker.wait_for_scan(Duration::from_secs(10)));

        let matches =
            search_files_with_scan_wait(temp.path(), "", None, &config, Duration::from_secs(10))
                .expect("search");
        assert!(
            matches.len() > 250,
            "file search returned {} results; expected more than the early batch size",
            matches.len()
        );
    }

    #[test]
    fn empty_file_search_uses_path_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("a.rs"), "older").expect("write file");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(temp.path().join("z.rs"), "newer").expect("write file");

        let config = FilePickerConfig::default();
        let matches =
            search_files_with_scan_wait(temp.path(), "", None, &config, Duration::from_secs(10))
                .expect("search");
        let names = matches
            .iter()
            .take(2)
            .map(|item| {
                item.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();

        assert_eq!(names, ["a.rs", "z.rs"]);
    }

    #[test]
    fn sqlite_frecency_store_stays_consistent_with_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Mutex::new(
            CacheStore::open(helix_store::StorePaths::new(
                temp.path().join("state.sqlite3"),
                temp.path().join("cache.sqlite3"),
            ))
            .expect("open store"),
        ));
        let workspace = "workspace-test".to_owned();
        let tracker = FrecencyTracker::new(HelixFrecencyStore {
            store: store.clone(),
            workspace: workspace.clone(),
        })
        .expect("tracker");
        let path = temp.path().join("src").join("main.rs");

        tracker.track_access(&path).expect("track access");
        assert!(tracker.get_access_score(&path, FFFMode::Neovim) > 0);

        let rows = store
            .lock()
            .unwrap()
            .frecency()
            .list_by_workspace(&workspace)
            .expect("list frecency");
        assert_eq!(rows.len(), 1);

        let reloaded =
            FrecencyTracker::new(HelixFrecencyStore { store, workspace }).expect("reload tracker");
        assert!(reloaded.get_access_score(&path, FFFMode::Neovim) > 0);
    }

    #[test]
    fn seeded_legacy_lmdb_import_is_once_and_queryable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let legacy = temp.path().join("legacy");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("root");
        let workspace = stable_path_hash(&root);
        seed_legacy_frecency(&legacy.join("frecency"));
        seed_legacy_queries(&legacy.join("queries"), &root);

        let frecency_entries =
            read_legacy_frecency(&legacy.join("frecency"), &workspace).expect("read frecency");
        let query_entries =
            read_legacy_queries(&legacy.join("queries"), &root, &workspace).expect("read queries");
        assert_eq!(frecency_entries.len(), 1);
        assert!(query_entries
            .iter()
            .any(|row| row.id.starts_with("fff:assoc:")));
        assert!(query_entries
            .iter()
            .any(|row| row.opened_path == "fff:history:file"));

        let mut store = CacheStore::open(helix_store::StorePaths::new(
            temp.path().join("state.sqlite3"),
            temp.path().join("cache.sqlite3"),
        ))
        .expect("open store");
        assert!(store
            .frecency()
            .import_fff_cache_once(FFF_CACHE_IMPORT_MARKER, &frecency_entries, &query_entries)
            .expect("import"));
        assert!(store
            .frecency()
            .import_marker_exists(FFF_CACHE_IMPORT_MARKER)
            .expect("marker"));
        assert_eq!(
            store
                .query_history()
                .history_at(&workspace, "file", 0)
                .expect("history"),
            Some("main".to_owned())
        );
        assert!(!store
            .frecency()
            .import_fff_cache_once(FFF_CACHE_IMPORT_MARKER, &[], &[])
            .expect("reimport"));
    }

    #[test]
    fn missing_legacy_lmdb_reads_empty_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("project");
        let workspace = "workspace-test";

        assert!(
            read_legacy_frecency(&temp.path().join("missing-frecency"), workspace)
                .expect("missing frecency")
                .is_empty()
        );
        assert!(
            read_legacy_queries(&temp.path().join("missing-queries"), &root, workspace)
                .expect("missing queries")
                .is_empty()
        );
    }

    fn seed_legacy_frecency(path: &Path) {
        std::fs::create_dir_all(path).expect("legacy frecency dir");
        let env = unsafe {
            let mut opts = EnvOpenOptions::new();
            opts.map_size(1024 * 1024);
            opts.open(path).expect("open legacy frecency")
        };
        let mut wtxn = env.write_txn().expect("write txn");
        let db: Database<Bytes, SerdeBincode<VecDeque<u64>>> =
            env.create_database(&mut wtxn, None).expect("db");
        db.put(&mut wtxn, &[7; 32], &VecDeque::from([100_u64, 200]))
            .expect("put frecency");
        wtxn.commit().expect("commit");
    }

    fn seed_legacy_queries(path: &Path, root: &Path) {
        std::fs::create_dir_all(path).expect("legacy queries dir");
        let env = unsafe {
            let mut opts = EnvOpenOptions::new();
            opts.map_size(1024 * 1024);
            opts.max_dbs(16);
            opts.open(path).expect("open legacy queries")
        };
        let mut wtxn = env.write_txn().expect("write txn");
        let query_file_db: Database<Bytes, SerdeBincode<QueryMatchEntry>> = env
            .create_database(&mut wtxn, Some("query_file_associations"))
            .expect("query db");
        let query_history_db: Database<Bytes, SerdeBincode<VecDeque<LegacyHistoryEntry>>> = env
            .create_database(&mut wtxn, Some("query_history"))
            .expect("history db");
        let grep_query_history_db: Database<Bytes, SerdeBincode<VecDeque<LegacyHistoryEntry>>> =
            env.create_database(&mut wtxn, Some("grep_query_history"))
                .expect("grep history db");
        let project_key = legacy_project_key(root).expect("project key");
        let query_key = legacy_query_key(root, "main").expect("query key");
        query_file_db
            .put(
                &mut wtxn,
                &query_key,
                &QueryMatchEntry {
                    file_path: root.join("src/main.rs"),
                    open_count: 2,
                    last_opened: 300,
                },
            )
            .expect("put association");
        query_history_db
            .put(
                &mut wtxn,
                &project_key,
                &VecDeque::from([LegacyHistoryEntry {
                    query: "main".to_owned(),
                    timestamp: 300,
                }]),
            )
            .expect("put file history");
        grep_query_history_db
            .put(
                &mut wtxn,
                &project_key,
                &VecDeque::from([LegacyHistoryEntry {
                    query: "struct".to_owned(),
                    timestamp: 301,
                }]),
            )
            .expect("put grep history");
        wtxn.commit().expect("commit");
    }

    #[test]
    #[ignore]
    fn fff_cache_perf_probe_50k() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("root");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_secs();
        let candidates = (0..50_000)
            .map(|index| root.join(format!("src/file-{index:05}.rs")))
            .collect::<Vec<_>>();
        let accesses = VecDeque::from([now - 20, now - 10, now]);

        let legacy_path = temp.path().join("legacy-frecency");
        std::fs::create_dir_all(&legacy_path).expect("legacy dir");
        let legacy_env = unsafe {
            let mut opts = EnvOpenOptions::new();
            opts.map_size(64 * 1024 * 1024);
            opts.open(&legacy_path).expect("open legacy")
        };
        let mut wtxn = legacy_env.write_txn().expect("legacy seed txn");
        let legacy_db: Database<Bytes, SerdeBincode<VecDeque<u64>>> =
            legacy_env.create_database(&mut wtxn, None).expect("db");
        for path in &candidates {
            legacy_db
                .put(&mut wtxn, &path_hash(path), &accesses)
                .expect("legacy put");
        }
        wtxn.commit().expect("legacy seed commit");

        let old_read_start = std::time::Instant::now();
        let mut old_total = 0_i64;
        for path in &candidates {
            let rtxn = legacy_env.read_txn().expect("legacy read txn");
            let value = legacy_db
                .get(&rtxn, &path_hash(path))
                .expect("legacy get")
                .unwrap_or_default();
            old_total += FrecencyTracker::calculate_access_score(&value, now, FFFMode::Neovim);
        }
        let old_read = old_read_start.elapsed();

        let workspace = "perf-workspace".to_owned();
        let frecency_entries = candidates
            .iter()
            .map(|path| FrecencyEntry {
                workspace: workspace.clone(),
                path_hash: hash_to_hex(&path_hash(path)),
                first_accessed_at: now as i64 - 20,
                last_accessed_at: now as i64,
                access_count: accesses.len() as i64,
                timestamps_json: serde_json::to_string(&accesses).expect("timestamps"),
            })
            .collect::<Vec<_>>();
        let store = Arc::new(Mutex::new(
            CacheStore::open(helix_store::StorePaths::new(
                temp.path().join("state.sqlite3"),
                temp.path().join("cache.sqlite3"),
            ))
            .expect("open store"),
        ));
        store
            .lock()
            .unwrap()
            .frecency()
            .import_fff_cache_once("perf-marker", &frecency_entries, &[])
            .expect("seed sqlite");

        let load_start = std::time::Instant::now();
        let tracker = FrecencyTracker::new(HelixFrecencyStore {
            store: store.clone(),
            workspace,
        })
        .expect("tracker");
        let sqlite_load = load_start.elapsed();

        let new_read_start = std::time::Instant::now();
        let mut new_total = 0_i64;
        for path in &candidates {
            new_total += tracker.get_access_score(path, FFFMode::Neovim);
        }
        let new_read = new_read_start.elapsed();

        let old_write_start = std::time::Instant::now();
        for path in candidates.iter().take(100) {
            let key = path_hash(path);
            let rtxn = legacy_env.read_txn().expect("legacy write read txn");
            let mut value = legacy_db
                .get(&rtxn, &key)
                .expect("legacy write read")
                .unwrap_or_default();
            drop(rtxn);
            value.push_back(now + 1);
            let mut wtxn = legacy_env.write_txn().expect("legacy write txn");
            legacy_db
                .put(&mut wtxn, &key, &value)
                .expect("legacy write");
            wtxn.commit().expect("legacy write commit");
        }
        let old_write = old_write_start.elapsed();

        let new_write_start = std::time::Instant::now();
        for path in candidates.iter().take(100) {
            tracker.track_access(path).expect("sqlite write-through");
        }
        let new_write = new_write_start.elapsed();

        eprintln!(
            "fff_perf_50k old_lmdb_read={old_read:?} sqlite_load={sqlite_load:?} \
             new_index_read={new_read:?} old_lmdb_write_100={old_write:?} \
             new_sqlite_write_100={new_write:?} totals=({old_total},{new_total})"
        );
    }

    fn path_hash(path: &Path) -> [u8; 32] {
        *blake3::hash(path.to_string_lossy().as_bytes()).as_bytes()
    }

    #[test]
    #[ignore]
    fn file_picker_explorer_workspace_reuse_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join("src")).expect("project dirs");
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}\n").expect("fixture file");

        let picker_config = FilePickerConfig {
            hidden: false,
            follow_symlinks: false,
            deduplicate_links: true,
            parents: false,
            ignore: false,
            git_ignore: false,
            git_global: false,
            git_exclude: false,
            max_depth: None,
            hide_preview: false,
        };
        let explorer_config = FilePickerConfig {
            hide_preview: true,
            ..picker_config.clone()
        };

        let first_start = std::time::Instant::now();
        let picker_workspace = workspace_for_root(&root, &picker_config).expect("picker workspace");
        let first_init = first_start.elapsed();
        let second_start = std::time::Instant::now();
        let explorer_workspace =
            workspace_for_root(&root, &explorer_config).expect("explorer workspace");
        let second_init = second_start.elapsed();

        eprintln!(
            "fff_workspace_reuse same_workspace={} first_init={first_init:?} second_init={second_init:?}",
            Arc::ptr_eq(&picker_workspace, &explorer_workspace),
        );
    }

    #[test]
    #[ignore]
    fn file_search_timing_current_workspace() {
        let root = std::env::var_os("DHX_FFF_PROBE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(helix_stdx::env::current_working_dir);
        let config = FilePickerConfig::default();

        eprintln!("root={}", root.display());
        let init_start = std::time::Instant::now();
        let workspace = workspace_for_root(&root, &config).expect("workspace");
        eprintln!("workspace_init={:?}", init_start.elapsed());

        let first_result_start = std::time::Instant::now();
        let mut first_result = None;
        while first_result_start.elapsed() < Duration::from_secs(2) {
            let matches = search_files(&root, "", None, &config).expect("search");
            if !matches.is_empty() {
                first_result = Some((first_result_start.elapsed(), matches.len()));
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        match first_result {
            Some((elapsed, count)) => eprintln!("first_results={elapsed:?} count={count}"),
            None => eprintln!("first_results=none"),
        }

        let scan_start = std::time::Instant::now();
        let scan_ready = workspace.picker.wait_for_scan(Duration::from_secs(60));
        eprintln!(
            "scan_ready={scan_ready} scan_wait={:?}",
            scan_start.elapsed()
        );

        for query in ["", "src", "picker", "fff"] {
            let search_start = std::time::Instant::now();
            let matches = search_files(&root, query, None, &config).expect("search");
            eprintln!(
                "query={query:?} elapsed={:?} results={}",
                search_start.elapsed(),
                matches.len()
            );
        }
    }
}
