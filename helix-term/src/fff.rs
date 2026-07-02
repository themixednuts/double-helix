use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use fff_search::{
    ContentOverlay, FFFMode, FilePicker, FilePickerOptions, FilePickerScanOptions,
    FileSearchConfig, FrecencyTracker, FuzzySearchOptions, GrepConfig, GrepMode, GrepSearchOptions,
    PaginationArgs, QueryParser, QueryTracker, SharedFrecency, SharedPicker, SharedQueryTracker,
};
use helix_view::editor::FilePickerConfig;

const FILE_SEARCH_LIMIT: usize = 1_000;
const SCAN_WAIT: Duration = Duration::from_millis(20);
const FIRST_RESULTS_WAIT: Duration = Duration::from_secs(2);
const INITIAL_SCAN_WAIT: Duration = Duration::from_secs(30);
const GREP_SEARCH_LIMIT: usize = 2_000;
const GREP_SCAN_WAIT: Duration = Duration::from_millis(250);
const PICKER_TRACE_TARGET: &str = crate::ui::picker::PICKER_TRACE_TARGET;

static ACTIVE_WORKSPACE: OnceLock<Mutex<Option<Arc<FffWorkspace>>>> = OnceLock::new();

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
    config: FilePickerConfig,
    picker: SharedPicker,
    frecency: SharedFrecency,
    query_tracker: SharedQueryTracker,
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
    search_files_with_scan_wait(root, query, current_file, config, Duration::ZERO)
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

fn search_files_with_scan_wait(
    root: &Path,
    query: &str,
    current_file: Option<&Path>,
    config: &FilePickerConfig,
    scan_wait: Duration,
) -> anyhow::Result<Vec<FileMatch>> {
    let total_start = std::time::Instant::now();
    let workspace = workspace_for_root(root, config)?;
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
            .filter(|file| !file.is_deleted())
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

    let search_start = std::time::Instant::now();
    let results = picker.fuzzy_search(
        &parsed,
        query_tracker,
        FuzzySearchOptions {
            max_threads: 0,
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
        "FFF file search query={query:?} scan_ready={scan_ready} wait={wait_elapsed:?} search={search_elapsed:?} total={:?} results={} matched={} files={}",
        total_start.elapsed(),
        matches.len(),
        results.total_matched,
        results.total_files,
    );

    Ok(matches)
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
    let active = ACTIVE_WORKSPACE.get_or_init(|| Mutex::new(None));
    let mut guard = active
        .lock()
        .map_err(|_| anyhow::anyhow!("FFF workspace lock was poisoned"))?;

    if let Some(workspace) = guard.as_ref() {
        if workspace.root == root && workspace.config == *config {
            log::info!(
                target: PICKER_TRACE_TARGET,
                "phase=fff_workspace root={} state=reused elapsed_us={}",
                root.display(),
                start.elapsed().as_micros(),
            );
            return Ok(workspace.clone());
        }
    }

    log::info!(
        target: PICKER_TRACE_TARGET,
        "phase=fff_workspace root={} state=create_start",
        root.display(),
    );
    let workspace = Arc::new(FffWorkspace::new(root, config.clone())?);
    log::info!(
        target: PICKER_TRACE_TARGET,
        "phase=fff_workspace root={} state=create_done elapsed_us={}",
        workspace.root.display(),
        start.elapsed().as_micros(),
    );
    *guard = Some(workspace.clone());
    Ok(workspace)
}

impl FffWorkspace {
    fn new(root: PathBuf, config: FilePickerConfig) -> anyhow::Result<Self> {
        let picker = SharedPicker::default();
        let frecency = init_frecency(&root);
        let query_tracker = init_query_tracker(&root);
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
                scan,
            },
        )?;

        Ok(Self {
            root,
            config,
            picker,
            frecency,
            query_tracker,
        })
    }
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

fn init_frecency(root: &Path) -> SharedFrecency {
    let shared = SharedFrecency::default();
    let path = db_dir(root).join("frecency");
    match FrecencyTracker::new(&path, false).and_then(|tracker| shared.init(tracker)) {
        Ok(()) => shared,
        Err(err) => {
            log::debug!(
                "disabling FFF frecency for {} at {}: {err}",
                root.display(),
                path.display()
            );
            SharedFrecency::noop()
        }
    }
}

fn init_query_tracker(root: &Path) -> SharedQueryTracker {
    let shared = SharedQueryTracker::default();
    let path = db_dir(root).join("queries");
    match QueryTracker::new(&path, false).and_then(|tracker| shared.init(tracker)) {
        Ok(()) => shared,
        Err(err) => {
            log::debug!(
                "disabling FFF query tracking for {} at {}: {err}",
                root.display(),
                path.display()
            );
            SharedQueryTracker::noop()
        }
    }
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

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
