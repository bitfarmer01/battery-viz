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
    /// How often this collector should be sampled (re-audit N9): the spawn
    /// cost per sample differs wildly per platform (a subprocess launch on
    /// macOS vs. a cached WMI query on Windows), so the cadence belongs to
    /// the collector. The default is the 1 Hz pipeline cadence.
    fn cadence(&self) -> Duration {
        SAMPLE_INTERVAL
    }
}

/// Spawn `collector` on a background thread; returns the sample receiver.
pub fn spawn_collector(mut collector: Box<dyn Collector>) -> Receiver<CollectorResult> {
    let cadence = collector.cadence();
    let (tx, rx) = crossbeam_channel::bounded::<CollectorResult>(CHANNEL_CAPACITY);
    std::thread::Builder::new()
        .name("battery-collector".into())
        .spawn(move || loop {
            let result = collector.sample();
            // Try-send: a full channel means the UI thread is behind; drop
            // this sample rather than back-pressure the collector.
            let _ = tx.try_send(result);
            std::thread::sleep(cadence);
        })
        .expect("spawn battery-collector thread");
    rx
}

// Platform selection now lives in `crate::caps` (FR-0): `caps::platform_collector()`
// returns the adapter for this platform, and the adapters delegate to the
// collectors below.
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
