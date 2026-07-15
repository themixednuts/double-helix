use std::{path::Path, sync::Arc};

use helix_lsp::{jsonrpc, lsp, OffsetEncoding};
use helix_view::{
    editor::{
        FileOperationApplied, FileOperationChange, FileOperationCompletion, FileOperationDispatch,
        FileOperationError, FileOperationId, FileOperationOrigin, FileOperationPrepared,
        FileOperationRequest, FileOperationWorkspaceEditAction, WorkspaceEditBatchCompletion,
        WorkspaceEditBatchError, WorkspaceEditContinuation, WorkspaceEditExecutionDispatch,
        WorkspaceEditExecutionUpdate,
    },
    Editor,
};
use serde_json::json;

use crate::runtime::{RuntimeIngress, RuntimeTaskEvent};

/// Submit work to the single editor-owned FIFO and start it if idle.
pub(crate) fn submit(editor: &mut Editor, ingress: RuntimeIngress, request: FileOperationRequest) {
    editor.enqueue_file_operation(request);
    drive(editor, ingress);
}

/// Snapshot main-thread document state and prepare the workspace edit on a
/// blocking worker. Resource operations retain `WorkspaceEdit` origin, so they
/// cannot recurse into the `will*` path.
pub(crate) fn apply_workspace_edit(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    offset_encoding: OffsetEncoding,
    workspace_edit: &lsp::WorkspaceEdit,
    continuation: Option<WorkspaceEditContinuation>,
) -> Result<(), helix_view::handlers::workspace_edit::ApplyEditError> {
    let preparation = editor.prepare_workspace_edit(offset_encoding, workspace_edit.clone());
    spawn_workspace_edit_preparation(editor, ingress, None, continuation, preparation);
    Ok(())
}

pub(crate) fn apply_inspected(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    id: FileOperationId,
    result: Result<FileOperationPrepared, FileOperationError>,
) {
    let prepared = match result {
        Ok(prepared) => prepared,
        Err(error) => {
            complete_preparation_failure(editor, ingress, id, error);
            return;
        }
    };
    let Some((request, prepared)) = editor.accept_file_operation_preparation(prepared) else {
        return;
    };

    if request.origin.requests_lsp_will() {
        if let Some(change) = prepared.will_change().cloned() {
            spawn_will_requests(editor, ingress, id, change);
            return;
        }
    }
    spawn_mutation(editor, ingress, id);
}

pub(crate) fn apply_will_completed(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    id: FileOperationId,
    edits: Vec<(OffsetEncoding, lsp::WorkspaceEdit)>,
    errors: Vec<String>,
) {
    if !editor.file_operation_accepts_will_completion(id) {
        return;
    }
    for error in errors {
        log::error!("file operation will* request failed: {error}");
    }
    if !editor.begin_file_operation_workspace_edits(id, edits) {
        return;
    }
    advance_will_workspace_edits(editor, ingress, id);
}

pub(crate) fn apply_workspace_edit_prepared(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    parent: Option<FileOperationId>,
    continuation: Option<WorkspaceEditContinuation>,
    result: Result<
        helix_view::handlers::workspace_edit::WorkspaceEditPlan,
        helix_view::handlers::workspace_edit::ApplyEditError,
    >,
) {
    if let Some(parent) = parent {
        if !editor.file_operation_accepts_workspace_edit_preparation(parent) {
            return;
        }
    }

    match result {
        Ok(plan) => {
            let continuation = continuation
                .or_else(|| parent.map(|id| WorkspaceEditContinuation::ResumeFileOperation { id }));
            let update = editor.start_workspace_edit_execution(plan, continuation, parent);
            apply_workspace_edit_execution_update(editor, ingress, update);
        }
        Err(error) => match parent {
            Some(parent) => complete_waiting_failure(
                editor,
                ingress,
                parent,
                FileOperationError::WorkspaceEdit {
                    message: error.kind.to_string(),
                },
            ),
            None => apply_workspace_edit_batch_completion(
                editor,
                WorkspaceEditBatchCompletion {
                    continuation,
                    result: Err(WorkspaceEditBatchError {
                        message: error.kind.to_string(),
                        failed_change_idx: Some(error.failed_change_idx),
                    }),
                },
                ingress,
            ),
        },
    }
}

pub(crate) fn apply_mutated(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    outcome: helix_view::editor::FileOperationOutcome,
) {
    let Some(completions) = editor.finish_file_operation(outcome) else {
        return;
    };
    for completion in completions {
        apply_completion(editor, ingress.clone(), completion);
    }
    drive(editor, ingress);
}

