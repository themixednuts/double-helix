use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, path::PathBuf};

pub const MAX_WORKSPACE_PATH_BYTES: usize = 16 * 1024;
pub const MAX_WORKSPACE_PATH_SEGMENTS: usize = 256;
pub const MAX_WORKSPACE_PATH_SEGMENT_BYTES: usize = 4 * 1024;

/// A validated UTF-8 path relative to a workspace authority.
///
/// Segments are serialized independently so platform separators cannot change
/// the path's meaning at a transport boundary.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WorkspacePath(Box<[String]>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspacePathError {
    #[error("workspace path must be relative")]
    Absolute,
    #[error("workspace path contains an empty segment")]
    EmptySegment,
    #[error("workspace path may not contain '.' or '..'")]
    Traversal,
    #[error("workspace path contains a NUL byte")]
    Nul,
    #[error("workspace path segment contains the wire path separator")]
    PathSeparator,
    #[error("workspace path has too many segments")]
    TooManySegments,
    #[error("workspace path segment is too long")]
    SegmentTooLong,
    #[error("workspace path is too long")]
    PathTooLong,
}

impl WorkspacePath {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn new(segments: impl IntoIterator<Item = String>) -> Result<Self, WorkspacePathError> {
        let segments = segments.into_iter().collect::<Box<[_]>>();
        validate(&segments)?;
        Ok(Self(segments))
    }

    pub fn from_slash_path(path: &str) -> Result<Self, WorkspacePathError> {
        if path.is_empty() {
            return Ok(Self::root());
        }
        if path.starts_with('/') || path.starts_with('\\') || has_windows_prefix(path) {
            return Err(WorkspacePathError::Absolute);
        }
        Self::new(path.split('/').map(str::to_owned))
    }

    /// Resolve user-entered relative path syntax without allowing it to escape
    /// this path. Both separators are accepted at the UI boundary; stored
    /// workspace paths remain platform-independent segments.
    pub fn resolve_relative(&self, path: &str) -> Result<Self, WorkspacePathError> {
        if path.starts_with('/') || path.starts_with('\\') || has_windows_prefix(path) {
            return Err(WorkspacePathError::Absolute);
        }

        let root_depth = self.0.len();
        let mut segments = self.0.to_vec();
        for segment in path.split(['/', '\\']) {
            match segment {
                "" if path.is_empty() => {}
                "" => return Err(WorkspacePathError::EmptySegment),
                "." => {}
                ".." if segments.len() > root_depth => {
                    segments.pop();
                }
                ".." => return Err(WorkspacePathError::Traversal),
                segment => segments.push(segment.to_owned()),
            }
        }
        Self::new(segments)
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&self, name: impl Into<String>) -> Result<Self, WorkspacePathError> {
        let mut segments = self.0.to_vec();
        segments.push(name.into());
        Self::new(segments)
    }

    pub fn join_path(&self, path: &Self) -> Result<Self, WorkspacePathError> {
        let mut segments = Vec::with_capacity(self.0.len() + path.0.len());
        segments.extend(self.0.iter().cloned());
        segments.extend(path.0.iter().cloned());
        Self::new(segments)
    }

    pub fn parent(&self) -> Option<Self> {
        (!self.0.is_empty()).then(|| Self(self.0[..self.0.len() - 1].into()))
    }

    pub fn file_name(&self) -> Option<&str> {
        self.0.last().map(String::as_str)
    }

    pub fn starts_with(&self, base: &Self) -> bool {
        self.0.starts_with(&base.0)
    }

    pub fn strip_prefix(&self, base: &Self) -> Option<Self> {
        self.starts_with(base)
            .then(|| Self(self.0[base.0.len()..].into()))
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.iter().collect()
    }

    /// Convert this protocol path to a path on the current filesystem authority.
    ///
    /// A backslash is valid filename data on Unix but a separator on Windows.
    /// Keep it valid in the authority-neutral wire type and reject it only when
    /// a Windows authority would otherwise reinterpret the segment.
    pub fn to_native_path_buf(&self) -> Result<PathBuf, WorkspacePathError> {
        #[cfg(windows)]
        if self.0.iter().any(|segment| segment.contains('\\')) {
            return Err(WorkspacePathError::PathSeparator);
        }

        Ok(self.to_path_buf())
    }
}

