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
    pub formatter: Vec<String>,
    #[serde(default)]
    pub linter: Vec<String>,
    #[serde(default)]
    pub grammar: Vec<String>,
    #[serde(default)]
    pub plugin: Vec<String>,
    #[serde(default)]
    pub acp: Vec<String>,
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
                self.formatter
                    .iter()
                    .map(|name| (PkgKind::Formatter, name.as_str())),
            )
            .chain(
                self.linter
                    .iter()
                    .map(|name| (PkgKind::Linter, name.as_str())),
            )
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
            .chain(self.acp.iter().map(|name| (PkgKind::Acp, name.as_str())))
    }

    pub fn contains(&self, kind: PkgKind, name: &str) -> bool {
        match kind {
            PkgKind::Lsp => self.lsp.iter().any(|package| package == name),
            PkgKind::Dap => self.dap.iter().any(|package| package == name),
            PkgKind::Formatter => self.formatter.iter().any(|package| package == name),
            PkgKind::Linter => self.linter.iter().any(|package| package == name),
            PkgKind::Grammar => self.grammar.iter().any(|package| package == name),
            PkgKind::Plugin => self.plugin.iter().any(|package| package == name),
            PkgKind::Acp => self.acp.iter().any(|package| package == name),
        }
    }

    pub fn merged_with(&self, overlay: &Self) -> Self {
        Self {
            lsp: merge_package_names(&self.lsp, &overlay.lsp),
            dap: merge_package_names(&self.dap, &overlay.dap),
            formatter: merge_package_names(&self.formatter, &overlay.formatter),
            linter: merge_package_names(&self.linter, &overlay.linter),
            grammar: merge_package_names(&self.grammar, &overlay.grammar),
            plugin: merge_package_names(&self.plugin, &overlay.plugin),
            acp: merge_package_names(&self.acp, &overlay.acp),
        }
    }
}

fn merge_package_names(base: &[String], overlay: &[String]) -> Vec<String> {
    let mut merged = Vec::with_capacity(base.len() + overlay.len());
    for name in base.iter().chain(overlay) {
        if !merged.iter().any(|existing| existing == name) {
            merged.push(name.clone());
        }
    }
    merged
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
    #[serde(default)]
    pub previous_version: Option<String>,
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
            previous_version: None,
            source: "archive".to_owned(),
            url: "file:///tmp/ra.zip".to_owned(),
            sha256: "00".to_owned(),
            bin: "rust-analyzer".to_owned(),
        });
        lock.write(&lock_path).unwrap();
        assert_eq!(Lock::read(&lock_path).unwrap(), lock);
    }

    #[test]
    fn manifest_merge_preserves_order_and_deduplicates_overlay() {
        let user = Manifest {
            lsp: vec!["rust-analyzer".to_owned(), "pyright".to_owned()],
            grammar: vec!["rust".to_owned()],
            ..Manifest::default()
        };
        let project = Manifest {
            lsp: vec![
                "rust-analyzer".to_owned(),
                "typescript-language-server".to_owned(),
            ],
            dap: vec!["codelldb".to_owned()],
            ..Manifest::default()
        };

        let merged = user.merged_with(&project);
        assert_eq!(
            merged.lsp,
            vec![
                "rust-analyzer".to_owned(),
                "pyright".to_owned(),
                "typescript-language-server".to_owned()
            ]
        );
        assert_eq!(merged.dap, vec!["codelldb".to_owned()]);
        assert!(project.contains(PkgKind::Lsp, "rust-analyzer"));
        assert!(!project.contains(PkgKind::Grammar, "rust"));
    }
}
