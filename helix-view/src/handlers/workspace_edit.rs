use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::{access::DynAccess, ArcSwap};
use helix_core::{syntax, Rope, Transaction, Uri};
use helix_lsp::util::generate_transaction_from_edits;
use helix_lsp::{lsp, OffsetEncoding};

use crate::{
    document::LanguageInitialization,
    editor::{
        Action, Config, FileOperation, FileOperationDeleteMode, FileOperationDestination,
        FileOperationOrigin, FileOperationRequest,
    },
    Document, DocumentId, Editor,
};

#[derive(Debug)]
pub struct ApplyEditError {
    pub kind: ApplyEditErrorKind,
    pub failed_change_idx: usize,
}

impl ApplyEditError {
    pub fn worker_failed(message: String) -> Self {
        Self {
            kind: ApplyEditErrorKind::IoError(std::io::Error::other(message)),
            failed_change_idx: 0,
        }
    }
}

#[derive(Debug)]
pub enum ApplyEditErrorKind {
    DocumentChanged,
    FileNotFound,
    InvalidEdit,
    InvalidUrl(helix_core::uri::UrlConversionError),
    IoError(std::io::Error),
}

/// Text edits are applied on the editor main loop. Resource operations are
/// deliberately returned to the terminal FIFO, where filesystem work and LSP
/// file-operation notifications can be serialized asynchronously.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct WorkspaceEditResult {
    pub file_operations: Vec<WorkspaceEditFileOperation>,
}

#[cfg(test)]
#[derive(Debug)]
pub struct WorkspaceEditFileOperation {
    pub request: FileOperationRequest,
    pub failed_change_idx: usize,
}

impl From<std::io::Error> for ApplyEditErrorKind {
    fn from(err: std::io::Error) -> Self {
        ApplyEditErrorKind::IoError(err)
    }
}

impl From<helix_core::uri::UrlConversionError> for ApplyEditErrorKind {
    fn from(err: helix_core::uri::UrlConversionError) -> Self {
        ApplyEditErrorKind::InvalidUrl(err)
    }
}

impl Display for ApplyEditErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyEditErrorKind::DocumentChanged => f.write_str("document has changed"),
            ApplyEditErrorKind::FileNotFound => f.write_str("file not found"),
            ApplyEditErrorKind::InvalidEdit => f.write_str("invalid workspace edit"),
            ApplyEditErrorKind::InvalidUrl(err) => f.write_str(&format!("{err}")),
            ApplyEditErrorKind::IoError(err) => f.write_str(&format!("{err}")),
        }
    }
}

/// Immutable UI-thread snapshot handed to one blocking workspace-edit planner.
/// Closed files are opened only by [`WorkspaceEditPreparation::execute`].
pub struct WorkspaceEditPreparation {
    workspace_edit: lsp::WorkspaceEdit,
    offset_encoding: OffsetEncoding,
    documents: HashMap<PathBuf, WorkspaceEditDocumentSnapshot>,
    config: Arc<dyn DynAccess<Config> + Send + Sync>,
    syn_loader: Arc<ArcSwap<syntax::Loader>>,
}

impl WorkspaceEditPreparation {
    pub fn execute(self) -> Result<WorkspaceEditPlan, ApplyEditError> {
        WorkspaceEditPlanner {
            documents: self.documents,
            config: self.config,
            syn_loader: self.syn_loader,
        }
        .plan(self.workspace_edit, self.offset_encoding)
    }
}

#[derive(Clone, Debug)]
struct WorkspaceEditDocumentSnapshot {
    doc_id: DocumentId,
    text: Rope,
    version: i32,
}

#[derive(Debug)]
struct PreparedTextDocumentEdit {
    doc_id: Option<DocumentId>,
    path: PathBuf,
    initial_text: Rope,
    transaction: Transaction,
    snapshot_version: Option<i32>,
}

