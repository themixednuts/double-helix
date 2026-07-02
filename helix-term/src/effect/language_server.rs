use std::{collections::HashSet, time::Duration};

use futures_util::{
    stream::{FuturesOrdered, FuturesUnordered},
    StreamExt,
};
use helix_core::{
    diagnostic::DiagnosticProvider, syntax::config::LanguageServerFeature,
    text_annotations::InlineAnnotation, Uri,
};
use helix_lsp::{self, lsp, util::lsp_range_to_range, LanguageServerId};
use helix_runtime::Token;
use helix_view::{
    document::{DocumentInlayHints, DocumentInlayHintsId},
    document_lsp::DocumentColorSwatches,
    handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId},
    DocumentId, Editor, Theme, ViewId,
};

use crate::runtime::{
    ingress::{send_task_event_with, send_ui_command_with},
    ui::command::LspCodeActionItem,
    RuntimeTaskEvent, UiCommand,
};

pub(crate) fn request_document_diagnostics_for_language_servers(
    editor: &mut Editor,
    doc_id: DocumentId,
    language_servers: HashSet<LanguageServerId>,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    let cancel = doc.restart_pull_diagnostics();

    let mut futures: FuturesUnordered<_> = language_servers
        .iter()
        .filter_map(|server_id| {
            doc.language_servers()
                .find(|server| &server.id() == server_id)
        })
        .filter_map(|language_server| {
            let future = language_server.text_document_diagnostic(
                doc.identifier(),
                doc.previous_diagnostic_id().map(ToOwned::to_owned),
            )?;

            let identifier = language_server
                .capabilities()
                .diagnostic_provider
                .as_ref()
                .and_then(|diagnostic_provider| match diagnostic_provider {
                    lsp::DiagnosticServerCapabilities::Options(options) => {
                        options.identifier.clone()
                    }
                    lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                        options.diagnostic_options.identifier.clone()
                    }
                });

            let provider = DiagnosticProvider::Lsp {
                server_id: language_server.id(),
                identifier,
            };
            let uri = doc.uri()?;

            Some(async move {
                let result = future.await;
                (result, provider, uri)
            })
        })
        .collect();

    if futures.is_empty() {
        return;
    }

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            let mut retry_language_servers = HashSet::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.canceled() => return,
                    next = futures.next() => next,
                };
                match next {
                    Some((Ok(result), provider, uri)) => {
                        send_task_event_with(
                            RuntimeTaskEvent::PullDiagnosticsResponse {
                                doc_id,
                                uri,
                                provider,
                                result,
                            },
                            ingress.clone(),
                        )
                        .await;
                    }
                    Some((Err(err), DiagnosticProvider::Lsp { server_id, .. }, _)) => {
                        let parsed_cancellation_data = if let helix_lsp::Error::Rpc(error) = err {
                            error.data.and_then(|data| {
                                serde_json::from_value::<lsp::DiagnosticServerCancellationData>(
                                    data,
                                )
                                .ok()
                            })
                        } else {
                            log::error!("Pull diagnostic request failed: {err}");
                            continue;
                        };
                        if parsed_cancellation_data.is_some_and(|data| data.retrigger_request) {
                            retry_language_servers.insert(server_id);
                        }
                    }
                    None => break,
                }
            }

            if !retry_language_servers.is_empty() {
                tokio::select! {
                    _ = cancel.canceled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }

                send_task_event_with(
                    RuntimeTaskEvent::RetryPullDiagnostics {
                        doc_id,
                        language_servers: retry_language_servers,
                    },
                    ingress,
                )
                .await;
            }
        })
        .detach();
}

pub(crate) fn request_document_diagnostics(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(doc) = editor.document(doc_id) else {
        return;
    };

    let language_servers = doc
        .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
        .map(|language_server| language_server.id())
        .collect();

    request_document_diagnostics_for_language_servers(editor, doc_id, language_servers, ingress);
}

pub(crate) fn request_document_colors(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.display_color_swatches {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    let cancel = doc.restart_color_swatches();

    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::DocumentColors)
        .filter(|language_server| seen_language_servers.insert(language_server.id()))
        .map(|language_server| {
            let text = doc.text().clone();
            let offset_encoding = language_server.offset_encoding();
            let future = language_server
                .text_document_document_color(doc.identifier(), None)
                .unwrap();

            async move {
                let colors: Vec<_> = future
                    .await?
                    .into_iter()
                    .filter_map(|color_info| {
                        let pos = helix_lsp::util::lsp_pos_to_pos(
                            &text,
                            color_info.range.start,
                            offset_encoding,
                        )?;
                        Some((pos, color_info.color))
                    })
                    .collect();
                anyhow::Ok(colors)
            }
        })
        .collect();

    if futures.is_empty() {
        return;
    }

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            let mut all_colors = Vec::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.canceled() => return,
                    next = futures.next() => next,
                };
                match next {
                    Some(Ok(items)) => all_colors.extend(items),
                    Some(Err(err)) => log::error!("document color request failed: {err}"),
                    None => break,
                }
            }
            send_task_event_with(
                RuntimeTaskEvent::AttachDocumentColors {
                    doc_id,
                    colors: all_colors,
                },
                ingress,
            )
            .await;
        })
        .detach();
}

