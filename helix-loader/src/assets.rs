use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_store::Store;

pub use helix_store::{
    ActivePackage, RuntimeAsset, RuntimeAssetKind, RuntimeAssetSpec, RuntimeSnapshot,
};

#[cfg(target_os = "macos")]
pub(crate) const DYLIB_EXTENSION: &str = "dylib";

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) const DYLIB_EXTENSION: &str = "so";

#[cfg(windows)]
pub(crate) const DYLIB_EXTENSION: &str = "dll";

#[cfg(target_arch = "wasm32")]
pub(crate) const DYLIB_EXTENSION: &str = "wasm";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Explicit,
    Managed { package: ActivePackage },
    Path,
    RuntimeOverride { root: PathBuf },
    BundledRuntime { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    /// Runtime snapshot generation used to make this resolution decision.
    pub generation: u64,
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
    pub default_args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// Runtime snapshot generation used to make this resolution decision.
    pub generation: u64,
    pub path: PathBuf,
    pub origin: Origin,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeAssetsError {
    #[error(transparent)]
    Store(#[from] helix_store::Error),
    #[error("managed {kind} asset '{key}' from {package:?} is broken at {path}")]
    BrokenManaged {
        kind: RuntimeAssetKind,
        key: String,
        package: Box<ActivePackage>,
        path: PathBuf,
    },
    #[error("invalid logical runtime path {0}")]
    InvalidLogicalPath(PathBuf),
    #[error("runtime file not found: {0}")]
    MissingFile(PathBuf),
}

pub type Result<T> = std::result::Result<T, RuntimeAssetsError>;

/// Stable identity of one logical asset within a runtime snapshot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeAssetKey {
    pub kind: RuntimeAssetKind,
    pub key: String,
}

impl RuntimeAssetKey {
    #[must_use]
    pub fn new(kind: RuntimeAssetKind, key: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
        }
    }
}

impl From<&RuntimeAsset> for RuntimeAssetKey {
    fn from(asset: &RuntimeAsset) -> Self {
        Self::new(asset.kind, asset.key.clone())
    }
}

/// Assets and package domains affected by one accepted snapshot publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssetsChange {
    pub previous_generation: u64,
    pub generation: u64,
    pub changed_asset_keys: BTreeSet<RuntimeAssetKey>,
    pub changed_package_kinds: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssetsSnapshot {
    managed: RuntimeSnapshot,
    managed_index: BTreeMap<RuntimeAssetKind, BTreeMap<String, usize>>,
    runtime_overrides: Vec<PathBuf>,
    bundled_runtime: Vec<PathBuf>,
    search_path: Option<OsString>,
}

impl RuntimeAssetsSnapshot {
    #[must_use]
    pub fn new(
        managed: RuntimeSnapshot,
        runtime_overrides: Vec<PathBuf>,
        bundled_runtime: Vec<PathBuf>,
    ) -> Self {
        let mut managed_index = BTreeMap::<RuntimeAssetKind, BTreeMap<String, usize>>::new();
        for (index, asset) in managed.assets.iter().enumerate() {
            managed_index
                .entry(asset.kind)
                .or_default()
                .entry(asset.key.clone())
                .or_insert(index);
        }
        Self {
            managed,
            managed_index,
            runtime_overrides,
            bundled_runtime,
            search_path: std::env::var_os("PATH"),
        }
    }

    #[must_use]
    pub fn with_search_path(mut self, search_path: Option<OsString>) -> Self {
        self.search_path = search_path;
        self
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.managed.generation
    }

    #[must_use]
    pub fn runtime_overrides(&self) -> &[PathBuf] {
        &self.runtime_overrides
    }

    #[must_use]
    pub fn bundled_runtime(&self) -> &[PathBuf] {
        &self.bundled_runtime
    }

    pub fn resolve_command(&self, command: &str) -> Result<Option<ResolvedLaunch>> {
        let explicit = Path::new(command);
        if is_explicit_command(explicit) {
            return Ok(explicit.is_file().then(|| ResolvedLaunch {
                generation: self.generation(),
                program: explicit.to_path_buf(),
                prefix_args: Vec::new(),
                default_args: Vec::new(),
                env: BTreeMap::new(),
                origin: Origin::Explicit,
            }));
        }

        if let Some(asset) = managed_asset(self, RuntimeAssetKind::Command, command) {
            ensure_managed_path(asset, ManagedPathType::File)?;
            return Ok(Some(ResolvedLaunch {
                generation: self.generation(),
                program: asset.path.clone(),
                prefix_args: asset.prefix_args.clone(),
                default_args: asset.default_args.clone(),
                env: asset.env.clone(),
                origin: Origin::Managed {
                    package: asset.package.clone(),
                },
            }));
        }

        let program = which::which_in(
            command,
            self.search_path.as_ref(),
            helix_stdx::env::current_working_dir(),
        )
        .ok();
        Ok(program.map(|program| ResolvedLaunch {
            generation: self.generation(),
            program,
            prefix_args: Vec::new(),
            default_args: Vec::new(),
            env: BTreeMap::new(),
            origin: Origin::Path,
        }))
    }

