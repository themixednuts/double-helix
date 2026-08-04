use crate::{
    protocol::{
        ErrorCode, FileChange, FileChangeKind, FileChanges, RemoteError, ServerEvent, ServerFrame,
        Watch, WatchId, MAX_ACTIVE_WATCHES,
    },
    workspace::{is_internal_path, relative_path, Workspace},
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};

pub(crate) struct WatchTable {
    watches: Mutex<HashMap<WatchId, WatchHandle>>,
    slots: Arc<Semaphore>,
}

struct WatchHandle {
    _watcher: RecommendedWatcher,
    _slot: OwnedSemaphorePermit,
}

impl WatchTable {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            watches: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_ACTIVE_WATCHES)),
        })
    }

    pub(crate) async fn start(
        &self,
        id: WatchId,
        request: Watch,
        workspace: Arc<Workspace>,
        outbound: mpsc::Sender<ServerFrame>,
    ) -> Result<(), RemoteError> {
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            RemoteError::new(ErrorCode::ResourceExhausted, "remote watch limit reached").retryable()
        })?;
        let path = workspace.resolve_existing(&request.path).await?;
        let root = workspace.root().to_path_buf();
        let mut watcher =
            notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
                match result {
                    Ok(event) => {
                        let kind = change_kind(&event.kind);
                        let changes = event
                            .paths
                            .iter()
                            .filter_map(|path| relative_path(&root, path).ok())
                            .filter(|path| !is_internal_path(path))
                            .map(|path| FileChange { path, kind })
                            .collect::<Vec<_>>();
                        if !changes.is_empty() {
                            let _ = outbound.blocking_send(ServerFrame::Event(
                                ServerEvent::FileChanges(FileChanges { watch: id, changes }),
                            ));
                        }
                    }
                    Err(error) => {
                        let _ = outbound.blocking_send(ServerFrame::Event(ServerEvent::Log(
                            crate::RemoteLog {
                                level: crate::RemoteLogLevel::Warn,
                                target: "remote_watch".to_owned(),
                                message: error.to_string(),
                            },
                        )));
                    }
                }
            })
            .map_err(|error| {
                RemoteError::new(
                    ErrorCode::Io,
                    format!("failed to create remote file watcher: {error}"),
                )
            })?;
        watcher
            .watch(
                &path,
                if request.recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|error| {
                RemoteError::new(
                    ErrorCode::Io,
                    format!("failed to watch remote path: {error}"),
                )
                .at(request.path)
            })?;
        let mut watches = self.watches.lock().await;
        if watches.contains_key(&id) {
            return Err(RemoteError::new(
                ErrorCode::Conflict,
                "remote watch ID is already active",
            ));
        }
        watches.insert(
            id,
            WatchHandle {
                _watcher: watcher,
                _slot: slot,
            },
        );
        Ok(())
    }

    pub(crate) async fn stop(&self, id: WatchId) {
        self.watches.lock().await.remove(&id);
    }

    pub(crate) async fn stop_all(&self) {
        self.watches.lock().await.clear();
    }
}

fn change_kind(kind: &EventKind) -> FileChangeKind {
    match kind {
        EventKind::Create(_) => FileChangeKind::Created,
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => FileChangeKind::Renamed,
        EventKind::Modify(_) => FileChangeKind::Modified,
        EventKind::Remove(_) => FileChangeKind::Removed,
        EventKind::Access(_) | EventKind::Other | EventKind::Any => FileChangeKind::Other,
    }
}
