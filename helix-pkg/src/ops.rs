use std::{
    fs,
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
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
    spec::{Artifact, PackageSpec},
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

    pub fn default() -> Result<Self> {
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
        let mut lock = if lock_path.exists() {
            Lock::read(&lock_path)?
        } else {
            Lock::default()
        };
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

    fn install_package(
        &self,
        package: &PackageSpec,
        locked: Option<&LockedPackage>,
        progress: &mut Progress<'_>,
    ) -> Result<LockedPackage> {
        let artifact = package.artifact()?;
        if let Some(command) = &artifact.source.system {
            let path = resolve::system_binary(command)?;
            let receipt = Receipt {
                name: package.name.clone(),
                kind: package.kind,
                version: "system".to_owned(),
                source: "system".to_owned(),
                url: path.display().to_string(),
                archive_sha256: String::new(),
                bin: artifact.bin.clone(),
                shim: String::new(),
                files: Default::default(),
                installed_at: timestamp(),
            };
            self.store.write_receipt(&receipt)?;
            return Ok(LockedPackage {
                name: package.name.clone(),
                kind: package.kind,
                version: receipt.version,
                source: receipt.source,
                url: receipt.url,
                sha256: receipt.archive_sha256,
                bin: receipt.bin,
            });
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

        progress(OpEvent::Progress {
            name: package.name.clone(),
            message: format!("downloading {}", resolved.url),
        });
        let archive = download(&resolved.url)?;
        let actual = sha256_bytes(&archive);
        if let Some(expected) = resolved
            .sha256
            .as_deref()
            .or(artifact.source.sha256.as_deref())
        {
            if expected != actual {
                return Err(Error::HashMismatch {
                    path: resolved.url,
                    expected: expected.to_owned(),
                    actual,
                });
            }
        }

        let install_dir = self
            .store
            .install_dir(package.kind, &package.name, &resolved.version);
        let install_parent = install_dir
            .parent()
            .ok_or_else(|| Error::Message("invalid store path".to_owned()))?;
        fs::create_dir_all(install_parent)
            .map_err(|source| pkg_io(install_parent.display(), source))?;
        let tmp = TempDir::new_in(install_parent)
            .map_err(|source| pkg_io(install_dir.display(), source))?;
        unpack(&resolved.url, &archive, tmp.path(), &artifact.bin)?;
        if install_dir.exists() {
            fs::remove_dir_all(&install_dir)
                .map_err(|source| pkg_io(install_dir.display(), source))?;
        }
        fs::rename(tmp.path(), &install_dir)
            .map_err(|source| pkg_io(install_dir.display(), source))?;

        let target = install_dir.join(&artifact.bin);
        if !target.exists() {
            return Err(Error::Message(format!(
                "artifact for {} did not contain {}",
                package.name, artifact.bin
            )));
        }

        let mut receipt = Receipt {
            name: package.name.clone(),
            kind: package.kind,
            version: resolved.version,
            source: resolved.source,
            url: resolved.url,
            archive_sha256: actual,
            bin: artifact.bin.clone(),
            shim: String::new(),
            files: hash_tree(&install_dir)?,
            installed_at: timestamp(),
        };
        self.store.activate(&mut receipt, &target)?;
        self.store.write_receipt(&receipt)?;

        Ok(LockedPackage {
            name: receipt.name,
            kind: receipt.kind,
            version: receipt.version,
            source: receipt.source,
            url: receipt.url,
            sha256: receipt.archive_sha256,
            bin: receipt.bin,
        })
    }
}

#[derive(Debug, Default)]
pub struct DoctorReport {
    pub ok: Vec<String>,
    pub bad: Vec<(String, String)>,
}

#[derive(Debug)]
struct ResolvedSource {
    version: String,
    url: String,
    sha256: Option<String>,
    source: String,
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
        let version = package
            .version
            .tag_source
            .as_deref()
            .and_then(|tag| tag.strip_prefix("static:"))
            .unwrap_or("archive")
            .to_owned();
        return Ok(ResolvedSource {
            version,
            url: expand_asset(url, "archive"),
            sha256: source.sha256.clone(),
            source: "archive".to_owned(),
        });
    }

    Err(Error::UnsupportedArchive(source.kind().to_owned()))
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
    #[ignore = "downloads from GitHub; run with: cargo test -p helix-pkg --test '*' ignored_install_seed -- --ignored"]
    fn ignored_install_seed() {
        let dir = TempDir::new().unwrap();
        let ops = Ops::new(
            Registry::builtin().unwrap(),
            Store::open(dir.path().join("pkg")),
            dir.path().join("config"),
        );
        ops.install(&["marksman".to_owned()], &mut |_| {}).unwrap();
    }

    fn make_zip(path: &Path, name: &str, bytes: &[u8]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }
}
