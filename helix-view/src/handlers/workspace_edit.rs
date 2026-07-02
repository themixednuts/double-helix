use std::collections::HashMap;
use std::fmt::Display;

use crate::editor::Action;
use crate::{DocumentId, Editor};
use helix_core::{Rope, Transaction, Uri};
use helix_lsp::util::generate_transaction_from_edits;
use helix_lsp::{lsp, OffsetEncoding};

#[derive(Debug)]
pub struct ApplyEditError {
    pub kind: ApplyEditErrorKind,
    pub failed_change_idx: usize,
}

#[derive(Debug)]
pub enum ApplyEditErrorKind {
    DocumentChanged,
    FileNotFound,
    InvalidEdit,
    InvalidUrl(helix_core::uri::UrlConversionError),
    IoError(std::io::Error),
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

struct PreparedTextDocumentEdit {
    doc_id: Option<DocumentId>,
    path: std::path::PathBuf,
    transaction: Transaction,
}

#[derive(Clone)]
struct PlannedDocumentState {
    doc_id: Option<DocumentId>,
    text: Rope,
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
    source_path: Option<std::path::PathBuf>,
}

enum PreparedWorkspaceOperation {
    TextEdit(PreparedTextDocumentEdit),
    ResourceOp(lsp::ResourceOp),
}

#[derive(Default)]
struct WorkspaceEditPlan {
    planned_docs: HashMap<std::path::PathBuf, PlannedDocumentState>,
    planned_paths: HashMap<std::path::PathBuf, PlannedPathState>,
    operations: Vec<(usize, PreparedWorkspaceOperation)>,
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

    fn into_operations(self) -> Vec<(usize, PreparedWorkspaceOperation)> {
        self.operations
    }
}

impl Editor {
    fn workspace_edit_path(
        &self,
        url: &helix_lsp::Url,
    ) -> Result<std::path::PathBuf, ApplyEditErrorKind> {
        let uri = match Uri::try_from(url) {
            Ok(uri) => uri,
            Err(err) => {
                log::error!("{err}");
                return Err(err.into());
            }
        };

        Ok(helix_stdx::path::canonicalize(
            uri.as_path().expect("URIs are valid paths"),
        ))
    }

    fn load_workspace_edit_snapshot(
        &self,
        path: &std::path::Path,
    ) -> Result<PlannedDocumentState, ApplyEditErrorKind> {
        if let Some(doc_id) = self.document_id_by_path(path) {
            let doc = self
                .document(doc_id)
                .expect("document id returned from path lookup should exist");
            return Ok(PlannedDocumentState {
                doc_id: Some(doc_id),
                text: doc.text().clone(),
            });
        }

        let doc = crate::Document::open(
            path,
            None,
            crate::document::LanguageInitialization::Full,
            self.config.clone(),
            self.syn_loader.clone(),
        )
        .map_err(|err| {
            log::error!(
                "failed to open document: {}: {}",
                path.to_string_lossy(),
                err
            );
            ApplyEditErrorKind::FileNotFound
        })?;

        Ok(PlannedDocumentState {
            doc_id: None,
            text: doc.text().clone(),
        })
    }

    fn current_path_state(&self, path: &std::path::Path) -> PlannedPathState {
        let path = helix_stdx::path::canonicalize(path);
        if self.document_id_by_path(&path).is_some() || path.is_file() {
            PlannedPathState {
                kind: PlannedPathKind::File,
                source_path: Some(path),
            }
        } else if path.is_dir() {
            PlannedPathState {
                kind: PlannedPathKind::Directory,
                source_path: None,
            }
        } else {
            PlannedPathState {
                kind: PlannedPathKind::Missing,
                source_path: None,
            }
        }
    }