pub(crate) fn request_signature_help(
    editor: &mut Editor,
    invoked: SignatureHelpInvoked,
    request: SignatureHelpRequestId,
    cancel: Token,
    ingress: crate::runtime::RuntimeIngress,
) {
    let (view_id, doc) = focused!(editor);

    let future = doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .find_map(|language_server| {
            let pos = doc.position(view_id, language_server.offset_encoding());
            language_server.text_document_signature_help(doc.identifier(), pos, None)
        });

    let Some(future) = future else {
        if invoked == SignatureHelpInvoked::Manual {
            editor.set_error("No configured language server supports signature-help");
        }
        return;
    };

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            tokio::select! {
                _ = cancel.canceled() => {}
                res = future => match res {
                    Ok(response) => {
                        send_ui_command_with(
                            UiCommand::Lsp(crate::runtime::ui::command::LspCommand::SignatureHelp {
                                invoked,
                                request,
                                response,
                            }),
                            ingress,
                        )
                        .await
                    }
                    Err(err) => log::error!("signature help request failed: {err}"),
                }
            }
        })
        .detach();
}

pub(crate) fn apply_execute_lsp_command(
    editor: &mut Editor,
    command: lsp::Command,
    server_id: LanguageServerId,
) {
    let Some(future) = editor
        .language_server_by_id(server_id)
        .and_then(|server| server.command(command))
    else {
        editor.set_error("Language server does not support executing commands");
        return;
    };

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            if let Err(err) = future.await {
                log::error!("Error executing LSP command: {err}");
            }
        })
        .detach();
}

pub(crate) fn request_apply_code_action(
    editor: &mut Editor,
    item: LspCodeActionItem,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(language_server) = editor.language_server_by_id(item.language_server_id) else {
        editor.set_error("Language Server disappeared");
        return;
    };
    let offset_encoding = language_server.offset_encoding();

    match item.lsp_item {
        lsp::CodeActionOrCommand::Command(command) => {
            ingress.task(RuntimeTaskEvent::ExecuteLspCommand {
                command,
                server_id: item.language_server_id,
            });
        }
        lsp::CodeActionOrCommand::CodeAction(code_action) => {
            let server_id = item.language_server_id;

            if code_action.edit.is_none() || code_action.command.is_none() {
                if let Some(future) = language_server.resolve_code_action(&code_action) {
                    crate::runtime::ingress::spawn_task_event_with_future(
                        editor.work(),
                        async move {
                            let resolved = future.await.ok();
                            let code_action = resolved.as_ref().unwrap_or(&code_action);
                            Ok(RuntimeTaskEvent::ApplyCodeAction {
                                offset_encoding,
                                workspace_edit: code_action.edit.clone(),
                                command: code_action.command.clone(),
                                server_id,
                            })
                        },
                        ingress,
                    );
                    return;
                }
            }

            ingress.task(RuntimeTaskEvent::ApplyCodeAction {
                offset_encoding,
                workspace_edit: code_action.edit,
                command: code_action.command,
                server_id,
            });
        }
    }
}

pub(crate) fn apply_code_action(
    editor: &mut Editor,
    offset_encoding: helix_lsp::OffsetEncoding,
    workspace_edit: Option<lsp::WorkspaceEdit>,
    command: Option<lsp::Command>,
    server_id: LanguageServerId,
) {
    if let Some(workspace_edit) = workspace_edit {
        let _ = editor.apply_workspace_edit(offset_encoding, &workspace_edit);
    }

    if let Some(command) = command {
        apply_execute_lsp_command(editor, command, server_id);
    }
}

pub(crate) fn apply_document_highlights(
    editor: &mut Editor,
    offset_encoding: helix_lsp::OffsetEncoding,
    highlights: Vec<lsp::DocumentHighlight>,
) {
    if highlights.is_empty() {
        return;
    }

    let (view_id, doc) = focused!(editor);
    let text = doc.text();
    let pos = doc.selection(view_id).primary().cursor(text.slice(..));

    let mut primary_index = 0;
    let ranges = highlights
        .iter()
        .filter_map(|highlight| lsp_range_to_range(text, highlight.range, offset_encoding))
        .enumerate()
        .map(|(i, range)| {
            if range.contains(pos) {
                primary_index = i;
            }
            range
        })
        .collect();
    let selection = helix_core::Selection::new(ranges, primary_index);
    doc.set_selection(view_id, selection);
}

