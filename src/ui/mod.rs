//! Dashboard UI: GPU-rendered egui over wgpu.
//!
//! Layout: a benchmark ring on the left (score + grade), live gauges and
//! status top-right, per-parameter scorecard beneath, time-series plots at
//! the bottom. Unknown telemetry renders as "no data", never as a good score.

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use crate::app::App;
use crate::benchmark::{grade, BenchmarkResult};
use crate::capdetect::{CapPromptAnswer, CapStatus};
use crate::collect::types::{state_of_health_pct, ChargingState};

pub struct Dashboard {
    app: App,
    /// Status line after the last CSV export (destination path or error).
    export_status: Option<String>,
    /// macOS tray agent (menu: show dashboard / pause alerts / uninstall /
    /// quit). `None`
    /// when the tray could not be built — the app degrades to banner-only
    /// alerting with the reason surfaced through the notify error sink,
    /// instead of aborting.
    #[cfg(target_os = "macos")]
    tray: Option<crate::notify::macos::MacosTray>,
    /// Set when the user chooses Quit in the tray: close proceeds through
    /// eframe's normal teardown instead of the hide-to-tray intercept.
    #[cfg(target_os = "macos")]
    quit_requested: bool,
}

impl Dashboard {
    pub fn new(app: App) -> Self {
        #[cfg(target_os = "macos")]
        let tray = match crate::notify::macos::MacosTray::new(app.alerts_paused) {
            Ok(tray) => Some(tray),
            Err(e) => {
                crate::notify::record_notify_error(format!("tray unavailable: {e}"));
                None
            }
        };
        Dashboard {
            #[cfg(target_os = "macos")]
            tray,
            #[cfg(target_os = "macos")]
            quit_requested: false,
            app,
            export_status: None,
        }
    }
}

