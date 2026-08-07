use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use helix_lsp::{
    Client, ClientLaunchIdentity, LanguageServerLaunchRequest, LanguageServerName,
    PreparedClientLaunch, PreparedExternalLanguageServerCommand, PreparedLanguageServerLaunch,
    SpawnedLanguageServer,
};
use helix_runtime::{Receiver, Sender};

use crate::{DocumentId, Editor};

const COMPLETION_CAPACITY: usize = 256;
const MAX_RESTART_ATTEMPTS: u8 = 5;
const BASE_RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LaunchOrigin {
    Automatic,
    RuntimeChange,
    ExplicitRestart,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DemandKey {
    document: DocumentId,
    server: LanguageServerName,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InstanceKey {
    server: LanguageServerName,
    root: String,
}

#[derive(Clone, Debug, PartialEq)]
struct DemandSignature {
    config_generation: u64,
    language: String,
    location: DemandLocation,
    command: String,
    args: Vec<String>,
    environment: HashMap<String, String>,
    config: Option<serde_json::Value>,
    root_dirs: Vec<PathBuf>,
    enable_snippets: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum DemandLocation {
    Local(PathBuf),
    Remote {
        session: helix_remote::SessionId,
        path: helix_remote::WorkspacePath,
    },
    Collaboration {
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
    },
}

#[derive(Clone, Debug)]
enum SupervisorLaunchRequest {
    Local(LanguageServerLaunchRequest),
    Remote(RemoteLanguageServerLaunchRequest),
    Collaboration(CollaborationLanguageServerLaunchRequest),
}

#[derive(Clone, Debug)]
struct RemoteLanguageServerLaunchRequest {
    backend: Arc<helix_remote::backend::RemoteWorkspaceClient>,
    name: String,
    language: Arc<helix_core::syntax::config::LanguageConfiguration>,
    server: helix_core::syntax::config::LanguageServerConfiguration,
    document: helix_remote::WorkspacePath,
    root_dirs: Vec<PathBuf>,
    enable_snippets: bool,
}

#[derive(Clone, Debug)]
struct CollaborationLanguageServerLaunchRequest {
    session: helix_collab::GuestSessionHandle,
    project: helix_collab::ProjectId,
    buffer: helix_collab::BufferId,
    name: String,
    enable_snippets: bool,
    timeout: u64,
}

#[derive(Debug)]
enum PreparedSupervisorLaunch {
    Ready(Box<(PreparedClientLaunch, LanguageServerLaunchTarget)>),
    NoRequiredRoot,
}

#[derive(Debug)]
enum LanguageServerLaunchTarget {
    Local,
    Remote {
        backend: Arc<helix_remote::backend::RemoteWorkspaceClient>,
        cwd: helix_remote::WorkspacePath,
    },
    Collaboration {
        session: helix_collab::GuestSessionHandle,
        buffer: helix_collab::BufferId,
        server: String,
    },
}

#[derive(Debug)]
struct RemoteLanguageServerProcess(helix_remote::backend::RemoteProcessControl);

impl helix_lsp::LanguageServerProcess for RemoteLanguageServerProcess {
    fn kill(&self) -> futures_util::future::BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move { self.0.kill().await.map_err(std::io::Error::other) })
    }

    fn wait(&self) -> futures_util::future::BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            self.0
                .wait()
                .await
                .map(|_| ())
                .map_err(std::io::Error::other)
        })
    }
}

#[derive(Debug)]
struct CollaborationLanguageServerState {
    closed: tokio::sync::watch::Sender<bool>,
}

impl CollaborationLanguageServerState {
    fn new() -> Arc<Self> {
        let (closed, _) = tokio::sync::watch::channel(false);
        Arc::new(Self { closed })
    }

    fn close(&self) {
        self.closed.send_if_modified(|closed| {
            if *closed {
                false
            } else {
                *closed = true;
                true
            }
        });
    }

    async fn wait(&self) {
        let mut closed = self.closed.subscribe();
        while !*closed.borrow() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }
}

#[derive(Debug)]
struct CollaborationLanguageServerProcess(Arc<CollaborationLanguageServerState>);

impl helix_lsp::LanguageServerProcess for CollaborationLanguageServerProcess {
    fn kill(&self) -> futures_util::future::BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            self.0.close();
            Ok(())
        })
    }

    fn wait(&self) -> futures_util::future::BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            self.0.wait().await;
            Ok(())
        })
    }
}

#[derive(Debug)]
struct CollaborationLanguageServerProxy {
    session: helix_collab::GuestSessionHandle,
    buffer: helix_collab::BufferId,
    server: String,
    state: Arc<CollaborationLanguageServerState>,
}

