use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantThread {
    pub id: String,
    pub scope: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub rating: Option<String>,
    pub has_feedback: bool,
    pub record_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantLayout {
    pub scope: String,
    pub open_ids: Vec<String>,
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantPermission {
    pub agent: String,
    pub tool: String,
    pub choice: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrecencyEntry {
    pub workspace: String,
    pub path_hash: String,
    pub first_accessed_at: i64,
    pub last_accessed_at: i64,
    pub access_count: i64,
    pub timestamps_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHistory {
    pub id: String,
    pub workspace: String,
    pub query: String,
    pub opened_path: String,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgReceipt {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub hash: String,
    pub bin: String,
    pub shim: String,
    pub files_json: String,
    pub installed_at: String,
    pub native_manager: Option<String>,
    pub native_id: Option<String>,
    pub receipt_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActivePackage {
    pub kind: String,
    pub name: String,
    pub version: String,
}

impl ActivePackage {
    #[must_use]
    pub fn new(
        kind: impl Into<String>,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            version: version.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeAssetKind {
    Command,
    File,
    Grammar,
    PluginRoot,
}

impl RuntimeAssetKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::File => "file",
            Self::Grammar => "grammar",
            Self::PluginRoot => "plugin-root",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "command" => Some(Self::Command),
            "file" => Some(Self::File),
            "grammar" => Some(Self::Grammar),
            "plugin-root" => Some(Self::PluginRoot),
            _ => None,
        }
    }
}

impl std::fmt::Display for RuntimeAssetKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAssetSpec {
    pub kind: RuntimeAssetKind,
    pub key: String,
    pub path: PathBuf,
    pub prefix_args: Vec<String>,
    pub default_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl RuntimeAssetSpec {
    #[must_use]
    pub fn new(kind: RuntimeAssetKind, key: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            key: key.into(),
            path: path.into(),
            prefix_args: Vec::new(),
            default_args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn command(key: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::new(RuntimeAssetKind::Command, key, path)
    }

    #[must_use]
    pub fn file(key: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::new(RuntimeAssetKind::File, key, path)
    }

    #[must_use]
    pub fn grammar(key: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::new(RuntimeAssetKind::Grammar, key, path)
    }

    #[must_use]
    pub fn plugin_root(key: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self::new(RuntimeAssetKind::PluginRoot, key, path)
    }

    #[must_use]
    pub fn with_prefix_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.prefix_args = args.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_default_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.default_args = args.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env = env.into_iter().collect();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAsset {
    pub package: ActivePackage,
    pub kind: RuntimeAssetKind,
    pub key: String,
    pub path: PathBuf,
    pub prefix_args: Vec<String>,
    pub default_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl RuntimeAsset {
    #[must_use]
    pub fn from_spec(package: ActivePackage, spec: RuntimeAssetSpec) -> Self {
        Self {
            package,
            kind: spec.kind,
            key: spec.key,
            path: spec.path,
            prefix_args: spec.prefix_args,
            default_args: spec.default_args,
            env: spec.env,
        }
    }

    #[must_use]
    pub fn as_spec(&self) -> RuntimeAssetSpec {
        RuntimeAssetSpec {
            kind: self.kind,
            key: self.key.clone(),
            path: self.path.clone(),
            prefix_args: self.prefix_args.clone(),
            default_args: self.default_args.clone(),
            env: self.env.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageActivation {
    pub package: ActivePackage,
    pub assets: Vec<RuntimeAssetSpec>,
}

impl PackageActivation {
    #[must_use]
    pub fn new(package: ActivePackage, assets: Vec<RuntimeAssetSpec>) -> Self {
        Self { package, assets }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub generation: u64,
    pub assets: Vec<RuntimeAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageState {
    pub package_kind: String,
    pub package_name: String,
    pub receipt: Option<PkgReceipt>,
    pub assets: Vec<RuntimeAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStateCommit {
    pub before: PackageState,
    pub after: PackageState,
    pub snapshot: RuntimeSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationHistory {
    pub id: i64,
    pub package: ActivePackage,
    pub operation: String,
    pub previous_assets: Vec<RuntimeAsset>,
    pub activated_assets: Vec<RuntimeAsset>,
    pub generation: u64,
    pub rolled_back_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHead {
    pub registry: String,
    pub source: String,
    pub revision: String,
    pub updated_at: i64,
}
