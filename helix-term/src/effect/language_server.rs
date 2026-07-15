#![allow(
    clippy::items_after_test_module,
    reason = "LSP feature tests stay near the helper functions they cover in this large effect module"
)]

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

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
        apply_semantic_token_delta_edits, decode_semantic_tokens, DocumentCodeLens,
        DocumentCodeLenses, DocumentColorSwatches, DocumentInlineValues, DocumentLink,
        DocumentLinks, DocumentSemanticTokenUpdate, InlineCompletionGhost,
    },
    editor::{Action, Config},
    handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId},
    DocumentId, Editor, Theme, ViewId,
};

use crate::handlers::diagnostics::{
    PullDiagnosticsPriority, PullDiagnosticsRequestOutcome, PullDiagnosticsResponse,
    PullDiagnosticsTarget,
};
use crate::runtime::{
    ingress::{send_task_event_with, send_ui_command_with},
    ui::command::LspCodeActionItem,
    RuntimeTaskEvent, UiCommand,
};

const CODE_LENS_PLUGIN_SCOPE: &str = "helix-lsp-code-lens";
const INLINE_VALUE_LIMIT: usize = 128;
const INLINE_VALUE_EVALUATE_LIMIT: usize = 16;
const SEMANTIC_TOKENS_FULL_LINE_LIMIT: usize = 10_000;

fn folding_ranges_are_needed(config: &Config) -> bool {
    config.lsp.folding && config.fold_on_open
}

pub(crate) fn queue_document_diagnostics(
    editor: &mut Editor,
    document_ids: impl IntoIterator<Item = DocumentId>,
    language_servers: Option<&HashSet<LanguageServerId>>,
    ingress: crate::runtime::RuntimeIngress,
) {
    queue_document_diagnostics_with_priority(
        editor,
        document_ids,
        language_servers,
        PullDiagnosticsPriority::Interactive,
        ingress,
    );
}

fn queue_document_diagnostics_with_priority(
    editor: &mut Editor,
    document_ids: impl IntoIterator<Item = DocumentId>,
    language_servers: Option<&HashSet<LanguageServerId>>,
    priority: PullDiagnosticsPriority,
    ingress: crate::runtime::RuntimeIngress,
) {
    let mut targets = Vec::new();
    for document_id in document_ids {
        let Some(doc) = editor.document_mut(document_id) else {
            continue;
        };
        let version = doc.version();
        let Some(uri) = doc.uri() else {
            continue;
        };
        let mut seen = HashSet::new();
        let server_ids = doc
            .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
            .map(|server| server.id())
            .filter(|server_id| {
                seen.insert(*server_id)
                    && language_servers.is_none_or(|servers| servers.contains(server_id))
            })
            .collect::<Vec<_>>();

        for server_id in server_ids {
            targets.push(PullDiagnosticsTarget {
                server_id,
                document_id,
                generation: doc.next_pull_diagnostics_generation(server_id),
                version,
                uri: uri.clone(),
                priority,
            });
        }
    }
    ingress.schedule_pull_diagnostics(targets);
}

pub(crate) fn queue_document_diagnostics_for_language_servers(
    editor: &mut Editor,
    document_ids: impl IntoIterator<Item = DocumentId>,
    language_servers: &HashSet<LanguageServerId>,
    ingress: crate::runtime::RuntimeIngress,
) {
    queue_document_diagnostics_with_priority(
        editor,
        document_ids,
        Some(language_servers),
        PullDiagnosticsPriority::Background,
        ingress,
    );
}

