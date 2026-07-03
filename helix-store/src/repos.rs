use drizzle::core::expr::{and, eq};
use drizzle::core::{asc, desc};
use drizzle::sqlite::connection::SQLiteTransactionType;

use crate::backend::DrizzleBackend;
use crate::dto::{
    AssistantLayout, AssistantPermission, AssistantThread, FrecencyEntry, PkgReceipt, QueryHistory,
};
use crate::error::Result;
use crate::schema::{
    InsertAssistantLayout, InsertAssistantPermissions, InsertAssistantThreads, InsertFrecency,
    InsertPkgReceipts, InsertQueryHistory, SelectAssistantLayout, SelectAssistantPermissions,
    SelectAssistantThreads, SelectFrecency, SelectPkgReceipts, SelectQueryHistory,
    UpdateAssistantPermissions, UpdateFrecency, UpdatePkgReceipts,
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
        let table = self.backend.schema.pkg_receipts;
        self.backend
            .db()
            .transaction(SQLiteTransactionType::Immediate, |tx| {
                tx.insert(table)
                    .value(InsertPkgReceipts::new(
                        receipt.kind.clone(),
                        receipt.name.clone(),
                        receipt.version.clone(),
                        receipt.source.clone(),
                        receipt.hash.clone(),
                        receipt.installed_at.clone(),
                        receipt.receipt_json.clone(),
                    ))
                    .execute()
                    .or_else(|_| {
                        tx.update(table)
                            .set(
                                UpdatePkgReceipts::default()
                                    .with_version(receipt.version)
                                    .with_source(receipt.source)
                                    .with_hash(receipt.hash)
                                    .with_installed_at(receipt.installed_at)
                                    .with_receipt_json(receipt.receipt_json),
                            )
                            .r#where(and(
                                eq(table.kind, receipt.kind),
                                eq(table.name, receipt.name),
                            ))
                            .execute()
                    })?;
                Ok(())
            })?;
        Ok(())
    }

    /// Lists all package receipts ordered by kind/name.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    pub fn all(&mut self) -> Result<Vec<PkgReceipt>> {
        let table = self.backend.schema.pkg_receipts;
        let rows: Vec<SelectPkgReceipts> = self
            .backend
            .db()
            .select(())
            .from(table)
            .order_by((asc(table.kind), asc(table.name)))
            .all()?;
        Ok(rows.into_iter().map(PkgReceipt::from).collect())
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
            installed_at: row.installed_at,
            receipt_json: row.receipt_json,
        }
    }
}

fn bool_to_i64(value: bool) -> i64 {
    i64::from(value)
}

fn append_timestamp(raw: &str, ts: i64) -> Result<String> {
    let mut values: Vec<i64> = serde_json::from_str(raw)?;
    values.push(ts);
    Ok(serde_json::to_string(&values)?)
}
