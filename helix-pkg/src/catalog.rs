use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    str::FromStr,
};

use helix_loader::{Origin, RuntimeAssets, RuntimeAssetsSnapshot};
use helix_store::ActivePackage;

use crate::{PackageSpec, PkgKind, Receipt, Registry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityProviderSource {
    Path,
    Explicit,
    Runtime,
}

impl CapabilityProviderSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Path => "$PATH",
            Self::Explicit => "explicit path",
            Self::Runtime => "runtime",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityProvider {
    Managed {
        version: String,
    },
    Command {
        command: String,
        path: PathBuf,
        source: CapabilityProviderSource,
    },
    RuntimeGrammar {
        grammar: String,
    },
    BrokenManaged {
        package: String,
        version: String,
        message: String,
    },
    Missing,
}

impl CapabilityProvider {
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        !matches!(self, Self::BrokenManaged { .. } | Self::Missing)
    }

    #[must_use]
    pub const fn source_label(&self) -> Option<&'static str> {
        match self {
            Self::Managed { .. } => Some("pkg"),
            Self::Command { source, .. } => Some(source.label()),
            Self::RuntimeGrammar { .. } => Some(CapabilityProviderSource::Runtime.label()),
            Self::BrokenManaged { .. } => Some("pkg"),
            Self::Missing => None,
        }
    }

    #[must_use]
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::Command { command, .. } => Some(command),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredCapability {
    pub kind: PkgKind,
    pub name: String,
    pub command: String,
    pub languages: BTreeSet<String>,
}

