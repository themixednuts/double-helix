use crate::theme::Theme;
use std::borrow::Cow;
use tokio::time::{Duration, Instant};

use super::{Editor, NotificationStyle, PopupBorderConfig, Severity};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WorkspaceDiagnosticCounts {
    pub hints: u32,
    pub info: u32,
    pub warnings: u32,
    pub errors: u32,
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
        if let Some(timeout) = self.timeout {
            let elapsed = self.timestamp.elapsed();
            let expired = elapsed >= timeout;
            if expired {
                log::warn!(
                    "Notification {} expired: elapsed={:?}, timeout={:?}",
                    self.id,
                    elapsed,
                    timeout
                );
            }
            expired
        } else {
            false
        }
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

    pub fn document_diagnostics(
        &self,
        uri: &helix_core::Uri,
    ) -> Vec<(
        helix_lsp::lsp::Diagnostic,
        helix_core::diagnostic::DiagnosticProvider,
    )> {
        self.diagnostics.get(uri).cloned().unwrap_or_default()
    }

    pub fn diagnostics_snapshot(&self) -> super::types::Diagnostics {
        self.diagnostics.clone()
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
            self.set_theme(last_theme);
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
            theme_name.and_then(|name| self.theme_loader.load(&name).ok());
    }

    pub fn assistant_theme(&self) -> &Theme {
        self.frontend
            .assistant_panel_theme
            .as_ref()
            .unwrap_or(&self.theme)
    }

    fn set_theme_impl(&mut self, theme: Theme, preview: ThemeAction) {
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

        self._refresh();
    }
}
