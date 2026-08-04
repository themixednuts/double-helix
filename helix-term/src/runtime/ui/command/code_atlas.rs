use code_atlas::Briefing;
use helix_core::Range;
use helix_view::{DocumentId, ViewId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BriefingTarget {
    pub document: DocumentId,
    pub range: Range,
}

#[derive(Debug)]
pub enum CodeAtlasCommand {
    Open {
        briefing: Briefing<BriefingTarget>,
        source_document: DocumentId,
        source_view: ViewId,
    },
}
