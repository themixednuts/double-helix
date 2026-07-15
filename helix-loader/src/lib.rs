pub mod assets;
pub mod config;
pub mod grammar;

use helix_stdx::{env::current_working_dir, path};

use std::path::{Path, PathBuf};

pub use assets::{
    bootstrap_runtime_assets, runtime_assets, runtime_assets_if_initialized, ActivePackage, Origin,
    ResolvedLaunch, ResolvedPath, RuntimeAsset, RuntimeAssetKey, RuntimeAssetKind,
    RuntimeAssetSpec, RuntimeAssets, RuntimeAssetsChange, RuntimeAssetsError,
    RuntimeAssetsSnapshot, RuntimeSnapshot,
};
pub use helix_stdx::paths::{
    cache_dir, config_dir, data_dir, legacy_config_dir, LEGACY_CONFIG_DIR, PRODUCT_CONFIG_DIR,
};

pub const VERSION_AND_GIT_HASH: &str = env!("VERSION_AND_GIT_HASH");
pub const WORKSPACE_CONFIG_DIR: &str = ".double-helix";
pub const WORKSPACE_IGNORE_FILE: &str = ".double-helix/ignore";

static RUNTIME_DIRS: once_cell::sync::Lazy<RuntimeDirectories> =
    once_cell::sync::Lazy::new(prioritize_runtime_dirs);

static CONFIG_FILE: once_cell::sync::OnceCell<PathBuf> = once_cell::sync::OnceCell::new();

static LOG_FILE: once_cell::sync::OnceCell<PathBuf> = once_cell::sync::OnceCell::new();

pub fn initialize_config_file(specified_file: Option<PathBuf>) {
    let config_file = specified_file.unwrap_or_else(default_config_file);
    ensure_parent_dir(&config_file);
    CONFIG_FILE.set(config_file).ok();
}

pub fn initialize_log_file(specified_file: Option<PathBuf>) {
    let log_file = specified_file.unwrap_or_else(default_log_file);
    ensure_parent_dir(&log_file);
    LOG_FILE.set(log_file).ok();
}

/// A list of runtime directories from highest to lowest priority
///
/// The priority is:
///
/// 1. sibling directory to `CARGO_MANIFEST_DIR` (if environment variable is set)
/// 2. subdirectory of user config directory (always included)
/// 3. `DOUBLE_HELIX_RUNTIME` (if environment variable is set)
/// 4. `DOUBLE_HELIX_DEFAULT_RUNTIME` (if environment variable is set *at build time*)
/// 5. subdirectory of path to helix executable (always included)
///
/// Postcondition: returns at least two paths (they might not exist).
struct RuntimeDirectories {
    overrides: Vec<PathBuf>,
    bundled: Vec<PathBuf>,
}

fn prioritize_runtime_dirs() -> RuntimeDirectories {
    const RT_DIR: &str = "runtime";
    // Adding higher priority first
    let mut overrides = Vec::new();
    if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
        // this is the directory of the crate being run by cargo, we need the workspace path so we take the parent
        let path = PathBuf::from(dir).parent().unwrap().join(RT_DIR);
        log::debug!("runtime dir: {}", path.to_string_lossy());
        overrides.push(path);
    }

    let conf_rt_dir = config_dir().join(RT_DIR);
    overrides.push(conf_rt_dir);

    if let Ok(dir) = std::env::var("DOUBLE_HELIX_RUNTIME") {
        let dir = path::expand_tilde(Path::new(&dir));
        overrides.push(path::normalize(dir));
    }

    let mut bundled = Vec::new();

    // If this variable is set during build time, it will always be included
    // in the lookup list. This allows downstream packagers to set a fallback
    // directory to a location that is conventional on their distro so that they
    // need not resort to a wrapper script or a global environment variable.
    if let Some(dir) = std::option_env!("DOUBLE_HELIX_DEFAULT_RUNTIME") {
        bundled.push(dir.into());
    }

    // fallback to location of the executable being run
    // canonicalize the path in case the executable is symlinked
    let exe_rt_dir = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|path| path.parent().map(|path| path.to_path_buf().join(RT_DIR)))
        .unwrap();
    bundled.push(exe_rt_dir);

    RuntimeDirectories { overrides, bundled }
}

