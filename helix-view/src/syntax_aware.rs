use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::{
    self,
    config::{LanguageConfiguration, LanguageServerFeature},
};
use helix_core::{Assoc, ChangeSet, Diagnostic, RopeSlice, Syntax};
use helix_lsp::{Client, LanguageServerId, LanguageServerName};
use std::collections::HashMap;
use std::sync::Arc;

use crate::bench::log_run_event;
use crate::revision::Revision;

#[derive(Debug, Default)]
pub struct SyntaxAwareState {
    syntax_snapshot: SyntaxSnapshotState,
    language: Option<Arc<LanguageConfiguration>>,
    diagnostics: Vec<Diagnostic>,
    diagnostics_gen: u64,
    language_servers: HashMap<LanguageServerName, Arc<Client>>,
}

const DEFERRED_SYNTAX_LAYER_THRESHOLD: usize = 32;
const DEFERRED_SYNTAX_ROOT_INJECTION_THRESHOLD: usize = 16;
const DEFERRED_SYNTAX_GIANT_LINE_BYTE_THRESHOLD: usize = 256 * 1024;
const DEFERRED_SYNTAX_AVG_BYTES_PER_LINE_THRESHOLD: usize = 8 * 1024;

#[derive(Debug, Clone, Copy)]
struct DocumentShape {
    line_count: usize,
    byte_count: usize,
}

impl DocumentShape {
    fn from_text(text: RopeSlice<'_>) -> Self {
        Self {
            line_count: text.len_lines(),
            byte_count: text.len_bytes(),
        }
    }

    fn average_bytes_per_line(self) -> usize {
        self.byte_count / self.line_count.max(1)
    }

