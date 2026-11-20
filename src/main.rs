//! Entry point: boots the platform collector and the GPU-rendered window.
//!
//! Two modes (spec: menu-bar agent vs. main window):
//! - macOS: menu-bar agent — LSUIElement accessory (no Dock icon), window
//!   hidden at launch, tray icon drives show/pause/quit.
//! - Everywhere else: main window shown as before; alerts are notifications.

mod alert;
mod app;
mod benchmark;
mod caps;
mod clock;
mod collect;
mod dwell;
mod history;
mod notify;
mod ui;
mod uninstall;

use eframe::egui::ViewportBuilder;

fn main() -> eframe::Result<()> {
    let rx = collect::spawn_collector(caps::platform_collector());

    // Menu-bar agent mode: hidden at launch. `BATTERY_VIZ_START_HIDDEN`
    // forces the same state on dev platforms (used for RSS measurement).
    let start_hidden =
        cfg!(target_os = "macos") || std::env::var("BATTERY_VIZ_START_HIDDEN").is_ok();

    // Hidden agent mode never renders, so it boots with a minimal surface;
    // showing the dashboard from the tray restores the full size. This keeps
    // the swapchain and framebuffer allocations tiny while hidden.
    let (width, height, min) = if start_hidden {
        (320.0, 200.0, [320.0, 200.0])
    } else {
        (1280.0, 800.0, [900.0, 600.0])
    };
    let viewport = ViewportBuilder::default()
        .with_inner_size([width, height])
        .with_min_inner_size(min)
        .with_title("Battery Viz")
        .with_visible(!start_hidden);

    let native_options = eframe::NativeOptions {
        viewport,
        // Battery-first rendering: the discrete GPU is never woken.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            power_preference: eframe::wgpu::PowerPreference::LowPower,
            ..Default::default()
        },
        // macOS only: run as an accessory (LSUIElement) so the app stays out
        // of the Dock; the tray icon is the UI surface.
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            builder.with_activation_policy(winit::platform::macos::ActivationPolicy::Accessory);
        })),
        ..Default::default()
    };

    eframe::run_native(
        "Battery Viz — battery optimization visualizer",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ui::Dashboard::new(app::App::new(rx))) as Box<dyn eframe::App>)
        }),
    )
}
