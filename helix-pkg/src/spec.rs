use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PkgKind {
    Lsp,
    Dap,
    Formatter,
    Linter,
    Grammar,
    Plugin,
    Acp,
}

impl PkgKind {
    pub const ALL: [Self; 7] = [
        Self::Lsp,
        Self::Dap,
        Self::Formatter,
        Self::Linter,
        Self::Grammar,
        Self::Plugin,
        Self::Acp,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lsp => "lsp",
            Self::Dap => "dap",
            Self::Formatter => "formatter",
            Self::Linter => "linter",
            Self::Grammar => "grammar",
            Self::Plugin => "plugin",
            Self::Acp => "acp",
        }
    }

    pub fn default_category(self) -> &'static str {
        match self {
            Self::Lsp => "language-server",
            Self::Dap => "debug-adapter",
            Self::Formatter => "formatter",
            Self::Linter => "linter",
            Self::Grammar => "tree-sitter-grammar",
            Self::Plugin => "plugin",
            Self::Acp => "agent-client-protocol",
        }
    }
}

impl fmt::Display for PkgKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PkgKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "lsp" => Ok(Self::Lsp),
            "dap" => Ok(Self::Dap),
            "formatter" => Ok(Self::Formatter),
            "linter" => Ok(Self::Linter),
            "grammar" => Ok(Self::Grammar),
            "plugin" => Ok(Self::Plugin),
            "acp" => Ok(Self::Acp),
            other => Err(Error::Message(format!("unknown package kind: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageSpec {
    pub name: String,
    pub kind: PkgKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub schemas: BTreeMap<String, String>,
    #[serde(default)]
    pub version: VersionSpec,
    #[serde(default, rename = "artifact")]
    pub artifacts: Vec<Artifact>,
}

impl PackageSpec {
    pub fn artifact_for(&self, os: &str, arch: &str) -> Result<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.matches(os, arch))
            .ok_or_else(|| Error::NoArtifact {
                name: self.name.clone(),
                os: os.to_owned(),
                arch: arch.to_owned(),
            })
    }

    pub fn artifacts_for<'a>(
        &'a self,
        os: &str,
        arch: &str,
    ) -> impl Iterator<Item = &'a Artifact> + 'a {
        let os = os.to_owned();
        let arch = arch.to_owned();
        self.artifacts
            .iter()
            .filter(move |artifact| artifact.matches(&os, &arch))
    }

    pub fn artifact(&self) -> Result<&Artifact> {
        self.artifact_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn is_system_only(&self) -> bool {
        self.artifacts
            .iter()
            .all(|artifact| artifact.source.system.is_some())
    }

    pub fn matches_search(&self, term: &str) -> bool {
        let needle = term.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return true;
        }
        self.search_terms()
            .any(|term| term.to_ascii_lowercase().contains(&needle))
    }

    pub fn search_terms(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str())
            .chain(std::iter::once(self.kind.as_str()))
            .chain(std::iter::once(self.kind.default_category()))
            .chain(std::iter::once(self.description.as_str()))
            .chain(self.homepage.iter().map(String::as_str))
            .chain(self.aliases.iter().map(String::as_str))
            .chain(self.categories.iter().map(String::as_str))
            .chain(self.languages.iter().map(String::as_str))
            .chain(
                self.schemas
                    .iter()
                    .flat_map(|(name, url)| [name.as_str(), url.as_str()]),
            )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VersionSpec {
    #[serde(default, rename = "tag-source")]
    pub tag_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Artifact {
    pub os: String,
    pub arch: String,
    pub source: Source,
    #[serde(default)]
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Artifact {
    pub fn matches(&self, os: &str, arch: &str) -> bool {
        self.os == os && self.arch == arch
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Source {
    #[serde(default, rename = "github-release")]
    pub github_release: Option<String>,
    #[serde(default)]
    pub archive: Option<String>,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub npx: Option<String>,
    #[serde(default)]
    pub uvx: Option<String>,
    #[serde(default)]
    pub pip: Option<String>,
    #[serde(default)]
    pub cargo: Option<String>,
    #[serde(default)]
    pub go: Option<String>,
    #[serde(default)]
    pub git: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub native: Option<NativeSource>,
    #[serde(default)]
    pub plugin: Option<String>,
    #[serde(default, rename = "ref")]
    pub plugin_ref: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub subpath: Option<String>,
    #[serde(default)]
    pub bin: Option<String>,
    #[serde(default, rename = "bin-js")]
    pub bin_js: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Source {
    pub fn kind(&self) -> &'static str {
        if self.github_release.is_some() {
            "github-release"
        } else if self.archive.is_some() {
            "archive"
        } else if self.npm.is_some() {
            "npm"
        } else if self.npx.is_some() {
            "npx"
        } else if self.uvx.is_some() {
            "uvx"
        } else if self.pip.is_some() {
            "pip"
        } else if self.cargo.is_some() {
            "cargo"
        } else if self.go.is_some() {
            "go"
        } else if self.git.is_some() {
            "git"
        } else if self.system.is_some() {
            "system"
        } else if self.native.is_some() {
            "native"
        } else if self.plugin.is_some() {
            "plugin"
        } else {
            "unknown"
        }
    }

    pub fn validate(&self, package: &str) -> Result<()> {
        let count = self.github_release.is_some() as usize
            + self.archive.is_some() as usize
            + self.npm.is_some() as usize
            + self.npx.is_some() as usize
            + self.uvx.is_some() as usize
            + self.pip.is_some() as usize
            + self.cargo.is_some() as usize
            + self.go.is_some() as usize
            + self.git.is_some() as usize
            + self.system.is_some() as usize
            + self.native.is_some() as usize
            + self.plugin.is_some() as usize;
        if count != 1 {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "source must specify exactly one backend".to_owned(),
            });
        }
        if self.github_release.is_some() && self.asset.is_none() {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "github-release source requires an asset".to_owned(),
            });
        }
        if self.git.is_some() && self.rev.is_none() {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "git source requires a rev".to_owned(),
            });
        }
        if self.npm.is_some() && self.bin.is_some() && self.bin_js.is_some() {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "npm source accepts either bin or bin-js, not both".to_owned(),
            });
        }
        if self.npx.is_some() && (self.bin.is_some() || self.bin_js.is_some()) {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "npx source does not accept bin or bin-js".to_owned(),
            });
        }
        if self.uvx.is_some() && (self.bin.is_some() || self.bin_js.is_some()) {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "uvx source does not accept bin or bin-js".to_owned(),
            });
        }
        if self.plugin.is_some() && self.plugin_ref.is_none() {
            return Err(Error::InvalidPackage {
                name: package.to_owned(),
                message: "plugin source requires a ref".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NativeSource {
    #[serde(default)]
    pub winget: Option<String>,
    #[serde(default)]
    pub brew: Option<String>,
    #[serde(default)]
    pub apt: Option<String>,
    #[serde(default)]
    pub pacman: Option<String>,
    #[serde(default)]
    pub dnf: Option<String>,
}

impl NativeSource {
    pub fn id_for(&self, manager: NativeManager) -> Option<&str> {
        match manager {
            NativeManager::Winget => self.winget.as_deref(),
            NativeManager::Brew => self.brew.as_deref(),
            NativeManager::Apt => self.apt.as_deref(),
            NativeManager::Pacman => self.pacman.as_deref(),
            NativeManager::Dnf => self.dnf.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeManager {
    Winget,
    Brew,
    Apt,
    Pacman,
    Dnf,
}

impl NativeManager {
    pub const fn command(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Brew => "brew",
            Self::Apt => "apt",
            Self::Pacman => "pacman",
            Self::Dnf => "dnf",
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.command()
    }
}

impl fmt::Display for NativeManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
