//! Platform notification adapters.
//!
//! Both paths are deliberately dependency-free: macOS shells out to
//! `osascript`, Windows to a PowerShell WinRT toast one-liner. The only new
//! dependency in this feature is `tray-icon` (macOS tray menu, cfg-gated).

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Deliver one notification through the platform's native, user-level quiet
/// path (macOS Notification Center, Windows Focus Assist). Fire-and-forget:
/// failures are swallowed on purpose — a missed nudge is better than a dead
/// dashboard, and the alert banner in the dashboard always shows the latest.
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
