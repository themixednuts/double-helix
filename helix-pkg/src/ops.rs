use std::{
    ffi::OsStr,
    fs,
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::read::GzDecoder;
use serde::Deserialize;
use tempfile::TempDir;
use xz2::read::XzDecoder;

use crate::{
    io as pkg_io,
    lock::{Lock, LockedPackage, Manifest},
    registry::Registry,
    resolve,
    spec::{Artifact, PackageSpec, PkgKind, Source},
    store::{hash_tree, Receipt, Store},
    Error, Result,
};

#[derive(Debug, Clone)]
pub enum OpEvent {
    Started { name: String },
    Progress { name: String, message: String },
    Done { name: String },
    Failed { name: String, message: String },
}

pub type Progress<'a> = dyn FnMut(OpEvent) + 'a;

#[derive(Debug)]
pub struct Ops {
    registry: Registry,
    store: Store,
    config_dir: PathBuf,
}

impl Ops {
    pub fn new(registry: Registry, store: Store, config_dir: PathBuf) -> Self {
        Self {
            registry,
            store,
            config_dir,
        }
    }

    pub fn open_default() -> Result<Self> {
        let config_dir = config_dir();
        Ok(Self::new(
            Registry::builtin()?,
            Store::open_default(),
            config_dir,
        ))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn install(&self, names: &[String], progress: &mut Progress<'_>) -> Result<Lock> {
        self.store.prepare()?;
        let lock_path = self.config_dir.join("pkg.lock");
        let mut lock = read_lock_or_default(&lock_path)?;
        for name in names {
            let package = self
                .registry
                .find(name)
                .ok_or_else(|| Error::NotFound(name.clone()))?;
            progress(OpEvent::Started { name: name.clone() });
            match self.install_package(package, None, progress) {
                Ok(locked) => {
                    lock.upsert(locked);
                    progress(OpEvent::Done { name: name.clone() });
                }
                Err(err) => {
                    progress(OpEvent::Failed {
                        name: name.clone(),
                        message: err.to_string(),
                    });
                    return Err(err);
                }
            }
        }
        lock.write(&lock_path)?;
        Ok(lock)
    }

    pub fn remove(&self, names: &[String]) -> Result<()> {
        for name in names {
            let package = self
                .registry
                .find(name)
                .ok_or_else(|| Error::NotFound(name.clone()))?;
            self.store.remove(package.kind, &package.name)?;
        }
        Ok(())
    }

    pub fn sync(&self, progress: &mut Progress<'_>) -> Result<()> {
        self.store.prepare()?;
        let manifest_path = self.config_dir.join("pkg.toml");
        let lock_path = self.config_dir.join("pkg.lock");
        let manifest = Manifest::read(&manifest_path)?;
        let lock = Lock::read(&lock_path)?;
        for (kind, name) in manifest.packages() {
            let package = self
                .registry
                .get(kind, name)
                .ok_or_else(|| Error::NotFound(name.to_owned()))?;
            let locked = lock
                .find(kind, name)
                .ok_or_else(|| Error::Message(format!("{name} is missing from pkg.lock")))?;
            progress(OpEvent::Started {
                name: name.to_owned(),
            });
            self.install_package(package, Some(locked), progress)?;
            progress(OpEvent::Done {
                name: name.to_owned(),
            });
        }
        Ok(())
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let mut report = DoctorReport::default();
        for receipt in self.store.receipts()? {
            match self.store.verify(&receipt) {
                Ok(()) => report.ok.push(receipt.name),
                Err(err) => report.bad.push((receipt.name, err.to_string())),
            }
        }
        Ok(report)
    }

    pub fn outdated(&self, names: &[String]) -> Result<Vec<OutdatedPackage>> {
        let receipts = self.store.receipts()?;
        let mut reports = Vec::new();
        for receipt in receipts {
            if !names.is_empty() && !names.iter().any(|name| name == &receipt.name) {
                continue;
            }
            let Some(package) = self.registry.get(receipt.kind, &receipt.name) else {
                reports.push(OutdatedPackage {
                    name: receipt.name,
                    kind: receipt.kind,
                    installed: receipt.version,
                    latest: None,
                    error: Some("package is no longer in the registry".to_owned()),
                });
                continue;
            };
            let artifact = package.artifact()?;
            match latest_version(package, artifact) {
                Ok(latest) => reports.push(OutdatedPackage {
                    name: receipt.name,
                    kind: receipt.kind,
                    installed: receipt.version,
                    latest: Some(latest),
                    error: None,
                }),
                Err(err) => reports.push(OutdatedPackage {
                    name: receipt.name,
                    kind: receipt.kind,
                    installed: receipt.version,
                    latest: None,
                    error: Some(err.to_string()),
                }),
            }
        }
        Ok(reports)
    }

    pub fn update(&self, names: &[String], progress: &mut Progress<'_>) -> Result<Lock> {
        self.store.prepare()?;
        let lock_path = self.config_dir.join("pkg.lock");
        let mut lock = read_lock_or_default(&lock_path)?;
        let packages = if names.is_empty() {
            self.store
                .receipts()?
                .into_iter()
                .map(|receipt| receipt.name)
                .collect()
        } else {
            names.to_vec()
        };

        for name in packages {
            let package = self
                .registry
                .find(&name)
                .ok_or_else(|| Error::NotFound(name.clone()))?;
            let artifact = package.artifact()?;
            let current = self.active_receipt(package.kind, &package.name)?;
            let latest = latest_version(package, artifact)?;
            if current
                .as_ref()
                .is_some_and(|receipt| receipt.version == latest)
            {
                progress(OpEvent::Progress {
                    name,
                    message: format!("already at {latest}"),
                });
                continue;
            }
            progress(OpEvent::Started { name: name.clone() });
            let locked = self.install_package(package, None, progress)?;
            lock.upsert(locked);
            progress(OpEvent::Done { name });
        }
        lock.write(&lock_path)?;
        Ok(lock)
    }

    pub fn rollback(&self, name: &str) -> Result<LockedPackage> {
        self.store.prepare()?;
        let package = self
            .registry
            .find(name)
            .ok_or_else(|| Error::NotFound(name.to_owned()))?;
        let current = self
            .active_receipt(package.kind, &package.name)?
            .ok_or_else(|| Error::Message(format!("{name} is not installed")))?;
        let previous = current
            .previous_version
            .clone()
            .ok_or_else(|| Error::Message(format!("{name} has no previous version to rollback")))?;
        let install_dir = self
            .store
            .install_dir(package.kind, &package.name, &previous);
        if !install_dir.exists() {
            return Err(Error::Message(format!(
                "previous version {previous} for {name} is no longer present"
            )));
        }
        let artifact = package.artifact()?;
        let mut receipt = Receipt {
            name: package.name.clone(),
            kind: package.kind,
            version: previous,
            source: current.source.clone(),
            url: current.url.clone(),
            archive_sha256: current.archive_sha256.clone(),
            bin: current.bin.clone(),
            shim: String::new(),
            previous_version: Some(current.version),
            files: hash_tree(&install_dir)?,
            installed_at: timestamp(),
        };
        self.activate_installed(package, artifact, &install_dir, &mut receipt)?;
        self.store.write_receipt(&receipt)?;

        let locked = LockedPackage {
            name: receipt.name,
            kind: receipt.kind,
            version: receipt.version,
            previous_version: receipt.previous_version,
            source: receipt.source,
            url: receipt.url,
            sha256: receipt.archive_sha256,
            bin: receipt.bin,
        };
        let lock_path = self.config_dir.join("pkg.lock");
        let mut lock = read_lock_or_default(&lock_path)?;
        lock.upsert(locked.clone());
        lock.write(&lock_path)?;
        Ok(locked)
    }

    fn install_package(
        &self,
        package: &PackageSpec,
        locked: Option<&LockedPackage>,
        progress: &mut Progress<'_>,
    ) -> Result<LockedPackage> {
        let artifact = package.artifact()?;
        let previous_version = self
            .active_receipt(package.kind, &package.name)?
            .map(|receipt| receipt.version);

        if let Some(command) = &artifact.source.system {
            let path = resolve::system_binary(command)?;
            let mut receipt = Receipt {
                name: package.name.clone(),
                kind: package.kind,
                version: "system".to_owned(),
                source: "system".to_owned(),
                url: path.display().to_string(),
                archive_sha256: String::new(),
                bin: artifact.bin.clone(),
                shim: String::new(),
                previous_version,
                files: Default::default(),
                installed_at: timestamp(),
            };
            self.store.activate(&mut receipt, &path)?;
            self.store.write_receipt(&receipt)?;
            return Ok(lock_from_receipt(receipt));
        }

        let resolved = if let Some(locked) = locked {
            ResolvedSource {
                version: locked.version.clone(),
                url: locked.url.clone(),
                sha256: Some(locked.sha256.clone()),
                source: locked.source.clone(),
            }
        } else {
            resolve_source(package, artifact, progress)?
        };

        let install_dir = self
            .store
            .install_dir(package.kind, &package.name, &resolved.version);
        let install_parent = install_dir
            .parent()
            .ok_or_else(|| Error::Message("invalid store path".to_owned()))?;
        fs::create_dir_all(install_parent)
            .map_err(|source| pkg_io(install_parent.display(), source))?;

        let mut installed_archive_sha256 = None;
        if !install_dir.exists() {
            let tmp = TempDir::new_in(install_parent)
                .map_err(|source| pkg_io(install_dir.display(), source))?;
            installed_archive_sha256 = install_into(
                package,
                artifact,
                &resolved,
                tmp.path(),
                &self.store,
                progress,
            )?;
            fs::rename(tmp.path(), &install_dir)
                .map_err(|source| pkg_io(install_dir.display(), source))?;
        }

        let mut receipt = Receipt {
            name: package.name.clone(),
            kind: package.kind,
            version: resolved.version.clone(),
            source: resolved.source.clone(),
            url: resolved.url.clone(),
            archive_sha256: installed_archive_sha256
                .or_else(|| resolved.sha256.clone())
                .unwrap_or_default(),
            bin: artifact.bin.clone(),
            shim: String::new(),
            previous_version,
            files: hash_tree(&install_dir)?,
            installed_at: timestamp(),
        };
        self.activate_installed(package, artifact, &install_dir, &mut receipt)?;
        self.store.write_receipt(&receipt)?;

        Ok(lock_from_receipt(receipt))
    }

    fn active_receipt(&self, kind: PkgKind, name: &str) -> Result<Option<Receipt>> {
        let path = self.store.receipt_path(kind, name);
        if path.exists() {
            Receipt::read(&path).map(Some)
        } else {
            Ok(None)
        }
    }

    fn activate_installed(
        &self,
        _package: &PackageSpec,
        artifact: &Artifact,
        install_dir: &Path,
        receipt: &mut Receipt,
    ) -> Result<()> {
        let source = &artifact.source;
        if source.git.is_some() {
            return Ok(());
        }
        if source.npm.is_some() {
            if let Some(bin_js) = &source.bin_js {
                let node = required_tool("node", "Node.js is required for npm packages")?;
                let package_dir = npm_package_dir(install_dir, source.npm.as_deref().unwrap());
                return self
                    .store
                    .activate_command(receipt, &node, &package_dir.join(bin_js));
            }
            let bin = source.bin.as_deref().unwrap_or(&artifact.bin);
            return self.store.activate(
                receipt,
                &with_windows_cmd(&install_dir.join("node_modules").join(".bin").join(bin)),
            );
        }
        if source.pip.is_some() {
            let target = python_venv_bin(install_dir, &artifact.bin);
            return self.store.activate(receipt, &target);
        }
        let target = produced_bin(install_dir, &artifact.bin, source);
        self.store.activate(receipt, &target)
    }
}

#[derive(Debug, Default)]
pub struct DoctorReport {
    pub ok: Vec<String>,
    pub bad: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedPackage {
    pub name: String,
    pub kind: PkgKind,
    pub installed: String,
    pub latest: Option<String>,
    pub error: Option<String>,
}

impl OutdatedPackage {
    pub fn is_outdated(&self) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|latest| latest != &self.installed)
    }
}

#[derive(Debug)]
struct ResolvedSource {
    version: String,
    url: String,
    sha256: Option<String>,
    source: String,
}

fn install_into(
    package: &PackageSpec,
    artifact: &Artifact,
    resolved: &ResolvedSource,
    dest: &Path,
    store: &Store,
    progress: &mut Progress<'_>,
) -> Result<Option<String>> {
    let source = &artifact.source;
    if source.github_release.is_some() || source.archive.is_some() {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("downloading {}", resolved.url),
        });
        let archive = download(&resolved.url)?;
        let actual = sha256_bytes(&archive);
        if let Some(expected) = resolved.sha256.as_deref().or(source.sha256.as_deref()) {
            if expected != actual {
                return Err(Error::HashMismatch {
                    path: resolved.url.clone(),
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }
        unpack(&resolved.url, &archive, dest, &artifact.bin)?;
        return Ok(Some(actual));
    }

    if let Some(package_name) = &source.npm {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("npm install {package_name}@{}", resolved.version),
        });
        install_npm(package_name, &resolved.version, dest, store)?;
        return Ok(None);
    }

    if let Some(package_name) = &source.pip {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("pip install {package_name}=={}", resolved.version),
        });
        install_pip(package_name, &resolved.version, dest)?;
        return Ok(None);
    }

    if let Some(crate_name) = &source.cargo {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("cargo install {crate_name} {}", resolved.version),
        });
        install_cargo(crate_name, &resolved.version, &source.features, dest)?;
        return Ok(None);
    }

    if let Some(module) = &source.go {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("go install {module}@{}", resolved.version),
        });
        install_go(module, &resolved.version, dest)?;
        return Ok(None);
    }

    if let Some(remote) = &source.git {
        let rev = source
            .rev
            .as_deref()
            .ok_or_else(|| Error::Message("git source missing rev".to_owned()))?;
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("building grammar {remote}@{rev}"),
        });
        helix_loader::grammar::install_pkg_grammar(
            &package.name,
            remote,
            rev,
            source.subpath.as_deref(),
            dest,
        )
        .map_err(|err| Error::Message(err.to_string()))?;
        return Ok(None);
    }

    Err(Error::UnsupportedArchive(source.kind().to_owned()))
}

