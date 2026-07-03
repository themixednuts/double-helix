use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{io, spec::PkgKind, Error, Result};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub lsp: Vec<String>,
    #[serde(default)]
    pub dap: Vec<String>,
    #[serde(default)]
    pub grammar: Vec<String>,
    #[serde(default)]
    pub plugin: Vec<String>,
}

impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|source| io(path.display(), source))?;
        toml::from_str(&content).map_err(|source| Error::TomlDe {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io(parent.display(), source))?;
        }
        fs::write(path, toml::to_string_pretty(self)?).map_err(|source| io(path.display(), source))
    }

    pub fn packages(&self) -> impl Iterator<Item = (PkgKind, &str)> {
        self.lsp
            .iter()
            .map(|name| (PkgKind::Lsp, name.as_str()))
            .chain(self.dap.iter().map(|name| (PkgKind::Dap, name.as_str())))
            .chain(
                self.grammar
                    .iter()
                    .map(|name| (PkgKind::Grammar, name.as_str())),
            )
            .chain(
                self.plugin
                    .iter()
                    .map(|name| (PkgKind::Plugin, name.as_str())),
            )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|source| io(path.display(), source))?;
        toml::from_str(&content).map_err(|source| Error::TomlDe {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| io(parent.display(), source))?;
        }
        fs::write(path, toml::to_string_pretty(self)?).map_err(|source| io(path.display(), source))
    }

    pub fn find(&self, kind: PkgKind, name: &str) -> Option<&LockedPackage> {
        self.packages
            .iter()
            .find(|package| package.kind == kind && package.name == name)
    }

    pub fn upsert(&mut self, package: LockedPackage) {
        if let Some(existing) = self
            .packages
            .iter_mut()
            .find(|entry| entry.kind == package.kind && entry.name == package.name)
        {
            *existing = package;
        } else {
            self.packages.push(package);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub kind: PkgKind,
    pub version: String,
    pub source: String,
    pub url: String,
    pub sha256: String,
    pub bin: String,
}

#[cfg(test)]
mod tests {
    use assert_fs::TempDir;

    use super::*;

    #[test]
    fn manifest_lock_round_trip() {
        let dir = TempDir::new().unwrap();
        let manifest_path = dir.path().join("pkg.toml");
        let lock_path = dir.path().join("pkg.lock");
        let manifest = Manifest {
            lsp: vec!["rust-analyzer".to_owned()],
            dap: vec!["codelldb".to_owned()],
            ..Manifest::default()
        };
        manifest.write(&manifest_path).unwrap();
        assert_eq!(Manifest::read(&manifest_path).unwrap(), manifest);

        let mut lock = Lock::default();
        lock.upsert(LockedPackage {
            name: "rust-analyzer".to_owned(),
            kind: PkgKind::Lsp,
            version: "1".to_owned(),
            source: "archive".to_owned(),
            url: "file:///tmp/ra.zip".to_owned(),
            sha256: "00".to_owned(),
            bin: "rust-analyzer".to_owned(),
        });
        lock.write(&lock_path).unwrap();
        assert_eq!(Lock::read(&lock_path).unwrap(), lock);
    }
}
