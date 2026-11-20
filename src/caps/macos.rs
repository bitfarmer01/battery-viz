//! macOS capability adapter: ioreg metrics plus the System Settings battery
//! pane. Apple exposes no documented way to read the charge limit or
//! Optimized Battery Charging state (roadmap FR-1/FR-5), so both settings
//! answer BehavioralOnly and FR-1's behavioural detection is the fallback.

use super::{
    CapOptions, CapRange, Capabilities, Detectability, Maker, MetricId, Platform, SettingGuidance,
    SettingKind,
};
use crate::collect::macos::MacCollector;
use crate::collect::{Collector, SourceKind};

/// macOS 26.4+ on Apple silicon offers the charge limit at 80–100% in 5%
/// steps (roadmap FR-2). No programmatic read of the current value.
const MAC_CAP_RANGE: CapRange = CapRange {
    min: 80,
    max: 100,
    step: 5,
};

const MAC_METRICS: &[MetricId] = &[
    MetricId::StateOfCharge,
    MetricId::DesignCapacity,
    MetricId::FullChargeCapacity,
    MetricId::CycleCount,
    MetricId::CycleLimit,
    MetricId::Temperature,
    MetricId::Voltage,
    MetricId::Power,
    MetricId::ChargingState,
];

pub struct MacCapabilities;

impl MacCapabilities {
    pub fn new() -> Self {
        MacCapabilities
    }
}

impl Capabilities for MacCapabilities {
    fn platform(&self) -> Platform {
        Platform::Mac
    }

    fn maker(&self) -> Maker {
        Maker::Apple
    }

    fn readable_metrics(&self) -> &'static [MetricId] {
        MAC_METRICS
    }

    fn setting_detectability(&self, _kind: SettingKind) -> Detectability {
        // Neither the charge limit nor Optimized Battery Charging is exposed
        // by a documented API; infer from charge behaviour instead.
        Detectability::BehavioralOnly
    }

    fn cap_options(&self) -> CapOptions {
        CapOptions {
            fixed: &[],
            range: Some(MAC_CAP_RANGE),
            adaptive: true,
        }
    }

    fn guidance(&self, kind: SettingKind) -> Option<SettingGuidance> {
        match kind {
            SettingKind::ChargeCap => Some(SettingGuidance {
                tool: "System Settings",
                steps: "System Settings → Battery → Charging and set the charge limit (80%–100%)",
                // System Settings URL scheme for the battery pane; if the
                // link fails on a given macOS version, the steps stand alone.
                url: Some("x-apple.systempreferences:com.apple.Battery-Settings.extension"),
            }),
            SettingKind::AdaptiveCharging => Some(SettingGuidance {
                tool: "System Settings",
                steps: "System Settings → Battery → Charging and check Optimized Battery Charging",
                url: Some("x-apple.systempreferences:com.apple.Battery-Settings.extension"),
            }),
        }
    }
}

impl Collector for MacCapabilities {
    fn source(&self) -> SourceKind {
        MacCollector::new().source()
    }

    fn sample(&mut self) -> Result<crate::collect::types::BatterySample, String> {
        MacCollector::new().sample()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_range_is_80_to_100_in_5_percent_steps() {
        let caps = MacCapabilities::new();
        let range = caps.cap_options().range.expect("macOS cap range");
        assert_eq!((range.min, range.max, range.step), (80, 100, 5));
        assert!(caps.cap_options().fixed.is_empty());
    }

    #[test]
    fn both_settings_are_behavioral_only_with_guidance() {
        let caps = MacCapabilities::new();
        for kind in [SettingKind::ChargeCap, SettingKind::AdaptiveCharging] {
            assert_eq!(
                caps.setting_detectability(kind),
                Detectability::BehavioralOnly
            );
            let g = caps.guidance(kind).expect("guidance");
            assert!(!g.steps.is_empty());
        }
    }

    #[test]
    fn declared_metrics_match_ioreg_parse() {
        let caps = MacCapabilities::new();
        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<plist version=\"1.0\">\n<array>\n  <dict>\n\
    <key>CycleCount</key><integer>214</integer>\n\
    <key>CycleLimit</key><integer>1000</integer>\n\
    <key>DesignCapacity</key><integer>58030</integer>\n\
    <key>NominalChargeCapacity</key><integer>53120</integer>\n\
    <key>Temperature</key><integer>2912</integer>\n\
    <key>Voltage</key><integer>12531</integer>\n\
    <key>Amperage</key><integer>-1200</integer>\n\
    <key>CurrentCapacity</key><integer>73</integer>\n\
    <key>ExternalConnected</key><false/>\n\
    <key>IsCharging</key><false/>\n\
  </dict>\n</array>\n</plist>";
        let s = crate::collect::macos::parse_ioreg_plist(xml).expect("parse");
        for m in super::super::ALL_METRICS {
            assert_eq!(
                m.is_present(&s),
                caps.readable_metrics().contains(m),
                "metric {m:?} declaration vs ioreg parse mismatch"
            );
        }
    }
}
