//! Dashboard UI: GPU-rendered egui over wgpu.
//!
//! Layout: a benchmark ring on the left (score + grade), live gauges and
//! status top-right, per-parameter scorecard beneath, time-series plots at
//! the bottom. Unknown telemetry renders as "no data", never as a good score.


use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use crate::app::App;
use crate::benchmark::{grade, BenchmarkResult};
use crate::collect::types::{state_of_health_pct, ChargingState};

pub struct Dashboard {
    app: App,
    /// Status line after the last CSV export (destination path or error).
    export_status: Option<String>,
    /// macOS tray agent (menu: show dashboard / pause alerts / quit).
    #[cfg(target_os = "macos")]
    tray: crate::notify::macos::MacosTray,
}

impl Dashboard {
    pub fn new(app: App) -> Self {
        Dashboard {
            #[cfg(target_os = "macos")]
            tray: crate::notify::macos::MacosTray::new(app.alerts_paused),
            app,
            export_status: None,
        }
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tray commands arrive between repaints; handle them first.
        #[cfg(target_os = "macos")]
        self.handle_tray_commands(ctx);

        self.app.update();

        // Hidden (menu-bar agent) mode: the 1 Hz tick advances the engine and
        // collector, but no GPU frame is rendered while the window is hidden.
        if self.app.window_visible {
            draw(ctx, &mut self.app, &mut self.export_status);
        }
        ctx.request_repaint_after(self.app.repaint_interval());
    }
}

impl Dashboard {
    /// Tray commands arrive between repaints; handle them first.
    #[cfg(target_os = "macos")]
    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        use crate::notify::macos::TrayCommand;
        use egui::ViewportCommand;
        match self.tray.poll() {
            Some(TrayCommand::ShowDashboard) => {
                self.app.window_visible = true;
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(1280.0, 800.0)));
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            Some(TrayCommand::SetPaused(paused)) => self.app.set_alerts_paused(paused),
            Some(TrayCommand::Quit) => std::process::exit(0),
            None => {}
        }
    }
}

fn score_color(v: f64) -> egui::Color32 {
    // green → amber → red by score, without relying on color alone (labels
    // always carry the numbers).
    let (r, g) = if v >= 60.0 {
        let t = (v - 60.0) / 40.0;
        (200.0 - 100.0 * t, 200.0 + 55.0 * t)
    } else {
        let t = v / 60.0;
        (230.0 - 30.0 * t, 60.0 + 140.0 * t)
    };
    egui::Color32::from_rgb(r as u8, g as u8, 70)
}

fn draw(
    ctx: &egui::Context,
    app: &mut App,
    export_status: &mut Option<String>,
) {
    egui::TopBottomPanel::top("header").show(ctx, |ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("⚡ Battery Viz");
            ui.separator();
            if let Some(s) = &app.latest {
                ui.label(format!("Source: {}", s.source.label()));
                if let Some(state) = s.charging_state {
                    ui.separator();
                    ui.label(match state {
                        ChargingState::Charging => "🔌 Charging",
                        ChargingState::Full => "🔋 Full (on AC)",
                        ChargingState::OnAcNotCharging => "🔌 On AC, not charging",
                        ChargingState::Discharging => "🔋 On battery",
                        ChargingState::Unknown => "Power state unknown",
                    });
                }
            } else {
                ui.weak("Waiting for first sample…");
            }
            if let Some(err) = &app.last_error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 120, 90), format!("⚠ {err}"));
            }
            ui.separator();
            let pause_label = if app.alerts_paused { "▶ Resume alerts" } else { "⏸ Pause alerts" };
            if ui.button(pause_label).clicked() {
                app.set_alerts_paused(!app.alerts_paused);
            }
            if app.alerts_paused {
                ui.weak("alerts paused");
            }
        });
        ui.add_space(8.0);
    });

    // Coaching-nudge banner: the latest alert stays visible until dismissed.
    if let Some(alert) = app.last_alert {
        egui::TopBottomPanel::top("alert_banner").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(240, 190, 90), "💡");
                ui.label(alert.message);
                if ui.small_button("Dismiss").clicked() {
                    app.last_alert = None;
                }
            });
            ui.add_space(6.0);
        });
    }

    egui::CentralPanel::default()
        .frame(
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(16, 18, 24))
                .inner_margin(12.0),
        )
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(egui::Color32::from_rgb(226, 230, 238));

            egui::SidePanel::left("benchmark_panel")
                .resizable(false)
                .exact_width(300.0)
                .show_inside(ui, |ui| {
                    draw_benchmark_ring(ui, app.benchmark.as_ref());
                    ui.add_space(12.0);
                    draw_scorecard(ui, app.benchmark.as_ref());
                });

            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                draw_live_gauges(ui, app);
                draw_plots(ui, app);
                draw_trends(ui, app, export_status);
            });
        });
}

