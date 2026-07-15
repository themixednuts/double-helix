use std::collections::BTreeSet;
use std::path::PathBuf;

use drizzle::core::desc;
use drizzle::core::expr::{and, eq};
use drizzle::sqlite::connection::SQLiteTransactionType;
use rusqlite::{params, OptionalExtension};

use crate::backend::{Backend, DrizzleBackend};
use crate::dto::{
    ActivationHistory, ActivePackage, AssistantLayout, AssistantPermission, AssistantThread,
    FrecencyEntry, PackageActivation, PackageState, PackageStateCommit, PkgReceipt, QueryHistory,
    RegistryHead, RuntimeAsset, RuntimeAssetKind, RuntimeSnapshot,
};
use crate::error::Result;
use crate::schema::{
    InsertAssistantLayout, InsertAssistantPermissions, InsertAssistantThreads, InsertFrecency,
    InsertQueryHistory, SelectAssistantLayout, SelectAssistantPermissions, SelectAssistantThreads,
    SelectFrecency, SelectPkgReceipts, SelectQueryHistory, UpdateAssistantPermissions,
    UpdateFrecency,
};

pub struct AssistantThreadsRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> AssistantThreadsRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Inserts or updates a persisted assistant thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn upsert(&mut self, thread: AssistantThread) -> Result<()> {
        let table = self.backend.schema.assistant_threads;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.delete(table)
                    .r#where(eq(table.id, thread.id.as_str()))
                    .execute()?;
                let row = InsertAssistantThreads::new(
                    thread.id,
                    thread.scope,
                    thread.created_at,
                    thread.updated_at,
                    bool_to_i64(thread.has_feedback),
                    thread.record_json,
                );
                match (thread.title, thread.rating) {
                    (Some(title), Some(rating)) => {
                        tx.insert(table)
                            .value(row.with_title(title).with_rating(rating))
                            .execute()?;
                    }
                    (Some(title), None) => {
                        tx.insert(table).value(row.with_title(title)).execute()?;
                    }
                    (None, Some(rating)) => {
                        tx.insert(table).value(row.with_rating(rating)).execute()?;
                    }
                    (None, None) => {
                        tx.insert(table).value(row).execute()?;
                    }
                }
                Ok(())
            })?;
        Ok(())
    }

    /// Lists thread stubs for one serialized scope, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn list_by_scope(&mut self, scope: &str) -> Result<Vec<AssistantThread>> {
        let table = self.backend.schema.assistant_threads;
        let rows: Vec<SelectAssistantThreads> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(eq(table.scope, scope))
            .order_by(desc(table.updated_at))
            .all()?;
        Ok(rows.into_iter().map(AssistantThread::from).collect())
    }

    /// Lists thread stubs for one serialized scope, filtered by indexed feedback columns.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn list_by_scope_filtered(
        &mut self,
        scope: &str,
        rating: Option<&str>,
        has_feedback: Option<bool>,
    ) -> Result<Vec<AssistantThread>> {
        let mut sql = String::from(
            "SELECT id, scope, title, created_at, updated_at, rating, has_feedback, record_json \
             FROM assistant_threads WHERE scope = ?1",
        );
        match (rating, has_feedback) {
            (Some(_), Some(_)) => sql.push_str(" AND rating = ?2 AND has_feedback = ?3"),
            (Some(_), None) => sql.push_str(" AND rating = ?2"),
            (None, Some(_)) => sql.push_str(" AND has_feedback = ?2"),
            (None, None) => {}
        }
        sql.push_str(" ORDER BY updated_at DESC");

        let mut stmt = self.backend.conn().prepare(&sql)?;
        let rows = match (rating, has_feedback) {
            (Some(rating), Some(has_feedback)) => stmt
                .query_map(
                    params![scope, rating, bool_to_i64(has_feedback)],
                    select_thread,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (Some(rating), None) => stmt
                .query_map(params![scope, rating], select_thread)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (None, Some(has_feedback)) => stmt
                .query_map(params![scope, bool_to_i64(has_feedback)], select_thread)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (None, None) => stmt
                .query_map(params![scope], select_thread)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// Loads one assistant thread by id.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn get(&mut self, id: &str) -> Result<Option<AssistantThread>> {
        let table = self.backend.schema.assistant_threads;
        let rows: Vec<SelectAssistantThreads> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(eq(table.id, id))
            .limit(1)
            .all()?;
        Ok(rows.into_iter().next().map(AssistantThread::from))
    }

    /// Deletes one assistant thread by id.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite delete fails.
    pub fn delete(&mut self, id: &str) -> Result<()> {
        let table = self.backend.schema.assistant_threads;
        self.backend
            .db()
            .delete(table)
            .r#where(eq(table.id, id))
            .execute()?;
        Ok(())
    }

    /// Returns true once legacy assistant files have been imported into the state DB.
    ///
    /// # Errors
    ///
    /// Returns an error if the marker query fails.
    pub fn has_assistant_import_marker(&mut self) -> Result<bool> {
        ensure_assistant_import_marker_table(self.backend)?;
        let value: Option<i64> = self
            .backend
            .conn()
            .query_row(
                "SELECT 1 FROM helix_store_import_markers WHERE name = ?1 LIMIT 1",
                params![ASSISTANT_IMPORT_MARKER],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.is_some())
    }

    /// Imports legacy assistant state and records the marker in one transaction.
    ///
    /// Returns `true` when this call performed the import, or `false` when the marker already
    /// existed and the inputs were ignored.
    ///
    /// # Errors
    ///
    /// Returns an error if any SQLite write in the import transaction fails.
    pub fn import_assistant_state_once(
        &mut self,
        threads: Vec<AssistantThread>,
        layouts: Vec<AssistantLayout>,
        permissions: Vec<AssistantPermission>,
    ) -> Result<bool> {
        self.backend.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            ensure_assistant_import_marker_table(self.backend)?;
            let exists: Option<i64> = self
                .backend
                .conn()
                .query_row(
                    "SELECT 1 FROM helix_store_import_markers WHERE name = ?1 LIMIT 1",
                    params![ASSISTANT_IMPORT_MARKER],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_some() {
                return Ok(false);
            }

            for thread in threads {
                self.backend.execute(
                    "INSERT OR REPLACE INTO assistant_threads \
                     (id, scope, title, created_at, updated_at, rating, has_feedback, record_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        thread.id,
                        thread.scope,
                        thread.title,
                        thread.created_at,
                        thread.updated_at,
                        thread.rating,
                        bool_to_i64(thread.has_feedback),
                        thread.record_json,
                    ],
                )?;
            }

            for layout in layouts {
                self.backend.execute(
                    "INSERT OR REPLACE INTO assistant_layout(scope, open_ids, active_id) \
                     VALUES (?1, ?2, ?3)",
                    params![
                        layout.scope,
                        serde_json::to_string(&layout.open_ids)?,
                        layout.active_id,
                    ],
                )?;
            }

            for permission in permissions {
                self.backend.execute(
                    "INSERT OR REPLACE INTO assistant_permissions(agent, tool, choice) \
                     VALUES (?1, ?2, ?3)",
                    params![permission.agent, permission.tool, permission.choice],
                )?;
            }

            self.backend.execute(
                "INSERT INTO helix_store_import_markers(name) VALUES (?1)",
                params![ASSISTANT_IMPORT_MARKER],
            )?;
            Ok(true)
        })();

        match result {
            Ok(imported) => {
                self.backend.execute_batch("COMMIT")?;
                Ok(imported)
            }
            Err(err) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

pub struct AssistantLayoutRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> AssistantLayoutRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Inserts or updates layout state for one scope.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or SQLite write fails.
    pub fn upsert(&mut self, layout: AssistantLayout) -> Result<()> {
        let table = self.backend.schema.assistant_layout;
        let open_ids = serde_json::to_string(&layout.open_ids)?;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.delete(table)
                    .r#where(eq(table.scope, layout.scope.as_str()))
                    .execute()?;
                let row = InsertAssistantLayout::new(layout.scope, open_ids);
                if let Some(active_id) = layout.active_id {
                    tx.insert(table)
                        .value(row.with_active_id(active_id))
                        .execute()?;
                } else {
                    tx.insert(table).value(row).execute()?;
                }
                Ok(())
            })?;
        Ok(())
    }

    /// Loads layout state for one scope.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query or JSON decoding fails.
    pub fn get(&mut self, scope: &str) -> Result<Option<AssistantLayout>> {
        let table = self.backend.schema.assistant_layout;
        let rows: Vec<SelectAssistantLayout> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(eq(table.scope, scope))
            .limit(1)
            .all()?;
        rows.into_iter()
            .next()
            .map(AssistantLayout::try_from)
            .transpose()
    }
}

