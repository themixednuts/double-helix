use std::path::Path;

use futures_util::future;

use crate::{bench::log_run_event, graphics::CursorKind, Document, View};
use helix_core::Position;

use helix_core::{diagnostic::DiagnosticProvider, syntax::config::LanguageServerFeature, Range};

use super::{types::Diagnostics, Editor, Mode};

/// Lock-free cursor position cache. Packs `Option<Option<Position>>` into
/// a single `AtomicU64` so the cache is `Sync` without any locking.
///
/// Encoding: row (upper 32 bits) | col (lower 32 bits).
/// Sentinel `u64::MAX` = not yet computed, `u64::MAX - 1` = offscreen.
pub struct CursorCache(std::sync::atomic::AtomicU64);

const CURSOR_UNSET: u64 = u64::MAX;
const CURSOR_OFFSCREEN: u64 = u64::MAX - 1;

impl Default for CursorCache {
    fn default() -> Self {
        Self(std::sync::atomic::AtomicU64::new(CURSOR_UNSET))
    }
}

impl CursorCache {
    pub fn get(&self, view: &View, doc: &Document) -> Option<Position> {
        let value = self.0.load(std::sync::atomic::Ordering::Relaxed);
        if value != CURSOR_UNSET {
            return Self::decode(value);
        }

        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let pos = view.screen_coords_at_pos(doc, text, cursor);
        self.set(pos);
        pos
    }

