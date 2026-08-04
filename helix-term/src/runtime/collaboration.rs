use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use helix_collab::{
    Backend, ConnectCode, GuestSession, HostEndpoint, HostHandle, LocalBackend, Project,
    RemoteBackend, Role,
};

pub enum ShareBackend {
    Local(PathBuf),
    Remote(Arc<helix_remote::backend::RemoteWorkspaceClient>),
}

#[derive(Clone)]
pub struct HostedProject {
    project: helix_collab::ProjectId,
    publisher: helix_collab::HostProjectPublisher,
    workspace: HostedWorkspace,
}

#[derive(Clone)]
enum HostedWorkspace {
    Local(PathBuf),
    Remote {
        session: helix_remote::SessionId,
        root: String,
        path_separator: char,
        case_sensitive: bool,
    },
}

impl std::fmt::Debug for HostedProject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedProject")
            .field("remote", &self.is_remote())
            .finish_non_exhaustive()
    }
}

impl HostedProject {
    pub fn is_remote(&self) -> bool {
        matches!(self.workspace, HostedWorkspace::Remote { .. })
    }

    pub fn document_path(
        &self,
        location: &helix_view::file_bound::DocumentLocation,
    ) -> Option<helix_workspace::WorkspacePath> {
        match (&self.workspace, location) {
            (
                HostedWorkspace::Local(root),
                helix_view::file_bound::DocumentLocation::Local(path),
            ) => helix_workspace::relative_workspace_path(root, path).ok(),
            (
                HostedWorkspace::Remote { session, .. },
                helix_view::file_bound::DocumentLocation::Remote(remote),
            ) if remote.session == *session => Some(remote.path.clone()),
            _ => None,
        }
    }

    pub fn workspace_document_path(
        &self,
        path: &helix_workspace::WorkspacePath,
    ) -> helix_view::editor::WorkspaceDocumentPath {
        match &self.workspace {
            HostedWorkspace::Local(root) => {
                helix_view::editor::WorkspaceDocumentPath::Local(root.join(path.to_path_buf()))
            }
            HostedWorkspace::Remote { .. } => {
                helix_view::editor::WorkspaceDocumentPath::Remote(path.clone())
            }
        }
    }

    pub fn collaboration_document_url(&self, path: &helix_workspace::WorkspacePath) -> url::Url {
        helix_collab::uri::document_url(self.project, path)
    }

    pub async fn publish_language_server_diagnostics(
        &self,
        path: helix_workspace::WorkspacePath,
        server: String,
        params: Vec<u8>,
    ) -> Result<(), String> {
        self.publisher
            .publish_language_server_diagnostics(helix_collab::LanguageServerDiagnostics {
                path,
                server,
                params: params.into(),
            })
            .await
            .map_err(|error| error.message)
    }

    pub async fn publish_language_server_refresh(
        &self,
        server: String,
        kind: helix_collab::LanguageServerRefreshKind,
    ) -> Result<(), String> {
        self.publisher
            .publish_language_server_refresh(helix_collab::LanguageServerRefresh { server, kind })
            .await
            .map_err(|error| error.message)
    }

    pub fn rewrite_language_server_request(
        &self,
        value: &mut serde_json::Value,
    ) -> Result<(), String> {
        rewrite_json_strings(value, &mut |string| {
            rewrite_language_server_request_uri(self.project, string, |path| {
                self.host_file_url(path)
            })
        })
    }

    pub fn rewrite_language_server_response(
        &self,
        value: &mut serde_json::Value,
    ) -> Result<(), String> {
        rewrite_json_strings(value, &mut |string| {
            rewrite_language_server_response_uri(self.project, string, |url| {
                self.workspace_path_from_file_url(url)
            })
        })
    }

    fn host_root_url(&self) -> Option<url::Url> {
        match &self.workspace {
            HostedWorkspace::Local(root) => url::Url::from_directory_path(root).ok(),
            HostedWorkspace::Remote {
                root,
                path_separator,
                ..
            } => Some(helix_remote::uri::file_url(
                root,
                &helix_workspace::WorkspacePath::root(),
                *path_separator,
            )),
        }
    }