fn resolve_source(
    package: &PackageSpec,
    artifact: &Artifact,
    progress: &mut Progress<'_>,
) -> Result<ResolvedSource> {
    let source = &artifact.source;
    if let Some(repo) = &source.github_release {
        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("querying github release {repo}"),
        });
        let release = github_latest(repo)?;
        let pattern = source
            .asset
            .as_ref()
            .ok_or_else(|| Error::Message("github source missing asset".to_owned()))?;
        let asset_name = expand_asset(pattern, &release.tag_name);
        let asset = release
            .assets
            .iter()
            .find(|asset| {
                asset.name == asset_name || wildcard(pattern, &asset.name, &release.tag_name)
            })
            .ok_or_else(|| {
                Error::Message(format!(
                    "release {repo}@{} has no asset matching {pattern}",
                    release.tag_name
                ))
            })?;
        return Ok(ResolvedSource {
            version: release.tag_name,
            url: asset.browser_download_url.clone(),
            sha256: source.sha256.clone(),
            source: "github-release".to_owned(),
        });
    }

    if let Some(url) = &source.archive {
        let version = latest_version(package, artifact)?;
        return Ok(ResolvedSource {
            version: version.clone(),
            url: expand_asset(url, &version),
            sha256: source.sha256.clone(),
            source: "archive".to_owned(),
        });
    }

    let version = latest_version(package, artifact)?;
    Ok(ResolvedSource {
        version: version.clone(),
        url: format!("{}:{version}", source.kind()),
        sha256: None,
        source: source.kind().to_owned(),
    })
}

