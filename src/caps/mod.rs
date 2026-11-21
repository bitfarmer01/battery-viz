//! Platform capability layer (FR-0, longevity roadmap).
//!
//! Features never contain platform-specific logic. They ask one adapter three
//! questions:
//! 1. can this *metric* be read on this machine?
//! 2. can this *setting's* state be detected on this machine?
//! 3. where does the user change this setting?
//!
//! When the answer is "no", the feature degrades to advice or hides itself.
//! Nothing is guessed: every question returns either an answer or an explicit
//! `Unavailable`-shaped value (`None`, empty list, `Detectability::AdviceOnly`).
//!
//! The same adapters double as [`Collector`]s, so platform selection happens
//! in exactly one place — here — and the sampling thread, benchmark, and
//! alert engine keep consuming the unchanged `BatterySample` stream.
//!
//! The dead_code allowance is deliberate: this is the foundation layer for
//! FR-1..FR-9, so the interface ships complete before its consumers.

#![allow(dead_code)]

pub mod macos;
pub mod sim;
pub mod windows;

use crate::collect::types::BatterySample;
use crate::collect::Collector;

/// Which platform an adapter serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
    Simulator,
}

/// Hardware identity of the machine the adapter runs on. On Windows this
/// drives the per-maker module (cap options and guidance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maker {
    Apple,
    Dell,
    Hp,
    Lenovo,
    Asus,
    Surface,
    /// No recognized maker. The adapter still works, with generic advice.
    Unknown,
}

/// A metric a feature may want to read. Membership in an adapter's
/// `readable_metrics` is the answer to question 1, machine-wide; per-sample
/// presence is `MetricId::is_present`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricId {
    StateOfCharge,
    DesignCapacity,
    FullChargeCapacity,
    CycleCount,
    CycleLimit,
    Temperature,
    Voltage,
    Power,
    ChargingState,
    HighSocDwell,
}

/// Every metric any adapter can declare. Test-only completeness checks.
pub const ALL_METRICS: &[MetricId] = &[
    MetricId::StateOfCharge,
    MetricId::DesignCapacity,
    MetricId::FullChargeCapacity,
    MetricId::CycleCount,
    MetricId::CycleLimit,
    MetricId::Temperature,
    MetricId::Voltage,
    MetricId::Power,
    MetricId::ChargingState,
    MetricId::HighSocDwell,
];

impl MetricId {
    /// Question 1, per sample: does this metric carry a value here?
    /// `false` is an explicit "unavailable", never a guess.
    pub fn is_present(self, s: &BatterySample) -> bool {
        match self {
            MetricId::StateOfCharge => s.state_of_charge.is_some(),
            MetricId::DesignCapacity => s.design_capacity.is_some(),
            MetricId::FullChargeCapacity => s.full_charge_capacity.is_some(),
            MetricId::CycleCount => s.cycle_count.is_some(),
            MetricId::CycleLimit => s.cycle_limit.is_some(),
            MetricId::Temperature => s.temperature_c.is_some(),
            MetricId::Voltage => s.voltage_mv.is_some(),
            MetricId::Power => s.power_w.is_some(),
            MetricId::ChargingState => s.charging_state.is_some(),
            MetricId::HighSocDwell => s.high_soc_dwell_s.is_some(),
        }
    }
}

/// A setting a feature may want to detect or coach about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// Charge cap: a maximum state of charge the firmware holds.
    ChargeCap,
    /// OS or maker adaptive charging (macOS Optimized Battery Charging, etc.).
    AdaptiveCharging,
}

/// Question 2: can the adapter detect a setting's state on this machine?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detectability {
    /// Readable programmatically.
    Readable,
    /// Not readable directly; infer from charge behaviour (FR-1's fallback).
    BehavioralOnly,
    /// Not detectable at all — degrade to advice or hide the feature.
    AdviceOnly,
}

/// Cap controls that exist on a machine. Which of these a maker's tool
/// offers is what each per-maker module reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapOptions {
    /// Fixed cap presets, percent of full charge (e.g. ASUS 60/80).
    pub fixed: &'static [u8],
    /// Configurable cap range, when the maker tool exposes one.
    pub range: Option<CapRange>,
    /// An adaptive/optimized charging mode exists alongside the caps.
    pub adaptive: bool,
}

impl CapOptions {
    pub fn none() -> Self {
        CapOptions {
            fixed: &[],
            range: None,
            adaptive: false,
        }
    }

    /// True when the machine exposes no cap control at all.
    pub fn is_none(self) -> bool {
        self.fixed.is_empty() && self.range.is_none() && !self.adaptive
    }
}

/// A configurable cap range, percent of full charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapRange {
    pub min: u8,
    pub max: u8,
    pub step: u8,
}

