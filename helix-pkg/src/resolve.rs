use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{registry::Registry, spec::PkgKind, store::Store, PackageSpec, Result};

pub fn binary(store: &Store, _kind: PkgKind, name: &str) -> Option<PathBuf> {
    shim_binary(store, name).or_else(|| which(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSource {
    Explicit,
    Shim,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub path: PathBuf,
    pub source: CommandSource,
}

pub fn command(store: &Store, kind: PkgKind, command: &str) -> Option<ResolvedCommand> {
    command_with_paths(store, kind, command, env::var_os("PATH"))
}

pub fn command_with_paths(
    store: &Store,
    _kind: PkgKind,
    command: &str,
    paths: Option<OsString>,
) -> Option<ResolvedCommand> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Some(ResolvedCommand {
            path: path.to_path_buf(),
            source: CommandSource::Explicit,
        });
    }

    if let Some(path) = shim_binary(store, command) {
        return Some(ResolvedCommand {
            path,
            source: CommandSource::Shim,
        });
    }

    which_in(command, paths).map(|path| ResolvedCommand {
        path,
        source: CommandSource::Path,
    })
}

pub fn package_for_missing_command<'a>(
    registry: &'a Registry,
    kind: PkgKind,
    language: Option<&str>,
    command: &str,
) -> Option<&'a PackageSpec> {
    let by_command = registry
        .iter()
        .filter(|package| package.kind == kind && !package.is_system_only())
        .find(|package| package_commands(package).any(|candidate| candidate == command));
    by_command.or_else(|| {
        let language = language?;
        registry
            .iter()
            .filter(|package| package.kind == kind && !package.is_system_only())
            .find(|package| {
                package
                    .languages
                    .iter()
                    .any(|candidate| candidate == language)
            })
    })
}

#[derive(Debug, Default, Clone)]
pub struct NudgeSession {
    seen: HashSet<String>,
}

impl NudgeSession {
    pub fn should_emit(&mut self, server: &str, package: &str) -> bool {
        self.seen.insert(format!("{server}:{package}"))
    }
}

fn package_commands(package: &PackageSpec) -> impl Iterator<Item = &str> {
    package.artifacts.iter().flat_map(|artifact| {
        [
            Some(artifact.bin.as_str()),
            artifact.source.bin.as_deref(),
            artifact.source.system.as_deref(),
        ]
        .into_iter()
        .flatten()
    })
}

fn shim_binary(store: &Store, name: &str) -> Option<PathBuf> {
    let bin = store.bin_dir();
    let candidates = if cfg!(windows) {
        vec![
            bin.join(format!("{name}.exe")),
            bin.join(format!("{name}.cmd")),
            bin.join(format!("{name}.bat")),
            bin.join(name),
        ]
    } else {
        vec![bin.join(name)]
    };
    candidates.into_iter().find(|path| path.exists())
}

fn which(name: &str) -> Option<PathBuf> {
    which_in(name, env::var_os("PATH"))
}

fn which_in(name: &str, paths: Option<OsString>) -> Option<PathBuf> {
    let paths = paths?;
    env::split_paths(&paths).find_map(|dir| {
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let path = dir.join(format!("{name}.{ext}"));
                if path.exists() {
                    return Some(path);
                }
            }
        }
        None
    })
}

pub fn system_binary(name: &str) -> Result<PathBuf> {
    which(name).ok_or_else(|| crate::Error::SystemMissing(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use assert_fs::TempDir;

    use crate::{Registry, Store};

    use super::*;

    #[test]
    fn command_resolution_order_is_explicit_shim_path() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        fs::create_dir_all(store.bin_dir()).unwrap();
        let path_dir = dir.path().join("path");
        fs::create_dir_all(&path_dir).unwrap();

        let explicit = dir.path().join(executable_name("explicit"));
        let shim = store.bin_dir().join(executable_name("demo"));
        let path_bin = path_dir.join(executable_name("demo"));
        fs::write(&explicit, b"explicit").unwrap();
        fs::write(&shim, b"shim").unwrap();
        fs::write(&path_bin, b"path").unwrap();
        let paths = env::join_paths([path_dir]).unwrap();

        let resolved = command_with_paths(
            &store,
            PkgKind::Lsp,
            explicit.to_str().unwrap(),
            Some(paths.clone()),
        )
        .unwrap();
        assert_eq!(resolved.source, CommandSource::Explicit);
        assert_eq!(resolved.path, explicit);

        let resolved = command_with_paths(&store, PkgKind::Lsp, "demo", Some(paths)).unwrap();
        assert_eq!(resolved.source, CommandSource::Shim);
        assert_eq!(resolved.path, shim);

        fs::remove_file(store.bin_dir().join(executable_name("demo"))).unwrap();
        let paths = env::join_paths([dir.path().join("path")]).unwrap();
        let resolved = command_with_paths(&store, PkgKind::Lsp, "demo", Some(paths)).unwrap();
        assert_eq!(resolved.source, CommandSource::Path);
        assert_eq!(resolved.path, path_bin);
    }

    #[test]
    fn missing_command_suggestion_is_actionable_and_language_aware() {
        let mut registry = Registry::default();
        registry
            .insert_str(
                "archive",
                r#"
name = "rust-analyzer"
kind = "lsp"
description = "Rust"
languages = ["rust"]

[[artifact]]
os = "windows"
arch = "x86_64"
source = { archive = "file:///ra.zip" }
bin = "rust-analyzer"
"#,
            )
            .unwrap();
        registry
            .insert_str(
                "system",
                r#"
name = "clangd"
kind = "lsp"
description = "Clangd"
languages = ["c"]

[[artifact]]
os = "windows"
arch = "x86_64"
source = { system = "clangd" }
bin = "clangd"
"#,
            )
            .unwrap();

        assert_eq!(
            package_for_missing_command(&registry, PkgKind::Lsp, Some("rust"), "unknown")
                .unwrap()
                .name,
            "rust-analyzer"
        );
        assert_eq!(
            package_for_missing_command(&registry, PkgKind::Lsp, None, "rust-analyzer")
                .unwrap()
                .name,
            "rust-analyzer"
        );
        assert!(
            package_for_missing_command(&registry, PkgKind::Lsp, Some("c"), "clangd").is_none()
        );
    }

    #[test]
    fn nudge_session_emits_once_per_server_package() {
        let mut session = NudgeSession::default();
        assert!(session.should_emit("rust-analyzer", "rust-analyzer"));
        assert!(!session.should_emit("rust-analyzer", "rust-analyzer"));
        assert!(session.should_emit("rust-analyzer", "other"));
    }

    fn executable_name(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        }
    }
}