    fn host_file_url(&self, path: &helix_workspace::WorkspacePath) -> Option<url::Url> {
        if path.is_root() {
            return self.host_root_url();
        }
        match &self.workspace {
            HostedWorkspace::Local(root) => {
                url::Url::from_file_path(root.join(path.to_path_buf())).ok()
            }
            HostedWorkspace::Remote {
                root,
                path_separator,
                ..
            } => Some(helix_remote::uri::file_url(root, path, *path_separator)),
        }
    }

    fn workspace_path_from_file_url(
        &self,
        url: &url::Url,
    ) -> Option<helix_workspace::WorkspacePath> {
        if url.scheme() != "file" {
            return None;
        }
        match &self.workspace {
            HostedWorkspace::Local(root) => {
                let path = url.to_file_path().ok()?;
                helix_workspace::relative_workspace_path(root, &path).ok()
            }
            HostedWorkspace::Remote {
                root,
                path_separator,
                case_sensitive,
                ..
            } => helix_remote::uri::workspace_path_from_file_url(
                url,
                root,
                *path_separator,
                *case_sensitive,
            ),
        }
    }

    pub fn workspace_path_from_language_server_url(
        &self,
        url: &url::Url,
    ) -> Option<helix_workspace::WorkspacePath> {
        self.workspace_path_from_file_url(url)
    }

