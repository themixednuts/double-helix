use std::{
    collections::HashSet,
    time::Duration,
};

use futures_util::{stream::FuturesOrdered, StreamExt};
use helix_core::{syntax::config::LanguageServerFeature, text_annotations::InlineAnnotation};
use helix_event::{cancelable_future, register_hook};
use helix_lsp::lsp;
use helix_view::bench::log_command_phase;
use helix_view::{
    document_lsp::DocumentColorSwatches,
    events::{DocumentDidChange, DocumentDidOpen, LanguageServerExited, LanguageServerInitialized},
    handlers::{lsp::DocumentColorsEvent, Handlers},
    DocumentId, Editor, Theme,
};
use tokio::time::Instant;

use crate::job;

#[derive(Default)]
pub(super) struct DocumentColorsHandler {
    docs: HashSet<DocumentId>,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

impl helix_event::AsyncHook for DocumentColorsHandler {
    type Event = DocumentColorsEvent;

    fn handle_event(&mut self, event: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        let DocumentColorsEvent(doc_id) = event;
        self.docs.insert(doc_id);
        Some(Instant::now() + DOCUMENT_CHANGE_DEBOUNCE)
    }

    fn finish_debounce(&mut self) {
        let docs = std::mem::take(&mut self.docs);

        job::dispatch_blocking(move |editor, _compositor| {
            for doc in docs {
                request_document_colors(editor, doc);
            }
        });
    }
}

fn request_document_colors(editor: &mut Editor, doc_id: DocumentId) {
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
        .filter(|ls| seen_language_servers.insert(ls.id()))
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

    tokio::spawn(async move {
        let mut all_colors = Vec::new();
        loop {
            match cancelable_future(futures.next(), &cancel).await {
                Some(Some(Ok(items))) => all_colors.extend(items),
                Some(Some(Err(err))) => log::error!("document color request failed: {err}"),
                Some(None) => break,
                // The request was cancelled.
                None => return,
            }
        }
        job::dispatch(move |editor, _| attach_document_colors(editor, doc_id, all_colors)).await;
    });
}

fn attach_document_colors(
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

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        // when a document is initially opened, request colors for it
        request_document_colors(event.editor, event.doc);

        Ok(())
    });

    let tx = handlers.document_colors.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let hook_start = std::time::Instant::now();
        // Update the color swatch positions so they stay aligned with edits.
        event.doc.update_color_swatches(event.changes);

        // Avoid re-requesting document colors if the change is a ghost transaction (completion)
        // because the language server will not know about the updates to the document and will
        // give out-of-date locations.
        if !event.ghost_transaction {
            // Cancel the ongoing request, if present.
            event.doc.cancel_color_swatches();
            helix_event::send_blocking(&tx, DocumentColorsEvent(event.doc.id()));
        }

        let hook_dur = hook_start.elapsed();
        log_command_phase("document_did_change_hook", "document_colors", hook_dur, || {
            format!(
                "doc_id={:?} ghost={} lines={} bytes={} has_swatches={}",
                event.doc.id(),
                event.ghost_transaction,
                event.doc.text().len_lines(),
                event.doc.text().len_bytes(),
                event.doc.color_swatches().is_some()
            )
        });
        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id);
        }

        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        // Clear and re-request all color swatches when a server exits.
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.clear_color_swatches();
            }
        }

        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id);
        }

        Ok(())
    });
}
