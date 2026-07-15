use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::time::SystemTime;
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::channel,
};
use tempfile::TempPath;
use tree_house::tree_sitter::Grammar;

use crate::assets::DYLIB_EXTENSION;

#[derive(Debug, Serialize, Deserialize)]
struct Configuration {
    #[serde(rename = "use-grammars")]
    pub grammar_selection: Option<GrammarSelection>,
    pub grammar: Vec<GrammarConfiguration>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", untagged)]
pub enum GrammarSelection {
    Only { only: HashSet<String> },
    Except { except: HashSet<String> },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrammarConfiguration {
    #[serde(rename = "name")]
    pub grammar_id: String,
    pub source: GrammarSource,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", untagged)]
pub enum GrammarSource {
    Local {
        path: String,
    },
    Git {
        #[serde(rename = "git")]
        remote: String,
        #[serde(rename = "rev")]
        revision: String,
        subpath: Option<String>,
    },
}

const BUILD_TARGET: &str = env!("BUILD_TARGET");
const REMOTE_NAME: &str = "origin";

#[cfg(target_arch = "wasm32")]
pub fn get_language(name: &str) -> Result<Option<Grammar>> {
    unimplemented!()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn get_language(name: &str) -> Result<Option<Grammar>> {
    let Some(library_path) = grammar_library_path(name, crate::runtime_assets()?)? else {
        return Ok(None);
    };

    let grammar = unsafe { Grammar::new(name, &library_path) }?;
    Ok(Some(grammar))
}

#[cfg(not(target_arch = "wasm32"))]
fn grammar_library_path(
    name: &str,
    runtime_assets: &crate::RuntimeAssets,
) -> Result<Option<PathBuf>> {
    Ok(runtime_assets
        .resolve_grammar(name)?
        .map(|resolved| resolved.path))
}

fn ensure_git_is_available() -> Result<()> {
    helix_stdx::env::which("git")?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn install_pkg_grammar(
    grammar_id: &str,
    remote: &str,
    revision: &str,
    subpath: Option<&str>,
    install_dir: &Path,
) -> Result<()> {
    ensure_git_is_available()?;
    let sources = install_dir.join("sources").join(grammar_id);
    let repo = VendoredGrammar::at(sources);
    repo.fetch(remote, revision)?;
    let grammar = GrammarConfiguration {
        grammar_id: grammar_id.to_owned(),
        source: GrammarSource::Git {
            remote: remote.to_owned(),
            revision: revision.to_owned(),
            subpath: subpath.map(str::to_owned),
        },
    };
    build_grammar_to(grammar, None, install_dir)?;
    Ok(())
}

pub fn fetch_grammars() -> Result<()> {
    ensure_git_is_available()?;

    // We do not need to fetch local grammars.
    let mut grammars = get_grammar_configs()?;
    grammars.retain(|grammar| !matches!(grammar.source, GrammarSource::Local { .. }));

    println!("Fetching {} grammars", grammars.len());
    let results = run_parallel(grammars, fetch_grammar);

    let mut errors = Vec::new();
    let mut git_updated = Vec::new();
    let mut git_up_to_date = 0;
    let mut non_git = Vec::new();

    for (grammar_id, res) in results {
        match res {
            Ok(FetchStatus::GitUpToDate) => git_up_to_date += 1,
            Ok(FetchStatus::GitUpdated { revision }) => git_updated.push((grammar_id, revision)),
            Ok(FetchStatus::NonGit) => non_git.push(grammar_id),
            Err(e) => errors.push((grammar_id, e)),
        }
    }

    non_git.sort_unstable();
    git_updated.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    if git_up_to_date != 0 {
        println!("{} up to date git grammars", git_up_to_date);
    }

    if !non_git.is_empty() {
        println!("{} non git grammars", non_git.len());
        println!("\t{:?}", non_git);
    }

    if !git_updated.is_empty() {
        println!("{} updated grammars", git_updated.len());
        // We checked the vec is not empty, unwrapping will not panic
        let longest_id = git_updated.iter().map(|x| x.0.len()).max().unwrap();
        for (id, rev) in git_updated {
            println!(
                "\t{id:width$} now on {rev}",
                id = id,
                width = longest_id,
                rev = rev
            );
        }
    }

    if !errors.is_empty() {
        let len = errors.len();
        for (i, (grammar, error)) in errors.into_iter().enumerate() {
            println!("Failure {}/{len}: {grammar} {error}", i + 1);
        }
        bail!("{len} grammars failed to fetch");
    }

    Ok(())
}

pub fn build_grammars(target: Option<String>) -> Result<()> {
    ensure_git_is_available()?;

    let grammars = get_grammar_configs()?;
    println!("Building {} grammars", grammars.len());
    let results = run_parallel(grammars, move |grammar| {
        build_grammar(grammar, target.as_deref())
    });

    let mut errors = Vec::new();
    let mut already_built = 0;
    let mut built = Vec::new();

    for (grammar_id, res) in results {
        match res {
            Ok(BuildStatus::AlreadyBuilt) => already_built += 1,
            Ok(BuildStatus::Built) => built.push(grammar_id),
            Err(e) => errors.push((grammar_id, e)),
        }
    }

    built.sort_unstable();

    if already_built != 0 {
        println!("{} grammars already built", already_built);
    }

    if !built.is_empty() {
        println!("{} grammars built now", built.len());
        println!("\t{:?}", built);
    }

    if !errors.is_empty() {
        let len = errors.len();
        for (i, (grammar_id, error)) in errors.into_iter().enumerate() {
            println!("Failure {}/{len}: {grammar_id} {error}", i + 1);
        }
        bail!("{len} grammars failed to build");
    }

    Ok(())
}

// Returns the set of grammar configurations the user requests.
// Grammars are configured in the default and user `languages.toml` and are
// merged. The `grammar_selection` key of the config is then used to filter
// down all grammars into a subset of the user's choosing.
fn get_grammar_configs() -> Result<Vec<GrammarConfiguration>> {
    let config: Configuration = crate::config::user_lang_config()
        .context("Could not parse languages.toml")?
        .try_into()?;

    let grammars = match config.grammar_selection {
        Some(GrammarSelection::Only { only: selections }) => config
            .grammar
            .into_iter()
            .filter(|grammar| selections.contains(&grammar.grammar_id))
            .collect(),
        Some(GrammarSelection::Except { except: rejections }) => config
            .grammar
            .into_iter()
            .filter(|grammar| !rejections.contains(&grammar.grammar_id))
            .collect(),
        None => config.grammar,
    };

    Ok(grammars)
}

/// Returns grammar identities enabled by the merged `languages.toml` selection.
pub fn configured_grammar_names() -> Result<BTreeSet<String>> {
    Ok(get_grammar_configs()?
        .into_iter()
        .map(|grammar| grammar.grammar_id)
        .collect())
}

pub fn get_grammar_names() -> Result<Option<HashSet<String>>> {
    let config: Configuration = crate::config::user_lang_config()
        .context("Could not parse languages.toml")?
        .try_into()?;

    let grammars = match config.grammar_selection {
        Some(GrammarSelection::Only { only: selections }) => Some(selections),
        Some(GrammarSelection::Except { except: rejections }) => Some(
            config
                .grammar
                .into_iter()
                .map(|grammar| grammar.grammar_id)
                .filter(|id| !rejections.contains(id))
                .collect(),
        ),
        None => None,
    };

    Ok(grammars)
}

fn run_parallel<F, Res>(grammars: Vec<GrammarConfiguration>, job: F) -> Vec<(String, Result<Res>)>
where
    F: Fn(GrammarConfiguration) -> Result<Res> + Send + 'static + Clone,
    Res: Send + 'static,
{
    let pool = threadpool::Builder::new().build();
    let (tx, rx) = channel();

    for grammar in grammars {
        let tx = tx.clone();
        let job = job.clone();

        pool.execute(move || {
            // Ignore any SendErrors, if any job in another thread has encountered an
            // error the Receiver will be closed causing this send to fail.
            let _ = tx.send((grammar.grammar_id.clone(), job(grammar)));
        });
    }

    drop(tx);

    rx.iter().collect()
}

enum FetchStatus {
    GitUpToDate,
    GitUpdated { revision: String },
    NonGit,
}

struct VendoredGrammar {
    dir: PathBuf,
}

impl VendoredGrammar {
    fn new(grammar: &str) -> Self {
        let dir = crate::runtime_write_dir()
            .join("grammars")
            .join("sources")
            .join(grammar);

        Self { dir }
    }

    fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Gets the current revision of the repo.
    fn revision(&self) -> Option<String> {
        git(&self.dir, ["rev-parse", "HEAD"]).ok()
    }

    /// Fetches grammar at the given revision.
    fn fetch(&self, remote: &str, rev: &str) -> Result<()> {
        let staging_dir = self.prepare_update()?;
        let staged = Self::at(staging_dir.clone());
        let result = (|| {
            staged.init(remote)?;
            git(&staging_dir, ["fetch", "--depth", "1", REMOTE_NAME, rev])?;
            git(&staging_dir, ["checkout", rev])?;
            self.promote(&staging_dir)
        })();

        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "Failed to update grammar checkout {:?} from {remote:?} at revision {rev:?}: {error:#}",
                    self.dir
                );
                match remove_path_if_exists(&staging_dir) {
                    Ok(()) => Err(anyhow!(message)),
                    Err(cleanup_error) => Err(anyhow!(
                        "{message}\nAdditionally failed to remove staged checkout {:?}: {cleanup_error}",
                        staging_dir
                    )),
                }
            }
        }
    }

    /// Initializes the grammar directory.
    ///
    /// Creates directory and sets it up as a git repo, with remote set correctly.
    fn init(&self, remote: &str) -> Result<()> {
        // Create the grammar directory if needed.
        fs::create_dir_all(&self.dir).map_err(|error| {
            anyhow!(
                "Could not create grammar directory {:?}: {error}",
                &self.dir
            )
        })?;

        // Ensure directory is git initialized.
        if !self.dir.join(".git").exists() {
            git(&self.dir, ["init"])?;
        }

        // Ensure the remote matches the configured remote, setting if needed.
        if self.remote().as_deref() != Some(remote) {
            self.set_remote(remote)?;
        }

        Ok(())
    }

    fn prepare_update(&self) -> Result<PathBuf> {
        let staging_dir = self.staging_dir()?;
        let backup_dir = self.backup_dir()?;
        let live_exists = path_exists(&self.dir)?;
        let backup_exists = path_exists(&backup_dir)?;

        if !live_exists && backup_exists {
            fs::rename(&backup_dir, &self.dir).map_err(|error| {
                anyhow!(
                    "Could not restore grammar checkout {:?} from backup {:?} after an interrupted update: {error}",
                    self.dir,
                    backup_dir
                )
            })?;
        } else if live_exists && backup_exists {
            remove_path_if_exists(&backup_dir).map_err(|error| {
                anyhow!(
                    "Could not remove stale grammar backup {:?}: {error}",
                    backup_dir
                )
            })?;
        }

        remove_path_if_exists(&staging_dir).map_err(|error| {
            anyhow!(
                "Could not remove stale staged grammar checkout {:?}: {error}",
                staging_dir
            )
        })?;
        Ok(staging_dir)
    }

    fn promote(&self, staging_dir: &Path) -> Result<()> {
        let backup_dir = self.backup_dir()?;

        if !path_exists(&self.dir)? {
            return fs::rename(staging_dir, &self.dir).map_err(|error| {
                anyhow!(
                    "Could not promote staged grammar checkout {:?} to {:?}: {error}",
                    staging_dir,
                    self.dir
                )
            });
        }

        if path_exists(&backup_dir)? {
            bail!(
                "Could not promote staged grammar checkout {:?}: backup path {:?} already exists",
                staging_dir,
                backup_dir
            );
        }

        fs::rename(&self.dir, &backup_dir).map_err(|error| {
            anyhow!(
                "Could not move current grammar checkout {:?} to backup {:?}: {error}",
                self.dir,
                backup_dir
            )
        })?;

        if let Err(promote_error) = fs::rename(staging_dir, &self.dir) {
            return match fs::rename(&backup_dir, &self.dir) {
                Ok(()) => Err(anyhow!(
                    "Could not promote staged grammar checkout {:?} to {:?}: {promote_error}; the previous checkout was restored",
                    staging_dir,
                    self.dir
                )),
                Err(rollback_error) => Err(anyhow!(
                    "Could not promote staged grammar checkout {:?} to {:?}: {promote_error}; also failed to restore backup {:?}: {rollback_error}",
                    staging_dir,
                    self.dir,
                    backup_dir
                )),
            };
        }

        if let Err(error) = remove_path_if_exists(&backup_dir) {
            log::warn!(
                "Grammar checkout {:?} was updated, but stale backup {:?} could not be removed: {error}",
                self.dir,
                backup_dir
            );
        }
        Ok(())
    }

    fn staging_dir(&self) -> Result<PathBuf> {
        self.sibling_dir(".staging")
    }

    fn backup_dir(&self) -> Result<PathBuf> {
        self.sibling_dir(".backup")
    }

    fn sibling_dir(&self, suffix: &str) -> Result<PathBuf> {
        let name = self.dir.file_name().ok_or_else(|| {
            anyhow!(
                "Grammar checkout path {:?} has no final component",
                self.dir
            )
        })?;
        let mut sibling_name = name.to_os_string();
        sibling_name.push(suffix);
        Ok(self.dir.with_file_name(sibling_name))
    }

    /// Gets remote URL of grammar repo.
    fn remote(&self) -> Option<String> {
        git(&self.dir, ["remote", "get-url", REMOTE_NAME]).ok()
    }

    /// Sets remote URL of grammar repo.
    fn set_remote(&self, remote: &str) -> Result<()> {
        git(&self.dir, ["remote", "set-url", REMOTE_NAME, remote])
            .or_else(|_| git(&self.dir, ["remote", "add", REMOTE_NAME, remote]))?;
        Ok(())
    }
}

fn fetch_grammar(grammar: GrammarConfiguration) -> Result<FetchStatus> {
    let GrammarSource::Git {
        remote, revision, ..
    } = grammar.source
    else {
        return Ok(FetchStatus::NonGit);
    };

    let repo = VendoredGrammar::new(&grammar.grammar_id);

    if repo.revision().is_some_and(|rev| rev == revision)
        && repo.remote().as_deref() == Some(remote.as_str())
    {
        return Ok(FetchStatus::GitUpToDate);
    }

    // Fetch the grammar if the revision doesn't match.
    repo.fetch(&remote, &revision)?;

    Ok(FetchStatus::GitUpdated { revision })
}

// A wrapper around 'git' commands which returns stdout in success and a
// helpful error message showing the command, stdout, and stderr in error.
fn git<I, S>(repository_dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let command_args = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new("git")
        .args(&args)
        .current_dir(repository_dir)
        .output()
        .map_err(|error| {
            anyhow!(
                "Failed to execute `git {command_args}` in {:?}: {error}",
                repository_dir
            )
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    } else {
        Err(anyhow!(
            "`git {command_args}` failed in {:?} with status {}.\nStdout: {}\nStderr: {}",
            repository_dir,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow!(
            "Could not inspect grammar path {:?}: {error}",
            path
        )),
    }
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

enum BuildStatus {
    AlreadyBuilt,
    Built,
}

fn build_grammar(grammar: GrammarConfiguration, target: Option<&str>) -> Result<BuildStatus> {
    let parser_lib_path = crate::runtime_write_dir().join("grammars");
    build_grammar_to(grammar, target, &parser_lib_path)
}

fn build_grammar_to(
    grammar: GrammarConfiguration,
    target: Option<&str>,
    parser_lib_path: &Path,
) -> Result<BuildStatus> {
    let grammar_dir = if let GrammarSource::Local { path } = &grammar.source {
        PathBuf::from(&path)
    } else {
        parser_lib_path.join("sources").join(&grammar.grammar_id)
    };

    let grammar_dir_entries = grammar_dir.read_dir().with_context(|| {
        format!(
            "Failed to read directory {:?}. Did you use 'hx --grammar fetch'?",
            grammar_dir
        )
    })?;

    if grammar_dir_entries.count() == 0 {
        return Err(anyhow!(
            "Directory {:?} is empty. Did you use 'hx --grammar fetch'?",
            grammar_dir
        ));
    };

    let path = match &grammar.source {
        GrammarSource::Git {
            subpath: Some(subpath),
            ..
        } => grammar_dir.join(subpath),
        _ => grammar_dir,
    }
    .join("src");

    build_tree_sitter_library(&path, grammar, target, parser_lib_path)
}

fn build_tree_sitter_library(
    src_path: &Path,
    grammar: GrammarConfiguration,
    target: Option<&str>,
    parser_lib_path: &Path,
) -> Result<BuildStatus> {
    let header_path = src_path;
    let parser_path = src_path.join("parser.c");
    let mut scanner_path = src_path.join("scanner.c");

    let scanner_path = if scanner_path.exists() {
        Some(scanner_path)
    } else {
        scanner_path.set_extension("cc");
        if scanner_path.exists() {
            Some(scanner_path)
        } else {
            None
        }
    };
    fs::create_dir_all(parser_lib_path).context("Failed to create grammar output directory")?;
    let mut library_path = parser_lib_path.join(&grammar.grammar_id);
    library_path.set_extension(DYLIB_EXTENSION);

    // if we are running inside a buildscript emit cargo metadata
    // to detect if we are running from a buildscript check some env variables
    // that cargo only sets for build scripts
    if std::env::var("OUT_DIR").is_ok() && std::env::var("CARGO").is_ok() {
        if let Some(scanner_path) = scanner_path.as_ref().and_then(|path| path.to_str()) {
            println!("cargo:rerun-if-changed={scanner_path}");
        }
        if let Some(parser_path) = parser_path.to_str() {
            println!("cargo:rerun-if-changed={parser_path}");
        }
    }

    let recompile = needs_recompile(&library_path, &parser_path, scanner_path.as_ref())
        .context("Failed to compare source and binary timestamps")?;

    if !recompile {
        return Ok(BuildStatus::AlreadyBuilt);
    }

    let mut config = cc::Build::new();
    config
        .cpp(true)
        .opt_level(3)
        .cargo_metadata(false)
        .host(BUILD_TARGET)
        .target(target.unwrap_or(BUILD_TARGET));
    let compiler = config.get_compiler();
    let mut command = Command::new(compiler.path());
    command.current_dir(src_path);
    for (key, value) in compiler.env() {
        command.env(key, value);
    }

    command.args(compiler.args());
    // used to delay dropping the temporary object file until after the compilation is complete
    let _path_guard;

    if compiler.is_like_msvc() {
        command
            .args(["/nologo", "/LD", "/I"])
            .arg(header_path)
            .arg("/utf-8")
            .arg("/std:c11");
        if let Some(scanner_path) = scanner_path.as_ref() {
            if scanner_path.extension() == Some("c".as_ref()) {
                command.arg(scanner_path);
            } else {
                let mut cpp_command = Command::new(compiler.path());
                cpp_command.current_dir(src_path);
                for (key, value) in compiler.env() {
                    cpp_command.env(key, value);
                }
                cpp_command.args(compiler.args());
                let object_file =
                    library_path.with_file_name(format!("{}_scanner.obj", &grammar.grammar_id));
                cpp_command
                    .args(["/nologo", "/LD", "/I"])
                    .arg(header_path)
                    .arg("/utf-8")
                    .arg("/std:c++14")
                    .arg(format!("/Fo{}", object_file.display()))
                    .arg("/c")
                    .arg(scanner_path);
                let output = cpp_command
                    .output()
                    .context("Failed to execute C++ compiler")?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "Parser compilation failed.\nStdout: {}\nStderr: {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                command.arg(&object_file);
                _path_guard = TempPath::try_from_path(object_file)
                    .context("failed to guard temporary scanner object file")?;
            }
        }

        command
            .arg(parser_path)
            .arg("/link")
            .arg(format!("/out:{}", library_path.to_str().unwrap()));
    } else {
        #[cfg(not(windows))]
        command.arg("-fPIC");

        command
            .arg("-shared")
            .arg("-fno-exceptions")
            .arg("-I")
            .arg(header_path)
            .arg("-o")
            .arg(&library_path);

        if let Some(scanner_path) = scanner_path.as_ref() {
            if scanner_path.extension() == Some("c".as_ref()) {
                command.arg("-xc").arg("-std=c11").arg(scanner_path);
            } else {
                let mut cpp_command = Command::new(compiler.path());
                cpp_command.current_dir(src_path);
                for (key, value) in compiler.env() {
                    cpp_command.env(key, value);
                }
                cpp_command.args(compiler.args());
                let object_file =
                    library_path.with_file_name(format!("{}_scanner.o", &grammar.grammar_id));

                #[cfg(not(windows))]
                cpp_command.arg("-fPIC");

                cpp_command
                    .arg("-fno-exceptions")
                    .arg("-I")
                    .arg(header_path)
                    .arg("-o")
                    .arg(&object_file)
                    .arg("-std=c++14")
                    .arg("-c")
                    .arg(scanner_path);
                let output = cpp_command
                    .output()
                    .context("Failed to execute C++ compiler")?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "Parser compilation failed.\nStdout: {}\nStderr: {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }

                command.arg(&object_file);
                _path_guard = TempPath::try_from_path(object_file)
                    .context("failed to guard temporary scanner object file")?;
            }
        }
        command.arg("-xc").arg("-std=c11").arg(parser_path);
        if cfg!(all(
            unix,
            not(any(target_os = "macos", target_os = "illumos"))
        )) {
            command.arg("-Wl,-z,relro,-z,now");
        }
    }

    let output = command
        .output()
        .context("Failed to execute C/C++ compiler")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Parser compilation failed.\nStdout: {}\nStderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(BuildStatus::Built)
}

fn needs_recompile(
    lib_path: &Path,
    parser_c_path: &Path,
    scanner_path: Option<&PathBuf>,
) -> Result<bool> {
    if !lib_path.exists() {
        return Ok(true);
    }
    let lib_mtime = mtime(lib_path)?;
    if mtime(parser_c_path)? > lib_mtime {
        return Ok(true);
    }
    if let Some(scanner_path) = scanner_path {
        if mtime(scanner_path)? > lib_mtime {
            return Ok(true);
        }
    }
    Ok(false)
}

fn mtime(path: &Path) -> Result<SystemTime> {
    Ok(fs::metadata(path)?.modified()?)
}

/// Gives the contents of a file from a language's `runtime/queries/<lang>`
/// directory
pub fn load_runtime_file(language: &str, filename: &str) -> Result<String, std::io::Error> {
    let logical_path = PathBuf::new().join("queries").join(language).join(filename);
    let path = crate::runtime_assets()
        .and_then(|assets| assets.require_file(&logical_path))
        .map_err(std::io::Error::other)?;
    std::fs::read_to_string(path.path)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{git, grammar_library_path, install_pkg_grammar, VendoredGrammar, DYLIB_EXTENSION};
    use crate::{RuntimeAssets, RuntimeAssetsSnapshot};
    use helix_store::{ActivePackage, RuntimeAsset, RuntimeAssetSpec, RuntimeSnapshot};

    fn create_grammar_remote(dir: &TempDir) -> (PathBuf, String) {
        let remote = dir.path().join("remote");
        fs::create_dir_all(remote.join("src")).unwrap();
        git(&remote, ["init"]).unwrap();
        git(
            &remote,
            ["config", "user.email", "grammar-test@example.com"],
        )
        .unwrap();
        git(&remote, ["config", "user.name", "Grammar Test"]).unwrap();
        let revision = commit_grammar_change(&remote, "initial grammar");
        (remote, revision)
    }

    fn commit_grammar_change(remote: &Path, message: &str) -> String {
        fs::write(
            remote.join("src").join("parser.c"),
            format!(
                r#"
#ifdef _WIN32
__declspec(dllexport)
#endif
void *tree_sitter_test(void) {{ return 0; }}
/* {message} */
"#
            ),
        )
        .unwrap();
        git(remote, ["add", "."]).unwrap();
        git(remote, ["commit", "-m", message]).unwrap();
        git(remote, ["rev-parse", "HEAD"]).unwrap()
    }

    #[test]
    fn pkg_grammar_fresh_install_fetches_and_builds() {
        let dir = TempDir::new().unwrap();
        let (remote, revision) = create_grammar_remote(&dir);
        let install_dir = dir.path().join("install");
        fs::create_dir(&install_dir).unwrap();

        install_pkg_grammar(
            "test",
            remote.to_str().unwrap(),
            &revision,
            None,
            &install_dir,
        )
        .unwrap();

        let source = install_dir.join("sources").join("test");
        assert_eq!(
            VendoredGrammar::at(source).revision().as_deref(),
            Some(revision.as_str())
        );
        let mut library = install_dir.join("test");
        library.set_extension(DYLIB_EXTENSION);
        assert!(library.is_file());
    }

    #[test]
    fn failed_fetch_retains_last_good_grammar() {
        let dir = TempDir::new().unwrap();
        let (remote, revision) = create_grammar_remote(&dir);
        let source = dir.path().join("sources").join("test");
        let repo = VendoredGrammar::at(source);
        repo.init(remote.to_str().unwrap()).unwrap();
        repo.fetch(remote.to_str().unwrap(), &revision).unwrap();

        let missing_revision = "missing-grammar-revision";
        let error = repo
            .fetch(remote.to_str().unwrap(), missing_revision)
            .unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains(missing_revision));
        assert!(error.contains("git fetch --depth 1 origin"));
        assert_eq!(repo.revision().as_deref(), Some(revision.as_str()));
        assert!(!repo.staging_dir().unwrap().exists());
    }

    #[test]
    fn stale_staging_is_replaced_on_next_fetch() {
        let dir = TempDir::new().unwrap();
        let (remote, revision) = create_grammar_remote(&dir);
        let source = dir.path().join("sources").join("test");
        let repo = VendoredGrammar::at(source);
        repo.fetch(remote.to_str().unwrap(), &revision).unwrap();

        let next_revision = commit_grammar_change(&remote, "updated grammar");
        let staging = repo.staging_dir().unwrap();
        fs::create_dir_all(staging.join("abandoned")).unwrap();
        fs::write(staging.join("abandoned").join("partial"), b"stale").unwrap();

        repo.fetch(remote.to_str().unwrap(), &next_revision)
            .unwrap();

        assert_eq!(repo.revision().as_deref(), Some(next_revision.as_str()));
        assert!(!staging.exists());
        assert!(!repo.backup_dir().unwrap().exists());
    }

    #[test]
    fn interrupted_promotion_restores_backup_before_fetching() {
        let dir = TempDir::new().unwrap();
        let (remote, revision) = create_grammar_remote(&dir);
        let source = dir.path().join("sources").join("test");
        let repo = VendoredGrammar::at(source);
        repo.fetch(remote.to_str().unwrap(), &revision).unwrap();

        let staging = repo.staging_dir().unwrap();
        let backup = repo.backup_dir().unwrap();
        fs::rename(&repo.dir, &backup).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("partial"), b"stale").unwrap();

        repo.fetch(remote.to_str().unwrap(), "missing-after-interruption")
            .unwrap_err();

        assert_eq!(repo.revision().as_deref(), Some(revision.as_str()));
        assert!(!staging.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn promotion_failure_restores_previous_checkout() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("sources").join("test");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("last-good"), b"grammar").unwrap();
        let repo = VendoredGrammar::at(source);
        let missing_staging = repo.staging_dir().unwrap();

        let error = repo.promote(&missing_staging).unwrap_err();

        assert!(format!("{error:#}").contains("previous checkout was restored"));
        assert_eq!(fs::read(repo.dir.join("last-good")).unwrap(), b"grammar");
        assert!(!repo.backup_dir().unwrap().exists());
    }

    #[test]
    fn managed_grammar_discovery_uses_runtime_assets_snapshot() {
        let dir = TempDir::new().unwrap();
        let mut library = dir.path().join("managed-rust");
        library.set_extension(DYLIB_EXTENSION);
        fs::write(&library, b"").unwrap();
        let package = ActivePackage::new("grammar", "rust", "rev1");
        let assets = RuntimeAssets::from_snapshot(RuntimeAssetsSnapshot::new(
            RuntimeSnapshot {
                generation: 1,
                assets: vec![RuntimeAsset::from_spec(
                    package,
                    RuntimeAssetSpec::grammar("rust", &library),
                )],
            },
            Vec::new(),
            Vec::new(),
        ));

        assert_eq!(
            grammar_library_path("rust", &assets).unwrap().as_deref(),
            Some(library.as_path())
        );
    }
}
