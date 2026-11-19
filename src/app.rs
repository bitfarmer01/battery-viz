//! Application state: drains the collector channel, keeps a bounded history
//! ring, and re-evaluates the benchmark as samples arrive.

use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::alert::{Alert, AlertEngine, ClockInput};
use crate::benchmark::{self, BenchmarkResult};
use crate::clock;
use crate::collect::CollectorResult;
use crate::history::HistoryStore;
use crate::notify;

/// How many samples of history the dashboard keeps (at 1 Hz, 10 minutes).
pub const HISTORY_LEN: usize = 600;

pub struct App {
    rx: Receiver<CollectorResult>,
    pub latest: Option<crate::collect::BatterySample>,
    pub benchmark: Option<BenchmarkResult>,
    pub history: History,
    /// Downsampled long-term store (1-minute buckets, 7-day retention).
    pub history_store: HistoryStore,
    /// Where the store persists; `None` when no platform data dir exists.
    history_path: Option<PathBuf>,
    pub last_error: Option<String>,
    /// Most recent platform-notification delivery failure (osascript /
    /// PowerShell / AUMID registration / tray build), shown in the header so
    /// a dead alert path is never silent.
    pub last_notify_error: Option<String>,
    pub frames_rendered: u64,
    /// Pure alert state machine; advanced once per incoming sample (1 Hz).
    alert_engine: AlertEngine,
    /// Master mute for every nudge (tray menu / header toggle).
    pub alerts_paused: bool,
    /// Most recent alert, for the dashboard banner.
    pub last_alert: Option<Alert>,
    /// True while the window is shown. While hidden (menu-bar agent mode),
    /// no egui frame is drawn and repaints fall back to the 1 Hz sample tick.
    pub window_visible: bool,
    /// Monotonic origin for the engine's injected clock.
    start: std::time::Instant,
}

/// Fixed-capacity ring of (time-seconds-since-start, sample) pairs.
pub struct History {
    pub t: Vec<f64>,
    pub soc: Vec<f64>,
    pub temp: Vec<f64>,
    pub watts: Vec<f64>,
    start: std::time::Instant,
}

impl History {
    fn new() -> Self {
        History {
            t: Vec::with_capacity(HISTORY_LEN),
            soc: Vec::with_capacity(HISTORY_LEN),
            temp: Vec::with_capacity(HISTORY_LEN),
            watts: Vec::with_capacity(HISTORY_LEN),
            start: std::time::Instant::now(),
        }
    }

    fn push(&mut self, s: &crate::collect::BatterySample) {
        let t = self.start.elapsed().as_secs_f64();
        if self.t.len() == HISTORY_LEN {
            self.t.remove(0);
            self.soc.remove(0);
            self.temp.remove(0);
            self.watts.remove(0);
        }
        self.t.push(t);
        self.soc.push(s.state_of_charge.unwrap_or(f64::NAN));
        self.temp.push(s.temperature_c.unwrap_or(f64::NAN));
        self.watts.push(s.power_w.unwrap_or(f64::NAN));
    }
}

/// Where the long-term history persists: `<data dir>/battery-viz/history.json`
/// (macOS `~/Library/Application Support`, Windows `%APPDATA%`, XDG data dir).
pub fn default_history_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("battery-viz").join("history.json"))
}

impl App {
    pub fn new(rx: Receiver<CollectorResult>) -> Self {
        // One-time platform setup: on Windows this registers the toast AUMID
        // so later toasts are not suppressed by the OS.
        notify::init();
        // Load persisted history on startup; a missing or corrupt file starts
        // fresh (handled inside `HistoryStore::load`).
        let history_path = default_history_path();
        let history_store = match &history_path {
            Some(path) => HistoryStore::load(path),
            None => HistoryStore::default(),
        };
        // Menu-bar agent mode (macOS, or any host started hidden): the window
        // exists but is not shown at launch; the tray / `--hidden` toggles it.
        let start_hidden =
            cfg!(target_os = "macos") || std::env::var("BATTERY_VIZ_START_HIDDEN").is_ok();
        App {
            rx,
            latest: None,
            benchmark: None,
            history: History::new(),
            history_store,
            history_path,
            last_error: None,
            last_notify_error: None,
            frames_rendered: 0,
            alert_engine: AlertEngine::new(),
            alerts_paused: false,
            last_alert: None,
            window_visible: !start_hidden,
            start: std::time::Instant::now(),
        }
    }