    pub fn set(&self, cursor_pos: Option<Position>) {
        self.0.store(
            Self::encode(cursor_pos),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn reset(&self) {
        self.0
            .store(CURSOR_UNSET, std::sync::atomic::Ordering::Relaxed);
    }

    fn encode(pos: Option<Position>) -> u64 {
        match pos {
            None => CURSOR_OFFSCREEN,
            Some(pos) => ((pos.row as u64) << 32) | (pos.col as u64 & 0xFFFF_FFFF),
        }
    }

    fn decode(value: u64) -> Option<Position> {
        if value == CURSOR_OFFSCREEN {
            return None;
        }
        Some(Position {
            row: (value >> 32) as usize,
            col: (value & 0xFFFF_FFFF) as usize,
        })
    }
}

impl Editor {
    pub fn has_stale_syntax(&self) -> bool {
        self.documents
            .values()
            .any(|doc| doc.syntax_snapshot().is_stale())
            || self
                .component_docs
                .values()
                .any(|doc| doc.syntax_snapshot().is_stale())
    }

    pub fn refresh_one_stale_syntax(&mut self) -> bool {
        let focused_doc_id = self.tree.get(self.tree.focus).doc;
        let loader = self.syn_loader.load();

        if let Some(doc) = self.documents.get_mut(&focused_doc_id) {
            if doc.syntax_snapshot().is_stale() {
                let refreshed = doc.refresh_stale_syntax(&loader);
                log_run_event("syntax_refresh_attempt", || {
                    format!(
                        "target=focused doc_id={} refreshed={}",
                        focused_doc_id, refreshed
                    )
                });
                if refreshed {
                    self.needs_redraw = true;
                }
                return refreshed;
            }
        }

        for (doc_id, doc) in &mut self.documents {
            if *doc_id == focused_doc_id || !doc.syntax_snapshot().is_stale() {
                continue;
            }
            let refreshed = doc.refresh_stale_syntax(&loader);
            log_run_event("syntax_refresh_attempt", || {
                format!(
                    "target=background doc_id={} refreshed={}",
                    doc_id, refreshed
                )
            });
            if refreshed {
                self.needs_redraw = true;
            }
            return refreshed;
        }

        for doc in self.component_docs.values_mut() {
            if !doc.syntax_snapshot().is_stale() {
                continue;
            }
            let refreshed = doc.refresh_stale_syntax(&loader);
            log_run_event("syntax_refresh_attempt", || {
                format!("target=component refreshed={}", refreshed)
            });
            if refreshed {
                self.needs_redraw = true;
            }
            return refreshed;
        }

        false
    }

    /// Returns all supported diagnostics for the document
    pub fn doc_diagnostics<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        Editor::doc_diagnostics_with_filter(language_servers, diagnostics, document, |_, _| true)
    }

    /// Returns all supported diagnostics for the document
    /// filtered by `filter` which is invocated with the raw `lsp::Diagnostic` and the language server id it came from
    pub fn doc_diagnostics_with_filter<'a>(
        language_servers: &'a helix_lsp::Registry,
        diagnostics: &'a Diagnostics,
        document: &Document,
        filter: impl Fn(&helix_lsp::lsp::Diagnostic, &DiagnosticProvider) -> bool + 'a,
    ) -> impl Iterator<Item = helix_core::Diagnostic> + 'a {
        let text = document.text().clone();
        let language_config = document.language_configuration().cloned();
        document
            .uri()
            .and_then(|uri| diagnostics.get(&uri))
            .map(|diags| {
                diags.iter().filter_map(move |(diagnostic, provider)| {
                    let server_id = provider.language_server_id()?;
                    let ls = language_servers.get_by_id(server_id)?;
                    language_config
                        .as_ref()
                        .and_then(|c| {
                            c.language_servers.iter().find(|features| {
                                features.name == ls.name()
                                    && features.has_feature(LanguageServerFeature::Diagnostics)
                            })
                        })
                        .and_then(|_| {
                            if filter(diagnostic, provider) {
                                Document::lsp_diagnostic_to_diagnostic(
                                    &text,
                                    language_config.as_deref(),
                                    diagnostic,
                                    provider.clone(),
                                    ls.offset_encoding(),
                                )
                            } else {
                                None
                            }
                        })
                })
            })
            .into_iter()
            .flatten()
    }

    /// Gets the primary cursor position in screen coordinates,
    /// or `None` if the primary cursor is not visible on screen.
    pub fn cursor(&self) -> (Option<Position>, CursorKind) {
        let config = self.config();
        let (view_id, doc) = focused_ref!(self);
        let view = self.tree.get(view_id);
        if let Some(mut pos) = self.cursor_cache.get(view, doc) {
            let inner = view.inner_area(doc);
            pos.col += inner.x as usize;
            pos.row += inner.y as usize;
            let cursorkind = config.cursor_shape.from_mode(self.mode);
            (Some(pos), cursorkind)
        } else {
            (None, CursorKind::default())
        }
    }

    /// Closes language servers with a short grace period, then kills any server
    /// process that did not shut down in time.
    pub async fn close_language_servers(&self, timeout: Option<u64>) {
        for client in self.language_servers.iter_clients() {
            self.language_servers
                .file_event_handler
                .remove_client(client.id());
        }

        let grace = tokio::time::Duration::from_millis(timeout.unwrap_or(3000));
        future::join_all(
            self.language_servers
                .iter_clients()
                .map(|client| async move {
                    match tokio::time::timeout(grace, async {
                        let _ = client.force_shutdown().await;
                        client.wait().await
                    })
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) => {
                            log::warn!(
                                "failed to wait for language server '{}' during shutdown: {}",
                                client.name(),
                                err
                            );
                        }
                        Err(_) => {
                            log::warn!(
                                "language server '{}' did not shut down within {:?}; killing process",
                                client.name(),
                                grace
                            );
                            if let Err(err) = client.force_kill().await {
                                log::warn!(
                                    "failed to kill language server '{}' after shutdown timeout: {}",
                                    client.name(),
                                    err
                                );
                            }
                        }
                    }
                }),
        )
        .await;
    }

    /// Switches the editor into normal mode.
    pub fn enter_normal_mode(&mut self) {
        use helix_core::graphemes;

        if self.mode == Mode::Normal {
            return;
        }

        self.mode = Mode::Normal;
        let (view_id, doc) = focused!(self);
        let view = self.tree.get_mut(view_id);

        try_restore_indent(doc, view);

        if doc.restore_cursor() {
            let text = doc.text().slice(..);
            let selection = doc.selection(view_id).clone().transform(|range| {
                let mut head = range.to();
                if range.head > range.anchor {
                    head = graphemes::prev_grapheme_boundary(text, head);
                }

                Range::new(range.from(), head)
            });

            doc.set_selection(view_id, selection);
            doc.clear_restore_cursor();
        }
    }

    pub fn current_stack_frame(&self) -> Option<&helix_dap::StackFrame> {
        self.debug_adapters.current_stack_frame()
    }

    pub fn set_cwd(&mut self, path: &Path) -> std::io::Result<()> {
        self.last_cwd = helix_stdx::env::set_current_working_dir(path)?;
        self.clear_doc_relative_paths();
        Ok(())
    }

    pub fn get_last_cwd(&mut self) -> Option<&Path> {
        self.last_cwd.as_deref()
    }
}

fn try_restore_indent(doc: &mut Document, view: &mut View) {
    use helix_core::{
        chars::char_is_whitespace,
        line_ending::{line_end_char_index, str_is_line_ending},
        unicode::segmentation::UnicodeSegmentation,
        Operation, Transaction,
    };

    fn inserted_a_new_blank_line(changes: &[Operation], pos: usize, line_end_pos: usize) -> bool {
        if let [Operation::Retain(move_pos), Operation::Insert(ref inserted_str), Operation::Retain(_)] =
            changes
        {
            let mut graphemes = inserted_str.graphemes(true);
            move_pos + inserted_str.len() == pos
                && graphemes.next().is_some_and(str_is_line_ending)
                && graphemes.all(|g| g.chars().all(char_is_whitespace))
                && pos == line_end_pos
        } else {
            false
        }
    }

    let doc_changes = doc.changes().changes();
    let text = doc.text().slice(..);
    let range = doc.selection(view.id).primary();
    let pos = range.cursor(text);
    let line_end_pos = line_end_char_index(&text, range.cursor_line(text));

    if inserted_a_new_blank_line(doc_changes, pos, line_end_pos) {
        let line_start_pos = text.line_to_char(range.cursor_line(text));
        let transaction =
            Transaction::change(doc.text(), [(line_start_pos, pos, None)].into_iter());
        doc.apply(&transaction, view.id);
    }
}