/// Runtime directory used by development commands that create grammar sources and libraries.
///
/// Runtime reads must go through [`RuntimeAssets`]; this path is only a write destination.
pub(crate) fn runtime_write_dir() -> &'static Path {
    RUNTIME_DIRS
        .overrides
        .first()
        .expect("runtime overrides always include the user config runtime")
}

/// Runtime override directories ordered from highest to lowest priority.
pub(crate) fn runtime_override_dirs() -> &'static [PathBuf] {
    &RUNTIME_DIRS.overrides
}

/// Bundled runtime directories ordered from highest to lowest priority.
pub(crate) fn bundled_runtime_dirs() -> &'static [PathBuf] {
    &RUNTIME_DIRS.bundled
}

pub fn config_file() -> PathBuf {
    CONFIG_FILE.get().map(|path| path.to_path_buf()).unwrap()
}

pub fn log_file() -> PathBuf {
    LOG_FILE.get().map(|path| path.to_path_buf()).unwrap()
}

pub fn workspace_config_file() -> PathBuf {
    find_workspace()
        .0
        .join(WORKSPACE_CONFIG_DIR)
        .join("config.toml")
}

pub fn workspace_ignore_file_name() -> &'static str {
    WORKSPACE_IGNORE_FILE
}

pub fn lang_config_file() -> PathBuf {
    config_dir().join("languages.toml")
}

pub fn default_log_file() -> PathBuf {
    cache_dir().join("double-helix.log")
}

/// Merge two TOML documents, merging values from `right` onto `left`
///
/// `merge_depth` sets the nesting depth up to which values are merged instead
/// of overridden.
///
/// When a table exists in both `left` and `right`, the merged table consists of
/// all keys in `left`'s table unioned with all keys in `right` with the values
/// of `right` being merged recursively onto values of `left`.
///
/// `crate::merge_toml_values(a, b, 3)` combines, for example:
///
/// b:
/// ```toml
/// [[language]]
/// name = "toml"
/// language-server = { command = "taplo", args = ["lsp", "stdio"] }
/// ```
/// a:
/// ```toml
/// [[language]]
/// language-server = { command = "/usr/bin/taplo" }
/// ```
///
/// into:
/// ```toml
/// [[language]]
/// name = "toml"
/// language-server = { command = "/usr/bin/taplo" }
/// ```
///
/// thus it overrides the third depth-level of b with values of a if they exist,
/// but otherwise merges their values
pub fn merge_toml_values(left: toml::Value, right: toml::Value, merge_depth: usize) -> toml::Value {
    use toml::Value;

    fn get_name(v: &Value) -> Option<&str> {
        v.get("name").and_then(Value::as_str)
    }

    match (left, right) {
        (Value::Array(mut left_items), Value::Array(right_items)) => {
            if merge_depth > 0 {
                left_items.reserve(right_items.len());
                for rvalue in right_items {
                    let lvalue = get_name(&rvalue)
                        .and_then(|rname| {
                            left_items.iter().position(|v| get_name(v) == Some(rname))
                        })
                        .map(|lpos| left_items.remove(lpos));
                    let mvalue = match lvalue {
                        Some(lvalue) => merge_toml_values(lvalue, rvalue, merge_depth - 1),
                        None => rvalue,
                    };
                    left_items.push(mvalue);
                }
                Value::Array(left_items)
            } else {
                Value::Array(right_items)
            }
        }
        (Value::Table(mut left_map), Value::Table(right_map)) => {
            if merge_depth > 0 {
                for (rname, rvalue) in right_map {
                    match left_map.remove(&rname) {
                        Some(lvalue) => {
                            let merged_value = merge_toml_values(lvalue, rvalue, merge_depth - 1);
                            left_map.insert(rname, merged_value);
                        }
                        None => {
                            left_map.insert(rname, rvalue);
                        }
                    }
                }
                Value::Table(left_map)
            } else {
                Value::Table(right_map)
            }
        }
        // Catch everything else we didn't handle, and use the right value
        (_, value) => value,
    }
}

