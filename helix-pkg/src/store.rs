use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
use fs4::{FileExt, TryLockError};
use helix_store::{PkgReceipt as StorePkgReceipt, Store as SqliteStore, StorePaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{io, spec::PkgKind, Error, Result};

const PRODUCT_CONFIG_DIR: &str = "double-helix";
pub(crate) const PKG_RECEIPTS_TOML_IMPORT_MARKER: &str = "pkg-receipts-toml-v1";

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn default_root() -> PathBuf {
        let strategy = choose_base_strategy().expect("Unable to find the data directory!");
        strategy.data_dir().join(PRODUCT_CONFIG_DIR).join("pkg")
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

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
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

    pub fn receipt_path(&self, kind: PkgKind, name: &str) -> PathBuf {
        self.root
            .join("receipts")
            .join(format!("{}-{name}.toml", kind.as_str()))
    }

    pub fn prepare(&self) -> Result<()> {
        for path in [
            self.root.join("store"),
            self.bin_dir(),
            self.staging_dir(),
            self.registry_dir(),
            self.root.join("receipts"),
            self.runtime_dir("node"),
            self.runtime_dir("py"),
        ] {
            fs::create_dir_all(&path).map_err(|source| io(path.display(), source))?;
        }
        Ok(())
    }

    pub fn receipts(&self) -> Result<Vec<Receipt>> {
        if let Ok(receipts) = self.db_receipts() {
            return Ok(receipts);
        }
        self.legacy_receipts()
    }

    pub fn receipt(&self, kind: PkgKind, name: &str) -> Result<Option<Receipt>> {
        if let Ok(receipt) = self.db_receipt(kind, name) {
            return Ok(receipt);
        }
        let path = self.receipt_path(kind, name);
        if path.exists() {
            Receipt::read(&path).map(Some)
        } else {
            Ok(None)
        }
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

    pub fn write_receipt(&self, receipt: &Receipt) -> Result<()> {
        if let Ok(()) = self.write_db_receipt(receipt) {
            return Ok(());
        }
        self.write_legacy_receipt(receipt)
    }

    fn write_legacy_receipt(&self, receipt: &Receipt) -> Result<()> {
        let path = self.receipt_path(receipt.kind, &receipt.name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io(parent.display(), source))?;
        }
        fs::write(&path, toml::to_string_pretty(receipt)?)
            .map_err(|source| io(path.display(), source))
    }

    pub fn remove(&self, kind: PkgKind, name: &str) -> Result<()> {
        let receipt_path = self.receipt_path(kind, name);
        let receipt = self.receipt(kind, name)?;
        if let Some(receipt) = receipt {
            if !receipt.shim.is_empty() {
                let shim = self.bin_dir().join(&receipt.shim);
                remove_path(&shim)?;
            }
        } else {
            for extension in ["", ".exe", ".cmd", ".bat"] {
                remove_path(&self.bin_dir().join(format!("{name}{extension}")))?;
            }
        }
        match self.delete_db_receipt(kind, name) {
            Ok(()) => Ok(()),
            Err(_) => remove_path(&receipt_path),
        }
    }

    pub fn activate(&self, receipt: &mut Receipt, target: &Path) -> Result<()> {
        fs::create_dir_all(self.bin_dir())
            .map_err(|source| io(self.bin_dir().display(), source))?;
        let shim_name = shim_name(&receipt.name, target);
        let shim = self.bin_dir().join(&shim_name);
        remove_path(&shim)?;
        activate_target(target, &shim)?;
        receipt.shim = shim_name;
        Ok(())
    }

    pub fn activate_command(
        &self,
        receipt: &mut Receipt,
        command: &Path,
        first_arg: &Path,
    ) -> Result<()> {
        fs::create_dir_all(self.bin_dir())
            .map_err(|source| io(self.bin_dir().display(), source))?;
        let shim_name = command_shim_name(&receipt.name);
        let shim = self.bin_dir().join(&shim_name);
        remove_path(&shim)?;
        write_command_shim(&shim, command, first_arg)?;
        receipt.shim = shim_name;
        Ok(())
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
            Err(TryLockError::WouldBlock) => Err(Error::Message(format!(
                "package operation already running for {kind} {name}"
            ))),
            Err(TryLockError::Error(source)) => Err(io(path.display(), source)),
        }
    }

    fn db_receipts(&self) -> Result<Vec<Receipt>> {
        let mut store = self.open_receipt_store()?;
        self.import_legacy_receipts(&mut store)?;
        let mut receipts = store
            .receipts()
            .all()?
            .into_iter()
            .map(Receipt::try_from)
            .collect::<Result<Vec<_>>>()?;
        receipts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(receipts)
    }

    fn db_receipt(&self, kind: PkgKind, name: &str) -> Result<Option<Receipt>> {
        let mut store = self.open_receipt_store()?;
        self.import_legacy_receipts(&mut store)?;
        store
            .receipts()
            .get(kind.as_str(), name)?
            .map(Receipt::try_from)
            .transpose()
    }

    fn write_db_receipt(&self, receipt: &Receipt) -> Result<()> {
        let mut store = self.open_receipt_store()?;
        self.import_legacy_receipts(&mut store)?;
        store
            .receipts()
            .upsert(StorePkgReceipt::try_from(receipt)?)?;
        Ok(())
    }

    fn delete_db_receipt(&self, kind: PkgKind, name: &str) -> Result<()> {
        let mut store = self.open_receipt_store()?;
        self.import_legacy_receipts(&mut store)?;
        store.receipts().delete(kind.as_str(), name)?;
        Ok(())
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

    fn open_receipt_store(&self) -> Result<SqliteStore> {
        Ok(SqliteStore::open(self.receipt_db_paths())?)
    }

    fn receipt_db_paths(&self) -> StorePaths {
        if self.root == Self::default_root() {
            return StorePaths::default_paths();
        }
        let base = self.root.parent().unwrap_or(&self.root);
        StorePaths::new(base.join("state.sqlite3"), base.join("cache.sqlite3"))
    }
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

fn shim_name(name: &str, target: &Path) -> String {
    if cfg!(windows) {
        match target.extension().and_then(|ext| ext.to_str()) {
            Some("exe" | "cmd" | "bat") => format!(
                "{}.{}",
                name,
                target
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("cmd")
            ),
            _ => format!("{name}.cmd"),
        }
    } else {
        name.to_owned()
    }
}

fn command_shim_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_owned()
    }
}

