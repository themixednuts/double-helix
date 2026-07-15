#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgRefreshStage {
    Catalog,
    Enrichment,
}

/// Package-manager UI operations completed off the main thread.
pub enum PkgCommand {
    Refresh {
        request_id: u64,
        finished_revision: u64,
        stage: PkgRefreshStage,
        result: Result<crate::ui::pkg::PkgManagerData, String>,
    },
}

impl std::fmt::Debug for PkgCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refresh {
                request_id,
                finished_revision,
                stage,
                result,
            } => f
                .debug_struct("Refresh")
                .field("request_id", request_id)
                .field("finished_revision", finished_revision)
                .field("stage", stage)
                .field("ok", &result.is_ok())
                .finish(),
        }
    }
}
