//! macOS menu-bar agent: tray icon + native notifications.
//!
//! Runs as an LSUIElement accessory (set in `main.rs` via winit's activation
//! policy), so the app never appears in the Dock. The window is hidden at
//! launch and shown from the tray. Notifications go through
//! `osascript display notification` — zero dependencies on this path.
//!
//! Verified against the `tray-icon` 0.19 API; real tray/notification behavior
//! on Apple hardware is fixture-testable only in this sandbox (same standing
//! caveat as the platform collectors).

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Commands the tray menu sends to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    ShowDashboard,
    SetPaused(bool),
    Quit,
}

pub struct MacosTray {
    _tray: TrayIcon,
    show_item: MenuItem,
    pause_item: CheckMenuItem,
    quit_item: MenuItem,
}

impl MacosTray {
    /// Build the tray. Must run on the main thread while an NSApplication
    /// event loop exists (eframe provides one). Fallible: in menu-bar agent
    /// mode a failed tray degrades the app to banner-only alerting instead
    /// of aborting — the error surfaces in the dashboard header.
    pub fn new(paused: bool) -> Result<Self, String> {
        let show_item = MenuItem::new("Show Dashboard", true, None);
        let pause_item = CheckMenuItem::new("Pause Alerts", true, paused, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show_item, &pause_item, &quit_item]);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Battery Viz — battery longevity coach")
            .with_icon(battery_icon()?)
            .build()
            .map_err(|e| format!("build tray icon: {e}"))?;

        Ok(MacosTray { _tray: tray, show_item, pause_item, quit_item })
    }

    /// Drain ALL pending menu events — clicks that arrive within one wake
    /// (up to 1 s in hidden mode) are queued, not collapsed.
    pub fn poll(&self) -> Vec<TrayCommand> {
        let events = MenuEvent::receiver();
        let mut commands = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.id == self.show_item.id() {
                commands.push(TrayCommand::ShowDashboard);
            } else if event.id == self.pause_item.id() {
                commands.push(TrayCommand::SetPaused(self.pause_item.is_checked()));
            } else if event.id == self.quit_item.id() {
                commands.push(TrayCommand::Quit);
            }
        }
        commands
    }
}

/// Deliver a notification via AppleScript. Spawned detached so the UI thread
/// never waits on `osascript` startup; the detached thread reaps the child
/// (`status()`) and surfaces failures instead of leaving zombies or silent
/// errors behind.
pub fn send_notification(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title)
    );
    std::thread::Builder::new()
        .name("notify-osascript".into())
        .spawn(move || match std::process::Command::new("osascript").args(["-e", &script]).status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => crate::notify::record_notify_error(format!(
                "osascript notification failed (exit {status})"
            )),
            Err(e) => crate::notify::record_notify_error(format!("osascript spawn failed: {e}")),
        })
        .ok();
}

/// Escape for an AppleScript double-quoted string.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A tiny fixed 16×16 battery glyph so the tray is not an empty square.
/// Built at startup (one allocation, once — never on the tick path).
fn battery_icon() -> Result<Icon, String> {
    const W: usize = 16;
    const H: usize = 16;
    let mut rgba = vec![0u8; W * H * 4];
    let set = |x: usize, y: usize, rgba: &mut [u8]| {
        let i = (y * W + x) * 4;
        rgba[i] = 60;
        rgba[i + 1] = 180;
        rgba[i + 2] = 75;
        rgba[i + 3] = 255;
    };
    // Outline 1..=14, plus a terminal nub on the right.
    for x in 1..=13 {
        set(x, 3, &mut rgba);
        set(x, 12, &mut rgba);
    }
    for y in 3..=12 {
        set(1, y, &mut rgba);
        set(13, y, &mut rgba);
        set(14, y + 1, &mut rgba);
    }
    // Fill bars: three segments, "charge level" look.
    for y in 5..=10 {
        for x in [3, 4, 6, 7, 9, 10] {
            set(x, y, &mut rgba);
        }
    }
    Icon::from_rgba(rgba, W as u32, H as u32).map_err(|e| format!("tray icon: {e:?}"))
}
