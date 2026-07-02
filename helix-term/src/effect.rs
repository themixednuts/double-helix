//! Editor-only main-thread effects for [`crate::runtime::ingress::RuntimeTaskEvent`].
//!
//! Async handlers send payloads via [`crate::runtime::send_task_event_with`]; the
//! application and compositor drain typed ingress and apply those effects here.

pub(crate) mod assistant;
pub(crate) mod dap;
pub(crate) mod language_server;
pub(crate) mod plugin;

use std::{
    path::PathBuf,
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
};

use crate::{
    commands,
    runtime::{ExitTaskResult, RuntimeTaskEvent},
};
use helix_core::Transaction;
use helix_plugin::contract::{adapt, events};
use helix_plugin::PluginManager;
use helix_runtime::TaskError;
use helix_vcs::FileBlame;
use helix_view::document::{FormatterError, Mode};
use helix_view::{Document, DocumentId, Editor, ViewId};

/// Fire a plugin event, logging errors.
fn fire_plugin_event(
    editor: &mut Editor,
    plugin_manager: &PluginManager,
    event: events::PluginEvent,
) {
    if let Err(err) = plugin_manager.fire_event(editor, &event) {
        log::error!("Failed to fire plugin event: {err}");
    }
}

/// Emit `AssistantThreadCreated` for threads that exist now but weren't in `before`.
fn emit_new_thread_events(
    editor: &mut Editor,
    plugin_manager: &PluginManager,
    before: &[helix_view::assistant::thread::Id],
) {
    let new_threads: Vec<_> = editor
        .assistant
        .threads()
        .filter(|t| !before.contains(&t.id))
        .map(|t| events::AssistantThreadCreatedEvent {
            thread: adapt::thread_handle(t.id),
            title: t.title().map(|s| s.to_string()),
            scope_cwd: t.scope().cwd.display().to_string(),
        })
        .collect();

    for event in new_threads {
        fire_plugin_event(
            editor,
            plugin_manager,
            events::PluginEvent::AssistantThreadCreated(event),
        );
    }
}

/// Label for an assistant context kind (for plugin events).
fn context_kind_label(kind: &helix_view::assistant::context::Kind) -> String {
    match kind {
        helix_view::assistant::context::Kind::Selection(_) => "selection".into(),
        helix_view::assistant::context::Kind::Symbol(_) => "symbol".into(),
        helix_view::assistant::context::Kind::File(_) => "file".into(),
        helix_view::assistant::context::Kind::Diagnostics(_) => "diagnostics".into(),
        helix_view::assistant::context::Kind::Diff(_) => "diff".into(),
    }
}

