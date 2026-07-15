use std::collections::{BTreeSet, HashSet};

use helix_runtime::Runtime;
use helix_view::handlers::PkgEvent;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MissingCapabilityKey {
    server: String,
    language: String,
    command: String,
    config_generation: u64,
    runtime_generation: u64,
}

#[derive(Debug)]
pub(super) struct PkgHandler {
    block: helix_runtime::Block,
    ingress: crate::runtime::RuntimeIngress,
    handled_missing: HashSet<MissingCapabilityKey>,
}

impl PkgHandler {
    fn new(block: helix_runtime::Block, ingress: crate::runtime::RuntimeIngress) -> Self {
        Self {
            block,
            ingress,
            handled_missing: HashSet::new(),
        }
    }

    async fn event(&mut self, event: PkgEvent) {
        let PkgEvent::MissingLanguageServer {
            documents,
            server,
            language,
            command,
            config,
            config_generation,
            runtime_generation,
        } = event;
        let key = MissingCapabilityKey {
            server: server.clone(),
            language: language.clone(),
            command: command.clone(),
            config_generation,
            runtime_generation,
        };
        if !self.handled_missing.insert(key) {
            return;
        }

        let lookup_language = language.clone();
        let lookup_command = command.clone();
        let lookup_config = config.clone();
        let status = match self
            .block
            .spawn(move || -> anyhow::Result<_> {
                let ops = helix_pkg::Ops::open_with_config(lookup_config)?;
                let runtime_assets = helix_loader::runtime_assets()?;
                let configured = helix_pkg::ConfiguredCapability::new(
                    helix_pkg::PkgKind::Lsp,
                    &lookup_command,
                    &lookup_command,
                    BTreeSet::from([lookup_language]),
                )
                .expect("missing language server command is non-empty");
                Ok(
                    helix_pkg::CapabilityCatalog::new(ops.registry(), runtime_assets)
                        .status_for_configured(configured)?,
                )
            })
            .await
        {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                log::warn!("failed to resolve package for missing language server: {error}");
                return;
            }
            Err(error) => {
                log::warn!("missing language server package lookup failed: {error}");
                return;
            }
        };

        if status.provider.is_usable() {
            let _ = self
                .ingress
                .send_task(crate::runtime::RuntimeTaskEvent::RefreshLanguageServers {
                    document_ids: documents,
                })
                .await;
            return;
        }
        let Some(package) = status.package.map(|package| package.name) else {
            log::debug!(
                "no package maps to missing language server '{server}' command '{command}'"
            );
            return;
        };
        if config.auto_install {
            if let Err(error) = self.ingress.package_with_origin(
                crate::runtime::PkgOperation::Install(vec![package]),
                config,
                crate::runtime::PkgOperationOrigin::MissingLanguageServer {
                    documents,
                    server,
                    command,
                },
            ) {
                self.ingress.status(error.to_string());
            }
        } else {
            self.ingress
                .status(format!("{command} not installed - :pkg-install {package}"));
        }
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<PkgEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        let block = runtime.block().clone();
        work.spawn(async move {
            let mut handler = PkgHandler::new(block, ingress);
            while let Some(event) = rx.recv().await {
                handler.event(event).await;
            }
        })
        .detach();
        tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_server_events_have_stable_deduplication_identity() {
        let key = MissingCapabilityKey {
            server: "rust-analyzer".to_owned(),
            language: "rust".to_owned(),
            command: "rust-analyzer".to_owned(),
            config_generation: 3,
            runtime_generation: 7,
        };
        let mut handled = HashSet::new();

        assert!(handled.insert(key.clone()));
        assert!(!handled.insert(key));
    }
}