impl helix_lsp::LanguageServerProxy for CollaborationLanguageServerProxy {
    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> futures_util::future::BoxFuture<'_, helix_lsp::Result<serde_json::Value>> {
        let method = method.to_owned();
        Box::pin(async move {
            if method == "shutdown" {
                self.state.close();
                return Ok(serde_json::Value::Null);
            }
            let params = serde_json::to_vec(&params)?;
            let result = self
                .session
                .language_server_request(self.buffer, self.server.clone(), method, params)
                .await
                .map_err(|error| helix_lsp::Error::Other(anyhow::Error::new(error)))?;
            match result {
                Ok(value) => serde_json::from_slice(&value).map_err(Into::into),
                Err(error) => {
                    let data = error
                        .data
                        .map(|data| serde_json::from_slice(&data))
                        .transpose()?;
                    Err(helix_lsp::Error::Rpc(helix_lsp::jsonrpc::Error {
                        code: error.code.into(),
                        message: error.message,
                        data,
                    }))
                }
            }
        })
    }
}

#[derive(Debug)]
enum DemandStage {
    Preparing {
        active_revision: u64,
    },
    Prepared {
        instance: InstanceKey,
        identity: Box<ClientLaunchIdentity>,
        launch: Option<Box<(PreparedClientLaunch, LanguageServerLaunchTarget)>>,
    },
    Attached {
        client: helix_lsp::LanguageServerId,
    },
    Retrying,
    Failed,
}

#[derive(Debug)]
struct DemandState {
    revision: u64,
    signature: DemandSignature,
    request: SupervisorLaunchRequest,
    origin: LaunchOrigin,
    failure_count: u8,
    stage: DemandStage,
}

#[derive(Debug)]
struct SpawnFlight {
    id: helix_lsp::LanguageServerId,
    identity: ClientLaunchIdentity,
}

#[derive(Debug)]
enum SupervisorEventKind {
    Prepared {
        key: DemandKey,
        revision: u64,
        result: helix_lsp::Result<PreparedSupervisorLaunch>,
    },
    Spawned {
        instance: InstanceKey,
        id: helix_lsp::LanguageServerId,
        result: Box<helix_lsp::Result<SpawnedLanguageServer>>,
    },
    Retry {
        key: DemandKey,
        revision: u64,
    },
}

#[derive(Debug)]
pub struct LanguageServerSupervisorEvent(SupervisorEventKind);

pub(crate) struct LanguageServerSupervisor {
    tx: Sender<LanguageServerSupervisorEvent>,
    rx: Option<Receiver<LanguageServerSupervisorEvent>>,
    demands: HashMap<DemandKey, DemandState>,
    instances: HashMap<InstanceKey, SpawnFlight>,
    next_revision: u64,
}

impl std::fmt::Debug for LanguageServerSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanguageServerSupervisor")
            .field("demands", &self.demands.len())
            .field("instances", &self.instances)
            .field("next_revision", &self.next_revision)
            .finish_non_exhaustive()
    }
}

impl Default for LanguageServerSupervisor {
    fn default() -> Self {
        let (tx, rx) = helix_runtime::channel(COMPLETION_CAPACITY);
        Self {
            tx,
            rx: Some(rx),
            demands: HashMap::new(),
            instances: HashMap::new(),
            next_revision: 0,
        }
    }
}

impl LanguageServerSupervisor {
    fn next_revision(&mut self) -> u64 {
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        self.next_revision
    }
}

impl Editor {
    pub fn take_language_server_supervisor_rx(
        &mut self,
    ) -> Receiver<LanguageServerSupervisorEvent> {
        self.language_server_supervisor
            .rx
            .take()
            .expect("language-server supervisor receiver already taken")
    }

    pub fn handle_language_server_supervisor_event(
        &mut self,
        event: LanguageServerSupervisorEvent,
    ) {
        match event.0 {
            SupervisorEventKind::Prepared {
                key,
                revision,
                result,
            } => self.handle_prepared_language_server(key, revision, result),
            SupervisorEventKind::Spawned {
                instance,
                id,
                result,
            } => self.handle_spawned_language_server(instance, id, *result),
            SupervisorEventKind::Retry { key, revision } => {
                self.handle_language_server_retry(key, revision)
            }
        }
    }

