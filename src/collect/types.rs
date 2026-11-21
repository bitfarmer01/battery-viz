//! Shared telemetry model.
//!
//! Every field is optional: OS and firmware expose different subsets.
//! `None` means "unknown", never "healthy". Provenance travels with the value.

use std::time::SystemTime;

/// One telemetry sample from a collector.
#[derive(Debug, Clone)]
pub struct BatterySample {
    #[allow(dead_code)]
    pub collected_at: SystemTime,
    pub source: SourceKind,
    /// State of charge, percent 0..=100.
    pub state_of_charge: Option<f64>,
    /// Design capacity as reported by firmware (mWh on Windows, mAh on Mac).
    pub design_capacity: Option<f64>,
    /// Full-charge capacity as reported by firmware.
    pub full_charge_capacity: Option<f64>,
    /// The firmware key the full-charge figure came from (re-audit N10),
    /// so the scorecard can label the figure EXACTLY — e.g. macOS
    /// `AppleRawMaxCapacity` (the pack's current full-charge capacity) vs
    /// `NominalChargeCapacity` (a firmware-nominal figure that reads higher
    /// on aged packs). `None` when the collector does not track the key.
    pub full_charge_capacity_key: Option<&'static str>,
    /// OEM-rated cycle limit, when known.
    pub cycle_limit: Option<u32>,
    pub cycle_count: Option<u32>,
    /// Pack temperature, degrees Celsius.
    pub temperature_c: Option<f64>,
    /// Pack voltage, millivolts.
    pub voltage_mv: Option<f64>,
    /// Instantaneous power: positive = discharging, negative = charging, watts.
    pub power_w: Option<f64>,
    /// Charging state.
    pub charging_state: Option<ChargingState>,
    /// Time the pack has spent at or above 80% state of charge while plugged
    /// in, seconds. No collector reports this: the app accumulates it from
    /// the sample stream (audit H4, `dwell` module), persists it in the
    /// history store, and stamps the running total onto the latest sample
    /// before the benchmark evaluates it.
    pub high_soc_dwell_s: Option<f64>,
    // Allowlist (M3 lint gate): populated by the macOS and Windows
    // collectors, but no component reads it yet — it is carried so the
    // benchmark can use design voltage without a collector change.
    #[allow(dead_code)]
    pub design_voltage_mv: Option<f64>,
    /// Warnings surfaced by the OS about the battery.
    pub condition_flags: Vec<ConditionFlag>,
}

impl Default for BatterySample {
    fn default() -> Self {
        Self {
            collected_at: SystemTime::UNIX_EPOCH,
            source: SourceKind::Simulator,
            state_of_charge: None,
            design_capacity: None,
            full_charge_capacity: None,
            full_charge_capacity_key: None,
            cycle_limit: None,
            cycle_count: None,
            temperature_c: None,
            voltage_mv: None,
            power_w: None,
            charging_state: None,
            high_soc_dwell_s: None,
            design_voltage_mv: None,
            condition_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum SourceKind {
    MacIoreg,
    MacSystemProfiler,
    WindowsWmi,
    WindowsPowerStatus,
    #[default]
    Simulator,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::MacIoreg => "macOS ioreg (AppleSmartBattery)",
            SourceKind::MacSystemProfiler => "macOS system_profiler",
            SourceKind::WindowsWmi => "Windows WMI/CIM",
            SourceKind::WindowsPowerStatus => "Windows GetSystemPowerStatus",
            SourceKind::Simulator => "simulator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum ChargingState {
    #[default]
    Discharging,
    Charging,
    Full,
    OnAcNotCharging,
    Unknown,
}

// Allowlist (M3 lint gate): variants are constructed only by the
// platform collectors (macOS condition, Windows threshold) — none of
// which build on every host.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionFlag {
    /// macOS "Condition: Replace Soon" / Windows capacity below threshold.
    ServiceRecommended,
    /// Firmware reports the pack cannot provide power.
    Failure,
}

/// State of health: full-charge over design capacity, percent. Either
/// figure at or below zero is "no data", not a zero health reading (a 0
/// full-charge figure would otherwise render as a 0% health figure).
pub fn state_of_health_pct(s: &BatterySample) -> Option<f64> {
    let design = s.design_capacity?;
    if design <= 0.0 {
        return None;
    }
    let fcc = s.full_charge_capacity?;
    if fcc <= 0.0 {
        return None;
    }
    Some(100.0 * fcc / design)
}

// Allowlist (M3 lint gate): the complement of state_of_health_pct,
// reserved for the presentation layer's DoD factor (not yet read).
#[allow(dead_code)]
pub fn depth_of_discharge_pct(s: &BatterySample) -> Option<f64> {
    s.state_of_charge.map(|soc| 100.0 - soc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(soc: f64, design: f64, fcc: f64) -> BatterySample {
        BatterySample {
            state_of_charge: Some(soc),
            design_capacity: Some(design),
            full_charge_capacity: Some(fcc),
            ..Default::default()
        }
    }

    #[test]
    fn soh_is_fcc_over_design() {
        let s = sample(50.0, 50_000.0, 43_000.0);
        let soh = state_of_health_pct(&s).unwrap();
        assert!((soh - 86.0).abs() < 1e-9);
    }

    #[test]
    fn soh_unknown_when_design_missing() {
        let s = BatterySample {
            full_charge_capacity: Some(43_000.0),
            ..Default::default()
        };
        assert!(state_of_health_pct(&s).is_none());
    }

    #[test]
    fn soh_none_on_zero_design() {
        let s = sample(50.0, 0.0, 43_000.0);
        assert!(state_of_health_pct(&s).is_none());
    }

    #[test]
    fn dod_is_complement_of_soc() {
        let s = sample(30.0, 1.0, 1.0);
        assert_eq!(depth_of_discharge_pct(&s), Some(70.0));
    }
}