fn drive(editor: &mut Editor, ingress: RuntimeIngress) {
    let Some(dispatch) = editor.next_file_operation_dispatch() else {
        return;
    };
    match dispatch {
        FileOperationDispatch::Inspect(inspection) => {
            let id = inspection.id();
            let inspection = editor.runtime().block().spawn(move || inspection.execute());
            editor
                .work()
                .spawn(async move {
                    let result = inspection
                        .await
                        .map_err(|error| FileOperationError::Io {
                            kind: std::io::ErrorKind::Other,
                            message: format!("file operation inspection task failed: {error}"),
                        })
                        .and_then(|result| result);
                    let _ = ingress
                        .send_task(RuntimeTaskEvent::FileOperationInspected { id, result })
                        .await;
                })
                .detach();
        }
        FileOperationDispatch::Mutate(work) => spawn_work_mutation(editor, ingress, work),
    }
}

fn spawn_mutation(editor: &mut Editor, ingress: RuntimeIngress, id: FileOperationId) {
    let Some(work) = editor.begin_file_operation_mutation(id) else {
        return;
    };
    spawn_work_mutation(editor, ingress, work);
}

fn spawn_work_mutation(
    editor: &Editor,
    ingress: RuntimeIngress,
    work: helix_view::editor::FileOperationWork,
) {
    let id = work.id();
    let mutation = editor.runtime().block().spawn(move || work.execute());
    editor
        .work()
        .spawn(async move {
            let outcome = mutation.await.unwrap_or_else(|error| {
                helix_view::editor::FileOperationOutcome::task_failed(
                    id,
                    format!("file operation mutation task failed: {error}"),
                )
            });
            let _ = ingress
                .send_task(RuntimeTaskEvent::FileOperationMutated(outcome))
                .await;
        })
        .detach();
}

fn advance_will_workspace_edits(editor: &mut Editor, ingress: RuntimeIngress, id: FileOperationId) {
    if let Some((offset_encoding, workspace_edit)) = editor.next_file_operation_workspace_edit(id) {
        let preparation = editor.prepare_workspace_edit(offset_encoding, workspace_edit);
        spawn_workspace_edit_preparation(editor, ingress, Some(id), None, preparation);
        return;
    }

    match editor.finish_file_operation_workspace_edits(id) {
        Some(FileOperationWorkspaceEditAction::Mutate) => spawn_mutation(editor, ingress, id),
        Some(FileOperationWorkspaceEditAction::Drive) => drive(editor, ingress),
        None => {}
    }
}

fn spawn_workspace_edit_preparation(
    editor: &Editor,
    ingress: RuntimeIngress,
    parent: Option<FileOperationId>,
    continuation: Option<WorkspaceEditContinuation>,
    preparation: helix_view::handlers::workspace_edit::WorkspaceEditPreparation,
) {
    let preparation = editor
        .runtime()
        .block()
        .spawn(move || preparation.execute());
    editor
        .work()
        .spawn(async move {
            let result = preparation.await.unwrap_or_else(|error| {
                Err(
                    helix_view::handlers::workspace_edit::ApplyEditError::worker_failed(
                        error.to_string(),
                    ),
                )
            });
            let _ = ingress
                .send_task(RuntimeTaskEvent::WorkspaceEditPrepared {
                    parent,
                    continuation,
                    result,
                })
                .await;
        })
        .detach();
}

fn spawn_will_requests(
    editor: &Editor,
    ingress: RuntimeIngress,
    id: FileOperationId,
    change: FileOperationChange,
) {
    let servers = editor.file_operation_language_servers();
    editor
        .work()
        .spawn(async move {
            let mut edits = Vec::new();
            let mut errors = Vec::new();
            for server in servers {
                let response = match &change {
                    FileOperationChange::Create { path, is_dir } => {
                        match server.will_create(path, *is_dir) {
                            Some(request) => request.await,
                            None => continue,
                        }
                    }
                    FileOperationChange::Delete { path, is_dir } => {
                        match server.will_delete(path, *is_dir) {
                            Some(request) => request.await,
                            None => continue,
                        }
                    }
                    FileOperationChange::Move { from, to, is_dir } => {
                        match server.will_rename(from, to, *is_dir) {
                            Some(request) => request.await,
                            None => continue,
                        }
                    }
                };
                match response {
                    Ok(Some(edit)) => edits.push((server.offset_encoding(), edit)),
                    Ok(None) => {}
                    Err(error) => errors.push(error.to_string()),
                }
            }
            let _ = ingress
                .send_task(RuntimeTaskEvent::FileOperationWillCompleted { id, edits, errors })
                .await;
        })
        .detach();
}

fn complete_preparation_failure(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    id: FileOperationId,
    error: FileOperationError,
) {
    let Some(completions) = editor.fail_file_operation_preparation(id, error) else {
        return;
    };
    for completion in completions {
        apply_completion(editor, ingress.clone(), completion);
    }
    drive(editor, ingress);
}

