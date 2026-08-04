use crate::{
    protocol::{
        ErrorCode, LanguageServerWorkspace, RemoteError, ResolveLanguageServerWorkspace,
        MAX_LANGUAGE_SERVER_ROOT_PATTERNS, MAX_LANGUAGE_SERVER_ROOT_PATTERN_BYTES,
    },
    workspace::{external_absolute_path, relative_path, Workspace},
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{path::Path, sync::Arc};

pub(crate) async fn resolve_workspace(
    workspace: Arc<Workspace>,
    request: ResolveLanguageServerWorkspace,
) -> Result<Option<LanguageServerWorkspace>, RemoteError> {
    validate_patterns(&request)?;
    let document = workspace.resolve_existing(&request.document).await?;
    let root = workspace.root().to_path_buf();
    tokio::task::spawn_blocking(move || resolve_workspace_blocking(&root, &document, request))
        .await
        .map_err(|error| {
            RemoteError::new(
                ErrorCode::Internal,
                format!("language-server root worker failed: {error}"),
            )
        })?
}

fn resolve_workspace_blocking(
    workspace: &Path,
    document: &Path,
    request: ResolveLanguageServerWorkspace,
) -> Result<Option<LanguageServerWorkspace>, RemoteError> {
    let root_markers = build_globset(&request.root_markers)?;
    let required_root_patterns = request
        .required_root_patterns
        .as_deref()
        .map(build_globset)
        .transpose()?;
    let stop_roots = request
        .root_dirs
        .iter()
        .map(|root| workspace.join(root.to_path_buf()))
        .collect::<Vec<_>>();
    let start = document.parent().unwrap_or(workspace);
    let mut top_marker = None;
    let mut selected = None;

    for ancestor in start.ancestors() {
        if !ancestor.starts_with(workspace) {
            break;
        }
        if directory_matches(ancestor, &root_markers)? {
            top_marker = Some(ancestor.to_path_buf());
        }
        if stop_roots.iter().any(|root| root == ancestor) || ancestor == workspace {
            selected = Some(top_marker.unwrap_or_else(|| workspace.to_path_buf()));
            break;
        }
    }

    let Some(selected) = selected else {
        return Err(RemoteError::new(
            ErrorCode::InvalidPath,
            "language-server document is outside the remote workspace",
        ));
    };
    if let Some(patterns) = &required_root_patterns {
        if !directory_matches(&selected, patterns)? {
            return Ok(None);
        }
    }
    let root = relative_path(workspace, &selected)?;
    let absolute_path = external_absolute_path(&selected);
    let uri = url::Url::from_directory_path(Path::new(&absolute_path))
        .map_err(|_| {
            RemoteError::new(
                ErrorCode::InvalidPath,
                "language-server root cannot be represented as a file URI",
            )
        })?
        .to_string();
    Ok(Some(LanguageServerWorkspace {
        root,
        absolute_path,
        uri,
    }))
}

fn validate_patterns(request: &ResolveLanguageServerWorkspace) -> Result<(), RemoteError> {
    let patterns =
        request.root_markers.len() + request.required_root_patterns.as_ref().map_or(0, Vec::len);
    if patterns > MAX_LANGUAGE_SERVER_ROOT_PATTERNS {
        return Err(RemoteError::new(
            ErrorCode::InvalidRequest,
            "too many language-server root patterns",
        ));
    }
    let bytes = request
        .root_markers
        .iter()
        .chain(request.required_root_patterns.iter().flatten())
        .try_fold(0usize, |bytes, pattern| bytes.checked_add(pattern.len()))
        .ok_or_else(patterns_too_large)?;
    if bytes > MAX_LANGUAGE_SERVER_ROOT_PATTERN_BYTES {
        return Err(patterns_too_large());
    }
    Ok(())
}

fn patterns_too_large() -> RemoteError {
    RemoteError::new(
        ErrorCode::InvalidRequest,
        "language-server root patterns are too large",
    )
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, RemoteError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|_| {
            RemoteError::new(
                ErrorCode::InvalidRequest,
                "language-server root pattern is invalid",
            )
        })?);
    }
    builder.build().map_err(|_| {
        RemoteError::new(
            ErrorCode::InvalidRequest,
            "language-server root patterns are invalid",
        )
    })
}

fn directory_matches(directory: &Path, patterns: &GlobSet) -> Result<bool, RemoteError> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        RemoteError::new(
            ErrorCode::Io,
            format!("failed to inspect language-server root: {error}"),
        )
    })?;
    for entry in entries.flatten() {
        if patterns.is_match(entry.file_name()) {
            return Ok(true);
        }
    }
    Ok(false)
}
