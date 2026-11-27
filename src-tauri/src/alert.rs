//! Picard's notifications, sent through `UNUserNotificationCenter`.
//!
//! Picard used tauri-plugin-notification, which sits on mac-notification-sys
//! and the deprecated `NSUserNotification`. That library waited on a thread
//! for each notification with a button. It also put a timer on the main run
//! loop that asked Notification Center, twice a second and with a blocking
//! call, whether the notification was still there. Alerts nobody clicked kept
//! their timers, and after a few days the menu and window stopped responding.
//!
//! `UNUserNotificationCenter` reports clicks to a delegate instead, so nothing
//! polls and no thread waits. Every alert uses one request identifier, so a new
//! alert replaces the last one and Notification Center holds only the newest.
//!
//! The API needs a real app bundle. Called from a bare binary, such as the one
//! `tauri dev` runs, it raises an Objective-C exception and Picard aborts.
//! Outside a bundle, alerts are logged and not shown.

use tauri::{AppHandle, Runtime};

use crate::windows::{self, Tab};

/// What one notification says.
pub struct Alert {
    pub title: String,
    pub body: String,
    /// Adds the Clean… button, which opens the Clean tab.
    pub clean_button: bool,
}

/// The one request identifier every alert uses, so each replaces the last.
const REQUEST_ID: &str = "picard-alert";
/// The category that carries the Clean… button.
const CLEAN_CATEGORY: &str = "picard-low-space";
/// The Clean… button's action.
const CLEAN_ACTION: &str = "clean";

/// Sets the delegate that handles clicks, and registers the Clean… button.
///
/// Call it once, in setup, so a click that arrives early has somewhere to go.
/// It asks for no permission. `request_permission` does that.
pub fn install<R: Runtime>(app: &AppHandle<R>) {
    if !imp::bundled() {
        log::info!("notifications are off: Picard is not running from an app bundle");
        return;
    }
    let handle = app.clone();
    imp::install(Box::new(move || {
        if let Err(e) = windows::open(&handle, Tab::Clean) {
            log::error!("could not open the Clean window from a notification: {e}");
        }
    }));
}

/// Asks macOS for permission to show alerts. macOS asks the user only once,
/// and later calls return the answer they gave. A refusal is logged.
pub fn request_permission() {
    if imp::bundled() {
        imp::request_permission();
    }
}

/// Shows `alert` in place of Picard's previous notification. Failures are
/// logged and not returned: a lost alert must not stop a poll pass or a restart.
pub fn show(alert: Alert) {
    if !imp::bundled() {
        log::info!(
            "notification not shown outside an app bundle: {}",
            alert.title
        );
        return;
    }
    imp::show(alert);
}

/// Whether a click macOS reports as `action` should open the Clean tab.
/// `default_action` is what macOS reports for a click on the alert itself.
fn opens_clean_tab(action: &str, default_action: &str) -> bool {
    action == CLEAN_ACTION || action == default_action
}

/// Whether the process runs from an app bundle: a `.app` folder with an
/// identifier in its `Info.plist`.
fn is_app_bundle(bundle_path: &str, identifier: Option<&str>) -> bool {
    bundle_path.ends_with(".app") && identifier.is_some_and(|id| !id.is_empty())
}

