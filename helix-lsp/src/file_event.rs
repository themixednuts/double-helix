use std::{collections::HashMap, path::PathBuf, sync::Weak};

use globset::{GlobBuilder, GlobSetBuilder};
use helix_runtime::{channel, send_blocking, Receiver, Sender};

use crate::{lsp, Client, LanguageServerId};

enum Event {
    FileChanged {
        path: PathBuf,
    },
    Register {
        client_id: LanguageServerId,
        client: Weak<Client>,
        registration_id: String,
        options: lsp::DidChangeWatchedFilesRegistrationOptions,
    },
    Unregister {
        client_id: LanguageServerId,
        registration_id: String,
    },
    RemoveClient {
        client_id: LanguageServerId,
    },
}

#[derive(Default)]
struct ClientState {
    client: Weak<Client>,
    registered: HashMap<String, globset::GlobSet>,
}

/// The Handler uses a dedicated tokio task to respond to file change events by
/// forwarding changes to LSPs that have registered for notifications with a
/// matching glob.
///
/// When an LSP registers for the DidChangeWatchedFiles notification, the
/// Handler is notified by sending the registration details in addition to a
/// weak reference to the LSP client. This is done so that the Handler can have
/// access to the client without preventing the client from being dropped if it
/// is closed and the Handler isn't properly notified.
#[derive(Clone, Debug)]
pub struct Handler {
    tx: Sender<Event>,
}

impl Default for Handler {
    fn default() -> Self {
        Self::new()
    }
}

impl Handler {
    pub fn new() -> Self {
        let (tx, rx) = channel(256);
        tokio::spawn(Self::run(rx));
        Self { tx }
    }

    pub fn register(
        &self,
        client_id: LanguageServerId,
        client: Weak<Client>,
        registration_id: String,
        options: lsp::DidChangeWatchedFilesRegistrationOptions,
    ) {
        send_blocking(&self.tx, Event::Register {
            client_id,
            client,
            registration_id,
            options,
        });
    }

    pub fn unregister(&self, client_id: LanguageServerId, registration_id: String) {
        send_blocking(&self.tx, Event::Unregister {
            client_id,
            registration_id,
        });
    }

    pub fn file_changed(&self, path: PathBuf) {
        send_blocking(&self.tx, Event::FileChanged { path });
    }

    pub fn remove_client(&self, client_id: LanguageServerId) {
        send_blocking(&self.tx, Event::RemoveClient { client_id });
    }

    async fn run(mut rx: Receiver<Event>) {
        let mut state: HashMap<LanguageServerId, ClientState> = HashMap::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::FileChanged { path } => {
                    log::debug!("Received file event for {:?}", &path);

                    state.retain(|id, client_state| {
                        if !client_state
                            .registered
                            .values()
                            .any(|glob| glob.is_match(&path))
                        {
                            return true;
                        }
                        let Some(client) = client_state.client.upgrade() else {
                            log::warn!("LSP client was dropped: {id}");
                            return false;
                        };
                        let Ok(uri) = lsp::Url::from_file_path(&path) else {
                            return true;
                        };
                        log::debug!(
                            "Sending didChangeWatchedFiles notification to client '{}'",
                            client.name()
                        );
                        client.did_change_watched_files(vec![lsp::FileEvent {
                            uri,
                            // We currently always send the CHANGED state
                            // since we don't actually have more context at
                            // the moment.
                            typ: lsp::FileChangeType::CHANGED,
                        }]);
                        true
                    });
                }
                Event::Register {
                    client_id,
                    client,
                    registration_id,
                    options: ops,
                } => {
                    log::debug!(
                        "Registering didChangeWatchedFiles for client '{}' with id '{}'",
                        client_id,
                        registration_id
                    );

                    let entry = state.entry(client_id).or_default();
                    entry.client = client;

                    let mut builder = GlobSetBuilder::new();
                    for watcher in ops.watchers {
                        if let lsp::GlobPattern::String(pattern) = watcher.glob_pattern {
                            if let Ok(glob) = GlobBuilder::new(&pattern).build() {
                                builder.add(glob);
                            }
                        }
                    }
                    match builder.build() {
                        Ok(globset) => {
                            entry.registered.insert(registration_id, globset);
                        }
                        Err(err) => {
                            // Remove any old state for that registration id and
                            // remove the entire client if it's now empty.
                            entry.registered.remove(&registration_id);
                            if entry.registered.is_empty() {
                                state.remove(&client_id);
                            }
                            log::warn!(
                                "Unable to build globset for LSP didChangeWatchedFiles {err}"
                            )
                        }
                    }
                }
                Event::Unregister {
                    client_id,
                    registration_id,
                } => {
                    log::debug!(
                        "Unregistering didChangeWatchedFiles with id '{}' for client '{}'",
                        registration_id,
                        client_id
                    );
                    if let Some(client_state) = state.get_mut(&client_id) {
                        client_state.registered.remove(&registration_id);
                        if client_state.registered.is_empty() {
                            state.remove(&client_id);
                        }
                    }
                }
                Event::RemoveClient { client_id } => {
                    log::debug!("Removing LSP client: {client_id}");
                    state.remove(&client_id);
                }
            }
        }
    }
}
