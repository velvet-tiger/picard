//! Shows Picard's notifications, and keeps only the newest one in
//! Notification Center.
//!
//! The notification plugin waits on a thread of its own for each notification
//! with a Clean… button. For each one, mac-notification-sys 0.6 also puts a
//! timer on the main run loop. Twice a second the timer asks Notification
//! Center whether the notification is still there, and the main thread blocks
//! on each answer. A notification the user never clicks or dismisses stays
//! there, so its timer never stops. After a few days of alerts the main thread
//! spent nearly all its time in those calls, and the menu and window stopped
//! responding.
//!
//! Removing Picard's earlier notifications before each new one fixes that. The
//! library sees a removed notification as dismissed, which ends its thread and
//! its timer, so at most one is ever live.

use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

/// What one notification says.
pub struct Alert {
    pub title: String,
    pub body: String,
    /// A registered action type, such as the one with the Clean… button.
    pub actions: Option<&'static str>,
}

/// Removes Picard's earlier notifications, then shows `alert`.
///
/// Both steps run on the main thread, in that order, so the removal cannot
/// catch the new notification. Failures are logged and not returned: a lost
/// alert must not stop a poll pass or a restart.
pub fn show<R: Runtime>(app: &AppHandle<R>, alert: Alert) {
    let handle = app.clone();
    let title = alert.title.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        clear_delivered();
        present(&handle, alert);
    }) {
        log::error!("could not show notification {title:?}: {e}");
    }
}

/// Hands `alert` to the plugin, which delivers it on a thread of its own.
fn present<R: Runtime>(app: &AppHandle<R>, alert: Alert) {
    let mut builder = app
        .notification()
        .builder()
        .title(&alert.title)
        .body(&alert.body);
    if let Some(actions) = alert.actions {
        builder = builder.action_type_id(actions);
    }
    if let Err(e) = builder.show() {
        log::error!("could not show notification {:?}: {e}", alert.title);
    }
}

/// Removes every notification Picard has in Notification Center.
///
/// NSUserNotification is deprecated, but it is the API mac-notification-sys
/// sends with, so only it can reach those notifications.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn clear_delivered() {
    // Under `tauri dev` the plugin sends as Terminal, so this would remove
    // Terminal's notifications instead of Picard's.
    if tauri::is_dev() {
        return;
    }
    objc2_foundation::NSUserNotificationCenter::defaultUserNotificationCenter()
        .removeAllDeliveredNotifications();
}

#[cfg(not(target_os = "macos"))]
fn clear_delivered() {}