fn draw_benchmark_ring(ui: &mut egui::Ui, result: Option<&BenchmarkResult>) {
    ui.add_space(8.0);
    let (total, grade_text) = match result {
        Some(r) => (r.total, grade(r.total).to_string()),
        None => (0.0, "—".to_string()),
    };
    let color = score_color(total);

    let (rect, _) = ui.allocate_exact_size(egui::vec2(240.0, 240.0), egui::Sense::hover());
    let center = rect.center();
    let radius = rect.size().min_elem() / 2.0 - 10.0;

    // Track + arc.
    ui.painter()
        .circle_stroke(center, radius, egui::Stroke::new(6.0_f32, egui::Color32::from_rgb(40, 44, 56)));
    let sweep = (std::f64::consts::TAU as f32)
        * ((total / 100.0).clamp(0.0, 1.0) as f32);
    let n_segments = 96;
    let points: Vec<egui::Pos2> = (0..=n_segments)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + sweep * (i as f32 / n_segments as f32);
            center + radius * egui::vec2(a.cos(), a.sin())
        })
        .collect();
    for w in points.windows(2) {
        ui.painter()
            .line_segment([w[0], w[1]], egui::Stroke::new(6.0_f32, color));
    }

    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        if result.is_some() { format!("{total:.0}") } else { "…".into() },
        egui::FontId::proportional(52.0),
        color,
    );
    ui.painter().text(
        center + egui::vec2(0.0, 40.0),
        egui::Align2::CENTER_CENTER,
        grade_text,
        egui::FontId::proportional(18.0),
        egui::Color32::from_rgb(150, 158, 175),
    );

    if let Some(r) = result {
        let missing = 1.0 - r.coverage;
        if missing > 0.001 {
            ui.add_space(6.0);
            ui.set_min_width(260.0);
            ui.weak(format!(
                "Benchmark coverage {:.0}% — missing telemetry shows as “no data”, not as a good score.",
                r.coverage * 100.0
            ));
        }
    }
}

fn draw_scorecard(ui: &mut egui::Ui, result: Option<&BenchmarkResult>) {
    ui.heading("Scorecard");
    ui.add_space(4.0);
    match result {
        None => {
            ui.weak("No sample yet.");
        }
        Some(r) if r.parameters.is_empty() => {
            ui.weak("No parameters could be scored.");
        }
        Some(r) => {
            egui::Grid::new("scorecard")
                .num_columns(3)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    for p in &r.parameters {
                        ui.label(p.name);
                        let bar = egui::ProgressBar::new((p.score / 100.0) as f32)
                            .desired_width(110.0)
                            .text(format!("{:.0}", p.score));
                        ui.add(bar);
                        ui.weak(egui::RichText::new(p.detail.clone()).small());
                        ui.end_row();
                    }
                });
        }
    }
}

fn draw_live_gauges(ui: &mut egui::Ui, app: &App) {
    ui.heading("Live telemetry");
    ui.add_space(4.0);
    let Some(s) = &app.latest else {
        ui.weak("Collecting…");
        return;
    };

    egui::Grid::new("gauges").num_columns(4).spacing([24.0, 6.0]).show(ui, |ui| {
        gauge(ui, "Charge", fmt_opt(s.state_of_charge, "%"));
        gauge(ui, "Health (SoH)", fmt_opt(state_of_health_pct(s), "%"));
        gauge(ui, "Temperature", fmt_opt(s.temperature_c, "°C"));
        gauge(ui, "Power", match s.power_w {
            Some(w) if w < 0.0 => format!("−{:.1} W in", -w),
            Some(w) => format!("{:.1} W out", w),
            None => "no data".into(),
        });
        ui.end_row();
        gauge(ui, "Cycles", s.cycle_count.map(|c| c.to_string()).unwrap_or_else(|| "no data".into()));
        gauge(ui, "Cycle limit", s.cycle_limit.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()));
        gauge(ui, "Voltage", fmt_opt(s.voltage_mv.map(|v| v / 1000.0), " V"));
        gauge(ui, "High-charge dwell", match s.high_soc_dwell_s {
            Some(sec) => format!("{:.0} min", sec / 60.0),
            None => "not tracked".into(),
        });
        ui.end_row();
    });

    if s.condition_flags.iter().any(|f| {
        matches!(f, crate::collect::ConditionFlag::ServiceRecommended | crate::collect::ConditionFlag::Failure)
    }) {
        ui.add_space(6.0);
        ui.colored_label(
            egui::Color32::from_rgb(235, 110, 90),
            "⚠ OS reports a battery condition — consider service.",
        );
    }
}

