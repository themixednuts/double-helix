use crate::ProjectId;
use helix_workspace::WorkspacePath;

pub fn document_url(project: ProjectId, path: &WorkspacePath) -> url::Url {
    let mut url = url::Url::parse(&format!("dhx-collab://{project}/"))
        .expect("collaboration project IDs always form a valid authority");
    if !path.is_root() {
        url.set_path(&format!("/{path}"));
    }
    url
}

pub fn workspace_path(project: ProjectId, uri: &str) -> Option<WorkspacePath> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "dhx-collab"
        || url.host_str()? != project.to_string()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    let path = path.strip_prefix('/')?;
    if path.starts_with('/') {
        return None;
    }
    if path.is_empty() {
        Some(WorkspacePath::root())
    } else {
        WorkspacePath::from_slash_path(path).ok()
    }
    .filter(|path| document_url(project, path).as_str() == uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_project_scoped_and_traversal_safe() {
        let project = ProjectId::from_bytes([1; 16]);
        let other = ProjectId::from_bytes([2; 16]);
        let path = WorkspacePath::from_slash_path("src/file name.rs").unwrap();
        let url = document_url(project, &path);

        assert_eq!(workspace_path(project, url.as_str()), Some(path));
        assert_eq!(workspace_path(other, url.as_str()), None);

        let traversal = format!("dhx-collab://{project}/%2e%2e/secret");
        assert_eq!(workspace_path(project, &traversal), None);

        let ambiguous = format!("dhx-collab://{project}//src/main.rs");
        assert_eq!(workspace_path(project, &ambiguous), None);

        let encoded_delimiter = format!("dhx-collab://{project}/src%2fmain.rs");
        assert_eq!(workspace_path(project, &encoded_delimiter), None);
    }
}