#[derive(Clone, Debug)]
struct PlannedDocumentState {
    doc_id: Option<DocumentId>,
    text: Rope,
    snapshot_version: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedPathKind {
    Missing,
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct PlannedPathState {
    kind: PlannedPathKind,
    source_path: Option<PathBuf>,
}

#[derive(Debug)]
enum PreparedWorkspaceOperation {
    TextEdit(PreparedTextDocumentEdit),
    ResourceOp(lsp::ResourceOp),
}

/// A worker-produced, validated WorkspaceEdit plan. It contains no filesystem
/// work; applying it only validates the captured document versions and applies
/// transactions on the editor main loop.
#[derive(Debug, Default)]
pub struct WorkspaceEditPlan {
    planned_docs: HashMap<PathBuf, PlannedDocumentState>,
    planned_paths: HashMap<PathBuf, PlannedPathState>,
    operations: Vec<(usize, PreparedWorkspaceOperation)>,
}

/// Main-thread execution cursor for a worker-prepared workspace edit. It
/// retains the original `documentChanges` order and records document versions
/// created by earlier steps so later steps accept only the execution's own
/// edits.
#[derive(Debug)]
pub struct WorkspaceEditExecution {
    operations: Vec<(usize, PreparedWorkspaceOperation)>,
    cursor: usize,
    expected_versions: HashMap<DocumentId, i32>,
}

#[derive(Debug)]
pub enum WorkspaceEditExecutionStep {
    Resource {
        member: usize,
        failed_change_idx: usize,
        request: FileOperationRequest,
    },
    Complete,
}

impl WorkspaceEditPlan {
    fn push_text_edit(&mut self, index: usize, edit: PreparedTextDocumentEdit) {
        self.operations
            .push((index, PreparedWorkspaceOperation::TextEdit(edit)));
    }

    fn push_resource_op(&mut self, index: usize, op: lsp::ResourceOp) {
        self.operations
            .push((index, PreparedWorkspaceOperation::ResourceOp(op)));
    }

    #[cfg(test)]
    fn into_operations(self) -> Vec<(usize, PreparedWorkspaceOperation)> {
        self.operations
    }

    pub fn into_execution(self) -> WorkspaceEditExecution {
        WorkspaceEditExecution {
            operations: self.operations,
            cursor: 0,
            expected_versions: HashMap::new(),
        }
    }
}

struct WorkspaceEditPlanner {
    documents: HashMap<PathBuf, WorkspaceEditDocumentSnapshot>,
    config: Arc<dyn DynAccess<Config> + Send + Sync>,
    syn_loader: Arc<ArcSwap<syntax::Loader>>,
}

impl WorkspaceEditPlanner {
    fn plan(
        &self,
        workspace_edit: lsp::WorkspaceEdit,
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        if let Some(document_changes) = workspace_edit.document_changes {
            return match document_changes {
                lsp::DocumentChanges::Edits(document_edits) => {
                    self.plan_document_change_edits(&document_edits, offset_encoding)
                }
                lsp::DocumentChanges::Operations(operations) => {
                    log::debug!("document changes - operations: {:?}", operations);
                    self.plan_document_operations(&operations, offset_encoding)
                }
            };
        }

        if let Some(changes) = workspace_edit.changes {
            log::debug!("workspace changes: {:?}", changes);
            return self.plan_workspace_changes(&changes, offset_encoding);
        }

        Ok(WorkspaceEditPlan::default())
    }

    fn workspace_edit_path(url: &helix_lsp::Url) -> Result<PathBuf, ApplyEditErrorKind> {
        let uri = Uri::try_from(url).map_err(ApplyEditErrorKind::InvalidUrl)?;
        Ok(helix_stdx::path::canonicalize(
            uri.as_path().expect("URIs are valid paths"),
        ))
    }

    fn load_workspace_edit_snapshot(
        &self,
        path: &Path,
    ) -> Result<PlannedDocumentState, ApplyEditErrorKind> {
        if let Some(snapshot) = self.documents.get(path) {
            return Ok(PlannedDocumentState {
                doc_id: Some(snapshot.doc_id),
                text: snapshot.text.clone(),
                snapshot_version: Some(snapshot.version),
            });
        }

        let document = Document::open(
            path,
            None,
            LanguageInitialization::Full,
            self.config.clone(),
            self.syn_loader.clone(),
        )
        .map_err(|err| {
            log::error!("failed to open document: {}: {}", path.display(), err);
            ApplyEditErrorKind::FileNotFound
        })?;

        Ok(PlannedDocumentState {
            doc_id: None,
            text: document.text().clone(),
            snapshot_version: None,
        })
    }

    fn current_path_state(&self, path: &Path) -> PlannedPathState {
        let path = helix_stdx::path::canonicalize(path);
        if self.documents.contains_key(&path) {
            return PlannedPathState {
                kind: PlannedPathKind::File,
                source_path: Some(path),
            };
        }

        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => PlannedPathState {
                kind: PlannedPathKind::File,
                source_path: Some(path),
            },
            Ok(metadata) if metadata.is_dir() => PlannedPathState {
                kind: PlannedPathKind::Directory,
                source_path: None,
            },
            _ => PlannedPathState {
                kind: PlannedPathKind::Missing,
                source_path: None,
            },
        }
    }

    fn planned_path_state(
        &self,
        planned_paths: &HashMap<PathBuf, PlannedPathState>,
        path: &Path,
    ) -> PlannedPathState {
        if let Some(state) = planned_paths.get(path) {
            return state.clone();
        }

        let inherited = planned_paths
            .iter()
            .filter_map(|(planned_root, state)| {
                path.strip_prefix(planned_root)
                    .ok()
                    .map(|suffix| (planned_root, suffix, state))
            })
            .max_by_key(|(planned_root, _, _)| planned_root.as_os_str().len());

        if let Some((planned_root, suffix, state)) = inherited {
            if state.kind == PlannedPathKind::Missing {
                return PlannedPathState {
                    kind: PlannedPathKind::Missing,
                    source_path: None,
                };
            }

            if suffix.as_os_str().is_empty() {
                return state.clone();
            }

            if let Some(source_root) = &state.source_path {
                let source_path = source_root.join(suffix);
                let source_state = self.current_path_state(&source_path);
                return PlannedPathState {
                    kind: source_state.kind,
                    source_path: Some(source_path),
                };
            }

            if state.kind == PlannedPathKind::Directory {
                return PlannedPathState {
                    kind: PlannedPathKind::Missing,
                    source_path: None,
                };
            }

            let _ = planned_root;
        }

        self.current_path_state(path)
    }

    fn remap_planned_document_subtree(
        planned_docs: &mut HashMap<PathBuf, PlannedDocumentState>,
        from: &Path,
        to: &Path,
    ) {
        let moved = planned_docs
            .drain()
            .map(|(path, state)| match path.strip_prefix(from) {
                Ok(suffix) => (to.join(suffix), state),
                Err(_) => (path, state),
            })
            .collect();
        *planned_docs = moved;
    }

    fn remove_planned_document_subtree(
        planned_docs: &mut HashMap<PathBuf, PlannedDocumentState>,
        root: &Path,
    ) {
        planned_docs.retain(|path, _| !path.starts_with(root));
    }

    fn remap_planned_path_subtree(
        planned_paths: &mut HashMap<PathBuf, PlannedPathState>,
        from: &Path,
        to: &Path,
    ) {
        let moved = planned_paths
            .drain()
            .map(|(path, state)| match path.strip_prefix(from) {
                Ok(suffix) => (to.join(suffix), state),
                Err(_) => (path, state),
            })
            .collect();
        *planned_paths = moved;
    }

    fn remove_planned_path_subtree(
        planned_paths: &mut HashMap<PathBuf, PlannedPathState>,
        root: &Path,
    ) {
        planned_paths.retain(|path, _| !path.starts_with(root));
    }

    fn ensure_planned_document_state(
        &self,
        planned_docs: &mut HashMap<PathBuf, PlannedDocumentState>,
        planned_paths: &HashMap<PathBuf, PlannedPathState>,
        path: &Path,
    ) -> Result<(), ApplyEditErrorKind> {
        if planned_docs.contains_key(path) {
            return Ok(());
        }

        self.validate_file_parent(planned_paths, path)?;
        if Self::path_blocked_by_plan(planned_paths, path) {
            return Err(ApplyEditErrorKind::InvalidEdit);
        }

        let path_state = self.planned_path_state(planned_paths, path);
        if path_state.kind == PlannedPathKind::Directory {
            return Err(ApplyEditErrorKind::InvalidEdit);
        }

        let source_path = path_state.source_path.unwrap_or_else(|| path.to_path_buf());
        let planned_doc = self.load_workspace_edit_snapshot(&source_path)?;
        planned_docs.insert(path.to_path_buf(), planned_doc);
        Ok(())
    }

    fn validate_file_parent(
        &self,
        planned_paths: &HashMap<PathBuf, PlannedPathState>,
        path: &Path,
    ) -> Result<(), ApplyEditErrorKind> {
        if let Some(parent) = path.parent() {
            if self.planned_path_state(planned_paths, parent).kind == PlannedPathKind::File {
                return Err(ApplyEditErrorKind::InvalidEdit);
            }
        }
        Ok(())
    }

    fn path_blocked_by_plan(
        planned_paths: &HashMap<PathBuf, PlannedPathState>,
        path: &Path,
    ) -> bool {
        planned_paths.iter().any(|(planned_root, state)| {
            state.kind == PlannedPathKind::Missing && path.starts_with(planned_root)
        })
    }

    fn plan_document_resource_op(
        &self,
        planned_docs: &mut HashMap<PathBuf, PlannedDocumentState>,
        planned_paths: &mut HashMap<PathBuf, PlannedPathState>,
        op: &lsp::ResourceOp,
    ) -> Result<(), ApplyEditErrorKind> {
        use lsp::ResourceOp;

        match op {
            ResourceOp::Create(op) => {
                let path = Self::workspace_edit_path(&op.uri)?;
                let ignore_if_exists = op.options.as_ref().is_some_and(|options| {
                    !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
                });
                let path_state = self.planned_path_state(planned_paths, &path);
                if ignore_if_exists && path_state.kind != PlannedPathKind::Missing {
                    return Ok(());
                }

                self.validate_file_parent(planned_paths, &path)?;
                planned_paths.insert(
                    path.clone(),
                    PlannedPathState {
                        kind: PlannedPathKind::File,
                        source_path: None,
                    },
                );
                planned_docs.insert(
                    path,
                    PlannedDocumentState {
                        doc_id: None,
                        text: Rope::new(),
                        snapshot_version: None,
                    },
                );
            }
            ResourceOp::Delete(op) => {
                let path = Self::workspace_edit_path(&op.uri)?;
                let path_state = self.planned_path_state(planned_paths, &path);
                if path_state.kind == PlannedPathKind::Directory {
                    Self::remove_planned_document_subtree(planned_docs, &path);
                    Self::remove_planned_path_subtree(planned_paths, &path);
                } else {
                    planned_docs.remove(&path);
                }
                planned_paths.insert(
                    path,
                    PlannedPathState {
                        kind: PlannedPathKind::Missing,
                        source_path: None,
                    },
                );
            }
            ResourceOp::Rename(op) => {
                let from = Self::workspace_edit_path(&op.old_uri)?;
                let to = Self::workspace_edit_path(&op.new_uri)?;
                let ignore_if_exists = op.options.as_ref().is_some_and(|options| {
                    !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
                });
                let from_state = self.planned_path_state(planned_paths, &from);
                let to_state = self.planned_path_state(planned_paths, &to);
                if ignore_if_exists && to_state.kind != PlannedPathKind::Missing {
                    return Ok(());
                }

                self.validate_file_parent(planned_paths, &to)?;
                if from_state.kind == PlannedPathKind::Directory {
                    Self::remap_planned_document_subtree(planned_docs, &from, &to);
                    Self::remap_planned_path_subtree(planned_paths, &from, &to);
                } else if let Some(planned_doc) = planned_docs.remove(&from) {
                    planned_docs.insert(to.clone(), planned_doc);
                }
                planned_paths.insert(
                    from.clone(),
                    PlannedPathState {
                        kind: PlannedPathKind::Missing,
                        source_path: None,
                    },
                );
                planned_paths.insert(
                    to,
                    PlannedPathState {
                        kind: from_state.kind,
                        source_path: from_state.source_path.or(Some(from)),
                    },
                );
            }
        }

        Ok(())
    }

    fn plan_text_edits(
        &self,
        planned_docs: &mut HashMap<PathBuf, PlannedDocumentState>,
        planned_paths: &HashMap<PathBuf, PlannedPathState>,
        url: &helix_lsp::Url,
        version: Option<i32>,
        text_edits: Vec<lsp::TextEdit>,
        offset_encoding: OffsetEncoding,
    ) -> Result<PreparedTextDocumentEdit, ApplyEditErrorKind> {
        let path = Self::workspace_edit_path(url)?;
        self.ensure_planned_document_state(planned_docs, planned_paths, &path)?;

        let planned_state = planned_docs
            .get_mut(&path)
            .expect("planned document state should be loaded");
        if let Some(version) = version {
            if planned_state
                .snapshot_version
                .is_some_and(|expected| expected != version)
            {
                return Err(ApplyEditErrorKind::DocumentChanged);
            }
        }

        let initial_text = planned_state.text.clone();
        let transaction =
            generate_transaction_from_edits(&initial_text, text_edits, offset_encoding);
        let mut next_text = initial_text.clone();
        if !transaction.apply(&mut next_text) {
            return Err(ApplyEditErrorKind::InvalidEdit);
        }
        planned_state.text = next_text;

        Ok(PreparedTextDocumentEdit {
            doc_id: planned_state.doc_id,
            path,
            initial_text,
            transaction,
            snapshot_version: planned_state.snapshot_version,
        })
    }

    fn plan_document_change_edits(
        &self,
        document_edits: &[lsp::TextDocumentEdit],
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();
        for (index, document_edit) in document_edits.iter().enumerate() {
            let edits = document_edit
                .edits
                .iter()
                .map(|edit| match edit {
                    lsp::OneOf::Left(text_edit) => text_edit,
                    lsp::OneOf::Right(annotated_text_edit) => &annotated_text_edit.text_edit,
                })
                .cloned()
                .collect();
            let prepared_edit = self
                .plan_text_edits(
                    &mut plan.planned_docs,
                    &plan.planned_paths,
                    &document_edit.text_document.uri,
                    document_edit.text_document.version,
                    edits,
                    offset_encoding,
                )
                .map_err(|kind| ApplyEditError {
                    kind,
                    failed_change_idx: index,
                })?;
            plan.push_text_edit(index, prepared_edit);
        }
        Ok(plan)
    }

    fn plan_workspace_changes(
        &self,
        changes: &HashMap<helix_lsp::Url, Vec<lsp::TextEdit>>,
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();
        for (index, (uri, text_edits)) in changes.iter().enumerate() {
            let prepared_edit = self
                .plan_text_edits(
                    &mut plan.planned_docs,
                    &plan.planned_paths,
                    uri,
                    None,
                    text_edits.to_vec(),
                    offset_encoding,
                )
                .map_err(|kind| ApplyEditError {
                    kind,
                    failed_change_idx: index,
                })?;
            plan.push_text_edit(index, prepared_edit);
        }
        Ok(plan)
    }

    fn plan_document_operations(
        &self,
        operations: &[lsp::DocumentChangeOperation],
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();
        for (index, operation) in operations.iter().enumerate() {
            match operation {
                lsp::DocumentChangeOperation::Op(op) => {
                    self.plan_document_resource_op(
                        &mut plan.planned_docs,
                        &mut plan.planned_paths,
                        op,
                    )
                    .map_err(|kind| ApplyEditError {
                        kind,
                        failed_change_idx: index,
                    })?;
                    plan.push_resource_op(index, op.clone());
                }
                lsp::DocumentChangeOperation::Edit(document_edit) => {
                    let edits = document_edit
                        .edits
                        .iter()
                        .map(|edit| match edit {
                            lsp::OneOf::Left(text_edit) => text_edit,
                            lsp::OneOf::Right(annotated_text_edit) => {
                                &annotated_text_edit.text_edit
                            }
                        })
                        .cloned()
                        .collect();
                    let prepared_edit = self
                        .plan_text_edits(
                            &mut plan.planned_docs,
                            &plan.planned_paths,
                            &document_edit.text_document.uri,
                            document_edit.text_document.version,
                            edits,
                            offset_encoding,
                        )
                        .map_err(|kind| ApplyEditError {
                            kind,
                            failed_change_idx: index,
                        })?;
                    plan.push_text_edit(index, prepared_edit);
                }
            }
        }
        Ok(plan)
    }
}

fn file_operation_request_from_resource_op(
    op: &lsp::ResourceOp,
) -> Result<FileOperationRequest, ApplyEditErrorKind> {
    use lsp::ResourceOp;

    match op {
        ResourceOp::Create(op) => {
            let path = WorkspaceEditPlanner::workspace_edit_path(&op.uri)?;
            let options = op.options.as_ref();
            Ok(FileOperationRequest {
                origin: FileOperationOrigin::workspace_edit(),
                operation: FileOperation::Create {
                    path,
                    is_dir: false,
                    overwrite: options
                        .and_then(|options| options.overwrite)
                        .unwrap_or(false),
                    ignore_if_exists: options
                        .and_then(|options| options.ignore_if_exists)
                        .unwrap_or(false),
                },
            })
        }
        ResourceOp::Delete(op) => {
            let path = WorkspaceEditPlanner::workspace_edit_path(&op.uri)?;
            Ok(FileOperationRequest {
                origin: FileOperationOrigin::workspace_edit(),
                operation: FileOperation::Delete {
                    path,
                    mode: FileOperationDeleteMode::Permanent,
                    recursive: op
                        .options
                        .as_ref()
                        .and_then(|options| options.recursive)
                        .unwrap_or(false),
                    ignore_missing: true,
                },
            })
        }
        ResourceOp::Rename(op) => {
            let source = WorkspaceEditPlanner::workspace_edit_path(&op.old_uri)?;
            let destination = WorkspaceEditPlanner::workspace_edit_path(&op.new_uri)?;
            Ok(FileOperationRequest {
                origin: FileOperationOrigin::workspace_edit(),
                operation: FileOperation::Move {
                    source,
                    destination: FileOperationDestination::Exact(destination),
                    overwrite: op
                        .options
                        .as_ref()
                        .and_then(|options| options.overwrite)
                        .unwrap_or(false),
                    ignore_if_exists: op
                        .options
                        .as_ref()
                        .and_then(|options| options.ignore_if_exists)
                        .unwrap_or(false),
                    create_parents: true,
                },
            })
        }
    }
}

impl Editor {
    /// Capture only editor-owned state on the UI thread. The returned request is
    /// intentionally executed by the terminal runtime's blocking task ingress.
    pub fn prepare_workspace_edit(
        &self,
        offset_encoding: OffsetEncoding,
        workspace_edit: lsp::WorkspaceEdit,
    ) -> WorkspaceEditPreparation {
        let documents = self
            .documents()
            .filter_map(|document| {
                let path = document.path()?;
                Some((
                    helix_stdx::path::canonicalize(path),
                    WorkspaceEditDocumentSnapshot {
                        doc_id: document.id(),
                        text: document.text().clone(),
                        version: document.version(),
                    },
                ))
            })
            .collect();
        WorkspaceEditPreparation {
            workspace_edit,
            offset_encoding,
            documents,
            config: self.config.clone(),
            syn_loader: self.syn_loader.clone(),
        }
    }