fn activate_target(target: &Path, shim: &Path) -> Result<()> {
    if cfg!(windows) {
        if target.is_dir() {
            copy_dir(target, shim)
        } else if matches!(
            target.extension().and_then(|ext| ext.to_str()),
            Some("exe" | "cmd" | "bat")
        ) {
            fs::copy(target, shim).map_err(|source| io(shim.display(), source))?;
            Ok(())
        } else {
            let content = format!("@echo off\r\n\"{}\" %*\r\n", target.display());
            fs::write(shim, content).map_err(|source| io(shim.display(), source))
        }
    } else {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, shim).map_err(|source| io(shim.display(), source))
        }
        #[cfg(not(unix))]
        {
            fs::copy(target, shim).map_err(|source| io(shim.display(), source))?;
            Ok(())
        }
    }
}

fn write_command_shim(shim: &Path, command: &Path, first_arg: &Path) -> Result<()> {
    let content = if cfg!(windows) {
        format!(
            "@echo off\r\n\"{}\" \"{}\" %*\r\n",
            command.display(),
            first_arg.display()
        )
    } else {
        format!(
            "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
            command.display(),
            first_arg.display()
        )
    };
    fs::write(shim, content).map_err(|source| io(shim.display(), source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(shim)
            .map_err(|source| io(shim.display(), source))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(shim, permissions).map_err(|source| io(shim.display(), source))?;
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(|source| io(to.display(), source))?;
    for entry in fs::read_dir(from).map_err(|source| io(from.display(), source))? {
        let entry = entry.map_err(|source| io(from.display(), source))?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            fs::copy(&path, &dest).map_err(|source| io(dest.display(), source))?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|source| io(path.display(), source))
    } else {
        fs::remove_file(path).map_err(|source| io(path.display(), source))
    }
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;
    use helix_store::Store as SqliteStore;

    use super::*;

    #[test]
    fn receipt_round_trip_and_verify_detects_corruption() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path());
        let install = store.install_dir(PkgKind::Lsp, "demo", "1");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("demo.exe"), b"ok").unwrap();
        let mut receipt = Receipt {
            name: "demo".to_owned(),
            kind: PkgKind::Lsp,
            version: "1".to_owned(),
            source: "archive".to_owned(),
            url: "file:///demo.zip".to_owned(),
            archive_sha256: "abc".to_owned(),
            bin: "demo.exe".to_owned(),
            shim: String::new(),
            previous_version: None,
            files: hash_tree(&install).unwrap(),
            installed_at: "now".to_owned(),
            native_manager: None,
            native_id: None,
        };
        store
            .activate(&mut receipt, &install.join("demo.exe"))
            .unwrap();
        store.write_receipt(&receipt).unwrap();
        store.verify(&receipt).unwrap();

        fs::write(install.join("demo.exe"), b"bad").unwrap();
        assert!(matches!(
            store.verify(&receipt),
            Err(Error::HashMismatch { .. })
        ));
    }

    #[test]
    fn native_receipt_round_trip_keeps_manager_metadata_without_shim() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path());
        store.prepare().unwrap();
        let receipt = Receipt {
            name: "rust-analyzer".to_owned(),
            kind: PkgKind::Lsp,
            version: "2026-06-29".to_owned(),
            source: "native:winget".to_owned(),
            url: "native:winget:Rustlang.rust-analyzer".to_owned(),
            archive_sha256: String::new(),
            bin: "rust-analyzer".to_owned(),
            shim: String::new(),
            previous_version: None,
            files: BTreeMap::new(),
            installed_at: "now".to_owned(),
            native_manager: Some("winget".to_owned()),
            native_id: Some("Rustlang.rust-analyzer".to_owned()),
        };
        store.write_receipt(&receipt).unwrap();
        let read = store
            .receipt(PkgKind::Lsp, "rust-analyzer")
            .unwrap()
            .unwrap();
        assert_eq!(read.native_manager.as_deref(), Some("winget"));
        assert_eq!(read.native_id.as_deref(), Some("Rustlang.rust-analyzer"));
        store.remove(PkgKind::Lsp, "rust-analyzer").unwrap();
        assert!(store.bin_dir().exists());
    }

    #[test]
    fn imports_legacy_receipts_once_and_preserves_toml_files() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let first = test_receipt(PkgKind::Lsp, "demo", "1");
        let second = test_receipt(PkgKind::Grammar, "rust", "rev1");
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
    fn falls_back_to_toml_receipts_when_database_cannot_open() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let receipt = test_receipt(PkgKind::Lsp, "demo", "1");
        store.write_legacy_receipt(&receipt).unwrap();
        fs::create_dir_all(dir.path().join("state.sqlite3")).unwrap();

        assert_eq!(store.receipts().unwrap(), vec![receipt.clone()]);
        assert_eq!(store.receipt(PkgKind::Lsp, "demo").unwrap(), Some(receipt));
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
}