fn complete_waiting_failure(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    id: FileOperationId,
    error: FileOperationError,
) {
    let Some(completion) = editor.fail_file_operation_waiting(id, error) else {
        return;
    };
    apply_completion(editor, ingress.clone(), completion);
    drive(editor, ingress);
}

fn apply_completion(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    completion: FileOperationCompletion,
) {
    let workspace_edit_update = editor.resume_workspace_edit_execution(&completion);
    match &completion.result {
        Ok(applied) => {
            update_open_document_paths(editor, applied);
            notify_language_servers(editor, applied);
            notify_completion(
                editor,
                ingress.clone(),
                &completion,
                Ok(describe_success(&completion.request, applied)),
            );
        }
        Err(error) => {
            notify_completion(editor, ingress.clone(), &completion, Err(error.to_string()))
        }
    }
    if let Some(mut update) = workspace_edit_update {
        if let Some(parent_completion) = update.parent_completion.take() {
            apply_completion(editor, ingress.clone(), parent_completion);
        }
        apply_workspace_edit_execution_update(editor, ingress, update);
    }
}

fn apply_workspace_edit_execution_update(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    update: WorkspaceEditExecutionUpdate,
) {
    match update.dispatch {
        WorkspaceEditExecutionDispatch::EnqueueResource(request) => {
            submit(editor, ingress, request)
        }
        WorkspaceEditExecutionDispatch::Drive => drive(editor, ingress),
        WorkspaceEditExecutionDispatch::Advance(batch_id) => {
            let update = editor.advance_workspace_edit_execution_batch(batch_id);
            apply_workspace_edit_execution_update(editor, ingress, update);
        }
        WorkspaceEditExecutionDispatch::Complete(completion) => {
            apply_workspace_edit_batch_completion(editor, completion, ingress)
        }
    }
}

fn apply_workspace_edit_batch_completion(
    editor: &mut Editor,
    completion: WorkspaceEditBatchCompletion,
    ingress: RuntimeIngress,
) {
    match completion.continuation {
        None => {
            if let Err(error) = completion.result {
                editor.set_error(format!(
                    "Failed to apply workspace edits: {}",
                    error.message
                ));
            }
        }
        Some(WorkspaceEditContinuation::ApplyEditReply {
            server_id,
            request_id,
        }) => {
            let response = apply_workspace_edit_response(completion.result);
            let Some(server) = editor.language_servers.get_by_id(server_id).cloned() else {
                return;
            };
            schedule_apply_workspace_edit_reply(&editor.work(), server, request_id, response);
        }
        Some(WorkspaceEditContinuation::ExecuteCommand { server_id, command }) => {
            if let Err(error) = completion.result {
                editor.set_error(format!(
                    "Failed to apply code action edits: {}",
                    error.message
                ));
                return;
            }
            crate::effect::language_server::apply_execute_lsp_command(editor, command, server_id);
        }
        Some(WorkspaceEditContinuation::ResumeFileOperation { id }) => match completion.result {
            Ok(()) => advance_will_workspace_edits(editor, ingress, id),
            Err(error) => complete_waiting_failure(
                editor,
                ingress,
                id,
                FileOperationError::WorkspaceEdit {
                    message: error.message,
                },
            ),
        },
    }
}

fn apply_workspace_edit_response(
    result: Result<(), WorkspaceEditBatchError>,
) -> lsp::ApplyWorkspaceEditResponse {
    match result {
        Ok(()) => lsp::ApplyWorkspaceEditResponse {
            applied: true,
            failure_reason: None,
            failed_change: None,
        },
        Err(error) => lsp::ApplyWorkspaceEditResponse {
            applied: false,
            failure_reason: Some(error.message),
            failed_change: error.failed_change_idx.map(|index| index as u32),
        },
    }
}

/// `reply_async` uses the transport's guaranteed-priority response path. Run
/// it on `Work` so a full UI/control queue cannot drop the one deferred reply.
fn schedule_apply_workspace_edit_reply(
    work: &helix_runtime::Work,
    server: Arc<helix_lsp::Client>,
    request_id: jsonrpc::Id,
    response: lsp::ApplyWorkspaceEditResponse,
) {
    work.spawn(async move {
        if let Err(error) = server.reply_async(request_id.clone(), Ok(json!(response))).await {
            log::error!(
                "Failed to send deferred workspace edit reply to server '{}' request {request_id}: {error}",
                server.name()
            );
        }
    })
    .detach();
}

#[cfg(test)]
mod deferred_reply_tests {
    use super::*;

