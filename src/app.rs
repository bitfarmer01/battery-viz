//! Application state: drains the collector channel, keeps a bounded history
//! ring, and re-evaluates the benchmark as samples arrive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::advice::{self, SettingAdvice};
use crate::alert::{Alert, AlertEngine, ClockInput};
use crate::benchmark::{self, BenchmarkResult};
use crate::capdetect::{self, CapMonitor, CapPromptAnswer, PromptRecord};
use crate::caps::{Capabilities, MetricId, SettingGuidance, SettingKind};
use crate::clock;
use crate::collect::CollectorResult;
use crate::digest::{DigestInputs, DigestSchedule, WeeklyDigest};
use crate::history::HistoryStore;
use crate::notify;
use crate::outcomes::OutcomeStore;
use crate::record::DailyRecord;
use crate::warm::{self, WarmHoursReport};

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
    /// FR-5 one-time advice card, or `None` when the machine has no
    /// adaptive-charging equivalent to point at, the stored history does not
    /// show the deep-discharge pattern, or the advice was already said.
    pub full_charge_advice: Option<SettingAdvice>,
    /// The adapter's answer for adaptive charging, resolved once at startup
    /// (it is static per machine): `None` hides the feature entirely.
    adaptive_advice: Option<SettingAdvice>,
    /// The FR-5 advice has been said; persisted so it is never said again.
    advice_shown: bool,
    /// Where the one-shot advice flag persists; `None` when no platform
    /// data dir exists (the card never shows).
    advice_state_path: Option<PathBuf>,
    /// Long-term daily record (FR-7): one summary row per local day,
    /// kept indefinitely within [`crate::record::MAX_DAYS`].
    pub daily_record: DailyRecord,
    /// Where the record persists; derived from the history path so tests
    /// and the uninstall seam point at the same data dir.
    record_path: Option<PathBuf>,
    /// Sticky dashboard notice: the persisted record could not be read.
    pub record_load_error: Option<String>,
    /// FR-6 outcome log: every alert fire, whether the targeted condition
    /// ended inside the outcome window, and per-kind retirement state
    /// (ignored N times in a row → the kind stops firing in real time).
    pub outcomes: OutcomeStore,
    /// Where the outcome store persists; derived from the history path so
    /// tests and the uninstall seam point at the same data dir.
    outcomes_path: Option<PathBuf>,
    /// Sticky dashboard notice: the persisted outcome store could not be
    /// read (it was preserved as `alert-outcomes.json.corrupt`) — never
    /// silent (M2 convention).
    pub outcomes_load_error: Option<String>,
    /// FR-9 weekly digest schedule: the local day the last digest went
    /// out, persisted so "at most one a week" survives restarts.
    pub digest_schedule: DigestSchedule,
    /// Where the schedule persists; `None` keeps it in memory only.
    digest_state_path: Option<PathBuf>,
    /// Sticky dashboard notice: the schedule file could not be read (it
    /// was preserved as `digest-state.json.corrupt`) — never silent.
    pub digest_load_error: Option<String>,
    /// A schedule save that failed at send time is retried every tick
    /// until it succeeds: the in-memory day already blocks a re-send, but
    /// until the disk agrees, a restart would re-arm the digest early.
    digest_save_pending: bool,
    /// FR-0 answers, resolved once at startup like
    /// `temperature_available`: they gate the digest's high-charge and
    /// cycling contributors (an Unavailable metric is absent, never zero).
    soc_available: bool,
    cycles_available: bool,
    /// The advice-state marker file existed but could not be read or was not
    /// the expected marker: it was quarantined and the advice stays hidden —
    /// never silent (M2 convention).
    pub advice_load_error: Option<String>,
    /// First-run import (FR-8), still running in the background: the
    /// capacity rows (Windows: `powercfg /batteryreport`) plus, when the
    /// machine holds one, the battery's own lifetime record (macOS: the
    /// ioreg `LifetimeData` counters) — both through the capability
    /// adapter. `None` when there is nothing to wait for — record not
    /// empty (not a first run), no data dir, or the import already
    /// applied.
    pending_record_seed: Option<(std::sync::mpsc::Receiver<crate::first_run::Import>, PathBuf)>,
    /// FR-8: the day-one note from the first-run import (what the machine
    /// already held), shown in the header for the session. `None` when
    /// nothing was imported — never a placeholder, never an error.
    pub first_run_note: Option<String>,
    /// New minute-buckets folded in since the last on-disk checkpoint (M1).
    buckets_since_save: u32,
    /// Monotonic time of the last dwell-accumulated sample (audit H4);
    /// `None` until the first sample — a first-ever sample credits nothing.
    dwell_prev_s: Option<f64>,
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
    // Features never sniff the platform; they ask the FR-0 adapter.
    temperature_available: bool,
    /// FR-1: behavioural charge-cap detector, fed every sample.
    pub cap_monitor: CapMonitor,
    /// FR-1: one-time setup prompt record. Once answered it never resets,
    /// so the prompt shows at most once per machine.
    cap_prompt_record: PromptRecord,
    /// Where the prompt record persists; `None` keeps it in memory only.
    cap_prompt_path: Option<PathBuf>,
    /// Corrupt or unreadable charge-cap state — sticky notice, surfaced in
    /// the header (the `history_load_error` convention; never silent).
    pub cap_setup_load_error: Option<String>,
    /// The stretch state as last written to disk, so checkpointing can tell
    /// "unchanged since the last persist" from "time to persist again".
    cap_stretch_persisted: Option<capdetect::StretchState>,
    /// FR-3: the settled cap verdict as last written to disk. A transition
    /// (cap detected or gone) persists immediately, so charge-alert
    /// retirement survives a restart instead of re-detecting for 3 h.
    cap_verdict_persisted: Option<capdetect::CapStatus>,
    /// FR-0 adapter for this machine: detectability, guidance, maker.
    /// Feature code asks it, never the OS directly.
    capabilities: Box<dyn Capabilities>,
    /// Test-only fixed local-clock reading for the alert tick (FR-6
    /// outcomes are wall-clock driven); `None` reads the OS clock.
    #[cfg(test)]
    test_clock: Option<clock::LocalNow>,
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

/// On-disk marker for FR-5's one-shot flag (sibling of the history file).
const ADVICE_STATE_FILE: &str = "advice-state.json";
const ADVICE_SHOWN_MARKER: &str = "{\"adaptive_charging_advice_shown\":true}";

