use std::path::PathBuf;

use helix_core::Syntax;
use helix_view::DocumentId;

pub enum DocumentCommand {
    ApplySyntax {
        document: DocumentId,
        path: PathBuf,
        version: i32,
        syntax: Syntax,
    },
}

impl std::fmt::Debug for DocumentCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplySyntax {
                document,
                path,
                version,
                ..
            } => f
                .debug_struct("ApplySyntax")
                .field("document", document)
                .field("path", path)
                .field("version", version)
                .finish_non_exhaustive(),
        }
    }
}