    pub(super) fn reconcile_language_server_demands(
        &mut self,
        document: DocumentId,
        origin: LaunchOrigin,
        force_servers: Option<&HashSet<String>>,
    ) {
        if !self.config().lsp.enable {
            self.remove_document_language_server_demands(document, &HashSet::new());
            return;
        }

        let Some(doc) = self.documents.get(&document) else {
            self.remove_document_language_server_demands(document, &HashSet::new());
            return;
        };
        if doc.url().is_none() {
            self.remove_document_language_server_demands(document, &HashSet::new());
            return;
        }
        let Some(language) = doc.language_configuration().cloned() else {
            self.remove_document_language_server_demands(document, &HashSet::new());
            return;
        };
        let Some(location) = doc.location().cloned() else {
            self.remove_document_language_server_demands(document, &HashSet::new());
            return;
        };
        let (demand_location, local_path, remote, collaboration) = match location {
            crate::file_bound::DocumentLocation::Local(path) => {
                (DemandLocation::Local(path.clone()), Some(path), None, None)
            }
            crate::file_bound::DocumentLocation::Remote(location) => {
                let Some(backend) = self
                    .workspace_backend
                    .remote()
                    .cloned()
                    .filter(|backend| backend.workspace().session == location.session)
                else {
                    self.remove_document_language_server_demands(document, &HashSet::new());
                    return;
                };
                (
                    DemandLocation::Remote {
                        session: location.session,
                        path: location.path.clone(),
                    },
                    None,
                    Some((backend, location.path)),
                    None,
                )
            }
            crate::file_bound::DocumentLocation::Collaboration(location) => {
                let Some(session) = self
                    .collaboration
                    .session()
                    .filter(|session| session.project().id == location.project)
                else {
                    self.remove_document_language_server_demands(document, &HashSet::new());
                    return;
                };
                let Some(buffer) = self.collaboration.buffer(document) else {
                    self.remove_document_language_server_demands(document, &HashSet::new());
                    return;
                };
                let demand = DemandLocation::Collaboration {
                    project: location.project,
                    path: location.path.clone(),
                };
                (
                    demand,
                    None,
                    None,
                    Some((session, location.project, buffer)),
                )
            }
        };
        let editor_config = doc.config.load();
        let root_dirs = editor_config.workspace_lsp_roots.clone();
        let enable_snippets = editor_config.lsp.snippets;
        drop(editor_config);

        let desired = language
            .language_servers
            .iter()
            .map(|server| server.name.clone())
            .collect::<HashSet<_>>();
        self.remove_document_language_server_demands(document, &desired);

        let loader = self.syn_loader.load();
        let requests = language
            .language_servers
            .iter()
            .filter_map(|features| {
                let server = loader
                    .language_server_configs()
                    .get(&features.name)?
                    .clone();
                Some((features.name.clone(), server))
            })
            .collect::<Vec<_>>();
        drop(loader);

        for (server_name, server) in requests {
            if self.language_servers.is_manually_stopped(&server_name) {
                self.language_server_supervisor.demands.remove(&DemandKey {
                    document,
                    server: server_name.clone(),
                });
                self.detach_document_language_server(document, &server_name);
                continue;
            }

            let signature = DemandSignature {
                config_generation: self.config_gen,
                language: language.language_id.clone(),
                location: demand_location.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                environment: server.environment.clone(),
                config: server.config.clone(),
                root_dirs: root_dirs.clone(),
                enable_snippets,
            };
            let request = match (&remote, &collaboration) {
                (Some((backend, path)), _) => {
                    SupervisorLaunchRequest::Remote(RemoteLanguageServerLaunchRequest {
                        backend: backend.clone(),
                        name: server_name.clone(),
                        language: language.clone(),
                        server,
                        document: path.clone(),
                        root_dirs: root_dirs.clone(),
                        enable_snippets,
                    })
                }
                (_, Some((session, project, buffer))) => SupervisorLaunchRequest::Collaboration(
                    CollaborationLanguageServerLaunchRequest {
                        session: session.clone(),
                        project: *project,
                        buffer: *buffer,
                        name: server_name.clone(),
                        enable_snippets,
                        timeout: server.timeout,
                    },
                ),
                (None, None) => SupervisorLaunchRequest::Local(LanguageServerLaunchRequest {
                    name: server_name.clone(),
                    language: language.clone(),
                    server,
                    doc_path: local_path.clone(),
                    root_dirs: root_dirs.clone(),
                    enable_snippets,
                }),
            };
            let force = force_servers.is_some_and(|servers| servers.contains(&server_name));
            self.upsert_language_server_demand(
                DemandKey {
                    document,
                    server: server_name,
                },
                signature,
                request,
                origin,
                force,
            );
        }
    }

    fn remove_document_language_server_demands(
        &mut self,
        document: DocumentId,
        desired: &HashSet<String>,
    ) {
        let obsolete = self
            .language_server_supervisor
            .demands
            .keys()
            .filter(|key| key.document == document && !desired.contains(&key.server))
            .cloned()
            .collect::<Vec<_>>();
        for key in obsolete {
            self.language_server_supervisor.demands.remove(&key);
            self.detach_document_language_server(document, &key.server);
        }
    }

