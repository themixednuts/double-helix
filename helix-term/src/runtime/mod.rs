pub mod app_event;
pub mod ingress;
pub mod ui;

use helix_runtime::{TaskError, WaitSet};

pub type RuntimeEventSender = helix_runtime::Sender<RuntimeEvent>;
pub(crate) type ExitTaskResult = Result<anyhow::Result<RuntimeTaskEvent>, TaskError>;
pub type ExitTaskSet = WaitSet<anyhow::Result<RuntimeTaskEvent>>;

pub use app_event::AppEvent;
pub use ingress::{
    install_status_bridge, send_redraw_with, send_status_message_with, send_task_event_with,
    send_ui_command_with, RuntimeEvent, RuntimeTaskEvent, StatusBridge,
};
pub use ui::{
    apply_ui_command, apply_ui_command_opt, AssistantCommand, DapCommand, LayerCommand,
    PickerCommand, UiCommand,
};
