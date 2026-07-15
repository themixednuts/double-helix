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
    editor: &mut helix_view::Editor,
    foreground: &crate::runtime::ForegroundEvents,
    connection: crate::runtime::AssistantBackendConnection,
) {
    if let Err(error) = foreground.task(RuntimeTaskEvent::ConnectAssistantBackend(Box::new(
        connection,
    ))) {
        editor.set_error(error.to_string());
    }
}

fn push_acp_agents_manager(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
) {
    editor.notify_info("No assistant agents installed; opening ACP Agents");
    match crate::ui::pkg::acp_manager(editor, ingress) {
        Ok(manager) => compositor.push(Box::new(crate::ui::overlay::overlaid(manager))),
        Err(err) => editor.set_error(format!("Failed to open ACP agents manager: {err}")),
    }
}

fn push_configured_agents_picker(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    agents: Vec<helix_view::editor::AgentConfig>,
) {
    let columns = [
        crate::ui::PickerColumn::new("name", |item: &helix_view::editor::AgentConfig, _: &()| {
            item.name.as_str().into()
        }),
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

    let picker = crate::ui::Picker::new(
        columns,
        0,
        agents,
        (),
        crate::ui::PickerRuntime::new(editor),
        ingress,
        move |cx: &mut crate::compositor::Context,
              item: &helix_view::editor::AgentConfig,
              _action| {
            cx.editor
                .notify_info(format!("Connecting assistant agent {}", item.name));
            match crate::runtime::AssistantBackendConnection::from_agent(
                item.clone(),
                None,
                helix_view::editor::PanelBehavior::Open,
            ) {
                Ok(connection) => connect_assistant_backend(cx.editor, &cx.foreground, connection),
                Err(err) => cx.editor.set_error(format!(
                    "Assistant agent {} has an invalid identity: {err}",
                    item.name
                )),
            }
        },
    );

    compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
}

fn push_agent_picker_or_manager(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
) {
    let agents = editor.assistant_agents();
    if agents.is_empty() {
        push_acp_agents_manager(editor, compositor, ingress);
    } else {
        push_configured_agents_picker(editor, compositor, ingress, agents);
    }
}

fn profile_agent(
    editor: &helix_view::Editor,
    profile: &helix_view::assistant::profile::Definition,
) -> Option<helix_view::editor::AgentConfig> {
    if let Some(agent_name) = &profile.agent {
        return editor
            .assistant_agents()
            .into_iter()
            .find(|agent| agent.name == *agent_name);
    }
    editor
        .active_assistant_backend_id()
        .and_then(|backend| editor.assistant_agent(&backend))
        .or_else(|| editor.assistant_agents().into_iter().next())
}

fn connect_profile(
    editor: &mut helix_view::Editor,
    foreground: &crate::runtime::ForegroundEvents,
    profile: &helix_view::assistant::profile::Definition,
) {
    let Some(mut agent) = profile_agent(editor, profile) else {
        editor.set_error("No assistant agent available for profile");
        return;
    };
    let defaults = helix_view::assistant::profile::assemble_session_defaults(
        Some(profile),
        &agent.mcp_servers,
    );
    agent.mcp_servers = defaults.mcp_servers;
    let connection = match crate::runtime::AssistantBackendConnection::from_agent(
        agent,
        defaults.profile,
        helix_view::editor::PanelBehavior::Open,
    ) {
        Ok(connection) => connection,
        Err(err) => {
            editor.set_error(format!(
                "Assistant profile agent has an invalid identity: {err}"
            ));
            return;
        }
    };
    connect_assistant_backend(editor, foreground, connection);
}

#[derive(Debug, Clone)]
struct PermissionPickerItem {
    id: helix_view::assistant::permission::ChoiceId,
    title: String,
    description: String,
    default: bool,
}

pub(crate) fn assistant_history_delete_remote(
    origin: Option<&helix_view::assistant::thread::Origin>,
    caps: Option<&helix_acp::AgentCaps>,
) -> bool {
    matches!(
        origin,
        Some(helix_view::assistant::thread::Origin::Backend { .. })
    ) && caps.is_some_and(|caps| caps.delete_session)
}

pub(crate) fn assistant_history_should_fetch_more(
    selected_index: usize,
    len: usize,
    next: Option<&helix_view::assistant::history::Cursor>,
) -> bool {
    next.is_some() && len != 0 && selected_index + 1 >= len
}

pub(crate) fn apply_assistant_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
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
            } else {
                push_agent_picker_or_manager(editor, compositor, ingress);
            }
        }
        AssistantCommand::ClosePanel => {
            use crate::ui::assistant::ID as ASSISTANT_PANEL_ID;

            if let Err(error) = foreground.task(RuntimeTaskEvent::RemoveAssistantPanel) {
                editor.set_error(error.to_string());
            }
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
            } else {
                push_agent_picker_or_manager(editor, compositor, ingress);
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
            } else {
                push_agent_picker_or_manager(editor, compositor, ingress);
            }
        }
        AssistantCommand::OpenPanel => {
            use crate::ui::assistant::{AssistantPanel, ID as ASSISTANT_PANEL_ID};
            compositor.replace_or_push(ASSISTANT_PANEL_ID, AssistantPanel::new());
        }
        AssistantCommand::ShowPermissionRequest { thread, request } => {
            let request_id = request.id().clone();
            let default = request.default().cloned();
            let items = request
                .choices()
                .iter()
                .map(|choice| PermissionPickerItem {
                    id: choice.id.clone(),
                    title: choice.label.clone(),
                    description: match &choice.kind {
                        helix_view::assistant::permission::Kind::Custom(kind) => kind.to_string(),
                        _ => request.body().to_string(),
                    },
                    default: default.as_ref() == Some(&choice.id),
                })
                .collect::<Vec<_>>();
            let columns = [
                crate::ui::PickerColumn::new("choice", |item: &PermissionPickerItem, _: &()| {
                    if item.default {
                        format!("{} default", item.title).into()
                    } else {
                        item.title.as_str().into()
                    }
                }),
                crate::ui::PickerColumn::new("details", |item: &PermissionPickerItem, _: &()| {
                    item.description.as_str().into()
                }),
            ];
            let picker = crate::ui::Picker::new(
                columns,
                0,
                items,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context, item: &PermissionPickerItem, _action| {
                    if let Err(error) = cx.foreground.assistant_permission_resolved(
                        thread,
                        request_id.clone(),
                        helix_view::assistant::permission::Decision::Choose(item.id.clone()),
                    ) {
                        cx.editor.set_error(error.to_string());
                    }
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushHistoryPicker {
            scope,
            entries,
            next,
        } => {
            if entries.is_empty() {
                editor.set_status("No assistant history for this scope");
                return;
            }
            let caps = editor.active_assistant_caps().cloned();
            let pending_delete = std::sync::Arc::new(std::sync::Mutex::new(
                None::<helix_view::assistant::thread::Id>,
            ));
            let entries_len = entries.len();
            let page_next = next.clone();
            let requested_next = std::sync::Arc::new(std::sync::Mutex::new(
                None::<helix_view::assistant::history::Cursor>,
            ));

            let columns = [
                crate::ui::PickerColumn::new(
                    "rating",
                    |item: &helix_view::assistant::history::Stub, _: &()| match item.feedback.rating
                    {
                        helix_view::assistant::thread::Rating::None => "".into(),
                        helix_view::assistant::thread::Rating::Up => "up".into(),
                        helix_view::assistant::thread::Rating::Down => "down".into(),
                    },
                ),
                crate::ui::PickerColumn::new(
                    "note",
                    |item: &helix_view::assistant::history::Stub, _: &()| {
                        if item.feedback.note.is_some() {
                            "note".into()
                        } else {
                            "".into()
                        }
                    },
                ),
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

            let mut delete_handlers = crate::ui::picker::PickerKeyHandlers::new();
            {
                let pending_delete = pending_delete.clone();
                let caps = caps.clone();
                delete_handlers.insert(
                    helix_view::input::KeyEvent {
                        code: helix_view::input::KeyCode::Char('d'),
                        modifiers: helix_view::input::KeyModifiers::NONE,
                    },
                    Box::new(
                        move |cx: &mut crate::compositor::Context,
                              item: &helix_view::assistant::history::Stub,
                              _data,
                              _cursor| {
                            let mut pending = pending_delete.lock().expect("delete state");
                            if *pending == Some(item.id) {
                                let delete_remote = assistant_history_delete_remote(
                                    item.origin.as_ref(),
                                    caps.as_ref(),
                                );
                                cx.submit_task(RuntimeTaskEvent::DeleteAssistantHistoryThread {
                                    thread: item.id,
                                    delete_remote,
                                });
                                *pending = None;
                            } else {
                                *pending = Some(item.id);
                                let title = item.title.as_deref().map_or_else(
                                    || format!("session {}", item.id),
                                    ToString::to_string,
                                );
                                cx.editor
                                    .notify_warning(format!("Press d again to delete {title}"));
                            }
                        },
                    ),
                );
            }

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
                        cx.submit_task(RuntimeTaskEvent::ActivateAssistantThread {
                            thread: item.id,
                            panel: helix_view::editor::PanelBehavior::Open,
                        });
                        return;
                    }

                    cx.submit_task(RuntimeTaskEvent::LoadAssistantHistoryThread {
                        thread: item.id,
                        activation: helix_view::editor::Activation::Activate,
                        panel: helix_view::editor::PanelBehavior::Open,
                    });
                },
            )
            .with_key_handlers(delete_handlers)
            .with_selection_changed_handler(Box::new(
                move |cx: &mut crate::compositor::Context,
                      _item: Option<&helix_view::assistant::history::Stub>,
                      _data,
                      cursor| {
                    if !assistant_history_should_fetch_more(
                        cursor as usize,
                        entries_len,
                        page_next.as_ref(),
                    ) {
                        return;
                    }
                    let Some(cursor) = page_next.clone() else {
                        return;
                    };
                    let mut requested = requested_next.lock().expect("pagination state");
                    if requested.as_ref() == Some(&cursor) {
                        return;
                    }
                    *requested = Some(cursor.clone());
                    cx.submit_task(RuntimeTaskEvent::FetchAssistantHistoryPage {
                        scope: scope.clone(),
                        cursor: Some(cursor),
                    });
                    cx.editor.set_status("Loading more assistant sessions...");
                },
            ));

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushProfilePicker { profiles } => {
            if profiles.is_empty() {
                editor.set_status("No assistant profiles configured");
                return;
            }

            let columns = [
                crate::ui::PickerColumn::new(
                    "profile",
                    |item: &helix_view::assistant::profile::Definition, _: &()| {
                        item.name.as_str().into()
                    },
                ),
                crate::ui::PickerColumn::new(
                    "agent",
                    |item: &helix_view::assistant::profile::Definition, _: &()| {
                        item.agent.as_deref().unwrap_or("current").into()
                    },
                ),
            ];

            let picker = crate::ui::Picker::new(
                columns,
                0,
                profiles,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress.clone(),
                move |cx: &mut crate::compositor::Context,
                      item: &helix_view::assistant::profile::Definition,
                      _action| {
                    if cx.editor.active_assistant_mode_config().is_some() {
                        let effects = match cx.editor.set_active_assistant_profile(item.defaults())
                        {
                            Ok(effects) => effects,
                            Err(err) => {
                                cx.editor.set_error(err.to_string());
                                return;
                            }
                        };
                        cx.editor.apply_assistant_effects(effects);
                        cx.editor
                            .set_status(format!("Assistant profile: {}", item.name));
                    } else {
                        connect_profile(cx.editor, &cx.foreground, item);
                    }
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
                    cx.submit_task(RuntimeTaskEvent::DetachAssistantContext {
                        item: item.id.clone(),
                    });
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushModeConfigPicker { thread, items } => {
            use crate::runtime::ui::command::ModeConfigPickerItem;

            if items.is_empty() {
                editor.set_status("No assistant mode or config options");
                return;
            }

            let columns = [
                crate::ui::PickerColumn::new("kind", |item: &ModeConfigPickerItem, _: &()| {
                    match item {
                        ModeConfigPickerItem::Profile { .. } => "profile".into(),
                        ModeConfigPickerItem::Mode { .. } => "mode".into(),
                        ModeConfigPickerItem::Config { category, .. } => {
                            category.as_deref().unwrap_or("config").into()
                        }
                    }
                }),
                crate::ui::PickerColumn::new("name", |item: &ModeConfigPickerItem, _: &()| {
                    match item {
                        ModeConfigPickerItem::Profile { name, .. } => name.as_str().into(),
                        ModeConfigPickerItem::Mode { name, .. }
                        | ModeConfigPickerItem::Config { name, .. } => name.as_str().into(),
                    }
                }),
                crate::ui::PickerColumn::new("value", |item: &ModeConfigPickerItem, _: &()| {
                    match item {
                        ModeConfigPickerItem::Profile { agent, current, .. } => {
                            let label = agent.as_deref().unwrap_or("current agent");
                            if *current {
                                format!("{label} current").into()
                            } else {
                                label.into()
                            }
                        }
                        ModeConfigPickerItem::Mode { current, .. } => {
                            if *current { "current" } else { "" }.into()
                        }
                        ModeConfigPickerItem::Config {
                            value_label,
                            current,
                            ..
                        } => {
                            if *current {
                                format!("{value_label} current").into()
                            } else {
                                value_label.as_str().into()
                            }
                        }
                    }
                }),
            ];

            let picker = crate::ui::Picker::new(
                columns,
                0,
                items,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress,
                move |cx: &mut crate::compositor::Context, item: &ModeConfigPickerItem, _action| {
                    let effects = match item {
                        ModeConfigPickerItem::Profile { profile, .. } => {
                            cx.editor.set_assistant_profile(thread, profile.clone())
                        }
                        ModeConfigPickerItem::Mode { id, .. } => {
                            cx.editor.set_assistant_mode(thread, id.clone())
                        }
                        ModeConfigPickerItem::Config { option, value, .. } => cx
                            .editor
                            .set_assistant_config(thread, option.clone(), value.clone()),
                    };
                    cx.editor.apply_assistant_effects(effects);
                },
            );

            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        AssistantCommand::PushConfiguredAgentsPicker { agents } => {
            push_configured_agents_picker(editor, compositor, ingress, agents);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str) -> helix_view::editor::AgentConfig {
        helix_view::editor::AgentConfig {
            id: Some(id.to_owned()),
            name: name.to_owned(),
            command: "node".to_owned(),
            args: vec!["agent.js".to_owned()],
            env: Default::default(),
            mcp_servers: Vec::new(),
            theme: None,
        }
    }

    #[test]
    fn selected_agent_connection_keeps_identity_and_display_name() {
        let first = crate::runtime::AssistantBackendConnection::from_agent(
            agent("first", "First Agent"),
            None,
            helix_view::editor::PanelBehavior::Open,
        )
        .unwrap();
        let second = crate::runtime::AssistantBackendConnection::from_agent(
            agent("second", "Second Agent"),
            None,
            helix_view::editor::PanelBehavior::Open,
        )
        .unwrap();

        assert_eq!(first.launch.backend_id.as_str(), "acp:agent:first");
        assert_eq!(first.launch.display_name, "First Agent");
        assert_eq!(second.launch.backend_id.as_str(), "acp:agent:second");
        assert_eq!(second.launch.display_name, "Second Agent");
        assert_eq!(first.launch.command, second.launch.command);
        assert_ne!(first.launch.backend_id, second.launch.backend_id);
    }

    #[test]
    fn assistant_history_delete_remote_requires_backend_origin_and_cap() {
        let origin = helix_view::assistant::thread::Origin::Backend {
            backend: helix_view::assistant::backend::Id::new("backend"),
            remote: helix_view::assistant::backend::Remote::new("remote"),
        };
        let caps = helix_acp::AgentCaps {
            delete_session: true,
            ..Default::default()
        };

        assert!(assistant_history_delete_remote(Some(&origin), Some(&caps)));
        assert!(!assistant_history_delete_remote(Some(&origin), None));
        assert!(!assistant_history_delete_remote(
            Some(&helix_view::assistant::thread::Origin::Local),
            Some(&caps)
        ));
    }

    #[test]
    fn assistant_history_fetch_more_triggers_at_page_end_only_with_cursor() {
        let cursor = helix_view::assistant::history::Cursor::new("next");

        assert!(assistant_history_should_fetch_more(2, 3, Some(&cursor)));
        assert!(!assistant_history_should_fetch_more(1, 3, Some(&cursor)));
        assert!(!assistant_history_should_fetch_more(2, 3, None));
        assert!(!assistant_history_should_fetch_more(0, 0, Some(&cursor)));
    }
}