fn validate(segments: &[String]) -> Result<(), WorkspacePathError> {
    if segments.len() > MAX_WORKSPACE_PATH_SEGMENTS {
        return Err(WorkspacePathError::TooManySegments);
    }
    let mut path_bytes = segments.len().saturating_sub(1);
    for segment in segments {
        if segment.is_empty() {
            return Err(WorkspacePathError::EmptySegment);
        }
        if matches!(segment.as_str(), "." | "..") {
            return Err(WorkspacePathError::Traversal);
        }
        if segment.contains('\0') {
            return Err(WorkspacePathError::Nul);
        }
        if segment.contains('/') {
            return Err(WorkspacePathError::PathSeparator);
        }
        if segment.len() > MAX_WORKSPACE_PATH_SEGMENT_BYTES {
            return Err(WorkspacePathError::SegmentTooLong);
        }
        path_bytes = path_bytes
            .checked_add(segment.len())
            .ok_or(WorkspacePathError::PathTooLong)?;
        if path_bytes > MAX_WORKSPACE_PATH_BYTES {
            return Err(WorkspacePathError::PathTooLong);
        }
    }
    Ok(())
}

impl<'de> Deserialize<'de> for WorkspacePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let segments = Vec::<String>::deserialize(deserializer)?;
        Self::new(segments).map_err(serde::de::Error::custom)
    }
}

fn has_windows_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

impl fmt::Debug for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkspacePath")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str("/")?;
            }
            formatter.write_str(segment)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_workspace_relative_paths() {
        assert_eq!(
            WorkspacePath::from_slash_path("").unwrap(),
            WorkspacePath::root()
        );
        assert_eq!(
            WorkspacePath::from_slash_path("src/main.rs")
                .unwrap()
                .segments(),
            &["src", "main.rs"]
        );
        assert_eq!(
            WorkspacePath::from_slash_path("../secret"),
            Err(WorkspacePathError::Traversal)
        );
        assert_eq!(
            WorkspacePath::from_slash_path("C:/secret"),
            Err(WorkspacePathError::Absolute)
        );
        assert_eq!(
            WorkspacePath::new([String::from("nested/name")]),
            Err(WorkspacePathError::PathSeparator)
        );
        assert_eq!(
            WorkspacePath::new([String::from(r"nested\name")])
                .unwrap()
                .segments(),
            &[r"nested\name"]
        );
        assert_eq!(
            WorkspacePath::new(["x".repeat(MAX_WORKSPACE_PATH_SEGMENT_BYTES + 1)]),
            Err(WorkspacePathError::SegmentTooLong)
        );
    }

    #[test]
    fn resolves_user_paths_within_the_authority_root() {
        let root = WorkspacePath::from_slash_path("project").unwrap();

        assert_eq!(
            root.resolve_relative(r"src\main.rs").unwrap().to_string(),
            "project/src/main.rs"
        );
        assert_eq!(
            root.resolve_relative("src/../README.md")
                .unwrap()
                .to_string(),
            "project/README.md"
        );
        assert_eq!(
            root.resolve_relative("../secret"),
            Err(WorkspacePathError::Traversal)
        );
        assert_eq!(
            root.resolve_relative("/etc/passwd"),
            Err(WorkspacePathError::Absolute)
        );
    }

    #[test]
    fn rejects_invalid_deserialized_paths() {
        let bytes = rmp_serde::to_vec(&vec![String::from("..")]).unwrap();
        assert!(rmp_serde::from_slice::<WorkspacePath>(&bytes).is_err());
    }

    #[test]
    fn deserializes_unix_backslashes_without_reinterpreting_segments() {
        let bytes = rmp_serde::to_vec(&vec![String::from(r"directory\name")]).unwrap();
        let path = rmp_serde::from_slice::<WorkspacePath>(&bytes).unwrap();

        assert_eq!(path.segments(), &[r"directory\name"]);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_unix_backslashes_at_a_windows_filesystem_boundary() {
        let path = WorkspacePath::new([String::from(r"directory\name")]).unwrap();

        assert_eq!(
            path.to_native_path_buf(),
            Err(WorkspacePathError::PathSeparator)
        );
    }
}
