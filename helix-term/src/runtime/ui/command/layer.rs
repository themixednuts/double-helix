use std::path::PathBuf;

use helix_lsp::{lsp, LanguageServerId};

/// Layer stack / overlay operations.
#[derive(Debug, Clone)]
pub enum LayerCommand {
    /// Show notification history in a popup (content derived from editor state at apply time).
    PushNotificationHistory,
    /// Regex compile failed in cmdline; show error in a small overlay.
    InvalidRegexPopup { message: String },
    /// Remove cmdline prompt overlay if present (focus lost).
    DismissPromptIfPresent,
    /// Markdown in a popup (`Markdown::new` uses editor `syn_loader` at apply time).
    MarkdownPopup {
        layer_id: &'static str,
        markdown: String,
    },
    /// Push the directory file picker rooted at `root` (cmdline `:open` on a directory).
    PushFilePicker { root: PathBuf },
    /// Push the runtime package manager.
    PkgManager,
    /// Push the package manager focused on Agent Client Protocol agents.
    AcpAgentsManager,
    /// Picker to run an LSP command when multiple servers advertise the same command list.
    LspCommandPicker {
        commands: Vec<(LanguageServerId, lsp::Command)>,
    },
    /// `:run-shell-command` output: positioned markdown popup (if non-empty) + status.
    ShellRunOutput { output: String },
}