impl eframe::App for Dashboard {
    /// M1: final checkpoint on a graceful exit (tray Quit or window close),
    /// so a normal restart never loses the up-to-15-minute cadence window.
    fn on_exit(&mut self) {
        self.app.save_history_now();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close-to-tray: closing the dashboard window hides it (the menu-bar
        // agent keeps running); only an explicit tray Quit ends the process,
        // and it does so through eframe's normal teardown path.
        #[cfg(target_os = "macos")]
        {
            use egui::ViewportCommand;
            if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
                self.app.window_visible = false;
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }

        // Tray commands arrive between repaints; handle them first.
        #[cfg(target_os = "macos")]
        self.handle_tray_commands(ctx);

        // Finish-card Quit: end the process through eframe's normal teardown
        // (on macOS the close-to-tray intercept yields because
        // `quit_requested` is set first).
        if self.app.quit_requested {
            #[cfg(target_os = "macos")]
            {
                self.quit_requested = true;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

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
    /// Tray commands arrive between repaints; handle every drained event.
    #[cfg(target_os = "macos")]
    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        use crate::notify::macos::TrayCommand;
        use egui::ViewportCommand;
        let commands = self.tray.as_ref().map(|t| t.poll()).unwrap_or_default();
        for command in commands {
            match command {
                TrayCommand::ShowDashboard => {
                    self.app.window_visible = true;
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(1280.0, 800.0)));
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                TrayCommand::SetPaused(paused) => self.app.set_alerts_paused(paused),
                // Uninstall opens the in-app confirm panel, so the agent must
                // be visible to show it.
                TrayCommand::Uninstall => {
                    self.app.uninstall_requested = true;
                    self.app.window_visible = true;
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(1280.0, 800.0)));
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                // Clean shutdown: request the viewport close and let eframe
                // unwind normally (the hide-to-tray intercept yields because
                // `quit_requested` is set).
                TrayCommand::Quit => {
                    self.quit_requested = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
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

fn draw(ctx: &egui::Context, app: &mut App, export_status: &mut Option<String>) {
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
            if let Some(err) = &app.last_notify_error {
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(230, 180, 90),
                    format!("⚠ notifications: {err}"),
                );
            }
            // Sticky notice: the persisted history could not be read and was
            // preserved as `history.json.corrupt` (M2 — never silent).
            if let Some(err) = &app.history_load_error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 180, 90), format!("⚠ {err}"));
            }
            if let Some(err) = &app.record_load_error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 180, 90), format!("⚠ {err}"));
            }
            // Sticky notice: the advice-state marker could not be read and
            // was preserved as `advice-state.json.corrupt` (M2 — never
            // silent). The advice stays hidden, deliberately.
            if let Some(err) = &app.advice_load_error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 180, 90), format!("⚠ {err}"));
            }
            // FR-8: what the first-run import found. Absent when nothing
            // was imported — never a placeholder.
            if let Some(note) = &app.first_run_note {
                ui.separator();
                ui.weak(note);
            }
            // FR-1: the charge-cap state file could not be read — sticky,
            // never silent.
            if let Some(err) = &app.cap_setup_load_error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(230, 180, 90), format!("⚠ {err}"));
            }
            ui.separator();
            let pause_label = if app.alerts_paused {
                "▶ Resume alerts"
            } else {
                "⏸ Pause alerts"
            };
            if ui.button(pause_label).clicked() {
                app.set_alerts_paused(!app.alerts_paused);
            }
            if app.alerts_paused {
                ui.weak("alerts paused");
            }
            ui.separator();
            if ui.button("Uninstall…").clicked() {
                app.request_uninstall();
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

    // FR-5 one-time advice card: rendered only while an unsaid advice is
    // up (the detector fired and the adapter has an adaptive equivalent to
    // point at). Dismissal persists the once-ever flag.
    if full_charge_advice_visible(app) {
        draw_full_charge_advice(ctx, app);
    }

    // FR-1: the one-time charge-cap setup prompt, above the dashboard.
    draw_cap_setup_prompt(ctx, app);

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
                draw_weekly_digest(ui, app);
            });
        });

    // Modal uninstall flow, drawn above the dashboard. The finish card is
    // checked first so the confirm panel never re-opens over it.
    if app.uninstall_results.is_some() {
        let results = app.uninstall_results.clone().unwrap_or_default();
        draw_uninstall_finish(ctx, app, &results);
    } else if app.uninstall_requested {
        draw_uninstall_confirm(ctx, app);
    }
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
    ui.painter().circle_stroke(
        center,
        radius,
        egui::Stroke::new(6.0_f32, egui::Color32::from_rgb(40, 44, 56)),
    );
    let sweep = (std::f64::consts::TAU as f32) * ((total / 100.0).clamp(0.0, 1.0) as f32);
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
        if result.is_some() {
            format!("{total:.0}")
        } else {
            "…".into()
        },
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

    egui::Grid::new("gauges")
        .num_columns(4)
        .spacing([24.0, 6.0])
        .show(ui, |ui| {
            gauge(ui, "Charge", fmt_opt(s.state_of_charge, "%"));
            gauge(ui, "Health (SoH)", fmt_opt(state_of_health_pct(s), "%"));
            gauge(ui, "Temperature", fmt_opt(s.temperature_c, "°C"));
            gauge(
                ui,
                "Power",
                match s.power_w {
                    Some(w) if w < 0.0 => format!("−{:.1} W in", -w),
                    Some(w) => format!("{:.1} W out", w),
                    None => "no data".into(),
                },
            );
            ui.end_row();
            gauge(
                ui,
                "Cycles",
                s.cycle_count
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "no data".into()),
            );
            gauge(
                ui,
                "Cycle limit",
                s.cycle_limit
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".into()),
            );
            gauge(
                ui,
                "Voltage",
                fmt_opt(s.voltage_mv.map(|v| v / 1000.0), " V"),
            );
            gauge(
                ui,
                "High-charge dwell",
                match s.high_soc_dwell_s {
                    Some(sec) => format!("{:.0} min", sec / 60.0),
                    None => "not tracked".into(),
                },
            );
            ui.end_row();
        });

    if s.condition_flags.iter().any(|f| {
        matches!(
            f,
            crate::collect::ConditionFlag::ServiceRecommended
                | crate::collect::ConditionFlag::Failure
        )
    }) {
        ui.add_space(6.0);
        ui.colored_label(
            egui::Color32::from_rgb(235, 110, 90),
            "⚠ OS reports a battery condition — consider service.",
        );
    }

    // FR-1: charge-cap detection status. Hidden = the adapter says the
    // feature is unsupported here; the row disappears rather than showing
    // noise (degrade-or-hide, never guess).
    match app.cap_monitor.status() {
        CapStatus::Hidden => {}
        CapStatus::Undetermined => {
            ui.add_space(4.0);
            ui.weak("Charge cap: watching charging behaviour…");
        }
        CapStatus::Capped => {
            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(120, 200, 130),
                "🛡 Charge cap: likely active (behavioural) — the pack has been held below full on AC for hours.",
            );
        }
        CapStatus::Uncapped => {
            ui.add_space(4.0);
            ui.weak("Charge cap: likely none (behavioural) — the pack reached full on AC.");
        }
    }
}

