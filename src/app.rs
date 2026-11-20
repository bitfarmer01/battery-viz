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

/// M1: checkpoint the long-term store every N new minute-buckets. The old
/// per-bucket rewrite was ~1 GB/day at full retention; 15 minutes is a 15x
/// cut (~96 writes/day) while the worst-case crash loses 15 buckets of a
/// 10,080-bucket retention window (~0.15%). Every checkpoint is atomic, and
/// a graceful exit saves immediately, so a normal restart loses nothing.
pub const SAVE_INTERVAL_MINUTES: u32 = 15;

pub struct App {
    rx: Receiver<CollectorResult>,
    pub latest: Option<crate::collect::BatterySample>,
    pub benchmark: Option<BenchmarkResult>,
    pub history: History,
    /// Downsampled long-term store (1-minute buckets, 7-day retention).
    pub history_store: HistoryStore,
    /// Where the store persists; `None` when no platform data dir exists.
    history_path: Option<PathBuf>,
    /// Sticky dashboard notice: the persisted history could not be read (it
    /// was preserved as `history.json.corrupt`) — M2, never silent.
    pub history_load_error: Option<String>,
    /// New minute-buckets folded in since the last on-disk checkpoint (M1).
    buckets_since_save: u32,
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
    /// Uninstall flow: confirm panel open — nothing deleted yet.
    pub uninstall_requested: bool,
    /// Per-path deletion outcomes once the user confirmed; `Some` also
    /// drives the finish card.
    pub uninstall_results: Option<Vec<Result<PathBuf, String>>>,
    /// Set when the finish card's "Copy removal command" was used, for the
    /// inline confirmation.
    pub uninstall_command_copied: bool,
    /// Set by the finish card's Quit button: the process ends through
    /// eframe's normal teardown.
    pub quit_requested: bool,
    /// Set BEFORE any deletion on confirm: the collector channel is still
    /// drained, but nothing is recorded or alerted — a live sampler must
    /// never re-write history mid-delete.
    sampler_stopped: bool,
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
        Self::with_history_path(rx, default_history_path())
    }

    /// Constructor with an explicit persistence path (tests inject a temp
    /// dir; production passes [`default_history_path`]).
    pub fn with_history_path(rx: Receiver<CollectorResult>, history_path: Option<PathBuf>) -> Self {
        // One-time platform setup: on Windows this registers the toast AUMID
        // so later toasts are not suppressed by the OS.
        notify::init();
        // Load persisted history on startup. A missing file starts fresh; an
        // unreadable one starts fresh too but is reported (M2) — the file was
        // preserved as `history.json.corrupt`, never silently overwritten.
        let load = match &history_path {
            Some(path) => HistoryStore::load(path),
            None => crate::history::LoadResult::default(),
        };
        let history_load_error = load.load_error.map(|e| {
            format!(
                "prior history could not be read ({}) — starting fresh; preserved at {}",
                e.message,
                e.backup_path.display()
            )
        });
        // Menu-bar agent mode (macOS, or any host started hidden): the window
        // exists but is not shown at launch; the tray / `--hidden` toggles it.
        let start_hidden =
            cfg!(target_os = "macos") || std::env::var("BATTERY_VIZ_START_HIDDEN").is_ok();
        App {
            rx,
            latest: None,
            benchmark: None,
            history: History::new(),
            history_store: load.store,
            history_path,
            history_load_error,
            buckets_since_save: 0,
            last_error: None,
            last_notify_error: None,
            frames_rendered: 0,
            alert_engine: AlertEngine::new(),
            alerts_paused: false,
            last_alert: None,
            window_visible: !start_hidden,
            uninstall_requested: false,
            uninstall_results: None,
            uninstall_command_copied: false,
            quit_requested: false,
            sampler_stopped: false,
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
        // Uninstall confirmed: the channel is still drained (no backlog)
        // but nothing is recorded or alerted — a dead sampler must not
        // re-write history mid-delete. Stop-then-delete ordering.
        if self.sampler_stopped {
            return;
        }
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

    /// Open the in-app confirm panel. No deletion happens here.
    pub fn request_uninstall(&mut self) {
        self.uninstall_requested = true;
        self.uninstall_command_copied = false;
    }

    /// Close the confirm panel; nothing was deleted.
    pub fn cancel_uninstall(&mut self) {
        self.uninstall_requested = false;
    }

    /// Confirm path: sampling and alerting stop FIRST, then every artifact
    /// is deleted. The binary itself is never touched — the finish card
    /// hands the person a copyable removal command instead.
    pub fn confirm_uninstall(&mut self) {
        self.sampler_stopped = true;
        let targets = self.uninstall_targets();
        self.uninstall_results = Some(crate::uninstall::remove_paths(targets));
        self.uninstall_requested = false;
    }

    /// Every artifact this installation created, derived from the history
    /// path the app was booted with (the seam tests point at a temp dir).
    fn uninstall_targets(&self) -> Vec<PathBuf> {
        match &self.history_path {
            Some(history) => crate::uninstall::artifacts_for(history),
            None => Vec::new(),
        }
    }

    /// Fold a sample into the long-term store. Checkpoints are written on a
    /// bounded cadence (every `SAVE_INTERVAL_MINUTES` buckets, plus once on
    /// graceful exit via the UI's `on_exit`) — the audit's per-bucket rewrite
    /// cost ~1 GB/day at full retention. Persistence failures surface in the
    /// header, they never take the dashboard down.
    fn record_history(&mut self, sample: &crate::collect::BatterySample) {
        let created_new_bucket = self.history_store.push(sample, crate::history::unix_now());
        self.checkpoint_history(created_new_bucket);
    }

    /// Count new buckets and checkpoint on the cadence boundary.
    fn checkpoint_history(&mut self, created_new_bucket: bool) {
        if !created_new_bucket {
            return;
        }
        self.buckets_since_save += 1;
        if self.buckets_since_save >= SAVE_INTERVAL_MINUTES {
            self.save_history_now();
        }
    }

    /// Checkpoint the store to disk immediately (cadence boundary or graceful
    /// exit). A failed checkpoint resets the cadence so the next attempt is a
    /// full interval away, not one bucket.
    pub fn save_history_now(&mut self) {
        // Uninstall confirmed: the finish card's Quit reaches this through
        // the UI's `on_exit` — a final checkpoint must never resurrect the
        // data that was just deleted.
        if self.sampler_stopped {
            return;
        }
        self.buckets_since_save = 0;
        if let Some(path) = &self.history_path {
            if let Err(e) = self.history_store.save(path) {
                self.last_error = Some(e);
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

#[cfg(test)]
mod tests {
    use super::*;
    fn tagged_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "battery-viz-app-tests-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn app_at(path: Option<PathBuf>) -> App {
        let (_tx, rx) = crossbeam_channel::unbounded();
        App::with_history_path(rx, path)
    }

    fn sample(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            ..Default::default()
        }
    }

    #[test]
    fn load_failure_produces_visible_notice_and_starts_fresh() {
        let dir = tagged_dir("load-notice");
        let path = dir.join("history.json");
        std::fs::write(&path, "{").unwrap();
        let app = app_at(Some(path.clone()));
        assert!(
            app.history_store.buckets.is_empty(),
            "corrupt file starts fresh"
        );
        let notice = app
            .history_load_error
            .as_ref()
            .expect("load failure surfaced as a dashboard notice");
        assert!(notice.contains("could not be read"), "{notice}");
        assert!(notice.contains("history.json.corrupt"), "{notice}");
        // The unreadable file was moved aside, not left in place to be
        // overwritten by the next checkpoint.
        assert!(!path.exists());
        assert!(dir.join("history.json.corrupt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoints_every_fifteenth_bucket_and_not_before() {
        let dir = tagged_dir("cadence");
        let path = dir.join("history.json");
        let mut app = app_at(Some(path.clone()));
        let minute0 = 1_800 * 60; // any minute-aligned unix time
        for m in 0..(SAVE_INTERVAL_MINUTES as u64 - 1) {
            let created = app.history_store.push(&sample(50.0), minute0 + m * 60);
            app.checkpoint_history(created);
            assert!(!path.exists(), "no checkpoint before the interval, m={m}");
        }
        let created = app
            .history_store
            .push(&sample(50.0), minute0 + SAVE_INTERVAL_MINUTES as u64 * 60);
        app.checkpoint_history(created);
        assert!(path.exists(), "checkpoint written at the interval boundary");
        // Atomic save: the temp sibling was consumed by the rename.
        assert!(!dir.join("history.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graceful_exit_checkpoint_writes_the_store() {
        let dir = tagged_dir("exit-save");
        let path = dir.join("history.json");
        let mut app = app_at(Some(path.clone()));
        let created = app.history_store.push(&sample(50.0), 1_800 * 60);
        app.checkpoint_history(created);
        assert!(!path.exists(), "nothing persisted before the interval");
        app.save_history_now();
        assert!(path.exists(), "graceful exit persists the store");
        assert_eq!(HistoryStore::load(&path).store, app.history_store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn confirmed_uninstall_stops_sampling_before_deleting() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let path = tagged_dir("stop-then-delete").join("history.json");
        let mut app = App::with_history_path(rx, Some(path.clone()));

        // A sample arrives and is checkpointed (the 15-bucket cadence does
        // not save one bucket on its own, so force the checkpoint).
        tx.send(Ok(sample(80.0))).unwrap();
        app.update();
        app.save_history_now();
        assert!(path.exists(), "history persisted before uninstall");

        // Confirm: sampling stops first, then the data is deleted.
        app.confirm_uninstall();
        assert!(!path.exists(), "history deleted on confirm");
        let results = app.uninstall_results.clone().expect("per-path results");
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        // A late sample is drained but never re-creates the deleted data —
        // the stopped sampler cannot checkpoint again.
        tx.send(Ok(sample(80.0))).unwrap();
        app.update();
        assert!(!path.exists(), "no re-write after uninstall confirmed");
    }

    #[test]
    fn graceful_exit_after_uninstall_never_resurrects_data() {
        let path = tagged_dir("exit-after-uninstall").join("history.json");
        let mut app = app_at(Some(path.clone()));
        let created = app.history_store.push(&sample(50.0), 1_800 * 60);
        app.checkpoint_history(created);

        // Uninstall deletes the data; the finish card's Quit then runs the
        // UI's on_exit -> save_history_now. That final checkpoint must not
        // write the in-memory (pre-uninstall) history back to disk.
        app.confirm_uninstall();
        assert!(!path.exists(), "history deleted on confirm");
        app.save_history_now();
        assert!(!path.exists(), "exit checkpoint must not resurrect data");
    }

    #[test]
    fn cancel_uninstall_changes_nothing() {
        let path = tagged_dir("cancel").join("history.json");
        let mut app = app_at(Some(path.clone()));

        app.request_uninstall();
        assert!(app.uninstall_requested);
        app.cancel_uninstall();
        assert!(!app.uninstall_requested);
        assert!(app.uninstall_results.is_none());
        assert!(!path.exists(), "cancel never deletes");
    }
}
