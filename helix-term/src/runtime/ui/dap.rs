use crate::{
    compositor::Compositor,
    runtime::{ui::command::DapThreadAction, DapCommand, RuntimeTaskEvent},
    ui::{self, Picker, Popup, Prompt, PromptEvent, Text},
};
use helix_dap::{StackFrame, Thread, ThreadStates};
use helix_view::editor::Breakpoint;
use tui::text::{Span, Spans};

pub(crate) fn apply_dap_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
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
                    cx.submit_task(crate::runtime::RuntimeTaskEvent::SetBreakpointCondition {
                        path: path.clone(),
                        index,
                        condition: match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        },
                    });
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
                    cx.submit_task(crate::runtime::RuntimeTaskEvent::SetBreakpointLogMessage {
                        path: path.clone(),
                        index,
                        log_message: match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        },
                    });
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
                        policy: helix_view::editor::ThreadSelectPolicy::ReplaceCurrent,
                    },
                    DapThreadAction::Pause => RuntimeTaskEvent::PauseDebugThread { thread_id },
                };
                if let Err(error) = ingress.task(event) {
                    log::warn!("debugger foreground admission failed: {error}");
                }
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
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context, thread: &Thread, _action| {
                    let event = match action {
                        DapThreadAction::Switch => RuntimeTaskEvent::SelectDebugThread {
                            thread_id: thread.id,
                            policy: helix_view::editor::ThreadSelectPolicy::ReplaceCurrent,
                        },
                        DapThreadAction::Pause => RuntimeTaskEvent::PauseDebugThread {
                            thread_id: thread.id,
                        },
                    };
                    cx.submit_task(event);
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
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context, frame: &StackFrame, _action| {
                    cx.submit_task(RuntimeTaskEvent::SelectStackFrame {
                        thread_id,
                        frame_id: frame.id,
                    });
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
        DapCommand::VariablesPopup { scopes } => {
            let scope_style = editor.theme.get("ui.linenr.selected");
            let type_style = editor.theme.get("ui.text");
            let text_style = editor.theme.get("ui.text.focus");
            let mut rows = Vec::new();
            for scope in scopes {
                rows.push(Spans::from(Span::styled(
                    format!("▸ {}", scope.name),
                    scope_style,
                )));
                rows.reserve(scope.variables.len());
                for variable in scope.variables {
                    let mut spans = Vec::with_capacity(5);
                    spans.push(Span::styled(variable.name, text_style));
                    if let Some(ty) = variable.ty {
                        spans.push(Span::raw(": "));
                        spans.push(Span::styled(ty, type_style));
                    }
                    spans.push(Span::raw(" = "));
                    spans.push(Span::styled(variable.value, text_style));
                    rows.push(Spans::from(spans));
                }
            }
            compositor.replace_or_push(
                "dap-variables",
                Popup::new("dap-variables", Text::from(tui::text::Text::from(rows))),
            );
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
