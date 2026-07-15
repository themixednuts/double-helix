use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ops::ResolvedPackage,
    spec::{Artifact, PackageSpec},
    Error, Result, Store,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct PkgConfig {
    pub auto_install: bool,
    pub registries: Vec<PathBuf>,
    pub registry_sources: Vec<RegistrySource>,
    pub allow_native: NativeInstallPolicy,
    pub policy: Policy,
}

impl Default for PkgConfig {
    fn default() -> Self {
        Self {
            auto_install: false,
            registries: Vec::new(),
            registry_sources: Vec::new(),
            allow_native: NativeInstallPolicy::Prompt,
            policy: Policy::default(),
        }
    }
}

impl PkgConfig {
    pub fn registry_dirs(&self, store: &Store) -> Result<Vec<PathBuf>> {
        let mut dirs = self.registries.clone();
        for source in &self.registry_sources {
            dirs.push(source.active_dir(store)?);
        }
        Ok(dirs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RegistrySource {
    pub name: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub git: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub rev: Option<String>,
}

impl RegistrySource {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::Message(
                "registry source name must not be empty".to_owned(),
            ));
        }
        if sanitize_registry_name(&self.name).is_empty() {
            return Err(Error::Message(format!(
                "registry source {} does not produce a usable cache name",
                self.name
            )));
        }
        let source_count = self.path.is_some() as usize + self.git.is_some() as usize;
        if source_count != 1 {
            return Err(Error::Message(format!(
                "registry source {} must specify exactly one of path or git",
                self.name
            )));
        }
        if self.branch.is_some() && self.rev.is_some() {
            return Err(Error::Message(format!(
                "registry source {} accepts either branch or rev, not both",
                self.name
            )));
        }
        if self.path.is_some() && (self.branch.is_some() || self.rev.is_some()) {
            return Err(Error::Message(format!(
                "registry source {} uses path and cannot specify branch or rev",
                self.name
            )));
        }
        Ok(())
    }

    pub fn active_dir(&self, store: &Store) -> Result<PathBuf> {
        self.validate()?;
        Ok(self.path.clone().unwrap_or_else(|| self.cache_dir(store)))
    }

    pub fn cache_dir(&self, store: &Store) -> PathBuf {
        store
            .registry_dir()
            .join(sanitize_registry_name(&self.name))
    }

    pub fn source_label(&self) -> String {
        self.path
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| self.git.clone())
            .unwrap_or_default()
    }
}

fn sanitize_registry_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeInstallPolicy {
    True,
    False,
    Prompt,
}

impl Serialize for NativeInstallPolicy {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::True => "true",
            Self::False => "false",
            Self::Prompt => "prompt",
        })
    }
}

