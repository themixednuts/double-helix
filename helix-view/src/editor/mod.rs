mod access;
mod assistant;
mod collab;
mod component;
mod config;
mod core;
mod document;
mod embed;
mod file_operation;
mod focus;
mod hooks;
mod jump;
mod label;
mod language;
mod navigation;
mod refresh;
mod runtime;
mod session;
mod setup;
mod state;
mod status;
mod storage;
mod surface;
#[cfg(test)]
mod test_support;
mod types;
mod workspace;

use crate::document::Mode;

pub use crate::bench::{BenchSnapshot, BenchState};
pub use config::{
    get_terminal_provider, AcpConfig, AgentConfig, AutoSave, AutoSaveAfterDelay, BufferLineConfig,
    BufferLineRenderMode, BufferPickerConfig, CmdlineConfig, CmdlineIcons, CmdlineStyle,
    CompletionHighlight, CompletionHighlightType, Config, CursorShapeConfig, EditingEngineConfig,
    FileExplorerConfig, FilePickerConfig, GradientBorderConfig, GradientDirection, GutterConfig,
    GutterLineNumbersConfig, GutterType, IndentGuidesConfig, InlineBlameConfig, InlineBlameShow,
    KittyKeyboardProtocolConfig, LineEndingConfig, LineNumber, LspConfig, LspSelectionRangeConfig,
    ModeConfig, NotificationBorderConfig, NotificationBorderStyle, NotificationConfig,
    NotificationEmojis, NotificationIcons, NotificationPosition, NotificationShadowConfig,
    NotificationStyle, PickerStartPosition, PkgConfig, PopupBorderConfig, SearchConfig,
    SignatureHelpPosition, SmartTabConfig, StatusLineConfig, StatusLineElement, TerminalConfig,
    WhitespaceCharacters, WhitespaceConfig, WhitespaceRender, WhitespaceRenderValue,
    WordCompletion,
};
pub use core::Editor;
pub use embed::{
    DocumentSnapshot, EditorSession, EditorSessionBuilder, EditorSessionEvent, EditorSnapshot,
    FocusedSnapshot, InsertPlacement, StatusSnapshot, ViewSnapshot,
};
pub use helix_core::diagnostic::Severity;
pub use hooks::LifecycleBus;
pub use navigation::{Action, CloseError};
pub use runtime::DocumentSaveReport;
pub use session::BenchOverlay;
pub use setup::EditorBuilder;
pub use state::CursorCache;
pub use status::{Notification, NotificationManager, WorkspaceDiagnosticCounts};
pub use types::{
    Activation, AssistantUpdateOutcome, BenchActionUpdate, BenchFrameUpdate, Breakpoint,
    ClosePolicy, CompleteAction, ConfigEvent, EditTarget, EditorEvent, FrameSelection,
    PanelBehavior, SavePolicy, ShowDocumentRequest, ThreadSelectPolicy,
};
pub use workspace::DetachedPreviewDocument;
impl crate::traits::Modal for Editor {
    fn mode(&self) -> Mode {
        Editor::mode(self)
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
}
