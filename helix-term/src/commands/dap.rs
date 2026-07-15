use super::Context;
use crate::{
    compositor,
    runtime::{
        ingress::DapSessionRequest, ui::command::DapThreadAction, DapCommand, RuntimeTaskEvent,
        UiCommand,
    },
    ui::{self, overlay::overlaid, Picker, Prompt, PromptEvent},
};
use helix_core::syntax::config::{DebugConfigCompletion, DebugTemplate};
use helix_dap::{self as dap, requests::TerminateArguments};

use serde_json::{to_value, Value};

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use anyhow::{anyhow, bail};

use crate::runtime::ui::dap::get_breakpoint_at_current_line;

fn thread_picker(cx: &mut Context, action: DapThreadAction) {
    let debugger = debugger!(cx.editor);

    let future = debugger.threads();
    cx.spawn_ui(Box::pin(async move {
        let json = future.await?;
        let response: dap::requests::ThreadsResponse = serde_json::from_value(json)?;
        let threads = response.threads;
        Ok(UiCommand::Dap(DapCommand::ThreadsPicker {
            threads,
            action,
        }))
    }));
}

// -- DAP

fn dap_callback<T>(
    call: impl Future<Output = helix_dap::Result<serde_json::Value>> + 'static + Send,
    work: helix_runtime::Work,
    ingress: crate::runtime::RuntimeIngress,
    task_event: impl FnOnce(T) -> RuntimeTaskEvent + Send + 'static,
) where
    T: for<'de> serde::Deserialize<'de> + Send + 'static,
{
    let callback = Box::pin(async move {
        let json = call.await?;
        let response = serde_json::from_value(json)?;
        Ok(task_event(response))
    });

    crate::runtime::ingress::spawn_task_event_with_future(work, callback, ingress);
}

pub fn dap_start_impl(
    cx: &mut compositor::Context,
    name: Option<&str>,
    socket: Option<std::net::SocketAddr>,
    params: Option<Vec<std::borrow::Cow<str>>>,
) -> Result<(), anyhow::Error> {
    let (_, doc) = focused_ref!(cx.editor);
    let config = doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
        .cloned()
        .ok_or_else(|| anyhow!("No debug adapter available for language"))?;

    // TODO: avoid refetching all of this... pass a config in
    let template = match name {
        Some(name) => config.templates.iter().find(|t| t.name == name),
        None => config.templates.first(),
    }
    .ok_or_else(|| anyhow!("No debug config with given name"))?;

    let mut args: HashMap<&str, Value> = if let Some(params) = params.as_ref() {
        let preprocessed_params = prepare_dap_params(template, params);
        template
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), map_value(v, &preprocessed_params)))
            .collect()
    } else {
        template
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect()
    };

    args.insert("cwd", to_value(helix_stdx::env::current_working_dir())?);

    let args = to_value(args).unwrap();

    let connection_type = match template.request.as_str() {
        "launch" => dap::ConnectionType::Launch,
        "attach" => dap::ConnectionType::Attach,
        request => bail!("Unsupported request '{}'", request),
    };
    crate::effect::dap::start_client(
        cx.editor,
        cx.ingress.clone(),
        socket,
        config,
        DapSessionRequest {
            connection_type,
            arguments: args,
            parent: None,
        },
    );

    // TODO: either await "initialized" or buffer commands until event is received
    Ok(())
}

fn prepare_dap_params(template: &DebugTemplate, params: &[std::borrow::Cow<str>]) -> Vec<String> {
    params
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let mut param = x.to_string();
            if let Some(DebugConfigCompletion::Advanced(cfg)) = template.completion.get(i) {
                if matches!(cfg.completion.as_deref(), Some("filename" | "directory")) {
                    param = std::fs::canonicalize(x.as_ref())
                        .ok()
                        .and_then(|pb| pb.into_os_string().into_string().ok())
                        .unwrap_or_else(|| x.to_string());
                }
            }
            param
        })
        .collect()
}

fn map_value(value: &Value, params: &[String]) -> Value {
    match value {
        Value::String(string) => {
            let mut string = string.clone();
            for (i, x) in params.iter().enumerate() {
                let pattern = format!("{{{}}}", i);
                string = string.replace(&pattern, x);
            }
            if let Ok(integer) = string.parse::<usize>() {
                to_value(integer).unwrap()
            } else {
                to_value(string).unwrap()
            }
        }
        Value::Array(array) => Value::Array(array.iter().map(|x| map_value(x, params)).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (k.clone(), map_value(v, params)))
                .collect(),
        ),

        _ => value.clone(),
    }
}

