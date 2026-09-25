//! Workspace trust.
//!
//! A workspace can carry configuration that runs code: `.double-helix/config.toml` and
//! `.double-helix/languages.toml` can name language servers, formatters and grammar sources, a
//! repository's `.git/config` can name filter programs, and debug adapters run whatever the
//! language config says. These are gated behind trust granted per workspace.
//!
//! Trust is granted with `:workspace-trust` (or the prompt) and revoked with `:workspace-untrust`.
//! A grant pins a hash of the workspace's `.double-helix/` configuration. If it changes later the
//! workspace becomes [`TrustStatus::Stale`] and local config is no longer loaded until the user
//! re-runs `:workspace-trust`. Language servers keep launching under stale trust: their binaries
//! are configured globally and were not part of what changed.
//!
//! ## Storage
//!
//! Decisions live in the `workspace_trust` table of the durable state database
//! (`data_dir()/state.sqlite3`), one row per workspace path holding the pinned hash (or none, for
//! excluded workspaces and workspaces without local config). SQLite's WAL keeps concurrent editor
//! instances consistent.
//!
//! ## Trusted globs (discouraged)
//!
//! `[editor.workspace-trust] trusted = [...]` trusts every workspace whose path matches a glob. It
//! skips the hash pin (local config changes are never re-checked) and trusts repositories that land
//! under a matching directory later. An explicit exclude still wins over a matching glob.

use std::{
    collections::HashMap,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use helix_store::{Store, StorePaths, WorkspaceTrustGrant};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};

use crate::{find_workspace, find_workspace_in, WORKSPACE_CONFIG_DIR};

/// Directories under `.double-helix/` that the editor itself writes (collaboration state, workspace
/// transaction journals). They are not configuration, so they stay out of the trust hash; hashing
/// them would make every trusted workspace go stale as soon as it is used.
const EDITOR_MANAGED_DIRS: &[&str] = &["state", "transactions"];

/// The capability a trust query is about.
#[derive(Debug, Clone, Copy)]
pub enum TrustQuery {
    /// Launching language servers.
    Lsp,
    /// Launching debug adapters.
    Dap,
    /// Loading `.double-helix/config.toml` and `.double-helix/languages.toml`.
    LocalConfig,
    /// Honoring the repository-local `.git/config` (gix `Trust::Full`).
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustStatus {
    /// Trusted for the queried capability.
    Trusted,
    /// No trust decision was made, and no implicit trust applies.
    Untrusted,
    /// Trusted before, but the workspace config changed since the grant. Local config stays
    /// unloaded until the user trusts again; language servers still launch.
    Stale,
    /// Explicitly excluded: never trusted and never prompted for.
    Excluded,
}

impl TrustStatus {
    pub fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted)
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Stale)
    }

    pub fn is_excluded(&self) -> bool {
        matches!(self, Self::Excluded)
    }
}

/// What every workspace is trusted with, without a grant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImplicitTrustLevel {
    /// Nothing: every capability needs a grant.
    None,
    /// Language servers and debug adapters. Their binaries are configured globally, so starting
    /// them in a fresh workspace is expected; workspace config and git `Trust::Full` still need a
    /// grant.
    #[default]
    Servers,
    /// Everything, unless the workspace is excluded.
    Insecure,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub level: ImplicitTrustLevel,
    /// Whether opening a file in a restricted workspace asks for trust.
    pub prompt: bool,
    /// Workspaces whose path matches one of these are trusted.
    pub trusted_globs: GlobSet,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            level: ImplicitTrustLevel::default(),
            prompt: true,
            trusted_globs: GlobSet::empty(),
        }
    }
}

/// Compile trusted-workspace glob patterns. `~` and environment variables are expanded; invalid
/// patterns are logged and skipped. Paths match case-insensitively on Windows.
pub fn build_trusted_globs(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let expanded = helix_stdx::path::expand(pattern);
        match GlobBuilder::new(&expanded.to_string_lossy())
            .case_insensitive(cfg!(windows))
            .literal_separator(true)
            .build()
        {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(err) => log::error!("ignoring invalid workspace-trust glob {pattern:?}: {err}"),
        }
    }
    builder.build().unwrap_or_else(|err| {
        log::error!("failed to compile workspace-trust globs: {err}");
        GlobSet::empty()
    })
}

