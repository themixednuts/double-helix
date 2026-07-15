use crate::theme::Theme;
use std::{borrow::Cow, sync::Arc};
use tokio::time::{Duration, Instant};

use super::{Editor, NotificationStyle, PopupBorderConfig, Severity};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WorkspaceDiagnosticCounts {
    pub hints: u32,
    pub info: u32,
    pub warnings: u32,
    pub errors: u32,
}

impl WorkspaceDiagnosticCounts {
    pub fn total(self) -> u32 {
        self.hints
            .saturating_add(self.info)
            .saturating_add(self.warnings)
            .saturating_add(self.errors)
    }

    pub(crate) fn from_diagnostics<'a>(
        diagnostics: impl IntoIterator<Item = &'a helix_lsp::lsp::Diagnostic>,
    ) -> Self {
        let mut counts = Self::default();
        for diagnostic in diagnostics {
            counts.increment(diagnostic);
        }
        counts
    }

    pub(crate) fn increment(&mut self, diagnostic: &helix_lsp::lsp::Diagnostic) {
        match diagnostic.severity {
            Some(helix_lsp::lsp::DiagnosticSeverity::WARNING) => {
                self.warnings = self.warnings.saturating_add(1)
            }
            Some(helix_lsp::lsp::DiagnosticSeverity::ERROR) => {
                self.errors = self.errors.saturating_add(1)
            }
            Some(helix_lsp::lsp::DiagnosticSeverity::INFORMATION) => {
                self.info = self.info.saturating_add(1)
            }
            Some(helix_lsp::lsp::DiagnosticSeverity::HINT) | None => {
                self.hints = self.hints.saturating_add(1)
            }
            Some(_) => self.hints = self.hints.saturating_add(1),
        }
    }

    pub(crate) fn replace(&mut self, previous: Self, replacement: Self) {
        debug_assert!(self.hints >= previous.hints);
        debug_assert!(self.info >= previous.info);
        debug_assert!(self.warnings >= previous.warnings);
        debug_assert!(self.errors >= previous.errors);

        self.hints = self
            .hints
            .saturating_sub(previous.hints)
            .saturating_add(replacement.hints);
        self.info = self
            .info
            .saturating_sub(previous.info)
            .saturating_add(replacement.info);
        self.warnings = self
            .warnings
            .saturating_sub(previous.warnings)
            .saturating_add(replacement.warnings);
        self.errors = self
            .errors
            .saturating_sub(previous.errors)
            .saturating_add(replacement.errors);
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: usize,
    pub message: Cow<'static, str>,
    pub severity: Severity,
    pub timestamp: Instant,
    pub timeout: Option<Duration>,
    pub dismissed: bool,
    pub corner_radius: Option<u8>,
}

impl Notification {
    pub fn new(message: impl Into<Cow<'static, str>>, severity: Severity) -> Self {
        Self {
            id: 0,
            message: message.into(),
            severity,
            timestamp: Instant::now(),
            timeout: None,
            dismissed: false,
            corner_radius: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn is_expired(&self) -> bool {
        self.timeout
            .is_some_and(|timeout| self.timestamp.elapsed() >= timeout)
    }

    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }
}

#[derive(Debug, Default)]
pub struct NotificationManager {
    pub(crate) notifications: Vec<Notification>,
    next_id: usize,
    max_history: usize,
}

impl NotificationManager {
    pub fn new(max_history: usize) -> Self {
        Self {
            notifications: Vec::new(),
            next_id: 1,
            max_history,
        }
    }

    pub fn add(&mut self, mut notification: Notification) -> usize {
        notification.id = self.next_id;
        self.next_id += 1;

        let id = notification.id;
        self.notifications.push(notification);

        if self.notifications.len() > self.max_history {
            let excess = self.notifications.len() - self.max_history;
            self.notifications.drain(0..excess);
        }

        id
    }

    pub fn dismiss(&mut self, id: usize) {
        if let Some(notification) = self
            .notifications
            .iter_mut()
            .find(|notification| notification.id == id)
        {
            notification.dismiss();
        }
    }

    pub fn dismiss_all(&mut self) {
        for notification in &mut self.notifications {
            notification.dismiss();
        }
    }

    pub fn get_active(&self) -> Vec<&Notification> {
        self.notifications
            .iter()
            .filter(|notification| !notification.dismissed && !notification.is_expired())
            .collect()
    }

    pub fn get_all(&self) -> &[Notification] {
        &self.notifications
    }

    pub fn cleanup_expired(&mut self) {
        self.notifications
            .retain(|notification| !notification.is_expired() && !notification.dismissed);
    }

    pub fn clear_history(&mut self) {
        self.notifications.clear();
    }
}

#[derive(Debug, Clone, Copy)]
enum ThemeAction {
    Set,
    Preview,
}

impl Editor {
    pub fn popup_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Popup
    }

