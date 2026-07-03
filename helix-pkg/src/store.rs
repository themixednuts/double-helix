use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{io, spec::PkgKind, Error, Result};

const PRODUCT_CONFIG_DIR: &str = "double-helix";

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
        let path = self.receipt_path(receipt.kind, &receipt.name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io(parent.display(), source))?;
        }
        fs::write(&path, toml::to_string_pretty(receipt)?)
            .map_err(|source| io(path.display(), source))
    }

    pub fn remove(&self, kind: PkgKind, name: &str) -> Result<()> {
        let receipt_path = self.receipt_path(kind, name);
        let receipt = if receipt_path.exists() {
            Some(Receipt::read(&receipt_path)?)
        } else {
            None
        };
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
        remove_path(&receipt_path)
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
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => Ok(PackageLock {
                path,
                file: Some(file),
            }),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(Error::Message(format!(
                    "package operation already running for {kind} {name}"
                )))
            }
            Err(source) => Err(io(path.display(), source)),
        }
    }
}

#[derive(Debug)]
#[must_use]
pub(crate) struct PackageLock {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for PackageLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
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
        let read = Receipt::read(&store.receipt_path(PkgKind::Lsp, "rust-analyzer")).unwrap();
        assert_eq!(read.native_manager.as_deref(), Some("winget"));
        assert_eq!(read.native_id.as_deref(), Some("Rustlang.rust-analyzer"));
        store.remove(PkgKind::Lsp, "rust-analyzer").unwrap();
        assert!(store.bin_dir().exists());
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
}
