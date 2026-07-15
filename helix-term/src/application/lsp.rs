use helix_core::{diagnostic::DiagnosticProvider, Uri};
use helix_lsp::{
    self,
    lsp::{self, notification::Notification as LspNotification},
    LanguageServerId, MethodCall, Notification, ServerEvent, ServerRequest, ServerRequestError,
};
use serde_json::json;

use super::Application;
use crate::ui;

struct SlowLspUiCall<'a> {
    server_id: LanguageServerId,
    kind: &'static str,
    method: &'a str,
    started_at: std::time::Instant,
}

fn notification_method(notification: &Notification) -> &'static str {
    match notification {
        Notification::Initialized => "initialized",
        Notification::Exit => "exit",
        Notification::PublishDiagnostics(_) => lsp::notification::PublishDiagnostics::METHOD,
        Notification::ShowMessage(_) => lsp::notification::ShowMessage::METHOD,
        Notification::LogMessage(_) => lsp::notification::LogMessage::METHOD,
        Notification::ProgressMessage(_) => lsp::notification::Progress::METHOD,
    }
}

impl<'a> SlowLspUiCall<'a> {
    fn new(server_id: LanguageServerId, kind: &'static str, method: &'a str) -> Self {
        Self {
            server_id,
            kind,
            method,
            started_at: std::time::Instant::now(),
        }
    }
}

impl Drop for SlowLspUiCall<'_> {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        if elapsed >= super::SLOW_LSP_EVENT_THRESHOLD {
            log::info!(
                target: crate::ui::picker::PICKER_TRACE_TARGET,
                "phase=lsp_ui_slow server_id={} kind={} method={} elapsed_us={}",
                self.server_id,
                self.kind,
                self.method,
                elapsed.as_micros(),
            );
        }
    }
}