fn latest_version(package: &PackageSpec, artifact: &Artifact) -> Result<String> {
    if let Some(tag_source) = package.version.tag_source.as_deref() {
        if let Some(version) = tag_source.strip_prefix("static:") {
            return Ok(version.to_owned());
        }
        if let Some(repo) = tag_source.strip_prefix("github:") {
            return Ok(github_latest(repo)?.tag_name);
        }
        if let Some(package_name) = tag_source.strip_prefix("npm:") {
            return npm_latest(package_name);
        }
        if let Some(package_name) = tag_source.strip_prefix("pip:") {
            return pip_latest(package_name);
        }
        if let Some(crate_name) = tag_source.strip_prefix("crates:") {
            return crates_latest(crate_name);
        }
        if let Some(module) = tag_source.strip_prefix("go:") {
            return go_latest(module);
        }
    }

    let source = &artifact.source;
    if let Some(package_name) = &source.npm {
        npm_latest(package_name)
    } else if let Some(package_name) = &source.pip {
        pip_latest(package_name)
    } else if let Some(crate_name) = &source.cargo {
        crates_latest(crate_name)
    } else if let Some(module) = &source.go {
        go_latest(module)
    } else if source.git.is_some() {
        source
            .rev
            .clone()
            .ok_or_else(|| Error::Message("git source missing rev".to_owned()))
    } else if source.system.is_some() {
        Ok("system".to_owned())
    } else {
        Ok("archive".to_owned())
    }
}

