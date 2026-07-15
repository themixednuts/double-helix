use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};

use globset::{GlobBuilder, GlobSetBuilder};
use helix_runtime::{PulseGate, PulseHandle, PulseReceiver};

use crate::{lsp, Client, LanguageServerId};

#[derive(Clone, Default)]
struct ClientState {
    client: Weak<Client>,
    registered: HashMap<String, Arc<globset::GlobSet>>,
}

type RegistrationSnapshot = HashMap<LanguageServerId, ClientState>;

#[derive(Default)]
struct PendingFileEvents {
    // A path captures the registrations that existed when the event occurred. Repeated changes to
    // the same path are supersedable and retain only the latest registration snapshot.
    paths: HashMap<PathBuf, Arc<RegistrationSnapshot>>,
}

#[derive(Debug)]
enum FileEventPulse {}

/// Routes watched-file changes without placing backpressure on editor input.
///
/// Registration mutations publish immutable snapshots synchronously. File changes only clone the
/// current snapshot, coalesce by path, and request a capacity-one wakeup. One worker performs glob
/// matching and batches notifications per language server.
#[derive(Clone)]
pub struct Handler {
    registrations: Arc<Mutex<Arc<RegistrationSnapshot>>>,
    pending: Arc<Mutex<PendingFileEvents>>,
    wake: PulseHandle<FileEventPulse>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registrations = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .len();
        formatter
            .debug_struct("Handler")
            .field("registrations", &registrations)
            .field("pending_paths", &pending)
            .finish()
    }
}

impl Default for Handler {
    fn default() -> Self {
        Self::new()
    }
}

impl Handler {
    pub fn new() -> Self {
        let registrations = Arc::new(Mutex::new(Arc::new(RegistrationSnapshot::new())));
        let pending = Arc::new(Mutex::new(PendingFileEvents::default()));
        let mut gate = PulseGate::new();
        tokio::spawn(Self::run(
            registrations.clone(),
            pending.clone(),
            gate.take_receiver(),
        ));
        Self {
            registrations,
            pending,
            wake: gate.handle(),
        }
    }

    pub fn register(
        &self,
        client_id: LanguageServerId,
        client: Weak<Client>,
        registration_id: String,
        options: lsp::DidChangeWatchedFilesRegistrationOptions,
    ) {
        log::debug!(
            "Registering didChangeWatchedFiles for client '{}' with id '{}'",
            client_id,
            registration_id
        );

        let mut builder = GlobSetBuilder::new();
        for watcher in options.watchers {
            if let lsp::GlobPattern::String(pattern) = watcher.glob_pattern {
                if let Ok(glob) = GlobBuilder::new(&pattern).build() {
                    builder.add(glob);
                }
            }
        }
        let globset = match builder.build() {
            Ok(globset) => Arc::new(globset),
            Err(error) => {
                log::warn!("Unable to build globset for LSP didChangeWatchedFiles {error}");
                self.unregister(client_id, registration_id);
                return;
            }
        };

        self.update_registrations(|registrations| {
            let entry = registrations.entry(client_id).or_default();
            entry.client = client;
            entry.registered.insert(registration_id, globset);
        });
    }

    pub fn unregister(&self, client_id: LanguageServerId, registration_id: String) {
        log::debug!(
            "Unregistering didChangeWatchedFiles with id '{}' for client '{}'",
            registration_id,
            client_id
        );
        self.update_registrations(|registrations| {
            if let Some(client_state) = registrations.get_mut(&client_id) {
                client_state.registered.remove(&registration_id);
                if client_state.registered.is_empty() {
                    registrations.remove(&client_id);
                }
            }
        });
    }

    pub fn file_changed(&self, path: PathBuf) {
        let snapshot = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if snapshot.is_empty() {
            return;
        }
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .insert(path, snapshot);
        self.wake.request();
    }

    pub fn remove_client(&self, client_id: LanguageServerId) {
        log::debug!("Removing LSP client: {client_id}");
        self.update_registrations(|registrations| {
            registrations.remove(&client_id);
        });
    }

    fn update_registrations(&self, update: impl FnOnce(&mut RegistrationSnapshot)) {
        let mut snapshot = self
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(Arc::make_mut(&mut snapshot));
    }

    async fn run(
        registrations: Arc<Mutex<Arc<RegistrationSnapshot>>>,
        pending: Arc<Mutex<PendingFileEvents>>,
        mut wake: PulseReceiver<FileEventPulse>,
    ) {
        while wake.recv().await.is_some() {
            let paths = std::mem::take(
                &mut pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .paths,
            );
            let mut notifications =
                HashMap::<LanguageServerId, (Arc<Client>, Vec<lsp::FileEvent>)>::new();
            let mut dropped_clients = Vec::new();

            for (path, snapshot) in paths {
                let Ok(uri) = lsp::Url::from_file_path(&path) else {
                    continue;
                };
                for (client_id, client_state) in snapshot.iter() {
                    if !client_state
                        .registered
                        .values()
                        .any(|glob| glob.is_match(&path))
                    {
                        continue;
                    }
                    let Some(client) = client_state.client.upgrade() else {
                        dropped_clients.push(*client_id);
                        continue;
                    };
                    notifications
                        .entry(*client_id)
                        .or_insert_with(|| (client, Vec::new()))
                        .1
                        .push(lsp::FileEvent {
                            uri: uri.clone(),
                            typ: lsp::FileChangeType::CHANGED,
                        });
                }
            }

            for (_, (client, changes)) in notifications {
                log::debug!(
                    "Sending {} didChangeWatchedFiles notifications to client '{}'",
                    changes.len(),
                    client.name()
                );
                client.did_change_watched_files(changes);
            }

            if !dropped_clients.is_empty() {
                let mut snapshot = registrations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let current = Arc::make_mut(&mut snapshot);
                dropped_clients.sort_unstable();
                dropped_clients.dedup();
                for client_id in dropped_clients {
                    if current
                        .get(&client_id)
                        .is_some_and(|state| state.client.upgrade().is_none())
                    {
                        current.remove(&client_id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_paths_coalesce_and_keep_latest_registration_snapshot() {
        let path = PathBuf::from("src/main.rs");
        let first = Arc::new(RegistrationSnapshot::new());
        let mut second_map = RegistrationSnapshot::new();
        second_map.insert(LanguageServerId::default(), ClientState::default());
        let second = Arc::new(second_map);
        let mut pending = PendingFileEvents::default();

        pending.paths.insert(path.clone(), first);
        pending.paths.insert(path.clone(), second.clone());

        assert_eq!(pending.paths.len(), 1);
        assert!(Arc::ptr_eq(pending.paths.get(&path).unwrap(), &second));
    }
}