impl<'de> Deserialize<'de> for NativeInstallPolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = NativeInstallPolicy;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("true, false, or prompt")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(if value {
                    NativeInstallPolicy::True
                } else {
                    NativeInstallPolicy::False
                })
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "true" => Ok(NativeInstallPolicy::True),
                    "false" => Ok(NativeInstallPolicy::False),
                    "prompt" => Ok(NativeInstallPolicy::Prompt),
                    other => Err(E::custom(format!("unknown native install policy: {other}"))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Policy {
    pub run_scripts: bool,
    pub allow_build: bool,
    pub min_release_age_days: u64,
    pub allowed_backends: Vec<String>,
    pub blocked_backends: Vec<String>,
    pub allowed_sources: Vec<String>,
    pub allowed_plugin_backends: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            run_scripts: false,
            allow_build: true,
            min_release_age_days: 0,
            allowed_backends: Vec::new(),
            blocked_backends: Vec::new(),
            allowed_sources: Vec::new(),
            allowed_plugin_backends: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReport {
    pub warnings: Vec<String>,
}

impl Policy {
    pub fn check_source(&self, package: &PackageSpec, artifact: &Artifact) -> Result<()> {
        let source = &artifact.source;
        let backend = source.kind().to_owned();
        if self
            .blocked_backends
            .iter()
            .any(|blocked| blocked == &backend)
        {
            return Err(policy_violation(
                "blocked-backends",
                format!("{backend} is blocked for {}", package.name),
            ));
        }
        if !self.allowed_backends.is_empty()
            && !self
                .allowed_backends
                .iter()
                .any(|allowed| allowed == &backend)
        {
            return Err(policy_violation(
                "allowed-backends",
                format!("{backend} is not allowed for {}", package.name),
            ));
        }
        if !self.allow_build
            && (source.cargo.is_some() || source.go.is_some() || source.git.is_some())
        {
            return Err(policy_violation(
                "allow-build",
                format!("{} requires a build backend ({backend})", package.name),
            ));
        }
        if let Some(name) = source.plugin.as_deref() {
            if !self
                .allowed_plugin_backends
                .iter()
                .any(|allowed| allowed == name)
            {
                return Err(policy_violation(
                    "allowed-plugin-backends",
                    format!("plugin backend {name} must be explicitly allowed"),
                ));
            }
        }
        Ok(())
    }

    pub fn check(
        &self,
        package: &PackageSpec,
        artifact: &Artifact,
        resolved: &ResolvedPackage,
    ) -> Result<PolicyReport> {
        self.check_source(package, artifact)?;
        let backend = artifact.source.kind().to_owned();
        if !self.allowed_sources.is_empty()
            && resolved
                .source_url()
                .is_some_and(|url| !self.source_allowed(url))
        {
            return Err(policy_violation(
                "allowed-sources",
                format!("{} is not allowed by allowed-sources", resolved.url),
            ));
        }

        let mut warnings = Vec::new();
        if self.min_release_age_days > 0 {
            if let Some(published_at) = resolved.published_at.as_deref() {
                let age = release_age_days(published_at)?;
                if age < self.min_release_age_days {
                    return Err(policy_violation(
                        "min-release-age-days",
                        format!(
                            "{} {} is {age} days old, below the {} day minimum",
                            package.name, resolved.version, self.min_release_age_days
                        ),
                    ));
                }
            } else {
                warnings.push(format!(
                    "policy min-release-age-days skipped for {} because {} has no publish timestamp",
                    package.name, backend
                ));
            }
        }

        Ok(PolicyReport { warnings })
    }

    fn source_allowed(&self, url: &str) -> bool {
        self.allowed_sources
            .iter()
            .any(|pattern| wildcard_match(pattern, url))
    }
}

fn policy_violation(key: &'static str, message: String) -> Error {
    Error::PolicyViolation { key, message }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let mut rest = value;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(stripped) = rest.strip_prefix(part) else {
                return false;
            };
            rest = stripped;
        } else if let Some(index) = rest.find(part) {
            rest = &rest[index + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn release_age_days(value: &str) -> Result<u64> {
    let date = value
        .get(..10)
        .ok_or_else(|| Error::Message(format!("invalid publish timestamp: {value}")))?;
    let mut parts = date.split('-');
    let year = parts
        .next()
        .and_then(|part| part.parse::<i32>().ok())
        .ok_or_else(|| Error::Message(format!("invalid publish timestamp: {value}")))?;
    let month = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| Error::Message(format!("invalid publish timestamp: {value}")))?;
    let day = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| Error::Message(format!("invalid publish timestamp: {value}")))?;
    let published = days_from_civil(year, month, day);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| Error::Message(err.to_string()))?
        .as_secs()
        / 86_400;
    Ok(now.saturating_sub(published as u64))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - (month <= 2) as i32;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{NativeSource, Source};

    fn package(source: Source) -> (PackageSpec, Artifact, ResolvedPackage) {
        let artifact = Artifact {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            source,
            bin: "demo".to_owned(),
            args: Vec::new(),
            env: Default::default(),
        };
        let package = PackageSpec {
            name: "demo".to_owned(),
            kind: crate::PkgKind::Lsp,
            description: String::new(),
            homepage: None,
            aliases: Vec::new(),
            categories: Vec::new(),
            languages: Vec::new(),
            schemas: Default::default(),
            version: Default::default(),
            artifacts: vec![artifact.clone()],
        };
        let resolved = ResolvedPackage {
            version: "1".to_owned(),
            url: "https://github.com/example/demo".to_owned(),
            sha256: None,
            source: artifact.source.kind().to_owned(),
            published_at: Some("2000-01-01T00:00:00Z".to_owned()),
        };
        (package, artifact, resolved)
    }

    #[test]
    fn blocked_backend_is_rejected_before_install() {
        let (package, artifact, resolved) = package(Source {
            npm: Some("demo".to_owned()),
            ..Source::default()
        });
        let policy = Policy {
            blocked_backends: vec!["npm".to_owned()],
            ..Policy::default()
        };
        let err = policy.check(&package, &artifact, &resolved).unwrap_err();
        assert!(err.to_string().contains("blocked-backends"));
    }

    #[test]
    fn allow_build_false_rejects_build_backends() {
        let (package, artifact, resolved) = package(Source {
            cargo: Some("demo".to_owned()),
            ..Source::default()
        });
        let policy = Policy {
            allow_build: false,
            ..Policy::default()
        };
        let err = policy.check(&package, &artifact, &resolved).unwrap_err();
        assert!(err.to_string().contains("allow-build"));
    }

    #[test]
    fn plugin_backend_requires_explicit_allowlist() {
        let (package, artifact, resolved) = package(Source {
            plugin: Some("fixture".to_owned()),
            plugin_ref: Some("demo".to_owned()),
            ..Source::default()
        });
        let err = Policy::default()
            .check(&package, &artifact, &resolved)
            .unwrap_err();
        assert!(err.to_string().contains("allowed-plugin-backends"));
    }

    #[test]
    fn source_allowlist_uses_wildcards() {
        let (package, artifact, resolved) = package(Source {
            github_release: Some("example/demo".to_owned()),
            asset: Some("demo.zip".to_owned()),
            ..Source::default()
        });
        let policy = Policy {
            allowed_sources: vec!["https://github.com/example/*".to_owned()],
            ..Policy::default()
        };
        policy.check(&package, &artifact, &resolved).unwrap();
    }

    #[test]
    fn min_release_age_rejects_fresh_metadata() {
        let (package, artifact, mut resolved) = package(Source {
            native: Some(NativeSource {
                brew: Some("demo".to_owned()),
                ..NativeSource::default()
            }),
            ..Source::default()
        });
        resolved.published_at = Some("2999-01-01T00:00:00Z".to_owned());
        let policy = Policy {
            min_release_age_days: 1,
            ..Policy::default()
        };
        let err = policy.check(&package, &artifact, &resolved).unwrap_err();
        assert!(err.to_string().contains("min-release-age-days"));
    }

    #[test]
    fn allow_native_accepts_bool_or_prompt() {
        let enabled: PkgConfig = toml::from_str("allow-native = true").unwrap();
        assert_eq!(enabled.allow_native, NativeInstallPolicy::True);
        let prompt: PkgConfig = toml::from_str("allow-native = \"prompt\"").unwrap();
        assert_eq!(prompt.allow_native, NativeInstallPolicy::Prompt);
    }

    #[test]
    fn registry_sources_parse_and_resolve_cache_dirs() {
        let config: PkgConfig = toml::from_str(
            r#"
registries = ["C:/local-registry"]

[[registry-sources]]
name = "corp/tools"
git = "https://example.com/tools.git"
branch = "main"
"#,
        )
        .unwrap();
        let store = Store::open("C:/data/pkg");
        let dirs = config.registry_dirs(&store).unwrap();
        assert_eq!(dirs[0], PathBuf::from("C:/local-registry"));
        assert!(dirs[1].ends_with("corp-tools"));
    }
}