pub fn dap_launch(cx: &mut Context) {
    // TODO: Now that we support multiple Clients, we could run multiple debuggers at once but for now keep this as is
    if cx.editor.debug_adapters.get_active_client().is_some()
        || cx.editor.debug_adapters.has_pending_clients()
    {
        cx.editor.set_error("Debugger is already running");
        return;
    }

    let (_, doc) = focused_ref!(cx.editor);

    let config = match doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
    {
        Some(c) => c,
        None => {
            cx.editor
                .set_error("No debug adapter available for language");
            return;
        }
    };

    let templates = config.templates.clone();

    let columns = [ui::PickerColumn::new(
        "template",
        |item: &DebugTemplate, _| item.name.as_str().into(),
    )];

    cx.push_layer(Box::new(overlaid(Picker::new(
        columns,
        0,
        templates,
        (),
        ui::PickerRuntime::new(cx.editor),
        cx.ingress.clone(),
        |cx: &mut crate::compositor::Context, template: &DebugTemplate, _action| {
            if template.completion.is_empty() {
                if let Err(err) = dap_start_impl(cx, Some(&template.name), None, None) {
                    cx.editor.set_error(err.to_string());
                }
            } else {
                let completions = template.completion.clone();
                let name = template.name.clone();
                let callback = Box::pin(async move {
                    Ok(UiCommand::Dap(DapCommand::PushDebugParameterPrompt {
                        completions,
                        config_name: name,
                        params: Vec::new(),
                    }))
                });
                cx.spawn_ui(callback);
            }
        },
    ))));
}

pub fn dap_restart(cx: &mut Context) {
    let debugger = match cx.editor.debug_adapters.get_active_client() {
        Some(debugger) => debugger,
        None => {
            cx.editor.set_error("Debugger is not running");
            return;
        }
    };
    if !debugger
        .capabilities()
        .supports_restart_request
        .unwrap_or(false)
    {
        cx.editor
            .set_error("Debugger does not support session restarts");
        return;
    }
    if debugger.starting_request_args().is_none() {
        cx.editor
            .set_error("No arguments found with which to restart the sessions");
        return;
    }

    dap_callback(
        debugger.restart(),
        cx.editor.work(),
        cx.ingress.clone(),
        |_resp: ()| RuntimeTaskEvent::DapRestarted,
    );
}

pub(crate) fn debug_parameter_prompt(
    completions: Vec<DebugConfigCompletion>,
    config_name: String,
    mut params: Vec<String>,
) -> Prompt {
    let completion = completions.get(params.len()).unwrap();
    let field_type = if let DebugConfigCompletion::Advanced(cfg) = completion {
        cfg.completion.as_deref().unwrap_or("")
    } else {
        ""
    };
    let name = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.name.as_deref().unwrap_or(field_type),
        DebugConfigCompletion::Named(name) => name.as_str(),
    };
    let default_val = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.default.as_deref().unwrap_or(""),
        _ => "",
    }
    .to_owned();

    let completer = match field_type {
        "filename" => ui::completers::filename_with_git_ignore(false),
        "directory" => ui::completers::directory_with_git_ignore(false),
        _ => ui::completers::none,
    };

    Prompt::new(
        format!("{}: ", name).into(),
        None,
        completer,
        move |cx, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }

            let mut value = input.to_owned();
            if value.is_empty() {
                value = default_val.clone();
            }
            params.push(value);

            if params.len() < completions.len() {
                let completions = completions.clone();
                let config_name = config_name.clone();
                let params = params.clone();
                let callback = Box::pin(async move {
                    Ok(UiCommand::Dap(DapCommand::PushDebugParameterPrompt {
                        completions,
                        config_name,
                        params,
                    }))
                });
                cx.spawn_ui(callback);
            } else if let Err(err) = dap_start_impl(
                cx,
                Some(&config_name),
                None,
                Some(params.iter().map(|x| x.into()).collect()),
            ) {
                cx.editor.set_error(err.to_string());
            }
        },
    )
}

pub fn dap_toggle_breakpoint(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let path = match doc.path() {
        Some(path) => path.clone(),
        None => {
            cx.editor
                .set_error("Can't set breakpoint: document has no path");
            return;
        }
    };
    let text = doc.text().slice(..);
    let line = doc.selection(view_id).primary().cursor_line(text);
    dap_toggle_breakpoint_impl(cx, path, line);
}

pub fn dap_toggle_breakpoint_impl(cx: &mut Context, path: PathBuf, line: usize) {
    cx.submit_task(crate::runtime::RuntimeTaskEvent::ToggleBreakpoint { path, line });
}

