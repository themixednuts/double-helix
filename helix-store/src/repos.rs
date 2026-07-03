use drizzle::core::desc;
use drizzle::core::expr::{and, eq};
use drizzle::sqlite::connection::SQLiteTransactionType;
use rusqlite::{params, OptionalExtension};

use crate::backend::{Backend, DrizzleBackend};
use crate::dto::{
    AssistantLayout, AssistantPermission, AssistantThread, FrecencyEntry, PkgReceipt, QueryHistory,
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