    fn planned_path_state(
        &self,
        planned_paths: &HashMap<std::path::PathBuf, PlannedPathState>,
        path: &std::path::Path,
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
        planned_docs: &mut HashMap<std::path::PathBuf, PlannedDocumentState>,
        from: &std::path::Path,
        to: &std::path::Path,
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
        planned_docs: &mut HashMap<std::path::PathBuf, PlannedDocumentState>,
        root: &std::path::Path,
    ) {
        planned_docs.retain(|path, _| !path.starts_with(root));
    }

    fn remap_planned_path_subtree(
        planned_paths: &mut HashMap<std::path::PathBuf, PlannedPathState>,
        from: &std::path::Path,
        to: &std::path::Path,
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
        planned_paths: &mut HashMap<std::path::PathBuf, PlannedPathState>,
        root: &std::path::Path,
    ) {
        planned_paths.retain(|path, _| !path.starts_with(root));
    }

    fn ensure_planned_document_state(
        &self,
        planned_docs: &mut HashMap<std::path::PathBuf, PlannedDocumentState>,
        planned_paths: &HashMap<std::path::PathBuf, PlannedPathState>,
        path: &std::path::Path,
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

        let source_path = path_state
            .source_path
            .clone()
            .unwrap_or_else(|| path.to_path_buf());
        let planned_doc = self.load_workspace_edit_snapshot(&source_path)?;
        planned_docs.insert(path.to_path_buf(), planned_doc);
        Ok(())
    }

    fn validate_file_parent(
        &self,
        planned_paths: &HashMap<std::path::PathBuf, PlannedPathState>,
        path: &std::path::Path,
    ) -> Result<(), ApplyEditErrorKind> {
        if let Some(parent) = path.parent() {
            let parent_state = self.planned_path_state(planned_paths, parent);
            if parent_state.kind == PlannedPathKind::File {
                return Err(ApplyEditErrorKind::InvalidEdit);
            }
        }

        Ok(())
    }

    fn path_blocked_by_plan(
        planned_paths: &HashMap<std::path::PathBuf, PlannedPathState>,
        path: &std::path::Path,
    ) -> bool {
        planned_paths.iter().any(|(planned_root, state)| {
            state.kind == PlannedPathKind::Missing && path.starts_with(planned_root)
        })
    }

