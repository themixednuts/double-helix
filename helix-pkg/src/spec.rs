use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PkgKind {
    Lsp,
    Dap,
    Grammar,
    Plugin,
}

impl PkgKind {
    pub const ALL: [Self; 4] = [Self::Lsp, Self::Dap, Self::Grammar, Self::Plugin];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lsp => "lsp",
            Self::Dap => "dap",
            Self::Grammar => "grammar",
            Self::Plugin => "plugin",
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
            "grammar" => Ok(Self::Grammar),
            "plugin" => Ok(Self::Plugin),
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
    pub languages: Vec<String>,
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

    pub fn artifact(&self) -> Result<&Artifact> {
        self.artifact_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn is_system_only(&self) -> bool {
        self.artifacts
            .iter()
            .all(|artifact| artifact.source.system.is_some())
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
        } else {
            "unknown"
        }
    }

    pub fn validate(&self, package: &str) -> Result<()> {
        let count = self.github_release.is_some() as usize
            + self.archive.is_some() as usize
            + self.npm.is_some() as usize
            + self.pip.is_some() as usize
            + self.cargo.is_some() as usize
            + self.go.is_some() as usize
            + self.git.is_some() as usize
            + self.system.is_some() as usize;
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
        Ok(())
    }
}