/// Question 3: where the user changes a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingGuidance {
    /// The tool or pane that holds the setting.
    pub tool: &'static str,
    /// Exact steps for this machine.
    pub steps: &'static str,
    /// Deep link, when one exists.
    pub url: Option<&'static str>,
}

/// One day of capacity history a machine already holds, offered by an
/// adapter to seed FR-7's daily record on first run (Windows: the daily
/// rows of `powercfg /batteryreport`). An empty list means "nothing to
/// seed from" — the record simply starts empty, never a guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeedCapacity {
    /// Day the reading is from, days since the local Unix epoch (the same
    /// counter as `clock::LocalNow.day`).
    pub day: u32,
    /// Full-charge over design capacity, percent.
    pub soh_pct: f64,
}

/// The battery's own lifetime record, as far as an adapter can read it
/// with corroborated semantics (FR-8 first-run import).
///
/// The macOS source field is undocumented; the one figure exported here —
/// lifetime operating hours — is the only one whose semantics survive
/// scrutiny (it is a calendar-speed clock: it rises in sleep, so it is
/// pack age, never time-in-use; corroborated by measurement on macOS
/// 14.2.1–27.2). Everything else the record holds — notably the
/// `TimeAtHighSoc` blob, whose per-counter charge-band meaning is not
/// established — is deliberately left unread: exporting an
/// interpretation would be a guess, which the roadmap forbids.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LifetimeRecord {
    /// Lifetime operating hours, from the battery's own clock.
    pub operating_hours: f64,
}

/// One platform adapter: readable metrics, detectable settings, guidance.
pub trait Capabilities: Send {
    fn platform(&self) -> Platform;
    fn maker(&self) -> Maker;
    /// Metrics readable on this machine; anything absent is Unavailable.
    fn readable_metrics(&self) -> &'static [MetricId];
    /// Can this setting's state be detected here?
    fn setting_detectability(&self, kind: SettingKind) -> Detectability;
    /// Cap controls that exist on this machine.
    fn cap_options(&self) -> CapOptions;
    /// Where the user changes this setting. `None` when there is nothing
    /// meaningful to point at.
    fn guidance(&self, kind: SettingKind) -> Option<SettingGuidance>;
    /// Capacity history this machine already holds, to seed the FR-7 daily
    /// record on first run. Default: unavailable — an adapter with no
    /// source of prior history offers none, and nothing is invented.
    fn capacity_seed(&self) -> Vec<SeedCapacity> {
        Vec::new()
    }
    /// The lifetime record this machine's battery already holds (FR-8
    /// first-run import), or `None` when it cannot be read without
    /// guessing. The macOS source is undocumented and its layout is
    /// inferred, so adapters export only figures with corroborated
    /// semantics and fail silently on any deviation: a parse failure is
    /// no record, never an error. Default: unavailable.
    fn lifetime_record(&self) -> Option<LifetimeRecord> {
        None
    }
}

/// Best adapter for this platform, for feature code (FR-1+).
pub fn platform_capabilities() -> Box<dyn Capabilities> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacCapabilities::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsCapabilities::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(sim::SimCapabilities::new())
    }
}

/// Best adapter for this platform, as a collector for the sampling thread.
/// The adapters implement [`Collector`] by delegating to the existing
/// platform collectors, so parsing behavior is unchanged.
pub fn platform_collector() -> Box<dyn Collector> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacCapabilities::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsCapabilities::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Dev platforms: the simulator, same as before the caps layer.
        Box::new(crate::collect::sim::SimCollector::new())
    }
}

/// Open a guidance deep link with the platform's URL handler. Platform
/// logic lives here in the capability layer, never in features (FR-0 rule).
/// URLs come only from adapter constants, never user input.
pub fn open_guidance_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("open {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_declared_metrics_match_sample_values() {
        let caps = sim::SimCapabilities::new();
        let mut collector = crate::collect::sim::SimCollector::new();
        let s = collector.sample().expect("sim sample");
        for m in ALL_METRICS {
            assert_eq!(
                m.is_present(&s),
                caps.readable_metrics().contains(m),
                "metric {m:?} declaration vs sample mismatch"
            );
        }
    }

    #[test]
    fn sim_answers_all_three_questions() {
        let caps = sim::SimCapabilities::new();
        assert_eq!(caps.platform(), Platform::Simulator);
        assert!(!caps.readable_metrics().is_empty());
        assert!(!caps.cap_options().is_none());
        for kind in [SettingKind::ChargeCap, SettingKind::AdaptiveCharging] {
            assert!(caps.guidance(kind).is_some(), "no guidance for {kind:?}");
        }
    }

    #[test]
    fn cap_options_none_is_explicit() {
        let none = CapOptions::none();
        assert!(none.is_none());
        assert!(!CapOptions {
            fixed: &[80],
            ..none
        }
        .is_none());
    }
}
