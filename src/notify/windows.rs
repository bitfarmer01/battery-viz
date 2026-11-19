//! Windows toast notifications via PowerShell WinRT — zero new crates.
//!
//! The one-liner builds a `ToastText02` template and shows it through a
//! toast notifier registered to the app's AUMID. Toasts inherit Focus Assist
//! and quiet-hour behavior for free, which is the "subtle, never nagware"
//! requirement on this platform.
//!
//! Real delivery on Windows hardware is fixture-testable only in this
//! sandbox (same standing caveat as the platform collectors).

use std::os::windows::process::CommandExt;

/// Spawn a toast. PowerShell cold-starts slowly (~100–300 ms), so the call
/// runs detached: the UI thread never blocks on it.
pub fn send_toast(title: &str, body: &str) {
    let script = toast_script(title, body);
    std::thread::Builder::new()
        .name("notify-toast".into())
        .spawn(move || {
            let mut command = std::process::Command::new("powershell");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &script,
            ]);
            // CREATE_NO_WINDOW: no console flash when spawned from a GUI app.
            command.creation_flags(0x0800_0000);
            let _ = command.spawn();
        })
        .ok();
}

/// Build the PowerShell one-liner. Public for fixture inspection.
fn toast_script(title: &str, body: &str) -> String {
    format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, \
         ContentType = WindowsRuntime] | Out-Null; \
         $t=[Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent(\
         [Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $x=$t.GetElementsByTagName('text'); \
         $x.Item(0).AppendChild($t.CreateTextNode('{title}')) | Out-Null; \
         $x.Item(1).AppendChild($t.CreateTextNode('{body}')) | Out-Null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier(\
         'Battery Viz').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
        title = ps_escape(title),
        body = ps_escape(body)
    )
}

/// Escape a string for single-quoted PowerShell string context.
fn ps_escape(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_script_embeds_escaped_text() {
        let s = toast_script("Battery Viz", "it's hot; unplug");
        assert!(s.contains("'Battery Viz'"));
        assert!(s.contains("'it''s hot; unplug'"), "single quotes must be doubled");
        assert!(s.contains("ToastText02"));
        // The toast is built from the template document `$t`, and the
        // notification is created from that same document.
        assert!(s.contains("::new($t)"));
        assert!(!s.contains("$x.Owner"), "text nodes must come from $t, not a bogus Owner");
    }
}
