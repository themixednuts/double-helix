#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantThread {
    pub id: String,
    pub scope: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub rating: Option<String>,
    pub has_feedback: bool,
    pub record_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantLayout {
    pub scope: String,
    pub open_ids: Vec<String>,
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantPermission {
    pub agent: String,
    pub tool: String,
    pub choice: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrecencyEntry {
    pub workspace: String,
    pub path_hash: String,
    pub first_accessed_at: i64,
    pub last_accessed_at: i64,
    pub access_count: i64,
    pub timestamps_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHistory {
    pub id: String,
    pub workspace: String,
    pub query: String,
    pub opened_path: String,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgReceipt {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub hash: String,
    pub installed_at: String,
    pub receipt_json: String,
}