pub(crate) fn apply_inlay_hints(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    offset_encoding: helix_lsp::OffsetEncoding,
    id: DocumentInlayHintsId,
    mut hints: Vec<lsp::InlayHint>,
) {
    if !editor.config().lsp.display_inlay_hints || editor.tree.try_get(view_id).is_none() {
        return;
    }

    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };

    if hints.is_empty() {
        doc.set_inlay_hints(view_id, DocumentInlayHints::empty_with_id(id));
        doc.clear_inlay_hints_outdated();
        return;
    }

    hints.sort_by_key(|inlay_hint| inlay_hint.position);

    let mut padding_before_inlay_hints = Vec::new();
    let mut type_inlay_hints = Vec::new();
    let mut parameter_inlay_hints = Vec::new();
    let mut other_inlay_hints = Vec::new();
    let mut padding_after_inlay_hints = Vec::new();

    let doc_text = doc.text();
    let inlay_hints_length_limit = doc.config.load().lsp.inlay_hints_length_limit;

    for hint in hints {
        let char_idx =
            match helix_lsp::util::lsp_pos_to_pos(doc_text, hint.position, offset_encoding) {
                Some(pos) => pos,
                None => continue,
            };

        let mut label = match hint.label {
            lsp::InlayHintLabel::String(s) => s,
            lsp::InlayHintLabel::LabelParts(parts) => parts
                .into_iter()
                .map(|p| p.value)
                .collect::<Vec<_>>()
                .join(""),
        };

        if let Some(limit) = inlay_hints_length_limit {
            use helix_core::unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr};

            let width = label.width();
            let limit = limit.get().into();
            if width > limit {
                let mut floor_boundary = 0;
                let mut acc = 0;
                for (i, grapheme_cluster) in label.grapheme_indices(true) {
                    acc += grapheme_cluster.width();

                    if acc > limit {
                        floor_boundary = i;
                        break;
                    }
                }

                label.truncate(floor_boundary);
                label.push('.');
            }
        }

        let inlay_hints_vec = match hint.kind {
            Some(lsp::InlayHintKind::TYPE) => &mut type_inlay_hints,
            Some(lsp::InlayHintKind::PARAMETER) => &mut parameter_inlay_hints,
            _ => &mut other_inlay_hints,
        };

        if let Some(true) = hint.padding_left {
            padding_before_inlay_hints.push(InlineAnnotation::new(char_idx, " "));
        }

        inlay_hints_vec.push(InlineAnnotation::new(char_idx, label));

        if let Some(true) = hint.padding_right {
            padding_after_inlay_hints.push(InlineAnnotation::new(char_idx, " "));
        }
    }

    doc.set_inlay_hints(
        view_id,
        DocumentInlayHints {
            id,
            type_inlay_hints,
            parameter_inlay_hints,
            other_inlay_hints,
            padding_before_inlay_hints,
            padding_after_inlay_hints,
        },
    );
    doc.clear_inlay_hints_outdated();
}

pub(crate) fn attach_document_colors(
    editor: &mut Editor,
    doc_id: DocumentId,
    mut doc_colors: Vec<(usize, lsp::Color)>,
) {
    let config = editor.config();

    if !config.lsp.display_color_swatches {
        return;
    }

    let color_swatch_string = &config.lsp.color_swatches_string;

    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };

    if doc_colors.is_empty() {
        doc.clear_color_swatches();
        return;
    }

    doc_colors.sort_by_key(|(pos, _)| *pos);

    let mut color_swatches = Vec::with_capacity(doc_colors.len());
    let mut color_swatches_padding = Vec::with_capacity(doc_colors.len());
    let mut colors = Vec::with_capacity(doc_colors.len());

    for (pos, color) in doc_colors {
        color_swatches_padding.push(InlineAnnotation::new(pos, " "));
        color_swatches.push(InlineAnnotation::new(pos, color_swatch_string));
        colors.push(Theme::rgb_highlight(
            (color.red * 255.) as u8,
            (color.green * 255.) as u8,
            (color.blue * 255.) as u8,
        ));
    }

    doc.set_color_swatches(DocumentColorSwatches {
        color_swatches,
        colors,
        color_swatches_padding,
    });
}

pub(crate) fn apply_pull_diagnostics_response(
    editor: &mut Editor,
    result: lsp::DocumentDiagnosticReportResult,
    provider: DiagnosticProvider,
    uri: Uri,
    document_id: DocumentId,
) {
    match result {
        lsp::DocumentDiagnosticReportResult::Report(report) => {
            let result_id = match report {
                lsp::DocumentDiagnosticReport::Full(report) => {
                    editor.handle_lsp_diagnostics(
                        &provider,
                        uri,
                        None,
                        report.full_document_diagnostic_report.items,
                    );

                    report.full_document_diagnostic_report.result_id
                }
                lsp::DocumentDiagnosticReport::Unchanged(report) => {
                    Some(report.unchanged_document_diagnostic_report.result_id)
                }
            };

            if let Some(doc) = editor.document_mut(document_id) {
                doc.set_previous_diagnostic_id(result_id);
            };
        }
        lsp::DocumentDiagnosticReportResult::Partial(_) => {}
    };
}