    pub(crate) fn validate_workspace_edit_plan(
        &self,
        plan: &WorkspaceEditPlan,
    ) -> Result<(), ApplyEditError> {
        let mut checked = HashSet::new();
        for (failed_change_idx, operation) in &plan.operations {
            let PreparedWorkspaceOperation::TextEdit(edit) = operation else {
                continue;
            };
            let Some(doc_id) = edit.doc_id else {
                continue;
            };
            if !checked.insert(doc_id) {
                continue;
            }
            if self.document(doc_id).is_none_or(|document| {
                document.version() != edit.snapshot_version.unwrap_or_default()
            }) {
                return Err(ApplyEditError {
                    kind: ApplyEditErrorKind::DocumentChanged,
                    failed_change_idx: *failed_change_idx,
                });
            }
        }
        Ok(())
    }

    fn open_prepared_workspace_edit_document(
        &mut self,
        path: &Path,
        initial_text: Rope,
    ) -> DocumentId {
        let mut document = Document::from(
            initial_text,
            None,
            self.config.clone(),
            self.syn_loader.clone(),
        );
        document.set_path(Some(path));
        let doc_id = self.new_file_from_document(Action::Load, document);
        self.refresh_doc_language(doc_id);
        doc_id
    }

    fn apply_workspace_edit_execution_text(
        &mut self,
        prepared_edit: &PreparedTextDocumentEdit,
        expected_versions: &mut HashMap<DocumentId, i32>,
    ) -> Result<(), ApplyEditErrorKind> {
        let existing_doc_id = prepared_edit
            .doc_id
            .or_else(|| self.document_id_by_path(&prepared_edit.path));
        let doc_id = match existing_doc_id {
            Some(doc_id) => {
                let document = self
                    .document(doc_id)
                    .ok_or(ApplyEditErrorKind::DocumentChanged)?;
                if let Some(expected_version) = expected_versions.get(&doc_id) {
                    if document.version() != *expected_version {
                        return Err(ApplyEditErrorKind::DocumentChanged);
                    }
                } else if let Some(snapshot_version) = prepared_edit.snapshot_version {
                    if document.version() != snapshot_version {
                        return Err(ApplyEditErrorKind::DocumentChanged);
                    }
                } else if document.text() != &prepared_edit.initial_text {
                    // A document that was closed during worker planning appeared or
                    // changed before this step became due.
                    return Err(ApplyEditErrorKind::DocumentChanged);
                }
                doc_id
            }
            None if prepared_edit.doc_id.is_some() => {
                return Err(ApplyEditErrorKind::DocumentChanged);
            }
            None => self.open_prepared_workspace_edit_document(
                &prepared_edit.path,
                prepared_edit.initial_text.clone(),
            ),
        };

        let view_id = self.get_synced_view_id(doc_id);
        {
            let document = self
                .document_mut(doc_id)
                .expect("workspace edit document should still be open");
            if !document.apply(&prepared_edit.transaction, view_id) {
                return Err(ApplyEditErrorKind::InvalidEdit);
            }
            expected_versions.insert(doc_id, document.version());
        }

        let view = view_mut!(self, view_id);
        let document = doc_mut!(self, &doc_id);
        document.append_changes_to_history(view);
        Ok(())
    }