#[cfg(target_os = "macos")]
mod imp {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AnyThread, DefinedClass, define_class, msg_send};
    use objc2_foundation::{NSArray, NSBundle, NSError, NSSet, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
        UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };

    use super::{Alert, CLEAN_ACTION, CLEAN_CATEGORY, REQUEST_ID};

    /// Runs when the user clicks an alert or its Clean… button.
    pub type OnClean = Box<dyn Fn() + Send + Sync>;

    pub struct Ivars {
        on_clean: OnClean,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and AlertDelegate
        // does not implement Drop.
        #[unsafe(super(NSObject))]
        #[ivars = Ivars]
        struct AlertDelegate;

        unsafe impl NSObjectProtocol for AlertDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for AlertDelegate {
            // Without this, macOS hides an alert while Picard's window is in front.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                done.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                done: &block2::DynBlock<dyn Fn()>,
            ) {
                let action = response.actionIdentifier().to_string();
                // SAFETY: a constant string the framework exports.
                let default_action = unsafe { UNNotificationDefaultActionIdentifier }.to_string();
                if super::opens_clean_tab(&action, &default_action) {
                    (self.ivars().on_clean)();
                }
                done.call(());
            }
        }
    );

    impl AlertDelegate {
        fn new(on_clean: OnClean) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { on_clean });
            // SAFETY: NSObject's designated initializer.
            unsafe { msg_send![super(this), init] }
        }
    }

    pub fn bundled() -> bool {
        let bundle = NSBundle::mainBundle();
        let identifier = bundle.bundleIdentifier().map(|id| id.to_string());
        super::is_app_bundle(&bundle.bundlePath().to_string(), identifier.as_deref())
    }

    pub fn install(on_clean: OnClean) {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = AlertDelegate::new(on_clean);
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center holds the delegate weakly, so Picard keeps it for the life
        // of the process.
        let _ = Retained::into_raw(delegate);

        let clean = UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(CLEAN_ACTION),
            &NSString::from_str("Clean…"),
            UNNotificationActionOptions::empty(),
        );
        let category =
            UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                &NSString::from_str(CLEAN_CATEGORY),
                &NSArray::from_retained_slice(&[clean]),
                &NSArray::new(),
                UNNotificationCategoryOptions::empty(),
            );
        center.setNotificationCategories(&NSSet::from_retained_slice(&[category]));
    }

    pub fn request_permission() {
        let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
            if granted.as_bool() {
                return;
            }
            match describe(error) {
                Some(reason) => log::warn!("notifications are not allowed: {reason}"),
                None => {
                    log::warn!("notifications are not allowed; turn them on in System Settings")
                }
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert,
                &done,
            );
    }

    pub fn show(alert: Alert) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&alert.title));
        content.setBody(&NSString::from_str(&alert.body));
        if alert.clean_button {
            content.setCategoryIdentifier(&NSString::from_str(CLEAN_CATEGORY));
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(REQUEST_ID),
            &content,
            None,
        );
        let title = alert.title;
        let done = RcBlock::new(move |error: *mut NSError| {
            if let Some(reason) = describe(error) {
                log::error!("could not show notification {title:?}: {reason}");
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }

    /// The message of a framework error, or `None` when there is none.
    fn describe(error: *mut NSError) -> Option<String> {
        // SAFETY: the framework passes a valid NSError or null.
        unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string())
    }
}

// Picard ships for macOS only. This keeps other targets compiling.
#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Alert;

    pub type OnClean = Box<dyn Fn() + Send + Sync>;

    pub fn bundled() -> bool {
        false
    }
    pub fn install(_on_clean: OnClean) {}
    pub fn request_permission() {}
    pub fn show(_alert: Alert) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What macOS reports for a click on the alert itself.
    const DEFAULT: &str = "com.apple.UNNotificationDefaultActionIdentifier";

    #[test]
    fn a_click_on_the_alert_or_its_clean_button_opens_the_clean_tab() {
        assert!(opens_clean_tab(DEFAULT, DEFAULT));
        assert!(opens_clean_tab(CLEAN_ACTION, DEFAULT));
    }

    #[test]
    fn dismissing_an_alert_opens_nothing() {
        assert!(!opens_clean_tab(
            "com.apple.UNNotificationDismissActionIdentifier",
            DEFAULT
        ));
        assert!(!opens_clean_tab("", DEFAULT));
    }

    #[test]
    fn only_an_app_folder_with_an_identifier_is_a_bundle() {
        assert!(is_app_bundle(
            "/Applications/Picard.app",
            Some("com.velvettiger.picard")
        ));
        assert!(!is_app_bundle("/Applications/Picard.app", None));
        assert!(!is_app_bundle("/Applications/Picard.app", Some("")));
        // `tauri dev` runs the bare binary, whose bundle is its folder.
        assert!(!is_app_bundle(
            "/src/picard/src-tauri/target/debug",
            Some("picard")
        ));
    }
}