    pub fn resolve_file(&self, logical_path: impl AsRef<Path>) -> Result<Option<ResolvedPath>> {
        self.resolve_file_with(logical_path, |_| true)
    }

    pub fn resolve_file_with(
        &self,
        logical_path: impl AsRef<Path>,
        accept: impl FnMut(&Path) -> bool,
    ) -> Result<Option<ResolvedPath>> {
        let key = logical_path_key(logical_path.as_ref())?;
        resolve_file_tiers(self, &key, RuntimeAssetKind::File, &key, accept)
    }

    pub fn file_keys_in(&self, logical_dir: impl AsRef<Path>) -> Result<Vec<String>> {
        let logical_dir = logical_path_key(logical_dir.as_ref())?;
        let mut keys = BTreeSet::new();

        for root in self.runtime_overrides.iter().chain(&self.bundled_runtime) {
            let Ok(entries) = std::fs::read_dir(root.join(&logical_dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                keys.insert(format!("{logical_dir}/{name}"));
            }
        }

        let prefix = format!("{logical_dir}/");
        keys.extend(
            self.managed
                .assets
                .iter()
                .filter(|asset| asset.kind == RuntimeAssetKind::File)
                .map(|asset| asset.key.as_str())
                .filter(|key| {
                    key.strip_prefix(&prefix)
                        .is_some_and(|relative| !relative.is_empty() && !relative.contains('/'))
                })
                .map(str::to_owned),
        );

        Ok(keys.into_iter().collect())
    }

    pub fn require_file(&self, logical_path: impl AsRef<Path>) -> Result<ResolvedPath> {
        let logical_path = logical_path.as_ref();
        self.resolve_file(logical_path)?
            .ok_or_else(|| RuntimeAssetsError::MissingFile(logical_path.to_path_buf()))
    }

    pub fn resolve_grammar(&self, name: &str) -> Result<Option<ResolvedPath>> {
        let grammar_key = logical_path_key(Path::new(name))?;
        if grammar_key != name {
            return Err(RuntimeAssetsError::InvalidLogicalPath(PathBuf::from(name)));
        }
        resolve_file_tiers(
            self,
            &format!("grammars/{name}.{DYLIB_EXTENSION}"),
            RuntimeAssetKind::Grammar,
            name,
            |_| true,
        )
    }

    pub fn resolve_plugin_root(&self, name: &str) -> Result<Option<ResolvedPath>> {
        let Some(asset) = managed_asset(self, RuntimeAssetKind::PluginRoot, name) else {
            return Ok(None);
        };
        ensure_managed_path(asset, ManagedPathType::Directory)?;
        Ok(Some(ResolvedPath {
            generation: self.generation(),
            path: asset.path.clone(),
            origin: Origin::Managed {
                package: asset.package.clone(),
            },
        }))
    }

    #[must_use]
    pub fn plugin_roots(&self) -> Vec<PathBuf> {
        self.managed
            .assets
            .iter()
            .filter(|asset| asset.kind == RuntimeAssetKind::PluginRoot)
            .filter_map(|asset| {
                if let Err(error) = ensure_managed_path(asset, ManagedPathType::Directory) {
                    log::warn!("skipping broken managed plugin root: {error}");
                    return None;
                }
                Some(asset.path.clone())
            })
            .collect()
    }

    #[must_use]
    pub fn active_packages(&self) -> Vec<ActivePackage> {
        self.managed
            .assets
            .iter()
            .map(|asset| asset.package.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn command_keys(&self) -> Vec<String> {
        self.managed
            .assets
            .iter()
            .filter(|asset| asset.kind == RuntimeAssetKind::Command)
            .map(|asset| asset.key.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn command_keys_for_package(&self, package: &ActivePackage) -> Vec<String> {
        self.managed
            .assets
            .iter()
            .filter(|asset| asset.kind == RuntimeAssetKind::Command && &asset.package == package)
            .map(|asset| asset.key.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

impl From<RuntimeSnapshot> for RuntimeAssetsSnapshot {
    fn from(managed: RuntimeSnapshot) -> Self {
        Self::new(managed, Vec::new(), Vec::new())
    }
}

/// Filesystem resolver backed by atomically published immutable activation snapshots.
///
/// Resolution only reads the current in-memory snapshot. Persistence access is kept at the
/// explicit bootstrap and snapshot-loading boundary.
#[derive(Clone)]
pub struct RuntimeAssets {
    snapshot: Arc<ArcSwap<RuntimeAssetsSnapshot>>,
}

impl std::fmt::Debug for RuntimeAssets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeAssets")
            .field("generation", &self.snapshot.load().managed.generation)
            .finish_non_exhaustive()
    }
}

impl RuntimeAssets {
    /// Creates a resolver from an explicit immutable snapshot.
    ///
    /// This is the test seam for deterministic runtime roots and `PATH` contents.
    #[must_use]
    pub fn from_snapshot(snapshot: impl Into<RuntimeAssetsSnapshot>) -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot.into())),
        }
    }

    /// Constructs a resolver from one snapshot loaded from the default state store.
    ///
    /// # Errors
    ///
    /// Returns an error if local product paths or SQLite state cannot be read.
    pub fn open_default() -> Result<Self> {
        bootstrap_runtime_assets()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.snapshot.load().generation()
    }

    /// Resolves a command using explicit path, managed activation, then `PATH` precedence.
    ///
    /// An active managed command whose program is missing returns
    /// [`RuntimeAssetsError::BrokenManaged`] and never falls through to `PATH`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected managed activation is broken.
    pub fn resolve_command(&self, command: &str) -> Result<Option<ResolvedLaunch>> {
        self.snapshot().resolve_command(command)
    }

    /// Resolves a logical runtime file using overrides, managed assets, then bundled runtime.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid logical path or a broken selected managed asset.
    pub fn resolve_file(&self, logical_path: impl AsRef<Path>) -> Result<Option<ResolvedPath>> {
        self.resolve_file_with(logical_path, |_| true)
    }

    /// Resolves the first logical runtime file accepted by `accept` while preserving tier order.
    ///
    /// Rejected paths are skipped before managed-path validation. This allows consumers such as
    /// inherited theme loading to continue below an already visited higher-priority candidate.
    pub fn resolve_file_with(
        &self,
        logical_path: impl AsRef<Path>,
        accept: impl FnMut(&Path) -> bool,
    ) -> Result<Option<ResolvedPath>> {
        self.snapshot().resolve_file_with(logical_path, accept)
    }

    /// Returns direct logical file keys in `logical_dir` across the current runtime snapshot.
    ///
    /// Keys are deduplicated and sorted. Optional filesystem roots that do not exist are skipped;
    /// managed entries are returned by logical identity and validated when resolved.
    pub fn file_keys_in(&self, logical_dir: impl AsRef<Path>) -> Result<Vec<String>> {
        self.snapshot().file_keys_in(logical_dir)
    }

    /// Resolves a logical runtime file and reports absence as a typed error.
    pub fn require_file(&self, logical_path: impl AsRef<Path>) -> Result<ResolvedPath> {
        self.snapshot().require_file(logical_path)
    }

    /// Resolves a grammar library using runtime overrides, the active grammar package, then the
    /// bundled runtime.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid grammar name or a broken active grammar package.
    pub fn resolve_grammar(&self, name: &str) -> Result<Option<ResolvedPath>> {
        self.snapshot().resolve_grammar(name)
    }

    /// Resolves one active plugin root by package name.
    ///
    /// Unlike [`Self::plugin_roots`], this keeps package capability checks focused and reports a
    /// broken selected root without scanning unrelated plugins.
    pub fn resolve_plugin_root(&self, name: &str) -> Result<Option<ResolvedPath>> {
        self.snapshot().resolve_plugin_root(name)
    }

    /// Returns valid active plugin roots in stable asset-key order.
    ///
    /// Broken roots are reported and isolated so one corrupt package cannot disable unrelated
    /// plugins. Use [`Self::resolve_plugin_root`] when the caller needs the focused error.
    #[must_use]
    pub fn plugin_roots(&self) -> Vec<PathBuf> {
        self.snapshot().plugin_roots()
    }

    /// Returns the packages represented by the current active runtime snapshot.
    #[must_use]
    pub fn active_packages(&self) -> Vec<ActivePackage> {
        self.snapshot().active_packages()
    }

    /// Returns the logical names of commands in the current managed activation.
    ///
    /// These keys are the same names accepted by [`Self::resolve_command`], so command
    /// discovery and launch resolution observe one immutable generation.
    #[must_use]
    pub fn command_keys(&self) -> Vec<String> {
        self.snapshot().command_keys()
    }

    /// Returns the command keys owned by one exact active package version.
    #[must_use]
    pub fn command_keys_for_package(&self, package: &ActivePackage) -> Vec<String> {
        self.snapshot().command_keys_for_package(package)
    }

    /// Reloads persisted activation state and atomically publishes it when newer.
    ///
    /// This performs SQLite I/O and must run on a blocking worker outside latency-sensitive UI
    /// paths.
    pub fn refresh(&self) -> Result<Option<RuntimeAssetsChange>> {
        Ok(self.publish_if_newer(load_runtime_snapshot()?))
    }

    /// Atomically publishes `managed` if its generation is newer than the current snapshot.
    ///
    /// Stale and duplicate generations return `None`. Concurrent publishers use compare-and-swap
    /// so a lower generation can never replace a higher generation. Existing readers retain their
    /// old `Arc` and complete against one coherent generation.
    pub fn publish_if_newer(&self, managed: RuntimeSnapshot) -> Option<RuntimeAssetsChange> {
        loop {
            let current = self.snapshot.load_full();
            if managed.generation <= current.managed.generation {
                return None;
            }

            let change = RuntimeAssetsChange::between(&current.managed, &managed);
            let replacement = Arc::new(
                RuntimeAssetsSnapshot::new(
                    managed.clone(),
                    current.runtime_overrides.clone(),
                    current.bundled_runtime.clone(),
                )
                .with_search_path(current.search_path.clone()),
            );
            let previous =
                arc_swap::Guard::into_inner(self.snapshot.compare_and_swap(&current, replacement));
            if Arc::ptr_eq(&previous, &current) {
                return Some(change);
            }
        }
    }

    /// Returns the current immutable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<RuntimeAssetsSnapshot> {
        self.snapshot.load_full()
    }
}

impl RuntimeAssetsChange {
    fn between(previous: &RuntimeSnapshot, current: &RuntimeSnapshot) -> Self {
        let previous_assets = assets_by_key(previous);
        let current_assets = assets_by_key(current);
        let keys = previous_assets
            .keys()
            .chain(current_assets.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changed_asset_keys = BTreeSet::new();
        let mut changed_package_kinds = BTreeSet::new();

        for key in keys {
            let previous_asset = previous_assets.get(&key).copied();
            let current_asset = current_assets.get(&key).copied();
            if previous_asset == current_asset {
                continue;
            }

            changed_asset_keys.insert(key);
            if let Some(asset) = previous_asset {
                changed_package_kinds.insert(asset.package.kind.clone());
            }
            if let Some(asset) = current_asset {
                changed_package_kinds.insert(asset.package.kind.clone());
            }
        }

        Self {
            previous_generation: previous.generation,
            generation: current.generation,
            changed_asset_keys,
            changed_package_kinds,
        }
    }
}

fn assets_by_key(snapshot: &RuntimeSnapshot) -> BTreeMap<RuntimeAssetKey, &RuntimeAsset> {
    snapshot
        .assets
        .iter()
        .map(|asset| (RuntimeAssetKey::from(asset), asset))
        .collect()
}

/// Loads one coherent runtime activation snapshot from the default state store.
///
/// # Errors
///
/// Returns an error if local product paths or SQLite state cannot be read.
fn load_runtime_snapshot() -> Result<RuntimeSnapshot> {
    let mut store = Store::open_default()?;
    Ok(store.runtime_assets().snapshot()?)
}

/// Constructs the default in-memory runtime resolver from one persisted snapshot.
///
/// The state store is dropped before the resolver is returned.
///
/// # Errors
///
/// Returns an error if the initial runtime snapshot cannot be loaded.
pub fn bootstrap_runtime_assets() -> Result<RuntimeAssets> {
    let managed = load_runtime_snapshot()?;
    Ok(RuntimeAssets::from_snapshot(RuntimeAssetsSnapshot::new(
        managed,
        crate::runtime_override_dirs().to_vec(),
        crate::bundled_runtime_dirs().to_vec(),
    )))
}

static DEFAULT_RUNTIME_ASSETS: once_cell::sync::OnceCell<RuntimeAssets> =
    once_cell::sync::OnceCell::new();

/// Returns the process-wide default runtime resolver.
///
/// # Errors
///
/// Returns an error if the default resolver cannot open local state.
pub fn runtime_assets() -> Result<&'static RuntimeAssets> {
    DEFAULT_RUNTIME_ASSETS.get_or_try_init(bootstrap_runtime_assets)
}

/// Returns the process-wide resolver only when another startup path has initialized it.
///
/// Latency-sensitive UI discovery should use this accessor so it never opens persistent state on
/// the input thread. Bootstrap remains the responsibility of application startup and package work.
#[must_use]
pub fn runtime_assets_if_initialized() -> Option<&'static RuntimeAssets> {
    DEFAULT_RUNTIME_ASSETS.get()
}

