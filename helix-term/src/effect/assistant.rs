use helix_runtime::{send_blocking, Sender as IngressSender};

use crate::runtime::{
    ingress::RuntimeEvent,
    send_task_event_with,
    RuntimeTaskEvent,
    UiCommand,
};
use helix_view::{editor::Action, Editor};

pub(crate) fn apply_restore_assistant_history_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    record: helix_view::assistant::history::Record,
    activate: bool,
    open_panel: bool,
) {
    let effects = editor.load_assistant_thread(record, activate);
    editor.apply_assistant_effects(effects);
    editor.persist_assistant_layout();

    if open_panel {
        send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Assistant(
                crate::runtime::ui::command::AssistantCommand::OpenPanel,
            )),
        );
    }
}

pub(crate) fn apply_activate_assistant_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    thread: helix_view::assistant::thread::Id,
    open_panel: bool,
) {
    let effects = editor.activate_assistant_thread(thread);
    editor.apply_assistant_effects(effects);
    editor.persist_assistant_layout();

    if open_panel {
        send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Assistant(
                crate::runtime::ui::command::AssistantCommand::OpenPanel,
            )),
        );
    }

    editor.set_status("Activated assistant history thread");
}

pub(crate) fn apply_detach_assistant_context(
    editor: &mut Editor,
    item: helix_view::assistant::context::Id,
) {
    let effects = match editor.detach_active_assistant_context(item) {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.set_status("Detached context");
}

pub(crate) fn apply_remove_assistant_panel(editor: &mut Editor) {
    let assistant_panels: Vec<_> = editor
        .model
        .panels
        .iter()
        .filter_map(|(id, entry)| {
            entry
                .content
                .as_any()
                .is::<helix_view::model::AssistantModel>()
                .then_some(id)
        })
        .collect();

    for id in assistant_panels {
        editor.model.remove_panel(id);
    }
}

pub(crate) fn apply_connect_assistant_backend(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    command: String,
    args: Vec<String>,
    open_panel: bool,
) {
    let (_, effects) = match editor.connect_assistant_backend(command.clone(), args) {
        Ok(result) => result,
        Err(err) => {
            editor.set_error(format!("Agent failed: {err}"));
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.persist_assistant_layout();

    if open_panel {
        send_blocking(
            &ingress,
            RuntimeEvent::Ui(UiCommand::Assistant(
                crate::runtime::ui::command::AssistantCommand::OpenPanel,
            )),
        );
    }

    editor.set_status(format!("Connecting assistant backend: {command}..."));
}

pub(crate) fn apply_cycle_assistant_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    delta: isize,
) {
    let effects = match editor.cycle_active_assistant_thread(delta) {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.persist_assistant_layout();
    send_blocking(
        &ingress,
        RuntimeEvent::Ui(UiCommand::Assistant(
            crate::runtime::ui::command::AssistantCommand::OpenPanel,
        )),
    );
}

pub(crate) fn apply_close_active_assistant_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
) {
    let effects = match editor.close_active_assistant_thread() {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.persist_assistant_layout();
    send_blocking(
        &ingress,
        RuntimeEvent::Ui(UiCommand::Assistant(
            crate::runtime::ui::command::AssistantCommand::OpenPanel,
        )),
    );
}

pub(crate) fn apply_new_assistant_thread_from_active_backend(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
) {
    let effects = match editor.new_assistant_thread_from_active_backend() {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.request_redraw();
    editor.persist_assistant_layout();
    send_blocking(
        &ingress,
        RuntimeEvent::Ui(UiCommand::Assistant(
            crate::runtime::ui::command::AssistantCommand::OpenPanel,
        )),
    );
}

pub(crate) fn apply_toggle_active_assistant_follow(editor: &mut Editor) {
    let (status, effects) = match editor.toggle_active_assistant_follow() {
        Ok(value) => value,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.set_status(status);
}

pub(crate) fn apply_attach_assistant_context(
    editor: &mut Editor,
    item: helix_view::assistant::context::Kind,
    status: &'static str,
) {
    let effects = match editor.attach_active_assistant_context(item) {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.set_status(status);
}

pub(crate) fn apply_submit_assistant_prompt(editor: &mut Editor, text: String) {
    let effects = match editor.submit_active_assistant_prompt(text) {
        Ok(effects) => effects,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    editor.apply_assistant_effects(effects);
    editor.set_status("Sending prompt to agent...");
}

pub(crate) fn apply_cancel_active_assistant_thread(editor: &mut Editor) {
    if let Some(effects) = editor.cancel_active_assistant_thread() {
        editor.apply_assistant_effects(effects);
    }
    editor.set_status("Cancelling assistant...");
}

pub(crate) fn apply_open_selected_assistant_entry_scratch(editor: &mut Editor) {
    let Some(effects) = editor.open_selected_assistant_entry_scratch(Action::Replace) else {
        editor.set_error("No assistant entry selected");
        return;
    };
    editor.apply_assistant_effects(effects);
}

pub(crate) fn apply_open_selected_assistant_turn_changes(editor: &mut Editor) {
    if !editor.open_selected_assistant_turn_changes() {
        editor.set_error("Selected entry has no turn changes");
    }
}

pub(crate) fn apply_open_active_assistant_thread_changes(editor: &mut Editor) {
    if !editor.open_active_assistant_thread_changes() {
        editor.set_error("Active assistant thread has no changes");
    }
}

pub(crate) fn apply_assistant_history_entries(
    editor: &mut Editor,
    scope: helix_view::assistant::thread::Scope,
    entries: Vec<helix_view::assistant::history::Stub>,
) {
    let outcome = editor.apply_assistant_update(helix_view::assistant::backend::Update::History {
        scope,
        entries,
        next: None,
    });
    editor.apply_assistant_effects(outcome.effects);
}

pub(crate) fn request_load_assistant_history_thread(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    thread: helix_view::assistant::thread::Id,
    activate: bool,
    open_panel: bool,
) {
    let Some(history) = editor.assistant_history_backend() else {
        editor.set_error("Assistant history backend missing");
        return;
    };

    editor.runtime().work().clone().spawn(async move {
        match history.load(thread).await {
            Ok(Some(record)) => {
                send_task_event_with(
                    RuntimeTaskEvent::RestoreAssistantHistoryThread {
                        record,
                        activate,
                        open_panel,
                    },
                    ingress,
                )
                .await;
            }
            Ok(None) => {
                send_task_event_with(
                    RuntimeTaskEvent::SetEditorError {
                        message: "Assistant history record missing".to_string(),
                    },
                    ingress,
                )
                .await;
            }
            Err(error) => {
                send_task_event_with(
                    RuntimeTaskEvent::SetEditorError {
                        message: error.to_string(),
                    },
                    ingress,
                )
                .await;
            }
        }
    }).detach();
}

pub(crate) fn request_bootstrap_assistant_history(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    scope: helix_view::assistant::thread::Scope,
) {
    let Some(history) = editor.assistant_history_backend() else {
        return;
    };

    editor.runtime().work().clone().spawn(async move {
        if let Ok(entries) = history.load_scope(&scope).await {
            send_task_event_with(
                RuntimeTaskEvent::ApplyAssistantHistoryEntries {
                    scope: scope.clone(),
                    entries,
                },
                ingress.clone(),
            )
            .await;
        }

        if let Ok(Some(layout)) = helix_view::assistant::layout::load_layout(&scope).await {
            for thread in layout.open {
                send_task_event_with(
                    RuntimeTaskEvent::LoadAssistantHistoryThread {
                        thread,
                        activate: layout.active == Some(thread),
                        open_panel: false,
                    },
                    ingress.clone(),
                )
                .await;
            }
        }
    }).detach();
}