fn fmt_opt(v: Option<f64>, suffix: &str) -> String {
    match v {
        Some(v) => format!("{v:.1}{suffix}"),
        None => "no data".into(),
    }
}

/// FR-1: the one-time setup prompt. Names the exact per-machine steps from
/// the FR-0 adapter (macOS: the System Settings battery pane; Windows: the
/// maker's tool). The app never changes the cap itself — it only points.
fn draw_cap_setup_prompt(ctx: &egui::Context, app: &mut App) {
    // One gate: the record says unanswered, the monitor has a verdict, and
    // the adapter has meaningful steps to give — otherwise stay quiet.
    if !app.cap_prompt_visible() {
        return;
    }
    let Some(g) = app.charge_cap_guidance() else {
        return;
    };
    egui::TopBottomPanel::top("cap_setup_prompt").show(ctx, |ui| {
        ui.add_space(8.0);
        ui.heading("🛡 Set a charge limit on this machine");
        ui.add_space(2.0);
        ui.label(
            "Sitting at full charge ages this battery fastest. A charge cap \
             holds the pack below full whenever it is plugged in — the single \
             most effective setting you can change.",
        );
        ui.add_space(2.0);
        ui.label(format!("On this machine: {} — {}", g.tool, g.steps));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            match g.url {
                Some(url) => {
                    if ui.button(format!("Open {}", g.tool)).clicked() {
                        match crate::caps::open_guidance_url(url) {
                            Ok(()) => app.answer_cap_prompt(CapPromptAnswer::Acted),
                            Err(e) => app.last_error = Some(e),
                        }
                    }
                }
                None => {
                    if ui.button("Got it — I'll set it").clicked() {
                        app.answer_cap_prompt(CapPromptAnswer::Acted);
                    }
                }
            }
            if ui.button("Dismiss").clicked() {
                app.answer_cap_prompt(CapPromptAnswer::Dismissed);
            }
            ui.weak("Shown once — we will not ask again.");
        });
        ui.add_space(8.0);
    });
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
                    Line::new(PlotPoints::from_iter(temp.iter().copied())).color(score_color(75.0)),
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

/// FR-4 weekly digest — heat. Drawn only when the app has a report: the
/// FR-0 adapter reports temperature Unavailable or the stored history has
/// no temperature readings, this renders nothing at all — no heading, no
/// placeholder. Digest channel, never a notification: real-time heat
/// coaching stays with the sustained ≥45 °C alert.
fn draw_weekly_digest(ui: &mut egui::Ui, app: &App) {
    let Some(report) = app.weekly_heat_digest() else {
        return;
    };
    ui.add_space(8.0);
    ui.heading("Weekly digest — heat");
    ui.add_space(2.0);
    ui.label(report.digest_line());
    ui.weak(egui::RichText::new(format!(
        "Counted from {:.1} h of 1-minute history at ≥ {:.0} °C. Slow problem, so it is only reported here — the sustained 45 °C alert still fires in real time.",
        report.covered_hours(),
        crate::warm::WARM_THRESHOLD_C
    )).small());
}

