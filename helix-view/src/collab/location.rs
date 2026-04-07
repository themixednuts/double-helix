//! Locations for follow / presence (`docs/runtime-collaboration-implementation-plan.md` Phase 5).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Read,
    Write,
    Tool,
    Change,
    Cursor,
}

impl Default for Source {
    fn default() -> Self {
        Self::Tool
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeAnchor {
    pub anchor: usize,
    pub head: usize,
}

impl RangeAnchor {
    #[must_use]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    #[must_use]
    pub fn to_range(self) -> helix_core::Range {
        helix_core::Range::new(self.anchor, self.head)
    }
}

impl From<helix_core::Range> for RangeAnchor {
    fn from(range: helix_core::Range) -> Self {
        Self::new(range.anchor, range.head)
    }
}

impl From<RangeAnchor> for helix_core::Range {
    fn from(range: RangeAnchor) -> Self {
        range.to_range()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewportAnchor {
    pub anchor: usize,
    pub vertical_offset: usize,
    pub horizontal_offset: usize,
}

impl ViewportAnchor {
    #[must_use]
    pub const fn new(anchor: usize, vertical_offset: usize, horizontal_offset: usize) -> Self {
        Self {
            anchor,
            vertical_offset,
            horizontal_offset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Option<RangeAnchor>,
    pub source: Source,
    pub surface: Option<crate::collab::SurfaceId>,
    pub entry: Option<crate::assistant::thread::EntryId>,
}

impl Location {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, source: Source) -> Self {
        Self {
            path: path.into(),
            range: None,
            source,
            surface: None,
            entry: None,
        }
    }

    #[must_use]
    pub fn with_range(mut self, range: impl Into<RangeAnchor>) -> Self {
        self.range = Some(range.into());
        self
    }

    #[must_use]
    pub fn on_surface(mut self, surface: crate::collab::SurfaceId) -> Self {
        self.surface = Some(surface);
        self
    }

    #[must_use]
    pub fn for_entry(mut self, entry: crate::assistant::thread::EntryId) -> Self {
        self.entry = Some(entry);
        self
    }
}
