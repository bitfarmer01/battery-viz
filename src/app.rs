//! Application state: drains the collector channel, keeps a bounded history
//! ring, and re-evaluates the benchmark as samples arrive.

use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::benchmark::{self, BenchmarkResult};
use crate::collect::CollectorResult;

/// How many samples of history the dashboard keeps (at 1 Hz, 10 minutes).
pub const HISTORY_LEN: usize = 600;

pub struct App {
    rx: Receiver<CollectorResult>,
    pub latest: Option<crate::collect::BatterySample>,
    pub benchmark: Option<BenchmarkResult>,
    pub history: History,
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

impl App {
    pub fn new(rx: Receiver<CollectorResult>) -> Self {
        App {
            rx,
            latest: None,
            benchmark: None,
            history: History::new(),
            last_error: None,
            frames_rendered: 0,
        }
    }

    /// Drain pending samples (non-blocking) and refresh derived state.
    pub fn update(&mut self) {
        self.frames_rendered += 1;
        let mut newest: Option<crate::collect::BatterySample> = None;
        let mut error: Option<String> = None;
        for result in self.rx.try_iter() {
            match result {
                Ok(sample) => {
                    self.history.push(&sample);
                    newest = Some(sample);
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(sample) = newest {
            self.benchmark = Some(benchmark::evaluate(&sample));
            self.latest = Some(sample);
        }
        if error.is_some() {
            self.last_error = error;
        }
    }

    /// Next-wake interval for eframe: paint continuously for smooth
    /// animations; battery data itself only changes at 1 Hz.
    pub fn repaint_interval(&self) -> Duration {
        Duration::from_millis(16)
    }
}