pub(crate) fn apply_runtime_task_event(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    plugin_manager: std::sync::Arc<PluginManager>,
    task: RuntimeTaskEvent,
) {
    match task {
        RuntimeTaskEvent::Stub => {}
        RuntimeTaskEvent::ApplyTransactionIfCurrent {
            doc_id,
            view_id,
            expected_version,
            transaction,
        } => {
            if let Some(doc) = editor.document_mut(doc_id) {
                if doc.version() == expected_version {
                    doc.apply(&transaction, view_id);
                }
            }
        }
        RuntimeTaskEvent::DismissNotification { id } => {
            editor.dismiss_notification(id);
        }
        RuntimeTaskEvent::ApplyFormattingResult {
            doc_id,
            view_id,
            expected_version,
            format_result,
            write,
        } => apply_formatting_result(
            editor,
            doc_id,
            view_id,
            expected_version,
            format_result,
            write,
        ),
        RuntimeTaskEvent::SetEditorError { message } => {
            editor.set_error(message);
        }
        RuntimeTaskEvent::AttachDocumentColors { doc_id, colors } => {
            language_server::attach_document_colors(editor, doc_id, colors);
        }
        RuntimeTaskEvent::PullDiagnosticsResponse {
            doc_id,
            uri,
            provider,
            result,
        } => {
            language_server::apply_pull_diagnostics_response(editor, result, provider, uri, doc_id)
        }
        RuntimeTaskEvent::RetryPullDiagnostics {
            doc_id,
            language_servers,
        } => language_server::request_document_diagnostics_for_language_servers(
            editor,
            doc_id,
            language_servers,
            ingress.clone(),
        ),
        RuntimeTaskEvent::RequestDocumentColorsDebounced { doc_ids } => {
            for doc_id in doc_ids {
                language_server::request_document_colors(editor, doc_id, ingress.clone());
            }
        }
        RuntimeTaskEvent::PullDiagnosticsDebounced { document_ids } => {
            for document_id in document_ids {
                language_server::request_document_diagnostics(editor, document_id, ingress.clone());
            }
        }
        RuntimeTaskEvent::PullAllDocumentsDiagnosticsDebounced { language_servers } => {
            let documents: Vec<_> = editor.document_ids().collect();
            for document in documents {
                language_server::request_document_diagnostics_for_language_servers(
                    editor,
                    document,
                    language_servers.clone(),
                    ingress.clone(),
                );
            }
        }
        RuntimeTaskEvent::RequestSignatureDebounced {
            invoked,
            request,
            cancel,
        } => {
            language_server::request_signature_help(editor, invoked, request, cancel, ingress);
        }
        RuntimeTaskEvent::BlameFetchDebounced { doc_id, path, line } => {
            apply_blame_fetch_debounced(editor, doc_id, path, line);
        }
        RuntimeTaskEvent::SelectDocumentHighlights {
            offset_encoding,
            highlights,
        } => language_server::apply_document_highlights(editor, offset_encoding, highlights),
        RuntimeTaskEvent::ApplyInlayHints {
            view_id,
            doc_id,
            offset_encoding,
            id,
            hints,
        } => {
            language_server::apply_inlay_hints(editor, view_id, doc_id, offset_encoding, id, hints)
        }
        RuntimeTaskEvent::DapRestarted => dap::apply_dap_restarted(editor),
        RuntimeTaskEvent::ResumeDebuggerApplication => {
            dap::apply_resume_debugger_application(editor)
        }
        RuntimeTaskEvent::UnsetActiveDebugClient => dap::apply_unset_active_debug_client(editor),
        RuntimeTaskEvent::DapExceptionsConfigured => {}
        RuntimeTaskEvent::RestoreAssistantHistoryThread {
            record,
            activation,
            panel,
        } => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_restore_assistant_history_thread(
                editor, ingress, *record, activation, panel,
            );
            emit_new_thread_events(editor, &plugin_manager, &before);
        }
        RuntimeTaskEvent::ActivateAssistantThread { thread, panel } => {
            assistant::apply_activate_assistant_thread(editor, ingress, thread, panel)
        }
        RuntimeTaskEvent::DetachAssistantContext { item } => {
            assistant::apply_detach_assistant_context(editor, item);
            if let Some(thread) = editor.assistant.active().map(adapt::thread_handle) {
                fire_plugin_event(
                    editor,
                    &plugin_manager,
                    events::PluginEvent::AssistantContextChanged(
                        events::AssistantContextChangedEvent {
                            thread,
                            attached: false,
                            context_kind: "unknown".into(),
                        },
                    ),
                );
            }
        }
        RuntimeTaskEvent::DeliverPluginUiCallback { callback, value } => {
            plugin::apply_plugin_ui_callback(editor, plugin_manager, callback, value)
        }
        RuntimeTaskEvent::RemoveAssistantPanel => assistant::apply_remove_assistant_panel(editor),
        RuntimeTaskEvent::ConnectAssistantBackend {
            command,
            args,
            panel,
        } => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_connect_assistant_backend(editor, ingress, command, args, panel);
            emit_new_thread_events(editor, &plugin_manager, &before);
        }
        RuntimeTaskEvent::CycleAssistantThread { delta } => {
            assistant::apply_cycle_assistant_thread(editor, ingress, delta)
        }
        RuntimeTaskEvent::CloseActiveAssistantThread => {
            let thread = editor.assistant.active().map(adapt::thread_handle);
            assistant::apply_close_active_assistant_thread(editor, ingress);
            if let Some(thread) = thread {
                fire_plugin_event(
                    editor,
                    &plugin_manager,
                    events::PluginEvent::AssistantThreadClosed(
                        events::AssistantThreadClosedEvent { thread },
                    ),
                );
            }
        }
        RuntimeTaskEvent::NewAssistantThreadFromActiveBackend => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_new_assistant_thread_from_active_backend(editor, ingress);
            emit_new_thread_events(editor, &plugin_manager, &before);
        }
        RuntimeTaskEvent::ToggleActiveAssistantFollow => {
            assistant::apply_toggle_active_assistant_follow(editor)
        }
        RuntimeTaskEvent::AttachAssistantContext { item, status } => {
            let context_kind = context_kind_label(&item);
            assistant::apply_attach_assistant_context(editor, item, status);
            if let Some(thread) = editor.assistant.active().map(adapt::thread_handle) {
                fire_plugin_event(
                    editor,
                    &plugin_manager,
                    events::PluginEvent::AssistantContextChanged(
                        events::AssistantContextChangedEvent {
                            thread,
                            attached: true,
                            context_kind,
                        },
                    ),
                );
            }
        }
        RuntimeTaskEvent::SubmitAssistantPrompt { text } => {
            assistant::apply_submit_assistant_prompt(editor, text);
            if let Some(thread) = editor.assistant.active().map(adapt::thread_handle) {
                fire_plugin_event(
                    editor,
                    &plugin_manager,
                    events::PluginEvent::AssistantMessageReceived(
                        events::AssistantMessageReceivedEvent {
                            thread,
                            entry_id: 0,
                            kind: "user_prompt".into(),
                        },
                    ),
                );
            }
        }
        RuntimeTaskEvent::CancelActiveAssistantThread => {
            assistant::apply_cancel_active_assistant_thread(editor)
        }
        RuntimeTaskEvent::OpenSelectedAssistantEntryScratch => {
            assistant::apply_open_selected_assistant_entry_scratch(editor)
        }
        RuntimeTaskEvent::OpenSelectedAssistantTurnChanges => {
            assistant::apply_open_selected_assistant_turn_changes(editor)
        }
        RuntimeTaskEvent::OpenActiveAssistantThreadChanges => {
            assistant::apply_open_active_assistant_thread_changes(editor)
        }
        RuntimeTaskEvent::ApplyAssistantHistoryEntries { scope, entries } => {
            assistant::apply_assistant_history_entries(editor, scope, entries)
        }
        RuntimeTaskEvent::LoadAssistantHistoryThread {
            thread,
            activation,
            panel,
        } => assistant::request_load_assistant_history_thread(
            editor, ingress, thread, activation, panel,
        ),
        RuntimeTaskEvent::BootstrapAssistantHistory { scope } => {
            assistant::request_bootstrap_assistant_history(editor, ingress, scope)
        }
        RuntimeTaskEvent::SelectDebugThread { thread_id, policy } => {
            dap::request_select_debug_thread(editor, ingress, thread_id, policy)
        }
        RuntimeTaskEvent::PauseDebugThread { thread_id } => {
            dap::request_pause_debug_thread(editor, ingress, thread_id)
        }
        RuntimeTaskEvent::SelectStackFrame {
            thread_id,
            frame_id,
        } => dap::apply_select_stack_frame(editor, thread_id, frame_id),
        RuntimeTaskEvent::ApplyStackFrames {
            thread_id,
            frames,
            selection,
        } => dap::apply_stack_frames(editor, thread_id, frames, selection),
        RuntimeTaskEvent::ExecuteLspCommand { command, server_id } => {
            language_server::apply_execute_lsp_command(editor, command, server_id)
        }
        RuntimeTaskEvent::ApplyCodeAction {
            offset_encoding,
            workspace_edit,
            command,
            server_id,
        } => language_server::apply_code_action(
            editor,
            offset_encoding,
            workspace_edit,
            command,
            server_id,
        ),
        RuntimeTaskEvent::SetBreakpointCondition {
            path,
            index,
            condition,
        } => dap::apply_breakpoint_condition(editor, path, index, condition),
        RuntimeTaskEvent::SetBreakpointLogMessage {
            path,
            index,
            log_message,
        } => dap::apply_breakpoint_log_message(editor, path, index, log_message),
        RuntimeTaskEvent::ToggleBreakpoint { path, line } => {
            dap::apply_toggle_breakpoint(editor, path, line)
        }
        RuntimeTaskEvent::AutoSaveRun { save_pending } => {
            apply_auto_save_debounce(editor, save_pending);
        }
        RuntimeTaskEvent::AutoReloadRun { reload_pending } => {
            apply_auto_reload_debounce(editor, reload_pending);
        }
    }
}