/// Workspace trust state. Cheap to clone: clones share the cache and the configuration.
#[derive(Clone)]
pub struct WorkspaceTrust {
    inner: Arc<Mutex<HashMap<PathBuf, CacheEntry>>>,
    config: Arc<RwLock<Config>>,
    /// Where decisions persist; `None` keeps them for the session only.
    store: Option<StorePaths>,
}

impl std::fmt::Debug for WorkspaceTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceTrust")
            .field("level", &self.config.read().level)
            .field("prompt", &self.config.read().prompt)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct CacheEntry {
    status: TrustStatus,
    /// Whether the workspace had a local `config.toml` or `languages.toml` when first queried;
    /// kept so the statusline indicator doesn't stat files on every render.
    has_local_config: bool,
}

impl WorkspaceTrust {
    pub fn new(config: Config) -> Self {
        Self::with_store(config, StorePaths::default_paths())
    }

    /// Trust state that keeps its decisions in the databases at `store`.
    pub fn with_store(config: Config, store: StorePaths) -> Self {
        Self {
            inner: Arc::default(),
            config: Arc::new(RwLock::new(config)),
            store: Some(store),
        }
    }

    /// Trust state that honors explicit grants only, not configured implicit trust: for code
    /// that runs without the editor's settings and acts on workspace config (grammar builds, the
    /// package catalog).
    pub fn explicit_grants_only() -> Self {
        Self::new(Config::default())
    }

    /// Trust state that grants everything and reads no stored decisions: for non-interactive
    /// uses (`--health`, tests) where there is nobody to ask.
    pub fn fully_trusted() -> Self {
        Self {
            inner: Arc::default(),
            config: Arc::new(RwLock::new(Config {
                level: ImplicitTrustLevel::Insecure,
                ..Config::default()
            })),
            store: None,
        }
    }

    pub fn implicit_level(&self) -> ImplicitTrustLevel {
        self.config.read().level
    }

    /// Whether opening a file in a restricted workspace should ask for trust. Without the prompt,
    /// the statusline indicator is the only signal.
    pub fn prompts_enabled(&self) -> bool {
        self.config.read().prompt
    }

    /// Replace the configuration. Also clears the cache so the next query re-reads grants and
    /// re-hashes: config reloads are how changes made to `.double-helix/` while the editor runs
    /// are picked up. Session-only denials are dropped with it.
    pub fn set_config(&self, config: Config) {
        *self.config.write() = config;
        self.inner.lock().clear();
    }

    /// The stored status of `workspace`, before implicit trust and per-capability rules; the only
    /// way to tell [`TrustStatus::Stale`] from [`TrustStatus::Untrusted`].
    pub fn status(&self, workspace: &Path) -> TrustStatus {
        self.entry(workspace).status
    }

    fn entry(&self, workspace: &Path) -> CacheEntry {
        if let Some(entry) = self.inner.lock().get(workspace).copied() {
            return entry;
        }
        let entry = CacheEntry {
            status: self.load_status(workspace),
            has_local_config: has_local_config(workspace),
        };
        self.inner.lock().insert(workspace.to_path_buf(), entry);
        entry
    }

    /// Whether `workspace` is trusted for `query`.
    pub fn query(&self, workspace: &Path, query: TrustQuery) -> TrustStatus {
        let stored = self.status(workspace);
        if stored == TrustStatus::Excluded {
            return TrustStatus::Excluded;
        }
        let level = self.implicit_level();
        if level == ImplicitTrustLevel::Insecure || self.is_glob_trusted(workspace) {
            return TrustStatus::Trusted;
        }
        if level == ImplicitTrustLevel::Servers
            && matches!(query, TrustQuery::Lsp | TrustQuery::Dap)
        {
            return TrustStatus::Trusted;
        }
        demote_for_query(stored, query)
    }

