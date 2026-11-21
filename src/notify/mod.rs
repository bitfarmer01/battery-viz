//! Platform notification adapters.
//!
//! Both paths are deliberately dependency-free: macOS shells out to
//! `osascript`, Windows to a PowerShell WinRT toast one-liner. The only new
//! dependency in this feature is `tray-icon` (macOS tray menu, cfg-gated).

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use std::sync::Mutex;

/// Last platform-notification failure, surfaced in the dashboard header. A
/// missed nudge is better than a dead dashboard, but a dead delivery path
/// must never be silent — the banner always shows the alert, and this shows
/// why the OS did not.
static NOTIFY_ERROR: Mutex<Option<String>> = Mutex::new(None);

// Allowlist (M3 lint gate): called only from the cfg-gated platform
// notification backends (macOS/Windows); it is never reachable from a
// host where those modules do not compile.
#[allow(dead_code)]
pub(crate) fn record_notify_error(message: String) {
    if let Ok(mut slot) = NOTIFY_ERROR.lock() {
        *slot = Some(message);
    }
}

/// Take the most recent delivery failure, if any (consumed by the app tick).
pub fn take_notify_error() -> Option<String> {
    NOTIFY_ERROR.lock().ok().and_then(|mut slot| slot.take())
}

/// One-time platform setup (Windows: register the toast AUMID so toasts are
/// not suppressed). Cheap, idempotent, and safe to call before any alert.
pub fn init() {
    #[cfg(target_os = "windows")]
    {
        windows::ensure_aumid_registered();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // macOS registers itself with Notification Center implicitly via
        // `osascript`; dev hosts have no native surface to configure.
    }
}

/// Deliver one notification through the platform's native, user-level quiet
/// path (macOS Notification Center, Windows Focus Assist). Fire-and-forget:
/// the call never blocks the UI thread. Delivery failures are surfaced
/// through `take_notify_error` (rendered in the header) — never swallowed.
pub fn send(kind_label: &str, message: &str) {
    #[cfg(target_os = "macos")]
    {
        macos::send_notification(kind_label, message);
    }

    #[cfg(target_os = "windows")]
    {
        windows::send_toast(kind_label, message);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Dev platforms: notifications have no native surface here. The
        // dashboard banner still shows every alert, so this is a no-op.
        let _ = (kind_label, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_error_sink_is_a_one_slot_queue() {
        // Cross-platform: every platform module reports failures into this
        // sink; the app tick drains it. Take-once semantics.
        record_notify_error("first".into());
        record_notify_error("second".into());
        assert_eq!(take_notify_error().as_deref(), Some("second"));
        assert_eq!(take_notify_error(), None);
    }
}