    fn plan_document_resource_op(
        &self,
        planned_docs: &mut HashMap<std::path::PathBuf, PlannedDocumentState>,
        planned_paths: &mut HashMap<std::path::PathBuf, PlannedPathState>,
        op: &lsp::ResourceOp,
    ) -> Result<(), ApplyEditErrorKind> {
        use lsp::ResourceOp;

        match op {
            ResourceOp::Create(op) => {
                let path = self.workspace_edit_path(&op.uri)?;
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
                    },
                );
            }
            ResourceOp::Delete(op) => {
                let path = self.workspace_edit_path(&op.uri)?;
                let path_state = self.planned_path_state(planned_paths, &path);
                if path_state.kind == PlannedPathKind::Directory {
                    Self::remove_planned_document_subtree(planned_docs, &path);
                    Self::remove_planned_path_subtree(planned_paths, &path);
                } else {
                    planned_docs.remove(&path);
                }
                planned_paths.insert(
                    path.clone(),
                    PlannedPathState {
                        kind: PlannedPathKind::Missing,
                        source_path: None,
                    },
                );
            }
            ResourceOp::Rename(op) => {
                let from = self.workspace_edit_path(&op.old_uri)?;
                let to = self.workspace_edit_path(&op.new_uri)?;
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
                    to.clone(),
                    PlannedPathState {
                        kind: from_state.kind,
                        source_path: from_state
                            .source_path
                            .clone()
                            .or_else(|| Some(from.clone())),
                    },
                );
            }
        }

        Ok(())
    }

    fn plan_text_edits(
        &mut self,
        planned_docs: &mut HashMap<std::path::PathBuf, PlannedDocumentState>,
        planned_paths: &HashMap<std::path::PathBuf, PlannedPathState>,
        url: &helix_lsp::Url,
        version: Option<i32>,
        text_edits: Vec<lsp::TextEdit>,
        offset_encoding: OffsetEncoding,
    ) -> Result<PreparedTextDocumentEdit, ApplyEditErrorKind> {
        let path = self.workspace_edit_path(url)?;
        self.ensure_planned_document_state(planned_docs, planned_paths, &path)?;

        let current_doc_id = planned_docs.get(&path).and_then(|doc| doc.doc_id);
        if let Some(version) = version {
            if let Some(doc_id) = current_doc_id {
                let current_doc = self
                    .document(doc_id)
                    .expect("workspace edit document should exist during planning");
                if version != current_doc.version() {
                    let err = format!("outdated workspace edit for {path:?}");
                    log::error!(
                        "{err}, expected {} but got {version}",
                        current_doc.version()
                    );
                    self.set_error(err);
                    return Err(ApplyEditErrorKind::DocumentChanged);
                }
            }
        }

        let planned_state = planned_docs
            .get_mut(&path)
            .expect("planned document state should be loaded");
        let transaction =
            generate_transaction_from_edits(&planned_state.text, text_edits, offset_encoding);

        let mut next_text = planned_state.text.clone();
        if !transaction.apply(&mut next_text) {
            return Err(ApplyEditErrorKind::InvalidEdit);
        }
        planned_state.text = next_text;

        Ok(PreparedTextDocumentEdit {
            doc_id: planned_state.doc_id,
            path,
            transaction,
        })
    }

    fn open_workspace_edit_document(
        &mut self,
        path: &std::path::Path,
    ) -> Result<DocumentId, ApplyEditErrorKind> {
        match self.open(path, Action::Load) {
            Ok(doc_id) => Ok(doc_id),
            Err(err) => {
                let err = format!(
                    "failed to open document: {}: {}",
                    path.to_string_lossy(),
                    err
                );
                log::error!("{}", err);
                self.set_error(err);
                Err(ApplyEditErrorKind::FileNotFound)
            }
        }
    }

    fn apply_prepared_text_edit(
        &mut self,
        prepared_edit: PreparedTextDocumentEdit,
    ) -> Result<(), ApplyEditErrorKind> {
        let doc_id = match prepared_edit
            .doc_id
            .or_else(|| self.document_id_by_path(&prepared_edit.path))
        {
            Some(doc_id) => doc_id,
            None => self.open_workspace_edit_document(&prepared_edit.path)?,
        };

        let view_id = self.get_synced_view_id(doc_id);
        {
            let doc = self
                .document_mut(doc_id)
                .expect("workspace edit document should still be open");
            if !doc.apply(&prepared_edit.transaction, view_id) {
                return Err(ApplyEditErrorKind::InvalidEdit);
            }
        }

        let view = view_mut!(self, view_id);
        let doc = doc_mut!(self, &doc_id);
        doc.append_changes_to_history(view);
        Ok(())
    }

    fn plan_document_change_edits(
        &mut self,
        document_edits: &[lsp::TextDocumentEdit],
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();

        for (i, document_edit) in document_edits.iter().enumerate() {
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
                    failed_change_idx: i,
                })?;

            plan.push_text_edit(i, prepared_edit);
        }

        Ok(plan)
    }

    fn plan_workspace_changes(
        &mut self,
        changes: &std::collections::HashMap<helix_lsp::Url, Vec<lsp::TextEdit>>,
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();

        for (i, (uri, text_edits)) in changes.iter().enumerate() {
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
                    failed_change_idx: i,
                })?;

            plan.push_text_edit(i, prepared_edit);
        }

        Ok(plan)
    }

    fn plan_document_operations(
        &mut self,
        operations: &[lsp::DocumentChangeOperation],
        offset_encoding: OffsetEncoding,
    ) -> Result<WorkspaceEditPlan, ApplyEditError> {
        let mut plan = WorkspaceEditPlan::default();

        for (i, operation) in operations.iter().enumerate() {
            match operation {
                lsp::DocumentChangeOperation::Op(op) => {
                    self.plan_document_resource_op(
                        &mut plan.planned_docs,
                        &mut plan.planned_paths,
                        op,
                    )
                    .map_err(|kind| ApplyEditError {
                        kind,
                        failed_change_idx: i,
                    })?;
                    plan.push_resource_op(i, op.clone());
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
                            failed_change_idx: i,
                        })?;
                    plan.push_text_edit(i, prepared_edit);
                }
            }
        }

        Ok(plan)
    }

    fn apply_prepared_workspace_operations(
        &mut self,
        plan: WorkspaceEditPlan,
    ) -> Result<(), ApplyEditError> {
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
                    self.apply_document_resource_op(&op)
                        .map_err(|kind| ApplyEditError {
                            kind,
                            failed_change_idx,
                        })?;
                }
            }
        }

        Ok(())
    }

    pub fn apply_workspace_edit(
        &mut self,
        offset_encoding: OffsetEncoding,
        workspace_edit: &lsp::WorkspaceEdit,
    ) -> Result<(), ApplyEditError> {
        if let Some(ref document_changes) = workspace_edit.document_changes {
            match document_changes {
                lsp::DocumentChanges::Edits(document_edits) => {
                    let plan = self.plan_document_change_edits(document_edits, offset_encoding)?;
                    self.apply_prepared_workspace_operations(plan)?;
                }
                lsp::DocumentChanges::Operations(operations) => {
                    log::debug!("document changes - operations: {:?}", operations);
                    let plan = self.plan_document_operations(operations, offset_encoding)?;
                    self.apply_prepared_workspace_operations(plan)?;
                }
            }

            return Ok(());
        }

        if let Some(ref changes) = workspace_edit.changes {
            log::debug!("workspace changes: {:?}", changes);
            let plan = self.plan_workspace_changes(changes, offset_encoding)?;
            self.apply_prepared_workspace_operations(plan)?;
        }

        Ok(())
    }

    fn apply_document_resource_op(
        &mut self,
        op: &lsp::ResourceOp,
    ) -> Result<(), ApplyEditErrorKind> {
        use lsp::ResourceOp;
        use std::fs;

        match op {
            ResourceOp::Create(op) => {
                let uri = Uri::try_from(&op.uri)?;
                let path = uri.as_path().expect("URIs are valid paths");
                let ignore_if_exists = op.options.as_ref().is_some_and(|options| {
                    !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
                });
                if !ignore_if_exists || !path.exists() {
                    if let Some(dir) = path.parent() {
                        if !dir.is_dir() {
                            fs::create_dir_all(dir)?;
                        }
                    }

                    fs::write(path, [])?;
                    self.language_servers
                        .file_event_handler
                        .file_changed(path.to_path_buf());
                }
            }
            ResourceOp::Delete(op) => {
                let uri = Uri::try_from(&op.uri)?;
                let path = uri.as_path().expect("URIs are valid paths");
                if path.is_dir() {
                    let recursive = op
                        .options
                        .as_ref()
                        .and_then(|options| options.recursive)
                        .unwrap_or(false);

                    if recursive {
                        fs::remove_dir_all(path)?
                    } else {
                        fs::remove_dir(path)?
                    }
                    self.language_servers
                        .file_event_handler
                        .file_changed(path.to_path_buf());
                } else if path.is_file() {
                    fs::remove_file(path)?;
                }
            }
            ResourceOp::Rename(op) => {
                let from_uri = Uri::try_from(&op.old_uri)?;
                let from = from_uri.as_path().expect("URIs are valid paths");
                let to_uri = Uri::try_from(&op.new_uri)?;
                let to = to_uri.as_path().expect("URIs are valid paths");
                let ignore_if_exists = op.options.as_ref().is_some_and(|options| {
                    !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
                });
                if !ignore_if_exists || !to.exists() {
                    self.move_path(from, to)?;
                }
            }
        }
        Ok(())
    }
}