    /// [`Self::query`] for the workspace that contains `file`.
    pub fn query_for_file(&self, file: &Path, query: TrustQuery) -> TrustStatus {
        let workspace = file
            .parent()
            .map(|dir| find_workspace_in(dir).0)
            .unwrap_or_else(|| find_workspace().0);
        self.query(&workspace, query)
    }

    /// [`Self::query`] for the working directory's workspace.
    pub fn query_current(&self, query: TrustQuery) -> TrustStatus {
        self.query(&find_workspace().0, query)
    }

    fn is_glob_trusted(&self, workspace: &Path) -> bool {
        let config = self.config.read();
        !config.trusted_globs.is_empty() && config.trusted_globs.is_match(workspace)
    }

    /// Whether `workspace` runs restricted in a way trusting it would change: it has local config
    /// that is not loaded, or its grant went stale.
    pub fn workspace_restricted(&self, workspace: &Path) -> bool {
        if self.implicit_level() == ImplicitTrustLevel::Insecure || self.is_glob_trusted(workspace)
        {
            return false;
        }
        let entry = self.entry(workspace);
        match entry.status {
            TrustStatus::Stale => true,
            TrustStatus::Trusted | TrustStatus::Excluded => false,
            TrustStatus::Untrusted => entry.has_local_config,
        }
    }

    /// [`Self::workspace_restricted`] for a document: also restricted when the document has
    /// language servers or a debugger that trust would let start.
    pub fn restricted_for_doc(&self, workspace: &Path, servers_to_load: bool) -> bool {
        if self.workspace_restricted(workspace) {
            return true;
        }
        if !servers_to_load || self.status(workspace) != TrustStatus::Untrusted {
            return false;
        }
        !self.query(workspace, TrustQuery::Lsp).is_trusted()
            || !self.query(workspace, TrustQuery::Dap).is_trusted()
    }

    /// Trust `workspace`, pinning the current hash of its configuration. The decision applies to
    /// this session even when it could not be saved.
    pub fn trust(&self, workspace: &Path) -> helix_store::Result<()> {
        self.cache(workspace, TrustStatus::Trusted);
        self.put(workspace, compute_workspace_hash(workspace), false)
    }

    /// Drop the grant or exclusion for `workspace`.
    pub fn untrust(&self, workspace: &Path) -> helix_store::Result<()> {
        self.inner.lock().remove(workspace);
        match self.open()? {
            Some(mut store) => store.workspace_trust().remove(&workspace_key(workspace)),
            None => Ok(()),
        }
    }

    /// Exclude `workspace`: never trusted, never prompted for again. The decision applies to this
    /// session even when it could not be saved.
    pub fn exclude(&self, workspace: &Path) -> helix_store::Result<()> {
        self.cache(workspace, TrustStatus::Excluded);
        self.put(workspace, None, true)
    }

    /// Treat `workspace` as untrusted for the rest of the session.
    pub fn deny_once(&self, workspace: &Path) {
        self.cache(workspace, TrustStatus::Untrusted);
    }

    fn cache(&self, workspace: &Path, status: TrustStatus) {
        let entry = CacheEntry {
            status,
            has_local_config: has_local_config(workspace),
        };
        self.inner.lock().insert(workspace.to_path_buf(), entry);
    }

    fn load_status(&self, workspace: &Path) -> TrustStatus {
        let grant = match self.open().and_then(|store| match store {
            Some(mut store) => store.workspace_trust().get(&workspace_key(workspace)),
            None => Ok(None),
        }) {
            Ok(grant) => grant,
            Err(err) => {
                log::error!("reading workspace trust for {workspace:?} failed: {err}");
                None
            }
        };
        match grant {
            Some(grant) if grant.excluded => TrustStatus::Excluded,
            Some(grant) if grant.hash == compute_workspace_hash(workspace) => TrustStatus::Trusted,
            Some(_) => TrustStatus::Stale,
            None => TrustStatus::Untrusted,
        }
    }

