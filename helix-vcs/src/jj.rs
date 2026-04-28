use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, bail, Context as _, Result};

use crate::FileChange;

pub fn for_each_changed_file(
    cwd: &Path,
    mut f: impl FnMut(Result<FileChange>) -> bool,
) -> Result<()> {
    let root = repo_root(cwd)?;
    let output = Command::new("jj")
        .args(["--color", "never", "--no-pager", "diff", "--summary"])
        .current_dir(&root)
        .output()
        .context("failed to run `jj diff --summary`")?;

    if !output.status.success() {
        bail!("{}", command_error("jj diff --summary", &output.stderr));
    }

    let stdout = String::from_utf8(output.stdout).context("jj diff output was not utf-8")?;
    for line in stdout.lines() {
        let Some(change) = parse_diff_summary_line(&root, line) else {
            continue;
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

fn repo_root(cwd: &Path) -> Result<PathBuf> {
    if !has_jj_marker(cwd) {
        bail!("not a jj repository");
    }

    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "--color",
            "never",
            "--no-pager",
            "root",
        ])
        .current_dir(cwd)
        .output()
        .context("failed to run `jj root`")?;

    if !output.status.success() {
        bail!("{}", command_error("jj root", &output.stderr));
    }

    let stdout = String::from_utf8(output.stdout).context("jj root output was not utf-8")?;
    let root = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .ok_or_else(|| anyhow!("jj root returned no repository path"))?;

    Ok(helix_stdx::path::normalize(root))
}

fn has_jj_marker(cwd: &Path) -> bool {
    let mut cursor = if cwd.is_file() {
        cwd.parent()
    } else {
        Some(cwd)
    };

    while let Some(path) = cursor {
        if path.join(".jj").is_dir() {
            return true;
        }
        cursor = path.parent();
    }

    false
}

fn command_error(command: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("`{command}` failed")
    } else {
        format!("`{command}` failed: {stderr}")
    }
}

fn parse_diff_summary_line(root: &Path, line: &str) -> Option<FileChange> {
    let line = line.trim();
    let (status, path) = line.split_once(' ')?;
    let status = status.chars().next()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let path = helix_stdx::path::normalize(root.join(path.trim_matches('"')));

    match status {
        'A' => Some(FileChange::Untracked { path }),
        'M' => Some(FileChange::Modified { path }),
        'D' => Some(FileChange::Deleted { path }),
        'C' => Some(FileChange::Conflict { path }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jj_diff_summary_lines() {
        let root = Path::new("/repo");

        assert_eq!(
            parse_diff_summary_line(root, "M src/main.rs"),
            Some(FileChange::Modified {
                path: helix_stdx::path::normalize("/repo/src/main.rs"),
            })
        );
        assert_eq!(
            parse_diff_summary_line(root, "A new file.txt"),
            Some(FileChange::Untracked {
                path: helix_stdx::path::normalize("/repo/new file.txt"),
            })
        );
        assert_eq!(
            parse_diff_summary_line(root, "D old.txt"),
            Some(FileChange::Deleted {
                path: helix_stdx::path::normalize("/repo/old.txt"),
            })
        );
        assert_eq!(parse_diff_summary_line(root, "No changes."), None);
    }
}