/// Finds the current workspace folder.
/// Used as a ceiling dir for LSP root resolution, the filepicker and potentially as a future filewatching root
///
/// This function starts searching the FS upward from the CWD
/// and returns the first directory that contains either `.git`, `.svn`, `.jj` or `.double-helix`.
/// If no workspace was found returns (CWD, true).
/// Otherwise (workspace, false) is returned
pub fn find_workspace() -> (PathBuf, bool) {
    let current_dir = current_working_dir();
    find_workspace_in(current_dir)
}

pub fn find_workspace_in(dir: impl AsRef<Path>) -> (PathBuf, bool) {
    let dir = dir.as_ref();
    for ancestor in dir.ancestors() {
        if ancestor.join(".git").exists()
            || ancestor.join(".svn").exists()
            || ancestor.join(".jj").exists()
            || ancestor.join(WORKSPACE_CONFIG_DIR).exists()
        {
            return (ancestor.to_owned(), false);
        }
    }

    (dir.to_owned(), true)
}

fn default_config_file() -> PathBuf {
    config_dir().join("config.toml")
}

fn ensure_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).ok();
        }
    }
}

#[cfg(test)]
mod config_path_tests {
    use super::{workspace_ignore_file_name, WORKSPACE_IGNORE_FILE};

    #[test]
    fn workspace_ignore_file_name_uses_slash_separator() {
        assert_eq!(workspace_ignore_file_name(), WORKSPACE_IGNORE_FILE);
        assert_eq!(workspace_ignore_file_name(), ".double-helix/ignore");
    }
}

#[cfg(test)]
mod merge_toml_tests {
    use std::str;

    use super::merge_toml_values;
    use toml::Value;

    #[test]
    fn language_toml_map_merges() {
        const USER: &str = r#"
        [[language]]
        name = "nix"
        test = "bbb"
        indent = { tab-width = 4, unit = "    ", test = "aaa" }
        "#;

        let base = include_bytes!("../../languages.toml");
        let base = str::from_utf8(base).expect("Couldn't parse built-in languages config");
        let base: Value = toml::from_str(base).expect("Couldn't parse built-in languages config");
        let user: Value = toml::from_str(USER).unwrap();

        let merged = merge_toml_values(base, user, 3);
        let languages = merged.get("language").unwrap().as_array().unwrap();
        let nix = languages
            .iter()
            .find(|v| v.get("name").unwrap().as_str().unwrap() == "nix")
            .unwrap();
        let nix_indent = nix.get("indent").unwrap();

        // We changed tab-width and unit in indent so check them if they are the new values
        assert_eq!(
            nix_indent.get("tab-width").unwrap().as_integer().unwrap(),
            4
        );
        assert_eq!(nix_indent.get("unit").unwrap().as_str().unwrap(), "    ");
        // We added a new keys, so check them
        assert_eq!(nix.get("test").unwrap().as_str().unwrap(), "bbb");
        assert_eq!(nix_indent.get("test").unwrap().as_str().unwrap(), "aaa");
        // We didn't change comment-token so it should be same
        assert_eq!(nix.get("comment-token").unwrap().as_str().unwrap(), "#");
    }

    #[test]
    fn language_toml_nested_array_merges() {
        const USER: &str = r#"
        [[language]]
        name = "typescript"
        language-server = { command = "deno", args = ["lsp"] }
        "#;

        let base = include_bytes!("../../languages.toml");
        let base = str::from_utf8(base).expect("Couldn't parse built-in languages config");
        let base: Value = toml::from_str(base).expect("Couldn't parse built-in languages config");
        let user: Value = toml::from_str(USER).unwrap();

        let merged = merge_toml_values(base, user, 3);
        let languages = merged.get("language").unwrap().as_array().unwrap();
        let ts = languages
            .iter()
            .find(|v| v.get("name").unwrap().as_str().unwrap() == "typescript")
            .unwrap();
        assert_eq!(
            ts.get("language-server")
                .unwrap()
                .get("args")
                .unwrap()
                .as_array()
                .unwrap(),
            &vec![Value::String("lsp".into())]
        )
    }
}