    /// Advance all consecutive text changes due at this point, then return the
    /// next resource operation. Resource completion resumes this same cursor.
    pub fn advance_workspace_edit_execution(
        &mut self,
        execution: &mut WorkspaceEditExecution,
    ) -> Result<WorkspaceEditExecutionStep, ApplyEditError> {
        while execution.cursor < execution.operations.len() {
            let member = execution.cursor;
            let (failed_change_idx, operation) = &execution.operations[member];
            match operation {
                PreparedWorkspaceOperation::TextEdit(prepared_edit) => {
                    self.apply_workspace_edit_execution_text(
                        prepared_edit,
                        &mut execution.expected_versions,
                    )
                    .map_err(|kind| ApplyEditError {
                        kind,
                        failed_change_idx: *failed_change_idx,
                    })?;
                    execution.cursor += 1;
                }
                PreparedWorkspaceOperation::ResourceOp(op) => {
                    let request = file_operation_request_from_resource_op(op).map_err(|kind| {
                        ApplyEditError {
                            kind,
                            failed_change_idx: *failed_change_idx,
                        }
                    })?;
                    execution.cursor += 1;
                    return Ok(WorkspaceEditExecutionStep::Resource {
                        member,
                        failed_change_idx: *failed_change_idx,
                        request,
                    });
                }
            }
        }
        Ok(WorkspaceEditExecutionStep::Complete)
    }