fn fmt_opt(v: Option<f64>, suffix: &str) -> String {
    match v {
        Some(v) => format!("{v:.1}{suffix}"),
        None => "no data".into(),
    }
}

fn gauge(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.weak(egui::RichText::new(label).small());
        ui.label(egui::RichText::new(value).size(20.0).strong());
    });
}

fn draw_plots(ui: &mut egui::Ui, app: &App) {
    ui.add_space(8.0);
    let h = &app.history;
    if h.t.is_empty() {
        ui.weak("History builds as samples arrive…");
        return;
    }
    let t: Vec<f64> = h.t.clone();
    ui.columns(2, |cols| {
        cols[0].label("Temperature (°C)");
        Plot::new("temp_plot")
            .height(160.0)
            .allow_drag(false)
            .allow_zoom(false)
            .show_axes([false, true])
            .show(&mut cols[0], |plot| {
                plot.line(
                    Line::new(PlotPoints::from_iter(
                        t.iter().zip(h.temp.iter()).map(|(a, b)| [*a, *b]),
                    ))
                    .color(score_color(75.0)),
                );
            });
        cols[1].label("Power (W, negative = charging)");
        Plot::new("power_plot")
            .height(160.0)
            .allow_drag(false)
            .allow_zoom(false)
            .show_axes([false, true])
            .show(&mut cols[1], |plot| {
                plot.line(
                    Line::new(PlotPoints::from_iter(
                        t.iter().zip(h.watts.iter()).map(|(a, b)| [*a, *b]),
                    ))
                    .color(score_color(50.0)),
                );
            });
    });
}

/// Long-horizon trends from the downsampled store: charge and temperature
/// over the full retained window (up to 7 days of 1-minute means), plus CSV
/// export of the same data.
fn draw_trends(ui: &mut egui::Ui, app: &App, export_status: &mut Option<String>) {
    ui.add_space(8.0);
    ui.heading("Trends — up to 7 days, 1-minute averages");
    let store = &app.history_store;
    if store.buckets.is_empty() {
        ui.weak("Long-term history builds one minute at a time and loads from disk on startup…");
        return;
    }

    // X axis: hours ago, so the window reads naturally without date ticks.
    let now_min = crate::history::unix_now() / 60;
    let hours_ago = |minute: u64| now_min.saturating_sub(minute) as f64 / 60.0;
    let trend_points = |values: Vec<Option<f64>>| -> Vec<[f64; 2]> {
        store
            .buckets
            .iter()
            .zip(values)
            .filter_map(|(b, v)| v.map(|v| [hours_ago(b.minute), v]))
            .collect()
    };
    let charge = trend_points(store.buckets.iter().map(|b| b.soc_mean()).collect());
    let temp = trend_points(store.buckets.iter().map(|b| b.temp_mean()).collect());

    ui.columns(2, |cols| {
        cols[0].label("Charge (%)");
        Plot::new("trend_charge_plot")
            .height(160.0)
            .allow_drag(false)
            .allow_zoom(false)
            .show_axes([false, true])
            .show(&mut cols[0], |plot| {
                plot.line(
                    Line::new(PlotPoints::from_iter(charge.iter().copied()))
                        .color(score_color(90.0)),
                );
            });
        cols[1].label("Temperature (°C)");
        Plot::new("trend_temp_plot")
            .height(160.0)
            .allow_drag(false)
            .allow_zoom(false)
            .show_axes([false, true])
            .show(&mut cols[1], |plot| {
                plot.line(
                    Line::new(PlotPoints::from_iter(temp.iter().copied()))
                        .color(score_color(75.0)),
                );
            });
    });

    ui.horizontal(|ui| {
        if ui.button("Export CSV").clicked() {
            *export_status = Some(match app.export_csv() {
                Ok(path) => format!("Exported to {}", path.display()),
                Err(e) => format!("Export failed: {e}"),
            });
        }
        if let Some(status) = export_status {
            ui.weak(egui::RichText::new(status.as_str()).small());
        }
    });
}