pub(crate) fn apply_exit_task_result(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    plugin_manager: std::sync::Arc<PluginManager>,
    result: ExitTaskResult,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(task)) => {
            apply_runtime_task_event(editor, ingress, plugin_manager, task);
            Ok(())
        }
        Ok(Err(err)) => Err(err),
        Err(TaskError::Canceled) => Ok(()),
        Err(TaskError::Panic) => anyhow::bail!("wait task panicked"),
    }
}

pub(crate) fn apply_blame_fetch_debounced(
    editor: &mut Editor,
    doc_id: DocumentId,
    path: PathBuf,
    line: Option<u32>,
) {
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let result = FileBlame::try_new(path);
    doc.set_file_blame(result);
    if !editor.config().inline_blame.auto_fetch {
        if let Some(line) = line {
            commands::blame_line_impl(editor, doc_id, line);
        } else {
            editor.set_status("Blame for this file is now available");
        }
    }
}

pub(crate) fn apply_auto_save_debounce(editor: &mut Editor, save_pending: Arc<AtomicBool>) {
    if editor.mode() == Mode::Insert {
        save_pending.store(true, atomic::Ordering::Relaxed);
    } else {
        request_auto_save(editor);
        save_pending.store(false, atomic::Ordering::Relaxed);
    }
}

