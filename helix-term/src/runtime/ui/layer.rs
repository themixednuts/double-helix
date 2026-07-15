use crate::{compositor::Compositor, runtime::ui::command::LayerCommand};
use helix_view::editor::Severity;

pub(crate) fn apply_layer_command(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    cmd: LayerCommand,
) {
    match cmd {
        LayerCommand::PushNotificationHistory => push_notification_history(editor, compositor),
        LayerCommand::InvalidRegexPopup { message } => invalid_regex_popup(compositor, message),
        LayerCommand::DismissPromptIfPresent => {
            if compositor.find::<crate::ui::Prompt>().is_some() {
                compositor.remove_type::<crate::ui::Prompt>();
            }
        }
        LayerCommand::MarkdownPopup { layer_id, markdown } => {
            let contents = crate::ui::Markdown::new(markdown, editor.syn_loader.clone());
            let popup = crate::ui::Popup::new(layer_id, contents).auto_close(true);
            compositor.replace_or_push(layer_id, popup);
        }
        LayerCommand::PushFilePicker { root } => {
            let picker = crate::ui::file_picker(editor, root, ingress);
            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        LayerCommand::PkgManager => match crate::ui::pkg::manager(editor, ingress) {
            Ok(manager) => compositor.push(Box::new(crate::ui::overlay::overlaid(manager))),
            Err(err) => editor.set_error(format!("Failed to open package manager: {err}")),
        },
        LayerCommand::AcpAgentsManager => match crate::ui::pkg::acp_manager(editor, ingress) {
            Ok(manager) => compositor.push(Box::new(crate::ui::overlay::overlaid(manager))),
            Err(err) => editor.set_error(format!("Failed to open ACP agents manager: {err}")),
        },
        LayerCommand::LspCommandPicker { commands } => {
            let columns = [crate::ui::PickerColumn::new(
                "title",
                |(_ls_id, command): &(_, helix_lsp::lsp::Command), _| command.title.as_str().into(),
            )];
            let picker = crate::ui::Picker::new(
                columns,
                0,
                commands,
                (),
                crate::ui::PickerRuntime::new(editor),
                ingress,
                move |cx, (ls_id, command), _action| {
                    cx.submit_task(crate::runtime::RuntimeTaskEvent::ExecuteLspCommand {
                        command: command.clone(),
                        server_id: *ls_id,
                    });
                },
            );
            compositor.push(Box::new(crate::ui::overlay::overlaid(picker)));
        }
        LayerCommand::ShellRunOutput { output } => {
            if !output.trim().is_empty() {
                let contents = crate::ui::Markdown::new(
                    format!("```sh\n{}\n```", output.trim_end()),
                    editor.syn_loader.clone(),
                );
                let popup = crate::ui::Popup::new("shell", contents).position(Some(
                    helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                ));
                compositor.replace_or_push("shell", popup);
            }
            editor.set_status("Command run");
        }
    }
}

fn push_notification_history(editor: &mut helix_view::Editor, compositor: &mut Compositor) {
    let history = editor.get_notification_history();

    if history.is_empty() {
        editor.set_status("No notifications in history");
        return;
    }

    let mut content = String::new();
    content.push_str("Notification History:\n\n");

    for (i, notification) in history.iter().enumerate().rev().take(50) {
        let severity_icon = match notification.severity {
            Severity::Error => "❌",
            Severity::Warning => "⚠️",
            Severity::Info => "ℹ️",
            Severity::Hint => "💡",
        };

        let timestamp = notification.timestamp.elapsed().as_secs();
        let time_str = if timestamp < 60 {
            format!("{}s ago", timestamp)
        } else if timestamp < 3600 {
            format!("{}m ago", timestamp / 60)
        } else {
            format!("{}h ago", timestamp / 3600)
        };

        content.push_str(&format!(
            "{:2}. {} {} ({})\n    {}\n\n",
            history.len() - i,
            severity_icon,
            time_str,
            if notification.dismissed {
                "dismissed"
            } else {
                "active"
            },
            notification.message
        ));
    }

    let popup = crate::ui::Popup::new("notification-history", crate::ui::Text::new(content))
        .auto_close(true);
    compositor.push(Box::new(popup));
}

fn invalid_regex_popup(compositor: &mut Compositor, message: String) {
    use helix_core::Position;

    let contents = crate::ui::Text::new(message);
    let size = compositor.size();
    let popup = crate::ui::Popup::new("invalid-regex", contents)
        .position(Some(Position::new(size.height as usize - 2, 0)))
        .auto_close(true);
    compositor.replace_or_push("invalid-regex", popup);
}
