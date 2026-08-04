use crate::WorkspacePath;

pub fn absolute_path(root: &str, path: &WorkspacePath, separator: char) -> String {
    let mut absolute = root.trim_end_matches(['/', '\\']).to_owned();
    if (absolute.is_empty() && root.starts_with(['/', '\\']))
        || (path.is_root()
            && absolute.as_bytes().get(1) == Some(&b':')
            && root.ends_with(['/', '\\']))
    {
        absolute.push(separator);
    }
    for segment in path.segments() {
        if !absolute.ends_with(['/', '\\']) {
            absolute.push(separator);
        }
        absolute.push_str(segment);
    }
    absolute
}

pub fn file_url(root: &str, path: &WorkspacePath, separator: char) -> url::Url {
    let normalized = url_path(root, path, separator);
    let mut url = url::Url::parse("file:///").expect("static file URL is valid");
    url.set_path(&normalized);
    url
}

pub fn url_path(root: &str, path: &WorkspacePath, separator: char) -> String {
    normalize_url_path(&absolute_path(root, path, separator), separator)
}

pub fn workspace_path_from_file_url(
    url: &url::Url,
    root: &str,
    separator: char,
    case_sensitive: bool,
) -> Option<WorkspacePath> {
    if url.scheme() != "file" || url.host().is_some() {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    let absolute = normalize_url_path(&decoded, separator);
    let root = normalize_url_path(root, separator);
    let relative = strip_root(&absolute, &root, case_sensitive)?;
    if relative.is_empty() {
        Some(WorkspacePath::root())
    } else {
        WorkspacePath::from_slash_path(relative).ok()
    }
}

pub fn workspace_path_from_absolute_path(
    path: &str,
    root: &str,
    case_sensitive: bool,
) -> Option<WorkspacePath> {
    let path = path.replace('\\', "/");
    let root = root.replace('\\', "/");
    let absolute = normalize_url_path(&path, '/');
    let root = normalize_url_path(&root, '/');
    let relative = strip_root(&absolute, &root, case_sensitive)?;
    if relative.is_empty() {
        Some(WorkspacePath::root())
    } else {
        WorkspacePath::from_slash_path(relative).ok()
    }
}

fn normalize_url_path(path: &str, separator: char) -> String {
    let normalized = if separator == '\\' {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    };
    let drive_root = normalized.len() == 3
        && normalized.as_bytes().get(1) == Some(&b':')
        && normalized.ends_with('/');
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() {
        "/".to_owned()
    } else if normalized.as_bytes().get(1) == Some(&b':') {
        format!("/{normalized}{}", if drive_root { "/" } else { "" })
    } else {
        normalized.to_owned()
    }
}

fn strip_root<'a>(absolute: &'a str, root: &str, case_sensitive: bool) -> Option<&'a str> {
    if absolute.len() < root.len() {
        return None;
    }
    let prefix = absolute.get(..root.len())?;
    let equal = if case_sensitive {
        prefix == root
    } else {
        prefix.eq_ignore_ascii_case(root)
    };
    if !equal
        || (absolute.len() != root.len() && absolute.as_bytes().get(root.len()) != Some(&b'/'))
    {
        return None;
    }
    Some(absolute.get(root.len()..)?.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> WorkspacePath {
        WorkspacePath::from_slash_path(path).unwrap()
    }

    #[test]
    fn file_urls_preserve_remote_roots_and_encoding() {
        assert_eq!(
            file_url("/", &path("src/main.rs"), '/').as_str(),
            "file:///src/main.rs"
        );
        assert_eq!(
            file_url(r"C:\", &WorkspacePath::root(), '\\').path(),
            "/C:/"
        );
        assert_eq!(
            file_url("/srv/work tree", &path("src/file name.rs"), '/').as_str(),
            "file:///srv/work%20tree/src/file%20name.rs"
        );
    }

    #[test]
    fn reverse_mapping_enforces_root_and_case_boundaries() {
        let url = url::Url::parse("file:///repo/src/main.rs").unwrap();
        assert_eq!(
            workspace_path_from_file_url(&url, "/repo", '/', true),
            Some(path("src/main.rs"))
        );
        assert_eq!(
            workspace_path_from_file_url(
                &url::Url::parse("file:///repository/main.rs").unwrap(),
                "/repo",
                '/',
                true,
            ),
            None
        );
        assert_eq!(
            workspace_path_from_file_url(
                &url::Url::parse("file:///Repo/src/main.rs").unwrap(),
                "/repo",
                '/',
                false,
            ),
            Some(path("src/main.rs"))
        );
    }

    #[test]
    fn reverse_mapping_is_panic_free_at_unicode_boundaries() {
        assert_eq!(
            workspace_path_from_file_url(
                &url::Url::parse("file:///%C3%A9/src").unwrap(),
                "/a",
                '/',
                true,
            ),
            None
        );
    }

    #[test]
    fn reverse_mapping_accepts_foreign_platform_absolute_paths() {
        assert_eq!(
            workspace_path_from_absolute_path(r"\home\jonfo\src\main.rs", "/home/jonfo", true,),
            Some(path("src/main.rs"))
        );
        assert_eq!(
            workspace_path_from_absolute_path(r"C:\repo\src\main.rs", r"C:\repo", false,),
            Some(path("src/main.rs"))
        );
        assert!(
            workspace_path_from_absolute_path("/home/other/file", "/home/jonfo", true).is_none()
        );
    }
}
