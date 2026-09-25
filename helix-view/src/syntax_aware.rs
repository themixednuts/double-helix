use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::config::{LanguageConfiguration, LanguageServerFeature};
use helix_core::syntax::Loader;
use helix_core::{Assoc, ChangeSet, Diagnostic, Rope, RopeSlice, Syntax};
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

/// A tree to update in place, and everything that changed since it was
/// parsed. Updating reparses only what the changes touched.
#[derive(Debug, Clone)]
pub struct IncrementalSyntax {
    pub tree: Arc<Syntax>,
    /// The loader `tree` was parsed with; a different loader may number its
    /// languages differently, so only that one may update it.
    pub loader: Arc<Loader>,
    /// The text `tree` was parsed from.
    pub base: Rope,
    /// Every change since `base`, composed into one changeset.
    pub changes: ChangeSet,
}

#[derive(Debug, Default)]
struct SyntaxSnapshotState {
    revision: Revision,
    status: SyntaxStatus,
    tree: Option<Arc<Syntax>>,
    /// Set when `tree` came from a parse whose loader is known.
    tree_loader: Option<Arc<Loader>>,
    /// Text and changes since `tree` was parsed, recorded once a document
    /// edit leaves it behind.
    edits: Option<(Rope, ChangeSet)>,
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
        self.tree_loader = None;
        self.edits = None;
        self.tree = tree.map(Arc::new);
        self.status = if self.tree.is_some() {
            SyntaxStatus::Fresh
        } else {
            SyntaxStatus::Disabled
        };
        self.revision.advance();
    }

    fn set_parsed_tree(&mut self, tree: Syntax, loader: Arc<Loader>) {
        self.set_tree(Some(tree));
        self.tree_loader = Some(loader);
    }

    fn record_edit(&mut self, old_text: &Rope, changes: &ChangeSet) {
        if self.tree_loader.is_none() {
            return;
        }
        self.edits = Some(match self.edits.take() {
            Some((base, pending)) => (base, pending.compose(changes.clone())),
            None => (old_text.clone(), changes.clone()),
        });
    }

    fn incremental(&self) -> Option<IncrementalSyntax> {
        let (base, changes) = self.edits.clone()?;
        Some(IncrementalSyntax {
            tree: self.tree.clone()?,
            loader: self.tree_loader.clone()?,
            base,
            changes,
        })
    }

    fn mark_pending_initial_parse(&mut self) {
        self.tree_loader = None;
        self.edits = None;
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

    /// Installs a tree parsed with `loader`, which later edits may update in
    /// place; see [`IncrementalSyntax`].
    pub fn set_parsed_syntax(&mut self, syntax: Syntax, loader: Arc<Loader>) {
        self.syntax_snapshot.set_parsed_tree(syntax, loader);
    }

    pub fn record_syntax_edit(&mut self, old_text: &Rope, changes: &ChangeSet) {
        self.syntax_snapshot.record_edit(old_text, changes);
    }

    pub fn incremental_syntax(&self) -> Option<IncrementalSyntax> {
        self.syntax_snapshot.incremental()
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

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::Transaction;

    fn parse(text: &Rope, loader: &Loader) -> Syntax {
        let language = loader.language_for_name("rust").unwrap();
        Syntax::new(text.slice(..), language, loader).unwrap()
    }

    #[test]
    fn edits_since_a_parsed_tree_compose_over_its_text() {
        let loader = Arc::new(helix_core::config::default_lang_loader());
        let base = Rope::from_str("fn main() {}\n");
        let mut state = SyntaxAwareState::default();
        state.set_parsed_syntax(parse(&base, &loader), loader.clone());
        assert!(state.incremental_syntax().is_none());

        let mut text = base.clone();
        for (at, insert) in [(12, " // a"), (0, "// b\n")] {
            let edit = Transaction::change(&text, [(at, at, Some(insert.into()))].into_iter());
            let before = text.clone();
            assert!(edit.apply(&mut text));
            state.record_syntax_edit(&before, edit.changes());
        }

        let incremental = state.incremental_syntax().expect("edits since the parse");
        assert!(Arc::ptr_eq(&incremental.loader, &loader));
        assert_eq!(incremental.base, base);
        let mut replayed = incremental.base.clone();
        assert!(incremental.changes.apply(&mut replayed));
        assert_eq!(replayed, text);

        state.set_parsed_syntax(parse(&text, &loader), loader.clone());
        assert!(state.incremental_syntax().is_none());
    }

    #[test]
    fn a_tree_without_a_known_loader_is_never_updated_in_place() {
        let loader = helix_core::config::default_lang_loader();
        let base = Rope::from_str("fn main() {}\n");
        let mut state = SyntaxAwareState::default();
        state.set_syntax(Some(parse(&base, &loader)));

        let edit = Transaction::change(&base, [(0, 0, Some("// a\n".into()))].into_iter());
        state.record_syntax_edit(&base, edit.changes());

        assert!(state.incremental_syntax().is_none());
    }
}