    /// Drain pending samples (non-blocking) and refresh derived state.
    pub fn update(&mut self) {
        self.frames_rendered += 1;
        // Surface any platform-notification failure observed since the last
        // tick (osascript / PowerShell / AUMID / tray build). Sticky: it
        // stays visible until the next failure replaces it.
        if let Some(e) = notify::take_notify_error() {
            self.last_notify_error = Some(e);
        }
        // Drain first, then mutate: the channel iterator borrows `self.rx`,
        // so `&mut self` methods cannot run inside the loop.
        let samples: Vec<crate::collect::BatterySample> = self
            .rx
            .try_iter()
            .filter_map(|result| match result {
                Ok(sample) => Some(sample),
                Err(e) => {
                    self.last_error = Some(e);
                    None
                }
            })
            .collect();
        for sample in &samples {
            self.history.push(sample);
            self.record_history(sample);
            self.tick_alerts(sample);
        }
        if let Some(sample) = samples.into_iter().last() {
            self.benchmark = Some(benchmark::evaluate(&sample));
            self.latest = Some(sample);
        }
    }

    /// Advance the alert engine by one sample and deliver a nudge when it
    /// fires. Pausing resets the engine: sustain windows from before the
    /// pause cannot fire stale alerts on resume.
    fn tick_alerts(&mut self, sample: &crate::collect::BatterySample) {
        if self.alerts_paused {
            return;
        }
        let local = clock::local_now();
        let tick = ClockInput {
            now_s: self.start.elapsed().as_secs_f64(),
            hour: local.hour,
            day: local.day,
        };
        if let Some(alert) = self.alert_engine.tick(sample, tick) {
            self.last_alert = Some(alert);
            notify::send(alert.kind.label(), alert.message);
        }
    }

    /// Pause/unpause coaching nudges. Resetting the engine means a pause of
    /// any length never surfaces a backlog of stale conditions on resume.
    pub fn set_alerts_paused(&mut self, paused: bool) {
        if self.alerts_paused != paused {
            self.alerts_paused = paused;
            self.alert_engine = AlertEngine::new();
        }
    }

    /// Fold a sample into the long-term store; persist after each NEW bucket
    /// (write-behind on the UI thread — the spec pins one write per bucket,
    /// no background thread). Persistence failures surface in the header,
    /// they never take the dashboard down.
    fn record_history(&mut self, sample: &crate::collect::BatterySample) {
        let created_new_bucket = self.history_store.push(sample, crate::history::unix_now());
        if created_new_bucket {
            if let Some(path) = &self.history_path {
                if let Err(e) = self.history_store.save(path) {
                    self.last_error = Some(e);
                }
            }
        }
    }

    /// Write the retained window to `<data dir>/battery-viz-export.csv`.
    pub fn export_csv(&self) -> Result<PathBuf, String> {
        let path = dirs::data_dir()
            .ok_or_else(|| "no platform data directory".to_string())?
            .join("battery-viz-export.csv");
        std::fs::write(&path, self.history_store.to_csv())
            .map_err(|e| format!("write CSV: {e}"))?;
        Ok(path)
    }

    /// Wake interval for eframe. Visible: paint continuously for smooth
    /// animations. Hidden (menu-bar agent): the sample tick at 1 Hz is the
    /// only wake — no GPU frame, no egui draw, engine still advances.
    pub fn repaint_interval(&self) -> Duration {
        if self.window_visible {
            Duration::from_millis(16)
        } else {
            Duration::from_secs(1)
        }
    }
}