    #[cfg(test)]
    fn apply_prepared_text_edit(
        &mut self,
        prepared_edit: PreparedTextDocumentEdit,
    ) -> Result<(), ApplyEditErrorKind> {
        let doc_id = match prepared_edit
            .doc_id
            .or_else(|| self.document_id_by_path(&prepared_edit.path))
        {
            Some(doc_id) => doc_id,
            None => self.open_prepared_workspace_edit_document(
                &prepared_edit.path,
                prepared_edit.initial_text.clone(),
            ),
        };

        let view_id = self.get_synced_view_id(doc_id);
        {
            let document = self
                .document_mut(doc_id)
                .expect("workspace edit document should still be open");
            if !document.apply(&prepared_edit.transaction, view_id) {
                return Err(ApplyEditErrorKind::InvalidEdit);
            }
        }

        let view = view_mut!(self, view_id);
        let document = doc_mut!(self, &doc_id);
        document.append_changes_to_history(view);
        Ok(())
    }

    #[cfg(test)]
    pub fn apply_prepared_workspace_edit(
        &mut self,
        plan: WorkspaceEditPlan,
    ) -> Result<WorkspaceEditResult, ApplyEditError> {
        self.validate_workspace_edit_plan(&plan)?;
        let mut result = WorkspaceEditResult::default();
        for (failed_change_idx, operation) in plan.into_operations() {
            match operation {
                PreparedWorkspaceOperation::TextEdit(prepared_edit) => {
                    self.apply_prepared_text_edit(prepared_edit)
                        .map_err(|kind| ApplyEditError {
                            kind,
                            failed_change_idx,
                        })?;
                }
                PreparedWorkspaceOperation::ResourceOp(op) => {
                    result.file_operations.push(WorkspaceEditFileOperation {
                        request: file_operation_request_from_resource_op(&op).map_err(|kind| {
                            ApplyEditError {
                                kind,
                                failed_change_idx,
                            }
                        })?,
                        failed_change_idx,
                    });
                }
            }
        }
        Ok(result)
    }

    #[cfg(test)]
    pub fn apply_workspace_edit(
        &mut self,
        offset_encoding: OffsetEncoding,
        workspace_edit: &lsp::WorkspaceEdit,
    ) -> Result<WorkspaceEditResult, ApplyEditError> {
        let plan = self
            .prepare_workspace_edit(offset_encoding, workspace_edit.clone())
            .execute()?;
        self.apply_prepared_workspace_edit(plan)
    }
}