fn install_npm(package: &str, version: &str, dest: &Path, store: &Store) -> Result<()> {
    let _node = required_tool("node", "Node.js is required for npm packages")?;
    let npm = required_tool("npm", "npm is required for npm packages")?;
    fs::create_dir_all(store.runtime_dir("node").join("cache"))
        .map_err(|source| pkg_io(store.runtime_dir("node").display(), source))?;
    let package_spec = format!("{package}@{version}");
    let args = vec![
        "install".to_owned(),
        "--prefix".to_owned(),
        dest.display().to_string(),
        "--ignore-scripts".to_owned(),
        package_spec,
    ];
    let mut command = Command::new(&npm);
    command
        .args(&args)
        .env("npm_config_cache", store.runtime_dir("node").join("cache"));
    run_command(npm.as_os_str(), &args, &mut command)
}

fn install_pip(package: &str, version: &str, dest: &Path) -> Result<()> {
    let python = python_tool()?;
    let venv_args = vec![
        "-m".to_owned(),
        "venv".to_owned(),
        dest.display().to_string(),
    ];
    let mut venv = Command::new(&python);
    venv.args(&venv_args);
    run_command(python.as_os_str(), &venv_args, &mut venv)?;

    let venv_python = python_venv_python(dest);
    let install_spec = format!("{package}=={version}");
    let pip_args = vec![
        "-m".to_owned(),
        "pip".to_owned(),
        "install".to_owned(),
        install_spec,
    ];
    let mut pip = Command::new(&venv_python);
    pip.args(&pip_args);
    run_command(venv_python.as_os_str(), &pip_args, &mut pip)
}

fn install_cargo(crate_name: &str, version: &str, features: &[String], dest: &Path) -> Result<()> {
    let cargo = required_tool("cargo", "Cargo is required for cargo packages")?;
    let mut args = vec![
        "install".to_owned(),
        crate_name.to_owned(),
        "--root".to_owned(),
        dest.display().to_string(),
        "--locked".to_owned(),
        "--version".to_owned(),
        version.to_owned(),
    ];
    if !features.is_empty() {
        args.push("--features".to_owned());
        args.push(features.join(","));
    }
    let mut command = Command::new(&cargo);
    command.args(&args);
    run_command(cargo.as_os_str(), &args, &mut command)
}

fn install_go(module: &str, version: &str, dest: &Path) -> Result<()> {
    let go = required_tool("go", "Go is required for go packages")?;
    fs::create_dir_all(dest).map_err(|source| pkg_io(dest.display(), source))?;
    let args = vec!["install".to_owned(), format!("{module}@{version}")];
    let mut command = Command::new(&go);
    command.args(&args).env("GOBIN", dest);
    run_command(go.as_os_str(), &args, &mut command)
}

fn required_tool(name: &str, context: &str) -> Result<PathBuf> {
    resolve::system_binary(name).map_err(|_| {
        Error::Message(format!(
            "{context}; install `{name}` and ensure it is available on PATH"
        ))
    })
}

