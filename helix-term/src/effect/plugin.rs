use helix_plugin::contract::UiCallbackToken;
use helix_plugin::contract::{adapt, events};
use helix_plugin::{PluginManager, PluginNotification};
use helix_view::Editor;

/// Convert a [`PluginNotification`] (lightweight channel signal) to a full
/// [`events::PluginEvent`] (contract event with enriched editor context).
///
/// Returns `None` when the notification requires editor context that isn't
/// available (e.g. the focused view doesn't exist for selection changes).
pub(crate) fn notification_to_event(
    notification: &PluginNotification,
    editor: &Editor,
) -> Option<events::PluginEvent> {
    match notification {
        PluginNotification::BufferOpen {
            document_id, path, ..
        } => {
            let lang = editor
                .documents
                .get(document_id)
                .and_then(|d| d.language_name().map(|s| s.to_string()));
            Some(events::PluginEvent::DocumentOpened(
                events::DocumentOpenedEvent {
                    document: adapt::document_handle(*document_id),
                    path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    language: lang,
                },
            ))
        }
        PluginNotification::SelectionChange { document_id, .. } => {
            let focused_view_id = editor.tree.focus;
            let view = editor.tree.try_get(focused_view_id)?;
            let doc = editor.documents.get(document_id)?;
            let cursor_char = doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..));
            Some(events::PluginEvent::SelectionChanged(
                events::SelectionChangedEvent {
                    document: adapt::document_handle(*document_id),
                    view: adapt::view_handle(view.id),
                    primary_cursor: adapt::char_to_position(doc.text(), cursor_char),
                },
            ))
        }
        PluginNotification::ModeChange { old_mode, new_mode } => {
            Some(events::PluginEvent::ModeChanged(events::ModeChangedEvent {
                old: adapt::mode_str_to_contract(old_mode),
                new: adapt::mode_str_to_contract(new_mode),
            }))
        }
        PluginNotification::KeyPress { key } => {
            Some(events::PluginEvent::KeyPressed(events::KeyPressedEvent {
                key: key.clone(),
                mode: adapt::mode_to_contract(editor.mode),
            }))
        }
        PluginNotification::LspDiagnostic {
            document_id,
            diagnostic_count,
        } => Some(events::PluginEvent::DiagnosticsUpdated(
            events::DiagnosticsUpdatedEvent {
                document: adapt::document_handle(*document_id),
                count: *diagnostic_count,
            },
        )),
    }
}

pub(crate) fn apply_plugin_ui_callback(
    editor: &mut Editor,
    plugin_manager: std::sync::Arc<PluginManager>,
    callback: UiCallbackToken,
    value: helix_plugin::contract::DynamicValue,
) {
    let Some(callback_id) = helix_plugin::UiCallbackId::new(callback.raw().get()) else {
        editor.set_error("invalid plugin UI callback token");
        return;
    };
    if let Err(err) = plugin_manager.handle_ui_callback(editor, callback_id, value) {
        editor.set_error(err.to_string());
    }
}
