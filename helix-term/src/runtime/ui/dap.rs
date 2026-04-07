use crate::{
    compositor::Compositor,
    runtime::{ui::command::DapThreadAction, DapCommand, RuntimeEvent, RuntimeTaskEvent},
    ui::{self, Picker, Prompt, PromptEvent},
};
use helix_dap::{StackFrame, Thread, ThreadStates};
use helix_view::editor::Breakpoint;

pub(crate) fn apply_dap_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
    cmd: DapCommand,
) {
    match cmd {
        DapCommand::PushDebugParameterPrompt {
            completions,
            config_name,
            params,
        } => {
            let prompt =
                crate::commands::dap::debug_parameter_prompt(completions, config_name, params);
            compositor.push(Box::new(prompt));
        }
        DapCommand::PushBreakpointConditionPrompt {
            path,
            index,
            initial,
        } => {
            let mut prompt = Prompt::new(
                "condition:".into(),
                None,
                ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    helix_runtime::send_blocking(
                        &cx.ingress,
                        RuntimeEvent::Task(
                            crate::runtime::RuntimeTaskEvent::SetBreakpointCondition {
                                path: path.clone(),
                                index,
                                condition: match input {
                                    "" => None,
                                    input => Some(input.to_owned()),
                                },
                            },
                        ),
                    );
                },
            );
            if let Some(condition) = initial {
                prompt.insert_str(&condition, editor);
            }
            compositor.push(Box::new(prompt));
        }
        DapCommand::PushBreakpointLogPrompt {
            path,
            index,
            initial,
        } => {
            let mut prompt = Prompt::new(
                "log-message:".into(),
                None,
                ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    helix_runtime::send_blocking(
                        &cx.ingress,
                        RuntimeEvent::Task(
                            crate::runtime::RuntimeTaskEvent::SetBreakpointLogMessage {
                                path: path.clone(),
                                index,
                                log_message: match input {
                                    "" => None,
                                    input => Some(input.to_owned()),
                                },
                            },
                        ),
                    );
                },
            );
            if let Some(log_message) = initial {
                prompt.insert_str(&log_message, editor);
            }
            compositor.push(Box::new(prompt));
        }
        DapCommand::ThreadsPicker { threads, action } => {
            if threads.len() == 1 {
                let thread_id = threads[0].id;
                let event = match action {
                    DapThreadAction::Switch => RuntimeTaskEvent::SelectDebugThread {
                        thread_id,
                        force: true,
                    },
                    DapThreadAction::Pause => RuntimeTaskEvent::PauseDebugThread { thread_id },
                };
                helix_runtime::send_blocking(&ingress, RuntimeEvent::Task(event));
                return;
            }
            let Some(debugger) = editor.debug_adapters.get_active_client_mut() else {
                editor.set_error("Debugger is not running");
                return;
            };
            let thread_states = debugger.thread_states.clone();
            let columns = [
                ui::PickerColumn::new("name", |item: &Thread, _| item.name.as_str().into()),
                ui::PickerColumn::new("state", |item: &Thread, thread_states: &ThreadStates| {
                    thread_states
                        .get(&item.id)
                        .map(|state| state.as_str())
                        .unwrap_or("unknown")
                        .into()
                }),
            ];
            let picker = Picker::new(
                columns,
                0,
                threads,
                thread_states,
                editor.runtime().clone(),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context, thread: &Thread, _action| {
                    let event = match action {
                        DapThreadAction::Switch => RuntimeTaskEvent::SelectDebugThread {
                            thread_id: thread.id,
                            force: true,
                        },
                        DapThreadAction::Pause => RuntimeTaskEvent::PauseDebugThread {
                            thread_id: thread.id,
                        },
                    };
                    helix_runtime::send_blocking(&cx.ingress, RuntimeEvent::Task(event));
                },
            )
            .with_preview(move |editor, thread| {
                let frames = editor
                    .debug_adapters
                    .get_active_client()
                    .as_ref()?
                    .stack_frames
                    .get(&thread.id)?;
                let frame = frames.first()?;
                let path = frame.source.as_ref()?.path.as_ref()?.as_path();
                let pos = Some((
                    frame.line.saturating_sub(1),
                    frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                ));
                Some((path.into(), pos))
            });
            compositor.push(Box::new(picker));
        }
        DapCommand::StackFramesPicker { thread_id, frames } => {
            let columns = [ui::PickerColumn::new("frame", |item: &StackFrame, _| {
                item.name.as_str().into()
            })];
            let picker = Picker::new(
                columns,
                0,
                frames,
                (),
                editor.runtime().clone(),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context, frame: &StackFrame, _action| {
                    helix_runtime::send_blocking(
                        &cx.ingress,
                        RuntimeEvent::Task(RuntimeTaskEvent::SelectStackFrame {
                            thread_id,
                            frame_id: frame.id,
                        }),
                    );
                },
            )
            .with_preview(move |_editor, frame| {
                frame
                    .source
                    .as_ref()
                    .and_then(|source| source.path.as_ref())
                    .map(|path| {
                        (
                            path.as_path().into(),
                            Some((
                                frame.line.saturating_sub(1),
                                frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                            )),
                        )
                    })
            });
            compositor.push(Box::new(picker));
        }
    }
}

pub(crate) fn get_breakpoint_at_current_line(
    editor: &mut helix_view::Editor,
) -> Option<(usize, Breakpoint)> {
    let (view_id, doc) = focused!(editor);
    let text = doc.text().slice(..);

    let line = doc.selection(view_id).primary().cursor_line(text);
    let path = doc.path()?;
    editor.breakpoints.get(path).and_then(|breakpoints| {
        let index = breakpoints
            .iter()
            .position(|breakpoint| breakpoint.line == line);
        index.map(|index| (index, breakpoints[index].clone()))
    })
}