pub(crate) fn start_pull_diagnostics_request(
    editor: &mut Editor,
    target: PullDiagnosticsTarget,
    cancel: helix_runtime::Token,
    ingress: crate::runtime::RuntimeIngress,
) {
    let work = editor.runtime().work().clone();
    let request = editor.document(target.document_id).and_then(|doc| {
        if doc.version() != target.version
            || !doc.is_current_pull_diagnostics(target.server_id, target.generation)
            || doc.uri().as_ref() != Some(&target.uri)
        {
            return None;
        }

        let language_server = doc
            .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
            .find(|server| server.id() == target.server_id)?;
        let future = language_server.text_document_diagnostic(
            doc.identifier(),
            doc.previous_diagnostic_id(target.server_id)
                .map(ToOwned::to_owned),
        )?;
        let identifier = language_server
            .capabilities()
            .diagnostic_provider
            .as_ref()
            .and_then(|diagnostic_provider| match diagnostic_provider {
                lsp::DiagnosticServerCapabilities::Options(options) => options.identifier.clone(),
                lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                    options.diagnostic_options.identifier.clone()
                }
            });
        let provider = DiagnosticProvider::Lsp {
            server_id: target.server_id,
            identifier,
        };
        Some((future, provider, target.uri.clone()))
    });

    let Some((future, provider, uri)) = request else {
        ingress.finish_pull_diagnostics_now(target, PullDiagnosticsRequestOutcome::Abandoned);
        return;
    };

    work.spawn(async move {
        let outcome = tokio::select! {
            biased;
            _ = cancel.canceled() => PullDiagnosticsRequestOutcome::Abandoned,
            result = future => match result {
                Ok(result) => PullDiagnosticsRequestOutcome::Response(PullDiagnosticsResponse {
                    result,
                    provider,
                    uri,
                }),
                Err(error) => PullDiagnosticsRequestOutcome::Failed(error),
            },
        };
        ingress.finish_pull_diagnostics(target, outcome).await;
    })
    .detach();
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
    let expected_version = doc.version();

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
                    expected_version,
                    request: cancel,
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
    let expected_version = doc.version();
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
                RuntimeTaskEvent::ApplyCodeLenses {
                    doc_id,
                    expected_version,
                    request: cancel,
                    lenses,
                },
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
    let expected_version = doc.version();
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
                RuntimeTaskEvent::ApplyDocumentLinks {
                    doc_id,
                    expected_version,
                    request: cancel,
                    links,
                },
                ingress,
            )
            .await;
        })
        .detach();
}

fn semantic_result_data(
    result: lsp::SemanticTokensResult,
) -> (Vec<lsp::SemanticToken>, Option<String>) {
    match result {
        lsp::SemanticTokensResult::Tokens(tokens) => (tokens.data, tokens.result_id),
        lsp::SemanticTokensResult::Partial(tokens) => (tokens.data, None),
    }
}

fn semantic_range_result_data(result: lsp::SemanticTokensRangeResult) -> Vec<lsp::SemanticToken> {
    match result {
        lsp::SemanticTokensRangeResult::Tokens(tokens) => tokens.data,
        lsp::SemanticTokensRangeResult::Partial(tokens) => tokens.data,
    }
}