/// FR-5 render gate: the card shows exactly while an unsaid advice is up.
/// Every real hide gate lives upstream in `App` — the deep-discharge
/// detector (advice::regular_deep_daytime_discharge), the adapter's
/// adaptive equivalent (`None` hides the feature entirely, e.g. a maker
/// module with no adaptive charging), and the persisted once-ever flag. The
/// card's message comes from the adapter too, so on every shipping platform
/// (all BehavioralOnly) it asks the user to check the setting rather than
/// claiming to know whether it is on.
fn full_charge_advice_visible(app: &App) -> bool {
    app.full_charge_advice.is_some()
}

/// FR-5 one-time advice card. Say-so-once UX: shown at most one time ever;
/// dismissing takes it down for the session and persists the shown flag, so
/// neither more arriving history nor an app restart can bring it back.
fn draw_full_charge_advice(ctx: &egui::Context, app: &mut App) {
    let Some(advice) = app.full_charge_advice.clone() else {
        return;
    };
    egui::TopBottomPanel::top("full_charge_advice_card").show(ctx, |ui| {
        // Top-down primitives only, dismiss button last: a horizontal row
        // mid-panel gets its height from the frame-1 panel-size hint and
        // anything after it can be culled before the hint settles.
        ui.colored_label(
            egui::Color32::from_rgb(120, 190, 240),
            "🔋 Adaptive charging — worth checking once",
        );
        ui.label(&advice.message);
        if let Some(url) = advice.url {
            ui.hyperlink(url);
        }
        ui.horizontal(|ui| {
            if ui.small_button("Got it").clicked() {
                app.dismiss_full_charge_advice();
            }
        });
        ui.add_space(6.0);
    });
}

/// Modal confirm: what gets deleted (only paths that exist), what does not,
/// and Cancel / Delete everything.
fn draw_uninstall_confirm(ctx: &egui::Context, app: &mut App) {
    let existing: Vec<String> = crate::uninstall::paths()
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();

    egui::Window::new("Uninstall Battery Viz?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.add_space(4.0);
            if existing.is_empty() {
                ui.weak("Nothing stored yet — there is no data to delete.");
            } else {
                ui.weak("This permanently deletes everything the app has stored:");
                for path in &existing {
                    ui.monospace(egui::RichText::new(format!("  {path}")).small());
                }
            }
            ui.add_space(6.0);
            ui.weak(
                egui::RichText::new(
                    "Not deleted: CSV exports you already saved elsewhere, and the app file \
                     itself — the next step gives you a copy-paste command for it.",
                )
                .small(),
            );
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.button("Cancel").clicked() {
                    app.cancel_uninstall();
                }
                let delete = ui.button(
                    egui::RichText::new("Delete everything")
                        .color(egui::Color32::from_rgb(235, 110, 90)),
                );
                if delete.clicked() {
                    app.confirm_uninstall();
                }
            });
        });
}