fn resolve_file_tiers(
    snapshot: &RuntimeAssetsSnapshot,
    logical_path: &str,
    managed_kind: RuntimeAssetKind,
    managed_key: &str,
    mut accept: impl FnMut(&Path) -> bool,
) -> Result<Option<ResolvedPath>> {
    for root in &snapshot.runtime_overrides {
        let path = root.join(logical_path);
        if accept(&path) && path.is_file() {
            return Ok(Some(ResolvedPath {
                generation: snapshot.managed.generation,
                path,
                origin: Origin::RuntimeOverride { root: root.clone() },
            }));
        }
    }

    if let Some(asset) = managed_asset(snapshot, managed_kind, managed_key) {
        if accept(&asset.path) {
            ensure_managed_path(asset, ManagedPathType::File)?;
            return Ok(Some(ResolvedPath {
                generation: snapshot.managed.generation,
                path: asset.path.clone(),
                origin: Origin::Managed {
                    package: asset.package.clone(),
                },
            }));
        }
    }

    for root in &snapshot.bundled_runtime {
        let path = root.join(logical_path);
        if accept(&path) && path.is_file() {
            return Ok(Some(ResolvedPath {
                generation: snapshot.managed.generation,
                path,
                origin: Origin::BundledRuntime { root: root.clone() },
            }));
        }
    }
    Ok(None)
}