fn semantic_delta_result_data(
    previous_data: &[lsp::SemanticToken],
    result: lsp::SemanticTokensFullDeltaResult,
) -> anyhow::Result<(Vec<lsp::SemanticToken>, Option<String>)> {
    match result {
        lsp::SemanticTokensFullDeltaResult::Tokens(tokens) => Ok((tokens.data, tokens.result_id)),
        lsp::SemanticTokensFullDeltaResult::TokensDelta(delta) => {
            let data = apply_semantic_token_delta_edits(previous_data, &delta.edits)
                .ok_or_else(|| anyhow::anyhow!("invalid semantic token delta edit"))?;
            Ok((data, delta.result_id))
        }
        lsp::SemanticTokensFullDeltaResult::PartialTokensDelta { edits } => {
            let data = apply_semantic_token_delta_edits(previous_data, &edits)
                .ok_or_else(|| anyhow::anyhow!("invalid semantic token delta edit"))?;
            Ok((data, None))
        }
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
                        .map(|result| (result.map(semantic_range_result_data).unwrap_or_default(), None))
                })
            } else {
                let full_request =
                    language_server.text_document_semantic_tokens_full(doc.identifier(), None)?;
                let previous = doc
                    .semantic_token_delta_state(server_id)
                    .and_then(|state| {
                        helix_lsp::Client::semantic_tokens_delta_previous_result_id(
                            language_server.supports_semantic_tokens_delta(),
                            state.result_id.as_deref(),
                        )
                        .map(|result_id| (result_id, state.data.clone()))
                    });
                let delta_request = previous.as_ref().and_then(|(result_id, _)| {
                    language_server.text_document_semantic_tokens_full_delta(
                        doc.identifier(),
                        result_id.clone(),
                        None,
                    )
                });
                futures_util::future::Either::Right(async move {
                    if let (Some(delta_request), Some((_, previous_data))) =
                        (delta_request, previous)
                    {
                        match delta_request.await {
                            Ok(Some(result)) => match semantic_delta_result_data(&previous_data, result) {
                                Ok(data) => return Ok(data),
                                Err(err) => log::debug!(
                                    "semantic token delta failed, falling back to full request: {err}"
                                ),
                            },
                            Ok(None) => log::debug!(
                                "semantic token delta returned no data, falling back to full request"
                            ),
                            Err(err) => log::debug!(
                                "semantic token delta request failed, falling back to full request: {err}"
                            ),
                        }
                    }

                    full_request
                        .await
                        .map(|result| result.map(semantic_result_data).unwrap_or_default())
                })
            };
            let text = text.clone();
            Some(async move {
                let (data, result_id) = request.await?;
                anyhow::Ok((
                    server_id,
                    version,
                    legend,
                    offset_encoding,
                    text,
                    data,
                    result_id,
                ))
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
                    Ok((server_id, version, legend, offset_encoding, text, data, result_id)) => {
                        let tokens = decode_semantic_tokens(&text, &legend, offset_encoding, &data);
                        send_task_event_with(
                            RuntimeTaskEvent::ApplySemanticTokens {
                                doc_id,
                                server_id,
                                request: cancel.clone(),
                                tokens: DocumentSemanticTokenUpdate {
                                    version,
                                    result_id,
                                    data,
                                    tokens,
                                },
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
    if !folding_ranges_are_needed(&editor.config()) {
        return;
    }
    let block = editor.runtime().block().clone();
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let cancel = doc.restart_folding_ranges();
    let expected_version = doc.version();
    let text = doc.text().clone();
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
            let range_count = ranges.len();
            let build_start = Instant::now();
            let build = block.spawn(move || {
                let points = ranges
                    .iter()
                    .filter_map(|(_, offset_encoding, range)| {
                        folding_range_to_fold(&text, *offset_encoding, range)
                    })
                    .collect::<Vec<_>>();
                FoldContainer::from(text.slice(..), points)
            });
            let folds = tokio::select! {
                _ = cancel.canceled() => return,
                result = build => match result {
                    Ok(folds) => folds,
                    Err(err) => {
                        log::error!("folding range preparation failed: {err}");
                        return;
                    }
                }
            };
            let build_elapsed = build_start.elapsed();
            if build_elapsed >= Duration::from_millis(8) {
                log::info!(
                    "lsp_folding phase=prepare_slow ranges={} folds={} elapsed_us={}",
                    range_count,
                    folds.len(),
                    build_elapsed.as_micros(),
                );
            }
            send_task_event_with(
                RuntimeTaskEvent::ApplyFoldingRanges {
                    doc_id,
                    expected_version,
                    request: cancel,
                    folds,
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
    trigger_kind: lsp::SignatureHelpTriggerKind,
    is_retrigger: bool,
    cancel: Token,
    ingress: crate::runtime::RuntimeIngress,
) {
    let (view_id, doc) = focused!(editor);

    let future = doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .find_map(|language_server| {
            let pos = doc.position(view_id, language_server.offset_encoding());
            let context = lsp::SignatureHelpContext {
                trigger_kind: trigger_kind.clone(),
                trigger_character: signature_help_trigger_character(
                    doc,
                    view_id,
                    language_server,
                    &trigger_kind,
                ),
                is_retrigger,
                active_signature_help: None,
            };
            language_server.text_document_signature_help(doc.identifier(), pos, None, Some(context))
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

fn signature_help_trigger_character(
    doc: &helix_view::Document,
    view_id: ViewId,
    language_server: &helix_lsp::Client,
    trigger_kind: &lsp::SignatureHelpTriggerKind,
) -> Option<String> {
    if trigger_kind != &lsp::SignatureHelpTriggerKind::TRIGGER_CHARACTER {
        return None;
    }

    let lsp::ServerCapabilities {
        signature_help_provider: Some(options),
        ..
    } = language_server.capabilities()
    else {
        return None;
    };

    let text = doc.text().slice(..);
    let cursor = doc.selection(view_id).primary().cursor(text);
    let text_before_cursor = text.slice(..cursor);
    options
        .trigger_characters
        .iter()
        .chain(options.retrigger_characters.iter())
        .flatten()
        .find(|trigger| text_before_cursor.ends_with(trigger))
        .cloned()
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

pub(crate) fn apply_resolved_code_lens(
    editor: &mut Editor,
    doc_id: DocumentId,
    expected_version: i32,
    server_id: LanguageServerId,
    original: lsp::CodeLens,
    resolved: lsp::CodeLens,
    foreground: &crate::runtime::ForegroundEvents,
) {
    let current = editor.document_mut(doc_id).is_some_and(|doc| {
        if doc.version() != expected_version {
            return false;
        }
        let Some(code_lenses) = doc.code_lenses_mut() else {
            return false;
        };
        let Some(stored) = code_lenses
            .lenses
            .iter_mut()
            .find(|stored| stored.server_id == server_id && stored.lens == original)
        else {
            return false;
        };
        stored.lens = resolved.clone();
        stored.resolved = true;
        true
    });
    if !current {
        editor.set_status("Code lens changed before it resolved");
        return;
    }

    let Some(command) = resolved.command else {
        editor.set_error("Code lens did not resolve to a command");
        return;
    };
    if let Err(error) = foreground.task(RuntimeTaskEvent::ExecuteLspCommand { command, server_id })
    {
        editor.set_error(error.to_string());
    }
}

pub(crate) fn apply_resolved_document_link(
    editor: &mut Editor,
    doc_id: DocumentId,
    expected_version: i32,
    target: lsp::Url,
    action: Action,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
) {
    if editor
        .document(doc_id)
        .is_none_or(|doc| doc.version() != expected_version)
    {
        editor.set_status("Document link changed before it resolved");
        return;
    }

    if target.scheme() == "file" {
        let Ok(uri) = Uri::try_from(target) else {
            editor.set_error("Document link target is not a valid file URI");
            return;
        };
        let Some(path) = uri.as_path() else {
            editor.set_error("Document link target is not a local path");
            return;
        };
        let target = editor.focused_view_id();
        crate::runtime::ui::document::queue_document_open(
            editor,
            &ingress,
            foreground,
            crate::runtime::DocumentOpenRequest {
                path: path.to_path_buf(),
                action,
                lane: crate::runtime::DocumentOpenLane::Navigation,
                target: crate::runtime::DocumentOpenTarget::View(target),
                selection: crate::runtime::DocumentOpenSelection::None,
                alignment: crate::runtime::DocumentOpenAlignment::None,
                default_folding_if_new: false,
                fff_record: None,
                external_if_binary: None,
                post_action: crate::runtime::DocumentOpenPostAction::None,
                completion: crate::runtime::DocumentOpenCompletionTarget::Editor,
            },
        );
    } else {
        crate::runtime::ingress::spawn_task_event_with_future(
            editor.work(),
            crate::open_external_url_task_event(target),
            ingress,
        );
    }
}

pub(crate) fn apply_rename_edit(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    doc_id: DocumentId,
    expected_version: i32,
    offset_encoding: helix_lsp::OffsetEncoding,
    workspace_edit: Option<lsp::WorkspaceEdit>,
) {
    if editor
        .document(doc_id)
        .is_none_or(|doc| doc.version() != expected_version)
    {
        editor.set_status("Document changed before rename completed");
        return;
    }
    if let Err(error) = crate::effect::file_operation::apply_workspace_edit(
        editor,
        ingress,
        offset_encoding,
        &workspace_edit.unwrap_or_default(),
        None,
    ) {
        editor.set_error(format!("Failed to apply rename edits: {}", error.kind));
    }
}

pub(crate) fn request_apply_code_action(
    editor: &mut Editor,
    item: LspCodeActionItem,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
) {
    let Some(language_server) = editor.language_server_by_id(item.language_server_id) else {
        editor.set_error("Language Server disappeared");
        return;
    };
    let offset_encoding = language_server.offset_encoding();

    match item.lsp_item {
        lsp::CodeActionOrCommand::Command(command) => {
            if let Err(error) = foreground.task(RuntimeTaskEvent::ExecuteLspCommand {
                command,
                server_id: item.language_server_id,
            }) {
                editor.set_error(error.to_string());
            }
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

            if let Err(error) = foreground.task(RuntimeTaskEvent::ApplyCodeAction {
                offset_encoding,
                workspace_edit: code_action.edit,
                command: code_action.command,
                server_id,
            }) {
                editor.set_error(error.to_string());
            }
        }
    }
}

pub(crate) fn apply_code_action(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    offset_encoding: helix_lsp::OffsetEncoding,
    workspace_edit: Option<lsp::WorkspaceEdit>,
    command: Option<lsp::Command>,
    server_id: LanguageServerId,
) {
    if let Some(workspace_edit) = workspace_edit {
        let continuation =
            command.map(
                |command| helix_view::editor::WorkspaceEditContinuation::ExecuteCommand {
                    server_id,
                    command,
                },
            );
        let _ = crate::effect::file_operation::apply_workspace_edit(
            editor,
            ingress,
            offset_encoding,
            &workspace_edit,
            continuation,
        );
    } else if let Some(command) = command {
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
    let lsp_hints: Vec<DocumentInlayHint> = hints
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
            type_inlay_hints: type_inlay_hints.into(),
            parameter_inlay_hints: parameter_inlay_hints.into(),
            other_inlay_hints: other_inlay_hints.into(),
            padding_before_inlay_hints: padding_before_inlay_hints.into(),
            padding_after_inlay_hints: padding_after_inlay_hints.into(),
            lsp_hints: lsp_hints.into(),
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
    expected_version: i32,
    request: &helix_runtime::Token,
    lenses: Vec<(LanguageServerId, helix_lsp::OffsetEncoding, lsp::CodeLens)>,
) {
    if !editor.config().lsp.code_lens {
        return;
    }
    if !editor
        .document(doc_id)
        .is_some_and(|doc| doc.version() == expected_version && doc.is_current_code_lenses(request))
    {
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
    request: &helix_runtime::Token,
    tokens: DocumentSemanticTokenUpdate,
) {
    if !editor.config().lsp.semantic_tokens {
        return;
    }
    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == tokens.version && doc.is_current_semantic_tokens(request)
    }) {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    doc.set_semantic_token_update(server_id, tokens);
}

pub(crate) fn apply_inline_completion(
    editor: &mut Editor,
    doc_id: DocumentId,
    request: &helix_runtime::Token,
    completion: InlineCompletionGhost,
) {
    if !editor.config().lsp.inline_completion {
        return;
    }
    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == completion.version && doc.is_current_inline_completion(request)
    }) {
        return;
    }
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    doc.set_inline_completion(completion);
}

pub(crate) fn apply_inline_values(
    editor: &mut Editor,
    doc_id: DocumentId,
    expected_version: i32,
    request: &helix_runtime::Token,
    values: DocumentInlineValues,
) {
    if !editor.config().lsp.inline_values {
        return;
    }
    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == expected_version && doc.is_current_inline_values(request)
    }) {
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
                            request: cancel,
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
    let Some((frame, dap, cached_frame_variables)) = editor
        .debug_adapters
        .get_active_client()
        .and_then(|debugger| {
            let frame = debugger.current_stack_frame()?.clone();
            Some((
                frame.clone(),
                debugger.request_handle(),
                debugger.frame_variables(frame.id).cloned(),
            ))
        })
    else {
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
    let request =
        language_server.text_document_inline_values(doc.identifier(), range, context, None);
    let Some(request) = request else {
        return;
    };
    let frame_id = frame.id;
    let cancel = doc.restart_inline_values();
    let expected_version = doc.version();

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
                    let annotations = inline_value_annotations(
                        &text,
                        offset_encoding,
                        values,
                        cached_frame_variables,
                        dap,
                        frame_id,
                        cancel.clone(),
                    )
                    .await;
                    send_task_event_with(
                        RuntimeTaskEvent::ApplyInlineValues {
                            doc_id,
                            expected_version,
                            request: cancel,
                            values: DocumentInlineValues {
                                annotations: annotations.into(),
                            },
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

fn pending_inline_value_annotations(
    text: &helix_core::Rope,
    offset_encoding: helix_lsp::OffsetEncoding,
    values: Vec<lsp::InlineValue>,
) -> Vec<PendingInlineValueAnnotation> {
    values
        .into_iter()
        .take(INLINE_VALUE_LIMIT)
        .filter_map(|value| {
            let (range, label) = match value {
                lsp::InlineValue::Text(value) => {
                    (value.range, PendingInlineValueLabel::Text(value.text))
                }
                lsp::InlineValue::VariableLookup(value) => {
                    let name = value.variable_name.unwrap_or_else(|| {
                        lsp_range_to_range(text, value.range, offset_encoding)
                            .map(|range| text.slice(range.from()..range.to()).to_string())
                            .unwrap_or_default()
                    });
                    (
                        value.range,
                        PendingInlineValueLabel::Variable {
                            name,
                            case_sensitive: value.case_sensitive_lookup,
                        },
                    )
                }
                lsp::InlineValue::EvaluatableExpression(value) => {
                    let expression = value.expression.unwrap_or_else(|| {
                        lsp_range_to_range(text, value.range, offset_encoding)
                            .map(|range| text.slice(range.from()..range.to()).to_string())
                            .unwrap_or_default()
                    });
                    (value.range, PendingInlineValueLabel::Expression(expression))
                }
            };
            let range = lsp_range_to_range(text, range, offset_encoding)?;
            let line = text.char_to_line(range.to().min(text.len_chars()));
            let char_idx = helix_core::line_ending::line_end_char_index(&text.slice(..), line);
            Some(PendingInlineValueAnnotation { char_idx, label })
        })
        .collect()
}

#[derive(Debug)]
struct PendingInlineValueAnnotation {
    char_idx: usize,
    label: PendingInlineValueLabel,
}

#[derive(Debug)]
enum PendingInlineValueLabel {
    Text(String),
    Variable { name: String, case_sensitive: bool },
    Expression(String),
}

async fn fetch_inline_value_variables(
    dap: &helix_dap::RequestHandle,
    frame_id: usize,
    cancel: &Token,
) -> Option<helix_dap::FrameVariables> {
    let scopes = tokio::select! {
        _ = cancel.canceled() => return None,
        scopes = dap.scopes(frame_id) => scopes.ok()?,
    };
    let mut variables = Vec::new();
    for scope in &scopes {
        let scope_variables = tokio::select! {
            _ = cancel.canceled() => return None,
            variables = dap.variables(scope.variables_reference) => variables.ok()?,
        };
        variables.extend(scope_variables);
    }
    Some(helix_dap::FrameVariables { scopes, variables })
}

async fn inline_value_annotations(
    text: &helix_core::Rope,
    offset_encoding: helix_lsp::OffsetEncoding,
    values: Vec<lsp::InlineValue>,
    cached_frame_variables: Option<helix_dap::FrameVariables>,
    dap: helix_dap::RequestHandle,
    frame_id: usize,
    cancel: Token,
) -> Vec<InlineAnnotation> {
    let pending = pending_inline_value_annotations(text, offset_encoding, values);
    let needs_variables = pending
        .iter()
        .any(|value| matches!(value.label, PendingInlineValueLabel::Variable { .. }));
    let frame_variables = if needs_variables {
        match cached_frame_variables {
            Some(frame_variables) => Some(frame_variables),
            None => fetch_inline_value_variables(&dap, frame_id, &cancel).await,
        }
    } else {
        None
    };

    let mut annotations = Vec::new();
    let mut evaluations = FuturesUnordered::new();
    let mut in_flight_evaluations = 0;

    for value in pending {
        match value.label {
            PendingInlineValueLabel::Text(text) => {
                if !text.is_empty() {
                    annotations.push(InlineAnnotation::new(value.char_idx, text));
                }
            }
            PendingInlineValueLabel::Variable {
                name,
                case_sensitive,
            } => {
                let Some(label) = frame_variables
                    .as_ref()
                    .and_then(|variables| variables.variable_value(&name, case_sensitive))
                else {
                    continue;
                };
                annotations.push(InlineAnnotation::new(value.char_idx, format!(" = {label}")));
            }
            PendingInlineValueLabel::Expression(expression) => {
                if expression.is_empty()
                    || !helix_dap::should_evaluate_inline_value(
                        in_flight_evaluations,
                        INLINE_VALUE_EVALUATE_LIMIT,
                    )
                {
                    continue;
                }
                in_flight_evaluations += 1;
                let dap = dap.clone();
                let cancel = cancel.clone();
                evaluations.push(async move {
                    let response = tokio::select! {
                        _ = cancel.canceled() => return None,
                        response = dap.evaluate(
                            expression,
                            Some(frame_id),
                            Some("watch".to_string()),
                        ) => response.ok()?,
                    };
                    (!response.result.is_empty()).then(|| {
                        InlineAnnotation::new(value.char_idx, format!(" = {}", response.result))
                    })
                });
            }
        }
    }

    while let Some(annotation) = tokio::select! {
        _ = cancel.canceled() => return Vec::new(),
        annotation = evaluations.next() => annotation,
    } {
        if let Some(annotation) = annotation {
            annotations.push(annotation);
        }
    }

    annotations
}

pub(crate) fn apply_document_links(
    editor: &mut Editor,
    doc_id: DocumentId,
    expected_version: i32,
    request: &helix_runtime::Token,
    links: Vec<(
        LanguageServerId,
        helix_lsp::OffsetEncoding,
        lsp::DocumentLink,
    )>,
) {
    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == expected_version && doc.is_current_document_links(request)
    }) {
        return;
    }
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
    expected_version: i32,
    request: &helix_runtime::Token,
    folds: FoldContainer,
) {
    let config = editor.config();
    if !folding_ranges_are_needed(&config) {
        return;
    }
    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == expected_version && doc.is_current_folding_ranges(request)
    }) {
        return;
    }
    let fold_on_open = config.fold_on_open;
    let view_ids: Vec<_> = editor
        .tree
        .views()
        .filter_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        .collect();
    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };
    let target_view_ids = view_ids
        .into_iter()
        .filter(|view_id| {
            should_replace_folds_with_lsp(
                doc.fold_container(*view_id),
                doc.is_lsp_fold_container(*view_id),
                fold_on_open,
            )
        })
        .collect::<Vec<_>>();
    let target_count = target_view_ids.len();
    let mut folds = Some(folds);
    for (index, view_id) in target_view_ids.into_iter().enumerate() {
        let view_folds = if index + 1 == target_count {
            folds.take().expect("fold container available")
        } else {
            folds.as_ref().expect("fold container available").clone()
        };
        doc.insert_fold_container(view_id, view_folds);
        doc.mark_lsp_fold_container(view_id);
    }
}

pub(crate) fn should_replace_folds_with_lsp(
    existing: Option<&FoldContainer>,
    lsp_owned: bool,
    fold_on_open: bool,
) -> bool {
    fold_on_open && (lsp_owned || existing.is_none_or(FoldContainer::is_empty))
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
    use helix_view::{
        editor::{Action, Config, EditorBuilder},
        graphics::Rect,
        Document,
    };

    fn editor_with_text(text: &str, config: Config) -> (Editor, DocumentId, ViewId) {
        let mut editor =
            EditorBuilder::new(Rect::new(0, 0, 80, 24), helix_runtime::test::runtime())
                .config(config)
                .build();
        let doc = Document::from(
            Rope::from(text),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let doc_id = editor.new_file_from_document(Action::VerticalSplit, doc);
        let view_id = editor.tree.focus;
        (editor, doc_id, view_id)
    }

    fn prepared_folds(text: &str, range: &lsp::FoldingRange) -> FoldContainer {
        let text = Rope::from(text);
        let points = folding_range_to_fold(&text, helix_lsp::OffsetEncoding::Utf8, range)
            .expect("valid folding range");
        FoldContainer::from(text.slice(..), vec![points])
    }

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

        assert!(should_replace_folds_with_lsp(None, false, true));
        assert!(should_replace_folds_with_lsp(
            Some(&FoldContainer::new()),
            false,
            true
        ));
        assert!(should_replace_folds_with_lsp(Some(&non_empty), true, true));
        assert!(!should_replace_folds_with_lsp(
            Some(&non_empty),
            false,
            true
        ));
        assert!(!should_replace_folds_with_lsp(None, false, false));
        assert!(!should_replace_folds_with_lsp(
            Some(&FoldContainer::new()),
            false,
            false
        ));
        assert!(!should_replace_folds_with_lsp(
            Some(&non_empty),
            true,
            false
        ));
    }

    #[test]
    fn apply_folding_ranges_only_collapses_when_fold_on_open_is_enabled() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let text = "fn main() {\n    call();\n}\n";
        let range = lsp::FoldingRange {
            start_line: 0,
            end_line: 2,
            ..Default::default()
        };

        assert!(!folding_ranges_are_needed(&Config::default()));

        let (mut editor, doc_id, view_id) = editor_with_text(text, Config::default());
        let version = editor.document(doc_id).expect("document").version();
        let request = editor
            .document_mut(doc_id)
            .expect("document")
            .restart_folding_ranges();
        apply_folding_ranges(
            &mut editor,
            doc_id,
            version,
            &request,
            prepared_folds(text, &range),
        );
        let doc = editor.document(doc_id).expect("document");
        assert!(doc
            .fold_container(view_id)
            .is_none_or(FoldContainer::is_empty));

        let config = Config {
            fold_on_open: true,
            ..Default::default()
        };
        assert!(folding_ranges_are_needed(&config));
        let (mut editor, doc_id, view_id) = editor_with_text(text, config);
        let version = editor.document(doc_id).expect("document").version();
        let stale_request = editor
            .document_mut(doc_id)
            .expect("document")
            .restart_folding_ranges();
        let request = editor
            .document_mut(doc_id)
            .expect("document")
            .restart_folding_ranges();
        apply_folding_ranges(
            &mut editor,
            doc_id,
            version,
            &stale_request,
            prepared_folds(text, &range),
        );
        assert!(editor
            .document(doc_id)
            .expect("document")
            .fold_container(view_id)
            .is_none_or(FoldContainer::is_empty));
        apply_folding_ranges(
            &mut editor,
            doc_id,
            version,
            &request,
            prepared_folds(text, &range),
        );
        let doc = editor.document(doc_id).expect("document");
        assert_eq!(doc.fold_container(view_id).map(FoldContainer::len), Some(1));
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
    fn pending_inline_value_annotations_render_at_line_end() {
        let text = Rope::from("let x = 1;\n");
        let annotations = pending_inline_value_annotations(
            &text,
            helix_lsp::OffsetEncoding::Utf8,
            vec![lsp::InlineValue::VariableLookup(
                lsp::InlineValueVariableLookup {
                    range: lsp::Range::new(lsp::Position::new(0, 4), lsp::Position::new(0, 5)),
                    variable_name: None,
                    case_sensitive_lookup: true,
                },
            )],
        );

        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].char_idx, 10);
        let PendingInlineValueLabel::Variable { name, .. } = &annotations[0].label else {
            panic!("expected variable lookup");
        };
        assert_eq!(name, "x");
    }

    #[test]
    fn pull_diagnostics_response_requires_current_identity_and_attachment() {
        let server_id = LanguageServerId::default();
        let document_id = DocumentId::default();
        let uri = Uri::from(std::path::PathBuf::from("pull-diagnostics.rs"));
        let target = PullDiagnosticsTarget {
            server_id,
            document_id,
            generation: 2,
            version: 7,
            uri: uri.clone(),
            priority: crate::handlers::diagnostics::PullDiagnosticsPriority::Interactive,
        };
        let renamed = Uri::from(std::path::PathBuf::from("renamed.rs"));

        assert!(pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &uri,
            7,
            true,
            Some(&uri),
            true,
        ));
        assert!(!pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &uri,
            7,
            false,
            Some(&uri),
            true,
        ));
        assert!(!pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &uri,
            8,
            true,
            Some(&uri),
            true,
        ));
        assert!(!pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &uri,
            7,
            true,
            Some(&renamed),
            true,
        ));
        assert!(!pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &renamed,
            7,
            true,
            Some(&uri),
            true,
        ));
        assert!(!pull_diagnostics_response_is_current(
            &target,
            Some(server_id),
            &uri,
            7,
            true,
            Some(&uri),
            false,
        ));
    }
}