/// Read the FR-5 one-shot marker: `(said, notice)`. A missing file is a
/// fresh start. An existing but unreadable or unrecognized marker counts as
/// said — deliberate: the once-ever guarantee must not silently reset on a
/// crash — but never silently: the bytes are quarantined like history
/// (M1/M2) and the notice is surfaced on the dashboard, sticky.
fn read_advice_shown(path: &Path) -> (bool, Option<String>) {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, None),
        Ok(text) if text.trim() == ADVICE_SHOWN_MARKER => (true, None),
        read => {
            let reason = match read {
                Ok(_) => "not a recognized marker".to_string(),
                Err(e) => format!("unreadable: {e}"),
            };
            let err = crate::history::quarantine(path, format!("advice state {reason}"));
            (
                true,
                Some(format!(
                    "advice state could not be read ({}) — the one-time advice \
                     will not be offered again; preserved at {}",
                    err.message,
                    err.backup_path.display()
                )),
            )
        }
    }
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
        // The daily record (FR-7) lives next to the history file, same data
        // dir, so the uninstall listing covers both with one deletion.
        let (daily_record, record_path, record_load_error, pending_record_seed) =
            match &history_path {
                Some(path) => {
                    let record_path = path.with_file_name("daily-record.json");
                    let record_load = DailyRecord::load(&record_path);
                    let record_load_error = record_load.load_error.as_ref().map(|e| {
                    format!(
                        "prior daily record could not be read ({}) — starting fresh; preserved at {}",
                        e.message,
                        e.backup_path.display()
                    )
                });
                    let record = record_load.record;
                    // First run: import the long-term record the machine
                    // already holds, asked through the capability adapter
                    // (Windows: powercfg /batteryreport; macOS: the ioreg
                    // LifetimeData counters). The Windows answer needs a
                    // subprocess that can take seconds, so it runs on a
                    // background thread and is applied whenever it lands
                    // (`poll_record_seed` on a later frame) — window creation
                    // must not stall on first run. Until then — or when the
                    // adapter reports nothing — the record is simply empty;
                    // nothing is invented.
                    let pending_record_seed = if record.rows.is_empty() {
                        let record_path = record_path.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let caps = crate::caps::platform_capabilities();
                            let _ = tx.send(crate::first_run::Import {
                                seeds: caps.capacity_seed(),
                                lifetime: caps.lifetime_record(),
                            });
                        });
                        Some((rx, record_path))
                    } else {
                        None
                    };
                    (
                        record,
                        Some(record_path),
                        record_load_error,
                        pending_record_seed,
                    )
                }
                None => (DailyRecord::default(), None, None, None),
            };
        // FR-6: the alert outcome store lives next to the history file,
        // same data dir, so the uninstall listing covers it with the rest.
        let (outcomes_path, outcomes, outcomes_load_error) = match &history_path {
            Some(path) => {
                let outcomes_path = path.with_file_name("alert-outcomes.json");
                match OutcomeStore::load(&outcomes_path) {
                    Ok(store) => (Some(outcomes_path), store, None),
                    Err(err) => (
                        Some(outcomes_path),
                        OutcomeStore::new(),
                        Some(format!(
                            "prior alert outcomes could not be read ({}) — starting fresh; preserved at {}",
                            err.message,
                            err.backup_path.display()
                        )),
                    ),
                }
            }
            None => (None, OutcomeStore::new(), None),
        };
        // FR-9: the weekly digest schedule lives next to the history file
        // (same data dir, so the uninstall listing covers it with the
        // rest). A fresh install anchors on its first day: the first
        // digest is due a week later, when the record has a week of
        // samples to speak from.
        let digest_state_path = history_path
            .as_ref()
            .map(|p| p.with_file_name(crate::digest::DIGEST_STATE_FILE));
        let (digest_schedule, digest_load_error) =
            DigestSchedule::load(digest_state_path.as_deref(), clock::local_now().day);
        // Menu-bar agent mode (macOS, or any host started hidden): the window
        // exists but is not shown at launch; the tray / `--hidden` toggles it.
        let start_hidden =
            cfg!(target_os = "macos") || std::env::var("BATTERY_VIZ_START_HIDDEN").is_ok();
        // FR-5 one-shot flag: a marker file beside the history store. Its
        // absence (fresh start, no data dir) simply means the advice has
        // not been said yet.
        let advice_state_path = history_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join(ADVICE_STATE_FILE));
        let (advice_shown, advice_load_error) = advice_state_path
            .as_deref()
            .map(read_advice_shown)
            .unwrap_or((false, None));
        // FR-1: one-time charge-cap setup prompt record + detection stretch,
        // persisted next to the history store so "at most once ever" and a
        // running detection both survive restarts.
        let cap_prompt_path = history_path.as_ref().map(|p| capdetect::prompt_path_for(p));
        let (cap_prompt_record, cap_setup_load_error) =
            capdetect::load_record(cap_prompt_path.as_deref());
        // FR-0 adapter for this machine.
        let capabilities = crate::caps::platform_capabilities();
        let mut cap_monitor = CapMonitor::for_adapter(capabilities.as_ref());
        // Resume a detection stretch from a previous session, so a capped
        // machine can resolve across restarts instead of starting over.
        // FR-3: a settled verdict resumes first — retirement must hold from
        // the first tick, not after a 3 h re-detection.
        let cap_stretch_persisted = cap_prompt_record.stretch;
        if let Some(verdict) = cap_prompt_record.verdict {
            cap_monitor.restore_verdict(verdict);
        }
        if let Some(stretch) = cap_prompt_record.stretch {
            cap_monitor.restore_stretch(stretch, crate::history::unix_now());
        }
        let cap_verdict_persisted = cap_prompt_record.verdict;
        let mut app = App {
            rx,
            latest: None,
            benchmark: None,
            history: History::new(),
            history_store: load.store,
            history_path,
            history_load_error,
            full_charge_advice: None,
            // Static per machine: resolve the adapter once, not per recompute
            // (on Windows a fresh adapter means a fresh PowerShell query).
            adaptive_advice: advice::adaptive_charging_advice(
                crate::caps::platform_capabilities().as_ref(),
            ),
            advice_shown,
            advice_state_path,
            daily_record,
            record_path,
            record_load_error,
            outcomes,
            outcomes_path,
            outcomes_load_error,
            digest_schedule,
            digest_state_path,
            digest_load_error,
            digest_save_pending: false,
            soc_available: capabilities
                .readable_metrics()
                .contains(&MetricId::StateOfCharge),
            cycles_available: capabilities
                .readable_metrics()
                .contains(&MetricId::CycleCount),
            advice_load_error,
            pending_record_seed,
            first_run_note: None,
            buckets_since_save: 0,
            dwell_prev_s: None,
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
            // Features never sniff the platform; they ask the FR-0 adapter.
            temperature_available: crate::caps::platform_capabilities()
                .readable_metrics()
                .contains(&MetricId::Temperature),
            cap_monitor,
            cap_prompt_record,
            cap_prompt_path,
            cap_setup_load_error,
            cap_stretch_persisted,
            cap_verdict_persisted,
            capabilities,
            #[cfg(test)]
            test_clock: None,
            start: std::time::Instant::now(),
        };
        app.recompute_full_charge_advice();
        app
    }

    /// Apply the deferred first-run capacity seed when the background
    /// report lands. A cheap no-op on every frame until then; the
    /// adapter's empty answer (macOS / simulator have no report to ask)
    /// leaves the record empty — nothing is invented.
    fn poll_record_seed(&mut self) {
        let Some((rx, record_path)) = &self.pending_record_seed else {
            return;
        };
        match rx.try_recv() {
            Ok(import) => {
                let record_path = record_path.clone();
                self.pending_record_seed = None;
                // Real samples may have folded in while the report ran;
                // seed() fills only days with no row, so nothing sampled
                // is overwritten either way.
                if !import.seeds.is_empty() {
                    self.daily_record.seed(&import.seeds);
                    if let Err(e) = self.daily_record.save(&record_path) {
                        self.record_load_error =
                            Some(format!("seeded daily record could not be saved ({e})"));
                    }
                }
                // FR-8: the day-one note from whatever the machine already
                // held. Nothing imported — no note, no error, no placeholder.
                self.first_run_note =
                    crate::first_run::first_run_note(&self.daily_record, import.lifetime);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_record_seed = None;
            }
        }
    }

    /// Drain pending samples (non-blocking) and refresh derived state.
    pub fn update(&mut self) {
        self.frames_rendered += 1;
        // First-run capacity seed may have landed since the last frame.
        self.poll_record_seed();
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
            // Audit H4: fold the interval into the persisted high-charge
            // dwell total, then stamp the accumulated value onto the sample
            // that feeds the benchmark, the gauge, and the capability report.
            let now_s = self.start.elapsed().as_secs_f64();
            let credit = crate::dwell::dwell_credit(self.dwell_prev_s, now_s, sample);
            self.history_store.add_dwell(credit);
            self.dwell_prev_s = Some(now_s);
            self.record_daily(sample);
            self.cap_monitor.observe(sample, crate::history::unix_now());
            self.checkpoint_cap_state();
            self.observe_alert_outcomes(sample);
            self.tick_alerts(sample);
        }
        if let Some(mut sample) = samples.into_iter().last() {
            sample.high_soc_dwell_s = self.history_store.dwell_s;
            self.benchmark = Some(benchmark::evaluate(&sample));
            self.latest = Some(sample);
        }
        self.tick_digest();
    }

    /// FR-4: the week's warm-hour digest, or `None` when the feature hides
    /// itself — the adapter reports temperature Unavailable, or the stored
    /// history holds no temperature readings in the window. Digest-channel
    /// data: never delivered as a real-time alert (the sustained ≥45 °C
    /// HighTemp alert in `alert` remains the only real-time heat channel).
    pub fn weekly_heat_digest(&self) -> Option<WarmHoursReport> {
        if !self.temperature_available {
            return None;
        }
        let now = crate::history::unix_now();
        warm::weekly_report(&self.history_store, now / 60, clock::utc_offset_hours(now))
    }

    /// Local wall-clock read for the alert tick; tests pin it so outcome
    /// windows and day rollovers are deterministic.
    fn local_clock(&self) -> clock::LocalNow {
        #[cfg(test)]
        if let Some(pinned) = self.test_clock {
            return pinned;
        }
        clock::local_now()
    }

    /// FR-6: resolve open outcome watches against this sample — acted on
    /// when the targeted condition ends inside the window, ignored when the
    /// window runs out. Runs even while alerts are paused: a pending watch
    /// must resolve regardless of whether new fires are allowed. Any change
    /// checkpoints the store immediately (fires are rare; the file is tiny).
    fn observe_alert_outcomes(&mut self, sample: &crate::collect::BatterySample) {
        let now = self.local_clock().unix_secs.max(0) as u64;
        if self.outcomes.observe(sample, now) {
            self.save_outcomes_now();
        }
    }

    /// Checkpoint the FR-6 outcome store.
    fn save_outcomes_now(&mut self) {
        if let Some(path) = &self.outcomes_path {
            if let Err(e) = self.outcomes.save(path) {
                self.last_error = Some(format!("alert outcomes could not be saved: {e}"));
            }
        }
    }

    /// Advance the alert engine by one sample and deliver a nudge when it
    /// fires. Pausing resets the engine: sustain windows from before the
    /// pause cannot fire stale alerts on resume. FR-3: the charge coaches
    /// are gated on the live cap verdict every tick — a detected cap
    /// retires DwellCoach/BandCoach, a disappeared cap re-arms them.
    fn tick_alerts(&mut self, sample: &crate::collect::BatterySample) {
        if self.alerts_paused {
            return;
        }
        let charge_coaching = self.alert_engine_charge_coaching_enabled();
        self.alert_engine
            .set_charge_coaching_enabled(charge_coaching);
        let local = self.local_clock();
        let tick = ClockInput {
            now_s: self.start.elapsed().as_secs_f64(),
            hour: local.hour,
            day: local.day,
        };
        if let Some(alert) = self.alert_engine.tick(sample, tick) {
            // FR-6: the outcome store gates delivery — a kind retired for
            // repeated ignores stops firing in real time. The fire is still
            // logged (undelivered) so the weekly digest can see how often
            // the condition recurs.
            let now = local.unix_secs.max(0) as u64;
            if self.outcomes.on_fire(alert.kind, local.day, now) {
                self.last_alert = Some(alert);
                notify::send(alert.kind.label(), alert.message);
            }
            self.save_outcomes_now();
        }
    }

    /// FR-9: deliver the weekly digest when it is due — at most one
    /// notification a week, never during quiet hours, and never a
    /// placeholder: a week with nothing recorded has nothing to say, so
    /// the digest simply waits for a week that has data. Paused alerts
    /// defer the digest too: the mute covers every nudge, and the schedule
    /// keeps its one-a-week guarantee across the deferral.
    fn tick_digest(&mut self) {
        // A save that failed at send time retries here every tick — ahead
        // of the pause check, because closing the double-send window is
        // bookkeeping, not a nudge.
        self.retry_pending_digest_save();
        if self.alerts_paused {
            return;
        }
        let local = self.local_clock();
        if !self.digest_schedule.due(local.hour, local.day) {
            return;
        }
        if let Some(digest) = self.weekly_digest() {
            notify::send("Weekly digest", &digest.notification_body());
            // Persist before the notification is even visible: a crash
            // right after sending must never double-send next week.
            // Send-before-persist remains an accepted tradeoff (PR #22
            // review, advisory-1): persisting first would risk losing the
            // digest entirely on a crash, while a crash between the two
            // calls costs at most one duplicate next launch. A persist
            // that outright fails is retried every tick below, which
            // shrinks the stale-disk window from a week to one tick.
            if let Err(e) = self
                .digest_schedule
                .mark_sent(self.digest_state_path.as_deref(), local.day)
            {
                self.last_error = Some(format!("digest schedule could not be saved: {e}"));
                self.digest_save_pending = true;
            }
        }
    }

    /// Retry a schedule save that failed at send time, every tick, until
    /// the disk carries the day the in-memory schedule already advanced
    /// to — a restart before then would re-arm the digest inside the
    /// same week.
    fn retry_pending_digest_save(&mut self) {
        if !self.digest_save_pending {
            return;
        }
        match self
            .digest_schedule
            .persist(self.digest_state_path.as_deref())
        {
            Ok(()) => {
                self.digest_save_pending = false;
                // Clear only this feature's own sticky error, never one
                // another store set in the meantime.
                if self
                    .last_error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("digest schedule could not be saved"))
                {
                    self.last_error = None;
                }
            }
            Err(e) => {
                self.last_error = Some(format!("digest schedule could not be saved: {e}"));
            }
        }
    }

    /// FR-9: this week's digest, or `None` when it hides — the record has
    /// no samples in the window. Assembled fresh from the daily record
    /// (FR-7), the FR-4 warm report, the FR-6 outcome store, and the FR-0
    /// adapter's availability answers; no platform logic lives here.
    pub fn weekly_digest(&self) -> Option<WeeklyDigest> {
        WeeklyDigest::build(DigestInputs {
            record: &self.daily_record,
            warm: self.weekly_heat_digest(),
            soc_available: self.soc_available,
            cycles_available: self.cycles_available,
            cap_status: self.cap_monitor.status(),
            cap_guidance: self.capabilities.guidance(SettingKind::ChargeCap),
            outcomes: &self.outcomes,
            now_day: self.local_clock().day,
        })
    }

    /// Pause/unpause coaching nudges. Resetting the engine means a pause of
    /// any length never surfaces a backlog of stale conditions on resume.
    /// The FR-3 charge-cap gate survives the reset: it is state about the
    /// machine, not about the pause.
    pub fn set_alerts_paused(&mut self, paused: bool) {
        if self.alerts_paused != paused {
            self.alerts_paused = paused;
            let charge_coaching = self.alert_engine_charge_coaching_enabled();
            self.alert_engine = AlertEngine::new();
            self.alert_engine
                .set_charge_coaching_enabled(charge_coaching);
        }
    }

    /// Current FR-3 gate of the engine (kept beside the reset above so the
    /// machine-level gate outlives an engine reset).
    fn alert_engine_charge_coaching_enabled(&self) -> bool {
        !matches!(self.cap_monitor.status(), capdetect::CapStatus::Capped)
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

    /// FR-1: should the one-time charge-cap setup prompt be on screen?
    /// Hidden when the adapter has no guidance to give — there is nothing
    /// meaningful to point at, so the app does not ask.
    pub fn cap_prompt_visible(&self) -> bool {
        capdetect::should_prompt(self.cap_monitor.status(), self.cap_prompt_record)
            && self.capabilities.guidance(SettingKind::ChargeCap).is_some()
    }

    /// FR-1: the exact per-machine steps for setting a charge cap, from the
    /// FR-0 adapter. `None` = nothing meaningful to point at.
    pub fn charge_cap_guidance(&self) -> Option<SettingGuidance> {
        self.capabilities.guidance(SettingKind::ChargeCap)
    }

    /// FR-2: the cap-level recommendation for this machine, from the daily
    /// record (feature logic) and the FR-0 adapter (which cap levels exist).
    /// Recomputed on demand — a scan of at most `MAX_DAYS` rows, cheap even
    /// at 1 Hz — so there is no cached state to go stale.
    pub fn cap_assessment(&self) -> crate::caplevel::CapAssessment {
        crate::caplevel::assess(&self.daily_record, &self.capabilities.cap_options())
    }

    /// FR-1: record the user's answer; the prompt never shows again. The
    /// answer is persisted BEFORE it is committed to memory: a failed persist
    /// leaves the prompt on screen (the error surfaces in the header) rather
    /// than hiding it for the session and silently re-asking next launch.
    pub fn answer_cap_prompt(&mut self, answer: CapPromptAnswer) {
        let mut pending = self.cap_prompt_record;
        pending.answered = Some(answer);
        match self
            .cap_prompt_path
            .as_deref()
            .map(|p| capdetect::save_record(p, pending))
        {
            Some(Ok(())) | None => self.cap_prompt_record = pending,
            Some(Err(e)) => {
                self.last_error = Some(format!(
                    "charge-cap prompt answer was not saved; the prompt stays up: {e}"
                ));
            }
        }
    }

    /// FR-1/FR-3: keep the charge-cap state on disk — a verdict transition
    /// (cap detected or gone) writes immediately, so charge-alert retirement
    /// survives a restart; a running stretch writes at most once per
    /// [`capdetect::STRETCH_PERSIST_SECS`], plus once when it breaks — so a
    /// restart resumes a running detection instead of resetting it. Failures
    /// surface in the header, never silent.
    fn checkpoint_cap_state(&mut self) {
        let stretch = self
            .cap_monitor
            .stretch_state()
            .map(|(started, last_seen, max_soc)| capdetect::StretchState {
                started,
                last_seen,
                max_soc,
            });
        // FR-3: a verdict transition persists before anything else — the
        // retirement gate must be on disk by the time the next launch reads.
        let verdict = settled_verdict(self.cap_monitor.status());
        if verdict != self.cap_verdict_persisted {
            if self.persist_cap_record(stretch) {
                self.cap_verdict_persisted = verdict;
                self.cap_stretch_persisted = stretch;
            }
            return;
        }
        match (stretch, self.cap_stretch_persisted) {
            (Some(s), last) => {
                let due = last.is_none_or(|prev| {
                    s.last_seen.saturating_sub(prev.last_seen) >= capdetect::STRETCH_PERSIST_SECS
                });
                if due && self.persist_cap_record(Some(s)) {
                    self.cap_stretch_persisted = Some(s);
                }
            }
            // The stretch is gone (unplugged, or the verdict resolved); if
            // disk still carries one, clear it so a restart does not
            // resurrect a broken stretch.
            (None, Some(_)) => {
                if self.persist_cap_record(None) {
                    self.cap_stretch_persisted = None;
                }
            }
            (None, None) => {}
        }
    }

    /// Write the cap-setup record as it currently stands, optionally carrying
    /// a stretch. The live verdict rides along (FR-3). Returns whether the
    /// write landed.
    fn persist_cap_record(&mut self, stretch: Option<capdetect::StretchState>) -> bool {
        let mut record = self.cap_prompt_record;
        record.stretch = stretch;
        record.verdict = settled_verdict(self.cap_monitor.status());
        match self
            .cap_prompt_path
            .as_deref()
            .map(|p| capdetect::save_record(p, record))
        {
            Some(Ok(())) | None => true,
            Some(Err(e)) => {
                self.last_error = Some(format!("charge-cap state could not be saved: {e}"));
                false
            }
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
        // A new bucket can change the multi-day pattern FR-5 keys on; the
        // recompute is a cheap scan of the retained window, once a minute.
        if created_new_bucket {
            self.recompute_full_charge_advice();
        }
    }

    /// FR-5: rebuild the one-time advice card from the current pattern and
    /// adapter answers. A no-op once the advice has been said, while a card
    /// is already up, or when there is nothing new to say — the card never
    /// downgrades itself out of view mid-session, and never shows twice.
    fn recompute_full_charge_advice(&mut self) {
        if self.advice_shown || self.full_charge_advice.is_some() {
            return;
        }
        // One clock read for both the local slice and the unix instant: two
        // reads can straddle an hour tick and shift the daytime window by an
        // hour on the very evaluation that persists the once-ever flag.
        let local = clock::local_now();
        if !advice::regular_deep_daytime_discharge(&self.history_store.buckets, local) {
            return;
        }
        self.full_charge_advice = self.adaptive_advice.clone();
        // Mark at raise, not at dismiss: the once-ever contract is "shown at
        // most one time ever", so quitting with the card up must not re-show
        // it on the next launch. Dismissal is purely the hide-now case.
        self.mark_full_charge_advice_shown();
    }

    /// What the advice card's dismiss button does: persist the once-ever flag
    /// (idempotent — the raise already marked it) before the card leaves the
    /// UI, so a crash or a failed write can never lose a said advice, then
    /// take the card down for the rest of the session.
    pub fn dismiss_full_charge_advice(&mut self) {
        self.mark_full_charge_advice_shown();
        self.full_charge_advice = None;
    }

    /// Record that the FR-5 advice has been said: called the moment the card
    /// is raised, persisting the flag so it is never said again (even across
    /// restarts — quitting with the card up counts as having seen it) and
    /// stopping recomputes. The card itself stays up for the session until
    /// dismissed.
    pub fn mark_full_charge_advice_shown(&mut self) {
        if self.advice_shown {
            return;
        }
        self.advice_shown = true;
        if let Some(path) = &self.advice_state_path {
            if let Err(e) = crate::history::atomic_write(path, ADVICE_SHOWN_MARKER.as_bytes()) {
                self.last_error = Some(format!("persist advice state: {e}"));
            }
        }
    }

    /// Fold a sample into the long-term daily record (FR-7): one summary
    /// row per local day — capacity, cycles, hours at high charge, warm
    /// hours — kept well past the 7-day 1-minute history window.
    fn record_daily(&mut self, sample: &crate::collect::BatterySample) {
        let day = clock::local_now().day;
        self.daily_record.fold(sample, day);
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
        if let Some(path) = &self.record_path {
            if let Err(e) = self.daily_record.save(path) {
                self.last_error = Some(e);
            }
        }
        // FR-6: the outcome store joins every checkpoint, so retirement and
        // pending watches survive a restart.
        self.save_outcomes_now();
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

/// FR-3: a monitor status is a persisted verdict only when settled —
/// `Capped` or `Uncapped`. `Undetermined` and `Hidden` never persist.
fn settled_verdict(status: capdetect::CapStatus) -> Option<capdetect::CapStatus> {
    match status {
        capdetect::CapStatus::Capped | capdetect::CapStatus::Uncapped => Some(status),
        _ => None,
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

    /// Simulate a machine whose adapter reports temperature Unavailable
    /// (many Windows WMI setups): the FR-4 gate must hide the digest.
    fn heat_hidden_app(path: Option<PathBuf>) -> App {
        let mut app = app_at(path);
        app.temperature_available = false;
        app
    }

    #[test]
    fn cap_level_recommendation_wires_record_and_adapter_together() {
        let (_tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, None);
        // Fresh install: below the 7-day gate, nothing is recommended.
        assert_eq!(
            app.cap_assessment(),
            crate::caplevel::CapAssessment::NeedHistory { days: 0 }
        );
        // Seven normal days dipping to 62%: the adapter's lowest covering
        // level (sim/macOS grid 80–100 step 5 → 80) is recommended.
        for day in 0..7u32 {
            let mut s = sample(62.0);
            app.daily_record.fold(&s, day);
            s.state_of_charge = Some(100.0);
            for _ in 0..crate::caplevel::MIN_DAY_SAMPLES {
                app.daily_record.fold(&s, day);
            }
        }
        assert_eq!(
            app.cap_assessment(),
            crate::caplevel::CapAssessment::Recommend(crate::caplevel::CapRecommendation {
                level_pct: 80,
                lowest_pct: 62.0,
                days: 7
            })
        );
    }

    fn sample(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            ..Default::default()
        }
    }

    fn sample_charging(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(crate::collect::types::ChargingState::Charging),
            ..Default::default()
        }
    }

    fn sample_on_ac_parked() -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(80.0),
            charging_state: Some(crate::collect::types::ChargingState::OnAcNotCharging),
            ..Default::default()
        }
    }

    /// FR-3: a settled Capped verdict persists and reloads, so charge-alert
    /// retirement holds from the first tick after a restart — no 3 h
    /// re-detection window of nagging a capped machine.
    #[test]
    fn fr3_capped_verdict_persists_and_retires_charge_alerts_after_restart() {
        let dir = tagged_dir("fr3-verdict");
        let history = dir.join("history.json");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, Some(history.clone()));
        assert!(!app.cap_prompt_visible(), "no prompt before any evidence");

        // Drive the monitor to a Capped verdict (injected clock, pure).
        let base = crate::history::unix_now();
        let mut now = base;
        while now <= base + capdetect::CAP_SUSTAIN_SECS {
            app.cap_monitor.observe(&sample_on_ac_parked(), now);
            now += 60;
        }
        assert_eq!(app.cap_monitor.status(), capdetect::CapStatus::Capped);
        app.checkpoint_cap_state();
        let record_text = std::fs::read_to_string(app.cap_prompt_path.clone().unwrap())
            .expect("cap record written");
        assert!(
            record_text.contains("status capped"),
            "verdict must be on disk: {record_text:?}"
        );

        // A restart resumes the verdict: the gate is off from the first
        // tick, and the one-time prompt still does not show.
        let (tx2, rx2) = crossbeam_channel::unbounded();
        let mut reloaded = App::with_history_path(rx2, Some(history.clone()));
        assert_eq!(reloaded.cap_monitor.status(), capdetect::CapStatus::Capped);
        assert!(!reloaded.alert_engine_charge_coaching_enabled());
        assert!(!reloaded.cap_prompt_visible());

        // Feeding the reloaded app a parked-at-100 sample fires no alert.
        tx2.send(Ok(sample_charging(100.0))).unwrap();
        tx2.send(Ok(sample_on_ac_parked())).unwrap();
        reloaded.update();
        assert!(reloaded.last_alert.is_none(), "retired coaches stay quiet");
        drop(tx);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dwell_is_tracked_from_the_sample_stream_and_feeds_the_benchmark() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, None);

        // A fresh app has never tracked dwell: the factor is excluded.
        tx.send(Ok(sample(85.0))).unwrap();
        app.update();
        assert_eq!(app.history_store.dwell_s, Some(0.0), "tracking started");
        let benchmark = app.benchmark.clone().expect("benchmark evaluated");
        // Sub-1 Hz-frame gap may credit a hair, but the factor must exist and
        // the stamped sample must carry the accumulated total.
        assert!(benchmark
            .parameters
            .iter()
            .any(|p| p.name == "High-charge dwell"));
        let latest = app.latest.clone().expect("latest sample");
        assert_eq!(latest.high_soc_dwell_s, app.history_store.dwell_s);

        // Qualifying seconds (85% charging) accumulate into the store.
        let before = app.history_store.dwell_s.unwrap_or(0.0);
        for _ in 0..5 {
            tx.send(Ok(sample_charging(85.0))).unwrap();
            app.update();
        }
        assert!(
            app.history_store.dwell_s.unwrap_or(0.0) >= before,
            "dwell never decreases while samples qualify"
        );
        // The stamped benchmark sample carries the live total. (The size of
        // the credit in a sub-second test run is nondeterministic — the exact
        // accumulation math is pinned by the `dwell` module tests.)
        let latest = app.latest.clone().expect("latest after qualifying run");
        assert_eq!(latest.high_soc_dwell_s, app.history_store.dwell_s);
    }

    #[test]
    fn dwell_total_survives_a_restart_through_the_history_store() {
        let dir = tagged_dir("dwell-restart");
        let path = dir.join("history.json");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, Some(path.clone()));

        tx.send(Ok(sample_charging(90.0))).unwrap();
        app.update();
        app.history_store.add_dwell(600.0); // simulate ten qualifying minutes
        app.save_history_now();

        // A new app instance over the same data dir reloads the total.
        let (_tx2, rx2) = crossbeam_channel::unbounded();
        let reloaded = App::with_history_path(rx2, Some(path));
        assert_eq!(reloaded.history_store.dwell_s, Some(600.0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_app_excludes_the_dwell_factor_until_data_exists() {
        // No samples yet: never tracked, factor excluded from the scorecard.
        let app = app_at(None);
        assert_eq!(app.history_store.dwell_s, None);
        assert!(app.benchmark.is_none());

        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, None);
        tx.send(Ok(sample(50.0))).unwrap(); // below threshold, unplugged
        app.update();
        let benchmark = app.benchmark.clone().expect("benchmark after first sample");
        assert!(benchmark
            .parameters
            .iter()
            .any(|p| p.name == "High-charge dwell"));
        assert_eq!(app.history_store.dwell_s, Some(0.0));
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

    #[test]
    fn heat_digest_hides_when_adapter_reports_temperature_unavailable() {
        // Even with warm history in the store, an adapter without a
        // temperature reading means no heat UI — FR-4 acceptance.
        let mut app = heat_hidden_app(None);
        app.history_store.push(
            &crate::collect::BatterySample {
                temperature_c: Some(40.0),
                ..Default::default()
            },
            crate::history::unix_now(),
        );
        assert!(app.weekly_heat_digest().is_none());
    }

    #[test]
    fn heat_digest_hidden_with_no_temperature_history_even_when_supported() {
        // Supported adapter but a fresh install (empty store): nothing to
        // reconcile with, so no digest either — never a zero presented as
        // data.
        let app = app_at(None);
        assert!(app.weekly_heat_digest().is_none());
    }

    #[test]
    fn heat_digest_counts_from_the_stored_history() {
        // One warm minute and one cool minute in the store: the digest must
        // match the history exactly (0.5 h covered, 1 warm minute → the UI
        // shows the same number the CSV export would produce).
        let mut app = app_at(None);
        let now = crate::history::unix_now();
        app.history_store.push(
            &crate::collect::BatterySample {
                temperature_c: Some(31.0),
                ..Default::default()
            },
            now - 120,
        );
        app.history_store.push(
            &crate::collect::BatterySample {
                temperature_c: Some(25.0),
                ..Default::default()
            },
            now - 60,
        );
        let digest = app
            .weekly_heat_digest()
            .expect("digest when temperature is readable");
        assert_eq!(digest.warm_minutes, 1);
        assert_eq!(digest.covered_minutes, 2);
        assert_eq!(digest.warm_hours(), 1.0 / 60.0);
    }

    #[test]
    fn daily_record_folds_samples_and_persists_with_history() {
        let dir = tagged_dir("record");
        let mut app = app_at(Some(dir.join("history.json")));

        let mut s = sample(90.0);
        s.temperature_c = Some(35.0);
        s.design_capacity = Some(50_000.0);
        s.full_charge_capacity = Some(45_000.0);
        s.cycle_count = Some(300);
        app.record_daily(&s);
        assert_eq!(app.daily_record.rows.len(), 1);
        assert!(app.daily_record.rows[0].high_n == 1 && app.daily_record.rows[0].warm_n == 1);

        // The record checkpoints with the history store, next to it.
        app.save_history_now();
        let record_path = dir.join("daily-record.json");
        assert!(record_path.exists(), "record persisted beside history");
        let loaded = DailyRecord::load(&record_path);
        assert_eq!(loaded.record, app.daily_record);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_record_starts_fresh_with_a_visible_notice() {
        let dir = tagged_dir("record-notice");
        let record_path = dir.join("daily-record.json");
        std::fs::write(&record_path, "{").unwrap();
        let app = app_at(Some(dir.join("history.json")));
        assert!(
            app.daily_record.rows.is_empty(),
            "corrupt record starts fresh"
        );
        let notice = app
            .record_load_error
            .as_ref()
            .expect("record load failure surfaced as a dashboard notice");
        assert!(notice.contains("daily record"), "{notice}");
        assert!(notice.contains("daily-record.json.corrupt"), "{notice}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_deletes_the_daily_record_too() {
        let dir = tagged_dir("record-uninstall");
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, Some(dir.join("history.json")));
        app.record_daily(&sample(50.0));
        app.save_history_now();
        let record_path = dir.join("daily-record.json");
        assert!(record_path.exists(), "record persisted before uninstall");

        app.confirm_uninstall();
        assert!(!record_path.exists(), "uninstall removes the daily record");
        let results = app.uninstall_results.clone().expect("per-path results");
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        drop(tx);
    }

    // ---- FR-5: one-time advice card (render gating + shown state) ----

    fn unsaid_advice() -> SettingAdvice {
        SettingAdvice {
            message: "check the maker tool".into(),
            url: None,
        }
    }

    /// A retained window showing deep daytime discharge on four local days
    /// (yesterday and older): 30 daytime buckets near 95%, then 30 near 40%
    /// — a ≥40-point swing reaching ≤45%, the pattern FR-5 advises on.
    fn deep_discharge_buckets() -> Vec<crate::history::MinuteBucket> {
        use crate::history::MinuteBucket;
        let local = clock::local_now();
        let now = crate::history::unix_now() as i64;
        let utc_hour = now.rem_euclid(86_400) / 3_600;
        let offset = (local.hour as i64 - utc_hour) * 3_600;
        let today = (now + offset).div_euclid(86_400);
        let mut buckets = Vec::new();
        for day_age in 1..=4u64 {
            let noon_unix = (today - day_age as i64) * 86_400 + 12 * 3_600 - offset;
            for i in 0..60u64 {
                let soc = if i < 30 { 95.0 } else { 40.0 };
                buckets.push(MinuteBucket {
                    minute: (noon_unix / 60 - i as i64) as u64,
                    n: 1,
                    soc_sum: soc,
                    soc_n: 1,
                    temp_sum: 0.0,
                    temp_n: 0,
                    watts_sum: 0.0,
                    watts_n: 0,
                    soh_sum: 0.0,
                    soh_n: 0,
                    chg_n: 0,
                    cycles_last: None,
                });
            }
        }
        buckets.sort_by_key(|b| b.minute);
        buckets
    }

    #[test]
    fn fr5_advice_appears_only_when_the_detector_fires_and_an_adaptive_equivalent_exists() {
        // Detector fires + the adapter has guidance: the card goes up.
        let mut app = app_at(None);
        app.adaptive_advice = Some(unsaid_advice());
        app.history_store.buckets = deep_discharge_buckets();
        app.recompute_full_charge_advice();
        assert_eq!(app.full_charge_advice, Some(unsaid_advice()));

        // A maker module with no adaptive equivalent: the feature hides
        // entirely, even though the history pattern is there.
        let mut app = app_at(None);
        app.adaptive_advice = None;
        app.history_store.buckets = deep_discharge_buckets();
        app.recompute_full_charge_advice();
        assert!(app.full_charge_advice.is_none(), "no guidance → no card");

        // Well-behaved history (fresh install): nothing to advise on.
        let mut app = app_at(None);
        app.adaptive_advice = Some(unsaid_advice());
        app.recompute_full_charge_advice();
        assert!(app.full_charge_advice.is_none(), "no pattern → no card");
    }

    #[test]
    fn dismissing_the_advice_persists_the_shown_state_across_a_restart() {
        let dir = tagged_dir("advice-dismiss");
        let path = dir.join("history.json");
        let mut app = app_at(Some(path.clone()));
        app.adaptive_advice = Some(unsaid_advice());
        app.history_store.buckets = deep_discharge_buckets();
        app.recompute_full_charge_advice();
        assert!(
            app.full_charge_advice.is_some(),
            "detector fires before dismissal"
        );

        // What the card's dismiss button calls.
        app.dismiss_full_charge_advice();
        assert!(app.full_charge_advice.is_none(), "card closes at once");
        assert!(app.advice_shown);
        let state_path = dir.join("advice-state.json");
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            ADVICE_SHOWN_MARKER,
            "once-ever flag persisted"
        );

        // Persist the history too, so the restart below reloads the same
        // deep-discharge pattern: even then the card must never reappear.
        app.save_history_now();
        let (_tx, rx) = crossbeam_channel::unbounded();
        let restarted = App::with_history_path(rx, Some(path));
        assert!(restarted.advice_shown, "flag survives the restart");
        assert!(
            restarted.full_charge_advice.is_none(),
            "pattern still in the window, but the advice was said once ever"
        );
        assert!(!dir.join("advice-state.json.corrupt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn advice_stays_down_once_shown_even_as_more_history_arrives() {
        let mut app = app_at(None);
        app.adaptive_advice = Some(unsaid_advice());
        app.full_charge_advice = Some(unsaid_advice());
        app.advice_shown = false;

        // Card is up; new minute-buckets keep landing → recompute must not
        // churn or downgrade it, and dismissal must stop the rebuild.
        app.recompute_full_charge_advice();
        assert_eq!(app.full_charge_advice, Some(unsaid_advice()));
        app.dismiss_full_charge_advice();
        app.recompute_full_charge_advice();
        assert!(
            app.full_charge_advice.is_none(),
            "said once ever, never rebuilt"
        );
    }

    #[test]
    fn raising_the_advice_persists_the_shown_state_even_if_the_app_quits() {
        let dir = tagged_dir("advice-raise");
        let path = dir.join("history.json");
        let mut app = app_at(Some(path.clone()));
        app.adaptive_advice = Some(unsaid_advice());
        app.history_store.buckets = deep_discharge_buckets();
        app.recompute_full_charge_advice();
        assert!(
            app.full_charge_advice.is_some(),
            "detector fires before the quit"
        );

        // Quit right here — no dismissal. The once-ever contract is
        // "shown at most one time ever", so the raise itself must have
        // persisted the flag: a restart may not re-show the card.
        assert!(app.advice_shown, "marked at raise, not at dismiss");
        assert_eq!(
            std::fs::read_to_string(dir.join("advice-state.json")).unwrap(),
            ADVICE_SHOWN_MARKER,
            "once-ever flag persisted at raise"
        );

        app.save_history_now();
        let (_tx, rx) = crossbeam_channel::unbounded();
        let restarted = App::with_history_path(rx, Some(path));
        assert!(restarted.advice_shown, "flag survives the quit");
        assert!(
            restarted.full_charge_advice.is_none(),
            "pattern still in the window, but the advice was said once ever"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_advice_state_is_quarantined_and_never_silent() {
        // Deliberate semantics: an unreadable marker counts as said (the
        // once-ever guarantee never resets on a crash), but never silently —
        // the bytes are quarantined and a sticky notice says so, matching
        // the M1/M2 history and daily-record discipline.
        let dir = tagged_dir("advice-corrupt");
        std::fs::write(dir.join("advice-state.json"), "{").unwrap();
        let mut app = app_at(Some(dir.join("history.json")));
        assert!(app.advice_shown, "corrupt marker counts as said");
        let notice = app.advice_load_error.as_deref().expect("notice surfaced");
        assert!(notice.contains("advice-state.json.corrupt"), "{notice}");
        assert!(dir.join("advice-state.json.corrupt").exists());
        assert!(!dir.join("advice-state.json").exists(), "moved aside");

        // Even with the pattern live, the card stays hidden.
        app.adaptive_advice = Some(unsaid_advice());
        app.history_store.buckets = deep_discharge_buckets();
        app.recompute_full_charge_advice();
        assert!(
            app.full_charge_advice.is_none(),
            "said once ever, never re-shown"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unrecognized_marker_content_is_quarantined_too() {
        // The marker match is exact: a well-formed-but-unrecognized file is
        // corruption, not a "not said" vote, so it quarantines with a notice.
        let dir = tagged_dir("advice-unrecognized");
        std::fs::write(
            dir.join("advice-state.json"),
            "{\"adaptive_charging_advice_shown\":false}",
        )
        .unwrap();
        let app = app_at(Some(dir.join("history.json")));
        assert!(app.advice_shown);
        assert!(app.advice_load_error.is_some(), "notice surfaced");
        assert!(dir.join("advice-state.json.corrupt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- FR-6: alert outcome tracking ---------------------------------------

    use crate::alert::AlertKind;
    use crate::collect::types::ChargingState;
    use crate::outcomes::{OutcomeStore, OUTCOME_WINDOW_S};

    /// A pinned local clock: 10:00 local on day `DAY`, unix `T` seconds.
    fn pinned_clock(day: u32, unix_secs: i64) -> clock::LocalNow {
        clock::LocalNow {
            hour: 10,
            day,
            unix_secs,
        }
    }

    fn plugged_in(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(ChargingState::Charging),
            ..Default::default()
        }
    }

    fn unplugged(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(ChargingState::Discharging),
            ..Default::default()
        }
    }

    #[test]
    fn alert_fires_are_logged_and_act_on_when_the_condition_ends() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = App::with_history_path(rx, None);
        app.test_clock = Some(pinned_clock(20_000, 1_000_000));

        // Plugged in above the band: the BandCoach nudge fires and is
        // delivered, with an open outcome watch.
        tx.send(Ok(plugged_in(90.0))).unwrap();
        app.update();
        assert_eq!(app.last_alert.map(|a| a.kind), Some(AlertKind::BandCoach));
        assert_eq!(
            app.outcomes.acted_on_rate(AlertKind::BandCoach, 1_000_000),
            Some((0, 1))
        );

        // The user unplugs inside the window: acted on.
        tx.send(Ok(unplugged(70.0))).unwrap();
        app.update();
        assert_eq!(
            app.outcomes.acted_on_rate(AlertKind::BandCoach, 1_000_000),
            Some((1, 1)),
            "the condition ended inside the outcome window"
        );
    }

    #[test]
    fn retired_after_n_consecutive_ignores_the_kind_stops_firing_in_real_time() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let path = tagged_dir("fr6-retire").join("history.json");
        let mut app = App::with_history_path(rx, Some(path));
        app.test_clock = Some(pinned_clock(20_000, 1_000_000));

        // Three rounds, each one local day: fire (plugged in above the
        // band), never unplug, let the outcome window expire.
        for round in 0..3i64 {
            let t = 1_000_000 + round * 86_400;
            app.test_clock = Some(pinned_clock(20_000 + round as u32, t));
            tx.send(Ok(plugged_in(90.0))).unwrap();
            app.update();
            let delivered = app
                .outcomes
                .acted_on_rate(AlertKind::BandCoach, (t + 86_400) as u64)
                .map(|(_, total)| total)
                .unwrap_or(0);
            assert_eq!(delivered, round as u32 + 1, "round {round}: fire delivered");

            // Still plugged in when the window runs out: ignored.
            app.test_clock = Some(pinned_clock(
                20_000 + round as u32,
                t + OUTCOME_WINDOW_S as i64,
            ));
            tx.send(Ok(plugged_in(90.0))).unwrap();
            app.update();

            // Unplug once the watch has closed (after the ignore) so the
            // engine's episode latch re-arms for the next day's fire.
            tx.send(Ok(unplugged(70.0))).unwrap();
            app.update();
        }
        assert!(
            app.outcomes.is_retired(AlertKind::BandCoach),
            "three consecutive ignored firings must retire the kind"
        );

        // Day 4: the engine still fires (its own state is untouched), but
        // the outcome store stops the delivery — no real-time alert.
        app.test_clock = Some(pinned_clock(20_003, 1_000_000 + 3 * 86_400));
        tx.send(Ok(plugged_in(90.0))).unwrap();
        app.update();
        assert_eq!(
            app.outcomes
                .acted_on_rate(AlertKind::BandCoach, 1_000_000 + 3 * 86_400)
                .map(|(_, total)| total),
            Some(3),
            "the suppressed fire is not a delivered alert"
        );
        std::fs::remove_dir_all(tagged_dir("fr6-retire")).ok();
    }

    #[test]
    fn outcome_store_survives_a_restart_and_stays_retired() {
        let dir = tagged_dir("fr6-restart");
        let path = dir.join("history.json");
        let t = 1_000_000i64;
        {
            let (tx, rx) = crossbeam_channel::unbounded();
            let mut app = App::with_history_path(rx, Some(path.clone()));
            app.outcomes = OutcomeStore::with_retire_after(1);
            app.test_clock = Some(pinned_clock(20_000, t));
            tx.send(Ok(plugged_in(90.0))).unwrap();
            app.update();
            assert!(!app.outcomes.is_retired(AlertKind::BandCoach));
            // Ignore it: still plugged in at window expiry → retired (N=1).
            app.test_clock = Some(pinned_clock(20_000, t + OUTCOME_WINDOW_S as i64));
            tx.send(Ok(plugged_in(90.0))).unwrap();
            app.update();
            assert!(app.outcomes.is_retired(AlertKind::BandCoach));
            app.save_history_now();
        }
        // Restart over the same data dir: retirement and the log survive.
        let (_tx2, rx2) = crossbeam_channel::unbounded();
        let reloaded = App::with_history_path(rx2, Some(path));
        assert!(
            reloaded.outcomes.is_retired(AlertKind::BandCoach),
            "retirement persists"
        );
        assert_eq!(reloaded.outcomes.retire_after(), 1, "configured N persists");
        assert_eq!(
            reloaded
                .outcomes
                .acted_on_rate(AlertKind::BandCoach, t as u64 + 1)
                .map(|(_, total)| total),
            Some(1),
            "the logged fire reloads"
        );
        assert!(reloaded.outcomes_load_error.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_outcome_store_surfaces_a_notice_and_tracks_anew() {
        let dir = tagged_dir("fr6-corrupt");
        std::fs::write(dir.join("alert-outcomes.json"), "{\"version\":1,oops").unwrap();
        let app = app_at(Some(dir.join("history.json")));
        let notice = app
            .outcomes_load_error
            .as_ref()
            .expect("load failure surfaced as a sticky notice");
        assert!(notice.contains("could not be read"), "{notice}");
        assert!(dir.join("alert-outcomes.json.corrupt").exists());
        assert_eq!(
            app.outcomes,
            OutcomeStore::new(),
            "starts fresh, never silent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
