use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    io,
    spec::{PackageSpec, PkgKind},
    Error, Result,
};

const BUILTIN: &[&str] = &[
    include_str!("../../registry/lsp/rust-analyzer.toml"),
    include_str!("../../registry/lsp/clangd.toml"),
    include_str!("../../registry/lsp/gopls.toml"),
    include_str!("../../registry/lsp/lua-language-server.toml"),
    include_str!("../../registry/lsp/zls.toml"),
    include_str!("../../registry/lsp/taplo.toml"),
    include_str!("../../registry/lsp/marksman.toml"),
    include_str!("../../registry/lsp/jdtls.toml"),
    include_str!("../../registry/lsp/omnisharp.toml"),
    include_str!("../../registry/dap/codelldb.toml"),
    include_str!("../../registry/dap/netcoredbg.toml"),
];

#[derive(Debug, Clone, Default)]
pub struct Registry {
    packages: BTreeMap<(PkgKind, String), PackageSpec>,
}

impl Registry {
    pub fn builtin() -> Result<Self> {
        let mut registry = Self::default();
        for content in BUILTIN {
            registry.insert_str("<builtin>", content)?;
        }
        Ok(registry)
    }

    pub fn from_dirs(paths: &[PathBuf]) -> Result<Self> {
        let mut registry = Self::builtin()?;
        for path in paths {
            registry.merge_dir(path)?;
        }
        Ok(registry)
    }

    pub fn insert_str(&mut self, path: &str, content: &str) -> Result<()> {
        let package: PackageSpec = toml::from_str(content).map_err(|source| Error::TomlDe {
            path: path.to_owned(),
            source,
        })?;
        Self::lint(&package)?;
        self.packages
            .insert((package.kind, package.name.clone()), package);
        Ok(())
    }

    pub fn merge_dir(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        self.merge_dir_inner(path)
    }

    fn merge_dir_inner(&mut self, path: &Path) -> Result<()> {
        for entry in fs::read_dir(path).map_err(|source| io(path.display(), source))? {
            let entry = entry.map_err(|source| io(path.display(), source))?;
            let path = entry.path();
            if path.is_dir() {
                self.merge_dir_inner(&path)?;
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                let content =
                    fs::read_to_string(&path).map_err(|source| io(path.display(), source))?;
                self.insert_str(&path.display().to_string(), &content)?;
            }
        }
        Ok(())
    }

    pub fn get(&self, kind: PkgKind, name: &str) -> Option<&PackageSpec> {
        self.packages.get(&(kind, name.to_owned()))
    }

    pub fn find(&self, name: &str) -> Option<&PackageSpec> {
        self.packages.values().find(|package| package.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PackageSpec> {
        self.packages.values()
    }

    pub fn search(&self, term: &str) -> Vec<&PackageSpec> {
        let needle = term.to_ascii_lowercase();
        self.packages
            .values()
            .filter(|package| {
                package.name.to_ascii_lowercase().contains(&needle)
                    || package.description.to_ascii_lowercase().contains(&needle)
                    || package
                        .languages
                        .iter()
                        .any(|language| language.to_ascii_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn lint(package: &PackageSpec) -> Result<()> {
        if package.name.trim().is_empty() {
            return Err(Error::InvalidPackage {
                name: package.name.clone(),
                message: "name must not be empty".to_owned(),
            });
        }
        if package.artifacts.is_empty() {
            return Err(Error::InvalidPackage {
                name: package.name.clone(),
                message: "at least one artifact is required".to_owned(),
            });
        }
        for artifact in &package.artifacts {
            artifact.source.validate(&package.name)?;
            if artifact.bin.trim().is_empty() {
                return Err(Error::InvalidPackage {
                    name: package.name.clone(),
                    message: "artifact bin must not be empty".to_owned(),
                });
            }
        }
        Ok(())
    }

    pub fn lint_seed_shape(package: &PackageSpec) -> Result<()> {
        Self::lint(package)?;
        if !package.is_system_only() {
            for (os, arch) in [
                ("windows", "x86_64"),
                ("linux", "x86_64"),
                ("macos", "aarch64"),
            ] {
                package.artifact_for(os, arch)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses_and_lints() {
        let registry = Registry::builtin().unwrap();
        assert!(registry.find("rust-analyzer").is_some());
        for package in registry.iter() {
            Registry::lint_seed_shape(package).unwrap();
        }
    }

    #[test]
    fn merge_precedence_uses_later_entries() {
        let mut registry = Registry::default();
        registry
            .insert_str(
                "base",
                r#"
name = "demo"
kind = "lsp"
description = "old"
languages = ["a"]

[[artifact]]
os = "windows"
arch = "x86_64"
source = { system = "demo" }
bin = "demo"
"#,
            )
            .unwrap();
        registry
            .insert_str(
                "overlay",
                r#"
name = "demo"
kind = "lsp"
description = "new"
languages = ["b"]

[[artifact]]
os = "windows"
arch = "x86_64"
source = { system = "demo2" }
bin = "demo2"
"#,
            )
            .unwrap();

        let package = registry.find("demo").unwrap();
        assert_eq!(package.description, "new");
        assert_eq!(package.artifacts[0].bin, "demo2");
    }
}