fn request_auto_save(editor: &mut Editor) {
    let options = commands::WriteAllOptions {
        policy: helix_view::editor::SavePolicy::Safe,
        write_scratch: false,
        auto_format: false,
    };

    if let Err(err) = commands::typed::write_all_editor_impl(editor, None, None, options) {
        editor.set_error(err.to_string());
    }
}

pub(crate) fn apply_auto_reload_debounce(editor: &mut Editor, reload_pending: Arc<AtomicBool>) {
    if editor.mode() == Mode::Insert {
        reload_pending.store(true, atomic::Ordering::Relaxed);
    } else {
        reload_changed_documents(editor);
        reload_pending.store(false, atomic::Ordering::Relaxed);
    }
}

fn reload_changed_documents(editor: &mut Editor) {
    if count_externally_modified_documents(editor.documents()) == 0 {
        return;
    }

    match commands::typed::reload_all_impl(editor) {
        Ok(()) => editor.set_status("Reloaded modified documents"),
        Err(err) => editor.set_error(format!("Failed to reload document: {err}")),
    }
}

fn count_externally_modified_documents<'a>(docs: impl Iterator<Item = &'a Document>) -> usize {
    docs.filter(|doc| !doc.is_modified())
        .filter(|doc| {
            let last_saved_time = doc.get_last_saved_time();
            let Some(path) = doc.path() else {
                return false;
            };
            if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(modified_time) = metadata.modified() {
                    if modified_time > last_saved_time {
                        return true;
                    }
                }
            }
            false
        })
        .count()
}

pub(crate) fn apply_formatting_result(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    doc_version: i32,
    format: Result<Transaction, FormatterError>,
    write: Option<crate::runtime::PendingFormatWrite>,
) {
    if !editor.contains_document(doc_id) || !editor.contains_view(view_id) {
        return;
    }

    let scrolloff = editor.config().scrolloff;
    let doc = doc_mut!(editor, &doc_id);
    let view = view_mut!(editor, view_id);

    match format {
        Ok(format) => {
            if doc.version() == doc_version {
                doc.apply(&format, view.id);
                doc.append_changes_to_history(view);
                doc.detect_indent_and_line_ending();
                view.ensure_cursor_in_view(doc, scrolloff);
            } else {
                log::info!("discarded formatting changes because the document changed");
            }
        }
        Err(err) => {
            if write.is_none() {
                editor.set_error(err.to_string());
                return;
            }
            log::info!("failed to format '{}': {err}", doc.display_name());
        }
    }

    if let Some(write) = write {
        let id = doc.id();
        if let Err(err) = editor.save(id, write.path, write.policy) {
            editor.set_error(format!("Error saving: {}", err));
        }
    }
}
