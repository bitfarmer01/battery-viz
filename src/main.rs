//! Entry point: boots the platform collector and the GPU-rendered window.

mod app;
mod benchmark;
mod collect;
mod history;
mod ui;

fn main() -> eframe::Result<()> {
    let rx = collect::spawn_collector(collect::platform_default());

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Battery Viz"),
        ..Default::default()
    };

    eframe::run_native(
        "Battery Viz — battery optimization visualizer",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ui::Dashboard::new(app::App::new(rx)))
                as Box<dyn eframe::App>)
        }),
    )
}