    fn upsert_language_server_demand(
        &mut self,
        key: DemandKey,
        signature: DemandSignature,
        request: SupervisorLaunchRequest,
        origin: LaunchOrigin,
        force: bool,
    ) {
        let existing = self.language_server_supervisor.demands.get(&key);
        if !force
            && existing.is_some_and(|state| {
                state.signature == signature && !matches!(state.stage, DemandStage::Failed)
            })
        {
            return;
        }
        if !force
            && existing.is_some_and(|state| {
                state.signature == signature && matches!(state.stage, DemandStage::Failed)
            })
        {
            return;
        }

        let active_prepare = existing.and_then(|state| match state.stage {
            DemandStage::Preparing { active_revision } => Some(active_revision),
            _ => None,
        });
        let should_detach = existing.is_some_and(|state| {
            force
                || state.signature != signature
                    && matches!(state.stage, DemandStage::Attached { .. })
        });
        if should_detach {
            self.detach_document_language_server(key.document, &key.server);
        }

        let revision = self.language_server_supervisor.next_revision();
        let stage = active_prepare.map_or(
            DemandStage::Preparing {
                active_revision: revision,
            },
            |active_revision| DemandStage::Preparing { active_revision },
        );
        self.language_server_supervisor.demands.insert(
            key.clone(),
            DemandState {
                revision,
                signature,
                request,
                origin,
                failure_count: 0,
                stage,
            },
        );
        if active_prepare.is_none() {
            self.queue_language_server_prepare(key);
        }
    }

    fn queue_language_server_prepare(&mut self, key: DemandKey) {
        let Some(state) = self.language_server_supervisor.demands.get_mut(&key) else {
            return;
        };
        let revision = state.revision;
        let request = state.request.clone();
        state.stage = DemandStage::Preparing {
            active_revision: revision,
        };
        let tx = self.language_server_supervisor.tx.clone();
        let block = self.runtime().block().clone();
        self.work()
            .spawn(async move {
                let result = match request {
                    SupervisorLaunchRequest::Local(request) => match block
                        .spawn(move || helix_lsp::prepare_language_server_launch(request))
                        .await
                    {
                        Ok(Ok(PreparedLanguageServerLaunch::Ready(launch))) => {
                            Ok(PreparedSupervisorLaunch::Ready(Box::new((
                                *launch,
                                LanguageServerLaunchTarget::Local,
                            ))))
                        }
                        Ok(Ok(PreparedLanguageServerLaunch::NoRequiredRoot)) => {
                            Ok(PreparedSupervisorLaunch::NoRequiredRoot)
                        }
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(helix_lsp::Error::Other(anyhow::anyhow!(
                            "language-server preparation task failed: {error}"
                        ))),
                    },
                    SupervisorLaunchRequest::Remote(request) => {
                        prepare_remote_language_server(request).await
                    }
                    SupervisorLaunchRequest::Collaboration(request) => {
                        prepare_collaboration_language_server(request)
                    }
                };
                let _ = tx
                    .send(LanguageServerSupervisorEvent(
                        SupervisorEventKind::Prepared {
                            key,
                            revision,
                            result,
                        },
                    ))
                    .await;
            })
            .detach();
    }