impl ConfiguredCapability {
    #[must_use]
    pub fn new(
        kind: PkgKind,
        name: impl Into<String>,
        command: impl Into<String>,
        languages: BTreeSet<String>,
    ) -> Option<Self> {
        let name = name.into();
        let command = command.into();
        if name.trim().is_empty() || command.trim().is_empty() {
            return None;
        }
        Some(Self {
            kind,
            name,
            command,
            languages,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CapabilityStatus {
    pub name: String,
    pub kind: PkgKind,
    pub package: Option<PackageSpec>,
    pub receipt: Option<Receipt>,
    pub configured: Vec<ConfiguredCapability>,
    pub languages: BTreeSet<String>,
    pub provider: CapabilityProvider,
    pub installable: bool,
    pub active: Option<ActivePackage>,
}

impl CapabilityStatus {
    #[must_use]
    pub fn is_pkg_managed(&self) -> bool {
        self.active.is_some()
    }

    #[must_use]
    pub fn is_installable(&self) -> bool {
        self.installable
    }
}

/// Builds a UI-facing catalog from one runtime activation snapshot.
///
/// Registry entries describe install intent. `RuntimeAssets` is the sole authority for active
/// packages and runnable commands. Receipts are attached only as verification/history metadata.
pub struct CapabilityCatalog<'a> {
    registry: &'a Registry,
    runtime_assets: &'a RuntimeAssets,
    receipts: Vec<Receipt>,
    configured: Vec<ConfiguredCapability>,
}

impl<'a> CapabilityCatalog<'a> {
    #[must_use]
    pub fn new(registry: &'a Registry, runtime_assets: &'a RuntimeAssets) -> Self {
        Self {
            registry,
            runtime_assets,
            receipts: Vec::new(),
            configured: Vec::new(),
        }
    }

    #[must_use]
    pub fn receipts(mut self, receipts: Vec<Receipt>) -> Self {
        self.receipts = receipts;
        self
    }

    #[must_use]
    pub fn configured(mut self, configured: Vec<ConfiguredCapability>) -> Self {
        self.configured = configured;
        self
    }

    pub fn statuses(self) -> Result<Vec<CapabilityStatus>, helix_loader::RuntimeAssetsError> {
        let runtime = self.runtime_assets.snapshot();
        let active = runtime
            .active_packages()
            .into_iter()
            .map(|package| ((package.kind.clone(), package.name.clone()), package))
            .collect::<BTreeMap<_, _>>();
        let receipts = self
            .receipts
            .into_iter()
            .map(|receipt| ((receipt.kind, receipt.name.clone()), receipt))
            .collect::<BTreeMap<_, _>>();
        let mut statuses = BTreeMap::<(PkgKind, String), CapabilityStatus>::new();

        for package in self.registry.iter() {
            let key = (package.kind, package.name.clone());
            let active_package = active
                .get(&(package.kind.as_str().to_owned(), package.name.clone()))
                .cloned();
            statuses.insert(
                key,
                CapabilityStatus {
                    name: package.name.clone(),
                    kind: package.kind,
                    package: Some(package.clone()),
                    receipt: receipts.get(&(package.kind, package.name.clone())).cloned(),
                    configured: Vec::new(),
                    languages: package.languages.iter().cloned().collect(),
                    provider: CapabilityProvider::Missing,
                    installable: !package.is_system_only(),
                    active: active_package,
                },
            );
        }

        for configured in self.configured {
            let package = self.registry.package_for_command(
                configured.kind,
                configured.languages.iter().next().map(String::as_str),
                &configured.command,
            );
            let key = package
                .map(|package| (package.kind, package.name.clone()))
                .unwrap_or_else(|| (configured.kind, configured.name.clone()));
            let status = statuses.entry(key).or_insert_with(|| CapabilityStatus {
                name: configured.name.clone(),
                kind: configured.kind,
                package: None,
                receipt: receipts
                    .get(&(configured.kind, configured.name.clone()))
                    .cloned(),
                configured: Vec::new(),
                languages: BTreeSet::new(),
                provider: CapabilityProvider::Missing,
                installable: false,
                active: active
                    .get(&(configured.kind.as_str().to_owned(), configured.name.clone()))
                    .cloned(),
            });
            status
                .languages
                .extend(configured.languages.iter().cloned());
            status.configured.push(configured);
        }

        for package in active.values() {
            let Ok(kind) = PkgKind::from_str(&package.kind) else {
                log::warn!(
                    "ignoring active package with unknown kind '{}': {}",
                    package.kind,
                    package.name
                );
                continue;
            };
            statuses
                .entry((kind, package.name.clone()))
                .or_insert_with(|| CapabilityStatus {
                    name: package.name.clone(),
                    kind,
                    package: None,
                    receipt: receipts.get(&(kind, package.name.clone())).cloned(),
                    configured: Vec::new(),
                    languages: BTreeSet::new(),
                    provider: CapabilityProvider::Missing,
                    installable: false,
                    active: Some(package.clone()),
                });
        }

        for status in statuses.values_mut() {
            status.provider = provider_for(status, &runtime)?;
        }
        Ok(statuses.into_values().collect())
    }

    /// Projects one configured capability without constructing or probing the full registry.
    /// This is the lookup path for latency-sensitive missing-capability handling.
    pub fn status_for_configured(
        &self,
        configured: ConfiguredCapability,
    ) -> Result<CapabilityStatus, helix_loader::RuntimeAssetsError> {
        let runtime = self.runtime_assets.snapshot();
        let package = self
            .registry
            .package_for_command(
                configured.kind,
                configured.languages.iter().next().map(String::as_str),
                &configured.command,
            )
            .cloned();
        let name = package
            .as_ref()
            .map(|package| package.name.clone())
            .unwrap_or_else(|| configured.name.clone());
        let kind = package
            .as_ref()
            .map(|package| package.kind)
            .unwrap_or(configured.kind);
        let active = runtime
            .active_packages()
            .into_iter()
            .find(|active| active.kind == kind.as_str() && active.name == name);
        let receipt = self
            .receipts
            .iter()
            .find(|receipt| receipt.kind == kind && receipt.name == name)
            .cloned();
        let mut languages: BTreeSet<String> = package
            .as_ref()
            .map(|package| package.languages.iter().cloned().collect())
            .unwrap_or_default();
        languages.extend(configured.languages.iter().cloned());
        let installable = package
            .as_ref()
            .is_some_and(|package| !package.is_system_only());
        let mut status = CapabilityStatus {
            name,
            kind,
            package,
            receipt,
            configured: vec![configured],
            languages,
            provider: CapabilityProvider::Missing,
            installable,
            active,
        };
        status.provider = provider_for(&status, &runtime)?;
        Ok(status)
    }
}

fn provider_for(
    status: &CapabilityStatus,
    runtime_assets: &RuntimeAssetsSnapshot,
) -> Result<CapabilityProvider, helix_loader::RuntimeAssetsError> {
    if status.kind == PkgKind::Grammar {
        return match runtime_assets.resolve_grammar(&status.name) {
            Ok(Some(grammar)) => Ok(match grammar.origin {
                Origin::Managed { package } => CapabilityProvider::Managed {
                    version: package.version,
                },
                _ => CapabilityProvider::RuntimeGrammar {
                    grammar: status.name.clone(),
                },
            }),
            Ok(None) => missing_or_broken_activation(status),
            Err(error) => provider_from_resolution_error(error),
        };
    }

    if status.kind == PkgKind::Plugin {
        return match runtime_assets.resolve_plugin_root(&status.name) {
            Ok(Some(plugin)) => Ok(match plugin.origin {
                Origin::Managed { package } => CapabilityProvider::Managed {
                    version: package.version,
                },
                _ => unreachable!("plugin roots are currently managed assets"),
            }),
            Ok(None) => missing_or_broken_activation(status),
            Err(error) => provider_from_resolution_error(error),
        };
    }

    let mut commands = status
        .configured
        .iter()
        .map(|configured| configured.command.clone())
        .collect::<Vec<_>>();
    if let Some(package) = &status.package {
        let host_artifacts = package
            .artifacts_for(std::env::consts::OS, std::env::consts::ARCH)
            .collect::<Vec<_>>();
        let artifacts = if host_artifacts.is_empty() {
            package.artifacts.iter().collect::<Vec<_>>()
        } else {
            host_artifacts
        };
        for artifact in artifacts {
            commands.push(artifact.bin.clone());
            if let Some(command) = artifact.source.bin.as_deref() {
                commands.push(command.to_owned());
            }
            if let Some(command) = artifact.source.system.as_deref() {
                commands.push(command.to_owned());
            }
        }
    }
    if let Some(active) = &status.active {
        commands.extend(runtime_assets.command_keys_for_package(active));
    }

    let mut seen = BTreeSet::new();
    for command in commands {
        if !seen.insert(command.clone()) {
            continue;
        }
        let launch = match runtime_assets.resolve_command(&command) {
            Ok(Some(launch)) => launch,
            Ok(None) => continue,
            Err(error) => return provider_from_resolution_error(error),
        };
        return Ok(match launch.origin {
            Origin::Managed { package } => CapabilityProvider::Managed {
                version: package.version,
            },
            Origin::Explicit => CapabilityProvider::Command {
                command: command.to_owned(),
                path: launch.program,
                source: CapabilityProviderSource::Explicit,
            },
            Origin::Path => CapabilityProvider::Command {
                command: command.to_owned(),
                path: launch.program,
                source: CapabilityProviderSource::Path,
            },
            Origin::RuntimeOverride { .. } | Origin::BundledRuntime { .. } => continue,
        });
    }

    missing_or_broken_activation(status)
}

fn missing_or_broken_activation(
    status: &CapabilityStatus,
) -> Result<CapabilityProvider, helix_loader::RuntimeAssetsError> {
    Ok(match &status.active {
        Some(package) => CapabilityProvider::BrokenManaged {
            package: package.name.clone(),
            version: package.version.clone(),
            message: format!(
                "active pkg {} {} does not provide a usable {} capability",
                package.name, package.version, status.kind
            ),
        },
        None => CapabilityProvider::Missing,
    })
}

fn provider_from_resolution_error(
    error: helix_loader::RuntimeAssetsError,
) -> Result<CapabilityProvider, helix_loader::RuntimeAssetsError> {
    let helix_loader::RuntimeAssetsError::BrokenManaged { package, .. } = &error else {
        return Err(error);
    };
    Ok(CapabilityProvider::BrokenManaged {
        package: package.name.clone(),
        version: package.version.clone(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, fs};

    use helix_loader::{RuntimeAsset, RuntimeAssetSpec, RuntimeAssetsSnapshot, RuntimeSnapshot};
    use tempfile::TempDir;

    use super::*;

    fn empty_assets() -> RuntimeAssets {
        RuntimeAssets::from_snapshot(
            RuntimeAssetsSnapshot::from(RuntimeSnapshot {
                generation: 0,
                assets: Vec::new(),
            })
            .with_search_path(Some(OsString::new())),
        )
    }

    #[test]
    fn receipt_metadata_never_claims_a_package_is_active() {
        let registry = Registry::builtin().unwrap();
        let assets = empty_assets();
        let receipt = Receipt {
            name: "rust-analyzer".into(),
            kind: PkgKind::Lsp,
            version: "stale".into(),
            source: "fixture".into(),
            url: String::new(),
            archive_sha256: String::new(),
            bin: "rust-analyzer".into(),
            shim: String::new(),
            previous_version: None,
            files: BTreeMap::new(),
            installed_at: String::new(),
            native_manager: None,
            native_id: None,
        };

        let statuses = CapabilityCatalog::new(&registry, &assets)
            .receipts(vec![receipt])
            .statuses()
            .unwrap();
        let status = statuses
            .iter()
            .find(|status| status.name == "rust-analyzer")
            .unwrap();

        assert!(status.receipt.is_some());
        assert_eq!(status.provider, CapabilityProvider::Missing);
    }

    #[test]
    fn active_snapshot_surfaces_packages_missing_from_the_registry() {
        let dir = TempDir::new().unwrap();
        let executable = dir.path().join("custom-lsp");
        fs::write(&executable, "fixture").unwrap();
        let package = ActivePackage::new("lsp", "custom-lsp", "1.2.3");
        let assets = RuntimeAssets::from_snapshot(RuntimeAssetsSnapshot::from(RuntimeSnapshot {
            generation: 1,
            assets: vec![RuntimeAsset::from_spec(
                package,
                RuntimeAssetSpec::command("custom-lsp", executable),
            )],
        }));

        let statuses = CapabilityCatalog::new(&Registry::default(), &assets)
            .statuses()
            .unwrap();

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "custom-lsp");
        assert_eq!(
            statuses[0].provider,
            CapabilityProvider::Managed {
                version: "1.2.3".into()
            }
        );
    }

    #[test]
    fn configured_explicit_command_is_ready_but_not_pkg_managed() {
        let dir = TempDir::new().unwrap();
        let executable = dir.path().join("private-lsp");
        fs::write(&executable, "fixture").unwrap();
        let configured = ConfiguredCapability::new(
            PkgKind::Lsp,
            "private",
            executable.display().to_string(),
            BTreeSet::from(["private-language".into()]),
        )
        .unwrap();
        let assets = empty_assets();

        let statuses = CapabilityCatalog::new(&Registry::default(), &assets)
            .configured(vec![configured])
            .statuses()
            .unwrap();

        assert!(matches!(
            statuses[0].provider,
            CapabilityProvider::Command {
                source: CapabilityProviderSource::Explicit,
                ..
            }
        ));
        assert!(!statuses[0].is_pkg_managed());
    }

    #[test]
    fn broken_active_command_stays_installed_but_is_not_usable() {
        let dir = TempDir::new().unwrap();
        let package = ActivePackage::new("lsp", "broken-lsp", "1.2.3");
        let assets = RuntimeAssets::from_snapshot(RuntimeAssetsSnapshot::from(RuntimeSnapshot {
            generation: 1,
            assets: vec![RuntimeAsset::from_spec(
                package,
                RuntimeAssetSpec::command("broken-lsp", dir.path().join("missing")),
            )],
        }));

        let status = CapabilityCatalog::new(&Registry::default(), &assets)
            .statuses()
            .unwrap()
            .pop()
            .unwrap();

        assert!(status.is_pkg_managed());
        assert!(!status.provider.is_usable());
        assert!(matches!(
            status.provider,
            CapabilityProvider::BrokenManaged { .. }
        ));
    }

    #[test]
    fn broken_active_plugin_root_is_not_usable() {
        let dir = TempDir::new().unwrap();
        let package = ActivePackage::new("plugin", "broken-plugin", "9");
        let assets = RuntimeAssets::from_snapshot(RuntimeAssetsSnapshot::from(RuntimeSnapshot {
            generation: 1,
            assets: vec![RuntimeAsset::from_spec(
                package,
                RuntimeAssetSpec::plugin_root("broken-plugin", dir.path().join("missing")),
            )],
        }));

        let status = CapabilityCatalog::new(&Registry::default(), &assets)
            .statuses()
            .unwrap()
            .pop()
            .unwrap();

        assert!(status.is_pkg_managed());
        assert!(!status.provider.is_usable());
        assert!(matches!(
            status.provider,
            CapabilityProvider::BrokenManaged { .. }
        ));
    }

    #[test]
    fn focused_configured_projection_matches_full_catalog() {
        let registry = Registry::builtin().unwrap();
        let assets = empty_assets();
        let configured = ConfiguredCapability::new(
            PkgKind::Lsp,
            "rust-analyzer",
            "rust-analyzer",
            BTreeSet::from(["rust".into()]),
        )
        .unwrap();

        let focused = CapabilityCatalog::new(&registry, &assets)
            .status_for_configured(configured.clone())
            .unwrap();
        let full = CapabilityCatalog::new(&registry, &assets)
            .configured(vec![configured])
            .statuses()
            .unwrap()
            .into_iter()
            .find(|status| status.kind == PkgKind::Lsp && status.name == "rust-analyzer")
            .unwrap();

        assert_eq!(focused.name, full.name);
        assert_eq!(focused.kind, full.kind);
        assert_eq!(focused.package, full.package);
        assert_eq!(focused.provider, full.provider);
        assert_eq!(focused.installable, full.installable);
        assert_eq!(focused.languages, full.languages);
    }
}
