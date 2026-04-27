use std::sync::Arc;

use parking_lot::RwLock;

use crate::events;

type ErrorReporter = Arc<dyn Fn(anyhow::Error) + Send + Sync>;

type DocumentOpenHook =
    Arc<dyn for<'a> Fn(&mut events::DocumentDidOpen<'a>) -> anyhow::Result<()> + Send + Sync>;
type DocumentChangeHook =
    Arc<dyn for<'a> Fn(&mut events::DocumentDidChange<'a>) -> anyhow::Result<()> + Send + Sync>;
type EditorConfigChangeHook =
    Arc<dyn for<'a> Fn(&mut events::EditorConfigDidChange<'a>) -> anyhow::Result<()> + Send + Sync>;
type DocumentCloseHook =
    Arc<dyn for<'a> Fn(&mut events::DocumentDidClose<'a>) -> anyhow::Result<()> + Send + Sync>;
type SelectionChangeHook =
    Arc<dyn for<'a> Fn(&mut events::SelectionDidChange<'a>) -> anyhow::Result<()> + Send + Sync>;
type DiagnosticsChangeHook =
    Arc<dyn for<'a> Fn(&mut events::DiagnosticsDidChange<'a>) -> anyhow::Result<()> + Send + Sync>;
type DocumentFocusLostHook =
    Arc<dyn for<'a> Fn(&mut events::DocumentFocusLost<'a>) -> anyhow::Result<()> + Send + Sync>;
type LanguageServerInitializedHook = Arc<
    dyn for<'a> Fn(&mut events::LanguageServerInitialized<'a>) -> anyhow::Result<()> + Send + Sync,
>;
type LanguageServerExitedHook =
    Arc<dyn for<'a> Fn(&mut events::LanguageServerExited<'a>) -> anyhow::Result<()> + Send + Sync>;
type ConfigChangeHook =
    Arc<dyn for<'a> Fn(&mut events::ConfigDidChange<'a>) -> anyhow::Result<()> + Send + Sync>;

#[derive(Default)]
pub struct LifecycleBus {
    document_open: RwLock<Vec<DocumentOpenHook>>,
    document_change: RwLock<Vec<DocumentChangeHook>>,
    editor_config_change: RwLock<Vec<EditorConfigChangeHook>>,
    document_close: RwLock<Vec<DocumentCloseHook>>,
    selection_change: RwLock<Vec<SelectionChangeHook>>,
    diagnostics_change: RwLock<Vec<DiagnosticsChangeHook>>,
    document_focus_lost: RwLock<Vec<DocumentFocusLostHook>>,
    language_server_initialized: RwLock<Vec<LanguageServerInitializedHook>>,
    language_server_exited: RwLock<Vec<LanguageServerExitedHook>>,
    config_change: RwLock<Vec<ConfigChangeHook>>,
    error_reporter: RwLock<Option<ErrorReporter>>,
}

impl LifecycleBus {
    pub fn set_error_reporter(&self, reporter: ErrorReporter) {
        *self.error_reporter.write() = Some(reporter);
    }

    fn report_hook_error(&self, hook: &str, err: anyhow::Error) {
        log::error!("{hook} hook failed: {err:#}");
        if let Some(reporter) = self.error_reporter.read().as_ref() {
            reporter(err);
        }
    }

    pub fn on_document_open(
        &self,
        hook: impl for<'a> Fn(&mut events::DocumentDidOpen<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.document_open.write().push(Arc::new(hook));
    }

    pub fn on_document_change(
        &self,
        hook: impl for<'a> Fn(&mut events::DocumentDidChange<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.document_change.write().push(Arc::new(hook));
    }

    pub fn on_editor_config_change(
        &self,
        hook: impl for<'a> Fn(&mut events::EditorConfigDidChange<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.editor_config_change.write().push(Arc::new(hook));
    }

    pub fn on_document_close(
        &self,
        hook: impl for<'a> Fn(&mut events::DocumentDidClose<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.document_close.write().push(Arc::new(hook));
    }

    pub fn on_selection_change(
        &self,
        hook: impl for<'a> Fn(&mut events::SelectionDidChange<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.selection_change.write().push(Arc::new(hook));
    }

    pub fn on_diagnostics_change(
        &self,
        hook: impl for<'a> Fn(&mut events::DiagnosticsDidChange<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.diagnostics_change.write().push(Arc::new(hook));
    }

    pub fn on_document_focus_lost(
        &self,
        hook: impl for<'a> Fn(&mut events::DocumentFocusLost<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.document_focus_lost.write().push(Arc::new(hook));
    }

    pub fn on_language_server_initialized(
        &self,
        hook: impl for<'a> Fn(&mut events::LanguageServerInitialized<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.language_server_initialized
            .write()
            .push(Arc::new(hook));
    }

    pub fn on_language_server_exited(
        &self,
        hook: impl for<'a> Fn(&mut events::LanguageServerExited<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.language_server_exited.write().push(Arc::new(hook));
    }

    pub fn on_config_change(
        &self,
        hook: impl for<'a> Fn(&mut events::ConfigDidChange<'a>) -> anyhow::Result<()>
            + Send
            + Sync
            + 'static,
    ) {
        self.config_change.write().push(Arc::new(hook));
    }

    pub fn dispatch_document_open(&self, event: &mut events::DocumentDidOpen<'_>) {
        for hook in self.document_open.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("document_open", err);
            }
        }
    }

    pub fn dispatch_document_change(&self, event: &mut events::DocumentDidChange<'_>) {
        for hook in self.document_change.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("document_change", err);
            }
        }
    }

    pub fn dispatch_editor_config_change(&self, event: &mut events::EditorConfigDidChange<'_>) {
        for hook in self.editor_config_change.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("editor_config_change", err);
            }
        }
    }

    pub fn dispatch_document_close(&self, event: &mut events::DocumentDidClose<'_>) {
        for hook in self.document_close.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("document_close", err);
            }
        }
    }

    pub fn dispatch_selection_change(&self, event: &mut events::SelectionDidChange<'_>) {
        for hook in self.selection_change.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("selection_change", err);
            }
        }
    }

    pub fn dispatch_diagnostics_change(&self, event: &mut events::DiagnosticsDidChange<'_>) {
        for hook in self.diagnostics_change.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("diagnostics_change", err);
            }
        }
    }

    pub fn dispatch_document_focus_lost(&self, event: &mut events::DocumentFocusLost<'_>) {
        for hook in self.document_focus_lost.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("document_focus_lost", err);
            }
        }
    }

    pub fn dispatch_language_server_initialized(
        &self,
        event: &mut events::LanguageServerInitialized<'_>,
    ) {
        for hook in self.language_server_initialized.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("language_server_initialized", err);
            }
        }
    }

    pub fn dispatch_language_server_exited(&self, event: &mut events::LanguageServerExited<'_>) {
        for hook in self.language_server_exited.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("language_server_exited", err);
            }
        }
    }

    pub fn dispatch_config_change(&self, event: &mut events::ConfigDidChange<'_>) {
        for hook in self.config_change.read().iter() {
            if let Err(err) = hook(event) {
                self.report_hook_error("config_change", err);
            }
        }
    }
}
