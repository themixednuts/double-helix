use std::{collections::HashSet, time::Duration};

use futures_util::{
    stream::{FuturesOrdered, FuturesUnordered},
    StreamExt,
};
use helix_core::{
    diagnostic::DiagnosticProvider,
    syntax::config::LanguageServerFeature,
    text_annotations::InlineAnnotation,
    text_folding::{Fold, FoldContainer, FoldObject},
    Selection, SmallVec, Transaction, Uri,
};
use helix_lsp::{self, lsp, util::lsp_range_to_range, LanguageServerId};
use helix_runtime::Token;
use helix_stdx::rope::RopeSliceExt;
use helix_view::{
    document::{DocumentInlayHint, DocumentInlayHints, DocumentInlayHintsId, PluginAnnotation},
    document_lsp::{
        decode_semantic_tokens, DocumentCodeLens, DocumentCodeLenses, DocumentColorSwatches,
        DocumentInlineValues, DocumentLink, DocumentLinks, DocumentSemanticTokens,
        InlineCompletionGhost,
    },
    handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId},
    DocumentId, Editor, Theme, ViewId,
};

use crate::runtime::{
    ingress::{send_task_event_with, send_ui_command_with},
    ui::command::LspCodeActionItem,
    RuntimeTaskEvent, UiCommand,
};

const CODE_LENS_PLUGIN_SCOPE: &str = "helix-lsp-code-lens";
const INLINE_VALUE_LIMIT: usize = 128;
const SEMANTIC_TOKENS_FULL_LINE_LIMIT: usize = 10_000;

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

pub(crate) fn request_code_lenses(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.code_lens {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let cancel = doc.restart_code_lenses();
    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::CodeLens)
        .filter(|language_server| seen_language_servers.insert(language_server.id()))
        .map(|language_server| {
            let server_id = language_server.id();
            let offset_encoding = language_server.offset_encoding();
            let request = language_server
                .text_document_code_lens(doc.identifier(), None)
                .unwrap();
            async move {
                anyhow::Ok((
                    server_id,
                    offset_encoding,
                    request.await?.unwrap_or_default(),
                ))
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
            let mut lenses = Vec::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.canceled() => return,
                    next = futures.next() => next,
                };
                match next {
                    Some(Ok((server_id, offset_encoding, items))) => {
                        lenses.extend(
                            items
                                .into_iter()
                                .map(|lens| (server_id, offset_encoding, lens)),
                        );
                    }
                    Some(Err(err)) => log::error!("code lens request failed: {err}"),
                    None => break,
                }
            }
            send_task_event_with(
                RuntimeTaskEvent::ApplyCodeLenses { doc_id, lenses },
                ingress,
            )
            .await;
        })
        .detach();
}

pub(crate) fn request_document_links(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.document_links {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let cancel = doc.restart_document_links();
    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::DocumentLinks)
        .filter(|language_server| seen_language_servers.insert(language_server.id()))
        .map(|language_server| {
            let server_id = language_server.id();
            let offset_encoding = language_server.offset_encoding();
            let request = language_server
                .text_document_document_link(doc.identifier(), None)
                .unwrap();
            async move {
                anyhow::Ok((
                    server_id,
                    offset_encoding,
                    request.await?.unwrap_or_default(),
                ))
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
            let mut links = Vec::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.canceled() => return,
                    next = futures.next() => next,
                };
                match next {
                    Some(Ok((server_id, offset_encoding, items))) => {
                        links.extend(
                            items
                                .into_iter()
                                .map(|link| (server_id, offset_encoding, link)),
                        );
                    }
                    Some(Err(err)) => log::error!("document link request failed: {err}"),
                    None => break,
                }
            }
            send_task_event_with(
                RuntimeTaskEvent::ApplyDocumentLinks { doc_id, links },
                ingress,
            )
            .await;
        })
        .detach();
}

fn semantic_result_data(result: lsp::SemanticTokensResult) -> Vec<lsp::SemanticToken> {
    match result {
        lsp::SemanticTokensResult::Tokens(tokens) => tokens.data,
        lsp::SemanticTokensResult::Partial(tokens) => tokens.data,
    }
}

