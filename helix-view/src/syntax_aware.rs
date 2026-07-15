use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::config::{LanguageConfiguration, LanguageServerFeature};
use helix_core::{Assoc, ChangeSet, Diagnostic, RopeSlice, Syntax};
use helix_lsp::{Client, LanguageServerId, LanguageServerName};
use std::collections::HashMap;
use std::sync::Arc;

use crate::revision::Revision;

#[derive(Debug, Default)]
pub struct SyntaxAwareState {
    syntax_snapshot: SyntaxSnapshotState,
    language: Option<Arc<LanguageConfiguration>>,
    diagnostics: Arc<Vec<Diagnostic>>,
    diagnostics_gen: u64,
    language_servers: HashMap<LanguageServerName, Arc<Client>>,
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
    tree: Option<Arc<Syntax>>,
}

impl SyntaxSnapshotState {
    fn snapshot(&self) -> SyntaxSnapshot {
        SyntaxSnapshot::new(self.revision, self.status)
    }

    fn syntax(&self) -> Option<&Syntax> {
        self.tree.as_deref()
    }

    fn syntax_arc(&self) -> Option<Arc<Syntax>> {
        self.tree.clone()
    }

    fn set_tree(&mut self, tree: Option<Syntax>) {
        self.tree = tree.map(Arc::new);
        self.status = if self.tree.is_some() {
            SyntaxStatus::Fresh
        } else {
            SyntaxStatus::Disabled
        };
        self.revision.advance();
    }

    fn mark_pending_initial_parse(&mut self) {
        self.tree = None;
        if self.status != SyntaxStatus::StalePendingRefresh {
            self.status = SyntaxStatus::StalePendingRefresh;
            self.revision.advance();
        }
    }

    fn mark_stale(&mut self) {
        if self.tree.is_some() && self.status != SyntaxStatus::StalePendingRefresh {
            self.status = SyntaxStatus::StalePendingRefresh;
            self.revision.advance();
        }
    }
}

impl SyntaxAwareState {
    pub fn set_language(&mut self, language_config: Option<Arc<LanguageConfiguration>>) {
        self.language = language_config;
        if self.language.is_none() {
            self.syntax_snapshot.set_tree(None);
            return;
        }
        self.syntax_snapshot.mark_pending_initial_parse();
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
        self.diagnostics.as_slice()
    }

    pub fn diagnostics_arc(&self) -> Arc<Vec<Diagnostic>> {
        self.diagnostics.clone()
    }

    pub fn swap_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) -> Arc<Vec<Diagnostic>> {
        self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
        std::mem::replace(&mut self.diagnostics, Arc::new(diagnostics))
    }

    pub fn replace_diagnostics(
        &mut self,
        diagnostics: impl IntoIterator<Item = Diagnostic>,
        unchanged_sources: &[String],
        provider: Option<&DiagnosticProvider>,
    ) {
        let current = Arc::make_mut(&mut self.diagnostics);
        if unchanged_sources.is_empty() {
            if let Some(provider) = provider {
                current.retain(|diagnostic| &diagnostic.provider != provider);
            } else {
                current.clear();
            }
        } else {
            current.retain(|diagnostic| {
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
        current.extend(diagnostics);
        self.sort_diagnostics();
        self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
    }

    pub fn clear_diagnostics_for_language_server(&mut self, id: LanguageServerId) {
        Arc::make_mut(&mut self.diagnostics)
            .retain(|diagnostic| diagnostic.provider.language_server_id() != Some(id));
        self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
    }

    pub fn remap_diagnostics(&mut self, changes: &ChangeSet, text: RopeSlice<'_>) {
        let diagnostics = Arc::make_mut(&mut self.diagnostics);
        changes.update_positions(diagnostics.iter_mut().map(|diagnostic| {
            let assoc = if diagnostic.starts_at_word {
                Assoc::BeforeWord
            } else {
                Assoc::After
            };
            (&mut diagnostic.range.start, assoc)
        }));
        changes.update_positions(diagnostics.iter_mut().filter_map(|diagnostic| {
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
        diagnostics.retain_mut(|diagnostic| {
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

    pub fn insert_language_server(
        &mut self,
        name: LanguageServerName,
        client: Arc<Client>,
    ) -> Option<Arc<Client>> {
        self.language_servers.insert(name, client)
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

    pub fn syntax_arc(&self) -> Option<Arc<Syntax>> {
        self.syntax_snapshot.syntax_arc()
    }

    pub fn set_syntax(&mut self, syntax: Option<Syntax>) {
        self.syntax_snapshot.set_tree(syntax);
    }

    pub fn syntax_snapshot(&self) -> SyntaxSnapshot {
        self.syntax_snapshot.snapshot()
    }

    pub fn mark_syntax_stale(&mut self) {
        self.syntax_snapshot.mark_stale();
    }

    fn sort_diagnostics(&mut self) {
        Arc::make_mut(&mut self.diagnostics).sort_by_key(|diagnostic| {
            (
                diagnostic.range,
                diagnostic.severity,
                diagnostic.provider.clone(),
            )
        });
    }
}