    pub fn menu_border(&self) -> bool {
        self.config().popup_border == PopupBorderConfig::All
            || self.config().popup_border == PopupBorderConfig::Menu
    }

    pub fn workspace_diagnostic_counts(&self) -> WorkspaceDiagnosticCounts {
        self.workspace_diagnostic_counts
    }

    pub fn diagnostics_revision(&self) -> u64 {
        self.diagnostics_revision
    }

    pub fn workspace_diagnostic_summaries(
        &self,
    ) -> impl Iterator<Item = (&helix_core::Uri, WorkspaceDiagnosticCounts)> {
        self.diagnostic_summaries
            .iter()
            .map(|(uri, summary)| (uri, *summary))
    }

    pub fn workspace_diagnostic_path_summary(
        &self,
        path: &std::path::Path,
    ) -> Option<WorkspaceDiagnosticCounts> {
        self.diagnostic_path_summaries.get(path).copied()
    }

    pub fn workspace_diagnostic_path_summaries_under<'a>(
        &'a self,
        root: &'a std::path::Path,
    ) -> impl Iterator<Item = (&'a std::path::Path, WorkspaceDiagnosticCounts)> {
        let start = root.to_path_buf();
        self.diagnostic_path_summaries
            .range(start..)
            .take_while(move |(path, _)| path.starts_with(root))
            .map(|(path, summary)| (path.as_path(), *summary))
    }

    pub(crate) fn replace_workspace_diagnostic_summary(
        &mut self,
        uri: &helix_core::Uri,
        previous: WorkspaceDiagnosticCounts,
        replacement: WorkspaceDiagnosticCounts,
    ) {
        let summary_empty = {
            let summary = self.diagnostic_summaries.entry(uri.clone()).or_default();
            summary.replace(previous, replacement);
            summary.total() == 0
        };
        if summary_empty {
            self.diagnostic_summaries.remove(uri);
        }

        let Some(path) = uri.as_path() else {
            return;
        };
        let path = path.to_path_buf();
        for ancestor in path.ancestors() {
            let remove = {
                let summary = self
                    .diagnostic_path_summaries
                    .entry(ancestor.to_path_buf())
                    .or_default();
                summary.replace(previous, replacement);
                summary.total() == 0
            };
            if remove {
                self.diagnostic_path_summaries.remove(ancestor);
            }
        }
    }

    pub fn document_diagnostics(
        &self,
        uri: &helix_core::Uri,
    ) -> Vec<(
        helix_lsp::lsp::Diagnostic,
        helix_core::diagnostic::DiagnosticProvider,
    )> {
        self.diagnostics
            .get(uri)
            .map(|diagnostics| diagnostics.as_ref().clone())
            .unwrap_or_default()
    }

    pub fn diagnostics_snapshot(
        &self,
    ) -> std::collections::BTreeMap<
        helix_core::Uri,
        Vec<(
            helix_lsp::lsp::Diagnostic,
            helix_core::diagnostic::DiagnosticProvider,
        )>,
    > {
        self.diagnostics
            .iter()
            .map(|(uri, diagnostics)| (uri.clone(), diagnostics.as_ref().clone()))
            .collect()
    }

    pub fn clear_status(&mut self) {
        self.status_msg = None;
        self.notifications.dismiss_all();
    }

    #[inline]
    pub fn set_status<T: Into<Cow<'static, str>>>(&mut self, status: T) {
        let status = status.into();
        log::debug!("editor status: {}", status);

        let config = self.config();
        if config.notifications.enable && config.notifications.style == NotificationStyle::Popup {
            self.notify_info(status);
        } else {
            self.status_msg = Some((status.clone(), Severity::Info));
            if config.notifications.enable {
                self.notify_info(status);
            }
        }
    }

    #[inline]
    pub fn set_error<T: Into<Cow<'static, str>>>(&mut self, error: T) {
        let error = error.into();
        log::debug!("editor error: {}", error);

        let config = self.config();
        self.status_msg = Some((error.clone(), Severity::Error));
        if config.notifications.enable {
            self.notify_error(error);
        }
    }

    #[inline]
    pub fn set_result<T: Into<Cow<'static, str>>>(&mut self, result: Result<T, T>) {
        match result {
            Ok(ok) => self.set_status(ok),
            Err(err) => self.set_error(err),
        }
    }

    #[inline]
    pub fn set_warning<T: Into<Cow<'static, str>>>(&mut self, warning: T) {
        let warning = warning.into();
        log::warn!("editor warning: {}", warning);

        let config = self.config();
        if config.notifications.enable && config.notifications.style == NotificationStyle::Popup {
            self.notify_warning(warning);
        } else {
            self.status_msg = Some((warning.clone(), Severity::Warning));
            if config.notifications.enable {
                self.notify_warning(warning);
            }
        }
    }

    #[inline]
    pub fn get_status(&self) -> Option<(&Cow<'static, str>, &Severity)> {
        self.status_msg
            .as_ref()
            .map(|(status, sev)| (status, sev))
            .or_else(|| {
                self.notifications
                    .get_active()
                    .last()
                    .map(|n| (&n.message, &n.severity))
            })
    }

    #[inline]
    pub fn is_err(&self) -> bool {
        self.get_status()
            .map(|(_, sev)| *sev == Severity::Error)
            .unwrap_or(false)
    }

    pub fn notify<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Info)
    }

    pub fn notify_info<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Info)
    }

    pub fn notify_warning<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Warning)
    }

    pub fn notify_error<T: Into<Cow<'static, str>>>(&mut self, message: T) -> usize {
        self.notify_with_severity(message, Severity::Error)
    }

    pub fn notify_with_severity<T: Into<Cow<'static, str>>>(
        &mut self,
        message: T,
        severity: Severity,
    ) -> usize {
        let config = self.config();
        if !config.notifications.enable {
            match severity {
                Severity::Error => self.set_error(message),
                Severity::Warning => self.set_warning(message),
                _ => self.set_status(message),
            }
            return 0;
        }

        let mut notification = Notification::new(message, severity);

        if config.notifications.default_timeout > tokio::time::Duration::ZERO {
            let timeout = config.notifications.default_timeout;
            notification = notification.with_timeout(timeout);
            let runtime = self.runtime.clone();
            let redraw = self.frame_gate.handle();
            let redraw = async move {
                tokio::time::sleep(timeout).await;
                redraw.request_redraw();
            };
            runtime.work().spawn(redraw).detach();
        }

        let id = self.notifications.add(notification);

        if config.notifications.style == NotificationStyle::Statusline {
            match severity {
                Severity::Error => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Error));
                }
                Severity::Warning => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Warning));
                }
                _ => {
                    let msg = self
                        .notifications
                        .notifications
                        .last()
                        .unwrap()
                        .message
                        .clone();
                    self.status_msg = Some((msg, Severity::Info));
                }
            }
        }

        id
    }

    pub fn dismiss_notification(&mut self, id: usize) {
        self.notifications.dismiss(id);
    }

    pub fn dismiss_all_notifications(&mut self) {
        self.notifications.dismiss_all();
    }

    pub fn get_active_notifications(&self) -> Vec<&Notification> {
        self.notifications.get_active()
    }

    pub fn get_notification_history(&self) -> &[Notification] {
        self.notifications.get_all()
    }

    pub fn clear_notification_history(&mut self) {
        self.notifications.clear_history();
    }

    pub fn cleanup_notifications(&mut self) {
        self.notifications.cleanup_expired();
    }

    pub fn unset_theme_preview(&mut self) {
        if let Some(last_theme) = self.last_theme.take() {
            self.set_theme_arc(last_theme, ThemeAction::Set);
        }
    }

    pub fn set_theme_preview(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Preview);
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.set_theme_impl(theme, ThemeAction::Set);
    }

    pub fn apply_assistant_agent_theme(&mut self, agent_index: Option<usize>) {
        let theme_name = agent_index.and_then(|i| {
            self.config
                .load()
                .agents
                .get(i)
                .and_then(|a| a.theme.as_ref())
                .cloned()
        });

        self.frontend.assistant_panel_theme =
            theme_name.and_then(|name| self.theme_loader.load(&name).ok().map(Arc::new));
        self.theme_generation = self.theme_generation.wrapping_add(1);
    }

    pub fn assistant_theme(&self) -> &Theme {
        self.frontend
            .assistant_panel_theme
            .as_ref()
            .map_or(self.theme.as_ref(), Arc::as_ref)
    }

    pub fn theme_arc(&self) -> Arc<Theme> {
        Arc::clone(&self.theme)
    }

    pub fn assistant_theme_arc(&self) -> Arc<Theme> {
        self.frontend
            .assistant_panel_theme
            .as_ref()
            .map_or_else(|| Arc::clone(&self.theme), Arc::clone)
    }

    fn set_theme_impl(&mut self, theme: Theme, preview: ThemeAction) {
        self.set_theme_arc(Arc::new(theme), preview);
    }

    fn set_theme_arc(&mut self, theme: Arc<Theme>, preview: ThemeAction) {
        if theme.find_highlight_exact("ui.selection").is_none() {
            self.set_error("Invalid theme: `ui.selection` required");
            return;
        }

        let scopes = theme.scopes();
        (*self.syn_loader).load().set_scopes(scopes.to_vec());

        match preview {
            ThemeAction::Preview => {
                let last_theme = std::mem::replace(&mut self.theme, theme);
                self.last_theme.get_or_insert(last_theme);
            }
            ThemeAction::Set => {
                self.last_theme = None;
                self.theme = theme;
            }
        }

        self.theme_generation = self.theme_generation.wrapping_add(1);

        self._refresh();
    }
}