impl Application {
    pub fn handle_language_server_message(
        &mut self,
        call: ServerEvent,
        server_id: LanguageServerId,
    ) {
        macro_rules! language_server {
            () => {
                match self.editor.language_server_by_id(server_id) {
                    Some(language_server) => language_server,
                    None => {
                        log::warn!("can't find language server with id `{}`", server_id);
                        return;
                    }
                }
            };
        }

        match call {
            ServerEvent::Notification(notification) => {
                let _slow_call = SlowLspUiCall::new(
                    server_id,
                    "notification",
                    notification_method(&notification),
                );
                match notification {
                    Notification::Initialized => {
                        if !self
                            .editor
                            .mark_language_server_initialization_dispatched(server_id)
                        {
                            log::debug!(
                                "ignoring duplicate initialization completion from language server {server_id}"
                            );
                            return;
                        }
                        let language_server = language_server!();

                        if let Some(config) = language_server.config() {
                            language_server.did_change_configuration(config.clone());
                        }

                        self.editor.dispatch_language_server_initialized(server_id);
                    }
                    Notification::PublishDiagnostics(params) => {
                        self.queue_lsp_diagnostics(server_id, params);
                    }
                    Notification::ShowMessage(params) => {
                        self.handle_show_message(params.typ, params.message);
                    }
                    Notification::LogMessage(params) => {
                        log::debug!("window/logMessage: {:?}", params);

                        if self.config.load().editor.lsp.display_messages {
                            match params.typ {
                                lsp::MessageType::ERROR => {
                                    self.editor.notify_error(params.message);
                                }
                                lsp::MessageType::WARNING => {
                                    self.editor.notify_warning(params.message);
                                }
                                _ => {}
                            };
                        }
                    }
                    Notification::ProgressMessage(params)
                        if !self
                            .compositor
                            .has_component(std::any::type_name::<ui::Prompt>()) =>
                    {
                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let lsp::ProgressParams {
                            token,
                            value: lsp::ProgressParamsValue::WorkDone(work),
                        } = params;
                        let (title, message, percentage) = match &work {
                            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                                title,
                                message,
                                percentage,
                                ..
                            }) => (Some(title), message, percentage),
                            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                                message,
                                percentage,
                                ..
                            }) => (None, message, percentage),
                            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message }) => {
                                if message.is_some() {
                                    (None, message, &None)
                                } else {
                                    self.language.progress.end_progress(server_id, &token);
                                    if !self.language.progress.is_progressing(server_id) {
                                        editor_view.spinners_mut().get_or_create(server_id).stop();
                                    }
                                    self.editor.clear_status();
                                    return;
                                }
                            }
                        };

                        if self.editor.config().lsp.display_progress_messages {
                            let title =
                                title.or_else(|| self.language.progress.title(server_id, &token));
                            if title.is_some() || percentage.is_some() || message.is_some() {
                                use std::fmt::Write as _;
                                let mut status = format!("{}: ", language_server!().name());
                                if let Some(percentage) = percentage {
                                    write!(status, "{percentage:>2}% ").unwrap();
                                }
                                if let Some(title) = title {
                                    status.push_str(title);
                                }
                                if title.is_some() && message.is_some() {
                                    status.push_str(" ⋅ ");
                                }
                                if let Some(message) = message {
                                    status.push_str(message);
                                }
                                self.editor.set_status(status);
                            }
                        }

                        editor_view
                            .spinners_mut()
                            .get_or_create(server_id)
                            .set_progress(percentage.map(|percentage| percentage.min(100) as u8));

                        match work {
                            lsp::WorkDoneProgress::Begin(begin_status) => {
                                self.language.progress.begin(
                                    server_id,
                                    token.clone(),
                                    begin_status,
                                );
                            }
                            lsp::WorkDoneProgress::Report(report_status) => {
                                self.language.progress.update(
                                    server_id,
                                    token.clone(),
                                    report_status,
                                );
                            }
                            lsp::WorkDoneProgress::End(_) => {
                                self.language.progress.end_progress(server_id, &token);
                                if !self.language.progress.is_progressing(server_id) {
                                    editor_view.spinners_mut().get_or_create(server_id).stop();
                                };
                            }
                        }
                    }
                    Notification::ProgressMessage(_params) => {}
                    Notification::Exit => {
                        self.editor.set_status("Language server exited");
                        self.editor.remove_language_server_diagnostics(server_id);
                        self.editor
                            .clear_language_server_document_diagnostics(server_id);
                        for doc in self.editor.documents_mut() {
                            doc.clear_pull_diagnostics_server(server_id);
                        }
                        self.ingress().tx.pull_diagnostics_server_exited(server_id);

                        self.editor.dispatch_language_server_exited(server_id);

                        self.language
                            .diagnostics_generations
                            .retain(|(id, _), _| *id != server_id);

                        self.editor.remove_language_server(server_id);
                    }
                }
            }
            ServerEvent::Request(ServerRequest {
                method,
                request,
                id,
            }) => {
                let _slow_call = SlowLspUiCall::new(server_id, "request", method.as_str());
                let reply = match request {
                    Err(ServerRequestError::MethodNotFound) => {
                        log::error!(
                            "Language Server: Method {} not found in request {}",
                            method,
                            id
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::MethodNotFound,
                            message: format!("Method not found: {}", method),
                            data: None,
                        })
                    }
                    Err(ServerRequestError::Malformed(err)) => {
                        log::error!(
                            "Language Server: Received malformed method call {} in request {}: {}",
                            method,
                            id,
                            err
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::ParseError,
                            message: format!("Malformed method call: {}", method),
                            data: None,
                        })
                    }
                    Ok(MethodCall::WorkDoneProgressCreate(params)) => {
                        self.language.progress.create(server_id, params.token);

                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let spinner = editor_view.spinners_mut().get_or_create(server_id);
                        if spinner.is_stopped() {
                            spinner.start();
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ApplyWorkspaceEdit(params)) => {
                        let language_server = language_server!();
                        if language_server.is_initialized() {
                            let offset_encoding = language_server.offset_encoding();
                            let ingress = self.ingress_sender();
                            let _ = crate::effect::file_operation::apply_workspace_edit(
                                &mut self.editor,
                                ingress,
                                offset_encoding,
                                &params.edit,
                                Some(
                                    helix_view::editor::WorkspaceEditContinuation::ApplyEditReply {
                                        server_id,
                                        request_id: id.clone(),
                                    },
                                ),
                            );
                            return;
                        } else {
                            Err(helix_lsp::jsonrpc::Error {
                                code: helix_lsp::jsonrpc::ErrorCode::InvalidRequest,
                                message: "Server must be initialized to request workspace edits"
                                    .to_string(),
                                data: None,
                            })
                        }
                    }
                    Ok(MethodCall::WorkspaceFolders) => {
                        Ok(json!(&*language_server!().workspace_folders()))
                    }
                    Ok(MethodCall::WorkspaceConfiguration(params)) => {
                        let language_server = language_server!();
                        let result: Vec<_> = params
                            .items
                            .iter()
                            .map(|item| {
                                let mut config = language_server.config()?;
                                if let Some(section) = item.section.as_ref() {
                                    if !section.is_empty() {
                                        for part in section.split('.') {
                                            config = config.get(part)?;
                                        }
                                    }
                                }
                                Some(config)
                            })
                            .collect();
                        Ok(json!(result))
                    }
                    Ok(MethodCall::RegisterCapability(params)) => {
                        if let Some(client) = self.editor.language_server_client(server_id).cloned()
                        {
                            for reg in params.registrations {
                                match reg.method.as_str() {
                                    lsp::notification::DidChangeWatchedFiles::METHOD => {
                                        let Some(options) = reg.register_options else {
                                            continue;
                                        };
                                        let ops: lsp::DidChangeWatchedFilesRegistrationOptions =
                                            match serde_json::from_value(options) {
                                                Ok(ops) => ops,
                                                Err(err) => {
                                                    log::warn!(
                                                        "Failed to deserialize DidChangeWatchedFilesRegistrationOptions: {err}"
                                                    );
                                                    continue;
                                                }
                                            };
                                        self.editor.register_language_server_file_watch(
                                            client.id(),
                                            &client,
                                            reg.id,
                                            ops,
                                        )
                                    }
                                    _ => {
                                        log::warn!(
                                            "Ignoring a client/registerCapability request because dynamic capability registration is not enabled. Please report this upstream to the language server"
                                        );
                                    }
                                }
                            }
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::UnregisterCapability(params)) => {
                        for unreg in params.unregisterations {
                            match unreg.method.as_str() {
                                lsp::notification::DidChangeWatchedFiles::METHOD => {
                                    self.editor
                                        .unregister_language_server_file_watch(server_id, unreg.id);
                                }
                                _ => {
                                    log::warn!(
                                        "Received unregistration request for unsupported method: {}",
                                        unreg.method
                                    );
                                }
                            }
                        }
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowDocument(params)) => {
                        let language_server = language_server!();
                        let offset_encoding = language_server.offset_encoding();

                        let result = self.handle_show_document(params, offset_encoding);
                        Ok(json!(result))
                    }
                    Ok(MethodCall::WorkspaceDiagnosticRefresh) => {
                        let language_server = language_server!().id();
                        let documents = self
                            .editor
                            .documents_supporting_language_server(language_server);
                        let ingress = self.ingress().tx.clone();
                        let language_servers = std::collections::HashSet::from([language_server]);
                        crate::effect::language_server::queue_document_diagnostics_for_language_servers(
                            &mut self.editor,
                            documents,
                            &language_servers,
                            ingress,
                        );

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::SemanticTokensRefresh) => {
                        let language_server = language_server!().id();
                        let documents = self
                            .editor
                            .documents_supporting_language_server(language_server);
                        let ingress = self.ingress().tx.clone();

                        for document in documents {
                            crate::effect::language_server::request_semantic_tokens(
                                &mut self.editor,
                                document,
                                ingress.clone(),
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::CodeLensRefresh) => {
                        let language_server = language_server!().id();
                        let documents = self
                            .editor
                            .documents_supporting_language_server(language_server);
                        let ingress = self.ingress().tx.clone();

                        for document in documents {
                            crate::effect::language_server::request_code_lenses(
                                &mut self.editor,
                                document,
                                ingress.clone(),
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::InlayHintRefresh) => {
                        for doc in self.editor.documents_mut() {
                            doc.mark_inlay_hints_outdated();
                        }
                        let ingress = self.ingress().tx.clone();
                        crate::commands::compute_inlay_hints_for_all_views(
                            &mut self.editor,
                            ingress,
                        );
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::InlineValueRefresh) => {
                        let language_server = language_server!().id();
                        let documents = self
                            .editor
                            .documents_supporting_language_server(language_server);
                        let ingress = self.ingress().tx.clone();

                        for document in documents {
                            crate::effect::language_server::request_inline_values(
                                &mut self.editor,
                                document,
                                ingress.clone(),
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowMessageRequest(params)) => {
                        if let Some(actions) = params.actions.filter(|a| !a.is_empty()) {
                            let id = id.clone();
                            let select = ui::Select::new(
                                params.message,
                                actions,
                                (),
                                move |editor, action, event| {
                                    let reply = match event {
                                        ui::PromptEvent::Update => return,
                                        ui::PromptEvent::Validate => Some(action.clone()),
                                        ui::PromptEvent::Abort => None,
                                    };
                                    if let Some(language_server) =
                                        editor.language_server_by_id(server_id)
                                    {
                                        if let Err(err) =
                                            language_server.reply(id.clone(), Ok(json!(reply)))
                                        {
                                            log::error!(
                                                "Failed to send reply to server '{}' request {id}: {err}",
                                                language_server.name()
                                            );
                                        }
                                    }
                                },
                            );
                            self.compositor
                                .replace_or_push("lsp-show-message-request", select);
                            return;
                        } else {
                            self.handle_show_message(params.typ, params.message);
                            Ok(serde_json::Value::Null)
                        }
                    }
                };

                let language_server = language_server!();
                if let Err(err) = language_server.reply(id.clone(), reply) {
                    log::error!(
                        "Failed to send reply to server '{}' request {id}: {err}",
                        language_server.name()
                    );
                }
            }
            ServerEvent::Invalid { id } => {
                let _slow_call = SlowLspUiCall::new(server_id, "invalid", "<invalid>");
                log::error!("LSP invalid method call id={:?}", id);
            }
        }
    }

    fn queue_lsp_diagnostics(
        &mut self,
        server_id: LanguageServerId,
        params: lsp::PublishDiagnosticsParams,
    ) {
        let uri = match Uri::try_from(params.uri) {
            Ok(uri) => uri,
            Err(error) => {
                log::error!("{error}");
                return;
            }
        };
        let Some(language_server) = self.editor.language_server_by_id(server_id) else {
            log::warn!("can't find language server with id `{server_id}`");
            return;
        };
        if !language_server.is_initialized() {
            log::error!(
                "discarding diagnostics from uninitialized language server '{}'",
                language_server.name()
            );
            return;
        }

        let provider = DiagnosticProvider::Lsp {
            server_id,
            identifier: None,
        };
        let Some(work) = self.editor.prepare_lsp_diagnostics(
            provider,
            uri.clone(),
            params.version,
            params.diagnostics,
        ) else {
            return;
        };
        let generation = self
            .language
            .diagnostics_generations
            .entry((server_id, uri.clone()))
            .and_modify(|generation| *generation = generation.wrapping_add(1))
            .or_insert(1)
            .to_owned();
        self.spawn_lsp_diagnostics_work(server_id, uri, generation, work);
    }

    fn spawn_lsp_diagnostics_work(
        &self,
        server_id: LanguageServerId,
        uri: Uri,
        generation: u64,
        work: helix_view::handlers::lsp::LspDiagnosticsWork,
    ) {
        let blocking = self.runtime.block().spawn(move || work.execute());
        let ingress = self.ingress_sender();
        self.runtime
            .work()
            .spawn(async move {
                let started_at = std::time::Instant::now();
                let prepared = match blocking.await {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        log::error!(
                            "language-server diagnostics preparation failed for {uri:?}: {error}"
                        );
                        return;
                    }
                };
                let elapsed = started_at.elapsed();
                if elapsed >= std::time::Duration::from_millis(4) {
                    log::info!(
                        target: crate::ui::picker::PICKER_TRACE_TARGET,
                        "phase=lsp_diagnostics_prepare_slow server_id={} elapsed_us={}",
                        server_id,
                        elapsed.as_micros(),
                    );
                }
                let _ = ingress
                    .send_task(
                        crate::runtime::RuntimeTaskEvent::ApplyPreparedLspDiagnostics {
                            server_id,
                            uri,
                            generation,
                            prepared,
                        },
                    )
                    .await;
            })
            .detach();
    }

    pub(super) fn apply_prepared_lsp_diagnostics(
        &mut self,
        server_id: LanguageServerId,
        uri: Uri,
        generation: u64,
        prepared: helix_view::handlers::lsp::PreparedLspDiagnostics,
    ) {
        if self
            .language
            .diagnostics_generations
            .get(&(server_id, uri.clone()))
            .copied()
            != Some(generation)
        {
            return;
        }

        let started_at = std::time::Instant::now();
        let result = self.editor.apply_prepared_lsp_diagnostics(prepared);
        let elapsed = started_at.elapsed();
        if elapsed >= std::time::Duration::from_millis(4) {
            log::info!(
                target: crate::ui::picker::PICKER_TRACE_TARGET,
                "phase=lsp_diagnostics_commit_slow server_id={} elapsed_us={}",
                server_id,
                elapsed.as_micros(),
            );
        }
        match result {
            helix_view::handlers::lsp::LspDiagnosticsApply::Done {
                retired_document_diagnostics,
                retired_workspace_diagnostics,
            } => {
                if retired_document_diagnostics.is_some() || retired_workspace_diagnostics.is_some()
                {
                    self.runtime
                        .block()
                        .spawn(move || {
                            drop(retired_document_diagnostics);
                            drop(retired_workspace_diagnostics);
                        })
                        .detach();
                }
            }
            helix_view::handlers::lsp::LspDiagnosticsApply::Retry(work) => {
                self.spawn_lsp_diagnostics_work(server_id, uri, generation, work);
            }
        }
    }

    fn handle_show_message(&mut self, message_type: lsp::MessageType, message: String) {
        if self.config.load().editor.lsp.display_messages {
            match message_type {
                lsp::MessageType::ERROR => self.editor.set_error(message),
                lsp::MessageType::WARNING => self.editor.set_warning(message),
                _ => self.editor.set_status(message),
            }
        }
    }

    fn handle_show_document(
        &mut self,
        params: lsp::ShowDocumentParams,
        offset_encoding: helix_lsp::OffsetEncoding,
    ) -> lsp::ShowDocumentResult {
        if let lsp::ShowDocumentParams {
            external: Some(true),
            uri,
            ..
        } = params
        {
            crate::runtime::ingress::spawn_task_event_with_future(
                self.runtime.work().clone(),
                crate::open_external_url_task_event(uri),
                self.ingress().tx.clone(),
            );
            return lsp::ShowDocumentResult { success: true };
        };

        let lsp::ShowDocumentParams {
            uri,
            selection,
            take_focus,
            ..
        } = params;

        let uri = match Uri::try_from(uri) {
            Ok(uri) => uri,
            Err(err) => {
                log::error!("{err}");
                return lsp::ShowDocumentResult { success: false };
            }
        };
        let Some(path) = uri.as_path() else {
            log::error!("language server requested a non-file URI: {uri:?}");
            return lsp::ShowDocumentResult { success: false };
        };

        let action = match take_focus {
            Some(true) => helix_view::editor::Action::Replace,
            _ => helix_view::editor::Action::VerticalSplit,
        };

        let target = self.editor.focused_view_id();
        crate::runtime::ui::document::queue_document_open(
            &mut self.editor,
            &self.ingress.tx,
            &self.foreground,
            crate::runtime::DocumentOpenRequest {
                path: path.to_path_buf(),
                action,
                lane: crate::runtime::DocumentOpenLane::Navigation,
                target: crate::runtime::DocumentOpenTarget::View(target),
                selection: selection.map_or(crate::runtime::DocumentOpenSelection::None, |range| {
                    crate::runtime::DocumentOpenSelection::LspRange {
                        range,
                        offset_encoding,
                    }
                }),
                alignment: crate::runtime::DocumentOpenAlignment::CenterIfAction,
                default_folding_if_new: false,
                fff_record: None,
                external_if_binary: None,
                post_action: crate::runtime::DocumentOpenPostAction::None,
                completion: crate::runtime::DocumentOpenCompletionTarget::Editor,
            },
        );
        lsp::ShowDocumentResult { success: true }
    }
}
