use std::path::PathBuf;

use helix_dap::{StackFrame, ThreadId as DebugThreadId};
use helix_runtime::Sender as IngressSender;
use helix_view::Editor;

use crate::runtime::{
    ingress::RuntimeEvent,
    send_task_event_with,
    RuntimeTaskEvent,
};

pub(crate) fn apply_dap_restarted(editor: &mut Editor) {
    editor.set_status("Debugging session restarted");
}

pub(crate) fn apply_resume_debugger_application(editor: &mut Editor) {
    debugger!(editor).resume_application();
}

pub(crate) fn apply_unset_active_debug_client(editor: &mut Editor) {
    editor.debug_adapters.unset_active_client();
}

pub(crate) fn request_select_debug_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    thread_id: DebugThreadId,
    force: bool,
) {
    let work = editor.runtime().work().clone();
    let Some(debugger) = editor.debug_adapters.get_active_client_mut() else {
        editor.set_error("Debugger is not running");
        return;
    };

    if !force && debugger.thread_id.is_some() {
        return;
    }

    debugger.thread_id = Some(thread_id);
    let request = debugger.stack_trace_request(thread_id);
    let ingress_for_error = ingress.clone();

    work.spawn(async move {
        match request.await {
            Ok((frames, _)) => {
                send_task_event_with(
                    RuntimeTaskEvent::ApplyStackFrames {
                        thread_id,
                        frames,
                        auto_select_first_frame: true,
                    },
                    ingress,
                )
                .await;
            }
            Err(err) => {
                send_task_event_with(
                    RuntimeTaskEvent::SetEditorError {
                        message: format!("Failed to fetch stack trace: {}", err),
                    },
                    ingress_for_error,
                )
                .await;
            }
        }
    })
    .detach();
}

pub(crate) fn request_pause_debug_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    thread_id: DebugThreadId,
) {
    let work = editor.runtime().work().clone();
    let debugger = debugger!(editor);
    let request = debugger.pause(thread_id);
    work.spawn(async move {
        if let Err(err) = request.await {
            send_task_event_with(
                RuntimeTaskEvent::SetEditorError {
                    message: format!("Failed to pause: {}", err),
                },
                ingress,
            )
            .await;
        }
    })
    .detach();
}

pub(crate) fn apply_select_stack_frame(
    editor: &mut Editor,
    thread_id: DebugThreadId,
    frame_id: usize,
) {
    let debugger = debugger!(editor);
    let pos = debugger.stack_frames[&thread_id]
        .iter()
        .position(|f| f.id == frame_id);
    debugger.active_frame = pos;

    let frame = debugger.stack_frames[&thread_id].get(pos.unwrap_or(0)).cloned();
    if let Some(frame) = &frame {
        helix_view::handlers::dap::jump_to_stack_frame(editor, frame);
    }
}

pub(crate) fn apply_stack_frames(
    editor: &mut Editor,
    thread_id: DebugThreadId,
    frames: Vec<StackFrame>,
    auto_select_first_frame: bool,
) {
    let debugger = debugger!(editor);
    debugger.stack_frames.insert(thread_id, frames);
    debugger.active_frame = auto_select_first_frame.then_some(0);

    if auto_select_first_frame {
        let frame = debugger
            .stack_frames
            .get(&thread_id)
            .and_then(|frames| frames.first())
            .cloned();
        if let Some(frame) = &frame {
            helix_view::handlers::dap::jump_to_stack_frame(editor, frame);
        }
    }
}

pub(crate) fn apply_breakpoint_condition(
    editor: &mut Editor,
    path: PathBuf,
    index: usize,
    condition: Option<String>,
) {
    let Some(breakpoints) = editor.breakpoints.get_mut(&path) else {
        editor.set_error("Breakpoint file disappeared");
        return;
    };
    if index >= breakpoints.len() {
        editor.set_error("Breakpoint disappeared");
        return;
    }

    breakpoints[index].condition = condition;
    let debugger = debugger!(editor);

    if let Err(err) = helix_view::handlers::dap::breakpoints_changed(debugger, path, breakpoints) {
        editor.set_error(format!("Failed to set breakpoints: {}", err));
    }
}

pub(crate) fn apply_breakpoint_log_message(
    editor: &mut Editor,
    path: PathBuf,
    index: usize,
    log_message: Option<String>,
) {
    let Some(breakpoints) = editor.breakpoints.get_mut(&path) else {
        editor.set_error("Breakpoint file disappeared");
        return;
    };
    if index >= breakpoints.len() {
        editor.set_error("Breakpoint disappeared");
        return;
    }

    breakpoints[index].log_message = log_message;
    let debugger = debugger!(editor);

    if let Err(err) = helix_view::handlers::dap::breakpoints_changed(debugger, path, breakpoints) {
        editor.set_error(format!("Failed to set breakpoints: {}", err));
    }
}

pub(crate) fn apply_toggle_breakpoint(editor: &mut Editor, path: PathBuf, line: usize) {
    let breakpoints = editor.breakpoints.entry(path.clone()).or_default();

    if let Some(pos) = breakpoints
        .iter()
        .position(|breakpoint| breakpoint.line == line)
    {
        breakpoints.remove(pos);
    } else {
        breakpoints.push(helix_view::editor::Breakpoint {
            line,
            ..Default::default()
        });
    }

    let debugger = debugger!(editor);

    if let Err(err) = helix_view::handlers::dap::breakpoints_changed(debugger, path, breakpoints) {
        editor.set_error(format!("Failed to set breakpoints: {}", err));
    }
}
