use crate::{
    compositor::Compositor,
    runtime::{AssistantCommand, RuntimeTaskEvent},
};

fn context_label(item: &helix_view::assistant::context::Item) -> String {
    match &item.kind {
        helix_view::assistant::context::Kind::Selection(selection) => selection
            .label
            .clone()
            .unwrap_or_else(|| selection.path.display().to_string()),
        helix_view::assistant::context::Kind::Symbol(symbol) => symbol.name.clone(),
        helix_view::assistant::context::Kind::File(file) => file.path.display().to_string(),
        helix_view::assistant::context::Kind::Diagnostics(diagnostics) => {
            format!("diagnostics: {}", diagnostics.path.display())
        }
        helix_view::assistant::context::Kind::Diff(diff) => {
            format!("diff: {}", diff.path.display())
        }
    }
}

fn connect_assistant_backend(
    ingress: &crate::runtime::RuntimeIngress,
    command: String,
    args: Vec<String>,
) {
    ingress.task(RuntimeTaskEvent::ConnectAssistantBackend {
        command,
        args,
        panel: helix_view::editor::PanelBehavior::Open,
    });
}

pub(crate) fn apply_assistant_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    cmd: AssistantCommand,
) {
    match cmd {
        AssistantCommand::TogglePanelFocus => {
            use crate::ui::assistant::{AssistantPanel, ID as ASSISTANT_PANEL_ID};
            use helix_view::traits::Focusable;

            if let Some(panel) = compositor.find_id::<AssistantPanel>(ASSISTANT_PANEL_ID) {
                panel.toggle_focus();
            } else if editor.has_assistant_threads() {
                compositor.push(Box::new(AssistantPanel::new()));
            } else if let Some(agent) = editor.config().agents.first().cloned() {
                connect_assistant_backend(&ingress, agent.command, agent.args);
            } else {
                compositor.push(Box::new(AssistantPanel::new()));
            }
        }
        AssistantCommand::ClosePanel => {
            use crate::ui::assistant::ID as ASSISTANT_PANEL_ID;

            ingress.task(RuntimeTaskEvent::RemoveAssistantPanel);
            compositor.remove(ASSISTANT_PANEL_ID);
        }
        AssistantCommand::FocusPanelInput => {
            use crate::ui::assistant::{AssistantPanel, ID as ASSISTANT_PANEL_ID};

            if let Some(panel) = compositor.find_id::<AssistantPanel>(ASSISTANT_PANEL_ID) {
                panel.activate_input(editor);
            } else if editor.has_assistant_threads() {
                let mut panel = AssistantPanel::new();
                panel.activate_input(editor);
                compositor.push(Box::new(panel));
            } else if let Some(agent) = editor.config().agents.first().cloned() {
                connect_assistant_backend(&ingress, agent.command, agent.args);
            } else {
                let mut panel = AssistantPanel::new();
                panel.activate_input(editor);
                compositor.push(Box::new(panel));
            }
        }
        AssistantCommand::FocusPanelEntries => {
            use crate::ui::assistant::{AssistantPanel, ID as ASSISTANT_PANEL_ID};

            if let Some(panel) = compositor.find_id::<AssistantPanel>(ASSISTANT_PANEL_ID) {
                panel.focus_messages(editor);
                if panel.selected_message(editor).is_none() {
                    panel.select_last_message(editor);
                }
            } else if editor.has_assistant_threads() {
                let mut panel = AssistantPanel::new();
                panel.focus_messages(editor);
                compositor.push(Box::new(panel));
            } else if let Some(agent) = editor.config().agents.first().cloned() {
                connect_assistant_backend(&ingress, agent.command, agent.args);
            } else {
                let mut panel = AssistantPanel::new();
                panel.focus_messages(editor);
                compositor.push(Box::new(panel));
            }
        }
        AssistantCommand::OpenPanel => {
            use crate::ui::assistant::{AssistantPanel, ID as ASSISTANT_PANEL_ID};
            compositor.replace_or_push(ASSISTANT_PANEL_ID, AssistantPanel::new());
        }
        AssistantCommand::ShowPermissionRequest { thread, request } => {
            use crate::ui::assistant::{
                PermissionChoice, PermissionPopup, PermissionResponse, PERMISSION_ID,
            };

            let (tx, rx) = tokio::sync::oneshot::channel::<PermissionResponse>();
            let choices = request
                .choices()
                .iter()
                .map(|choice| PermissionChoice {
                    id: choice.id.as_str().to_string(),
                    title: choice.label.clone(),
                    description: match &choice.kind {
                        helix_view::assistant::permission::Kind::Custom(kind) => {
                            Some(kind.to_string())
                        }
                        _ => None,
                    },
                    key: match &choice.kind {
                        helix_view::assistant::permission::Kind::AllowOnce => Some('y'),
                        helix_view::assistant::permission::Kind::AllowAlways => Some('a'),
                        helix_view::assistant::permission::Kind::RejectOnce => Some('n'),
                        helix_view::assistant::permission::Kind::RejectAlways => Some('r'),
                        helix_view::assistant::permission::Kind::Custom(_) => choice
                            .label
                            .chars()
                            .find(|ch| ch.is_ascii_alphanumeric())
                            .map(|ch| ch.to_ascii_lowercase()),
                    },
                })
                .collect();

            let popup = PermissionPopup::new(
                request.title().to_string(),
                Some(request.body().to_string()),
                choices,
                request.default().map(|id| id.as_str().to_string()),
                tx,
            );
            compositor.replace_or_push(PERMISSION_ID, popup);

            let request_id = request.id().clone();
            editor
                .work()
                .spawn(async move {
                    let decision = match rx.await {
                        Ok(PermissionResponse::Selected(id)) => {
                            helix_view::assistant::permission::Decision::Choose(
                                helix_view::assistant::permission::ChoiceId::new(id),
                            )
                        }
                        _ => helix_view::assistant::permission::Decision::Dismiss,
                    };
                    ingress
                        .send_assistant_permission_resolved(thread, request_id, decision)
                        .await;
                })
                .detach();
        }
        AssistantCommand::PushHistoryPicker { entries } => {
            if entries.is_empty() {
                editor.set_status("No assistant history for this scope");
                return;
            }

            let columns = [
                crate::ui::PickerColumn::new(
                    "title",
                    |item: &helix_view::assistant::history::Stub, _: &()| {
                        item.title
                            .clone()
                            .unwrap_or_else(|| format!("Thread {}", item.id))
                            .into()
                    },
                ),
                crate::ui::PickerColumn::new(
                    "run",
                    |item: &helix_view::assistant::history::Stub, _: &()| match &item.run {
                        helix_view::assistant::thread::Run::Idle => "idle".into(),
                        helix_view::assistant::thread::Run::Running => "running".into(),
                        helix_view::assistant::thread::Run::Waiting => "waiting".into(),
                        helix_view::assistant::thread::Run::Failed { message } => {
                            format!("failed: {message}").into()
                        }
                    },
                ),
                crate::ui::PickerColumn::new(
                    "scope",
                    |item: &helix_view::assistant::history::Stub, _: &()| {
                        item.scope.cwd.display().to_string().into()
                    },
                ),
            ];

            let picker = crate::ui::Picker::new(
                columns,
                0,
                entries,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context,
                      item: &helix_view::assistant::history::Stub,
                      _action| {
                    if cx.editor.assistant_thread_exists(item.id) {
                        cx.ingress.task(RuntimeTaskEvent::ActivateAssistantThread {
                            thread: item.id,
                            panel: helix_view::editor::PanelBehavior::Open,
                        });
                        return;
                    }

                    cx.ingress
                        .task(RuntimeTaskEvent::LoadAssistantHistoryThread {
                            thread: item.id,
                            activation: helix_view::editor::Activation::Activate,
                            panel: helix_view::editor::PanelBehavior::Open,
                        });
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushDetachContextPicker { items } => {
            let picker = crate::ui::Picker::new(
                [crate::ui::PickerColumn::new(
                    "context",
                    |item: &helix_view::assistant::context::Item, _: &()| {
                        context_label(item).into()
                    },
                )],
                0,
                items,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress,
                move |cx: &mut crate::compositor::Context,
                      item: &helix_view::assistant::context::Item,
                      _action| {
                    cx.ingress.task(RuntimeTaskEvent::DetachAssistantContext {
                        item: item.id.clone(),
                    });
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushConfiguredAgentsPicker { agents } => {
            let columns = [
                crate::ui::PickerColumn::new(
                    "name",
                    |item: &helix_view::editor::AgentConfig, _: &()| item.name.as_str().into(),
                ),
                crate::ui::PickerColumn::new(
                    "command",
                    |item: &helix_view::editor::AgentConfig, _: &()| {
                        let mut cmd = item.command.clone();
                        if !item.args.is_empty() {
                            cmd.push(' ');
                            cmd.push_str(&item.args.join(" "));
                        }
                        cmd.into()
                    },
                ),
            ];

            let agents_for_callback = agents.clone();
            let picker = crate::ui::Picker::new(
                columns,
                0,
                agents,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context,
                      item: &helix_view::editor::AgentConfig,
                      _action| {
                    let idx = agents_for_callback
                        .iter()
                        .position(|a| a.name == item.name && a.command == item.command)
                        .or_else(|| {
                            agents_for_callback
                                .iter()
                                .position(|a| a.command == item.command)
                        });
                    let _ = idx;
                    connect_assistant_backend(&cx.ingress, item.command.clone(), item.args.clone());
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
    }
}
