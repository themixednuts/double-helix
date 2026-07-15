use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use fs4::{FileExt, TryLockError};
use helix_store::{
    ActivePackage, PackageActivation, PackageStateCommit, PkgReceipt as StorePkgReceipt,
    RuntimeAsset, RuntimeAssetKind, RuntimeAssetSpec, RuntimeSnapshot, Store as SqliteStore,
    StorePaths,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{io, spec::PkgKind, Error, Result};

pub(crate) const PKG_RECEIPTS_TOML_IMPORT_MARKER: &str = "pkg-receipts-toml-v1";
const PKG_RUNTIME_ASSETS_IMPORT_MARKER: &str = "pkg-runtime-assets-v2";
const TRANSACTION_SUFFIX: &str = ".transaction.json";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn default_root() -> PathBuf {
        helix_loader::data_dir().join("pkg")
    }

    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn open_default() -> Self {
        Self::open(Self::default_root())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_dir(&self, kind: PkgKind, name: &str, version: &str) -> PathBuf {
        self.root
            .join("store")
            .join(kind.as_str())
            .join(name)
            .join(version)
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.root.join("registries")
    }

    pub fn runtime_dir(&self, name: &str) -> PathBuf {
        self.root.join("runtimes").join(name)
    }

    #[cfg(test)]
    fn receipt_path(&self, kind: PkgKind, name: &str) -> PathBuf {
        self.root
            .join("receipts")
            .join(format!("{}-{name}.toml", kind.as_str()))
    }

    pub fn prepare(&self) -> Result<()> {
        for path in [
            self.root.join("store"),
            self.staging_dir(),
            self.registry_dir(),
            self.runtime_dir("node"),
            self.runtime_dir("py"),
        ] {
            fs::create_dir_all(&path).map_err(|source| io(path.display(), source))?;
        }
        Ok(())
    }

    pub fn receipts(&self) -> Result<Vec<Receipt>> {
        self.recover_pending_transactions()?;
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        let receipts = self.reconcile_receipts(&mut store)?;
        Ok(receipts)
    }

    pub fn receipt(&self, kind: PkgKind, name: &str) -> Result<Option<Receipt>> {
        Ok(self
            .receipts()?
            .into_iter()
            .find(|receipt| receipt.kind == kind && receipt.name == name))
    }

    fn legacy_receipts(&self) -> Result<Vec<Receipt>> {
        let dir = self.root.join("receipts");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut receipts = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|source| io(dir.display(), source))? {
            let path = entry.map_err(|source| io(dir.display(), source))?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                receipts.push(Receipt::read(&path)?);
            }
        }
        receipts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(receipts)
    }

    #[cfg(test)]
    fn write_legacy_receipt(&self, receipt: &Receipt) -> Result<()> {
        let path = self.receipt_path(receipt.kind, &receipt.name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io(parent.display(), source))?;
        }
        fs::write(&path, toml::to_string_pretty(receipt)?)
            .map_err(|source| io(path.display(), source))
    }

    pub(crate) fn receipt_for_mutation(
        &self,
        kind: PkgKind,
        name: &str,
    ) -> Result<Option<Receipt>> {
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        self.recover_transaction_locked(&mut store, kind, name)?;
        let state = store.package_state().get(kind.as_str(), name)?;
        let existing = state.receipt.map(Receipt::try_from).transpose()?;
        let assets = state.assets;
        let Some(package) = package_for_assets(kind, name, &assets)? else {
            if existing.is_some() {
                store
                    .package_state()
                    .reconcile_receipt(kind.as_str(), name, None)?;
            }
            return Ok(None);
        };
        let (receipt, needs_repair) = reconcile_receipt(self, existing, &package, &assets)?;
        if needs_repair {
            store.package_state().reconcile_receipt(
                kind.as_str(),
                name,
                Some(StorePkgReceipt::try_from(&receipt)?),
            )?;
        }
        Ok(Some(receipt))
    }

    pub(crate) fn commit_activation(
        &self,
        receipt: &Receipt,
        activation: PackageActivation,
    ) -> Result<PackageStateCommit> {
        if activation.package.kind != receipt.kind.as_str()
            || activation.package.name != receipt.name
            || activation.package.version != receipt.version
        {
            return Err(Error::Message(format!(
                "activation identity does not match receipt for {} {}",
                receipt.kind, receipt.name
            )));
        }
        validate_activation_paths(&activation)?;

        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        self.recover_transaction_locked(&mut store, receipt.kind, &receipt.name)?;
        let stored_receipt = StorePkgReceipt::try_from(receipt)?;
        let before = store
            .package_state()
            .get(receipt.kind.as_str(), &receipt.name)?;
        let previous_receipt = before.receipt.map(Receipt::try_from).transpose()?;
        let previous_assets = before.assets;
        let desired_assets = materialize_activation(&activation);
        let journal = MutationJournal {
            kind: receipt.kind,
            name: receipt.name.clone(),
            previous_receipt,
            previous_assets,
            desired_receipt: Some(receipt.clone()),
            desired_assets,
        };
        let journal_path = self.write_journal(&journal)?;

        match store.package_state().activate(stored_receipt, activation) {
            Ok(commit) => {
                self.cleanup_journal(&journal_path);
                Ok(commit)
            }
            Err(error) => {
                self.cleanup_journal(&journal_path);
                Err(error.into())
            }
        }
    }

    pub(crate) fn deactivate(&self, kind: PkgKind, name: &str) -> Result<PackageStateCommit> {
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        self.recover_transaction_locked(&mut store, kind, name)?;
        let before = store.package_state().get(kind.as_str(), name)?;
        if before.receipt.is_none() && before.assets.is_empty() {
            return Ok(store.package_state().deactivate(kind.as_str(), name)?);
        }
        let previous_receipt = before.receipt.map(Receipt::try_from).transpose()?;
        let previous_assets = before.assets;

        let journal = MutationJournal {
            kind,
            name: name.to_owned(),
            previous_receipt,
            previous_assets,
            desired_receipt: None,
            desired_assets: Vec::new(),
        };
        let journal_path = self.write_journal(&journal)?;
        match store.package_state().deactivate(kind.as_str(), name) {
            Ok(commit) => {
                self.cleanup_journal(&journal_path);
                Ok(commit)
            }
            Err(error) => {
                self.cleanup_journal(&journal_path);
                Err(error.into())
            }
        }
    }

    pub(crate) fn rollback_target(
        &self,
        kind: PkgKind,
        name: &str,
    ) -> Result<Option<ActivePackage>> {
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        self.recover_transaction_locked(&mut store, kind, name)?;
        let current = package_assets(&store.runtime_assets().snapshot()?, kind, name);
        let history = store.runtime_assets().history(kind.as_str(), name)?;
        Ok(history
            .into_iter()
            .rev()
            .find(|event| {
                event.rolled_back_generation.is_none()
                    && event.activated_assets == current
                    && !event.previous_assets.is_empty()
            })
            .and_then(|event| {
                event
                    .previous_assets
                    .first()
                    .map(|asset| asset.package.clone())
            }))
    }

    pub(crate) fn rollback(&self, receipt: &Receipt) -> Result<PackageStateCommit> {
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        self.recover_transaction_locked(&mut store, receipt.kind, &receipt.name)?;
        let stored_receipt = StorePkgReceipt::try_from(receipt)?;
        let before = store
            .package_state()
            .get(receipt.kind.as_str(), &receipt.name)?;
        let previous_receipt = before.receipt.map(Receipt::try_from).transpose()?;
        let previous_assets = before.assets;
        let history = store
            .runtime_assets()
            .history(receipt.kind.as_str(), &receipt.name)?;
        let event = history
            .into_iter()
            .rev()
            .find(|event| {
                event.rolled_back_generation.is_none()
                    && event.activated_assets == previous_assets
                    && !event.previous_assets.is_empty()
            })
            .ok_or_else(|| {
                Error::Message(format!(
                    "{} has no active package version to rollback",
                    receipt.name
                ))
            })?;
        let desired_assets = event.previous_assets;
        if desired_assets.first().is_none_or(|asset| {
            asset.package.kind != receipt.kind.as_str()
                || asset.package.name != receipt.name
                || asset.package.version != receipt.version
        }) {
            return Err(Error::Message(format!(
                "rollback receipt does not match activation history for {}",
                receipt.name
            )));
        }
        let journal = MutationJournal {
            kind: receipt.kind,
            name: receipt.name.clone(),
            previous_receipt,
            previous_assets,
            desired_receipt: Some(receipt.clone()),
            desired_assets,
        };
        let journal_path = self.write_journal(&journal)?;
        match store.package_state().rollback(stored_receipt) {
            Ok(Some(commit)) => {
                self.cleanup_journal(&journal_path);
                Ok(commit)
            }
            Ok(None) => {
                self.cleanup_journal(&journal_path);
                Err(Error::Message(format!(
                    "{} has no active package version to rollback",
                    receipt.name
                )))
            }
            Err(error) => {
                self.cleanup_journal(&journal_path);
                Err(error.into())
            }
        }
    }

    pub fn verify(&self, receipt: &Receipt) -> Result<()> {
        let install_dir = self.install_dir(receipt.kind, &receipt.name, &receipt.version);
        for (rel, expected) in &receipt.files {
            let path = install_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let actual = sha256_file(&path)?;
            if &actual != expected {
                return Err(Error::HashMismatch {
                    path: path.display().to_string(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn verify_activation(&self, receipt: &Receipt) -> Result<()> {
        let mut store = self.open_state_store()?;
        self.prepare_state(&mut store)?;
        let assets = package_assets(
            &store.runtime_assets().snapshot()?,
            receipt.kind,
            &receipt.name,
        );
        if assets.is_empty() {
            return Err(Error::Message(format!(
                "{} {} has no active runtime assets",
                receipt.kind, receipt.name
            )));
        }
        if assets
            .iter()
            .any(|asset| asset.package.version != receipt.version)
        {
            return Err(Error::Message(format!(
                "{} {} receipt version does not match its active runtime assets",
                receipt.kind, receipt.name
            )));
        }
        validate_runtime_asset_paths(&assets)
    }

    pub(crate) fn acquire_package_lock(&self, kind: PkgKind, name: &str) -> Result<PackageLock> {
        let dir = self.staging_dir();
        fs::create_dir_all(&dir).map_err(|source| io(dir.display(), source))?;
        let path = dir.join(format!("{}-{name}.lock", kind.as_str()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| io(path.display(), source))?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(PackageLock { file }),
            Err(TryLockError::WouldBlock) => Err(Error::PackageBusy {
                kind: kind.as_str().to_owned(),
                name: name.to_owned(),
            }),
            Err(TryLockError::Error(source)) => Err(io(path.display(), source)),
        }
    }

    fn prepare_state(&self, store: &mut SqliteStore) -> Result<()> {
        self.import_legacy_receipts(store)?;
        self.import_legacy_activations(store)
    }

    fn import_legacy_receipts(&self, store: &mut SqliteStore) -> Result<()> {
        if store
            .receipts()
            .import_marker_exists(PKG_RECEIPTS_TOML_IMPORT_MARKER)?
        {
            return Ok(());
        }
        let receipts = self
            .legacy_receipts()?
            .iter()
            .map(StorePkgReceipt::try_from)
            .collect::<Result<Vec<_>>>()?;
        store
            .receipts()
            .import_once(PKG_RECEIPTS_TOML_IMPORT_MARKER, &receipts)?;
        Ok(())
    }

    fn open_state_store(&self) -> Result<SqliteStore> {
        Ok(SqliteStore::open(self.receipt_db_paths())?)
    }

    fn receipt_db_paths(&self) -> StorePaths {
        if self.root == Self::default_root() {
            return StorePaths::default_paths();
        }
        let base = self.root.parent().unwrap_or(&self.root);
        StorePaths::new(base.join("state.sqlite3"), base.join("cache.sqlite3"))
    }

    fn import_legacy_activations(&self, store: &mut SqliteStore) -> Result<()> {
        if store
            .runtime_assets()
            .import_marker_exists(PKG_RUNTIME_ASSETS_IMPORT_MARKER)?
        {
            return Ok(());
        }
        let receipts = store
            .receipts()
            .all()?
            .into_iter()
            .map(Receipt::try_from)
            .collect::<Result<Vec<_>>>()?;
        let activations = receipts
            .into_iter()
            .filter_map(|receipt| self.legacy_activation(&receipt))
            .collect::<Vec<_>>();
        store
            .runtime_assets()
            .import_once(PKG_RUNTIME_ASSETS_IMPORT_MARKER, &activations)?;
        Ok(())
    }

    fn legacy_activation(&self, receipt: &Receipt) -> Option<PackageActivation> {
        let package = ActivePackage::new(receipt.kind.as_str(), &receipt.name, &receipt.version);
        let assets = match receipt.kind {
            PkgKind::Grammar => {
                let mut path = self
                    .install_dir(receipt.kind, &receipt.name, &receipt.version)
                    .join(&receipt.name);
                path.set_extension(std::env::consts::DLL_EXTENSION);
                vec![RuntimeAssetSpec::grammar(&receipt.name, path)]
            }
            PkgKind::Plugin => vec![RuntimeAssetSpec::plugin_root(
                &receipt.name,
                self.install_dir(receipt.kind, &receipt.name, &receipt.version),
            )],
            _ => {
                let path = if receipt.shim.is_empty() {
                    legacy_command_path(receipt)?
                } else {
                    self.root.join("bin").join(&receipt.shim)
                };
                legacy_command_keys(receipt)
                    .into_iter()
                    .map(|key| RuntimeAssetSpec::command(key, path.clone()))
                    .collect()
            }
        };
        Some(PackageActivation::new(package, assets))
    }

    fn reconcile_receipts(&self, store: &mut SqliteStore) -> Result<Vec<Receipt>> {
        let snapshot = store.runtime_assets().snapshot()?;
        let persisted = store
            .receipts()
            .all()?
            .into_iter()
            .map(|receipt| {
                let receipt = Receipt::try_from(receipt)?;
                Ok(((receipt.kind, receipt.name.clone()), receipt))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let active = active_packages(&snapshot)?;
        let keys = active
            .keys()
            .chain(persisted.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut receipts = Vec::with_capacity(active.len());

        for (kind, name) in keys {
            match self.acquire_package_lock(kind, &name) {
                Ok(_lock) => {
                    let state = store.package_state().get(kind.as_str(), &name)?;
                    let existing = state.receipt.map(Receipt::try_from).transpose()?;
                    let Some(package) = package_for_assets(kind, &name, &state.assets)? else {
                        if existing.is_some() {
                            store
                                .package_state()
                                .reconcile_receipt(kind.as_str(), &name, None)?;
                        }
                        continue;
                    };
                    let (receipt, needs_repair) =
                        reconcile_receipt(self, existing, &package, &state.assets)?;
                    if needs_repair {
                        store.package_state().reconcile_receipt(
                            kind.as_str(),
                            &name,
                            Some(StorePkgReceipt::try_from(&receipt)?),
                        )?;
                    }
                    receipts.push(receipt);
                }
                Err(Error::PackageBusy { .. }) => {
                    if let Some((package, assets)) = active.get(&(kind, name.clone())) {
                        let existing = persisted.get(&(kind, name.clone())).cloned();
                        let (receipt, _) = reconcile_receipt(self, existing, package, assets)?;
                        receipts.push(receipt);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        receipts.sort_by(|left, right| {
            (left.kind, left.name.as_str()).cmp(&(right.kind, right.name.as_str()))
        });
        Ok(receipts)
    }

    fn recover_pending_transactions(&self) -> Result<()> {
        let dir = self.staging_dir();
        if !dir.exists() {
            return Ok(());
        }
        let entries = fs::read_dir(&dir).map_err(|source| io(dir.display(), source))?;
        let mut paths = entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|source| io(dir.display(), source))
            })
            .collect::<Result<Vec<_>>>()?;
        paths.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(TRANSACTION_SUFFIX))
        });
        paths.sort();

        for path in paths {
            let journal = read_journal(&path)?;
            match self.acquire_package_lock(journal.kind, &journal.name) {
                Ok(_lock) => {
                    let mut store = self.open_state_store()?;
                    self.prepare_state(&mut store)?;
                    self.recover_journal(&mut store, &journal, &path)?;
                }
                Err(Error::PackageBusy { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn recover_transaction_locked(
        &self,
        store: &mut SqliteStore,
        kind: PkgKind,
        name: &str,
    ) -> Result<()> {
        let path = self.transaction_path(kind, name);
        if !path.exists() {
            return Ok(());
        }
        let journal = read_journal(&path)?;
        if journal.kind != kind || journal.name != name {
            return Err(Error::Message(format!(
                "package transaction journal {} has the wrong identity",
                path.display()
            )));
        }
        self.recover_journal(store, &journal, &path)
    }

    fn recover_journal(
        &self,
        store: &mut SqliteStore,
        journal: &MutationJournal,
        path: &Path,
    ) -> Result<()> {
        let current = store
            .package_state()
            .get(journal.kind.as_str(), &journal.name)?;
        let receipt = if current.assets == journal.desired_assets {
            journal.desired_receipt.as_ref()
        } else if current.assets == journal.previous_assets {
            journal.previous_receipt.as_ref()
        } else {
            return Err(Error::Message(format!(
                "package transaction for {} {} diverged from both recorded snapshots",
                journal.kind, journal.name
            )));
        };
        let receipt = receipt.map(StorePkgReceipt::try_from).transpose()?;
        store
            .package_state()
            .reconcile_receipt(journal.kind.as_str(), &journal.name, receipt)?;
        self.cleanup_journal(path);
        Ok(())
    }

    fn write_journal(&self, journal: &MutationJournal) -> Result<PathBuf> {
        let dir = self.staging_dir();
        fs::create_dir_all(&dir).map_err(|source| io(dir.display(), source))?;
        let path = self.transaction_path(journal.kind, &journal.name);
        if path.exists() {
            return Err(Error::Message(format!(
                "unrecovered package transaction already exists at {}",
                path.display()
            )));
        }
        let mut temp = NamedTempFile::new_in(&dir).map_err(|source| io(dir.display(), source))?;
        serde_json::to_writer(temp.as_file_mut(), journal)?;
        temp.as_file_mut()
            .flush()
            .map_err(|source| io(temp.path().display(), source))?;
        temp.as_file()
            .sync_all()
            .map_err(|source| io(temp.path().display(), source))?;
        temp.persist(&path)
            .map_err(|error| io(path.display(), error.error))?;
        Ok(path)
    }

    fn remove_journal(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io(path.display(), source)),
        }
    }

    fn cleanup_journal(&self, path: &Path) {
        if let Err(error) = self.remove_journal(path) {
            log::warn!(
                "failed to clean up package transaction journal {}: {error}",
                path.display()
            );
        }
    }

    fn transaction_path(&self, kind: PkgKind, name: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(name.as_bytes());
        self.staging_dir()
            .join(format!("{:x}{TRANSACTION_SUFFIX}", hasher.finalize()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MutationJournal {
    kind: PkgKind,
    name: String,
    previous_receipt: Option<Receipt>,
    previous_assets: Vec<RuntimeAsset>,
    desired_receipt: Option<Receipt>,
    desired_assets: Vec<RuntimeAsset>,
}

#[derive(Debug)]
#[must_use]
pub(crate) struct PackageLock {
    file: File,
}

impl Drop for PackageLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        // The lock file is intentionally retained: the advisory OS lock is tied
        // to this handle and is released on process death, so stale files are harmless.
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub name: String,
    pub kind: PkgKind,
    pub version: String,
    pub source: String,
    pub url: String,
    pub archive_sha256: String,
    pub bin: String,
    #[serde(default)]
    pub shim: String,
    #[serde(default)]
    pub previous_version: Option<String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub installed_at: String,
    #[serde(default)]
    pub native_manager: Option<String>,
    #[serde(default)]
    pub native_id: Option<String>,
}

impl Receipt {
    pub fn read(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|source| io(path.display(), source))?;
        toml::from_str(&content).map_err(|source| Error::TomlDe {
            path: path.display().to_string(),
            source,
        })
    }
}

impl TryFrom<StorePkgReceipt> for Receipt {
    type Error = Error;

    fn try_from(receipt: StorePkgReceipt) -> Result<Self> {
        serde_json::from_str(&receipt.receipt_json).map_err(Error::from)
    }
}

impl TryFrom<&Receipt> for StorePkgReceipt {
    type Error = Error;

    fn try_from(receipt: &Receipt) -> Result<Self> {
        Ok(Self {
            kind: receipt.kind.as_str().to_owned(),
            name: receipt.name.clone(),
            version: receipt.version.clone(),
            source: receipt.source.clone(),
            hash: receipt.archive_sha256.clone(),
            bin: receipt.bin.clone(),
            shim: receipt.shim.clone(),
            files_json: serde_json::to_string(&receipt.files)?,
            installed_at: receipt.installed_at.clone(),
            native_manager: receipt.native_manager.clone(),
            native_id: receipt.native_id.clone(),
            receipt_json: serde_json::to_string(receipt)?,
        })
    }
}

impl TryFrom<&StorePkgReceipt> for Receipt {
    type Error = Error;

    fn try_from(receipt: &StorePkgReceipt) -> Result<Self> {
        let mut decoded: Receipt = serde_json::from_str(&receipt.receipt_json)?;
        decoded.kind = PkgKind::from_str(&receipt.kind)?;
        Ok(decoded)
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|source| io(path.display(), source))?;
    let mut hasher = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buf)
            .map_err(|source| io(path.display(), source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buf[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn hash_tree(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    hash_tree_inner(root, root, &mut files)?;
    Ok(files)
}

fn hash_tree_inner(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|source| io(dir.display(), source))? {
        let path = entry.map_err(|source| io(dir.display(), source))?.path();
        if path.is_dir() {
            hash_tree_inner(root, &path, files)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|err| Error::Message(err.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(rel, sha256_file(&path)?);
        }
    }
    Ok(())
}

fn validate_activation_paths(activation: &PackageActivation) -> Result<()> {
    for asset in &activation.assets {
        let valid = match asset.kind {
            RuntimeAssetKind::PluginRoot => asset.path.is_dir(),
            RuntimeAssetKind::Command | RuntimeAssetKind::File | RuntimeAssetKind::Grammar => {
                asset.path.is_file()
            }
        };
        if !valid {
            return Err(Error::Message(format!(
                "cannot activate missing {} asset '{}' at {}",
                asset.kind,
                asset.key,
                asset.path.display()
            )));
        }
    }
    Ok(())
}

fn validate_runtime_asset_paths(assets: &[RuntimeAsset]) -> Result<()> {
    for asset in assets {
        let valid = match asset.kind {
            RuntimeAssetKind::PluginRoot => asset.path.is_dir(),
            RuntimeAssetKind::Command | RuntimeAssetKind::File | RuntimeAssetKind::Grammar => {
                asset.path.is_file()
            }
        };
        if !valid {
            return Err(Error::Message(format!(
                "active {} asset '{}' is missing at {}",
                asset.kind,
                asset.key,
                asset.path.display()
            )));
        }
    }
    Ok(())
}

fn materialize_activation(activation: &PackageActivation) -> Vec<RuntimeAsset> {
    let mut assets = activation
        .assets
        .iter()
        .cloned()
        .map(|asset| RuntimeAsset::from_spec(activation.package.clone(), asset))
        .collect::<Vec<_>>();
    sort_assets(&mut assets);
    assets
}

fn package_assets(snapshot: &RuntimeSnapshot, kind: PkgKind, name: &str) -> Vec<RuntimeAsset> {
    let mut assets = snapshot
        .assets
        .iter()
        .filter(|asset| asset.package.kind == kind.as_str() && asset.package.name == name)
        .cloned()
        .collect::<Vec<_>>();
    sort_assets(&mut assets);
    assets
}

fn package_for_assets(
    kind: PkgKind,
    name: &str,
    assets: &[RuntimeAsset],
) -> Result<Option<ActivePackage>> {
    let Some(package) = assets.first().map(|asset| asset.package.clone()) else {
        return Ok(None);
    };
    if package.kind != kind.as_str()
        || package.name != name
        || assets.iter().any(|asset| asset.package != package)
    {
        return Err(Error::Message(format!(
            "active assets for {kind} {name} contain multiple package identities"
        )));
    }
    Ok(Some(package))
}

type ActivePackages = BTreeMap<(PkgKind, String), (ActivePackage, Vec<RuntimeAsset>)>;

fn active_packages(snapshot: &RuntimeSnapshot) -> Result<ActivePackages> {
    let mut active: ActivePackages = BTreeMap::new();
    for asset in &snapshot.assets {
        let kind = PkgKind::from_str(&asset.package.kind)?;
        let key = (kind, asset.package.name.clone());
        let entry = active
            .entry(key)
            .or_insert_with(|| (asset.package.clone(), Vec::new()));
        if entry.0 != asset.package {
            return Err(Error::Message(format!(
                "active assets for {} {} contain multiple versions",
                kind, asset.package.name
            )));
        }
        entry.1.push(asset.clone());
    }
    for (_, assets) in active.values_mut() {
        sort_assets(assets);
    }
    Ok(active)
}

fn reconcile_receipt(
    store: &Store,
    existing: Option<Receipt>,
    package: &ActivePackage,
    assets: &[RuntimeAsset],
) -> Result<(Receipt, bool)> {
    let kind = PkgKind::from_str(&package.kind)?;
    let command = assets
        .iter()
        .find(|asset| asset.kind == RuntimeAssetKind::Command);
    let mut receipt = existing.clone().unwrap_or_else(|| Receipt {
        name: package.name.clone(),
        kind,
        version: package.version.clone(),
        source: "runtime-activation".to_owned(),
        url: assets
            .first()
            .map(|asset| asset.path.display().to_string())
            .unwrap_or_default(),
        archive_sha256: String::new(),
        bin: command
            .map(|asset| asset.key.clone())
            .unwrap_or_else(|| package.name.clone()),
        shim: String::new(),
        previous_version: None,
        files: BTreeMap::new(),
        installed_at: recovery_timestamp(),
        native_manager: None,
        native_id: None,
    });

    if receipt.version != package.version {
        let replaced = std::mem::replace(&mut receipt.version, package.version.clone());
        receipt.previous_version = Some(replaced);
        let install_dir = store.install_dir(kind, &package.name, &package.version);
        receipt.files = if install_dir.is_dir() {
            hash_tree(&install_dir)?
        } else {
            BTreeMap::new()
        };
    }
    receipt.kind = kind;
    receipt.name.clone_from(&package.name);
    if receipt.bin.is_empty() {
        receipt.bin = command
            .map(|asset| asset.key.clone())
            .unwrap_or_else(|| package.name.clone());
    }
    if receipt.installed_at.is_empty() {
        receipt.installed_at = recovery_timestamp();
    }
    let needs_repair = existing.as_ref() != Some(&receipt);
    Ok((receipt, needs_repair))
}

fn read_journal(path: &Path) -> Result<MutationJournal> {
    let source = fs::read(path).map_err(|error| io(path.display(), error))?;
    serde_json::from_slice(&source).map_err(Error::from)
}

fn sort_assets(assets: &mut [RuntimeAsset]) {
    assets.sort_by(|left, right| {
        (left.kind, left.key.as_str()).cmp(&(right.kind, right.key.as_str()))
    });
}

fn legacy_command_path(receipt: &Receipt) -> Option<PathBuf> {
    let recorded = PathBuf::from(&receipt.url);
    if recorded.is_file() {
        return Some(recorded);
    }
    let command = Path::new(&receipt.bin)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&receipt.name);
    find_on_path(command).or_else(|| find_on_path(&receipt.name))
}

fn legacy_command_keys(receipt: &Receipt) -> std::collections::BTreeSet<String> {
    let mut keys = std::collections::BTreeSet::new();
    for command in [&receipt.name, &receipt.bin, &receipt.shim] {
        if command.is_empty() {
            continue;
        }
        let path = Path::new(command);
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            keys.insert(name.to_owned());
            if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some(extension)
                    if extension.eq_ignore_ascii_case("exe")
                        || extension.eq_ignore_ascii_case("cmd")
                        || extension.eq_ignore_ascii_case("bat")
                        || extension.eq_ignore_ascii_case("ps1")
            ) {
                if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                    keys.insert(stem.to_owned());
                }
            }
        }
    }
    keys
}

fn find_on_path(command: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let direct = directory.join(command);
        if direct.is_file() {
            return Some(direct);
        }
        if cfg!(windows) && Path::new(command).extension().is_none() {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = directory.join(format!("{command}.{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    })
}

fn recovery_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;
    use helix_store::{RuntimeAssetSpec, Store as SqliteStore};

    use super::*;

    #[test]
    fn activation_receipt_round_trip_and_verify_detects_corruption() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let install = store.install_dir(PkgKind::Lsp, "demo", "1");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("demo.exe"), b"ok").unwrap();
        let mut receipt = test_receipt(PkgKind::Lsp, "demo", "1");
        receipt.files = hash_tree(&install).unwrap();
        let activation = command_activation(&receipt, install.join("demo.exe"), "demo");
        let _lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
        store.commit_activation(&receipt, activation).unwrap();
        store.verify(&receipt).unwrap();

        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        let snapshot = sqlite.runtime_assets().snapshot().unwrap();
        assert_eq!(snapshot.assets.len(), 1);
        assert_eq!(snapshot.assets[0].path, install.join("demo.exe"));
        assert!(receipt.shim.is_empty());

        fs::write(install.join("demo.exe"), b"bad").unwrap();
        assert!(matches!(
            store.verify(&receipt),
            Err(Error::HashMismatch { .. })
        ));
    }

    #[test]
    fn receipt_round_trip_keeps_native_metadata_on_active_package() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let command = dir.path().join("rust-analyzer.exe");
        fs::write(&command, b"native").unwrap();
        let mut receipt = test_receipt(PkgKind::Lsp, "rust-analyzer", "2026-06-29");
        receipt.source = "native:winget".to_owned();
        receipt.url = "native:winget:Rustlang.rust-analyzer".to_owned();
        receipt.native_manager = Some("winget".to_owned());
        receipt.native_id = Some("Rustlang.rust-analyzer".to_owned());
        let _lock = store
            .acquire_package_lock(PkgKind::Lsp, "rust-analyzer")
            .unwrap();
        store
            .commit_activation(
                &receipt,
                command_activation(&receipt, command, "rust-analyzer"),
            )
            .unwrap();
        let read = store
            .receipt_for_mutation(PkgKind::Lsp, "rust-analyzer")
            .unwrap()
            .unwrap();
        assert_eq!(read.native_manager.as_deref(), Some("winget"));
        assert_eq!(read.native_id.as_deref(), Some("Rustlang.rust-analyzer"));
        store.deactivate(PkgKind::Lsp, "rust-analyzer").unwrap();
        assert!(store.receipts().unwrap().is_empty());
    }

    #[test]
    fn imports_legacy_receipts_once_and_preserves_toml_files() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let mut first = test_receipt(PkgKind::Lsp, "demo", "1");
        first.shim = "demo-shim".to_owned();
        let second = test_receipt(PkgKind::Grammar, "rust", "rev1");
        fs::create_dir_all(store.root.join("bin")).unwrap();
        fs::write(store.root.join("bin").join("demo-shim"), b"shim").unwrap();
        store.write_legacy_receipt(&first).unwrap();
        store.write_legacy_receipt(&second).unwrap();

        let receipts = store.receipts().unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.name.as_str())
                .collect::<Vec<_>>(),
            vec!["demo", "rust"]
        );

        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        assert!(sqlite
            .receipts()
            .import_marker_exists(PKG_RECEIPTS_TOML_IMPORT_MARKER)
            .unwrap());
        assert_eq!(sqlite.receipts().all().unwrap().len(), 2);

        let changed = test_receipt(PkgKind::Lsp, "demo", "9");
        store.write_legacy_receipt(&changed).unwrap();
        let demo = store.receipt(PkgKind::Lsp, "demo").unwrap().unwrap();
        assert_eq!(demo.version, "1");
        assert!(store.receipt_path(PkgKind::Lsp, "demo").exists());
        assert!(store.receipt_path(PkgKind::Grammar, "rust").exists());
    }

    #[test]
    fn database_failure_does_not_fall_back_to_legacy_toml() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let receipt = test_receipt(PkgKind::Lsp, "demo", "1");
        store.write_legacy_receipt(&receipt).unwrap();
        fs::create_dir_all(dir.path().join("state.sqlite3")).unwrap();

        assert!(store.receipts().is_err());
    }

    #[test]
    fn activation_collision_keeps_previous_package_and_discards_new_receipt() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let first_path = dir.path().join("first.exe");
        let second_path = dir.path().join("second.exe");
        fs::write(&first_path, b"first").unwrap();
        fs::write(&second_path, b"second").unwrap();
        let first = test_receipt(PkgKind::Lsp, "first", "1");
        let second = test_receipt(PkgKind::Lsp, "second", "1");

        {
            let _lock = store.acquire_package_lock(PkgKind::Lsp, "first").unwrap();
            store
                .commit_activation(&first, command_activation(&first, first_path, "shared"))
                .unwrap();
        }
        let error = {
            let _lock = store.acquire_package_lock(PkgKind::Lsp, "second").unwrap();
            store
                .commit_activation(&second, command_activation(&second, second_path, "shared"))
                .unwrap_err()
        };
        assert!(error.to_string().contains("collision"));

        assert_eq!(store.receipts().unwrap(), vec![first]);
        assert!(!store.transaction_path(PkgKind::Lsp, "second").exists());
    }

    #[test]
    fn authoritative_activation_repairs_missing_and_stale_receipts() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let path = dir.path().join("active.exe");
        fs::write(&path, b"active").unwrap();
        let active = test_receipt(PkgKind::Lsp, "active", "2");
        let stale = test_receipt(PkgKind::Lsp, "stale", "1");
        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        sqlite
            .runtime_assets()
            .activate(command_activation(&active, path, "active"))
            .unwrap();
        sqlite
            .receipts()
            .upsert(StorePkgReceipt::try_from(&stale).unwrap())
            .unwrap();
        drop(sqlite);

        let receipts = store.receipts().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].name, "active");
        assert_eq!(receipts[0].version, "2");
        assert_eq!(receipts[0].source, "runtime-activation");

        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        assert!(sqlite.receipts().get("lsp", "active").unwrap().is_some());
        assert!(sqlite.receipts().get("lsp", "stale").unwrap().is_none());
    }

    #[test]
    fn interrupted_install_rolls_forward_receipt_from_active_snapshot() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let old_path = dir.path().join("old.exe");
        let new_path = dir.path().join("new.exe");
        fs::write(&old_path, b"old").unwrap();
        fs::write(&new_path, b"new").unwrap();
        let old = test_receipt(PkgKind::Lsp, "demo", "1");
        let new = test_receipt(PkgKind::Lsp, "demo", "2");
        let old_activation = command_activation(&old, old_path, "demo");
        let new_activation = command_activation(&new, new_path, "demo");
        {
            let _lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
            store
                .commit_activation(&old, old_activation.clone())
                .unwrap();
        }

        let journal = MutationJournal {
            kind: PkgKind::Lsp,
            name: "demo".to_owned(),
            previous_receipt: Some(old),
            previous_assets: materialize_activation(&old_activation),
            desired_receipt: Some(new.clone()),
            desired_assets: materialize_activation(&new_activation),
        };
        store.write_journal(&journal).unwrap();
        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        sqlite.runtime_assets().activate(new_activation).unwrap();
        drop(sqlite);

        assert_eq!(store.receipts().unwrap(), vec![new]);
        assert!(!store.transaction_path(PkgKind::Lsp, "demo").exists());
    }

    #[test]
    fn interrupted_uninstall_finishes_receipt_removal() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let path = dir.path().join("demo.exe");
        fs::write(&path, b"demo").unwrap();
        let receipt = test_receipt(PkgKind::Lsp, "demo", "1");
        let activation = command_activation(&receipt, path, "demo");
        {
            let _lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
            store
                .commit_activation(&receipt, activation.clone())
                .unwrap();
        }
        let journal = MutationJournal {
            kind: PkgKind::Lsp,
            name: "demo".to_owned(),
            previous_receipt: Some(receipt),
            previous_assets: materialize_activation(&activation),
            desired_receipt: None,
            desired_assets: Vec::new(),
        };
        store.write_journal(&journal).unwrap();
        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        sqlite.runtime_assets().remove("lsp", "demo").unwrap();
        drop(sqlite);

        assert!(store.receipts().unwrap().is_empty());
        let mut sqlite = SqliteStore::open(store.receipt_db_paths()).unwrap();
        assert!(sqlite.receipts().get("lsp", "demo").unwrap().is_none());
    }

    #[test]
    fn package_lock_rejects_concurrent_mutation_and_releases_on_drop() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path());

        let lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
        let err = store
            .acquire_package_lock(PkgKind::Lsp, "demo")
            .unwrap_err();
        assert!(err.to_string().contains("already running"));

        drop(lock);
        let _lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
    }

    #[test]
    fn package_lock_ignores_stale_lock_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path());
        let lock_path = store.staging_dir().join("lsp-demo.lock");
        fs::create_dir_all(store.staging_dir()).unwrap();
        fs::write(&lock_path, b"stale").unwrap();

        let _lock = store.acquire_package_lock(PkgKind::Lsp, "demo").unwrap();
        assert!(lock_path.exists());
    }

    fn test_receipt(kind: PkgKind, name: &str, version: &str) -> Receipt {
        Receipt {
            name: name.to_owned(),
            kind,
            version: version.to_owned(),
            source: "archive".to_owned(),
            url: format!("file:///{name}.zip"),
            archive_sha256: "abc".to_owned(),
            bin: name.to_owned(),
            shim: String::new(),
            previous_version: None,
            files: BTreeMap::new(),
            installed_at: "now".to_owned(),
            native_manager: None,
            native_id: None,
        }
    }

    fn command_activation(
        receipt: &Receipt,
        path: impl Into<PathBuf>,
        key: &str,
    ) -> PackageActivation {
        PackageActivation::new(
            ActivePackage::new(receipt.kind.as_str(), &receipt.name, &receipt.version),
            vec![RuntimeAssetSpec::command(key, path)],
        )
    }
}
