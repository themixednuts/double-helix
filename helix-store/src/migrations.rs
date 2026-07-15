use crate::DatabaseKind;

pub(crate) struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub(crate) fn migrations(kind: DatabaseKind) -> &'static [Migration] {
    match kind {
        DatabaseKind::State => STATE_MIGRATIONS,
        DatabaseKind::Cache => CACHE_MIGRATIONS,
    }
}

const STATE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "store_foundation_state",
        sql: r#"
CREATE TABLE IF NOT EXISTS assistant_threads (
    id TEXT PRIMARY KEY NOT NULL,
    scope TEXT NOT NULL,
    title TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    rating TEXT,
    has_feedback INTEGER NOT NULL DEFAULT 0,
    record_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_assistant_threads_scope_updated
    ON assistant_threads(scope, updated_at DESC);

CREATE TABLE IF NOT EXISTS assistant_layout (
    scope TEXT PRIMARY KEY NOT NULL,
    open_ids TEXT NOT NULL,
    active_id TEXT
);

CREATE TABLE IF NOT EXISTS assistant_permissions (
    agent TEXT NOT NULL,
    tool TEXT NOT NULL,
    choice TEXT NOT NULL,
    PRIMARY KEY(agent, tool)
);

CREATE TABLE IF NOT EXISTS pkg_receipts (
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    source TEXT NOT NULL,
    hash TEXT NOT NULL,
    installed_at TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    PRIMARY KEY(kind, name)
);
"#,
    },
    Migration {
        version: 2,
        name: "pkg_receipts_indexed_columns",
        sql: r#"
ALTER TABLE pkg_receipts ADD COLUMN bin TEXT NOT NULL DEFAULT '';
ALTER TABLE pkg_receipts ADD COLUMN shim TEXT NOT NULL DEFAULT '';
ALTER TABLE pkg_receipts ADD COLUMN files_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE pkg_receipts ADD COLUMN native_manager TEXT;
ALTER TABLE pkg_receipts ADD COLUMN native_id TEXT;

CREATE INDEX IF NOT EXISTS idx_pkg_receipts_kind_source
    ON pkg_receipts(kind, source);
"#,
    },
    Migration {
        version: 3,
        name: "assistant_threads_filtered_scope_index",
        sql: r#"
CREATE INDEX IF NOT EXISTS idx_assistant_threads_scope_rating_feedback_updated
    ON assistant_threads(scope, rating, has_feedback, updated_at DESC);
"#,
    },
    Migration {
        version: 4,
        name: "pkg_runtime_assets",
        sql: r#"
CREATE TABLE IF NOT EXISTS pkg_runtime_assets (
    asset_kind TEXT NOT NULL,
    asset_key TEXT NOT NULL,
    package_kind TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    path TEXT NOT NULL,
    prefix_args_json TEXT NOT NULL DEFAULT '[]',
    default_args_json TEXT NOT NULL DEFAULT '[]',
    env_json TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY(asset_kind, asset_key),
    CHECK(asset_kind IN ('command', 'file', 'grammar', 'plugin-root'))
);

CREATE INDEX IF NOT EXISTS idx_pkg_runtime_assets_package
    ON pkg_runtime_assets(package_kind, package_name);

CREATE TABLE IF NOT EXISTS pkg_activation_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_kind TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    operation TEXT NOT NULL,
    previous_assets_json TEXT NOT NULL,
    activated_assets_json TEXT NOT NULL,
    generation INTEGER NOT NULL,
    rolled_back_generation INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX IF NOT EXISTS idx_pkg_activation_history_package
    ON pkg_activation_history(package_kind, package_name, id DESC);

CREATE TABLE IF NOT EXISTS pkg_runtime_meta (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    runtime_generation INTEGER NOT NULL
);

INSERT INTO pkg_runtime_meta(singleton, runtime_generation)
VALUES (1, 0)
ON CONFLICT(singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS pkg_registry_heads (
    registry TEXT PRIMARY KEY NOT NULL,
    source TEXT NOT NULL,
    revision TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#,
    },
];

const CACHE_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "store_foundation_cache",
    sql: r#"
CREATE TABLE IF NOT EXISTS frecency (
    workspace TEXT NOT NULL,
    path_hash TEXT NOT NULL,
    first_accessed_at INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL,
    access_count INTEGER NOT NULL,
    timestamps_json TEXT NOT NULL,
    PRIMARY KEY(workspace, path_hash)
);

CREATE INDEX IF NOT EXISTS idx_frecency_workspace_last_accessed
    ON frecency(workspace, last_accessed_at DESC);

CREATE TABLE IF NOT EXISTS query_history (
    id TEXT PRIMARY KEY NOT NULL,
    workspace TEXT NOT NULL,
    query TEXT NOT NULL,
    opened_path TEXT NOT NULL,
    ts INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_query_history_workspace_ts
    ON query_history(workspace, ts DESC);
"#,
}];

pub(crate) fn version_table_sql() -> &'static str {
    r#"
CREATE TABLE IF NOT EXISTS helix_store_schema_versions (
    version INTEGER PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    applied_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#
}

pub(crate) fn insert_version_sql() -> &'static str {
    "INSERT INTO helix_store_schema_versions(version, name) VALUES (?1, ?2)"
}

pub(crate) fn has_version_sql() -> &'static str {
    "SELECT EXISTS(SELECT 1 FROM helix_store_schema_versions WHERE version = ?1)"
}