fn python_tool() -> Result<PathBuf> {
    resolve::system_binary(if cfg!(windows) { "python" } else { "python3" })
        .or_else(|_| resolve::system_binary("python"))
        .map_err(|_| {
            Error::Message(
                "Python is required for pip packages; install python and ensure it is available on PATH"
                    .to_owned(),
            )
        })
}

fn run_command(program: &OsStr, args: &[String], command: &mut Command) -> Result<()> {
    let output = command
        .output()
        .map_err(|source| pkg_io(program.to_string_lossy(), source))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            program: program.to_string_lossy().into_owned(),
            args: args.join(" "),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn command_stdout(program: &Path, args: &[String]) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    let output = command
        .output()
        .map_err(|source| pkg_io(program.display(), source))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(Error::CommandFailed {
            program: program.display().to_string(),
            args: args.join(" "),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn produced_bin(install_dir: &Path, bin: &str, source: &Source) -> PathBuf {
    if source.cargo.is_some() {
        with_windows_exe(&install_dir.join("bin").join(bin))
    } else if source.go.is_some() {
        with_windows_exe(&install_dir.join(bin))
    } else {
        install_dir.join(bin)
    }
}

fn python_venv_bin(venv: &Path, bin: &str) -> PathBuf {
    let dir = if cfg!(windows) { "Scripts" } else { "bin" };
    with_windows_exe(&venv.join(dir).join(bin))
}

fn python_venv_python(venv: &Path) -> PathBuf {
    let dir = if cfg!(windows) { "Scripts" } else { "bin" };
    with_windows_exe(&venv.join(dir).join("python"))
}

fn with_windows_exe(path: &Path) -> PathBuf {
    if cfg!(windows) && path.extension().is_none() {
        path.with_extension("exe")
    } else {
        path.to_owned()
    }
}

fn with_windows_cmd(path: &Path) -> PathBuf {
    if cfg!(windows) && path.extension().is_none() {
        path.with_extension("cmd")
    } else {
        path.to_owned()
    }
}

fn npm_package_dir(prefix: &Path, package: &str) -> PathBuf {
    package
        .split('/')
        .fold(prefix.join("node_modules"), |path, part| path.join(part))
}

fn read_lock_or_default(path: &Path) -> Result<Lock> {
    if path.exists() {
        Lock::read(path)
    } else {
        Ok(Lock::default())
    }
}

fn lock_from_receipt(receipt: Receipt) -> LockedPackage {
    LockedPackage {
        name: receipt.name,
        kind: receipt.kind,
        version: receipt.version,
        previous_version: receipt.previous_version,
        source: receipt.source,
        url: receipt.url,
        sha256: receipt.archive_sha256,
        bin: receipt.bin,
    }
}

fn expand_asset(pattern: &str, tag: &str) -> String {
    let version = tag.trim_start_matches('v');
    pattern.replace("{tag}", tag).replace("{version}", version)
}

fn wildcard(pattern: &str, name: &str, tag: &str) -> bool {
    let expanded = expand_asset(pattern, tag);
    let Some((prefix, suffix)) = expanded.split_once('*') else {
        return false;
    };
    name.starts_with(prefix) && name.ends_with(suffix)
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn github_latest(repo: &str) -> Result<GithubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let bytes = http_get(&url)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json { url, source })
}

fn npm_latest(package: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct NpmLatest {
        version: String,
    }
    let escaped = package.replace('/', "%2f");
    let url = format!("https://registry.npmjs.org/{escaped}/latest");
    let bytes = http_get(&url)?;
    let latest: NpmLatest =
        serde_json::from_slice(&bytes).map_err(|source| Error::Json { url, source })?;
    Ok(latest.version)
}

fn pip_latest(package: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct PipJson {
        info: PipInfo,
    }
    #[derive(Deserialize)]
    struct PipInfo {
        version: String,
    }
    let url = format!("https://pypi.org/pypi/{package}/json");
    let bytes = http_get(&url)?;
    let latest: PipJson =
        serde_json::from_slice(&bytes).map_err(|source| Error::Json { url, source })?;
    Ok(latest.info.version)
}

fn crates_latest(crate_name: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct CratesJson {
        #[serde(rename = "crate")]
        krate: CrateInfo,
    }
    #[derive(Deserialize)]
    struct CrateInfo {
        newest_version: String,
        max_stable_version: Option<String>,
    }
    let url = format!("https://crates.io/api/v1/crates/{crate_name}");
    let bytes = http_get(&url)?;
    let latest: CratesJson =
        serde_json::from_slice(&bytes).map_err(|source| Error::Json { url, source })?;
    Ok(latest
        .krate
        .max_stable_version
        .unwrap_or(latest.krate.newest_version))
}

fn go_latest(module: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct GoList {
        #[serde(rename = "Version")]
        version: String,
    }
    let go = required_tool("go", "Go is required to query go package versions")?;
    let args = vec![
        "list".to_owned(),
        "-m".to_owned(),
        "-json".to_owned(),
        format!("{module}@latest"),
    ];
    let stdout = command_stdout(&go, &args)?;
    let latest: GoList = serde_json::from_str(&stdout).map_err(|source| Error::Json {
        url: format!("go list -m -json {module}@latest"),
        source,
    })?;
    Ok(latest.version)
}

fn download(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|source| pkg_io(path, source));
    }
    http_get(url)
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .set("User-Agent", "dhx-pkg")
        .call()
        .map_err(|source| Error::Http {
            url: url.to_owned(),
            source: Box::new(source),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|source| pkg_io(url, source))?;
    Ok(bytes)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn unpack(url: &str, bytes: &[u8], dest: &Path, bin: &str) -> Result<()> {
    fs::create_dir_all(dest).map_err(|source| pkg_io(dest.display(), source))?;
    if url.ends_with(".zip") || url.ends_with(".vsix") {
        unpack_zip(url, bytes, dest)
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(dest)
            .map_err(|source| pkg_io(dest.display(), source))
    } else if url.ends_with(".tar.xz") {
        let decoder = XzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(dest)
            .map_err(|source| pkg_io(dest.display(), source))
    } else if url.ends_with(".gz") {
        let mut decoder = GzDecoder::new(Cursor::new(bytes));
        write_single_file(&mut decoder, &dest.join(bin))
    } else {
        write_single_file(&mut Cursor::new(bytes), &dest.join(bin))
    }
}

fn unpack_zip(url: &str, bytes: &[u8], dest: &Path) -> Result<()> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|source| Error::Zip {
        path: url.to_owned(),
        source,
    })?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|source| Error::Zip {
            path: url.to_owned(),
            source,
        })?;
        let Some(enclosed) = file.enclosed_name() else {
            continue;
        };
        let out = dest.join(enclosed);
        if file.is_dir() {
            fs::create_dir_all(&out).map_err(|source| pkg_io(out.display(), source))?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|source| pkg_io(parent.display(), source))?;
            }
            let mut writer =
                fs::File::create(&out).map_err(|source| pkg_io(out.display(), source))?;
            io::copy(&mut file, &mut writer).map_err(|source| pkg_io(out.display(), source))?;
        }
    }
    Ok(())
}

