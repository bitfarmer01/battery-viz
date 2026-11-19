//! Collector pipeline: a background thread samples the platform battery
//! interface and pushes samples over a bounded channel. The UI thread never
//! blocks on I/O, which is what keeps the frame budget intact.

pub mod macos;
pub mod sim;
pub mod types;
pub mod windows;

use std::time::Duration;

use crossbeam_channel::Receiver;
pub use types::{BatterySample, ConditionFlag, SourceKind};

/// How often collectors sample. Battery telemetry moves slowly; 1 Hz is
/// plenty and keeps spawn cost negligible on both platforms.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Channel depth: if the UI stalls, drop old samples instead of queueing lag.
pub const CHANNEL_CAPACITY: usize = 4;

pub type CollectorResult = Result<BatterySample, String>;

/// A blocking, single-sample read of platform battery telemetry.
pub trait Collector: Send + 'static {
    /// Identifier for provenance reporting. The UI currently reads the label
    /// off the sample's `source` field, so this stays interface-only on
    /// platforms whose collector hardcodes it.
    #[allow(dead_code)]
    fn source(&self) -> SourceKind;
    /// Return one sample. Implementations should be cheap and must not panic
    /// on partial data — missing fields stay `None`.
    fn sample(&mut self) -> CollectorResult;
}

/// Spawn `collector` on a background thread; returns the sample receiver.
pub fn spawn_collector(mut collector: Box<dyn Collector>) -> Receiver<CollectorResult> {
    let (tx, rx) = crossbeam_channel::bounded::<CollectorResult>(CHANNEL_CAPACITY);
    std::thread::Builder::new()
        .name("battery-collector".into())
        .spawn(move || loop {
            let result = collector.sample();
            // Try-send: a full channel means the UI thread is behind; drop
            // this sample rather than back-pressure the collector.
            let _ = tx.try_send(result);
            std::thread::sleep(SAMPLE_INTERVAL);
        })
        .expect("spawn battery-collector thread");
    rx
}

/// Pick the best collector for this platform at runtime.
pub fn platform_default() -> Box<dyn Collector> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacCollector::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsCollector::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Dev platforms: deterministic simulator so the UI is testable anywhere.
        Box::new(sim::SimCollector::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_collector_produces_samples() {
        let mut c = sim::SimCollector::new();
        let s = c.sample().expect("sim sample");
        assert_eq!(s.source, SourceKind::Simulator);
        assert!(s.state_of_charge.is_some());
    }
}