    fn schedule_language_server_retry(&mut self, key: &DemandKey, revision: u64) -> bool {
        if self.language_servers.is_manually_stopped(&key.server) {
            if let Some(state) = self.language_server_supervisor.demands.get_mut(key) {
                state.stage = DemandStage::Failed;
            }
            return false;
        }
        let Some(state) = self.language_server_supervisor.demands.get_mut(key) else {
            return false;
        };
        if state.revision != revision {
            return false;
        }
        state.failure_count = state.failure_count.saturating_add(1);
        if state.failure_count > MAX_RESTART_ATTEMPTS {
            state.stage = DemandStage::Failed;
            log::error!(
                "language server '{}' exhausted {} restart attempts for document {:?}",
                key.server,
                MAX_RESTART_ATTEMPTS,
                key.document
            );
            return false;
        }
        let delay = BASE_RESTART_DELAY.saturating_mul(1_u32 << (state.failure_count - 1));
        state.stage = DemandStage::Retrying;

        let tx = self.language_server_supervisor.tx.clone();
        let key = key.clone();
        self.work()
            .spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = tx
                    .send(LanguageServerSupervisorEvent(SupervisorEventKind::Retry {
                        key,
                        revision,
                    }))
                    .await;
            })
            .detach();
        true
    }

    fn handle_language_server_retry(&mut self, key: DemandKey, revision: u64) {
        if !self
            .language_server_supervisor
            .demands
            .get(&key)
            .is_some_and(|state| {
                state.revision == revision && matches!(state.stage, DemandStage::Retrying)
            })
        {
            return;
        }
        if !self.documents.contains_key(&key.document)
            || self.language_servers.is_manually_stopped(&key.server)
        {
            self.language_server_supervisor.demands.remove(&key);
            return;
        }
        self.queue_language_server_prepare(key);
    }

    fn handle_prepared_language_server(
        &mut self,
        key: DemandKey,
        revision: u64,
        result: helix_lsp::Result<PreparedSupervisorLaunch>,
    ) {
        let Some(state) = self.language_server_supervisor.demands.get(&key) else {
            return;
        };
        if state.revision != revision {
            if matches!(
                state.stage,
                DemandStage::Preparing { active_revision } if active_revision == revision
            ) {
                self.queue_language_server_prepare(key);
            }
            return;
        }

        let prepared = match result {
            Ok(PreparedSupervisorLaunch::Ready(prepared)) => prepared,
            Ok(PreparedSupervisorLaunch::NoRequiredRoot) => {
                if let Some(state) = self.language_server_supervisor.demands.get_mut(&key) {
                    state.stage = DemandStage::Failed;
                }
                return;
            }
            Err(error) => {
                let retryable =
                    matches!(error, helix_lsp::Error::IO(_) | helix_lsp::Error::Other(_));
                let origin = self
                    .language_server_supervisor
                    .demands
                    .get(&key)
                    .map(|state| state.origin)
                    .unwrap_or(LaunchOrigin::Automatic);
                self.report_language_server_launch_error(&key, origin, error);
                if !retryable || !self.schedule_language_server_retry(&key, revision) {
                    if let Some(state) = self.language_server_supervisor.demands.get_mut(&key) {
                        state.stage = DemandStage::Failed;
                    }
                }
                return;
            }
        };

        let (launch, _) = prepared.as_ref();
        if self.language_servers.is_manually_stopped(launch.name()) {
            if let Some(state) = self.language_server_supervisor.demands.get_mut(&key) {
                state.stage = DemandStage::Failed;
            }
            return;
        }

        let instance = InstanceKey {
            server: launch.name().to_owned(),
            root: launch
                .root_uri()
                .map(ToString::to_string)
                .unwrap_or_else(|| launch.root_path().display().to_string()),
        };
        let identity = launch.identity();
        if let Some(client) = self.language_servers.compatible_prepared_client(launch) {
            self.attach_supervised_language_server(&key, revision, client);
            return;
        }

        if let Some(state) = self.language_server_supervisor.demands.get_mut(&key) {
            state.stage = DemandStage::Prepared {
                instance: instance.clone(),
                identity: Box::new(identity),
                launch: Some(prepared),
            };
        }
        self.start_prepared_language_server(instance);
    }

    fn start_prepared_language_server(&mut self, instance: InstanceKey) {
        if self
            .language_server_supervisor
            .instances
            .contains_key(&instance)
        {
            return;
        }
        let selected = self
            .language_server_supervisor
            .demands
            .iter()
            .filter_map(|(key, state)| match &state.stage {
                DemandStage::Prepared {
                    instance: candidate,
                    launch: Some(_),
                    ..
                } if candidate == &instance => Some((state.revision, key.clone())),
                _ => None,
            })
            .max_by_key(|(revision, _)| *revision)
            .map(|(_, key)| key);
        let Some(selected) = selected else {
            return;
        };
        let (identity, launch) = {
            let state = self
                .language_server_supervisor
                .demands
                .get_mut(&selected)
                .expect("selected language-server demand exists");
            let DemandStage::Prepared {
                identity, launch, ..
            } = &mut state.stage
            else {
                return;
            };
            let Some(launch) = launch.take() else {
                return;
            };
            (identity.as_ref().clone(), *launch)
        };

        let id = self.language_servers.reserve_id();
        self.language_server_supervisor.instances.insert(
            instance.clone(),
            SpawnFlight {
                id,
                identity: identity.clone(),
            },
        );
        let tx = self.language_server_supervisor.tx.clone();
        let block = self.runtime().block().clone();
        self.work()
            .spawn(async move {
                let (launch, target) = launch;
                let result = match target {
                    LanguageServerLaunchTarget::Local => match block
                        .spawn(move || helix_lsp::spawn_language_server(id, launch))
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(helix_lsp::Error::Other(anyhow::anyhow!(
                            "language-server spawn task failed: {error}"
                        ))),
                    },
                    LanguageServerLaunchTarget::Remote { backend, cwd } => {
                        spawn_remote_language_server(id, launch, backend, cwd).await
                    }
                    LanguageServerLaunchTarget::Collaboration {
                        session,
                        buffer,
                        server,
                    } => spawn_collaboration_language_server(id, launch, session, buffer, server),
                };
                let _ = tx
                    .send(LanguageServerSupervisorEvent(
                        SupervisorEventKind::Spawned {
                            instance,
                            id,
                            result: Box::new(result),
                        },
                    ))
                    .await;
            })
            .detach();
    }

    fn handle_spawned_language_server(
        &mut self,
        instance: InstanceKey,
        id: helix_lsp::LanguageServerId,
        result: helix_lsp::Result<SpawnedLanguageServer>,
    ) {
        let Some(flight) = self.language_server_supervisor.instances.remove(&instance) else {
            self.language_servers.release_reserved_id(id);
            if let Ok(spawned) = result {
                self.work()
                    .spawn(async move { spawned.force_shutdown().await })
                    .detach();
            }
            return;
        };
        if flight.id != id {
            self.language_servers.release_reserved_id(id);
            if let Ok(spawned) = result {
                self.work()
                    .spawn(async move { spawned.force_shutdown().await })
                    .detach();
            }
            self.start_prepared_language_server(instance);
            return;
        }

        let latest_identity = self
            .language_server_supervisor
            .demands
            .values()
            .filter_map(|state| match &state.stage {
                DemandStage::Prepared {
                    instance: candidate,
                    identity,
                    ..
                } if candidate == &instance => Some((state.revision, identity)),
                _ => None,
            })
            .max_by_key(|(revision, _)| *revision)
            .map(|(_, identity)| identity.as_ref().clone());
        if latest_identity.as_ref() != Some(&flight.identity) {
            self.language_servers.release_reserved_id(id);
            if let Ok(spawned) = result {
                self.work()
                    .spawn(async move { spawned.force_shutdown().await })
                    .detach();
            }
            self.start_prepared_language_server(instance);
            return;
        }

        let client = match result {
            Ok(spawned) => match self.language_servers.install_spawned(spawned) {
                Ok(client) => client,
                Err(error) => {
                    self.language_servers.release_reserved_id(id);
                    log::error!("failed to install spawned language server: {error}");
                    self.fail_prepared_instance(&instance, &flight.identity, error);
                    return;
                }
            },
            Err(error) => {
                self.language_servers.release_reserved_id(id);
                self.fail_prepared_instance(&instance, &flight.identity, error);
                return;
            }
        };

        let demands = self
            .language_server_supervisor
            .demands
            .iter()
            .filter_map(|(key, state)| match &state.stage {
                DemandStage::Prepared {
                    instance: candidate,
                    identity,
                    ..
                } if candidate == &instance && identity.as_ref() == &flight.identity => {
                    Some((key.clone(), state.revision))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for (key, revision) in demands {
            self.attach_supervised_language_server(&key, revision, client.clone());
        }
    }

    fn fail_prepared_instance(
        &mut self,
        instance: &InstanceKey,
        identity: &ClientLaunchIdentity,
        error: helix_lsp::Error,
    ) {
        let error = error.to_string();
        let failed = self
            .language_server_supervisor
            .demands
            .iter()
            .filter_map(|(key, state)| match &state.stage {
                DemandStage::Prepared {
                    instance: candidate,
                    identity: candidate_identity,
                    ..
                } if candidate == instance && candidate_identity.as_ref() == identity => {
                    Some((key.clone(), state.origin))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        log::error!(
            "language server '{}' failed to spawn for {} document(s): {error}",
            instance.server,
            failed.len()
        );
        if failed
            .iter()
            .any(|(_, origin)| *origin == LaunchOrigin::ExplicitRestart)
        {
            self.notify_error(format!(
                "failed to restart language server '{}': {error}",
                instance.server
            ));
        }
        for (key, _) in &failed {
            let revision = self
                .language_server_supervisor
                .demands
                .get(key)
                .map(|state| state.revision)
                .unwrap_or_default();
            if !self.schedule_language_server_retry(key, revision) {
                if let Some(state) = self.language_server_supervisor.demands.get_mut(key) {
                    state.stage = DemandStage::Failed;
                }
            }
        }
    }

    fn attach_supervised_language_server(
        &mut self,
        key: &DemandKey,
        revision: u64,
        client: Arc<Client>,
    ) {
        if self
            .language_server_supervisor
            .demands
            .get(key)
            .is_none_or(|state| state.revision != revision)
        {
            return;
        }
        let initialization_was_dispatched = self
            .language_servers
            .initialization_was_dispatched(client.id());
        let Some(doc) = self.documents.get_mut(&key.document) else {
            self.language_server_supervisor.demands.remove(key);
            return;
        };
        if !doc.language_configuration().is_some_and(|language| {
            language
                .language_servers
                .iter()
                .any(|s| s.name == key.server)
        }) {
            self.language_server_supervisor.demands.remove(key);
            return;
        }

        let previous = doc.insert_language_server(key.server.clone(), client.clone());
        let changed = previous
            .as_ref()
            .is_none_or(|previous| previous.id() != client.id());
        if let Some(previous) = previous.filter(|previous| previous.id() != client.id()) {
            if self
                .language_servers
                .initialization_was_dispatched(previous.id())
            {
                previous.text_document_did_close(doc.identifier());
            }
        }
        if changed && initialization_was_dispatched {
            let language_id = doc.language_id().map(ToOwned::to_owned).unwrap_or_default();
            if let Some(url) = doc.url() {
                client.text_document_did_open(url, doc.version(), doc.text(), language_id);
            }
        }
        if let Some(state) = self.language_server_supervisor.demands.get_mut(key) {
            state.failure_count = 0;
            state.stage = DemandStage::Attached {
                client: client.id(),
            };
        }
        if changed {
            self.dispatch_document_language_servers_change(key.document);
        }
    }

    fn detach_document_language_server(&mut self, document: DocumentId, server: &str) {
        let Some(doc) = self.documents.get_mut(&document) else {
            return;
        };
        let Some(client) = doc.remove_language_server_by_name(server) else {
            return;
        };
        if self
            .language_servers
            .initialization_was_dispatched(client.id())
        {
            client.text_document_did_close(doc.identifier());
        }
        doc.clear_diagnostics_for_language_server(client.id());
        doc.reset_all_inlay_hints();
        doc.mark_inlay_hints_outdated();
        self.dispatch_document_language_servers_change(document);
    }

    fn report_language_server_launch_error(
        &mut self,
        key: &DemandKey,
        origin: LaunchOrigin,
        error: helix_lsp::Error,
    ) {
        if error.is_missing_launch_command() {
            let runtime_generation = error.runtime_generation().unwrap_or_default();
            if origin == LaunchOrigin::Automatic {
                if let Some(language) = self
                    .documents
                    .get(&key.document)
                    .and_then(|doc| doc.language_configuration().cloned())
                {
                    self.handle_missing_language_server(
                        key.document,
                        &language,
                        &key.server,
                        runtime_generation,
                    );
                }
            } else if origin == LaunchOrigin::ExplicitRestart {
                self.notify_error(format!(
                    "language server '{}' is unavailable: {error}",
                    key.server
                ));
            }
            return;
        }

        log::error!(
            "failed to launch language server '{}' for document {:?}: {error}",
            key.server,
            key.document
        );
        if origin == LaunchOrigin::ExplicitRestart {
            self.notify_error(format!(
                "failed to restart language server '{}': {error}",
                key.server
            ));
        }
    }

    pub(super) fn handle_language_server_exit(&mut self, server_id: helix_lsp::LanguageServerId) {
        let attached = self
            .language_server_supervisor
            .demands
            .iter()
            .filter_map(|(key, state)| match state.stage {
                DemandStage::Attached { client } if client == server_id => {
                    Some((key.clone(), state.revision))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let attached_keys = attached
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<HashSet<_>>();

        let mut detached = HashMap::<DocumentId, HashSet<String>>::new();
        for (document_id, document) in &mut self.documents {
            let names = document
                .all_language_servers()
                .filter(|client| client.id() == server_id)
                .map(|client| client.name().to_owned())
                .collect::<Vec<_>>();
            for name in names {
                if let Some(client) = document.remove_language_server_by_name(&name) {
                    document.clear_diagnostics_for_language_server(client.id());
                    document.reset_all_inlay_hints();
                    document.mark_inlay_hints_outdated();
                    detached.entry(*document_id).or_default().insert(name);
                }
            }
        }
        self.language_servers.remove_by_id(server_id);

        for document in detached.keys().copied().collect::<Vec<_>>() {
            self.dispatch_document_language_servers_change(document);
        }
        for (key, revision) in attached {
            if !self.documents.contains_key(&key.document) {
                self.language_server_supervisor.demands.remove(&key);
                continue;
            }
            self.schedule_language_server_retry(&key, revision);
        }

        for (document, names) in detached {
            let drifted = names
                .into_iter()
                .filter(|name| {
                    !attached_keys.contains(&DemandKey {
                        document,
                        server: name.clone(),
                    })
                })
                .collect::<HashSet<_>>();
            if !drifted.is_empty() {
                self.reconcile_language_server_demands(
                    document,
                    LaunchOrigin::Automatic,
                    Some(&drifted),
                );
            }
        }
    }

    pub(super) fn restart_language_server_demands(
        &mut self,
        servers: &HashSet<String>,
        origin: LaunchOrigin,
    ) {
        let servers = servers
            .iter()
            .filter(|server| {
                origin != LaunchOrigin::RuntimeChange
                    || !self.language_servers.is_manually_stopped(server)
            })
            .cloned()
            .collect::<HashSet<_>>();
        for server in &servers {
            if origin == LaunchOrigin::ExplicitRestart {
                self.language_servers.clear_manual_stop(server);
            }
            self.language_servers.invalidate(server);
        }
        let documents = self
            .documents
            .iter()
            .filter_map(|(id, doc)| {
                doc.language_configuration()
                    .is_some_and(|language| {
                        language
                            .language_servers
                            .iter()
                            .any(|features| servers.contains(&features.name))
                    })
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for document in &documents {
            for server in &servers {
                self.detach_document_language_server(*document, server);
            }
        }
        for document in documents {
            self.reconcile_language_server_demands(document, origin, Some(&servers));
        }
    }

    pub(super) fn remove_closed_document_language_server_demands(&mut self, document: DocumentId) {
        let keys = self
            .language_server_supervisor
            .demands
            .keys()
            .filter(|key| key.document == document)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.language_server_supervisor.demands.remove(&key);
        }
    }

    pub(super) fn stop_language_server_demands(&mut self, server: &str) {
        self.language_servers.stop(server);
        let keys = self
            .language_server_supervisor
            .demands
            .keys()
            .filter(|key| key.server == server)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.language_server_supervisor.demands.remove(&key);
            self.detach_document_language_server(key.document, server);
        }
        let documents = self.documents.keys().copied().collect::<Vec<_>>();
        for document in documents {
            self.detach_document_language_server(document, server);
        }
    }
}

fn prepare_collaboration_language_server(
    request: CollaborationLanguageServerLaunchRequest,
) -> helix_lsp::Result<PreparedSupervisorLaunch> {
    let root_uri = helix_lsp::lsp::Url::parse(&format!("dhx-collab://{}/", request.project))
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::new(error)))?;
    let scope: Arc<str> = Arc::from(format!("collaboration://{}", request.project));
    let launch = PreparedClientLaunch::proxy(
        request.name.clone(),
        root_uri,
        request.enable_snippets,
        scope,
        request.timeout,
    );
    Ok(PreparedSupervisorLaunch::Ready(Box::new((
        launch,
        LanguageServerLaunchTarget::Collaboration {
            session: request.session,
            buffer: request.buffer,
            server: request.name,
        },
    ))))
}

fn spawn_collaboration_language_server(
    id: helix_lsp::LanguageServerId,
    launch: PreparedClientLaunch,
    session: helix_collab::GuestSessionHandle,
    buffer: helix_collab::BufferId,
    server: String,
) -> helix_lsp::Result<SpawnedLanguageServer> {
    let state = CollaborationLanguageServerState::new();
    let process = Arc::new(CollaborationLanguageServerProcess(state.clone()));
    let proxy = Arc::new(CollaborationLanguageServerProxy {
        session,
        buffer,
        server,
        state,
    });
    helix_lsp::spawn_language_server_with_proxy(id, launch, process, proxy)
}

async fn prepare_remote_language_server(
    request: RemoteLanguageServerLaunchRequest,
) -> helix_lsp::Result<PreparedSupervisorLaunch> {
    let root_dirs = request
        .language
        .workspace_lsp_roots
        .as_deref()
        .unwrap_or(&request.root_dirs)
        .iter()
        .map(|path| remote_workspace_path(path))
        .collect::<helix_lsp::Result<Vec<_>>>()?;
    let workspace = request
        .backend
        .resolve_language_server_workspace(helix_remote::ResolveLanguageServerWorkspace {
            document: request.document,
            root_markers: request.language.roots.patterns().to_vec(),
            root_dirs,
            required_root_patterns: request
                .server
                .required_root_patterns
                .as_ref()
                .map(|patterns| patterns.patterns().to_vec()),
        })
        .await
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::new(error)))?;
    let Some(workspace) = workspace else {
        return Ok(PreparedSupervisorLaunch::NoRequiredRoot);
    };
    let root_uri = helix_lsp::Url::parse(&workspace.uri)
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::new(error)))?;
    let scope: Arc<str> = Arc::from(format!(
        "ssh://{}#{}",
        request.backend.authority(),
        request.backend.workspace().session.0
    ));
    let command = PreparedExternalLanguageServerCommand {
        scope,
        program: request.server.command,
        args: request.server.args,
        environment: request.server.environment.into_iter().collect(),
    };
    let launch = PreparedClientLaunch::external(
        request.name,
        PathBuf::from(workspace.absolute_path),
        Some(root_uri),
        request.enable_snippets,
        command,
        request.server.config,
        request.server.timeout,
    );
    Ok(PreparedSupervisorLaunch::Ready(Box::new((
        launch,
        LanguageServerLaunchTarget::Remote {
            backend: request.backend,
            cwd: workspace.root,
        },
    ))))
}

fn remote_workspace_path(path: &std::path::Path) -> helix_lsp::Result<helix_remote::WorkspacePath> {
    helix_remote::WorkspacePath::from_native_path(path).map_err(|error| {
        helix_lsp::Error::Other(anyhow::anyhow!(
            "invalid remote language-server root '{}': {error}",
            path.display()
        ))
    })
}

async fn spawn_remote_language_server(
    id: helix_lsp::LanguageServerId,
    launch: PreparedClientLaunch,
    backend: Arc<helix_remote::backend::RemoteWorkspaceClient>,
    cwd: helix_remote::WorkspacePath,
) -> helix_lsp::Result<SpawnedLanguageServer> {
    let command = launch.external_command().cloned().ok_or_else(|| {
        helix_lsp::Error::Other(anyhow::anyhow!(
            "remote language-server launch has no remote command"
        ))
    })?;
    let process = backend
        .start_process(helix_remote::backend::RemoteProcessSpec {
            program: command.program,
            args: command.args,
            cwd,
            env: command.environment,
        })
        .await
        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::new(error)))?;
    let parts = process.into_parts();
    let remote_control = parts.control.clone();
    let control: Arc<dyn helix_lsp::LanguageServerProcess> =
        Arc::new(RemoteLanguageServerProcess(parts.control));
    let result = helix_lsp::spawn_language_server_with_streams(
        id,
        launch,
        control,
        parts.stdout,
        parts.stdin,
        parts.stderr,
    );
    if result.is_err() {
        let _ = remote_control.kill().await;
    }
    result
}
