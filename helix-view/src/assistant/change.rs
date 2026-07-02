use crate::collab::{location, Location};

use super::review;

slotmap::new_key_type! {
    pub struct Id;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub range: Option<Location>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: std::path::PathBuf,
    pub hunks: Vec<Hunk>,
    pub review: Option<review::File>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub files: Vec<File>,
}

impl Hunk {
    #[must_use]
    pub fn location(&self) -> Option<Location> {
        self.range.clone()
    }
}

impl File {
    #[must_use]
    pub fn locations(&self) -> Vec<Location> {
        let mut locations: Vec<_> = self.hunks.iter().filter_map(Hunk::location).collect();
        if locations.is_empty() {
            locations.push(Location::new(self.path.clone(), location::Source::Change));
        }
        locations
    }
}

impl Summary {
    #[must_use]
    pub fn locations(&self) -> Vec<Location> {
        self.files.iter().flat_map(File::locations).collect()
    }

    #[must_use]
    pub fn to_markdown(&self, title: &str) -> String {
        let mut body = format!("# {title}\n\n");
        for file in &self.files {
            use std::fmt::Write as _;

            let _ = writeln!(body, "## {}\n", file.path.display());
            if file.hunks.is_empty() {
                body.push_str("- changed\n\n");
                continue;
            }
            for hunk in &file.hunks {
                let _ = writeln!(body, "- {}", hunk.summary);
            }
            body.push('\n');
        }
        body
    }
}