fn write_single_file(reader: &mut dyn Read, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| pkg_io(parent.display(), source))?;
    }
    let mut file = fs::File::create(path).map_err(|source| pkg_io(path.display(), source))?;
    io::copy(reader, &mut file).map_err(|source| pkg_io(path.display(), source))?;
    file.flush()
        .map_err(|source| pkg_io(path.display(), source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = file
            .metadata()
            .map_err(|source| pkg_io(path.display(), source))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|source| pkg_io(path.display(), source))?;
    }
    Ok(())
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

fn config_dir() -> PathBuf {
    use etcetera::base_strategy::{choose_base_strategy, BaseStrategy};
    choose_base_strategy()
        .expect("Unable to find the config directory!")
        .config_dir()
        .join("double-helix")
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use assert_fs::TempDir;
    use zip::{write::SimpleFileOptions, ZipWriter};

    use crate::{lock::Manifest, registry::Registry, spec::PkgKind, Store};

    use super::*;

    #[test]
    fn install_activate_remove_round_trip_with_local_archive() {
        let dir = TempDir::new().unwrap();
        let archive = dir.path().join("demo.zip");
        make_zip(&archive, "bin/demo.exe", b"demo");
        let mut registry = Registry::default();
        registry
            .insert_str(
                "demo",
                &format!(
                    r#"
name = "demo"
kind = "lsp"
description = "Demo"
languages = ["demo"]

[version]
tag-source = "static:1"

[[artifact]]
os = "{}"
arch = "{}"
source = {{ archive = "file://{}" }}
bin = "bin/demo.exe"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    archive.display().to_string().replace('\\', "/")
                ),
            )
            .unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let ops = Ops::new(registry, store.clone(), dir.path().join("config"));
        let lock = ops
            .install(&["demo".to_owned()], &mut |_| {})
            .expect("install succeeds");
        assert_eq!(lock.packages[0].name, "demo");
        let receipts = store.receipts().unwrap();
        assert_eq!(receipts.len(), 1);
        store.verify(&receipts[0]).unwrap();
        let shim = store.bin_dir().join(&receipts[0].shim);
        assert!(shim.exists());
        if cfg!(windows) {
            assert_eq!(fs::read(&shim).unwrap(), b"demo");
        }
        ops.remove(&["demo".to_owned()]).unwrap();
        assert!(!store.receipt_path(PkgKind::Lsp, "demo").exists());
    }

    #[test]
    fn lock_sync_round_trip_offline() {
        let dir = TempDir::new().unwrap();
        let archive = dir.path().join("demo.zip");
        make_zip(&archive, "demo.exe", b"demo");
        let mut registry = Registry::default();
        registry
            .insert_str(
                "demo",
                &format!(
                    r#"
name = "demo"
kind = "lsp"
description = "Demo"

[version]
tag-source = "static:1"

[[artifact]]
os = "{}"
arch = "{}"
source = {{ archive = "file://{}" }}
bin = "demo.exe"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    archive.display().to_string().replace('\\', "/")
                ),
            )
            .unwrap();
        let config = dir.path().join("config");
        Manifest {
            lsp: vec!["demo".to_owned()],
            ..Manifest::default()
        }
        .write(&config.join("pkg.toml"))
        .unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let ops = Ops::new(registry, store, config);
        ops.install(&["demo".to_owned()], &mut |_| {}).unwrap();
        ops.sync(&mut |_| {}).unwrap();
    }

    #[test]
    fn doctor_detects_corrupted_store_file() {
        let dir = TempDir::new().unwrap();
        let archive = dir.path().join("demo.zip");
        make_zip(&archive, "demo.exe", b"demo");
        let mut registry = Registry::default();
        registry
            .insert_str(
                "demo",
                &format!(
                    r#"
name = "demo"
kind = "lsp"
description = "Demo"

[version]
tag-source = "static:1"

[[artifact]]
os = "{}"
arch = "{}"
source = {{ archive = "file://{}" }}
bin = "demo.exe"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    archive.display().to_string().replace('\\', "/")
                ),
            )
            .unwrap();
        let store = Store::open(dir.path().join("pkg"));
        let ops = Ops::new(registry, store.clone(), dir.path().join("config"));
        ops.install(&["demo".to_owned()], &mut |_| {}).unwrap();
        fs::write(
            store
                .install_dir(PkgKind::Lsp, "demo", "1")
                .join("demo.exe"),
            b"bad",
        )
        .unwrap();
        let report = ops.doctor().unwrap();
        assert_eq!(report.bad.len(), 1);
    }

    #[test]
    fn update_and_outdated_use_static_version_source_offline() {
        let dir = TempDir::new().unwrap();
        let archive1 = dir.path().join("demo-1.zip");
        let archive2 = dir.path().join("demo-2.zip");
        make_zip(&archive1, "demo.exe", b"one");
        make_zip(&archive2, "demo.exe", b"two");
        let store = Store::open(dir.path().join("pkg"));
        let config = dir.path().join("config");

        let ops = Ops::new(
            registry_for_archive("1", &archive1),
            store.clone(),
            config.clone(),
        );
        ops.install(&["demo".to_owned()], &mut |_| {}).unwrap();

        let ops = Ops::new(registry_for_archive("2", &archive2), store, config);
        let outdated = ops.outdated(&[]).unwrap();
        assert_eq!(outdated.len(), 1);
        assert!(outdated[0].is_outdated());
        ops.update(&[], &mut |_| {}).unwrap();
        let receipt = ops.store().receipts().unwrap().into_iter().next().unwrap();
        assert_eq!(receipt.version, "2");
        assert_eq!(receipt.previous_version.as_deref(), Some("1"));
    }

    #[test]
    fn rollback_reactivates_previous_version() {
        let dir = TempDir::new().unwrap();
        let archive1 = dir.path().join("demo-1.zip");
        let archive2 = dir.path().join("demo-2.zip");
        make_zip(&archive1, "demo.exe", b"one");
        make_zip(&archive2, "demo.exe", b"two");
        let store = Store::open(dir.path().join("pkg"));
        let config = dir.path().join("config");
        let ops = Ops::new(
            registry_for_archive("1", &archive1),
            store.clone(),
            config.clone(),
        );
        ops.install(&["demo".to_owned()], &mut |_| {}).unwrap();
        let ops = Ops::new(registry_for_archive("2", &archive2), store, config);
        ops.update(&[], &mut |_| {}).unwrap();
        let rolled = ops.rollback("demo").unwrap();
        assert_eq!(rolled.version, "1");
        let receipt = ops.store().receipts().unwrap().pop().unwrap();
        assert_eq!(receipt.version, "1");
        assert_eq!(receipt.previous_version.as_deref(), Some("2"));
    }

    #[test]
    fn node_bin_js_activation_writes_runtime_wrapper() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let install = store.install_dir(PkgKind::Lsp, "demo", "1");
        fs::create_dir_all(install.join("node_modules/demo/bin")).unwrap();
        fs::write(install.join("node_modules/demo/bin/server.js"), b"").unwrap();
        let package = package_from_str(
            r#"
name = "demo"
kind = "lsp"
description = "Demo"
[version]
tag-source = "static:1"
[[artifact]]
os = "windows"
arch = "x86_64"
source = { npm = "demo", bin-js = "bin/server.js" }
bin = "demo"
"#,
        );
        let ops = Ops::new(
            Registry::default(),
            store.clone(),
            dir.path().join("config"),
        );
        let mut receipt = Receipt {
            name: "demo".to_owned(),
            kind: PkgKind::Lsp,
            version: "1".to_owned(),
            source: "npm".to_owned(),
            url: "npm:1".to_owned(),
            archive_sha256: String::new(),
            bin: "demo".to_owned(),
            shim: String::new(),
            previous_version: None,
            files: Default::default(),
            installed_at: "now".to_owned(),
        };
        let result = ops.activate_installed(
            &package,
            package.artifact_for("windows", "x86_64").unwrap(),
            &install,
            &mut receipt,
        );
        if resolve::system_binary("node").is_ok() {
            result.unwrap();
            let shim = fs::read_to_string(store.bin_dir().join(receipt.shim)).unwrap();
            assert!(shim.contains("server.js"));
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn pip_activation_points_at_venv_script() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pkg"));
        store.prepare().unwrap();
        let install = store.install_dir(PkgKind::Dap, "debugpy", "1");
        let target = python_venv_bin(&install, "debugpy-adapter");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"").unwrap();
        let package = package_from_str(
            r#"
name = "debugpy"
kind = "dap"
description = "debugpy"
[version]
tag-source = "static:1"
[[artifact]]
os = "windows"
arch = "x86_64"
source = { pip = "debugpy" }
bin = "debugpy-adapter"
"#,
        );
        let ops = Ops::new(
            Registry::default(),
            store.clone(),
            dir.path().join("config"),
        );
        let mut receipt = Receipt {
            name: "debugpy".to_owned(),
            kind: PkgKind::Dap,
            version: "1".to_owned(),
            source: "pip".to_owned(),
            url: "pip:1".to_owned(),
            archive_sha256: String::new(),
            bin: "debugpy-adapter".to_owned(),
            shim: String::new(),
            previous_version: None,
            files: Default::default(),
            installed_at: "now".to_owned(),
        };
        ops.activate_installed(
            &package,
            package.artifact_for("windows", "x86_64").unwrap(),
            &install,
            &mut receipt,
        )
        .unwrap();
        assert!(store.bin_dir().join(receipt.shim).exists());
    }

    #[test]
    #[ignore = "uses npm and network; run with: cargo test -p helix-pkg ignored_install_npm -- --ignored"]
    fn ignored_install_npm() {
        let dir = TempDir::new().unwrap();
        let mut registry = Registry::default();
        registry
            .insert_str(
                "bash-language-server",
                &format!(
                    r#"
name = "bash-language-server"
kind = "lsp"
description = "Bash language server"
[version]
tag-source = "npm:bash-language-server"
[[artifact]]
os = "{}"
arch = "{}"
source = {{ npm = "bash-language-server", bin = "bash-language-server" }}
bin = "bash-language-server"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .unwrap();
        let ops = Ops::new(
            registry,
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["bash-language-server".to_owned()], &mut |_| {})
            .unwrap();
    }

    #[test]
    #[ignore = "uses python/pip and network; run with: cargo test -p helix-pkg ignored_install_pip -- --ignored"]
    fn ignored_install_pip() {
        let dir = TempDir::new().unwrap();
        let mut registry = Registry::default();
        registry
            .insert_str(
                "debugpy",
                &format!(
                    r#"
name = "debugpy"
kind = "dap"
description = "Python debug adapter"
[version]
tag-source = "pip:debugpy"
[[artifact]]
os = "{}"
arch = "{}"
source = {{ pip = "debugpy" }}
bin = "debugpy-adapter"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .unwrap();
        let ops = Ops::new(
            registry,
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["debugpy".to_owned()], &mut |_| {}).unwrap();
    }

    #[test]
    #[ignore = "uses cargo and network; run with: cargo test -p helix-pkg ignored_install_cargo -- --ignored"]
    fn ignored_install_cargo() {
        let dir = TempDir::new().unwrap();
        let mut registry = Registry::default();
        registry
            .insert_str(
                "taplo-cargo",
                &format!(
                    r#"
name = "taplo-cargo"
kind = "lsp"
description = "Taplo via cargo"
[version]
tag-source = "crates:taplo-cli"
[[artifact]]
os = "{}"
arch = "{}"
source = {{ cargo = "taplo-cli" }}
bin = "taplo"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .unwrap();
        let ops = Ops::new(
            registry,
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["taplo-cargo".to_owned()], &mut |_| {})
            .unwrap();
    }

    #[test]
    #[ignore = "uses go and network; run with: cargo test -p helix-pkg ignored_install_go -- --ignored"]
    fn ignored_install_go() {
        let dir = TempDir::new().unwrap();
        let mut registry = Registry::default();
        registry
            .insert_str(
                "gopls",
                &format!(
                    r#"
name = "gopls"
kind = "lsp"
description = "Go language server"
[version]
tag-source = "go:golang.org/x/tools/gopls"
[[artifact]]
os = "{}"
arch = "{}"
source = {{ go = "golang.org/x/tools/gopls" }}
bin = "gopls"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
            .unwrap();
        let ops = Ops::new(
            registry,
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["gopls".to_owned()], &mut |_| {}).unwrap();
    }

    #[test]
    #[ignore = "downloads from GitHub; run with: cargo test -p helix-pkg ignored_install_seed -- --ignored"]
    fn ignored_install_seed() {
        let dir = TempDir::new().unwrap();
        let ops = Ops::new(
            Registry::builtin().unwrap(),
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["marksman".to_owned()], &mut |_| {}).unwrap();
    }

    fn registry_for_archive(version: &str, archive: &Path) -> Registry {
        let mut registry = Registry::default();
        registry
            .insert_str(
                "demo",
                &format!(
                    r#"
name = "demo"
kind = "lsp"
description = "Demo"

[version]
tag-source = "static:{version}"

[[artifact]]
os = "{}"
arch = "{}"
source = {{ archive = "file://{}" }}
bin = "demo.exe"
"#,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    archive.display().to_string().replace('\\', "/")
                ),
            )
            .unwrap();
        registry
    }

    fn package_from_str(content: &str) -> PackageSpec {
        toml::from_str(content).unwrap()
    }

    fn make_zip(path: &Path, name: &str, bytes: &[u8]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }
}
