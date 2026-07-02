use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Write,
    Review,
}

impl Mode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Review => "review",
        }
    }

    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Write => Self::Review,
            Self::Review => Self::Write,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    File(PathBuf),
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pending,
    Accepted,
    Rejected,
    Written,
}

impl Status {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Written => "written",
        }
    }

    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub diff: String,
    pub status: Status,
}

impl File {
    #[must_use]
    pub fn staged(path: PathBuf, before: String, after: String) -> Self {
        let diff = unified_diff(&path, &before, &after);
        Self {
            path,
            before,
            after,
            diff,
            status: Status::Pending,
        }
    }

    #[must_use]
    pub fn written(path: PathBuf, before: String, after: String) -> Self {
        let mut file = Self::staged(path, before, after);
        file.status = Status::Written;
        file
    }

    pub fn resolve(&mut self, decision: Decision) {
        self.status = match decision {
            Decision::Accept => Status::Accepted,
            Decision::Reject => Status::Rejected,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub mode: Mode,
    pub files: Vec<File>,
}

impl Summary {
    #[must_use]
    pub fn pending_files(&self) -> impl Iterator<Item = &File> {
        self.files.iter().filter(|file| file.status.is_pending())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stage { file: File, mode: Mode },
    Resolve { target: Target, decision: Decision },
    Mode(Mode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SetMode(Mode),
    Resolve { target: Target, decision: Decision },
}

#[must_use]
pub fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    if before == after {
        return format!("--- {}\n+++ {}\n", path.display(), path.display());
    }

    let before_lines = split_lines(before);
    let after_lines = split_lines(after);

    let mut prefix = 0;
    while prefix < before_lines.len()
        && prefix < after_lines.len()
        && before_lines[prefix] == after_lines[prefix]
    {
        prefix += 1;
    }

    let mut before_suffix = before_lines.len();
    let mut after_suffix = after_lines.len();
    while before_suffix > prefix
        && after_suffix > prefix
        && before_lines[before_suffix - 1] == after_lines[after_suffix - 1]
    {
        before_suffix -= 1;
        after_suffix -= 1;
    }

    let context = 3;
    let before_start = prefix.saturating_sub(context);
    let after_start = prefix.saturating_sub(context);
    let before_end = (before_suffix + context).min(before_lines.len());
    let after_end = (after_suffix + context).min(after_lines.len());

    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "--- {}", path.display());
    let _ = writeln!(out, "+++ {}", path.display());
    let _ = writeln!(
        out,
        "@@ -{},{} +{},{} @@",
        before_start + 1,
        before_end.saturating_sub(before_start),
        after_start + 1,
        after_end.saturating_sub(after_start)
    );

    for line in &before_lines[before_start..prefix] {
        let _ = writeln!(out, " {line}");
    }
    for line in &before_lines[prefix..before_suffix] {
        let _ = writeln!(out, "-{line}");
    }
    for line in &after_lines[prefix..after_suffix] {
        let _ = writeln!(out, "+{line}");
    }
    for line in &after_lines[after_suffix..after_end] {
        let _ = writeln!(out, " {line}");
    }

    out
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.lines().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_marks_added_and_removed_lines() {
        let diff = unified_diff(
            Path::new("src/main.rs"),
            "fn main() {\n    old();\n}\n",
            "fn main() {\n    new();\n}\n",
        );

        assert!(diff.contains("--- src/main.rs"));
        assert!(diff.contains("@@ -1,3 +1,3 @@"));
        assert!(diff.contains("-    old();"));
        assert!(diff.contains("+    new();"));
    }

    #[test]
    fn file_resolve_updates_status() {
        let mut file = File::staged(PathBuf::from("a.txt"), "a".into(), "b".into());

        file.resolve(Decision::Reject);

        assert_eq!(file.status, Status::Rejected);
    }
}