fn managed_asset<'a>(
    snapshot: &'a RuntimeAssetsSnapshot,
    kind: RuntimeAssetKind,
    key: &str,
) -> Option<&'a RuntimeAsset> {
    snapshot
        .managed_index
        .get(&kind)?
        .get(key)
        .and_then(|index| snapshot.managed.assets.get(*index))
}

#[derive(Clone, Copy)]
enum ManagedPathType {
    File,
    Directory,
}

fn ensure_managed_path(asset: &RuntimeAsset, path_type: ManagedPathType) -> Result<()> {
    let valid = match path_type {
        ManagedPathType::File => asset.path.is_file(),
        ManagedPathType::Directory => asset.path.is_dir(),
    };
    if valid {
        return Ok(());
    }
    Err(RuntimeAssetsError::BrokenManaged {
        kind: asset.kind,
        key: asset.key.clone(),
        package: Box::new(asset.package.clone()),
        path: asset.path.clone(),
    })
}

fn is_explicit_command(command: &Path) -> bool {
    command.is_absolute() || command.components().count() > 1
}

fn logical_path_key(path: &Path) -> Result<String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                let Some(segment) = segment.to_str() else {
                    return Err(RuntimeAssetsError::InvalidLogicalPath(path.to_path_buf()));
                };
                segments.push(segment);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RuntimeAssetsError::InvalidLogicalPath(path.to_path_buf()));
            }
        }
    }
    if segments.is_empty() {
        return Err(RuntimeAssetsError::InvalidLogicalPath(path.to_path_buf()));
    }
    Ok(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    #[test]
    fn command_and_file_precedence_preserves_launch_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let explicit = temp.path().join(executable_name("explicit"));
        let managed = temp.path().join(executable_name("managed"));
        let path_dir = temp.path().join("path");
        let path_program = path_dir.join(executable_name("path-only"));
        let override_root = temp.path().join("override");
        let managed_file = temp.path().join("managed-query.scm");
        let plugin_root = temp.path().join("managed-plugin");
        let bundled_root = temp.path().join("bundled");
        fs::create_dir_all(&path_dir).unwrap();
        fs::create_dir_all(override_root.join("queries/demo")).unwrap();
        fs::create_dir_all(&plugin_root).unwrap();
        fs::create_dir_all(bundled_root.join("queries/demo")).unwrap();
        fs::write(&explicit, b"explicit").unwrap();
        fs::write(&managed, b"managed").unwrap();
        fs::write(&path_program, b"path").unwrap();
        fs::write(override_root.join("queries/demo/test.scm"), b"override").unwrap();
        fs::write(&managed_file, b"managed file").unwrap();
        fs::write(bundled_root.join("queries/demo/test.scm"), b"bundled").unwrap();
        fs::write(
            bundled_root.join("queries/demo/bundled-only.scm"),
            b"bundled only",
        )
        .unwrap();

        let package = ActivePackage::new("lsp", "managed", "1");
        let command = RuntimeAsset::from_spec(
            package.clone(),
            RuntimeAssetSpec::command("managed", &managed)
                .with_prefix_args(["run".to_owned()])
                .with_default_args(["--stdio".to_owned()])
                .with_env([("MANAGED".to_owned(), "1".to_owned())]),
        );
        let file = RuntimeAsset::from_spec(
            package.clone(),
            RuntimeAssetSpec::file("queries/demo/test.scm", &managed_file),
        );
        let plugin = RuntimeAsset::from_spec(
            package.clone(),
            RuntimeAssetSpec::plugin_root("managed", &plugin_root),
        );
        let snapshot = RuntimeAssetsSnapshot::new(
            RuntimeSnapshot {
                generation: 1,
                assets: vec![command, file, plugin],
            },
            vec![override_root.clone()],
            vec![bundled_root],
        )
        .with_search_path(Some(std::env::join_paths([path_dir]).unwrap()));
        let assets = RuntimeAssets::from_snapshot(snapshot);

        let launch = assets
            .resolve_command(explicit.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(launch.generation, 1);
        assert_eq!(launch.program, explicit);
        assert_eq!(launch.origin, Origin::Explicit);

        let launch = assets.resolve_command("managed").unwrap().unwrap();
        assert_eq!(launch.generation, 1);
        assert_eq!(launch.program, managed);
        assert_eq!(launch.prefix_args, ["run"]);
        assert_eq!(launch.default_args, ["--stdio"]);
        assert_eq!(launch.env.get("MANAGED").map(String::as_str), Some("1"));
        assert_eq!(
            launch.origin,
            Origin::Managed {
                package: package.clone()
            }
        );

        let launch = assets.resolve_command("path-only").unwrap().unwrap();
        assert_eq!(launch.generation, 1);
        assert_eq!(launch.program, path_program);
        assert_eq!(launch.origin, Origin::Path);

        let file = assets
            .resolve_file("queries/demo/test.scm")
            .unwrap()
            .unwrap();
        assert_eq!(file.generation, 1);
        assert_eq!(file.path, override_root.join("queries/demo/test.scm"));
        assert!(matches!(file.origin, Origin::RuntimeOverride { .. }));

        fs::remove_file(override_root.join("queries/demo/test.scm")).unwrap();
        let file = assets
            .resolve_file("queries/demo/test.scm")
            .unwrap()
            .unwrap();
        assert_eq!(file.generation, 1);
        assert_eq!(file.path, managed_file);
        assert!(matches!(file.origin, Origin::Managed { .. }));

        let file = assets
            .resolve_file("queries/demo/bundled-only.scm")
            .unwrap()
            .unwrap();
        assert_eq!(file.generation, 1);
        assert!(file.path.ends_with("queries/demo/bundled-only.scm"));
        assert!(matches!(file.origin, Origin::BundledRuntime { .. }));
        assert!(matches!(
            assets.require_file("queries/demo/missing.scm"),
            Err(RuntimeAssetsError::MissingFile(path))
                if path == PathBuf::from("queries/demo/missing.scm")
        ));
        assert_eq!(
            assets.file_keys_in("queries/demo").unwrap(),
            [
                "queries/demo/bundled-only.scm".to_owned(),
                "queries/demo/test.scm".to_owned(),
            ]
        );
        let plugin = assets.resolve_plugin_root("managed").unwrap().unwrap();
        assert_eq!(plugin.path, plugin_root);
        assert!(matches!(plugin.origin, Origin::Managed { .. }));
        assert_eq!(assets.plugin_roots(), vec![plugin_root]);
        assert_eq!(assets.active_packages(), vec![package.clone()]);
        assert_eq!(assets.command_keys(), ["managed"]);
        assert_eq!(assets.command_keys_for_package(&package), ["managed"]);
        assert!(assets
            .resolve_command(temp.path().join("missing").to_str().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn broken_managed_assets_never_fall_through() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("path");
        let bundled = temp.path().join("bundled");
        fs::create_dir_all(&path_dir).unwrap();
        fs::create_dir_all(bundled.join("queries/demo")).unwrap();
        fs::write(path_dir.join(executable_name("demo")), b"path").unwrap();
        fs::write(bundled.join("queries/demo/test.scm"), b"bundled").unwrap();
        let package = ActivePackage::new("lsp", "demo", "1");
        let snapshot = RuntimeAssetsSnapshot::new(
            RuntimeSnapshot {
                generation: 1,
                assets: vec![
                    RuntimeAsset::from_spec(
                        package.clone(),
                        RuntimeAssetSpec::command("demo", temp.path().join("missing-command")),
                    ),
                    RuntimeAsset::from_spec(
                        package,
                        RuntimeAssetSpec::file(
                            "queries/demo/test.scm",
                            temp.path().join("missing-file"),
                        ),
                    ),
                ],
            },
            Vec::new(),
            vec![bundled],
        )
        .with_search_path(Some(std::env::join_paths([path_dir]).unwrap()));
        let assets = RuntimeAssets::from_snapshot(snapshot);

        assert!(matches!(
            assets.resolve_command("demo"),
            Err(RuntimeAssetsError::BrokenManaged {
                kind: RuntimeAssetKind::Command,
                ..
            })
        ));
        assert!(matches!(
            assets.resolve_file("queries/demo/test.scm"),
            Err(RuntimeAssetsError::BrokenManaged {
                kind: RuntimeAssetKind::File,
                ..
            })
        ));
    }

    #[test]
    fn broken_plugin_root_is_reported_by_focused_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let healthy_root = temp.path().join("healthy");
        fs::create_dir(&healthy_root).unwrap();
        let package = ActivePackage::new("plugin", "demo", "1");
        let assets = RuntimeAssets::from_snapshot(runtime_snapshot(
            1,
            vec![
                RuntimeAsset::from_spec(
                    package,
                    RuntimeAssetSpec::plugin_root("demo", temp.path().join("missing")),
                ),
                RuntimeAsset::from_spec(
                    ActivePackage::new("plugin", "healthy", "1"),
                    RuntimeAssetSpec::plugin_root("healthy", &healthy_root),
                ),
            ],
        ));

        assert!(matches!(
            assets.resolve_plugin_root("demo"),
            Err(RuntimeAssetsError::BrokenManaged {
                kind: RuntimeAssetKind::PluginRoot,
                ..
            })
        ));
        assert_eq!(assets.plugin_roots(), [healthy_root]);
    }

    #[test]
    fn publication_reports_added_removed_and_modified_assets() {
        let unchanged_file = runtime_asset(
            "query",
            "shared-file",
            "1",
            RuntimeAssetKind::File,
            "shared",
            "unchanged",
        );
        let assets = RuntimeAssets::from_snapshot(runtime_snapshot(
            7,
            vec![
                runtime_asset(
                    "lsp",
                    "shared-command",
                    "1",
                    RuntimeAssetKind::Command,
                    "shared",
                    "old-command",
                ),
                unchanged_file.clone(),
                runtime_asset(
                    "theme",
                    "removed",
                    "1",
                    RuntimeAssetKind::File,
                    "removed",
                    "removed-file",
                ),
                runtime_asset(
                    "plugin",
                    "changed",
                    "1",
                    RuntimeAssetKind::PluginRoot,
                    "changed",
                    "plugin-root",
                ),
            ],
        ));

        let change = assets
            .publish_if_newer(runtime_snapshot(
                9,
                vec![
                    runtime_asset(
                        "extension",
                        "changed",
                        "2",
                        RuntimeAssetKind::PluginRoot,
                        "changed",
                        "plugin-root",
                    ),
                    runtime_asset(
                        "grammar",
                        "added",
                        "1",
                        RuntimeAssetKind::Grammar,
                        "added",
                        "added-grammar",
                    ),
                    unchanged_file,
                    runtime_asset(
                        "lsp",
                        "shared-command",
                        "2",
                        RuntimeAssetKind::Command,
                        "shared",
                        "new-command",
                    ),
                ],
            ))
            .unwrap();

        assert_eq!(change.previous_generation, 7);
        assert_eq!(change.generation, 9);
        assert_eq!(
            change.changed_asset_keys,
            [
                RuntimeAssetKey::new(RuntimeAssetKind::Command, "shared"),
                RuntimeAssetKey::new(RuntimeAssetKind::File, "removed"),
                RuntimeAssetKey::new(RuntimeAssetKind::Grammar, "added"),
                RuntimeAssetKey::new(RuntimeAssetKind::PluginRoot, "changed"),
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            change.changed_package_kinds,
            ["extension", "grammar", "lsp", "plugin", "theme"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn publication_rejects_duplicate_stale_and_late_generations() {
        let assets = RuntimeAssets::from_snapshot(runtime_snapshot(10, Vec::new()));

        assert!(assets
            .publish_if_newer(runtime_snapshot(
                10,
                vec![runtime_asset(
                    "lsp",
                    "duplicate",
                    "1",
                    RuntimeAssetKind::Command,
                    "duplicate",
                    "duplicate",
                )],
            ))
            .is_none());
        assert!(assets
            .publish_if_newer(runtime_snapshot(9, Vec::new()))
            .is_none());

        let change = assets
            .publish_if_newer(runtime_snapshot(12, Vec::new()))
            .unwrap();
        assert_eq!(change.previous_generation, 10);
        assert_eq!(change.generation, 12);
        assert!(change.changed_asset_keys.is_empty());
        assert!(change.changed_package_kinds.is_empty());

        assert!(assets
            .publish_if_newer(runtime_snapshot(
                11,
                vec![runtime_asset(
                    "lsp",
                    "late",
                    "1",
                    RuntimeAssetKind::Command,
                    "late",
                    "late",
                )],
            ))
            .is_none());
        assert_eq!(assets.snapshot().managed.generation, 12);
        assert!(assets.snapshot().managed.assets.is_empty());
    }

    #[test]
    fn resolved_identities_pin_their_snapshot_generation() {
        let temp = tempfile::tempdir().unwrap();
        let old_program = temp.path().join(executable_name("old-command"));
        let new_program = temp.path().join(executable_name("new-command"));
        let old_file = temp.path().join("old-query.scm");
        let new_file = temp.path().join("new-query.scm");
        fs::write(&old_program, b"old").unwrap();
        fs::write(&new_program, b"new").unwrap();
        fs::write(&old_file, b"old").unwrap();
        fs::write(&new_file, b"new").unwrap();
        let override_root = temp.path().join("override");
        let bundled_root = temp.path().join("bundled");
        let search_path = OsString::from("pinned-search-path");
        let assets = RuntimeAssets::from_snapshot(
            RuntimeAssetsSnapshot::new(
                runtime_snapshot(
                    41,
                    vec![
                        runtime_asset(
                            "lsp",
                            "demo",
                            "1",
                            RuntimeAssetKind::Command,
                            "demo",
                            &old_program,
                        ),
                        runtime_asset(
                            "query",
                            "demo",
                            "1",
                            RuntimeAssetKind::File,
                            "queries/demo/test.scm",
                            &old_file,
                        ),
                    ],
                ),
                vec![override_root.clone()],
                vec![bundled_root.clone()],
            )
            .with_search_path(Some(search_path.clone())),
        );

        let old_launch = assets.resolve_command("demo").unwrap().unwrap();
        let old_resolved_file = assets
            .resolve_file("queries/demo/test.scm")
            .unwrap()
            .unwrap();
        let held_old = assets.snapshot();

        assets
            .publish_if_newer(runtime_snapshot(
                42,
                vec![
                    runtime_asset(
                        "lsp",
                        "demo",
                        "2",
                        RuntimeAssetKind::Command,
                        "demo",
                        &new_program,
                    ),
                    runtime_asset(
                        "query",
                        "demo",
                        "2",
                        RuntimeAssetKind::File,
                        "queries/demo/test.scm",
                        &new_file,
                    ),
                ],
            ))
            .unwrap();

        let new_launch = assets.resolve_command("demo").unwrap().unwrap();
        let new_resolved_file = assets
            .resolve_file("queries/demo/test.scm")
            .unwrap()
            .unwrap();
        let held_launch = held_old.resolve_command("demo").unwrap().unwrap();
        let held_resolved_file = held_old
            .resolve_file("queries/demo/test.scm")
            .unwrap()
            .unwrap();

        assert_eq!(
            (old_launch.generation, old_launch.program),
            (41, old_program.clone())
        );
        assert_eq!(
            (old_resolved_file.generation, old_resolved_file.path),
            (41, old_file.clone())
        );
        assert_eq!(
            (new_launch.generation, new_launch.program),
            (42, new_program)
        );
        assert_eq!(
            (new_resolved_file.generation, new_resolved_file.path),
            (42, new_file)
        );
        assert_eq!(held_launch.generation, 41);
        assert_eq!(held_launch.program, old_program);
        assert_eq!(held_resolved_file.generation, 41);
        assert_eq!(held_resolved_file.path, old_file);
        assert_eq!(held_old.managed.generation, 41);
        let current = assets.snapshot();
        assert_eq!(current.managed.generation, 42);
        assert_eq!(current.runtime_overrides, [override_root]);
        assert_eq!(current.bundled_runtime, [bundled_root]);
        assert_eq!(current.search_path, Some(search_path));
    }

    #[test]
    fn readers_observe_coherent_snapshots_and_held_arcs_stay_pinned() {
        let resolver = Arc::new(RuntimeAssets::from_snapshot(generation_snapshot(1)));
        let held_old = resolver.snapshot();

        let running = Arc::new(AtomicBool::new(true));
        let readers = (0..4)
            .map(|_| {
                let resolver = Arc::clone(&resolver);
                let running = Arc::clone(&running);
                thread::spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        let snapshot = resolver.snapshot();
                        let generation = snapshot.managed.generation;
                        let asset = &snapshot.managed.assets[0];
                        assert_eq!(asset.package.version, generation.to_string());
                        assert_eq!(asset.path, PathBuf::from(format!("command-{generation}")));
                    }
                })
            })
            .collect::<Vec<_>>();

        for generation in 2..=64 {
            resolver
                .publish_if_newer(generation_snapshot(generation))
                .unwrap();
        }
        running.store(false, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }

        assert_eq!(held_old.managed.generation, 1);
        assert_eq!(held_old.managed.assets[0].package.version, "1");
        let current = resolver.snapshot();
        assert_eq!(current.managed.generation, 64);
        assert_eq!(current.managed.assets[0].package.version, "64");
    }

    #[test]
    fn concurrent_publishers_never_regress_generation() {
        const LAST_GENERATION: u64 = 48;
        let assets = Arc::new(RuntimeAssets::from_snapshot(runtime_snapshot(
            1,
            Vec::new(),
        )));
        let barrier = Arc::new(Barrier::new(LAST_GENERATION as usize));
        let publishers = (2..=LAST_GENERATION)
            .map(|generation| {
                let assets = Arc::clone(&assets);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    assets.publish_if_newer(generation_snapshot(generation));
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        for publisher in publishers {
            publisher.join().unwrap();
        }

        let current = assets.snapshot();
        assert_eq!(current.managed.generation, LAST_GENERATION);
        assert_eq!(
            current.managed.assets[0].package.version,
            LAST_GENERATION.to_string()
        );
    }

    fn generation_snapshot(generation: u64) -> RuntimeSnapshot {
        runtime_snapshot(
            generation,
            vec![runtime_asset(
                "lsp",
                "demo",
                generation.to_string(),
                RuntimeAssetKind::Command,
                "demo",
                format!("command-{generation}"),
            )],
        )
    }

    fn runtime_snapshot(generation: u64, assets: Vec<RuntimeAsset>) -> RuntimeSnapshot {
        RuntimeSnapshot { generation, assets }
    }

    fn runtime_asset(
        package_kind: &str,
        package_name: &str,
        version: impl Into<String>,
        kind: RuntimeAssetKind,
        key: &str,
        path: impl Into<PathBuf>,
    ) -> RuntimeAsset {
        RuntimeAsset::from_spec(
            ActivePackage::new(package_kind, package_name, version),
            RuntimeAssetSpec::new(kind, key, path),
        )
    }

    fn executable_name(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        }
    }
}
