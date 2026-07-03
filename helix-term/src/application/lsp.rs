use helix_core::{diagnostic::DiagnosticProvider, Uri};
use helix_lsp::{
    self,
    lsp::{self, notification::Notification as LspNotification},
    Call, LanguageServerId, MethodCall, Notification,
};
use serde_json::json;

use super::Application;
use crate::ui;

impl Application {
    pub async fn handle_language_server_message(
        &mut self,
        call: helix_lsp::Call,
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
            Call::Notification(helix_lsp::jsonrpc::Notification { method, params, .. }) => {
                let notification = match Notification::parse(&method, params) {
                    Ok(notification) => notification,
                    Err(helix_lsp::Error::Unhandled) => {
                        log::info!("Ignoring Unhandled notification from Language Server");
                        return;
                    }
                    Err(err) => {
                        log::error!(
                            "Ignoring unknown notification from Language Server: {}",
                            err
                        );
                        return;
                    }
                };

                match notification {
                    Notification::Initialized => {
                        let language_server = language_server!();

                        if let Some(config) = language_server.config() {
                            language_server.did_change_configuration(config.clone());
                        }

                        self.editor.dispatch_language_server_initialized(server_id);
                    }
                    Notification::PublishDiagnostics(params) => {
                        let uri = match Uri::try_from(params.uri) {
                            Ok(uri) => uri,
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            }
                        };
                        let language_server = language_server!();
                        if !language_server.is_initialized() {
                            log::error!(
                                "Discarding publishDiagnostic notification sent by an uninitialized server: {}",
                                language_server.name()
                            );
                            return;
                        }
                        let provider = DiagnosticProvider::Lsp {
                            server_id,
                            identifier: None,
                        };
                        self.editor.handle_lsp_diagnostics(
                            &provider,
                            uri,
                            params.version,
                            params.diagnostics,
                        );
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

                        self.editor.dispatch_language_server_exited(server_id);

                        self.editor.remove_language_server(server_id);
                    }
                }
            }
            Call::MethodCall(helix_lsp::jsonrpc::MethodCall {
                method, params, id, ..
            }) => {
                let reply = match MethodCall::parse(&method, params) {
                    Err(helix_lsp::Error::Unhandled) => {
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
                    Err(err) => {
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
                            let res = self
                                .editor
                                .apply_workspace_edit(offset_encoding, &params.edit);

                            Ok(json!(lsp::ApplyWorkspaceEditResponse {
                                applied: res.is_ok(),
                                failure_reason: res.as_ref().err().map(|err| err.kind.to_string()),
                                failed_change: res
                                    .as_ref()
                                    .err()
                                    .map(|err| err.failed_change_idx as u32),
                            }))
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
                        Ok(json!(&*language_server!().workspace_folders().await))
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
                                                    log::warn!("Failed to deserialize DidChangeWatchedFilesRegistrationOptions: {err}");
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
                                        log::warn!("Ignoring a client/registerCapability request because dynamic capability registration is not enabled. Please report this upstream to the language server");
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

                        for document in documents {
                            crate::effect::language_server::request_document_diagnostics(
                                &mut self.editor,
                                document,
                                ingress.clone(),
                            );
                        }

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
            Call::Invalid { id } => log::error!("LSP invalid method call id={:?}", id),
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
        let path = uri.as_path().expect("URIs are valid paths");

        let action = match take_focus {
            Some(true) => helix_view::editor::Action::Replace,
            _ => helix_view::editor::Action::VerticalSplit,
        };

        match self
            .editor
            .show_document(helix_view::editor::ShowDocumentRequest {
                path: path.to_path_buf(),
                action,
                selection,
                offset_encoding,
            }) {
            Ok(()) => lsp::ShowDocumentResult { success: true },
            Err(err) => {
                log::error!("failed to open path: {:?}: {:?}", uri, err);
                lsp::ShowDocumentResult { success: false }
            }
        }
    }
}