    fn has_giant_lines(self) -> bool {
        self.byte_count >= DEFERRED_SYNTAX_GIANT_LINE_BYTE_THRESHOLD
            && self.average_bytes_per_line() >= DEFERRED_SYNTAX_AVG_BYTES_PER_LINE_THRESHOLD
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntaxBudget {
    Idle,
    Interactive(InteractiveSyntaxReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyntaxStatus {
    Fresh,
    StalePendingRefresh,
    #[default]
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntaxSnapshot {
    revision: Revision,
    status: SyntaxStatus,
}

impl SyntaxSnapshot {
    pub const fn new(revision: Revision, status: SyntaxStatus) -> Self {
        Self { revision, status }
    }

    pub const fn revision(self) -> Revision {
        self.revision
    }

    pub const fn status(self) -> SyntaxStatus {
        self.status
    }

    pub const fn is_stale(self) -> bool {
        matches!(self.status, SyntaxStatus::StalePendingRefresh)
    }
}

#[derive(Debug, Default)]
struct SyntaxSnapshotState {
    revision: Revision,
    status: SyntaxStatus,
    tree: Option<Syntax>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveSyntaxReason {
    Stale,
    GiantLines,
    LayerFanout,
    RootInjections,
}

impl InteractiveSyntaxReason {
    fn label(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::GiantLines => "giant_lines",
            Self::LayerFanout => "layer_fanout",
            Self::RootInjections => "root_injections",
        }
    }
}

impl SyntaxSnapshotState {
    fn snapshot(&self) -> SyntaxSnapshot {
        SyntaxSnapshot::new(self.revision, self.status)
    }

    fn syntax(&self) -> Option<&Syntax> {
        self.tree.as_ref()
    }

    fn syntax_mut(&mut self) -> Option<&mut Syntax> {
        self.tree.as_mut()
    }

    fn status(&self) -> SyntaxStatus {
        self.status
    }

    fn set_tree(&mut self, tree: Option<Syntax>) {
        self.tree = tree;
        self.status = if self.tree.is_some() {
            SyntaxStatus::Fresh
        } else {
            SyntaxStatus::Disabled
        };
        self.revision.advance();
    }

    fn mark_updated(&mut self) {
        if self.tree.is_some() {
            self.status = SyntaxStatus::Fresh;
            self.revision.advance();
        } else {
            self.mark_disabled();
        }
    }

    fn mark_stale(&mut self) {
        if self.tree.is_some() && self.status != SyntaxStatus::StalePendingRefresh {
            self.status = SyntaxStatus::StalePendingRefresh;
            self.revision.advance();
        }
    }

    fn mark_disabled(&mut self) {
        if self.tree.is_some() || self.status != SyntaxStatus::Disabled {
            self.tree = None;
            self.status = SyntaxStatus::Disabled;
            self.revision.advance();
        }
    }
}

fn syntax_budget(
    syntax_status: SyntaxStatus,
    shape: DocumentShape,
    complexity: syntax::SyntaxComplexity,
) -> SyntaxBudget {
    if matches!(syntax_status, SyntaxStatus::StalePendingRefresh) {
        return SyntaxBudget::Interactive(InteractiveSyntaxReason::Stale);
    }

    if shape.has_giant_lines() {
        return SyntaxBudget::Interactive(InteractiveSyntaxReason::GiantLines);
    }

    if complexity.total_layers >= DEFERRED_SYNTAX_LAYER_THRESHOLD {
        return SyntaxBudget::Interactive(InteractiveSyntaxReason::LayerFanout);
    }

    if complexity.root_injections >= DEFERRED_SYNTAX_ROOT_INJECTION_THRESHOLD {
        return SyntaxBudget::Interactive(InteractiveSyntaxReason::RootInjections);
    }

    SyntaxBudget::Idle
}

impl SyntaxAwareState {
    pub fn set_language(
        &mut self,
        language_config: Option<Arc<LanguageConfiguration>>,
        text: RopeSlice<'_>,
        loader: &syntax::Loader,
        display_name: &str,
    ) {
        self.language = language_config;
        let syntax = self.language.as_ref().and_then(|config| {
            Syntax::new(text, config.language(), loader)
                .map_err(|err| {
                    if err != syntax::HighlighterError::NoRootConfig {
                        log::warn!("Error building syntax for '{}': {err}", display_name);
                    }
                })
                .ok()
        });
        self.syntax_snapshot.set_tree(syntax);
    }

    pub fn set_language_configuration(
        &mut self,
        language_config: Option<Arc<LanguageConfiguration>>,
    ) {
        self.language = language_config;
    }

    pub fn language_configuration(&self) -> Option<&Arc<LanguageConfiguration>> {
        self.language.as_ref()
    }

    pub fn language_scope(&self) -> Option<&str> {
        self.language
            .as_ref()
            .map(|language| language.scope.as_str())
    }

    pub fn language_name(&self) -> Option<&str> {
        self.language
            .as_ref()
            .map(|language| language.language_id.as_str())
    }

    pub fn language_id(&self) -> Option<&str> {
        self.language_config()?
            .language_server_language_id
            .as_deref()
            .or_else(|| self.language_name())
    }

    pub fn language_config(&self) -> Option<&LanguageConfiguration> {
        self.language.as_deref()
    }

    pub fn diagnostics_gen(&self) -> u64 {
        self.diagnostics_gen
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn replace_diagnostics(
        &mut self,
        diagnostics: impl IntoIterator<Item = Diagnostic>,
        unchanged_sources: &[String],
        provider: Option<&DiagnosticProvider>,
    ) {
        if unchanged_sources.is_empty() {
            if let Some(provider) = provider {
                self.diagnostics
                    .retain(|diagnostic| &diagnostic.provider != provider);
            } else {
                self.diagnostics.clear();
            }
        } else {
            self.diagnostics.retain(|diagnostic| {
                if provider.is_some_and(|provider| provider != &diagnostic.provider) {
                    return true;
                }

                if let Some(source) = &diagnostic.source {
                    unchanged_sources.contains(source)
                } else {
                    false
                }
            });
        }
        self.diagnostics.extend(diagnostics);
        self.sort_diagnostics();
        self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
    }

    pub fn clear_diagnostics_for_language_server(&mut self, id: LanguageServerId) {
        self.diagnostics
            .retain(|diagnostic| diagnostic.provider.language_server_id() != Some(id));
        self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
    }

    pub fn remap_diagnostics(&mut self, changes: &ChangeSet, text: RopeSlice<'_>) {
        changes.update_positions(self.diagnostics.iter_mut().map(|diagnostic| {
            let assoc = if diagnostic.starts_at_word {
                Assoc::BeforeWord
            } else {
                Assoc::After
            };
            (&mut diagnostic.range.start, assoc)
        }));
        changes.update_positions(self.diagnostics.iter_mut().filter_map(|diagnostic| {
            if diagnostic.zero_width {
                return None;
            }
            let assoc = if diagnostic.ends_at_word {
                Assoc::AfterWord
            } else {
                Assoc::Before
            };
            Some((&mut diagnostic.range.end, assoc))
        }));
        self.diagnostics.retain_mut(|diagnostic| {
            if diagnostic.zero_width {
                diagnostic.range.end = diagnostic.range.start;
            } else if diagnostic.range.start >= diagnostic.range.end {
                return false;
            }
            diagnostic.line = text.char_to_line(diagnostic.range.start);
            true
        });
        self.sort_diagnostics();
    }

    pub fn language_servers(&self) -> impl Iterator<Item = &Client> {
        self.language_config().into_iter().flat_map(move |config| {
            config.language_servers.iter().filter_map(move |features| {
                let language_server = &**self.language_servers.get(&features.name)?;
                if language_server.is_initialized() {
                    Some(language_server)
                } else {
                    None
                }
            })
        })
    }

    pub fn all_language_servers(&self) -> impl Iterator<Item = &Arc<Client>> {
        self.language_servers.values()
    }

    pub fn has_language_servers(&self) -> bool {
        !self.language_servers.is_empty()
    }

    pub fn clear_language_servers(&mut self) {
        self.language_servers.clear();
    }

    pub fn language_server_by_name(&self, name: &LanguageServerName) -> Option<&Arc<Client>> {
        self.language_servers.get(name)
    }

    pub fn set_language_servers(
        &mut self,
        language_servers: HashMap<LanguageServerName, Arc<Client>>,
    ) {
        self.language_servers = language_servers;
    }

    pub fn remove_language_server_by_name(&mut self, name: &str) -> Option<Arc<Client>> {
        self.language_servers.remove(name)
    }

    pub fn language_servers_with_feature(
        &self,
        feature: LanguageServerFeature,
    ) -> impl Iterator<Item = &Client> {
        self.language_config().into_iter().flat_map(move |config| {
            config.language_servers.iter().filter_map(move |features| {
                let language_server = &**self.language_servers.get(&features.name)?;
                if language_server.is_initialized()
                    && language_server.supports_feature(feature)
                    && features.has_feature(feature)
                {
                    Some(language_server)
                } else {
                    None
                }
            })
        })
    }

    pub fn supports_language_server(&self, id: LanguageServerId) -> bool {
        self.language_servers()
            .any(|language_server| language_server.id() == id)
    }

    pub fn syntax(&self) -> Option<&Syntax> {
        self.syntax_snapshot.syntax()
    }

    pub fn set_syntax(&mut self, syntax: Option<Syntax>) {
        self.syntax_snapshot.set_tree(syntax);
    }

    pub fn syntax_snapshot(&self) -> SyntaxSnapshot {
        self.syntax_snapshot.snapshot()
    }

    pub fn update_syntax(
        &mut self,
        old_doc: RopeSlice<'_>,
        current_doc: RopeSlice<'_>,
        changes: &ChangeSet,
        loader: &syntax::Loader,
    ) {
        let syntax_status = self.syntax_snapshot.status();
        if self.syntax_snapshot.syntax().is_some() {
            let shape = DocumentShape::from_text(current_doc);
            let complexity = self
                .syntax_snapshot
                .syntax()
                .expect("syntax presence checked above")
                .complexity();
            let budget = syntax_budget(syntax_status, shape, complexity);
            let result = if matches!(budget, SyntaxBudget::Interactive(_)) {
                let syntax = self
                    .syntax_snapshot
                    .syntax_mut()
                    .expect("syntax presence checked above");
                syntax.update_with_timeout(
                    old_doc,
                    current_doc,
                    changes,
                    loader,
                    syntax::INTERACTIVE_PARSE_TIMEOUT,
                )
            } else {
                let syntax = self
                    .syntax_snapshot
                    .syntax_mut()
                    .expect("syntax presence checked above");
                syntax.update(old_doc, current_doc, changes, loader)
            };

            match result {
                Ok(()) => {
                    self.syntax_snapshot.mark_updated();
                }
                Err(syntax::HighlighterError::Timeout) => {
                    self.syntax_snapshot.mark_stale();
                    log_run_event("syntax_timeout_stale", || {
                        format!(
                            "lines={} bytes={} avg_bytes_per_line={} giant_lines={} use_interactive_budget={} reason={} changes={} total_layers={} root_injections={}",
                            shape.line_count,
                            shape.byte_count,
                            shape.average_bytes_per_line(),
                            shape.has_giant_lines(),
                            matches!(budget, SyntaxBudget::Interactive(_)),
                            match budget {
                                SyntaxBudget::Idle => "idle",
                                SyntaxBudget::Interactive(reason) => reason.label(),
                            },
                            changes.len(),
                            complexity.total_layers,
                            complexity.root_injections
                        )
                    });
                }
                Err(err) => {
                    log::error!("TS parser failed, disabling TS for the current buffer: {err}");
                    log_run_event("syntax_disabled", || {
                        format!(
                            "phase=update lines={} bytes={} error={err}",
                            current_doc.len_lines(),
                            current_doc.len_bytes()
                        )
                    });
                    self.syntax_snapshot.mark_disabled();
                }
            }
        }
    }

    pub fn refresh_stale_syntax(
        &mut self,
        current_doc: RopeSlice<'_>,
        loader: &syntax::Loader,
    ) -> bool {
        if !matches!(
            self.syntax_snapshot.status(),
            SyntaxStatus::StalePendingRefresh
        ) {
            return false;
        }

        if self.syntax_snapshot.syntax().is_none() {
            self.syntax_snapshot.mark_disabled();
            return true;
        }

        let result = {
            let syntax = self
                .syntax_snapshot
                .syntax_mut()
                .expect("syntax presence checked above");
            syntax.refresh_with_timeout(current_doc, loader, syntax::IDLE_PARSE_TIMEOUT)
        };

        match result {
            Ok(()) => {
                self.syntax_snapshot.mark_updated();
                log_run_event("syntax_idle_refresh_ok", || {
                    format!(
                        "lines={} bytes={}",
                        current_doc.len_lines(),
                        current_doc.len_bytes()
                    )
                });
                true
            }
            Err(syntax::HighlighterError::Timeout) => {
                log_run_event("syntax_idle_refresh_timeout", || {
                    format!(
                        "lines={} bytes={}",
                        current_doc.len_lines(),
                        current_doc.len_bytes()
                    )
                });
                false
            }
            Err(err) => {
                log::error!("TS parser failed during idle refresh, disabling TS for the current buffer: {err}");
                log_run_event("syntax_disabled", || {
                    format!(
                        "phase=idle_refresh lines={} bytes={} error={err}",
                        current_doc.len_lines(),
                        current_doc.len_bytes()
                    )
                });
                self.syntax_snapshot.mark_disabled();
                true
            }
        }
    }

    fn sort_diagnostics(&mut self) {
        self.diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic.range,
                diagnostic.severity,
                diagnostic.provider.clone(),
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        syntax_budget, DocumentShape, InteractiveSyntaxReason, SyntaxBudget, SyntaxStatus,
    };
    use helix_core::syntax::SyntaxComplexity;

    #[test]
    fn syntax_budget_uses_idle_for_plain_large_document() {
        let shape = DocumentShape {
            line_count: 4_000,
            byte_count: 128 * 1024,
        };
        let complexity = SyntaxComplexity {
            total_layers: 4,
            root_injections: 1,
        };

        assert_eq!(
            syntax_budget(SyntaxStatus::Fresh, shape, complexity),
            SyntaxBudget::Idle
        );
    }

    #[test]
    fn syntax_budget_uses_interactive_for_stale_syntax() {
        let shape = DocumentShape {
            line_count: 10,
            byte_count: 1_024,
        };
        let complexity = SyntaxComplexity {
            total_layers: 1,
            root_injections: 0,
        };

        assert_eq!(
            syntax_budget(SyntaxStatus::StalePendingRefresh, shape, complexity),
            SyntaxBudget::Interactive(InteractiveSyntaxReason::Stale)
        );
    }

    #[test]
    fn syntax_budget_uses_interactive_for_giant_lines() {
        let shape = DocumentShape {
            line_count: 2,
            byte_count: 512 * 1024,
        };
        let complexity = SyntaxComplexity {
            total_layers: 2,
            root_injections: 0,
        };

        assert_eq!(
            syntax_budget(SyntaxStatus::Fresh, shape, complexity),
            SyntaxBudget::Interactive(InteractiveSyntaxReason::GiantLines)
        );
    }

    #[test]
    fn syntax_budget_uses_interactive_for_layer_fanout() {
        let shape = DocumentShape {
            line_count: 500,
            byte_count: 32 * 1024,
        };
        let complexity = SyntaxComplexity {
            total_layers: 40,
            root_injections: 4,
        };

        assert_eq!(
            syntax_budget(SyntaxStatus::Fresh, shape, complexity),
            SyntaxBudget::Interactive(InteractiveSyntaxReason::LayerFanout)
        );
    }

    #[test]
    fn syntax_budget_uses_interactive_for_root_injections() {
        let shape = DocumentShape {
            line_count: 500,
            byte_count: 32 * 1024,
        };
        let complexity = SyntaxComplexity {
            total_layers: 12,
            root_injections: 20,
        };

        assert_eq!(
            syntax_budget(SyntaxStatus::Fresh, shape, complexity),
            SyntaxBudget::Interactive(InteractiveSyntaxReason::RootInjections)
        );
    }
}
