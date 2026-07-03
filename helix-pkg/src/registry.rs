use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    config::PkgConfig,
    io,
    spec::{PackageSpec, PkgKind},
    store::Store,
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
    include_str!("../../registry/lsp/pyright.toml"),
    include_str!("../../registry/lsp/basedpyright.toml"),
    include_str!("../../registry/lsp/typescript-language-server.toml"),
    include_str!("../../registry/lsp/bash-language-server.toml"),
    include_str!("../../registry/lsp/yaml-language-server.toml"),
    include_str!("../../registry/lsp/jdtls.toml"),
    include_str!("../../registry/lsp/omnisharp.toml"),
    include_str!("../../registry/dap/codelldb.toml"),
    include_str!("../../registry/dap/netcoredbg.toml"),
    include_str!("../../registry/dap/debugpy.toml"),
    include_str!("../../registry/formatter/prettier.toml"),
    include_str!("../../registry/formatter/stylua.toml"),
    include_str!("../../registry/linter/ruff.toml"),
    include_str!("../../registry/grammar/rust.toml"),
    include_str!("../../registry/grammar/python.toml"),
    include_str!("../../registry/grammar/typescript.toml"),
    include_str!("../../registry/grammar/go.toml"),
    include_str!("../../registry/grammar/markdown.toml"),
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

    pub fn from_config(config: &PkgConfig, store: &Store) -> Result<Self> {
        Self::from_dirs(&config.registry_dirs(store)?)
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
        self.packages
            .values()
            .filter(|package| package.matches_search(term))
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
        lint_non_empty_list(package, "aliases", &package.aliases)?;
        lint_non_empty_list(package, "categories", &package.categories)?;
        lint_non_empty_list(package, "languages", &package.languages)?;
        for (name, url) in &package.schemas {
            if name.trim().is_empty() || url.trim().is_empty() {
                return Err(Error::InvalidPackage {
                    name: package.name.clone(),
                    message: "schemas must not contain empty names or urls".to_owned(),
                });
            }
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

fn lint_non_empty_list(package: &PackageSpec, field: &str, values: &[String]) -> Result<()> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(Error::InvalidPackage {
            name: package.name.clone(),
            message: format!("{field} must not contain empty values"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assert_fs::TempDir;

    use crate::{RegistrySource, Store};

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

    #[test]
    fn search_includes_progressive_metadata() {
        let mut registry = Registry::default();
        registry
            .insert_str(
                "metadata",
                r#"
name = "rust-analyzer"
kind = "lsp"
description = "Rust language server"
aliases = ["ra"]
categories = ["language-server"]
languages = ["rust"]

[schemas]
lsp = "https://example.com/rust-analyzer-schema.json"

[[artifact]]
os = "windows"
arch = "x86_64"
source = { archive = "file:///rust-analyzer.zip" }
bin = "rust-analyzer"
"#,
            )
            .unwrap();

        assert_eq!(registry.search("ra")[0].name, "rust-analyzer");
        assert_eq!(registry.search("language-server")[0].name, "rust-analyzer");
        assert_eq!(registry.search("schema")[0].name, "rust-analyzer");
    }

    #[test]
    fn from_config_loads_direct_and_source_registries() {
        let dir = TempDir::new().unwrap();
        let direct = dir.path().join("direct");
        let sourced = dir.path().join("sourced");
        fs::create_dir_all(direct.join("lsp")).unwrap();
        fs::create_dir_all(sourced.join("lsp")).unwrap();
        fs::write(
            direct.join("lsp").join("direct.toml"),
            registry_package("direct"),
        )
        .unwrap();
        fs::write(
            sourced.join("lsp").join("sourced.toml"),
            registry_package("sourced"),
        )
        .unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let config = PkgConfig {
            registries: vec![direct],
            registry_sources: vec![RegistrySource {
                name: "fixture".to_owned(),
                path: Some(sourced),
                git: None,
                branch: None,
                rev: None,
            }],
            ..PkgConfig::default()
        };

        let registry = Registry::from_config(&config, &store).unwrap();
        assert!(registry.find("direct").is_some());
        assert!(registry.find("sourced").is_some());
    }

    fn registry_package(name: &str) -> String {
        format!(
            r#"
name = "{name}"
kind = "lsp"
description = "{name}"

[[artifact]]
os = "windows"
arch = "x86_64"
source = {{ system = "{name}" }}
bin = "{name}"
"#
        )
    }
}
