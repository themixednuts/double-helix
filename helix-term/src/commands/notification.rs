use crate::compositor::Context;
use crate::runtime::{send_ui_command_with, LayerCommand, UiCommand};

/// Show notification history
pub fn show_notification_history(cx: &mut Context) {
    if cx.editor.get_notification_history().is_empty() {
        cx.editor.set_status("No notifications in history");
        return;
    }

    let ingress = cx.ingress.clone();
    cx.editor
        .work()
        .spawn(async move {
            send_ui_command_with(
                UiCommand::Layer(LayerCommand::PushNotificationHistory),
                ingress,
            )
            .await;
        })
        .detach();
}

/// Clear notification history
pub fn clear_notification_history(cx: &mut Context) {
    cx.editor.clear_notification_history();
    cx.editor.set_status("Notification history cleared");
}

/// Dismiss all active notifications
pub fn dismiss_all_notifications(cx: &mut Context) {
    cx.editor.dismiss_all_notifications();
    cx.editor.set_status("All notifications dismissed");
}

/// Test notification system with sample notifications
pub fn test_notifications(cx: &mut Context) {
    let config = &cx.editor.config().notifications;
    let timeout_ms = config.default_timeout.as_millis();

    // Debug output to log
    log::warn!(
        "DEBUG: Creating notification with timeout: {:?} ({}ms)",
        config.default_timeout,
        timeout_ms
    );

    // Create a simple test notification
    let _id = cx.editor.notify_info(format!(
        "Test notification (timeout: {}ms) - should disappear in {}s",
        timeout_ms,
        timeout_ms as f64 / 1000.0
    ));
    // Check if the notification was created with timeout
    let all_notifications = cx.editor.get_notification_history();
    if let Some(notification) = all_notifications.last() {
        log::warn!(
            "DEBUG: Notification {} created with timeout: {:?}",
            notification.id,
            notification.timeout
        );
    }

    cx.editor.set_status(format!(
        "Test notification sent with {}ms timeout - check terminal for debug",
        timeout_ms
    ));
}
