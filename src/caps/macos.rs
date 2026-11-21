//! macOS capability adapter: ioreg metrics plus the System Settings battery
//! pane. Apple exposes no documented way to read the charge limit or
//! Optimized Battery Charging state (roadmap FR-1/FR-5), so both settings
//! answer BehavioralOnly and FR-1's behavioural detection is the fallback.

use std::io::Cursor;
use std::process::Command;

use super::{
    CapOptions, CapRange, Capabilities, Detectability, LifetimeRecord, Maker, MetricId, Platform,
    SettingGuidance, SettingKind,
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

    fn lifetime_record(&self) -> Option<LifetimeRecord> {
        // LifetimeData sits INSIDE the BatteryData child dict of the
        // battery entry on real Apple-silicon macOS (verified on a
        // MacBook Air M4, macOS 26.6.2 — re-audit N8; the previous
        // top-level lookup silently found nothing on that machine, so
        // first-run history import never ran). Both the battery entry
        // and the child pack entry are asked; no top-level fallback is
        // attempted because no evidence of that historical layout
        // exists.
        let battery = ioreg_battery_dict("AppleSmartBattery");
        let pack = ioreg_battery_dict("AppleSmartBatteryPack");
        for dict in [battery, pack].into_iter().flatten() {
            if let Some(record) = parse_lifetime_record(&dict) {
                return Some(record);
            }
        }
        // Absent on some models and versions, unreadable on others: no
        // record, no error — the import simply does not happen.
        None
    }
}

/// Run ioreg for one battery entry and return its property dictionary.
/// Any failure — spawn, exit status, empty output, plist parse, wrong
/// shape — is `None`; the lifetime record is optional end to end.
fn ioreg_battery_dict(name: &str) -> Option<plist::Dictionary> {
    let out = Command::new("ioreg")
        .args(["-r", "-n", name, "-d", "1", "-a", "-x"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = plist::Value::from_reader(Cursor::new(&out.stdout)).ok()?;
    // `ioreg -r -d 1` emits an array of dicts (or a single dict on some
    // versions) — the same shape the collector's parser handles.
    let dict = value
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(value);
    dict.as_dictionary().cloned()
}

/// The inferred `LifetimeData` layout, validated strictly.
///
/// On real Apple-silicon macOS (MacBook Air M4, macOS 26.6.2 — re-audit
/// N8) the `LifetimeData` block sits INSIDE the `BatteryData` child dict,
/// not at the top level of the battery entry. Only that verified layout
/// is read; a top-level block is not looked for because no evidence of
/// that historical layout exists.
///
/// Only `TotalOperatingTime` is exported, because it is the only figure
/// whose semantics survive scrutiny: it is lifetime hours, and it is a
/// calendar-speed clock (it rises in sleep and with the lid shut), so it
/// reads as pack age, never time-in-use — measured so on macOS 14.2.1
/// through 27.2. The `TimeAtHighSoc` blob beside it (28 little-endian
/// u32 counters on the machines that expose it, absent on all macOS 27
/// machines measured) deliberately stays unread: which counter holds
/// which charge band is not established anywhere, and an interpretation
/// exported as data would be exactly the guess the roadmap forbids.
///
/// Anything else — key missing, wrong type, negative, beyond a sanity
/// bound — fails silently: `None`, no import, no error (FR-8 acceptance).
fn parse_lifetime_record(dict: &plist::Dictionary) -> Option<LifetimeRecord> {
    let battery_data = dict.get("BatteryData")?.as_dictionary()?;
    let lifetime = battery_data.get("LifetimeData")?.as_dictionary()?;
    let hours = lifetime.get("TotalOperatingTime")?.as_signed_integer()?;
    let hours_f = hours as f64;
    // Sanity bound: a pack cannot have outlived 23 years of continuous
    // operation; a figure past 200 000 h is a misparse, not a reading.
    if !(hours_f > 0.0 && hours_f < 200_000.0) {
        return None;
    }
    Some(LifetimeRecord {
        operating_hours: hours_f,
    })
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
    <key>DesignCycleCount9C</key><integer>1000</integer>\n\
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

    fn dict_from_xml(xml: &str) -> plist::Dictionary {
        let v = plist::Value::from_reader(Cursor::new(xml.as_bytes())).expect("plist parse");
        v.as_dictionary().expect("dictionary").clone()
    }

    #[test]
    fn lifetime_record_reads_battery_data_nesting() {
        // Verified real layout (MacBook Air M4, macOS 26.6.2 — re-audit
        // N8): LifetimeData lives INSIDE BatteryData. The previous code
        // read the top level and silently found nothing, so first-run
        // import never ran on this machine.
        let dict = dict_from_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>BatteryData</key><dict>
    <key>LifetimeData</key><dict>
      <key>TotalOperatingTime</key><integer>12345</integer>
    </dict>
  </dict>
</dict></plist>"#,
        );
        let record = parse_lifetime_record(&dict).expect("record from verified nesting");
        assert_eq!(record.operating_hours, 12345.0);
    }

    #[test]
    fn lifetime_record_ignores_top_level_lifetime_data() {
        // Locks in the verified layout: a top-level LifetimeData block —
        // the shape the old (broken) code looked for — is NOT read. No
        // evidence of that historical layout exists, so the parser does
        // not guess at it.
        let dict = dict_from_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>LifetimeData</key><dict>
    <key>TotalOperatingTime</key><integer>12345</integer>
  </dict>
</dict></plist>"#,
        );
        assert!(parse_lifetime_record(&dict).is_none());
    }

    #[test]
    fn lifetime_record_sanitizes_implausible_hours() {
        for hours in ["0", "-5", "200001"] {
            let dict = dict_from_xml(&format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>BatteryData</key><dict>
    <key>LifetimeData</key><dict>
      <key>TotalOperatingTime</key><integer>{hours}</integer>
    </dict>
  </dict>
</dict></plist>"#
            ));
            assert!(
                parse_lifetime_record(&dict).is_none(),
                "hours {hours} must fail"
            );
        }
    }
}
