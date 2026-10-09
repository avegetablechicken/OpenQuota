use std::{ptr::NonNull, sync::mpsc, sync::Mutex, time::Duration};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNNotificationSettings, UNUserNotificationCenter,
};

fn is_bundled() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

fn permission_label(status: UNAuthorizationStatus) -> &'static str {
    match status {
        UNAuthorizationStatus::NotDetermined => "prompt",
        UNAuthorizationStatus::Denied => "denied",
        UNAuthorizationStatus::Authorized
        | UNAuthorizationStatus::Provisional
        | UNAuthorizationStatus::Ephemeral => "granted",
        _ => "unavailable",
    }
}

pub(super) fn permission() -> &'static str {
    // Apple's notification center requires an app bundle; command-line/dev
    // executables must not call it or pretend to have the app's permission.
    if !is_bundled() {
        return "unavailable";
    }
    let (sender, receiver) = mpsc::channel();
    let callback = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // The notification center supplies a valid object for this callback.
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let _ = sender.send(permission_label(status));
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&callback);
    receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or("unavailable")
}

pub(super) async fn request_permission() -> Result<(), String> {
    if !is_bundled() {
        return Err("Open the built OpenQuota.app to request notification permission.".into());
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    {
        let sender = Mutex::new(Some(sender));
        let callback = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
            if let Some(sender) = sender.lock().ok().and_then(|mut value| value.take()) {
                let result = if error.is_null() {
                    Ok(())
                } else {
                    // Apple keeps the NSError alive for the completion callback.
                    let error = unsafe { &*error };
                    crate::app_error!(
                        "notifications",
                        "macOS authorization failed ({} / {}): {}",
                        error.domain(),
                        error.code(),
                        error.localizedDescription()
                    );
                    Err(format!(
                        "Notification permission could not be requested: {}",
                        error.localizedDescription()
                    ))
                };
                let _ = sender.send(result);
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Sound
                    | UNAuthorizationOptions::Badge,
                &callback,
            );
    }
    // Wait asynchronously while the user answers the system prompt so the
    // webview and macOS main run loop remain responsive.
    receiver
        .await
        .map_err(|_| "Notification permission request was interrupted.".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_authorization_states_are_not_assumed_granted() {
        assert_eq!(
            permission_label(UNAuthorizationStatus::NotDetermined),
            "prompt"
        );
        assert_eq!(permission_label(UNAuthorizationStatus::Denied), "denied");
        for status in [
            UNAuthorizationStatus::Authorized,
            UNAuthorizationStatus::Provisional,
            UNAuthorizationStatus::Ephemeral,
        ] {
            assert_eq!(permission_label(status), "granted");
        }
        assert_eq!(permission_label(UNAuthorizationStatus(99)), "unavailable");
    }
}