/// Modal finish card: per-path outcomes, the binary the app never deletes
/// itself, and a copyable command for the person to run. No auto-quit — the
/// person reads the card first.
fn draw_uninstall_finish(
    ctx: &egui::Context,
    app: &mut App,
    results: &[Result<std::path::PathBuf, String>],
) {
    let all_deleted = results.iter().all(Result::is_ok);

    egui::Window::new("Uninstall — what happened")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.add_space(4.0);
            if all_deleted {
                ui.colored_label(
                    egui::Color32::from_rgb(120, 200, 120),
                    "✅ All stored data deleted.",
                );
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(235, 110, 90),
                    "⚠ Some deletions failed:",
                );
            }
            for result in results {
                match result {
                    Ok(path) => {
                        ui.monospace(format!("✅ deleted {}", path.display()));
                    }
                    Err(e) => {
                        ui.monospace(format!("❌ {e}"));
                    }
                }
            }
            ui.add_space(8.0);
            ui.weak(
                "The app itself was never deleted (a running program must not remove its own \
                 file). Finish by running this:",
            );
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<app binary>".to_string());
            ui.monospace(&exe);
            let command = crate::uninstall::removal_command(&exe);
            ui.horizontal(|ui| {
                if ui.button("Copy removal command").clicked() {
                    ui.output_mut(|o| o.copied_text = command.clone());
                    app.uninstall_command_copied = true;
                }
                if app.uninstall_command_copied {
                    ui.weak("Copied ✓");
                }
            });
            ui.weak(egui::RichText::new(command.as_str()).small());
            ui.add_space(10.0);
            if ui.button("Quit Battery Viz").clicked() {
                app.quit_requested = true;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advice::SettingAdvice;

    fn test_app() -> App {
        let (_tx, rx) = crossbeam_channel::unbounded();
        App::with_history_path(rx, None)
    }

    #[test]
    fn advice_card_gate_is_exactly_an_unsaid_advice() {
        let mut app = test_app();
        assert!(
            !full_charge_advice_visible(&app),
            "no advice computed → no card (fresh install, no pattern, or already said)"
        );
        app.full_charge_advice = Some(SettingAdvice {
            message: "check the maker tool".into(),
            url: None,
        });
        assert!(
            full_charge_advice_visible(&app),
            "unsaid advice → card shows"
        );
        // Dismissal is the only way the card goes down mid-session.
        app.dismiss_full_charge_advice();
        assert!(!full_charge_advice_visible(&app), "dismissed → card hidden");
    }

    /// Every string painted into a frame (text galleys), for asserting what
    /// actually rendered — not just that the frame did not panic.
    fn painted_texts(out: &egui::FullOutput) -> String {
        let mut text = String::new();
        for clipped in &out.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                text.push_str(t.galley.text());
                text.push('\n');
            }
        }
        text
    }

    const CARD_HEADING: &str = "Adaptive charging — worth checking once";

    /// One full dashboard frame rendered headlessly (CPU-side tessellation).
    fn headless_frame(
        ctx: &egui::Context,
        app: &mut App,
        export_status: &mut Option<String>,
    ) -> egui::FullOutput {
        ctx.run(egui::RawInput::default(), |ctx| {
            draw(ctx, app, export_status);
        })
    }

    #[test]
    fn advice_card_draws_headless_only_while_gated_on() {
        // Headless egui run: tessellation is CPU-side, so the whole draw
        // path (card included) executes without a GPU. Two frames — panel
        // size hints settle on the second, so that is the frame asserted on.
        let ctx = egui::Context::default();
        let mut app = test_app();
        let mut export_status: Option<String> = None;
        let out_hidden = headless_frame(&ctx, &mut app, &mut export_status);
        assert!(
            !painted_texts(&out_hidden).contains(CARD_HEADING),
            "no card may render while the gate is off"
        );

        app.full_charge_advice = Some(SettingAdvice {
            message: "check the maker tool".into(),
            url: Some("https://example.com/adaptive-charging"),
        });
        // First frame after the gate turns on sizes the new panel; the next
        // frame is the settled one.
        headless_frame(&ctx, &mut app, &mut export_status);
        let out_shown = headless_frame(&ctx, &mut app, &mut export_status);
        let painted = painted_texts(&out_shown);
        assert!(
            painted.contains(CARD_HEADING),
            "card heading painted: {painted}"
        );
        assert!(
            painted.contains("check the maker tool"),
            "the adapter's guidance message is what the card shows; painted:\n{painted}"
        );
        assert!(
            painted.contains("Got it"),
            "the dismiss button renders; painted:\n{painted}"
        );
    }
}
