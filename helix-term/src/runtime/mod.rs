pub mod app_event;
pub mod exit;
pub mod idle;
pub mod ingress;
pub mod pkg;
mod plugin;
mod syntax;
pub mod ui;

use helix_runtime::{TaskError, WaitSet};

pub(crate) type ExitTaskResult = Result<anyhow::Result<RuntimeTaskEvent>, TaskError>;
pub type ExitTaskSet = WaitSet<anyhow::Result<RuntimeTaskEvent>>;

pub use app_event::{AppEvent, ForegroundAdmissionError, ForegroundEvents};
pub use exit::{
    apply_exit_task, drain_exit_tasks_blocking, drain_exit_tasks_collect, schedule_exit_task,
};
pub use idle::{IdleResetGate, IdleResetHandle, IdleResetReceiver, IdleResetRequest};
pub use ingress::{
    send_status_message_with, send_task_event_with, send_ui_command_with, status_error_reporter,
    AssistantBackendConnection, IdleRender, PendingFormatWrite, PreparedAssistantAgents,
    PreparedConfigReload, PreparedLanguageLoader, RuntimeDelivery, RuntimeIngress,
    RuntimeIngressReceiver, RuntimeTaskDebouncer, RuntimeTaskEvent, RuntimeUiDebouncer,
};
pub use pkg::{
    PkgAdmissionError, PkgFailure, PkgOperation, PkgOperationOrigin, PkgOperationOutcome,
};
pub use plugin::PluginNotification;
pub use ui::{
    apply_ui_command, AssistantCommand, DapCommand, DocumentCommand, DocumentOpenAlignment,
    DocumentOpenCompletionTarget, DocumentOpenLane, DocumentOpenPostAction, DocumentOpenRequest,
    DocumentOpenSelection, DocumentOpenTarget, DocumentReloadOrigin, FffOpenRecord, LayerCommand,
    PickerCommand, PkgCommand, PkgRefreshStage, UiCommand,
};