pub(crate) fn attach_document_colors(
    editor: &mut Editor,
    doc_id: DocumentId,
    expected_version: i32,
    request: &helix_runtime::Token,
    mut doc_colors: Vec<(usize, lsp::Color)>,
) {
    let config = editor.config();

    if !config.lsp.display_color_swatches {
        return;
    }

    let color_swatch_string = &config.lsp.color_swatches_string;

    if !editor.document(doc_id).is_some_and(|doc| {
        doc.version() == expected_version && doc.is_current_color_swatches(request)
    }) {
        return;
    }

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
        color_swatches: color_swatches.into(),
        colors: colors.into(),
        color_swatches_padding: color_swatches_padding.into(),
    });
}

pub(crate) fn apply_pull_diagnostics_response(
    editor: &mut Editor,
    result: lsp::DocumentDiagnosticReportResult,
    provider: DiagnosticProvider,
    uri: Uri,
    target: PullDiagnosticsTarget,
) {
    let is_current = editor.document(target.document_id).is_some_and(|doc| {
        let server_attached = doc
            .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
            .any(|server| server.id() == target.server_id);
        pull_diagnostics_response_is_current(
            &target,
            provider.language_server_id(),
            &uri,
            doc.version(),
            doc.is_current_pull_diagnostics(target.server_id, target.generation),
            doc.uri().as_ref(),
            server_attached,
        )
    });
    if !is_current {
        log::debug!(
            "dropping stale pull diagnostics response for server {} document {:?} generation {} version {}",
            target.server_id,
            target.document_id,
            target.generation,
            target.version,
        );
        return;
    }

    let related = match result {
        lsp::DocumentDiagnosticReportResult::Report(report) => match report {
            lsp::DocumentDiagnosticReport::Full(report) => {
                let result_id = apply_document_diagnostic_kind(
                    editor,
                    &provider,
                    uri.clone(),
                    lsp::DocumentDiagnosticReportKind::Full(report.full_document_diagnostic_report),
                );
                if let Some(doc) = editor.document_mut(target.document_id) {
                    doc.set_previous_diagnostic_id(target.server_id, result_id);
                }
                report.related_documents
            }
            lsp::DocumentDiagnosticReport::Unchanged(report) => {
                let result_id = apply_document_diagnostic_kind(
                    editor,
                    &provider,
                    uri,
                    lsp::DocumentDiagnosticReportKind::Unchanged(
                        report.unchanged_document_diagnostic_report,
                    ),
                );
                if let Some(doc) = editor.document_mut(target.document_id) {
                    doc.set_previous_diagnostic_id(target.server_id, result_id);
                }
                report.related_documents
            }
        },
        lsp::DocumentDiagnosticReportResult::Partial(report) => report.related_documents,
    };

    for (url, report) in related.into_iter().flatten() {
        let Ok(uri) = Uri::try_from(&url) else {
            log::debug!("ignoring pull diagnostics for unsupported related URI {url}");
            continue;
        };
        let result_id = apply_document_diagnostic_kind(editor, &provider, uri.clone(), report);
        let document_id = editor
            .documents()
            .find(|document| document.uri().as_ref() == Some(&uri))
            .map(|document| document.id());
        if let Some(document_id) = document_id {
            if let Some(document) = editor.document_mut(document_id) {
                document.set_previous_diagnostic_id(target.server_id, result_id);
            }
        }
    }
}

fn pull_diagnostics_response_is_current(
    target: &PullDiagnosticsTarget,
    provider_server: Option<LanguageServerId>,
    response_uri: &Uri,
    document_version: i32,
    generation_is_current: bool,
    document_uri: Option<&Uri>,
    server_attached: bool,
) -> bool {
    provider_server == Some(target.server_id)
        && document_version == target.version
        && generation_is_current
        && document_uri == Some(&target.uri)
        && response_uri == &target.uri
        && server_attached
}

fn apply_document_diagnostic_kind(
    editor: &mut Editor,
    provider: &DiagnosticProvider,
    uri: Uri,
    report: lsp::DocumentDiagnosticReportKind,
) -> Option<String> {
    match report {
        lsp::DocumentDiagnosticReportKind::Full(report) => {
            editor.handle_lsp_diagnostics(provider, uri, None, report.items);
            report.result_id
        }
        lsp::DocumentDiagnosticReportKind::Unchanged(report) => Some(report.result_id),
    }
}
