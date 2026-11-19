//! Application state: drains the collector channel, keeps a bounded history
//! ring, and re-evaluates the benchmark as samples arrive.

use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::benchmark::{self, BenchmarkResult};
use crate::collect::CollectorResult;
use crate::history::HistoryStore;

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
    pub frames_rendered: u64,
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
        // Load persisted history on startup; a missing or corrupt file starts
        // fresh (handled inside `HistoryStore::load`).
        let history_path = default_history_path();
        let history_store = match &history_path {
            Some(path) => HistoryStore::load(path),
            None => HistoryStore::default(),
        };
        App {
            rx,
            latest: None,
            benchmark: None,
            history: History::new(),
            history_store,
            history_path,
            last_error: None,
            frames_rendered: 0,
        }
    }

    /// Drain pending samples (non-blocking) and refresh derived state.
    pub fn update(&mut self) {
        self.frames_rendered += 1;
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
        }
        if let Some(sample) = samples.into_iter().last() {
            self.benchmark = Some(benchmark::evaluate(&sample));
            self.latest = Some(sample);
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

    /// Next-wake interval for eframe: paint continuously for smooth
    /// animations; battery data itself only changes at 1 Hz.
    pub fn repaint_interval(&self) -> Duration {
        Duration::from_millis(16)
    }
}