pub fn dap_continue(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.continue_thread(thread_id);

        dap_callback(
            request,
            cx.editor.work(),
            cx.ingress.clone(),
            |_response: dap::requests::ContinueResponse| {
                RuntimeTaskEvent::ResumeDebuggerApplication
            },
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_pause(cx: &mut Context) {
    thread_picker(cx, DapThreadAction::Pause)
}

pub fn dap_step_in(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_in(thread_id);

        dap_callback(
            request,
            cx.editor.work(),
            cx.ingress.clone(),
            |_response: ()| RuntimeTaskEvent::ResumeDebuggerApplication,
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_step_out(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_out(thread_id);
        dap_callback(
            request,
            cx.editor.work(),
            cx.ingress.clone(),
            |_response: ()| RuntimeTaskEvent::ResumeDebuggerApplication,
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_next(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.next(thread_id);
        dap_callback(
            request,
            cx.editor.work(),
            cx.ingress.clone(),
            |_response: ()| RuntimeTaskEvent::ResumeDebuggerApplication,
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_variables(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_status("Cannot access variables while target is running.");
        return;
    }
    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => {
            cx.editor
                .set_status("Cannot find current stack frame to access variables.");
            return;
        }
    };

    let thread_frame = match debugger.stack_frames.get(&thread_id) {
        Some(thread_frame) => thread_frame,
        None => {
            cx.editor
                .set_error(format!("Failed to get stack frame for thread: {thread_id}"));
            return;
        }
    };
    let stack_frame = match thread_frame.get(frame) {
        Some(stack_frame) => stack_frame,
        None => {
            cx.editor.set_error(format!(
                "Failed to get stack frame for thread {thread_id} and frame {frame}."
            ));
            return;
        }
    };

    let frame_id = stack_frame.id;
    let request = debugger.request_handle();
    cx.editor.set_status("Loading debugger variables...");
    cx.spawn_ui(async move {
        let scopes = request.scopes(frame_id).await?;
        let groups = futures_util::future::try_join_all(scopes.into_iter().map(|scope| {
            let request = request.clone();
            async move {
                let variables = request.variables(scope.variables_reference).await?;
                Ok::<_, helix_dap::Error>(crate::runtime::ui::command::DapScopeVariables {
                    name: scope.name,
                    variables,
                })
            }
        }))
        .await?;
        Ok(UiCommand::Dap(DapCommand::VariablesPopup {
            scopes: groups,
        }))
    });
}

pub fn dap_terminate(cx: &mut Context) {
    let cancelled = cx.editor.debug_adapters.cancel_pending_clients();
    if cx.editor.debug_adapters.get_active_client().is_none() {
        if cancelled > 0 {
            cx.editor.set_status("Debugger startup cancelled");
        } else {
            cx.editor.set_status("Terminating debug session...");
        }
        return;
    }
    cx.editor.set_status("Terminating debug session...");
    let debugger = debugger!(cx.editor);

    if debugger
        .caps
        .as_ref()
        .is_some_and(|c| c.supports_terminate_request.unwrap_or_default())
    {
        let terminate_arguments = Some(TerminateArguments {
            restart: Some(false),
        });

        let request = debugger.terminate(terminate_arguments);
        dap_callback(
            request,
            cx.editor.work(),
            cx.ingress.clone(),
            |_response: ()| RuntimeTaskEvent::UnsetActiveDebugClient,
        );
    } else {
        cx.editor.debug_adapters.unset_active_client();
    }
}

pub fn dap_enable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let filters = match &debugger.capabilities().exception_breakpoint_filters {
        Some(filters) => filters.iter().map(|f| f.filter.clone()).collect(),
        None => return,
    };

    let request = debugger.set_exception_breakpoints(filters);

    dap_callback(
        request,
        cx.editor.work(),
        cx.ingress.clone(),
        |_response: dap::requests::SetExceptionBreakpointsResponse| {
            RuntimeTaskEvent::DapExceptionsConfigured
        },
    )
}

pub fn dap_disable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let request = debugger.set_exception_breakpoints(Vec::new());

    dap_callback(
        request,
        cx.editor.work(),
        cx.ingress.clone(),
        |_response: dap::requests::SetExceptionBreakpointsResponse| {
            RuntimeTaskEvent::DapExceptionsConfigured
        },
    )
}

// TODO: both edit condition and edit log need to be stable: we might get new breakpoints from the debugger which can change offsets
pub fn dap_edit_condition(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match focused_ref!(cx.editor).1.path() {
            Some(path) => path.clone(),
            None => return,
        };
        let initial = breakpoint.condition.clone();
        let callback = Box::pin(async move {
            Ok(UiCommand::Dap(DapCommand::PushBreakpointConditionPrompt {
                path,
                index: pos,
                initial,
            }))
        });
        cx.spawn_ui(callback);
    }
}

pub fn dap_edit_log(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match focused_ref!(cx.editor).1.path() {
            Some(path) => path.clone(),
            None => return,
        };
        let initial = breakpoint.log_message.clone();
        let callback = Box::pin(async move {
            Ok(UiCommand::Dap(DapCommand::PushBreakpointLogPrompt {
                path,
                index: pos,
                initial,
            }))
        });
        cx.spawn_ui(callback);
    }
}

pub fn dap_switch_thread(cx: &mut Context) {
    thread_picker(cx, DapThreadAction::Switch)
}
pub fn dap_switch_stack_frame(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let thread_id = match debugger.thread_id {
        Some(thread_id) => thread_id,
        None => {
            cx.editor.set_error("No thread is currently active");
            return;
        }
    };

    let frames = debugger.stack_frames[&thread_id].clone();

    cx.spawn_ui(async move {
        Ok(UiCommand::Dap(DapCommand::StackFramesPicker {
            thread_id,
            frames,
        }))
    });
}