    fn open(&self) -> helix_store::Result<Option<Store>> {
        self.store.clone().map(Store::open).transpose()
    }

    fn put(
        &self,
        workspace: &Path,
        hash: Option<String>,
        excluded: bool,
    ) -> helix_store::Result<()> {
        let Some(mut store) = self.open()? else {
            return Ok(());
        };
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        store.workspace_trust().put(WorkspaceTrustGrant {
            workspace: workspace_key(workspace),
            hash,
            excluded,
            updated_at,
        })
    }
}

fn workspace_key(workspace: &Path) -> String {
    workspace.to_string_lossy().into_owned()
}

fn has_local_config(workspace: &Path) -> bool {
    let dir = workspace.join(WORKSPACE_CONFIG_DIR);
    dir.join("config.toml").exists() || dir.join("languages.toml").exists()
}

/// A stale grant still lets language servers start (their binaries are global); everything else
/// is untrusted until the user trusts again.
fn demote_for_query(status: TrustStatus, query: TrustQuery) -> TrustStatus {
    match (status, query) {
        (TrustStatus::Stale, TrustQuery::Lsp) => TrustStatus::Trusted,
        (TrustStatus::Stale, _) => TrustStatus::Untrusted,
        _ => status,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------- hashing ----------

/// SHA-256 over the workspace's `.double-helix/` configuration (every file except the
/// editor-managed state), or `None` when there is none, so a workspace without local config can
/// still be trusted.
pub fn compute_workspace_hash(workspace: &Path) -> Option<String> {
    let config_dir = workspace.join(WORKSPACE_CONFIG_DIR);
    if !config_dir.is_dir() {
        return None;
    }

    let mut files = Vec::new();
    walk(&config_dir, &config_dir, &mut files);
    if files.is_empty() {
        return None;
    }
    files.sort();

    let mut hasher = Sha256::new();
    for file in &files {
        let relative = file.strip_prefix(&config_dir).unwrap_or(file);
        // Separators differ per platform; hash the same bytes everywhere.
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        hash_field(&mut hasher, relative.as_bytes());
        match fs::read(file) {
            Ok(bytes) => hash_field(&mut hasher, &bytes),
            Err(err) => {
                log::warn!("workspace hash: treating unreadable file {file:?} as empty: {err:?}");
                hash_field(&mut hasher, &[]);
            }
        }
    }
    Some(format!("sha256:{}", hex_encode(&hasher.finalize())))
}

/// Length-prefix each field so file boundaries can't be forged with content.
fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if dir == root
                && EDITOR_MANAGED_DIRS
                    .iter()
                    .any(|managed| entry.file_name() == *managed)
            {
                continue;
            }
            // Real directories only: following symlinked ones could loop.
            walk(root, &path, out);
        } else if file_type.is_file() {
            out.push(path);
        } else if file_type.is_symlink() {
            // A symlinked config file (dotfile managers) is hashed through its target, so edits
            // to the target are caught too.
            if fs::metadata(&path).is_ok_and(|target| target.is_file()) {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn config_file(workspace: &Path, name: &str) -> PathBuf {
        workspace.join(WORKSPACE_CONFIG_DIR).join(name)
    }

    /// Trust state whose decisions live in a temporary directory, not the user's data dir.
    fn trust_in(store: &tempfile::TempDir, config: Config) -> WorkspaceTrust {
        WorkspaceTrust::with_store(
            config,
            StorePaths::new(
                store.path().join("state.sqlite3"),
                store.path().join("cache.sqlite3"),
            ),
        )
    }

    #[test]
    fn hash_changes_when_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();

        assert_eq!(compute_workspace_hash(workspace), None);

        write_file(&config_file(workspace, "config.toml"), "a = 1");
        let h1 = compute_workspace_hash(workspace).expect("has files");
        assert!(h1.starts_with("sha256:"));
        assert_eq!(Some(&h1), compute_workspace_hash(workspace).as_ref());

        write_file(&config_file(workspace, "config.toml"), "a = 2");
        let h2 = compute_workspace_hash(workspace).expect("has files");
        assert_ne!(h1, h2);

        write_file(&config_file(workspace, "languages.toml"), "");
        let h3 = compute_workspace_hash(workspace).expect("has files");
        assert_ne!(h2, h3);
    }

    #[test]
    fn hash_ignores_editor_managed_state() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write_file(&config_file(workspace, "config.toml"), "a = 1");
        let before = compute_workspace_hash(workspace);

        write_file(&config_file(workspace, "state/data"), "private");
        write_file(&config_file(workspace, "transactions/1/journal"), "entry");
        assert_eq!(before, compute_workspace_hash(workspace));

        // Only at the top level: a nested `state` directory is user content.
        write_file(&config_file(workspace, "queries/state/x.scm"), "(x)");
        assert_ne!(before, compute_workspace_hash(workspace));
    }

    #[test]
    fn sha256_known_vector() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hex_encode(&hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn workspace_restricted_hides_when_no_local_config() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let trust = trust_in(&store, Config::default());

        assert!(!trust.workspace_restricted(workspace));

        write_file(&config_file(workspace, "config.toml"), "a = 1");
        trust.untrust(workspace).unwrap();
        assert!(trust.workspace_restricted(workspace));

        trust.trust(workspace).unwrap();
        assert!(!trust.workspace_restricted(workspace));

        write_file(&config_file(workspace, "config.toml"), "a = 2");
        trust.inner.lock().remove(workspace);
        assert!(trust.workspace_restricted(workspace));
    }

    #[test]
    fn set_config_invalidates_cache_so_stale_is_detected() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write_file(&config_file(workspace, "config.toml"), "a = 1");

        let trust = trust_in(&store, Config::default());
        trust.trust(workspace).unwrap();
        assert_eq!(trust.status(workspace), TrustStatus::Trusted);

        write_file(&config_file(workspace, "config.toml"), "a = 2");
        assert_eq!(trust.status(workspace), TrustStatus::Trusted);

        trust.set_config(Config::default());
        assert_eq!(trust.status(workspace), TrustStatus::Stale);
    }

    #[test]
    fn grants_persist_across_instances() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write_file(&config_file(workspace, "config.toml"), "a = 1");

        trust_in(&store, Config::default())
            .trust(workspace)
            .unwrap();
        let fresh = trust_in(&store, Config::default());
        assert_eq!(
            fresh.query(workspace, TrustQuery::LocalConfig),
            TrustStatus::Trusted
        );

        fresh.untrust(workspace).unwrap();
        let fresh = trust_in(&store, Config::default());
        assert_eq!(
            fresh.query(workspace, TrustQuery::LocalConfig),
            TrustStatus::Untrusted
        );
    }

    #[test]
    fn stale_grant_keeps_servers_but_not_local_config() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write_file(&config_file(workspace, "config.toml"), "a = 1");

        let trust = trust_in(
            &store,
            Config {
                level: ImplicitTrustLevel::None,
                ..Config::default()
            },
        );
        trust.trust(workspace).unwrap();
        write_file(&config_file(workspace, "config.toml"), "a = 2");
        trust.inner.lock().remove(workspace);

        assert_eq!(trust.status(workspace), TrustStatus::Stale);
        assert!(trust.query(workspace, TrustQuery::Lsp).is_trusted());
        assert_eq!(
            trust.query(workspace, TrustQuery::LocalConfig),
            TrustStatus::Untrusted
        );
        assert_eq!(
            trust.query(workspace, TrustQuery::Dap),
            TrustStatus::Untrusted
        );
    }

    #[cfg(unix)]
    #[test]
    fn hash_includes_symlinked_config_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let external = dir.path().join("external_config.toml");
        write_file(&external, "a = 1");

        fs::create_dir_all(workspace.join(WORKSPACE_CONFIG_DIR)).unwrap();
        symlink(&external, config_file(workspace, "config.toml")).unwrap();

        let h1 = compute_workspace_hash(workspace).expect("symlink should be hashed");
        write_file(&external, "a = 2");
        let h2 = compute_workspace_hash(workspace).expect("symlink should be hashed");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_distinguishes_file_split() {
        let dir1 = tempfile::tempdir().unwrap();
        let split = dir1.path();
        write_file(&config_file(split, "foo.toml"), "a");
        write_file(&config_file(split, "bar.toml"), "b");

        let dir2 = tempfile::tempdir().unwrap();
        let merged = dir2.path();
        write_file(&config_file(merged, "foo.toml"), "a\0bar.toml\0b");

        assert_ne!(
            compute_workspace_hash(split),
            compute_workspace_hash(merged)
        );
    }

    #[test]
    fn level_insecure_does_not_bypass_excluded() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();

        trust_in(&store, Config::default())
            .exclude(workspace)
            .unwrap();

        let trust = trust_in(
            &store,
            Config {
                level: ImplicitTrustLevel::Insecure,
                ..Config::default()
            },
        );
        assert_eq!(
            trust.query(workspace, TrustQuery::Lsp),
            TrustStatus::Excluded
        );
        assert_eq!(
            trust.query(workspace, TrustQuery::LocalConfig),
            TrustStatus::Excluded
        );
    }

    #[test]
    fn trusted_glob_grants_full_trust_but_exclude_wins() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let trusted = dir.path().join("trusted_proj");
        fs::create_dir_all(&trusted).unwrap();

        let pattern = format!("{}/*", dir.path().display());
        let trust = trust_in(
            &store,
            Config {
                level: ImplicitTrustLevel::None,
                trusted_globs: build_trusted_globs(&[pattern]),
                ..Config::default()
            },
        );

        assert_eq!(
            trust.query(&trusted, TrustQuery::LocalConfig),
            TrustStatus::Trusted
        );
        assert_eq!(trust.query(&trusted, TrustQuery::Git), TrustStatus::Trusted);
        assert!(!trust.workspace_restricted(&trusted));

        // `*` does not cross a separator.
        let nested = trusted.join("inner");
        assert_eq!(
            trust.query(&nested, TrustQuery::LocalConfig),
            TrustStatus::Untrusted
        );

        let outside = tempfile::tempdir().unwrap();
        assert_eq!(
            trust.query(outside.path(), TrustQuery::LocalConfig),
            TrustStatus::Untrusted
        );

        trust.exclude(&trusted).unwrap();
        assert_eq!(
            trust.query(&trusted, TrustQuery::LocalConfig),
            TrustStatus::Excluded
        );
        assert_eq!(
            trust.query(&trusted, TrustQuery::Lsp),
            TrustStatus::Excluded
        );
    }

    #[test]
    fn servers_level_trusts_servers_only() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let trust = trust_in(&store, Config::default());

        assert!(trust.query(workspace, TrustQuery::Lsp).is_trusted());
        assert!(trust.query(workspace, TrustQuery::Dap).is_trusted());
        assert!(!trust.query(workspace, TrustQuery::LocalConfig).is_trusted());
        assert!(!trust.query(workspace, TrustQuery::Git).is_trusted());
        // Nothing to unlock: no local config, and servers are already trusted.
        assert!(!trust.restricted_for_doc(workspace, true));

        let strict = trust_in(
            &store,
            Config {
                level: ImplicitTrustLevel::None,
                ..Config::default()
            },
        );
        assert!(strict.restricted_for_doc(workspace, true));
        assert!(!strict.restricted_for_doc(workspace, false));
    }

    #[test]
    fn workspace_restricted_detects_stale() {
        let store = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        write_file(&config_file(workspace, "config.toml"), "a = 1");

        let trust = trust_in(&store, Config::default());
        trust.trust(workspace).unwrap();
        assert!(!trust.workspace_restricted(workspace));

        write_file(&config_file(workspace, "config.toml"), "a = 2");
        trust.inner.lock().remove(workspace);

        assert_eq!(trust.status(workspace), TrustStatus::Stale);
        assert!(trust.workspace_restricted(workspace));
    }
}