pub struct AssistantPermissionsRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> AssistantPermissionsRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Remembers or replaces one permission choice for an agent/tool pair.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn remember(&mut self, permission: AssistantPermission) -> Result<()> {
        let table = self.backend.schema.assistant_permissions;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.insert(table)
                    .value(InsertAssistantPermissions::new(
                        permission.agent.clone(),
                        permission.tool.clone(),
                        permission.choice.clone(),
                    ))
                    .execute()
                    .or_else(|_| {
                        tx.update(table)
                            .set(
                                UpdateAssistantPermissions::default()
                                    .with_choice(permission.choice),
                            )
                            .r#where(and(
                                eq(table.agent, permission.agent),
                                eq(table.tool, permission.tool),
                            ))
                            .execute()
                    })?;
                Ok(())
            })?;
        Ok(())
    }

    /// Lists all stored permission choices.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn all(&mut self) -> Result<Vec<AssistantPermission>> {
        let table = self.backend.schema.assistant_permissions;
        let rows: Vec<SelectAssistantPermissions> =
            self.backend.db().select(()).from(table).all()?;
        Ok(rows.into_iter().map(AssistantPermission::from).collect())
    }

    /// Removes every stored assistant permission choice.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite delete fails.
    pub fn clear(&mut self) -> Result<()> {
        let table = self.backend.schema.assistant_permissions;
        self.backend.db().delete(table).execute()?;
        Ok(())
    }

    /// Replaces all stored assistant permission choices in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite transaction fails.
    pub fn replace_all(&mut self, permissions: Vec<AssistantPermission>) -> Result<()> {
        let table = self.backend.schema.assistant_permissions;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.delete(table).execute()?;
                for permission in permissions {
                    tx.insert(table)
                        .value(InsertAssistantPermissions::new(
                            permission.agent,
                            permission.tool,
                            permission.choice,
                        ))
                        .execute()?;
                }
                Ok(())
            })?;
        Ok(())
    }
}

pub struct FrecencyRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> FrecencyRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Records a file access for one workspace/path hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn bump(&mut self, workspace: &str, path_hash: &str, ts: i64) -> Result<()> {
        let existing = self.get(workspace, path_hash)?;
        match existing {
            Some(mut entry) => {
                entry.last_accessed_at = ts;
                entry.access_count += 1;
                entry.timestamps_json = append_timestamp(&entry.timestamps_json, ts)?;
                self.upsert(entry)
            }
            None => self.upsert(FrecencyEntry {
                workspace: workspace.to_owned(),
                path_hash: path_hash.to_owned(),
                first_accessed_at: ts,
                last_accessed_at: ts,
                access_count: 1,
                timestamps_json: serde_json::to_string(&[ts])?,
            }),
        }?;
        Ok(())
    }

    /// Inserts or updates one frecency entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn upsert(&mut self, entry: FrecencyEntry) -> Result<()> {
        let table = self.backend.schema.frecency;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.insert(table)
                    .value(InsertFrecency::new(
                        entry.workspace.clone(),
                        entry.path_hash.clone(),
                        entry.first_accessed_at,
                        entry.last_accessed_at,
                        entry.access_count,
                        entry.timestamps_json.clone(),
                    ))
                    .execute()
                    .or_else(|_| {
                        tx.update(table)
                            .set(
                                UpdateFrecency::default()
                                    .with_first_accessed_at(entry.first_accessed_at)
                                    .with_last_accessed_at(entry.last_accessed_at)
                                    .with_access_count(entry.access_count)
                                    .with_timestamps_json(entry.timestamps_json),
                            )
                            .r#where(and(
                                eq(table.workspace, entry.workspace),
                                eq(table.path_hash, entry.path_hash),
                            ))
                            .execute()
                    })?;
                Ok(())
            })?;
        Ok(())
    }

    /// Loads a frecency entry by workspace and path hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn get(&mut self, workspace: &str, path_hash: &str) -> Result<Option<FrecencyEntry>> {
        let table = self.backend.schema.frecency;
        let rows: Vec<SelectFrecency> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(and(
                eq(table.workspace, workspace),
                eq(table.path_hash, path_hash),
            ))
            .limit(1)
            .all()?;
        Ok(rows.into_iter().next().map(FrecencyEntry::from))
    }

    /// Lists frecency entries for one workspace, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn list_by_workspace(&mut self, workspace: &str) -> Result<Vec<FrecencyEntry>> {
        let table = self.backend.schema.frecency;
        let rows: Vec<SelectFrecency> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(eq(table.workspace, workspace))
            .order_by(desc(table.last_accessed_at))
            .all()?;
        Ok(rows.into_iter().map(FrecencyEntry::from).collect())
    }

    /// Imports rebuildable FFF cache rows once and records the marker in the same transaction.
    ///
    /// Returns `true` when this call performed the import, or `false` when the marker already
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite transaction fails.
    pub fn import_fff_cache_once(
        &mut self,
        marker: &str,
        frecency_entries: &[FrecencyEntry],
        query_entries: &[QueryHistory],
    ) -> Result<bool> {
        self.backend.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            ensure_cache_import_marker_table(self.backend)?;
            let exists: i64 = self.backend.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
                params![marker],
                |row| row.get(0),
            )?;
            if exists != 0 {
                return Ok(false);
            }

            for entry in frecency_entries {
                insert_frecency(self.backend.conn(), entry)?;
            }
            for entry in query_entries {
                insert_query_history(self.backend.conn(), entry)?;
            }
            self.backend.execute(
                "INSERT INTO helix_store_import_markers(name) VALUES (?1)",
                params![marker],
            )?;
            Ok(true)
        })();

        match result {
            Ok(imported) => {
                self.backend.execute_batch("COMMIT")?;
                Ok(imported)
            }
            Err(err) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    /// Checks whether an import marker exists in the cache DB.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn import_marker_exists(&mut self, marker: &str) -> Result<bool> {
        ensure_cache_import_marker_table(self.backend)?;
        let exists: i64 = self.backend.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
            params![marker],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Deletes one frecency entry by workspace and path hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite delete fails.
    pub fn delete(&mut self, workspace: &str, path_hash: &str) -> Result<()> {
        let table = self.backend.schema.frecency;
        self.backend
            .db()
            .delete(table)
            .r#where(and(
                eq(table.workspace, workspace),
                eq(table.path_hash, path_hash),
            ))
            .execute()?;
        Ok(())
    }
}