    #[test]
    fn deferred_apply_edit_reply_uses_typed_work_scheduling_and_preserves_failure_details() {
        let _: fn(
            &helix_runtime::Work,
            Arc<helix_lsp::Client>,
            jsonrpc::Id,
            lsp::ApplyWorkspaceEditResponse,
        ) = schedule_apply_workspace_edit_reply;

        let success = apply_workspace_edit_response(Ok(()));
        assert!(success.applied);
        assert_eq!(success.failure_reason, None);
        assert_eq!(success.failed_change, None);

        let failure = apply_workspace_edit_response(Err(WorkspaceEditBatchError {
            message: "resource operation failed".to_owned(),
            failed_change_idx: Some(4),
        }));
        assert!(!failure.applied);
        assert_eq!(
            failure.failure_reason.as_deref(),
            Some("resource operation failed")
        );
        assert_eq!(failure.failed_change, Some(4));
    }
}

fn update_open_document_paths(editor: &mut Editor, applied: &FileOperationApplied) {
    for change in &applied.changes {
        let FileOperationChange::Move { from, to, is_dir } = change else {
            continue;
        };
        let updates: Vec<_> = editor
            .documents()
            .filter_map(|document| {
                let path = document.path()?;
                let suffix = if *is_dir {
                    path.strip_prefix(from).ok()?
                } else if path == from {
                    Path::new("")
                } else {
                    return None;
                };
                Some((document.id(), to.join(suffix)))
            })
            .collect();
        for (document_id, path) in updates {
            editor.set_doc_path(document_id, &path);
        }
    }
}

fn notify_language_servers(editor: &mut Editor, applied: &FileOperationApplied) {
    let servers = editor.file_operation_language_servers();
    for change in &applied.changes {
        match change {
            FileOperationChange::Create { path, is_dir } => {
                for server in &servers {
                    server.did_create(path, *is_dir);
                }
            }
            FileOperationChange::Delete { path, is_dir } => {
                for server in &servers {
                    server.did_delete(path, *is_dir);
                }
            }
            FileOperationChange::Move { from, to, is_dir } => {
                for server in &servers {
                    server.did_rename(from, to, *is_dir);
                }
            }
        }
    }
    for path in &applied.affected_paths {
        editor.file_operation_path_changed(path.clone());
    }
}

fn notify_completion(
    editor: &mut Editor,
    ingress: RuntimeIngress,
    completion: &FileOperationCompletion,
    result: Result<String, String>,
) {
    match &completion.request.origin {
        FileOperationOrigin::Explorer {
            root,
            cursor,
            select_path,
        } => {
            let _ = ingress.ui(crate::runtime::UiCommand::FileExplorer(
                crate::runtime::ui::command::FileExplorerCommand::FileOperationCompleted {
                    root: root.clone(),
                    cursor: *cursor,
                    select_path: select_path.clone(),
                    result,
                },
            ));
        }
        FileOperationOrigin::Command => match result {
            Ok(message) => editor.set_status(message),
            Err(message) => editor.set_error(message),
        },
        FileOperationOrigin::WorkspaceEdit { batch: Some(_), .. } => {}
        FileOperationOrigin::WorkspaceEdit { batch: None, .. } => {
            if let Err(message) = result {
                editor.set_error(format!("workspace file operation failed: {message}"));
            }
        }
    }
}

fn describe_success(request: &FileOperationRequest, applied: &FileOperationApplied) -> String {
    let operation = match &request.operation {
        helix_view::editor::FileOperation::Create { .. } => "Created",
        helix_view::editor::FileOperation::Copy { .. } => "Copied",
        helix_view::editor::FileOperation::Move { .. } => "Moved",
        helix_view::editor::FileOperation::Delete { mode, .. } => match mode {
            helix_view::editor::FileOperationDeleteMode::Trash => "Moved to trash",
            helix_view::editor::FileOperationDeleteMode::Permanent => "Deleted",
        },
        helix_view::editor::FileOperation::Undo => "Undid file operation",
        helix_view::editor::FileOperation::Redo => "Redid file operation",
    };
    if let Some(path) = applied.affected_paths.last() {
        format!("{operation}: {}", path.display())
    } else {
        operation.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::editor::{FileOperation, FileOperationDestination};
    use std::path::PathBuf;

    #[test]
    fn server_resource_operations_do_not_request_will() {
        assert!(!FileOperationOrigin::workspace_edit().requests_lsp_will());
        assert!(FileOperationOrigin::Command.requests_lsp_will());
    }

    #[test]
    fn operation_description_uses_resolved_worker_paths() {
        let request = FileOperationRequest::copy_path(
            FileOperationOrigin::Command,
            PathBuf::from("from"),
            FileOperationDestination::Exact(PathBuf::from("to")),
        );
        let applied = FileOperationApplied {
            changes: Box::new([]),
            affected_paths: Box::new([PathBuf::from("to")]),
        };
        assert_eq!(describe_success(&request, &applied), "Copied: to");
        assert!(matches!(request.operation, FileOperation::Copy { .. }));
    }
}
