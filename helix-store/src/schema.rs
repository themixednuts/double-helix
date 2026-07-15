#![allow(dead_code)]

use drizzle::sqlite::prelude::*;

#[SQLiteTable(name = "assistant_threads")]
pub struct AssistantThreads {
    #[column(primary)]
    pub id: String,
    pub scope: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub rating: Option<String>,
    pub has_feedback: i64,
    pub record_json: String,
}

#[SQLiteTable(name = "assistant_layout")]
pub struct AssistantLayout {
    #[column(primary)]
    pub scope: String,
    pub open_ids: String,
    pub active_id: Option<String>,
}

#[SQLiteTable(name = "assistant_permissions")]
pub struct AssistantPermissions {
    #[column(primary)]
    pub agent: String,
    #[column(primary)]
    pub tool: String,
    pub choice: String,
}

#[SQLiteTable(name = "frecency")]
pub struct Frecency {
    #[column(primary)]
    pub workspace: String,
    #[column(primary)]
    pub path_hash: String,
    pub first_accessed_at: i64,
    pub last_accessed_at: i64,
    pub access_count: i64,
    pub timestamps_json: String,
}

#[SQLiteTable(name = "query_history")]
pub struct QueryHistory {
    #[column(primary)]
    pub id: String,
    pub workspace: String,
    pub query: String,
    pub opened_path: String,
    pub ts: i64,
}

#[SQLiteTable(name = "pkg_receipts")]
pub struct PkgReceipts {
    #[column(primary)]
    pub kind: String,
    #[column(primary)]
    pub name: String,
    pub version: String,
    pub source: String,
    pub hash: String,
    pub bin: String,
    pub shim: String,
    pub files_json: String,
    pub installed_at: String,
    pub native_manager: Option<String>,
    pub native_id: Option<String>,
    pub receipt_json: String,
}

#[SQLiteTable(name = "pkg_runtime_assets")]
pub struct PkgRuntimeAssets {
    #[column(primary)]
    pub asset_kind: String,
    #[column(primary)]
    pub asset_key: String,
    pub package_kind: String,
    pub package_name: String,
    pub package_version: String,
    pub path: String,
    pub prefix_args_json: String,
    pub default_args_json: String,
    pub env_json: String,
}

#[SQLiteTable(name = "pkg_activation_history")]
pub struct PkgActivationHistory {
    #[column(primary, autoincrement)]
    pub id: i64,
    pub package_kind: String,
    pub package_name: String,
    pub package_version: String,
    pub operation: String,
    pub previous_assets_json: String,
    pub activated_assets_json: String,
    pub generation: i64,
    pub rolled_back_generation: Option<i64>,
    pub created_at: i64,
}

#[SQLiteTable(name = "pkg_runtime_meta")]
pub struct PkgRuntimeMeta {
    #[column(primary)]
    pub singleton: i64,
    pub runtime_generation: i64,
}

#[SQLiteTable(name = "pkg_registry_heads")]
pub struct PkgRegistryHeads {
    #[column(primary)]
    pub registry: String,
    pub source: String,
    pub revision: String,
    pub updated_at: i64,
}

#[derive(SQLiteSchema)]
pub struct Schema {
    pub assistant_threads: AssistantThreads,
    pub assistant_layout: AssistantLayout,
    pub assistant_permissions: AssistantPermissions,
    pub frecency: Frecency,
    pub query_history: QueryHistory,
    pub pkg_receipts: PkgReceipts,
    pub pkg_runtime_assets: PkgRuntimeAssets,
    pub pkg_activation_history: PkgActivationHistory,
    pub pkg_runtime_meta: PkgRuntimeMeta,
    pub pkg_registry_heads: PkgRegistryHeads,
}