fn semantic_range_result_data(result: lsp::SemanticTokensRangeResult) -> Vec<lsp::SemanticToken> {
    match result {
        lsp::SemanticTokensRangeResult::Tokens(tokens) => tokens.data,
        lsp::SemanticTokensRangeResult::Partial(tokens) => tokens.data,
    }
}

pub(crate) fn request_semantic_tokens(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.semantic_tokens {
        return;
    }
    let visible_line_window = editor.document(doc_id).and_then(|doc| {
        editor
            .tree
            .views()
            .filter(|(view, _)| view.doc == doc_id)
            .max_by_key(|(_, focused)| *focused)
            .map(|(view, _)| {
                let offset = doc.view_offset(view.id);
                let height = view.inner_area(doc).height as usize;
                (doc.text().char_to_line(offset.anchor), height)
            })
    });
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let cancel = doc.restart_semantic_tokens();
    let version = doc.version();
    let text = doc.text().clone();
    let use_range = text.len_lines() > SEMANTIC_TOKENS_FULL_LINE_LIMIT;
    let range =
        semantic_tokens_request_range(&text, use_range.then_some(visible_line_window).flatten());

    let mut futures: FuturesOrdered<_> = doc
        .language_servers()
        .filter(|language_server| {
            language_server.supports_semantic_tokens_full()
                || language_server.supports_semantic_tokens_range()
        })
        .filter_map(|language_server| {
            let server_id = language_server.id();
            let offset_encoding = language_server.offset_encoding();
            let legend = language_server.semantic_tokens_legend()?.clone();
            let request = if use_range && language_server.supports_semantic_tokens_range() {
                let lsp_range = helix_lsp::util::range_to_lsp_range(&text, range, offset_encoding);
                let request = language_server.text_document_semantic_tokens_range(
                    doc.identifier(),
                    lsp_range,
                    None,
                )?;
                futures_util::future::Either::Left(async move {
                    request
                        .await
                        .map(|result| result.map(semantic_range_result_data).unwrap_or_default())
                })
            } else {
                let request =
                    language_server.text_document_semantic_tokens_full(doc.identifier(), None)?;
                futures_util::future::Either::Right(async move {
                    request
                        .await
                        .map(|result| result.map(semantic_result_data).unwrap_or_default())
                })
            };
            let text = text.clone();
            Some(async move {
                let data = request.await?;
                anyhow::Ok((server_id, version, legend, offset_encoding, text, data))
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
            while let Some(next) = tokio::select! {
                _ = cancel.canceled() => return,
                next = futures.next() => next,
            } {
                match next {
                    Ok((server_id, version, legend, offset_encoding, text, data)) => {
                        let tokens = decode_semantic_tokens(&text, &legend, offset_encoding, &data);
                        send_task_event_with(
                            RuntimeTaskEvent::ApplySemanticTokens {
                                doc_id,
                                server_id,
                                tokens: DocumentSemanticTokens { version, tokens },
                            },
                            ingress.clone(),
                        )
                        .await;
                    }
                    Err(err) => log::error!("semantic tokens request failed: {err}"),
                }
            }
        })
        .detach();
}

fn semantic_tokens_request_range(
    text: &helix_core::Rope,
    visible_line_window: Option<(usize, usize)>,
) -> helix_core::Range {
    let Some((first_line, height)) = visible_line_window else {
        return helix_core::Range::new(0, text.len_chars());
    };
    let first_line = first_line.min(text.len_lines().saturating_sub(1));
    let end_line = first_line.saturating_add(height).saturating_add(1);
    let end = if end_line >= text.len_lines() {
        text.len_chars()
    } else {
        text.line_to_char(end_line)
    };
    helix_core::Range::new(text.line_to_char(first_line), end)
}

pub(crate) fn request_folding_ranges(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.folding {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let cancel = doc.restart_folding_ranges();
    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::FoldingRange)
        .filter(|language_server| seen_language_servers.insert(language_server.id()))
        .map(|language_server| {
            let server_id = language_server.id();
            let offset_encoding = language_server.offset_encoding();
            let request = language_server
                .text_document_folding_range(doc.identifier(), None)
                .unwrap();
            async move {
                anyhow::Ok((
                    server_id,
                    offset_encoding,
                    request.await?.unwrap_or_default(),
                ))
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
            let mut ranges = Vec::new();
            loop {
                let next = tokio::select! {
                    _ = cancel.canceled() => return,
                    next = futures.next() => next,
                };
                match next {
                    Some(Ok((server_id, offset_encoding, items))) => {
                        ranges.extend(
                            items
                                .into_iter()
                                .map(|range| (server_id, offset_encoding, range)),
                        );
                    }
                    Some(Err(err)) => log::error!("folding range request failed: {err}"),
                    None => break,
                }
            }
            send_task_event_with(
                RuntimeTaskEvent::ApplyFoldingRanges { doc_id, ranges },
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
    server_id: LanguageServerId,
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
    let lsp_hints = hints
        .iter()
        .cloned()
        .map(|hint| DocumentInlayHint {
            server_id,
            offset_encoding,
            hint,
        })
        .collect();

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
            lsp_hints,
        },
    );
    doc.clear_inlay_hints_outdated();
}

fn code_lens_annotations(doc: &helix_view::Document) -> Vec<PluginAnnotation> {
    let Some(code_lenses) = doc.code_lenses() else {
        return Vec::new();
    };

    let text = doc.text();
    let mut annotations = Vec::new();
    let mut current_line = None;
    let mut current_text = String::new();

    for lens in &code_lenses.lenses {
        let Some(title) = lens.title() else {
            continue;
        };
        let line = text.char_to_line(lens.range.from().min(text.len_chars()));
        if current_line == Some(line) {
            current_text.push_str("  |  ");
            current_text.push_str(title);
        } else {
            if let Some(line) = current_line {
                annotations.push(PluginAnnotation {
                    char_idx: text.line_to_char(line),
                    text: current_text,
                    style: Some("ui.virtual".to_owned()),
                    fg: None,
                    bg: None,
                    offset: 0,
                    is_line: true,
                    virt_line_idx: Some(0),
                    dropped_text: None,
                });
            }
            current_line = Some(line);
            current_text = title.to_owned();
        }
    }

    if let Some(line) = current_line {
        annotations.push(PluginAnnotation {
            char_idx: text.line_to_char(line),
            text: current_text,
            style: Some("ui.virtual".to_owned()),
            fg: None,
            bg: None,
            offset: 0,
            is_line: true,
            virt_line_idx: Some(0),
            dropped_text: None,
        });
    }

    annotations
}

fn refresh_code_lens_annotations(editor: &mut Editor, doc_id: DocumentId) {
    let view_ids: Vec<_> = editor
        .tree
        .views()
        .filter_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        .collect();
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let annotations = code_lens_annotations(doc);
    for view_id in view_ids {
        doc.set_plugin_annotations(
            view_id,
            CODE_LENS_PLUGIN_SCOPE.to_owned(),
            annotations.clone(),
        );
    }
}

pub(crate) fn apply_code_lenses(
    editor: &mut Editor,
    doc_id: DocumentId,
    lenses: Vec<(LanguageServerId, helix_lsp::OffsetEncoding, lsp::CodeLens)>,
) {
    if !editor.config().lsp.code_lens {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let text = doc.text();
    let lenses = lenses
        .into_iter()
        .filter_map(|(server_id, offset_encoding, lens)| {
            let range = lsp_range_to_range(text, lens.range, offset_encoding)?;
            Some(DocumentCodeLens {
                server_id,
                range,
                offset_encoding,
                resolved: lens.command.is_some(),
                lens,
            })
        })
        .collect();
    doc.set_code_lenses(DocumentCodeLenses::sorted(lenses));
    refresh_code_lens_annotations(editor, doc_id);
}

pub(crate) fn apply_semantic_tokens(
    editor: &mut Editor,
    doc_id: DocumentId,
    server_id: LanguageServerId,
    tokens: DocumentSemanticTokens,
) {
    if !editor.config().lsp.semantic_tokens {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    if doc.version() != tokens.version {
        return;
    }
    doc.set_semantic_tokens(server_id, tokens);
}

pub(crate) fn apply_inline_completion(
    editor: &mut Editor,
    doc_id: DocumentId,
    completion: InlineCompletionGhost,
) {
    if !editor.config().lsp.inline_completion {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    if doc.version() == completion.version {
        doc.set_inline_completion(completion);
    }
}

pub(crate) fn apply_inline_values(
    editor: &mut Editor,
    doc_id: DocumentId,
    values: DocumentInlineValues,
) {
    if !editor.config().lsp.inline_values {
        return;
    }
    if let Some(doc) = editor.document_mut(doc_id) {
        doc.set_inline_values(values);
    }
}

pub(crate) fn request_inline_completion(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    invoked: bool,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.inline_completion || editor.mode() != helix_view::document::Mode::Insert
    {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let Some(language_server) = doc
        .language_servers()
        .find(|language_server| language_server.supports_inline_completion())
    else {
        return;
    };
    let cursor = doc
        .selection(view_id)
        .primary()
        .cursor(doc.text().slice(..));
    let position =
        helix_lsp::util::pos_to_lsp_pos(doc.text(), cursor, language_server.offset_encoding());
    let request = language_server.text_document_inline_completion(
        lsp::TextDocumentPositionParams {
            text_document: doc.identifier(),
            position,
        },
        lsp::InlineCompletionContext {
            trigger_kind: if invoked {
                lsp::InlineCompletionTriggerKind::Invoked
            } else {
                lsp::InlineCompletionTriggerKind::Automatic
            },
            selected_completion_info: None,
        },
        None,
    );
    let Some(request) = request else {
        return;
    };
    let version = doc.version();
    let text = doc.text().clone();
    let offset_encoding = language_server.offset_encoding();
    let cancel = doc.restart_inline_completion();

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            let response = tokio::select! {
                _ = cancel.canceled() => return,
                response = request => response,
            };
            match response {
                Ok(Some(response)) => {
                    let item = match response {
                        lsp::InlineCompletionResponse::Array(items) => items.into_iter().next(),
                        lsp::InlineCompletionResponse::List(list) => list.items.into_iter().next(),
                    };
                    let Some(item) = item else {
                        return;
                    };
                    let first_line = item
                        .insert_text
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    if first_line.is_empty() {
                        return;
                    }
                    let replace_range = item
                        .range
                        .and_then(|range| lsp_range_to_range(&text, range, offset_encoding));
                    send_task_event_with(
                        RuntimeTaskEvent::ApplyInlineCompletion {
                            doc_id,
                            completion: InlineCompletionGhost {
                                view_id,
                                version,
                                cursor,
                                text: item.insert_text,
                                annotation: InlineAnnotation::new(cursor, first_line),
                                replace_range,
                            },
                        },
                        ingress,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(err) => log::error!("inline completion request failed: {err}"),
            }
        })
        .detach();
}

pub(crate) fn accept_inline_completion(editor: &mut Editor) -> bool {
    let (view_id, doc) = focused!(editor);
    let Some(completion) = doc.inline_completion().cloned() else {
        return false;
    };
    if completion.view_id != view_id || completion.version != doc.version() {
        doc.clear_inline_completion();
        return false;
    }
    let range = completion
        .replace_range
        .unwrap_or_else(|| helix_core::Range::new(completion.cursor, completion.cursor));
    let transaction = Transaction::change(
        doc.text(),
        std::iter::once((range.from(), range.to(), Some(completion.text.into()))),
    );
    doc.apply(&transaction, view_id);
    doc.clear_inline_completion();
    true
}

pub(crate) fn request_inline_values(
    editor: &mut Editor,
    doc_id: DocumentId,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.inline_values {
        return;
    }
    let Some(frame) = editor.debug_adapters.current_stack_frame().cloned() else {
        return;
    };
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let Some(language_server) = doc
        .language_servers()
        .find(|language_server| language_server.supports_inline_values())
    else {
        return;
    };
    let offset_encoding = language_server.offset_encoding();
    let text = doc.text().clone();
    let context = inline_value_context(&text, &frame, offset_encoding);
    let range = helix_lsp::util::range_to_lsp_range(
        &text,
        helix_core::Range::new(0, text.len_chars()),
        offset_encoding,
    );
    let request = language_server.text_document_inline_values(
        doc.identifier(),
        range,
        context,
        None,
    );
    let Some(request) = request else {
        return;
    };
    let cancel = doc.restart_inline_values();

    editor
        .runtime()
        .work()
        .clone()
        .spawn(async move {
            let values = tokio::select! {
                _ = cancel.canceled() => return,
                values = request => values,
            };
            match values {
                Ok(Some(values)) => {
                    let annotations = inline_value_annotations(&text, offset_encoding, values);
                    send_task_event_with(
                        RuntimeTaskEvent::ApplyInlineValues {
                            doc_id,
                            values: DocumentInlineValues { annotations },
                        },
                        ingress,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(err) => log::error!("inline value request failed: {err}"),
            }
        })
        .detach();
}

fn inline_value_context(
    text: &helix_core::Rope,
    frame: &helix_dap::StackFrame,
    offset_encoding: helix_lsp::OffsetEncoding,
) -> lsp::InlineValueContext {
    let start = helix_view::handlers::dap::dap_pos_to_pos(text, frame.line, frame.column)
        .unwrap_or_else(|| text.line_to_char(frame.line.saturating_sub(1).min(text.len_lines())));
    let end = frame
        .end_line
        .and_then(|line| {
            helix_view::handlers::dap::dap_pos_to_pos(
                text,
                line,
                frame.end_column.unwrap_or(frame.column),
            )
        })
        .unwrap_or(start);
    let stopped_location = helix_lsp::util::range_to_lsp_range(
        text,
        helix_core::Range::new(start, end),
        offset_encoding,
    );

    lsp::InlineValueContext {
        frame_id: frame.id as i32,
        stopped_location,
    }
}

fn inline_value_annotations(
    text: &helix_core::Rope,
    offset_encoding: helix_lsp::OffsetEncoding,
    values: Vec<lsp::InlineValue>,
) -> Vec<InlineAnnotation> {
    values
        .into_iter()
        .take(INLINE_VALUE_LIMIT)
        .filter_map(|value| {
            let (range, label) = match value {
                lsp::InlineValue::Text(value) => (value.range, value.text),
                lsp::InlineValue::VariableLookup(value) => {
                    let label = value.variable_name.unwrap_or_else(|| {
                        lsp_range_to_range(text, value.range, offset_encoding)
                            .map(|range| text.slice(range.from()..range.to()).to_string())
                            .unwrap_or_default()
                    });
                    (value.range, format!(" = {label}"))
                }
                lsp::InlineValue::EvaluatableExpression(value) => {
                    let label = value.expression.unwrap_or_else(|| {
                        lsp_range_to_range(text, value.range, offset_encoding)
                            .map(|range| text.slice(range.from()..range.to()).to_string())
                            .unwrap_or_default()
                    });
                    (value.range, format!(" = {label}"))
                }
            };
            let range = lsp_range_to_range(text, range, offset_encoding)?;
            let line = text.char_to_line(range.to().min(text.len_chars()));
            let line_end = helix_core::line_ending::line_end_char_index(&text.slice(..), line);
            (!label.is_empty()).then(|| InlineAnnotation::new(line_end, label))
        })
        .collect()
}

pub(crate) fn apply_document_links(
    editor: &mut Editor,
    doc_id: DocumentId,
    links: Vec<(
        LanguageServerId,
        helix_lsp::OffsetEncoding,
        lsp::DocumentLink,
    )>,
) {
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let text = doc.text();
    let links = links
        .into_iter()
        .filter_map(|(server_id, offset_encoding, link)| {
            let range = lsp_range_to_range(text, link.range, offset_encoding)?;
            Some(DocumentLink {
                server_id,
                range,
                offset_encoding,
                resolved: link.target.is_some(),
                link,
            })
        })
        .collect();
    doc.set_document_links(DocumentLinks::sorted(links));
}

pub(crate) fn folding_range_to_fold(
    text: &helix_core::Rope,
    offset_encoding: helix_lsp::OffsetEncoding,
    range: &lsp::FoldingRange,
) -> Option<(
    helix_core::text_folding::StartFoldPoint,
    helix_core::text_folding::EndFoldPoint,
)> {
    let start_line = range.start_line as usize;
    let end_line = range.end_line as usize;
    if start_line >= end_line || end_line >= text.len_lines() {
        return None;
    }

    let text_slice = text.slice(..);
    let line_start = text.line_to_char(start_line);
    let line_end = helix_core::line_ending::line_end_char_index(&text_slice, start_line);
    let start = range
        .start_character
        .and_then(|character| {
            let pos = lsp::Position::new(range.start_line, character);
            helix_lsp::util::lsp_pos_to_pos(text, pos, offset_encoding)
        })
        .filter(|start| *start >= line_start && *start <= line_end)
        .unwrap_or_else(|| {
            line_start
                + text_slice
                    .line(start_line)
                    .first_non_whitespace_char()
                    .unwrap_or(0)
        });

    let end = range
        .end_character
        .and_then(|character| {
            let pos = lsp::Position::new(range.end_line, character);
            helix_lsp::util::lsp_pos_to_pos(text, pos, offset_encoding)
        })
        .unwrap_or_else(|| helix_core::line_ending::line_end_char_index(&text_slice, end_line))
        .saturating_sub(1);

    let object = match range.kind {
        Some(lsp::FoldingRangeKind::Comment) => FoldObject::TextObject("comment"),
        Some(lsp::FoldingRangeKind::Imports) => FoldObject::TextObject("imports"),
        Some(lsp::FoldingRangeKind::Region) => FoldObject::TextObject("region"),
        None => FoldObject::TextObject("lsp"),
    };
    Some(Fold::new_points(text_slice, object, start, &(start..=end)))
}

pub(crate) fn apply_folding_ranges(
    editor: &mut Editor,
    doc_id: DocumentId,
    ranges: Vec<(
        LanguageServerId,
        helix_lsp::OffsetEncoding,
        lsp::FoldingRange,
    )>,
) {
    if !editor.config().lsp.folding {
        return;
    }
    let view_ids: Vec<_> = editor
        .tree
        .views()
        .filter_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        .collect();
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let text = doc.text().clone();
    let points = ranges
        .iter()
        .filter_map(|(_, offset_encoding, range)| {
            folding_range_to_fold(&text, *offset_encoding, range)
        })
        .collect::<Vec<_>>();
    for view_id in view_ids {
        if should_replace_folds_with_lsp(
            doc.fold_container(view_id),
            doc.is_lsp_fold_container(view_id),
        ) {
            doc.insert_fold_container(view_id, FoldContainer::from(text.slice(..), points.clone()));
            doc.mark_lsp_fold_container(view_id);
        }
    }
}

pub(crate) fn should_replace_folds_with_lsp(
    existing: Option<&FoldContainer>,
    lsp_owned: bool,
) -> bool {
    lsp_owned || existing.is_none_or(FoldContainer::is_empty)
}

pub(crate) fn apply_linked_editing_ranges(
    editor: &mut Editor,
    offset_encoding: helix_lsp::OffsetEncoding,
    linked: lsp::LinkedEditingRanges,
) {
    let (view_id, doc) = focused!(editor);
    let text = doc.text();
    let pos = doc.selection(view_id).primary().cursor(text.slice(..));
    let mut primary_index = 0;
    let ranges: SmallVec<[_; 1]> = linked
        .ranges
        .into_iter()
        .filter_map(|range| lsp_range_to_range(text, range, offset_encoding))
        .enumerate()
        .map(|(idx, range)| {
            if range.contains(pos) {
                primary_index = idx;
            }
            range
        })
        .collect();
    if !ranges.is_empty() {
        doc.set_selection(view_id, Selection::new(ranges, primary_index));
    }
}

pub(crate) fn apply_on_type_formatting(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    expected_version: i32,
    offset_encoding: helix_lsp::OffsetEncoding,
    edits: Vec<lsp::TextEdit>,
) {
    if !editor.contains_document(doc_id) || !editor.contains_view(view_id) {
        return;
    }
    let scrolloff = editor.config().scrolloff;
    let doc = doc_mut!(editor, &doc_id);
    if doc.version() != expected_version {
        return;
    }
    let transaction =
        helix_lsp::util::generate_transaction_from_edits(doc.text(), edits, offset_encoding);
    doc.apply(&transaction, view_id);
    let view = view_mut!(editor, view_id);
    doc.append_changes_to_history(view);
    view.ensure_cursor_in_view(doc, scrolloff);
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::Rope;

    #[test]
    fn folding_range_conversion_rejects_empty_and_out_of_bounds_ranges() {
        let text = Rope::from("fn main() {\n    call();\n}\n");

        assert!(folding_range_to_fold(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            &lsp::FoldingRange {
                start_line: 1,
                end_line: 1,
                ..Default::default()
            },
        )
        .is_none());

        assert!(folding_range_to_fold(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            &lsp::FoldingRange {
                start_line: 0,
                end_line: 99,
                ..Default::default()
            },
        )
        .is_none());
    }

    #[test]
    fn folding_range_conversion_creates_fold_for_multiline_range() {
        let text = Rope::from("fn main() {\n    call();\n}\n");
        let points = folding_range_to_fold(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            &lsp::FoldingRange {
                start_line: 0,
                end_line: 2,
                kind: Some(lsp::FoldingRangeKind::Region),
                ..Default::default()
            },
        )
        .expect("valid folding range");

        let container = FoldContainer::from(text.slice(..), vec![points]);
        assert_eq!(container.len(), 1);
    }

    #[test]
    fn lsp_folds_only_replace_empty_existing_folds() {
        let text = Rope::from("fn main() {\n    call();\n}\n");
        let points = folding_range_to_fold(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            &lsp::FoldingRange {
                start_line: 0,
                end_line: 2,
                ..Default::default()
            },
        )
        .expect("valid folding range");
        let non_empty = FoldContainer::from(text.slice(..), vec![points]);

        assert!(should_replace_folds_with_lsp(None, false));
        assert!(should_replace_folds_with_lsp(
            Some(&FoldContainer::new()),
            false
        ));
        assert!(should_replace_folds_with_lsp(Some(&non_empty), true));
        assert!(!should_replace_folds_with_lsp(Some(&non_empty), false));
    }

    #[test]
    fn semantic_tokens_range_uses_visible_line_window() {
        let text = Rope::from("zero\none\ntwo\nthree\n");
        let range = semantic_tokens_request_range(&text, Some((1, 1)));

        assert_eq!(
            range,
            helix_core::Range::new(text.line_to_char(1), text.line_to_char(3))
        );
        assert_eq!(
            semantic_tokens_request_range(&text, None),
            helix_core::Range::new(0, text.len_chars())
        );
    }

    #[test]
    fn inline_value_context_uses_stopped_stack_frame_range() {
        let text = Rope::from("alpha\nbeta\n");
        let frame = helix_dap::StackFrame {
            id: 42,
            name: "frame".to_string(),
            source: None,
            line: 2,
            column: 2,
            end_line: Some(2),
            end_column: Some(4),
            can_restart: None,
            instruction_pointer_reference: None,
            module_id: None,
            presentation_hint: None,
        };

        let context = inline_value_context(&text, &frame, helix_lsp::OffsetEncoding::Utf8);

        assert_eq!(context.frame_id, 42);
        assert_eq!(
            context.stopped_location,
            lsp::Range::new(lsp::Position::new(1, 1), lsp::Position::new(1, 3))
        );
    }

    #[test]
    fn inline_value_annotations_render_at_line_end() {
        let text = Rope::from("let x = 1;\n");
        let annotations = inline_value_annotations(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            vec![lsp::InlineValue::VariableLookup(
                lsp::InlineValueVariableLookup {
                    range: lsp::Range::new(
                        lsp::Position::new(0, 4),
                        lsp::Position::new(0, 5),
                    ),
                    variable_name: None,
                    case_sensitive_lookup: true,
                },
            )],
        );

        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].char_idx, 10);
        assert_eq!(&annotations[0].text[..], " = x");
    }
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