    pub async fn reserve_local_file_change(
        &self,
        change: &helix_view::editor::FileOperationChange,
    ) -> Result<Option<helix_collab::HostFileMutation>, String> {
        let HostedWorkspace::Local(root) = &self.workspace else {
            return Ok(None);
        };
        let Some(transaction) = local_file_transaction(root, change)? else {
            return Ok(None);
        };
        self.publisher
            .reserve_file_mutation(transaction)
            .await
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

fn local_file_transaction(
    root: &Path,
    change: &helix_view::editor::FileOperationChange,
) -> Result<Option<helix_workspace::FileTransaction>, String> {
    let relative = |path: &Path| match helix_workspace::relative_workspace_path(root, path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == helix_workspace::WorkspaceFsErrorKind::OutsideRoot => {
            Ok(None)
        }
        Err(error) => Err(error.to_string()),
    };
    let operation = match change {
        helix_view::editor::FileOperationChange::Create { path, is_dir } => {
            let Some(path) = relative(path)? else {
                return Ok(None);
            };
            if *is_dir {
                helix_workspace::FileOperation::CreateDirectory { path }
            } else {
                helix_workspace::FileOperation::CreateFile {
                    path,
                    overwrite: false,
                }
            }
        }
        helix_view::editor::FileOperationChange::Delete { path, .. } => {
            let Some(path) = relative(path)? else {
                return Ok(None);
            };
            helix_workspace::FileOperation::Remove {
                path,
                recursive: true,
            }
        }
        helix_view::editor::FileOperationChange::Move { from, to, is_dir } => {
            match (relative(from)?, relative(to)?) {
                (Some(from), Some(to)) => helix_workspace::FileOperation::Rename {
                    from,
                    to,
                    overwrite: false,
                },
                (Some(path), None) => helix_workspace::FileOperation::Remove {
                    path,
                    recursive: true,
                },
                (None, Some(path)) if *is_dir => {
                    helix_workspace::FileOperation::CreateDirectory { path }
                }
                (None, Some(path)) => helix_workspace::FileOperation::CreateFile {
                    path,
                    overwrite: false,
                },
                (None, None) => return Ok(None),
            }
        }
    };
    Ok(Some(helix_workspace::FileTransaction {
        operations: vec![operation],
    }))
}

pub struct Launch {
    pub session: GuestSession,
    pub host: Option<HostHandle>,
    pub invitation: Option<ConnectCode>,
    pub hosted: Option<HostedProject>,
    pub language_servers:
        Option<tokio::sync::mpsc::Receiver<helix_collab::HostLanguageServerRequest>>,
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CollaborationLaunch")
            .field("hosted", &self.host.is_some())
            .field("has_invitation", &self.invitation.is_some())
            .finish_non_exhaustive()
    }
}

pub async fn share(
    backend: ShareBackend,
    advertised: SocketAddr,
    project_name: String,
    owner_name: String,
) -> anyhow::Result<Launch> {
    let (backend, workspace): (Arc<dyn Backend>, HostedWorkspace) = match backend {
        ShareBackend::Local(root) => {
            let backend = LocalBackend::open(root).await?;
            let root = backend.root().to_path_buf();
            (Arc::new(backend), HostedWorkspace::Local(root))
        }
        ShareBackend::Remote(client) => {
            let session = client.workspace().session;
            let root = client.workspace().root.clone();
            let path_separator = client.hello().platform.path_separator;
            let case_sensitive = client.workspace().case_sensitive;
            (
                Arc::new(RemoteBackend::new(client)),
                HostedWorkspace::Remote {
                    session,
                    root,
                    path_separator,
                    case_sensitive,
                },
            )
        }
    };
    let bind = bind_address(advertised);
    let endpoint = Arc::new(HostEndpoint::bind(bind, advertised, owner_name.clone())?);
    let owner = endpoint.owner().await;
    let project = Arc::new(Project::new(project_name, owner, backend)?);
    let project_id = project.id();
    let mut host = HostHandle::start(endpoint, project).await?;
    let language_servers = host.take_language_server_requests();
    let hosted = HostedProject {
        project: project_id,
        publisher: host.project_publisher(),
        workspace,
    };
    let owner_code = host.owner_code().await?;
    let session = GuestSession::join(owner_code, owner_name).await?;
    let invitation = session
        .handle()
        .invite(Role::Write, now_unix_secs().saturating_add(60 * 60))
        .await?;
    Ok(Launch {
        session,
        host: Some(host),
        invitation: Some(invitation),
        hosted: Some(hosted),
        language_servers: Some(language_servers),
    })
}

pub async fn join(code: ConnectCode, participant_name: String) -> anyhow::Result<Launch> {
    Ok(Launch {
        session: GuestSession::join(code, participant_name).await?,
        host: None,
        invitation: None,
        hosted: None,
        language_servers: None,
    })
}

fn rewrite_json_strings(
    value: &mut serde_json::Value,
    rewrite: &mut impl FnMut(&str) -> Result<Option<String>, String>,
) -> Result<(), String> {
    match value {
        serde_json::Value::String(string) => {
            if let Some(replacement) = rewrite(string)? {
                *string = replacement;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rewrite_json_strings(value, rewrite)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                rewrite_json_strings(value, rewrite)?;
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

fn rewrite_language_server_request_uri(
    project: helix_collab::ProjectId,
    string: &str,
    host_file_url: impl FnOnce(&helix_workspace::WorkspacePath) -> Option<url::Url>,
) -> Result<Option<String>, String> {
    let url = match url::Url::parse(string) {
        Ok(url) => url,
        Err(_) if string.starts_with("dhx-collab:") => {
            return Err("language-server request contains an invalid collaboration URI".to_owned());
        }
        Err(_) => return Ok(None),
    };
    match url.scheme() {
        "dhx-collab" => {
            let path = helix_collab::uri::workspace_path(project, string).ok_or_else(|| {
                "language-server request contains a foreign collaboration URI".to_owned()
            })?;
            let url = host_file_url(&path).ok_or_else(|| {
                "language-server request path cannot be represented on the host".to_owned()
            })?;
            Ok(Some(url.to_string()))
        }
        "file" => {
            Err("language-server request may not address a host file URI directly".to_owned())
        }
        _ => Ok(None),
    }
}

fn rewrite_language_server_response_uri(
    project: helix_collab::ProjectId,
    string: &str,
    workspace_path: impl FnOnce(&url::Url) -> Option<helix_workspace::WorkspacePath>,
) -> Result<Option<String>, String> {
    let Ok(url) = url::Url::parse(string) else {
        return Ok(None);
    };
    if url.scheme() != "file" {
        return Ok(None);
    }
    let path = workspace_path(&url).ok_or_else(|| {
        "language-server response contains a file URI outside the shared workspace".to_owned()
    })?;
    Ok(Some(
        helix_collab::uri::document_url(project, &path).to_string(),
    ))
}

pub fn participant_name() -> String {
    ["USER", "USERNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control))
        .unwrap_or_else(|| String::from("participant"))
}

fn bind_address(advertised: SocketAddr) -> SocketAddr {
    let ip = match advertised.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => IpAddr::V4(ip),
        IpAddr::V6(ip) if ip.is_loopback() => IpAddr::V6(ip),
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    SocketAddr::new(ip, advertised.port())
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(
        root: &Path,
        change: helix_view::editor::FileOperationChange,
    ) -> helix_workspace::FileOperation {
        local_file_transaction(root, &change)
            .unwrap()
            .unwrap()
            .operations
            .into_iter()
            .next()
            .unwrap()
    }

    fn workspace_path(path: &str) -> helix_workspace::WorkspacePath {
        helix_workspace::WorkspacePath::new([path.to_owned()]).unwrap()
    }

    #[test]
    fn public_advertised_addresses_bind_on_the_matching_unspecified_family() {
        assert_eq!(
            bind_address("192.0.2.1:7777".parse().unwrap()),
            "0.0.0.0:7777".parse().unwrap()
        );
        assert_eq!(
            bind_address("[2001:db8::1]:7777".parse().unwrap()),
            "[::]:7777".parse().unwrap()
        );
        assert_eq!(
            bind_address("127.0.0.1:0".parse().unwrap()),
            "127.0.0.1:0".parse().unwrap()
        );
    }

    #[test]
    fn local_moves_preserve_shared_root_semantics() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        assert_eq!(
            operation(
                project.path(),
                helix_view::editor::FileOperationChange::Move {
                    from: project.path().join("old.rs"),
                    to: project.path().join("new.rs"),
                    is_dir: false,
                },
            ),
            helix_workspace::FileOperation::Rename {
                from: workspace_path("old.rs"),
                to: workspace_path("new.rs"),
                overwrite: false,
            }
        );
        assert_eq!(
            operation(
                project.path(),
                helix_view::editor::FileOperationChange::Move {
                    from: project.path().join("removed.rs"),
                    to: outside.path().join("removed.rs"),
                    is_dir: false,
                },
            ),
            helix_workspace::FileOperation::Remove {
                path: workspace_path("removed.rs"),
                recursive: true,
            }
        );
        assert_eq!(
            operation(
                project.path(),
                helix_view::editor::FileOperationChange::Move {
                    from: outside.path().join("added.rs"),
                    to: project.path().join("added.rs"),
                    is_dir: false,
                },
            ),
            helix_workspace::FileOperation::CreateFile {
                path: workspace_path("added.rs"),
                overwrite: false,
            }
        );
    }

    #[test]
    fn file_changes_outside_shared_root_are_not_published() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let change = helix_view::editor::FileOperationChange::Create {
            path: outside.path().join("ignored.rs"),
            is_dir: false,
        };

        assert!(local_file_transaction(project.path(), &change)
            .unwrap()
            .is_none());
    }

    #[test]
    fn language_server_uri_rewrites_are_workspace_scoped() {
        let project = helix_collab::ProjectId::from_bytes([1; 16]);
        let other = helix_collab::ProjectId::from_bytes([2; 16]);
        let path = helix_workspace::WorkspacePath::from_slash_path("src/main.rs").unwrap();
        let collaboration_uri = helix_collab::uri::document_url(project, &path);
        let host_uri = url::Url::parse("file:///workspace/src/main.rs").unwrap();

        assert_eq!(
            rewrite_language_server_request_uri(project, collaboration_uri.as_str(), |_| Some(
                host_uri.clone()
            ),)
            .unwrap(),
            Some(host_uri.to_string())
        );
        assert!(rewrite_language_server_request_uri(
            project,
            "file:///workspace/src/main.rs",
            |_| Some(host_uri.clone()),
        )
        .is_err());
        assert!(rewrite_language_server_request_uri(
            project,
            helix_collab::uri::document_url(other, &path).as_str(),
            |_| Some(host_uri.clone()),
        )
        .is_err());

        assert_eq!(
            rewrite_language_server_response_uri(project, host_uri.as_str(), |_| {
                Some(path.clone())
            })
            .unwrap(),
            Some(collaboration_uri.to_string())
        );
        assert!(
            rewrite_language_server_response_uri(project, "file:///outside/secret", |_| None)
                .is_err()
        );
        assert_eq!(
            rewrite_language_server_response_uri(project, "rust-analyzer://crate/1", |_| None)
                .unwrap(),
            None
        );
    }
}
