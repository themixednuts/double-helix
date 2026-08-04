pub(crate) use helix_view::editor::{
    WorkspaceContext as ExplorerSource, WorkspaceDocumentPath as ExplorerPath,
    WorkspaceId as ExplorerSourceId,
};

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::editor::WorkspaceBackend;

    #[test]
    fn local_backend_rejects_non_local_workspace_paths() {
        let root = ExplorerPath::Remote(helix_remote::WorkspacePath::root());

        assert!(ExplorerSource::from_root(root, &WorkspaceBackend::Local).is_err());
    }
}
