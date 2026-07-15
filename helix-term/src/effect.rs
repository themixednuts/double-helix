//! Editor-only main-thread effects for [`crate::runtime::ingress::RuntimeTaskEvent`].
//!
//! Async handlers send payloads via [`crate::runtime::send_task_event_with`]; the
//! application and compositor drain typed ingress and apply those effects here.

pub(crate) mod assistant;
pub(crate) mod dap;
pub(crate) mod file_operation;
pub(crate) mod language_server;
pub(crate) mod pkg;
pub(crate) mod plugin;

use std::{
    collections::BTreeSet,
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
use helix_core::{Selection, Transaction};
use helix_plugin_api::events;
use helix_plugin_editor::adapt;
use helix_runtime::TaskError;
use helix_vcs::FileBlame;
use helix_view::document::{FormatterError, Mode};
use helix_view::{DocumentId, Editor, ViewId};

/// Fire a plugin event, logging errors.
fn fire_plugin_event(
    plugin_runtime: &crate::plugin_registry::PluginRuntime,
    event: events::PluginEvent,
) {
    plugin_runtime.notify_event(event);
}

/// Emit `AssistantThreadCreated` for threads that exist now but weren't in `before`.
fn emit_new_thread_events(
    editor: &mut Editor,
    plugin_runtime: &crate::plugin_registry::PluginRuntime,
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
            plugin_runtime,
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

fn apply_shell_result(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    expected_version: i32,
    transaction: Option<Transaction>,
    selection: Option<Selection>,
) {
    let view_is_current = editor
        .tree
        .views()
        .any(|(view, _)| view.id == view_id && view.doc == doc_id);
    if !view_is_current
        || editor
            .document(doc_id)
            .is_none_or(|doc| doc.version() != expected_version)
    {
        editor.set_status("Buffer changed before shell command completed");
        return;
    }

    let scrolloff = editor.config().scrolloff;
    let view = view_mut!(editor, view_id);
    let doc = doc_mut!(editor, &doc_id);
    if let Some(transaction) = transaction {
        doc.apply(&transaction, view_id);
        doc.append_changes_to_history(view);
    } else if let Some(selection) = selection {
        doc.set_selection(view_id, selection);
    }
    view.ensure_cursor_in_view(doc, scrolloff);
}

pub(crate) fn refresh_assistant_agent_cache(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
) {
    let config_generation = editor.config_gen;
    let config = editor.config().pkg.clone();
    let loaded = editor.runtime().block().spawn(move || {
        let runtime_assets = helix_loader::runtime_assets()?;
        Editor::load_assistant_packaged_agents(config, runtime_assets)
    });
    editor
        .work()
        .spawn(async move {
            let loaded = loaded.await;
            match loaded {
                Ok(Ok((generation, agents))) => {
                    let _ = ingress
                        .send_task(RuntimeTaskEvent::ApplyAssistantAgents(
                            crate::runtime::PreparedAssistantAgents {
                                config_generation,
                                generation,
                                agents,
                            },
                        ))
                        .await;
                }
                Ok(Err(error)) => {
                    log::warn!("failed to refresh packaged ACP-agent cache: {error}");
                }
                Err(error) => {
                    log::warn!("packaged ACP-agent cache worker failed: {error}");
                }
            }
        })
        .detach();
}

pub(crate) fn apply_runtime_task_event(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
    task: RuntimeTaskEvent,
) {
    match task {
        RuntimeTaskEvent::Stub => {}
        RuntimeTaskEvent::ApplySyntax {
            document,
            version,
            syntax,
        } => {
            let Some(doc) = editor.document_mut(document) else {
                return;
            };
            if doc.version() != version || !doc.syntax_snapshot().is_stale() {
                return;
            }
            doc.set_syntax(Some(syntax));
            editor.mark_redraw_pending();
            editor.request_redraw();
        }
        RuntimeTaskEvent::FileOperationInspected { id, result } => {
            file_operation::apply_inspected(editor, ingress.clone(), id, result)
        }
        RuntimeTaskEvent::FileOperationWillCompleted { id, edits, errors } => {
            file_operation::apply_will_completed(editor, ingress.clone(), id, edits, errors)
        }
        RuntimeTaskEvent::WorkspaceEditPrepared {
            parent,
            continuation,
            result,
        } => file_operation::apply_workspace_edit_prepared(
            editor,
            ingress.clone(),
            parent,
            continuation,
            result,
        ),
        RuntimeTaskEvent::FileOperationMutated(outcome) => {
            file_operation::apply_mutated(editor, ingress.clone(), outcome)
        }
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
        RuntimeTaskEvent::ApplyShellResult {
            doc_id,
            view_id,
            expected_version,
            transaction,
            selection,
        } => apply_shell_result(
            editor,
            doc_id,
            view_id,
            expected_version,
            transaction,
            selection,
        ),
        RuntimeTaskEvent::ApplyCompletionAdditionalEdits {
            doc_id,
            view_id,
            expected_version,
            offset_encoding,
            edits,
        } => {
            let view_is_current = editor
                .tree
                .views()
                .any(|(view, _)| view.id == view_id && view.doc == doc_id);
            if !view_is_current {
                return;
            }
            if let Some(doc) = editor.document_mut(doc_id) {
                if doc.version() == expected_version && !edits.is_empty() {
                    let transaction = helix_lsp::util::generate_transaction_from_edits(
                        doc.text(),
                        edits,
                        offset_encoding,
                    );
                    doc.apply(&transaction, view_id);
                }
            }
        }
        RuntimeTaskEvent::ApplyLspSelectionRange(response) => {
            helix_view::commands::editing::apply_lsp_selection_range_response(editor, response);
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
        RuntimeTaskEvent::AttachDocumentColors {
            doc_id,
            expected_version,
            request,
            colors,
        } => {
            language_server::attach_document_colors(
                editor,
                doc_id,
                expected_version,
                &request,
                colors,
            );
        }
        RuntimeTaskEvent::StartPullDiagnostics { target, cancel } => {
            language_server::start_pull_diagnostics_request(editor, target, cancel, ingress.clone())
        }
        RuntimeTaskEvent::PullDiagnosticsResponse {
            target,
            uri,
            provider,
            result,
        } => {
            language_server::apply_pull_diagnostics_response(editor, result, provider, uri, target)
        }
        RuntimeTaskEvent::RequestDocumentColorsDebounced { doc_ids } => {
            for doc_id in doc_ids {
                language_server::request_document_colors(editor, doc_id, ingress.clone());
            }
        }
        RuntimeTaskEvent::RequestLspFeaturesDebounced { docs } => {
            use helix_view::handlers::lsp::LspFeatureRefreshKind;
            for (doc_id, kinds) in docs {
                for kind in kinds {
                    match kind {
                        LspFeatureRefreshKind::CodeLens => {
                            language_server::request_code_lenses(editor, doc_id, ingress.clone())
                        }
                        LspFeatureRefreshKind::DocumentLinks => {
                            language_server::request_document_links(editor, doc_id, ingress.clone())
                        }
                        LspFeatureRefreshKind::FoldingRanges => {
                            language_server::request_folding_ranges(editor, doc_id, ingress.clone())
                        }
                        LspFeatureRefreshKind::SemanticTokens => {
                            language_server::request_semantic_tokens(
                                editor,
                                doc_id,
                                ingress.clone(),
                            )
                        }
                        LspFeatureRefreshKind::InlineCompletion => {
                            let Some(view_id) = editor
                                .tree
                                .views()
                                .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
                            else {
                                continue;
                            };
                            language_server::request_inline_completion(
                                editor,
                                doc_id,
                                view_id,
                                false,
                                ingress.clone(),
                            )
                        }
                    }
                }
            }
        }
        RuntimeTaskEvent::QueuePullDiagnosticsForDocuments { document_ids } => {
            language_server::queue_document_diagnostics(
                editor,
                document_ids,
                None,
                ingress.clone(),
            );
        }
        RuntimeTaskEvent::QueuePullDiagnosticsInterFileSweep { language_servers } => {
            let documents: Vec<_> = editor.document_ids().collect();
            language_server::queue_document_diagnostics_for_language_servers(
                editor,
                documents,
                &language_servers,
                ingress.clone(),
            );
        }
        RuntimeTaskEvent::RequestSignatureDebounced {
            invoked,
            request,
            trigger_kind,
            is_retrigger,
            cancel,
        } => {
            language_server::request_signature_help(
                editor,
                invoked,
                request,
                trigger_kind,
                is_retrigger,
                cancel,
                ingress,
            );
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
            server_id,
            offset_encoding,
            id,
            hints,
        } => language_server::apply_inlay_hints(
            editor,
            view_id,
            doc_id,
            server_id,
            offset_encoding,
            id,
            hints,
        ),
        RuntimeTaskEvent::ApplyCodeLenses {
            doc_id,
            expected_version,
            request,
            lenses,
        } => language_server::apply_code_lenses(editor, doc_id, expected_version, &request, lenses),
        RuntimeTaskEvent::ApplySemanticTokens {
            doc_id,
            server_id,
            request,
            tokens,
        } => language_server::apply_semantic_tokens(editor, doc_id, server_id, &request, tokens),
        RuntimeTaskEvent::ApplyInlineCompletion {
            doc_id,
            request,
            completion,
        } => language_server::apply_inline_completion(editor, doc_id, &request, completion),
        RuntimeTaskEvent::ApplyInlineValues {
            doc_id,
            expected_version,
            request,
            values,
        } => {
            language_server::apply_inline_values(editor, doc_id, expected_version, &request, values)
        }
        RuntimeTaskEvent::RequestInlineValues { doc_id } => {
            language_server::request_inline_values(editor, doc_id, ingress.clone())
        }
        RuntimeTaskEvent::ApplyDocumentLinks {
            doc_id,
            expected_version,
            request,
            links,
        } => {
            language_server::apply_document_links(editor, doc_id, expected_version, &request, links)
        }
        RuntimeTaskEvent::ApplyFoldingRanges {
            doc_id,
            expected_version,
            request,
            folds,
        } => {
            language_server::apply_folding_ranges(editor, doc_id, expected_version, &request, folds)
        }
        RuntimeTaskEvent::ApplyLinkedEditingRanges {
            offset_encoding,
            ranges,
        } => language_server::apply_linked_editing_ranges(editor, offset_encoding, ranges),
        RuntimeTaskEvent::ApplyOnTypeFormatting {
            doc_id,
            view_id,
            expected_version,
            offset_encoding,
            edits,
        } => language_server::apply_on_type_formatting(
            editor,
            doc_id,
            view_id,
            expected_version,
            offset_encoding,
            edits,
        ),
        RuntimeTaskEvent::DapClientStartupCompleted {
            id,
            result,
            session,
        } => dap::apply_client_startup_completed(editor, ingress, id, *result, session),
        RuntimeTaskEvent::DapSessionStartupCompleted {
            client_id,
            parent,
            result,
        } => dap::apply_session_startup_completed(editor, ingress, client_id, parent, result),
        RuntimeTaskEvent::DapStoppedCompleted {
            client_id,
            generation,
            preferred_thread_id,
            stacks,
            errors,
        } => dap::apply_stopped_completed(
            editor,
            ingress,
            &foreground,
            client_id,
            generation,
            preferred_thread_id,
            stacks,
            errors,
        ),
        RuntimeTaskEvent::DapInitializedCompleted {
            client_id,
            generation,
            breakpoints,
            configuration_result,
        } => dap::apply_initialized_completed(
            editor,
            ingress,
            client_id,
            generation,
            breakpoints,
            configuration_result,
        ),
        RuntimeTaskEvent::DapDisconnectCompleted {
            client_id,
            generation,
            restart,
            connection_type,
            result,
        } => dap::apply_disconnect_completed(
            editor,
            ingress,
            client_id,
            generation,
            restart,
            connection_type,
            result,
        ),
        RuntimeTaskEvent::DapRelaunchCompleted {
            client_id,
            generation,
            result,
        } => dap::apply_relaunch_completed(editor, ingress, client_id, generation, result),
        RuntimeTaskEvent::DapAdapterReplyReady { parent, result } => {
            dap::apply_adapter_reply(editor, ingress, parent, result)
        }
        RuntimeTaskEvent::DapRequestFailed { client_id, message } => {
            dap::apply_request_failed(editor, client_id, message)
        }
        RuntimeTaskEvent::DapRestarted => dap::apply_dap_restarted(editor),
        RuntimeTaskEvent::ResumeDebuggerApplication => {
            dap::apply_resume_debugger_application(editor);
            for doc in editor.documents_mut() {
                doc.clear_inline_values();
            }
        }
        RuntimeTaskEvent::UnsetActiveDebugClient => {
            dap::apply_unset_active_debug_client(editor);
            for doc in editor.documents_mut() {
                doc.clear_inline_values();
            }
        }
        RuntimeTaskEvent::DapExceptionsConfigured => {}
        RuntimeTaskEvent::PkgEvent(event) => pkg::apply_event(editor, &event),
        RuntimeTaskEvent::RuntimeAssetsChanged(change) => {
            editor.reconcile_runtime_asset_change(&change);
            if change
                .changed_package_kinds
                .contains(helix_pkg::PkgKind::Acp.as_str())
            {
                refresh_assistant_agent_cache(editor, ingress.clone());
            }
            let changed_grammars = change
                .changed_asset_keys
                .iter()
                .filter(|asset| asset.kind == helix_loader::RuntimeAssetKind::Grammar)
                .map(|asset| asset.key.clone())
                .collect::<std::collections::BTreeSet<_>>();
            if !changed_grammars.is_empty() {
                let generation = change.generation;
                let loader_ingress = ingress.clone();
                let loader = editor
                    .runtime()
                    .block()
                    .spawn(helix_core::config::user_lang_loader);
                editor
                    .work()
                    .spawn(async move {
                        let task = match loader.await {
                            Ok(Ok(loader)) =>
                                RuntimeTaskEvent::ApplyRuntimeLanguageLoader(
                                    crate::runtime::PreparedLanguageLoader {
                                        generation,
                                        changed_grammars,
                                        loader,
                                    },
                                ),
                            Ok(Err(error)) => RuntimeTaskEvent::SetEditorError {
                                    message: format!(
                                        "failed to rebuild language loader for runtime generation {generation}: {error}"
                                    ),
                                },
                            Err(error) => RuntimeTaskEvent::SetEditorError {
                                    message: format!(
                                        "language loader task failed for runtime generation {generation}: {error}"
                                    ),
                                },
                        };
                        let _ = loader_ingress.send_task(task).await;
                    })
                    .detach();
            }
        }
        RuntimeTaskEvent::ApplyRuntimeLanguageLoader(prepared) => {
            let current_generation = helix_loader::runtime_assets()
                .map(helix_loader::RuntimeAssets::generation)
                .unwrap_or_default();
            if current_generation != prepared.generation {
                return;
            }
            let documents =
                editor.apply_runtime_language_loader(prepared.loader, &prepared.changed_grammars);
            for document in documents {
                crate::runtime::ui::document::queue_runtime_syntax(
                    editor,
                    document,
                    prepared.generation,
                    ingress.clone(),
                );
            }
        }
        RuntimeTaskEvent::ApplyConfigReload(_)
        | RuntimeTaskEvent::ConfigReloadFailed { .. }
        | RuntimeTaskEvent::ApplyPreparedLspDiagnostics { .. } => {
            unreachable!("application-owned runtime event reached editor effects")
        }
        RuntimeTaskEvent::ApplyAssistantAgents(prepared) => {
            if prepared.config_generation == editor.config_gen {
                editor.set_assistant_packaged_agents(prepared.generation, prepared.agents);
            }
        }
        RuntimeTaskEvent::PkgOperationFinished(outcome) => {
            for warning in &outcome.warnings {
                editor.notify_warning(warning.clone());
            }
            if outcome.is_success() {
                if let crate::runtime::PkgOperationOrigin::MissingLanguageServer {
                    documents, ..
                } = outcome.origin
                {
                    for document_id in documents {
                        editor.refresh_language_servers(document_id);
                    }
                }
            }
        }
        RuntimeTaskEvent::RefreshLanguageServers { document_ids } => {
            for document_id in document_ids {
                editor.refresh_language_servers(document_id);
            }
        }
        RuntimeTaskEvent::RestoreAssistantHistoryThread {
            record,
            activation,
            panel,
        } => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_restore_assistant_history_thread(
                editor,
                &foreground,
                *record,
                activation,
                panel,
            );
            emit_new_thread_events(editor, &plugin_runtime, &before);
        }
        RuntimeTaskEvent::ActivateAssistantThread { thread, panel } => {
            assistant::apply_activate_assistant_thread(editor, &foreground, thread, panel)
        }
        RuntimeTaskEvent::DetachAssistantContext { item } => {
            assistant::apply_detach_assistant_context(editor, item);
            if let Some(thread) = editor.assistant.active().map(adapt::thread_handle) {
                fire_plugin_event(
                    &plugin_runtime,
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
        RuntimeTaskEvent::PluginHostRequest {
            state,
            request,
            respond_to,
        } => plugin::apply_plugin_host_request(
            editor,
            ingress,
            &foreground,
            state,
            request,
            respond_to,
        ),
        RuntimeTaskEvent::RemoveAssistantPanel => assistant::apply_remove_assistant_panel(editor),
        RuntimeTaskEvent::ConnectAssistantBackend(connection) => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_connect_assistant_backend(editor, &foreground, *connection);
            emit_new_thread_events(editor, &plugin_runtime, &before);
        }
        RuntimeTaskEvent::CycleAssistantThread { delta } => {
            assistant::apply_cycle_assistant_thread(editor, &foreground, delta)
        }
        RuntimeTaskEvent::CloseActiveAssistantThread => {
            let thread = editor.assistant.active().map(adapt::thread_handle);
            assistant::apply_close_active_assistant_thread(editor, &foreground);
            if let Some(thread) = thread {
                fire_plugin_event(
                    &plugin_runtime,
                    events::PluginEvent::AssistantThreadClosed(
                        events::AssistantThreadClosedEvent { thread },
                    ),
                );
            }
        }
        RuntimeTaskEvent::NewAssistantThreadFromActiveBackend => {
            let before: Vec<_> = editor.assistant.threads().map(|t| t.id).collect();
            assistant::apply_new_assistant_thread_from_active_backend(editor, &foreground);
            emit_new_thread_events(editor, &plugin_runtime, &before);
        }
        RuntimeTaskEvent::ToggleActiveAssistantFollow => {
            assistant::apply_toggle_active_assistant_follow(editor)
        }
        RuntimeTaskEvent::AttachAssistantContext { item, status } => {
            let context_kind = context_kind_label(&item);
            assistant::apply_attach_assistant_context(editor, item, status);
            if let Some(thread) = editor.assistant.active().map(adapt::thread_handle) {
                fire_plugin_event(
                    &plugin_runtime,
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
                    &plugin_runtime,
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
        RuntimeTaskEvent::DeleteAssistantHistoryThread {
            thread,
            delete_remote,
        } => assistant::apply_delete_assistant_history_thread(editor, thread, delete_remote),
        RuntimeTaskEvent::FetchAssistantHistoryPage { scope, cursor } => {
            assistant::request_fetch_assistant_history_page(editor, scope, cursor)
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
        } => {
            dap::apply_select_stack_frame(
                editor,
                ingress.clone(),
                &foreground,
                thread_id,
                frame_id,
            );
        }
        RuntimeTaskEvent::ApplyStackFrames {
            client_id,
            generation,
            thread_id,
            frames,
            selection,
        } => dap::apply_stack_frames(
            editor,
            ingress.clone(),
            &foreground,
            client_id,
            generation,
            thread_id,
            frames,
            selection,
        ),
        RuntimeTaskEvent::ApplyBreakpointsResponse {
            client_id,
            path,
            expected,
            response,
        } => dap::apply_breakpoints_response(editor, client_id, path, expected, response),
        RuntimeTaskEvent::ExecuteLspCommand { command, server_id } => {
            language_server::apply_execute_lsp_command(editor, command, server_id)
        }
        RuntimeTaskEvent::ApplyResolvedCodeLens {
            doc_id,
            expected_version,
            server_id,
            original,
            resolved,
        } => language_server::apply_resolved_code_lens(
            editor,
            doc_id,
            expected_version,
            server_id,
            original,
            resolved,
            &foreground,
        ),
        RuntimeTaskEvent::OpenResolvedDocumentLink {
            doc_id,
            expected_version,
            target,
            action,
        } => language_server::apply_resolved_document_link(
            editor,
            doc_id,
            expected_version,
            target,
            action,
            ingress,
            &foreground,
        ),
        RuntimeTaskEvent::ApplyRenameEdit {
            doc_id,
            expected_version,
            offset_encoding,
            workspace_edit,
        } => language_server::apply_rename_edit(
            editor,
            ingress,
            doc_id,
            expected_version,
            offset_encoding,
            workspace_edit,
        ),
        RuntimeTaskEvent::ApplyCodeAction {
            offset_encoding,
            workspace_edit,
            command,
            server_id,
        } => language_server::apply_code_action(
            editor,
            ingress,
            offset_encoding,
            workspace_edit,
            command,
            server_id,
        ),
        RuntimeTaskEvent::SetBreakpointCondition {
            path,
            index,
            condition,
        } => dap::apply_breakpoint_condition(editor, path, index, condition, ingress),
        RuntimeTaskEvent::SetBreakpointLogMessage {
            path,
            index,
            log_message,
        } => dap::apply_breakpoint_log_message(editor, path, index, log_message, ingress),
        RuntimeTaskEvent::ToggleBreakpoint { path, line } => {
            dap::apply_toggle_breakpoint(editor, path, line, ingress)
        }
        RuntimeTaskEvent::AutoSaveRun { save_pending } => {
            apply_auto_save_debounce(editor, save_pending);
        }
        RuntimeTaskEvent::AutoReloadRun {
            documents,
            reload_pending,
        } => {
            apply_auto_reload(editor, ingress, documents, reload_pending);
        }
    }
}

pub(crate) fn apply_exit_task_result(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
    result: ExitTaskResult,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(task)) => {
            apply_runtime_task_event(editor, ingress, foreground, plugin_runtime, task);
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

pub(crate) fn apply_auto_reload(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    documents: BTreeSet<DocumentId>,
    reload_pending: Arc<std::sync::Mutex<BTreeSet<DocumentId>>>,
) {
    if editor.mode() == Mode::Insert {
        reload_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(documents);
        return;
    }

    let mut documents = documents;
    documents.extend(std::mem::take(
        &mut *reload_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ));
    let documents = auto_reload_candidates(editor, documents);
    crate::runtime::ui::document::queue_document_reloads(
        editor,
        &ingress,
        documents,
        crate::runtime::DocumentReloadOrigin::Auto,
    );
}

fn auto_reload_candidates(
    editor: &Editor,
    documents: impl IntoIterator<Item = DocumentId>,
) -> Vec<DocumentId> {
    documents
        .into_iter()
        .filter(|document| {
            editor
                .document(*document)
                .is_some_and(|doc| !doc.is_modified())
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_view::{
        editor::{Action, Config},
        graphics::Rect,
        handlers::Handlers,
        theme,
    };

    use super::*;

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        Editor::new(
            Rect::new(0, 0, 100, 30),
            Arc::new(theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(
                helix_core::config::default_lang_loader(),
            )),
            Arc::new(ArcSwap::from_pointee(Config::default())),
            runtime,
            Handlers::dummy(),
        )
    }

    #[tokio::test]
    async fn auto_reload_never_targets_a_modified_document() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("document.txt");
        std::fs::write(&path, "disk").unwrap();
        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime);
        let document = editor.open(&path, Action::VerticalSplit).unwrap();
        let view = editor.focused_view_id();

        assert_eq!(auto_reload_candidates(&editor, [document]), vec![document]);

        let doc = editor.document_mut(document).unwrap();
        let transaction = Transaction::insert(doc.text(), doc.selection(view), "edit".into());
        doc.apply(&transaction, view);

        assert!(auto_reload_candidates(&editor, [document]).is_empty());
    }
}