pub struct QueryHistoryRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> QueryHistoryRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Adds one query-history record.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn add(&mut self, item: QueryHistory) -> Result<()> {
        let table = self.backend.schema.query_history;
        self.backend
            .db()
            .insert(table)
            .value(InsertQueryHistory::new(
                item.id,
                item.workspace,
                item.query,
                item.opened_path,
                item.ts,
            ))
            .execute()?;
        Ok(())
    }

    /// Lists query-history records for one workspace, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn list_by_workspace(&mut self, workspace: &str) -> Result<Vec<QueryHistory>> {
        let table = self.backend.schema.query_history;
        let rows: Vec<SelectQueryHistory> = self
            .backend
            .db()
            .select(())
            .from(table)
            .r#where(eq(table.workspace, workspace))
            .order_by(desc(table.ts))
            .all()?;
        Ok(rows.into_iter().map(QueryHistory::from).collect())
    }

    /// Saves the latest query-to-opened-file association payload for one workspace/query.
    ///
    /// The payload is owned by the caller so `fff-search` can stay storage-agnostic.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn save_query_match(
        &mut self,
        workspace: &str,
        query: &str,
        payload_json: &str,
        ts: i64,
    ) -> Result<()> {
        let item = QueryHistory {
            id: query_match_id(workspace, query),
            workspace: workspace.to_owned(),
            query: query.to_owned(),
            opened_path: payload_json.to_owned(),
            ts,
        };
        insert_query_history(self.backend.conn(), &item)?;
        Ok(())
    }

    /// Loads the latest query-to-opened-file association payload for one workspace/query.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn load_query_match(&mut self, workspace: &str, query: &str) -> Result<Option<String>> {
        self.backend
            .conn()
            .query_row(
                "SELECT opened_path FROM query_history WHERE id = ?1 LIMIT 1",
                params![query_match_id(workspace, query)],
                |row| row.get(0),
            )
            .optional()
            .map_err(crate::Error::from)
    }

    /// Appends one bounded file-picker or grep query-history entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn append_bounded_history(
        &mut self,
        workspace: &str,
        kind: &str,
        query: &str,
        ts: i64,
    ) -> Result<()> {
        self.backend.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let item = QueryHistory {
                id: query_history_id(workspace, kind, query, ts),
                workspace: workspace.to_owned(),
                query: query.to_owned(),
                opened_path: history_marker(kind),
                ts,
            };
            insert_query_history(self.backend.conn(), &item)?;
            prune_history(
                self.backend.conn(),
                workspace,
                kind,
                MAX_QUERY_HISTORY_ENTRIES,
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.backend.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(err) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    /// Reads a bounded query-history item where offset 0 is the newest item.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn history_at(
        &mut self,
        workspace: &str,
        kind: &str,
        offset: usize,
    ) -> Result<Option<String>> {
        self.backend
            .conn()
            .query_row(
                "SELECT query FROM query_history \
                 WHERE workspace = ?1 AND opened_path = ?2 \
                 ORDER BY ts DESC, id DESC LIMIT 1 OFFSET ?3",
                params![workspace, history_marker(kind), offset as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(crate::Error::from)
    }
}

pub struct PkgReceiptsRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> PkgReceiptsRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Inserts or updates one package receipt.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn upsert(&mut self, receipt: PkgReceipt) -> Result<()> {
        self.in_immediate_transaction(|conn| insert_pkg_receipt(conn, &receipt).map(|_| ()))
    }

    /// Imports legacy package receipts once and records the marker in the same transaction.
    ///
    /// Returns `true` when the import ran, and `false` when the marker already existed.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite import transaction fails.
    pub fn import_once(&mut self, marker: &str, receipts: &[PkgReceipt]) -> Result<bool> {
        self.in_immediate_transaction(|conn| {
            ensure_pkg_import_marker_table(conn)?;
            let exists: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
                params![marker],
                |row| row.get(0),
            )?;
            if exists != 0 {
                return Ok(false);
            }
            for receipt in receipts {
                insert_pkg_receipt(conn, receipt)?;
            }
            conn.execute(
                "INSERT INTO helix_store_import_markers(name) VALUES (?1)",
                params![marker],
            )?;
            Ok(true)
        })
    }

    /// Checks whether an import marker exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn import_marker_exists(&mut self, marker: &str) -> Result<bool> {
        self.backend.execute_batch(PKG_IMPORT_MARKERS_SQL)?;
        let exists: i64 = self.backend.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
            params![marker],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Loads one package receipt by kind/name.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn get(&mut self, kind: &str, name: &str) -> Result<Option<PkgReceipt>> {
        self.backend
            .conn()
            .query_row(
                PKG_RECEIPT_SELECT_SQL,
                params![kind, name],
                pkg_receipt_from_row,
            )
            .optional()
            .map_err(crate::Error::from)
    }

    /// Lists all package receipts ordered by kind/name.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn all(&mut self) -> Result<Vec<PkgReceipt>> {
        let mut stmt = self.backend.conn().prepare(PKG_RECEIPT_SELECT_ALL_SQL)?;
        let rows = stmt
            .query_map([], pkg_receipt_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deletes one package receipt.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite delete fails.
    pub fn delete(&mut self, kind: &str, name: &str) -> Result<()> {
        let table = self.backend.schema.pkg_receipts;
        self.backend
            .db()
            .delete(table)
            .r#where(and(eq(table.kind, kind), eq(table.name, name)))
            .execute()?;
        Ok(())
    }

    fn in_immediate_transaction<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        self.backend.execute_batch("BEGIN IMMEDIATE")?;
        let result = f(self.backend.conn());
        match result {
            Ok(value) => {
                self.backend.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

pub struct PackageStateRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> PackageStateRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Loads one package's receipt and active runtime assets from a coherent read transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the package key is invalid, the SQLite read fails, or persisted runtime
    /// assets cannot be decoded.
    pub fn get(&mut self, package_kind: &str, package_name: &str) -> Result<PackageState> {
        validate_package_key(package_kind, package_name)?;
        self.in_transaction("BEGIN DEFERRED", |conn| {
            select_package_state(conn, package_kind, package_name)
        })
    }

    /// Atomically replaces a package's runtime activation and receipt.
    ///
    /// The receipt row, runtime assets, activation history, generation, and returned snapshot are
    /// produced by one `BEGIN IMMEDIATE` transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or mismatched receipt and activation, asset collisions,
    /// serialization failures, or SQLite transaction failures.
    pub fn activate(
        &mut self,
        receipt: PkgReceipt,
        activation: PackageActivation,
    ) -> Result<PackageStateCommit> {
        validate_activation(&activation)?;
        validate_receipt_package(&receipt, &activation.package)?;
        let package = activation.package.clone();
        let activated = materialize_activation(activation);

        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let before = select_package_state(conn, &package.kind, &package.name)?;
            ensure_no_asset_collisions(conn, &package, &activated)?;
            if before.assets != activated {
                replace_package_assets(conn, &package, &activated)?;
                let generation = bump_runtime_generation(conn)?;
                insert_activation_history(
                    conn,
                    &package,
                    "activate",
                    &before.assets,
                    &activated,
                    generation,
                )?;
            }
            insert_pkg_receipt(conn, &receipt)?;
            package_state_commit(conn, before)
        })
    }

    /// Atomically removes a package's runtime activation and receipt.
    ///
    /// Removing absent state is a no-op that preserves the runtime generation.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid package key, invalid persisted state, or a SQLite
    /// transaction failure.
    pub fn deactivate(
        &mut self,
        package_kind: &str,
        package_name: &str,
    ) -> Result<PackageStateCommit> {
        validate_package_key(package_kind, package_name)?;
        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let before = select_package_state(conn, package_kind, package_name)?;
            if let Some(package) = package_from_assets(package_kind, package_name, &before.assets)?
            {
                delete_package_assets(conn, package_kind, package_name)?;
                let generation = bump_runtime_generation(conn)?;
                insert_activation_history(
                    conn,
                    &package,
                    "remove",
                    &before.assets,
                    &[],
                    generation,
                )?;
            }
            delete_pkg_receipt(conn, package_kind, package_name)?;
            package_state_commit(conn, before)
        })
    }

    /// Atomically restores the latest activation before-image and its matching receipt.
    ///
    /// Returns `None` when there is no prior non-empty package activation to restore. If current
    /// runtime rows no longer match the history after-image, rollback fails without changing any
    /// package state.
    ///
    /// # Errors
    ///
    /// Returns an error for a mismatched rollback receipt, history divergence, collisions,
    /// invalid persisted state, or a SQLite transaction failure.
    pub fn rollback(&mut self, receipt: PkgReceipt) -> Result<Option<PackageStateCommit>> {
        validate_package_key(&receipt.kind, &receipt.name)?;
        let package_kind = receipt.kind.clone();
        let package_name = receipt.name.clone();

        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let before = select_package_state(conn, &package_kind, &package_name)?;
            let Some(event) = select_latest_activation(conn, &package_kind, &package_name)? else {
                return Ok(None);
            };
            if before.assets != event.activated_assets {
                return Err(crate::Error::RuntimeHistoryDiverged {
                    package: package_label(&package_kind, &package_name),
                });
            }
            let Some(restored_package) =
                package_from_assets(&package_kind, &package_name, &event.previous_assets)?
            else {
                return Ok(None);
            };
            validate_receipt_package(&receipt, &restored_package)?;
            ensure_no_asset_collisions(conn, &restored_package, &event.previous_assets)?;
            replace_package_assets(conn, &restored_package, &event.previous_assets)?;
            let generation = bump_runtime_generation(conn)?;
            conn.execute(
                "UPDATE pkg_activation_history SET rolled_back_generation = ?1 WHERE id = ?2",
                params![generation_to_i64(generation)?, event.id],
            )?;
            insert_pkg_receipt(conn, &receipt)?;
            package_state_commit(conn, before).map(Some)
        })
    }

    /// Reconciles a receipt with the package's authoritative active runtime state.
    ///
    /// A receipt can only be written for matching non-empty runtime assets, and a receipt can only
    /// be removed when the package has no active runtime assets. The generation and activation
    /// history are unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested receipt conflicts with active runtime state or when the
    /// SQLite transaction fails.
    pub fn reconcile_receipt(
        &mut self,
        package_kind: &str,
        package_name: &str,
        receipt: Option<PkgReceipt>,
    ) -> Result<PackageStateCommit> {
        validate_package_key(package_kind, package_name)?;
        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let before = select_package_state(conn, package_kind, package_name)?;
            match &receipt {
                Some(receipt) => {
                    let package = package_from_assets(package_kind, package_name, &before.assets)?
                        .ok_or_else(|| {
                            crate::Error::InvalidPackageState(format!(
                                "cannot persist a receipt for inactive package '{}'",
                                package_label(package_kind, package_name)
                            ))
                        })?;
                    validate_receipt_package(receipt, &package)?;
                    insert_pkg_receipt(conn, receipt)?;
                }
                None => {
                    if !before.assets.is_empty() {
                        return Err(crate::Error::InvalidPackageState(format!(
                            "cannot remove the receipt for active package '{}'",
                            package_label(package_kind, package_name)
                        )));
                    }
                    delete_pkg_receipt(conn, package_kind, package_name)?;
                }
            }
            package_state_commit(conn, before)
        })
    }

    fn in_transaction<T>(
        &mut self,
        begin: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        self.backend.execute_batch(begin)?;
        let result = f(self.backend.conn());
        match result {
            Ok(value) => match self.backend.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(error) => {
                    let _ = self.backend.execute_batch("ROLLBACK");
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

pub struct RuntimeAssetsRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> RuntimeAssetsRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Replaces one package's active runtime assets in a single transaction.
    ///
    /// Asset keys are unique within each asset kind. An activation that would shadow another
    /// package is rejected without changing the active snapshot or generation.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid assets, key collisions, serialization failures, or SQLite
    /// transaction failures.
    pub fn activate(&mut self, activation: PackageActivation) -> Result<u64> {
        validate_activation(&activation)?;
        let package = activation.package.clone();
        let activated = materialize_activation(activation);
        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let previous = select_package_assets(conn, &package.kind, &package.name)?;
            ensure_no_asset_collisions(conn, &package, &activated)?;
            if previous == activated {
                return runtime_generation(conn);
            }

            replace_package_assets(conn, &package, &activated)?;
            let generation = bump_runtime_generation(conn)?;
            insert_activation_history(
                conn, &package, "activate", &previous, &activated, generation,
            )?;
            Ok(generation)
        })
    }

    /// Removes one package's active runtime assets in a single transaction.
    ///
    /// Removing a package with no active assets is a no-op and does not advance the generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the active snapshot cannot be read or the SQLite transaction fails.
    pub fn remove(&mut self, package_kind: &str, package_name: &str) -> Result<u64> {
        validate_package_key(package_kind, package_name)?;
        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let previous = select_package_assets(conn, package_kind, package_name)?;
            if previous.is_empty() {
                return runtime_generation(conn);
            }

            let package = previous[0].package.clone();
            delete_package_assets(conn, package_kind, package_name)?;
            let generation = bump_runtime_generation(conn)?;
            insert_activation_history(conn, &package, "remove", &previous, &[], generation)?;
            Ok(generation)
        })
    }

    /// Restores the exact before-image from the latest unconsumed activation event.
    ///
    /// Returns `None` when the package has no activation to roll back. If the active rows no
    /// longer match the event's after-image, rollback fails instead of overwriting divergent
    /// state.
    ///
    /// # Errors
    ///
    /// Returns an error for history divergence, key collisions, invalid persisted data, or a
    /// failed SQLite transaction.
    pub fn rollback(&mut self, package_kind: &str, package_name: &str) -> Result<Option<u64>> {
        validate_package_key(package_kind, package_name)?;
        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            let Some(event) = select_latest_activation(conn, package_kind, package_name)? else {
                return Ok(None);
            };
            let current = select_package_assets(conn, package_kind, package_name)?;
            if current != event.activated_assets {
                return Err(crate::Error::RuntimeHistoryDiverged {
                    package: package_label(package_kind, package_name),
                });
            }

            ensure_no_asset_collisions(conn, &event.package, &event.previous_assets)?;
            replace_package_assets(conn, &event.package, &event.previous_assets)?;
            let generation = bump_runtime_generation(conn)?;
            conn.execute(
                "UPDATE pkg_activation_history SET rolled_back_generation = ?1 WHERE id = ?2",
                params![generation_to_i64(generation)?, event.id],
            )?;
            Ok(Some(generation))
        })
    }

    /// Returns a transactionally coherent generation and active-asset snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the read transaction fails or persisted rows are invalid.
    pub fn snapshot(&mut self) -> Result<RuntimeSnapshot> {
        self.in_transaction("BEGIN DEFERRED", select_runtime_snapshot)
    }

    /// Returns the current runtime generation without loading asset rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails or the stored generation is invalid.
    pub fn generation(&mut self) -> Result<u64> {
        runtime_generation(self.backend.conn())
    }

    /// Imports locally discovered compatibility activations once.
    ///
    /// The marker and all imported rows commit together. Packages already represented in the
    /// runtime table are left untouched, allowing the importer to run alongside newer writers.
    ///
    /// Returns `true` when this call recorded the marker and `false` when another call had already
    /// completed the import.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid activations, collisions, or SQLite transaction failures.
    pub fn import_once(&mut self, marker: &str, activations: &[PackageActivation]) -> Result<bool> {
        if marker.is_empty() {
            return Err(crate::Error::InvalidRuntimeAsset(
                "import marker must not be empty".to_owned(),
            ));
        }
        let mut packages = BTreeSet::new();
        let mut materialized = Vec::with_capacity(activations.len());
        for activation in activations {
            validate_activation(activation)?;
            let key = (
                activation.package.kind.clone(),
                activation.package.name.clone(),
            );
            if !packages.insert(key) {
                return Err(crate::Error::InvalidRuntimeAsset(format!(
                    "duplicate compatibility activation for '{}'",
                    package_label(&activation.package.kind, &activation.package.name)
                )));
            }
            materialized.push((
                activation.package.clone(),
                materialize_activation(activation.clone()),
            ));
        }

        self.in_transaction("BEGIN IMMEDIATE", |conn| {
            ensure_pkg_import_marker_table(conn)?;
            let exists: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
                params![marker],
                |row| row.get(0),
            )?;
            if exists != 0 {
                return Ok(false);
            }

            let mut changes = Vec::new();
            for (package, activated) in &materialized {
                let previous = select_package_assets(conn, &package.kind, &package.name)?;
                if !previous.is_empty() {
                    continue;
                }
                ensure_no_asset_collisions(conn, package, activated)?;
                insert_runtime_assets(conn, activated)?;
                changes.push((package, previous, activated));
            }

            if !changes.is_empty() {
                let generation = bump_runtime_generation(conn)?;
                for (package, previous, activated) in changes {
                    insert_activation_history(
                        conn,
                        package,
                        "legacy-import",
                        &previous,
                        activated,
                        generation,
                    )?;
                }
            }
            conn.execute(
                "INSERT INTO helix_store_import_markers(name) VALUES (?1)",
                params![marker],
            )?;
            Ok(true)
        })
    }

    /// Checks whether a compatibility import marker exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn import_marker_exists(&mut self, marker: &str) -> Result<bool> {
        ensure_pkg_import_marker_table(self.backend.conn())?;
        let exists: i64 = self.backend.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM helix_store_import_markers WHERE name = ?1)",
            params![marker],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Lists activation events for one package in commit order.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or persisted history cannot be decoded.
    pub fn history(
        &mut self,
        package_kind: &str,
        package_name: &str,
    ) -> Result<Vec<ActivationHistory>> {
        let mut statement = self.backend.conn().prepare(
            "SELECT id, package_kind, package_name, package_version, operation, \
             previous_assets_json, activated_assets_json, generation, rolled_back_generation \
             FROM pkg_activation_history WHERE package_kind = ?1 AND package_name = ?2 \
             ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![package_kind, package_name], raw_history_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_history).collect()
    }

    fn in_transaction<T>(
        &mut self,
        begin: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        self.backend.execute_batch(begin)?;
        let result = f(self.backend.conn());
        match result {
            Ok(value) => match self.backend.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(error) => {
                    let _ = self.backend.execute_batch("ROLLBACK");
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.backend.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

pub struct RegistryHeadsRepo<'a> {
    backend: &'a mut DrizzleBackend,
}

impl<'a> RegistryHeadsRepo<'a> {
    pub(crate) fn new(backend: &'a mut DrizzleBackend) -> Self {
        Self { backend }
    }

    /// Inserts or updates one locally observed registry head.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite write fails.
    pub fn upsert(&mut self, head: RegistryHead) -> Result<()> {
        self.backend.conn().execute(
            r#"
INSERT INTO pkg_registry_heads(registry, source, revision, updated_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(registry) DO UPDATE SET
    source = excluded.source,
    revision = excluded.revision,
    updated_at = excluded.updated_at
"#,
            params![head.registry, head.source, head.revision, head.updated_at],
        )?;
        Ok(())
    }

    /// Loads one locally observed registry head.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn get(&mut self, registry: &str) -> Result<Option<RegistryHead>> {
        self.backend
            .conn()
            .query_row(
                "SELECT registry, source, revision, updated_at FROM pkg_registry_heads WHERE registry = ?1",
                params![registry],
                registry_head_from_row,
            )
            .optional()
            .map_err(crate::Error::from)
    }

    /// Lists locally observed registry heads ordered by registry name.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn all(&mut self) -> Result<Vec<RegistryHead>> {
        let mut statement = self.backend.conn().prepare(
            "SELECT registry, source, revision, updated_at FROM pkg_registry_heads ORDER BY registry",
        )?;
        let rows = statement
            .query_map([], registry_head_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deletes one locally observed registry head.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite delete fails.
    pub fn delete(&mut self, registry: &str) -> Result<()> {
        self.backend.conn().execute(
            "DELETE FROM pkg_registry_heads WHERE registry = ?1",
            params![registry],
        )?;
        Ok(())
    }
}

#[derive(Debug)]
struct RawRuntimeAsset {
    package_kind: String,
    package_name: String,
    package_version: String,
    asset_kind: String,
    asset_key: String,
    path: String,
    prefix_args_json: String,
    default_args_json: String,
    env_json: String,
}

#[derive(Debug)]
struct RawActivationHistory {
    id: i64,
    package_kind: String,
    package_name: String,
    package_version: String,
    operation: String,
    previous_assets_json: String,
    activated_assets_json: String,
    generation: i64,
    rolled_back_generation: Option<i64>,
}

fn validate_activation(activation: &PackageActivation) -> Result<()> {
    validate_package_key(&activation.package.kind, &activation.package.name)?;
    if activation.package.version.is_empty() {
        return Err(crate::Error::InvalidRuntimeAsset(format!(
            "package '{}' has an empty version",
            package_label(&activation.package.kind, &activation.package.name)
        )));
    }
    if activation.assets.is_empty() {
        return Err(crate::Error::InvalidRuntimeAsset(format!(
            "package '{}' activation has no assets; use remove instead",
            package_label(&activation.package.kind, &activation.package.name)
        )));
    }

    let mut keys = BTreeSet::new();
    for asset in &activation.assets {
        if asset.key.is_empty() {
            return Err(crate::Error::InvalidRuntimeAsset(format!(
                "package '{}' has an empty {} asset key",
                package_label(&activation.package.kind, &activation.package.name),
                asset.kind
            )));
        }
        if asset.path.as_os_str().is_empty() {
            return Err(crate::Error::InvalidRuntimeAsset(format!(
                "{} asset '{}' has an empty path",
                asset.kind, asset.key
            )));
        }
        if asset.path.to_str().is_none() {
            return Err(crate::Error::InvalidRuntimeAsset(format!(
                "{} asset '{}' path is not valid UTF-8",
                asset.kind, asset.key
            )));
        }
        if !keys.insert((asset.kind, asset.key.as_str())) {
            return Err(crate::Error::RuntimeAssetCollision {
                asset_kind: asset.kind.to_string(),
                asset_key: asset.key.clone(),
                existing_package: package_label(&activation.package.kind, &activation.package.name),
                requested_package: package_label(
                    &activation.package.kind,
                    &activation.package.name,
                ),
            });
        }
    }
    Ok(())
}

fn validate_package_key(package_kind: &str, package_name: &str) -> Result<()> {
    if package_kind.is_empty() || package_name.is_empty() {
        return Err(crate::Error::InvalidRuntimeAsset(
            "package kind and name must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn validate_receipt_package(receipt: &PkgReceipt, package: &ActivePackage) -> Result<()> {
    if receipt.kind == package.kind
        && receipt.name == package.name
        && receipt.version == package.version
    {
        return Ok(());
    }
    Err(crate::Error::InvalidPackageState(format!(
        "receipt '{}/{}@{}' does not match runtime package '{}/{}@{}'",
        receipt.kind, receipt.name, receipt.version, package.kind, package.name, package.version
    )))
}

fn package_from_assets(
    package_kind: &str,
    package_name: &str,
    assets: &[RuntimeAsset],
) -> Result<Option<ActivePackage>> {
    let Some(package) = assets.first().map(|asset| asset.package.clone()) else {
        return Ok(None);
    };
    if package.kind != package_kind
        || package.name != package_name
        || assets.iter().any(|asset| asset.package != package)
    {
        return Err(crate::Error::InvalidPackageState(format!(
            "active assets for '{}' contain inconsistent package identities",
            package_label(package_kind, package_name)
        )));
    }
    Ok(Some(package))
}

fn materialize_activation(mut activation: PackageActivation) -> Vec<RuntimeAsset> {
    let package = activation.package;
    let mut assets = activation
        .assets
        .drain(..)
        .map(|asset| RuntimeAsset::from_spec(package.clone(), asset))
        .collect::<Vec<_>>();
    sort_runtime_assets(&mut assets);
    assets
}

fn sort_runtime_assets(assets: &mut [RuntimeAsset]) {
    assets.sort_by(|left, right| {
        (left.kind, left.key.as_str()).cmp(&(right.kind, right.key.as_str()))
    });
}

fn ensure_no_asset_collisions(
    conn: &rusqlite::Connection,
    package: &ActivePackage,
    assets: &[RuntimeAsset],
) -> Result<()> {
    for asset in assets {
        let existing = conn
            .query_row(
                "SELECT package_kind, package_name FROM pkg_runtime_assets \
                 WHERE asset_kind = ?1 AND asset_key = ?2",
                params![asset.kind.as_str(), asset.key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((existing_kind, existing_name)) = existing {
            if existing_kind != package.kind || existing_name != package.name {
                return Err(crate::Error::RuntimeAssetCollision {
                    asset_kind: asset.kind.to_string(),
                    asset_key: asset.key.clone(),
                    existing_package: package_label(&existing_kind, &existing_name),
                    requested_package: package_label(&package.kind, &package.name),
                });
            }
        }
    }
    Ok(())
}

fn replace_package_assets(
    conn: &rusqlite::Connection,
    package: &ActivePackage,
    assets: &[RuntimeAsset],
) -> Result<()> {
    delete_package_assets(conn, &package.kind, &package.name)?;
    insert_runtime_assets(conn, assets)
}

fn delete_package_assets(
    conn: &rusqlite::Connection,
    package_kind: &str,
    package_name: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM pkg_runtime_assets WHERE package_kind = ?1 AND package_name = ?2",
        params![package_kind, package_name],
    )?;
    Ok(())
}

fn insert_runtime_assets(conn: &rusqlite::Connection, assets: &[RuntimeAsset]) -> Result<()> {
    for asset in assets {
        let path = asset.path.to_str().ok_or_else(|| {
            crate::Error::InvalidRuntimeAsset(format!(
                "{} asset '{}' path is not valid UTF-8",
                asset.kind, asset.key
            ))
        })?;
        conn.execute(
            r#"
INSERT INTO pkg_runtime_assets(
    asset_kind,
    asset_key,
    package_kind,
    package_name,
    package_version,
    path,
    prefix_args_json,
    default_args_json,
    env_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
"#,
            params![
                asset.kind.as_str(),
                asset.key,
                asset.package.kind,
                asset.package.name,
                asset.package.version,
                path,
                serde_json::to_string(&asset.prefix_args)?,
                serde_json::to_string(&asset.default_args)?,
                serde_json::to_string(&asset.env)?,
            ],
        )?;
    }
    Ok(())
}

fn select_package_assets(
    conn: &rusqlite::Connection,
    package_kind: &str,
    package_name: &str,
) -> Result<Vec<RuntimeAsset>> {
    select_runtime_assets(
        conn,
        "SELECT package_kind, package_name, package_version, asset_kind, asset_key, path, \
         prefix_args_json, default_args_json, env_json FROM pkg_runtime_assets \
         WHERE package_kind = ?1 AND package_name = ?2 ORDER BY asset_kind, asset_key",
        params![package_kind, package_name],
    )
}

fn select_all_runtime_assets(conn: &rusqlite::Connection) -> Result<Vec<RuntimeAsset>> {
    select_runtime_assets(
        conn,
        "SELECT package_kind, package_name, package_version, asset_kind, asset_key, path, \
         prefix_args_json, default_args_json, env_json FROM pkg_runtime_assets \
         ORDER BY asset_kind, asset_key",
        [],
    )
}

fn select_runtime_snapshot(conn: &rusqlite::Connection) -> Result<RuntimeSnapshot> {
    let generation = runtime_generation(conn)?;
    let assets = select_all_runtime_assets(conn)?;
    Ok(RuntimeSnapshot { generation, assets })
}

fn select_package_state(
    conn: &rusqlite::Connection,
    package_kind: &str,
    package_name: &str,
) -> Result<PackageState> {
    let receipt = conn
        .query_row(
            PKG_RECEIPT_SELECT_SQL,
            params![package_kind, package_name],
            pkg_receipt_from_row,
        )
        .optional()?;
    let assets = select_package_assets(conn, package_kind, package_name)?;
    Ok(PackageState {
        package_kind: package_kind.to_owned(),
        package_name: package_name.to_owned(),
        receipt,
        assets,
    })
}

fn package_state_commit(
    conn: &rusqlite::Connection,
    before: PackageState,
) -> Result<PackageStateCommit> {
    let after = select_package_state(conn, &before.package_kind, &before.package_name)?;
    let snapshot = select_runtime_snapshot(conn)?;
    Ok(PackageStateCommit {
        before,
        after,
        snapshot,
    })
}

fn select_runtime_assets<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
) -> Result<Vec<RuntimeAsset>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement
        .query_map(params, raw_runtime_asset_from_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter().map(decode_runtime_asset).collect()
}

fn raw_runtime_asset_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRuntimeAsset> {
    Ok(RawRuntimeAsset {
        package_kind: row.get(0)?,
        package_name: row.get(1)?,
        package_version: row.get(2)?,
        asset_kind: row.get(3)?,
        asset_key: row.get(4)?,
        path: row.get(5)?,
        prefix_args_json: row.get(6)?,
        default_args_json: row.get(7)?,
        env_json: row.get(8)?,
    })
}

fn decode_runtime_asset(raw: RawRuntimeAsset) -> Result<RuntimeAsset> {
    let kind = RuntimeAssetKind::from_db(&raw.asset_kind)
        .ok_or_else(|| crate::Error::UnknownRuntimeAssetKind(raw.asset_kind.clone()))?;
    Ok(RuntimeAsset {
        package: ActivePackage::new(raw.package_kind, raw.package_name, raw.package_version),
        kind,
        key: raw.asset_key,
        path: PathBuf::from(raw.path),
        prefix_args: serde_json::from_str(&raw.prefix_args_json)?,
        default_args: serde_json::from_str(&raw.default_args_json)?,
        env: serde_json::from_str(&raw.env_json)?,
    })
}

fn insert_activation_history(
    conn: &rusqlite::Connection,
    package: &ActivePackage,
    operation: &str,
    previous_assets: &[RuntimeAsset],
    activated_assets: &[RuntimeAsset],
    generation: u64,
) -> Result<()> {
    conn.execute(
        r#"
INSERT INTO pkg_activation_history(
    package_kind,
    package_name,
    package_version,
    operation,
    previous_assets_json,
    activated_assets_json,
    generation
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#,
        params![
            package.kind,
            package.name,
            package.version,
            operation,
            serde_json::to_string(previous_assets)?,
            serde_json::to_string(activated_assets)?,
            generation_to_i64(generation)?,
        ],
    )?;
    Ok(())
}

fn select_latest_activation(
    conn: &rusqlite::Connection,
    package_kind: &str,
    package_name: &str,
) -> Result<Option<ActivationHistory>> {
    conn.query_row(
        "SELECT id, package_kind, package_name, package_version, operation, \
         previous_assets_json, activated_assets_json, generation, rolled_back_generation \
         FROM pkg_activation_history WHERE package_kind = ?1 AND package_name = ?2 \
         AND rolled_back_generation IS NULL ORDER BY id DESC LIMIT 1",
        params![package_kind, package_name],
        raw_history_from_row,
    )
    .optional()?
    .map(decode_history)
    .transpose()
}

fn raw_history_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawActivationHistory> {
    Ok(RawActivationHistory {
        id: row.get(0)?,
        package_kind: row.get(1)?,
        package_name: row.get(2)?,
        package_version: row.get(3)?,
        operation: row.get(4)?,
        previous_assets_json: row.get(5)?,
        activated_assets_json: row.get(6)?,
        generation: row.get(7)?,
        rolled_back_generation: row.get(8)?,
    })
}

fn decode_history(raw: RawActivationHistory) -> Result<ActivationHistory> {
    Ok(ActivationHistory {
        id: raw.id,
        package: ActivePackage::new(raw.package_kind, raw.package_name, raw.package_version),
        operation: raw.operation,
        previous_assets: serde_json::from_str(&raw.previous_assets_json)?,
        activated_assets: serde_json::from_str(&raw.activated_assets_json)?,
        generation: generation_from_i64(raw.generation)?,
        rolled_back_generation: raw
            .rolled_back_generation
            .map(generation_from_i64)
            .transpose()?,
    })
}

fn runtime_generation(conn: &rusqlite::Connection) -> Result<u64> {
    let generation = conn.query_row(
        "SELECT runtime_generation FROM pkg_runtime_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    generation_from_i64(generation)
}

fn bump_runtime_generation(conn: &rusqlite::Connection) -> Result<u64> {
    conn.execute(
        "UPDATE pkg_runtime_meta SET runtime_generation = runtime_generation + 1 WHERE singleton = 1",
        [],
    )?;
    runtime_generation(conn)
}

fn generation_from_i64(generation: i64) -> Result<u64> {
    u64::try_from(generation).map_err(|_| crate::Error::InvalidRuntimeGeneration(generation))
}

fn generation_to_i64(generation: u64) -> Result<i64> {
    i64::try_from(generation).map_err(|_| {
        crate::Error::InvalidRuntimeAsset(format!(
            "runtime generation {generation} exceeds SQLite's integer range"
        ))
    })
}

fn registry_head_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegistryHead> {
    Ok(RegistryHead {
        registry: row.get(0)?,
        source: row.get(1)?,
        revision: row.get(2)?,
        updated_at: row.get(3)?,
    })
}

fn package_label(package_kind: &str, package_name: &str) -> String {
    format!("{package_kind}/{package_name}")
}

impl From<SelectAssistantThreads> for AssistantThread {
    fn from(row: SelectAssistantThreads) -> Self {
        Self {
            id: row.id,
            scope: row.scope,
            title: row.title,
            created_at: row.created_at,
            updated_at: row.updated_at,
            rating: row.rating,
            has_feedback: row.has_feedback != 0,
            record_json: row.record_json,
        }
    }
}

impl TryFrom<SelectAssistantLayout> for AssistantLayout {
    type Error = crate::Error;

    fn try_from(row: SelectAssistantLayout) -> Result<Self> {
        Ok(Self {
            scope: row.scope,
            open_ids: serde_json::from_str(&row.open_ids)?,
            active_id: row.active_id,
        })
    }
}

impl From<SelectAssistantPermissions> for AssistantPermission {
    fn from(row: SelectAssistantPermissions) -> Self {
        Self {
            agent: row.agent,
            tool: row.tool,
            choice: row.choice,
        }
    }
}

impl From<SelectFrecency> for FrecencyEntry {
    fn from(row: SelectFrecency) -> Self {
        Self {
            workspace: row.workspace,
            path_hash: row.path_hash,
            first_accessed_at: row.first_accessed_at,
            last_accessed_at: row.last_accessed_at,
            access_count: row.access_count,
            timestamps_json: row.timestamps_json,
        }
    }
}

impl From<SelectQueryHistory> for QueryHistory {
    fn from(row: SelectQueryHistory) -> Self {
        Self {
            id: row.id,
            workspace: row.workspace,
            query: row.query,
            opened_path: row.opened_path,
            ts: row.ts,
        }
    }
}

impl From<SelectPkgReceipts> for PkgReceipt {
    fn from(row: SelectPkgReceipts) -> Self {
        Self {
            kind: row.kind,
            name: row.name,
            version: row.version,
            source: row.source,
            hash: row.hash,
            bin: row.bin,
            shim: row.shim,
            files_json: row.files_json,
            installed_at: row.installed_at,
            native_manager: row.native_manager,
            native_id: row.native_id,
            receipt_json: row.receipt_json,
        }
    }
}

const PKG_IMPORT_MARKERS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS helix_store_import_markers (
    name TEXT PRIMARY KEY NOT NULL,
    imported_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#;

const PKG_RECEIPT_SELECT_SQL: &str = "SELECT kind, name, version, source, hash, bin, shim, files_json, installed_at, native_manager, native_id, receipt_json FROM pkg_receipts WHERE kind = ?1 AND name = ?2";
const PKG_RECEIPT_SELECT_ALL_SQL: &str = "SELECT kind, name, version, source, hash, bin, shim, files_json, installed_at, native_manager, native_id, receipt_json FROM pkg_receipts ORDER BY kind, name";

fn ensure_pkg_import_marker_table(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(PKG_IMPORT_MARKERS_SQL)
        .map_err(crate::Error::from)
}

fn insert_pkg_receipt(conn: &rusqlite::Connection, receipt: &PkgReceipt) -> Result<usize> {
    conn.execute(
        r#"
INSERT INTO pkg_receipts (
    kind,
    name,
    version,
    source,
    hash,
    bin,
    shim,
    files_json,
    installed_at,
    native_manager,
    native_id,
    receipt_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
ON CONFLICT(kind, name) DO UPDATE SET
    version = excluded.version,
    source = excluded.source,
    hash = excluded.hash,
    bin = excluded.bin,
    shim = excluded.shim,
    files_json = excluded.files_json,
    installed_at = excluded.installed_at,
    native_manager = excluded.native_manager,
    native_id = excluded.native_id,
    receipt_json = excluded.receipt_json
"#,
        params![
            &receipt.kind,
            &receipt.name,
            &receipt.version,
            &receipt.source,
            &receipt.hash,
            &receipt.bin,
            &receipt.shim,
            &receipt.files_json,
            &receipt.installed_at,
            &receipt.native_manager,
            &receipt.native_id,
            &receipt.receipt_json,
        ],
    )
    .map_err(crate::Error::from)
}

fn delete_pkg_receipt(
    conn: &rusqlite::Connection,
    package_kind: &str,
    package_name: &str,
) -> Result<usize> {
    conn.execute(
        "DELETE FROM pkg_receipts WHERE kind = ?1 AND name = ?2",
        params![package_kind, package_name],
    )
    .map_err(crate::Error::from)
}

fn pkg_receipt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PkgReceipt> {
    Ok(PkgReceipt {
        kind: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        source: row.get(3)?,
        hash: row.get(4)?,
        bin: row.get(5)?,
        shim: row.get(6)?,
        files_json: row.get(7)?,
        installed_at: row.get(8)?,
        native_manager: row.get(9)?,
        native_id: row.get(10)?,
        receipt_json: row.get(11)?,
    })
}

fn bool_to_i64(value: bool) -> i64 {
    i64::from(value)
}

const ASSISTANT_IMPORT_MARKER: &str = "assistant-state-v1";

fn ensure_assistant_import_marker_table(backend: &mut DrizzleBackend) -> Result<()> {
    backend.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS helix_store_import_markers (
    name TEXT PRIMARY KEY NOT NULL,
    imported_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#,
    )
}

fn select_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssistantThread> {
    Ok(AssistantThread {
        id: row.get(0)?,
        scope: row.get(1)?,
        title: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        rating: row.get(5)?,
        has_feedback: row.get::<_, i64>(6)? != 0,
        record_json: row.get(7)?,
    })
}

fn append_timestamp(raw: &str, ts: i64) -> Result<String> {
    let mut values: Vec<i64> = serde_json::from_str(raw)?;
    values.push(ts);
    Ok(serde_json::to_string(&values)?)
}

const FFF_CACHE_IMPORT_MARKER_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS helix_store_import_markers (
    name TEXT PRIMARY KEY NOT NULL,
    imported_at INTEGER NOT NULL DEFAULT (unixepoch())
);
"#;
const MAX_QUERY_HISTORY_ENTRIES: usize = 128;

fn ensure_cache_import_marker_table(backend: &mut DrizzleBackend) -> Result<()> {
    backend.execute_batch(FFF_CACHE_IMPORT_MARKER_SQL)
}

fn insert_frecency(conn: &rusqlite::Connection, entry: &FrecencyEntry) -> Result<usize> {
    conn.execute(
        r#"
INSERT INTO frecency (
    workspace,
    path_hash,
    first_accessed_at,
    last_accessed_at,
    access_count,
    timestamps_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(workspace, path_hash) DO UPDATE SET
    first_accessed_at = excluded.first_accessed_at,
    last_accessed_at = excluded.last_accessed_at,
    access_count = excluded.access_count,
    timestamps_json = excluded.timestamps_json
"#,
        params![
            &entry.workspace,
            &entry.path_hash,
            &entry.first_accessed_at,
            &entry.last_accessed_at,
            &entry.access_count,
            &entry.timestamps_json,
        ],
    )
    .map_err(crate::Error::from)
}

fn insert_query_history(conn: &rusqlite::Connection, item: &QueryHistory) -> Result<usize> {
    conn.execute(
        r#"
INSERT INTO query_history (
    id,
    workspace,
    query,
    opened_path,
    ts
) VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(id) DO UPDATE SET
    workspace = excluded.workspace,
    query = excluded.query,
    opened_path = excluded.opened_path,
    ts = excluded.ts
"#,
        params![
            &item.id,
            &item.workspace,
            &item.query,
            &item.opened_path,
            &item.ts,
        ],
    )
    .map_err(crate::Error::from)
}

fn prune_history(
    conn: &rusqlite::Connection,
    workspace: &str,
    kind: &str,
    max_entries: usize,
) -> Result<usize> {
    conn.execute(
        "DELETE FROM query_history \
         WHERE workspace = ?1 AND opened_path = ?2 AND id NOT IN ( \
             SELECT id FROM query_history \
             WHERE workspace = ?1 AND opened_path = ?2 \
             ORDER BY ts DESC, id DESC LIMIT ?3 \
         )",
        params![workspace, history_marker(kind), max_entries as i64],
    )
    .map_err(crate::Error::from)
}

fn query_match_id(workspace: &str, query: &str) -> String {
    format!("fff:assoc:{:016x}", stable_hash(&[workspace, query]))
}

fn query_history_id(workspace: &str, kind: &str, query: &str, ts: i64) -> String {
    let ts_string = ts.to_string();
    format!(
        "fff:history:{kind}:{ts}:{:016x}",
        stable_hash(&[workspace, kind, query, &ts_string])
    )
}

fn history_marker(kind: &str) -> String {
    format!("fff:history:{kind}")
}

fn stable_hash(parts: &[&str]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;

    let mut hash = FNV_OFFSET;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod runtime_asset_transaction_tests {
    use super::*;
    use crate::{DatabaseKind, Error};

    #[test]
    fn failed_commit_rolls_back_assets_history_and_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut backend =
            DrizzleBackend::open(temp.path().join("state.sqlite3"), DatabaseKind::State)
                .expect("open state database");
        backend
            .execute_batch(
                r#"
CREATE TABLE commit_failure_parent(id INTEGER PRIMARY KEY);
CREATE TABLE commit_failure_child(
    parent_id INTEGER NOT NULL,
    FOREIGN KEY(parent_id) REFERENCES commit_failure_parent(id)
        DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER fail_runtime_asset_commit
AFTER INSERT ON pkg_runtime_assets
BEGIN
    INSERT INTO commit_failure_child(parent_id) VALUES (999);
END;
"#,
            )
            .unwrap();

        let result = RuntimeAssetsRepo::new(&mut backend).activate(PackageActivation::new(
            ActivePackage::new("lsp", "demo", "1"),
            vec![crate::RuntimeAssetSpec::command(
                "demo",
                temp.path().join("demo"),
            )],
        ));
        assert!(matches!(result, Err(Error::Sqlite(_))));

        let generation: i64 = backend
            .conn()
            .query_row(
                "SELECT runtime_generation FROM pkg_runtime_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let assets: i64 = backend
            .conn()
            .query_row("SELECT COUNT(*) FROM pkg_runtime_assets", [], |row| {
                row.get(0)
            })
            .unwrap();
        let history: i64 = backend
            .conn()
            .query_row("SELECT COUNT(*) FROM pkg_activation_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        let deferred_rows: i64 = backend
            .conn()
            .query_row("SELECT COUNT(*) FROM commit_failure_child", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((generation, assets, history, deferred_rows), (0, 0, 0, 0));

        backend
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .expect("connection should not retain the failed transaction");
    }
}
