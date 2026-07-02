pub mod app_event;
pub mod exit;
pub mod idle;
pub mod ingress;
pub mod ui;

use helix_runtime::{TaskError, WaitSet};

pub(crate) type ExitTaskResult = Result<anyhow::Result<RuntimeTaskEvent>, TaskError>;
pub type ExitTaskSet = WaitSet<anyhow::Result<RuntimeTaskEvent>>;

pub use app_event::AppEvent;
pub use exit::{
    apply_exit_task, drain_exit_tasks_blocking, drain_exit_tasks_collect, schedule_exit_task,
};
pub use idle::{IdleResetGate, IdleResetHandle, IdleResetReceiver, IdleResetRequest};
pub use ingress::{
    send_status_message_with, send_task_event_with, send_ui_command_with, status_error_reporter,
    IdleRender, PendingFormatWrite, RuntimeDelivery, RuntimeIngress, RuntimeIngressReceiver,
    RuntimeTaskDebouncer, RuntimeTaskEvent, RuntimeUiDebouncer,
};
pub use ui::{
    apply_ui_command, apply_ui_command_opt, AssistantCommand, DapCommand, LayerCommand,
    PickerCommand, UiCommand,
};
