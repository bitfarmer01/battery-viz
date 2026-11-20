//! Windows capability adapter: WMI metrics, maker detection, and a
//! per-maker module (Dell, HP, Lenovo, ASUS, Surface, unknown) that reports
//! the cap options and guidance that exist on that machine.
//!
//! Win32_Battery exposes no pack temperature, cycle count, or power draw —
//! those stay declared-unavailable here rather than guessed downstream.

pub mod asus;
pub mod dell;
pub mod hp;
pub mod lenovo;
pub mod surface;
pub mod unknown;

use std::process::Command;

use super::{
    CapOptions, Capabilities, Detectability, Maker, MetricId, Platform, SeedCapacity,
    SettingGuidance, SettingKind,
};
use crate::collect::windows::WindowsCollector;
use crate::collect::{Collector, SourceKind};

/// What one maker's tooling can do on this machine.
pub struct MakerModule {
    pub maker: Maker,
    pub cap_options: CapOptions,
    /// Where the user sets a charge cap.
    pub cap_guidance: Option<SettingGuidance>,
    /// Where the user checks adaptive charging, when there is one to point at.
    pub adaptive_guidance: Option<SettingGuidance>,
}

const WINDOWS_METRICS: &[MetricId] = &[
    MetricId::StateOfCharge,
    MetricId::DesignCapacity,
    MetricId::FullChargeCapacity,
    MetricId::Voltage,
    MetricId::ChargingState,
];

/// Map a `Win32_ComputerSystem` manufacturer string onto a maker.
pub fn detect_maker(manufacturer: &str) -> Maker {
    let m = manufacturer.to_ascii_lowercase();
    if m.contains("dell") {
        Maker::Dell
    } else if m.contains("hp") || m.contains("hewlett") {
        Maker::Hp
    } else if m.contains("lenovo") {
        Maker::Lenovo
    } else if m.contains("asus") {
        Maker::Asus
    } else if m.contains("microsoft") || m.contains("surface") {
        Maker::Surface
    } else {
        Maker::Unknown
    }
}

/// The maker module for this machine.
pub fn module(maker: Maker) -> MakerModule {
    match maker {
        Maker::Dell => dell::module(),
        Maker::Hp => hp::module(),
        Maker::Lenovo => lenovo::module(),
        Maker::Asus => asus::module(),
        Maker::Surface => surface::module(),
        // Apple hardware does not run Windows drivers here in practice;
        // generic advice is the honest answer, not a guessed module.
        Maker::Apple | Maker::Unknown => unknown::module(),
    }
}

pub struct WindowsCapabilities {
    maker: Maker,
}

impl WindowsCapabilities {
    /// Detect the maker from `Win32_ComputerSystem`. A failed or unrecognized
    /// query degrades to `Maker::Unknown`, which still yields a working app
    /// with generic advice (FR-0 acceptance).
    pub fn new() -> Self {
        WindowsCapabilities {
            maker: Self::detect(),
        }
    }

    /// Inject a maker: tests, and callers that detected it themselves.
    pub fn with_maker(maker: Maker) -> Self {
        WindowsCapabilities { maker }
    }

    fn detect() -> Maker {
        let script = "Get-CimInstance -ClassName Win32_ComputerSystem | \
Select-Object -ExpandProperty Manufacturer";
        let detected = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| detect_maker(String::from_utf8_lossy(&out.stdout).trim()));
        // Unreadable maker → Unknown → generic advice. That fallback is the
        // designed behavior for this machine, not a swallowed failure.
        detected.unwrap_or(Maker::Unknown)
    }

    fn maker_module(&self) -> MakerModule {
        module(self.maker)
    }
}

impl Capabilities for WindowsCapabilities {
    fn platform(&self) -> Platform {
        Platform::Windows
    }

    fn maker(&self) -> Maker {
        self.maker
    }

    fn readable_metrics(&self) -> &'static [MetricId] {
        WINDOWS_METRICS
    }

    fn setting_detectability(&self, _kind: SettingKind) -> Detectability {
        // No generic read of a maker tool's charge settings exists (the
        // roadmap's Lenovo/ASUS question is open for every maker); FR-1's
        // behavioral detection is the fallback on Windows.
        Detectability::BehavioralOnly
    }

    fn cap_options(&self) -> CapOptions {
        self.maker_module().cap_options
    }

    fn guidance(&self, kind: SettingKind) -> Option<SettingGuidance> {
        let m = self.maker_module();
        match kind {
            SettingKind::ChargeCap => m.cap_guidance,
            SettingKind::AdaptiveCharging => m.adaptive_guidance,
        }
    }

    fn capacity_seed(&self) -> Vec<SeedCapacity> {
        // The report already holds daily capacity history, so a fresh
        // install's record starts with the machine's real past instead of
        // an empty slate. Any failure — tool missing (off-Windows hosts),
        // restricted shell, unreadable output — degrades to no seed; the
        // record simply starts empty, never a guess.
        let html = battery_report_html().unwrap_or_default();
        parse_battery_report(&html)
    }
}

/// The daily rows of `powercfg /batteryreport`'s "Battery capacity history"
/// table: one `SeedCapacity` per dated row, SoH = full-charge over design.
/// Rows that do not parse (different tool version, empty report) are
/// skipped, never guessed.
pub fn parse_battery_report(html: &str) -> Vec<SeedCapacity> {
    let Some(start) = html.find("Battery capacity history") else {
        return Vec::new();
    };
    let rest = &html[start..];
    let table = &rest[..rest.find("</table>").unwrap_or(rest.len())];
    let mut out = Vec::new();
    for chunk in table.split("<tr>").skip(1) {
        let cells: Vec<&str> = chunk
            .split("<td>")
            .skip(1)
            .map(|c| c.split("</td>").next().unwrap_or("").trim())
            .collect();
        // Row shape: the date leads, then the capacity figures (with
        // thousands separators). Extra columns are ignored.
        let Some(day) = cells.first().and_then(|c| parse_report_date(c)) else {
            continue;
        };
        let nums: Vec<f64> = cells[1..].iter().filter_map(|c| parse_capacity(c)).collect();
        if nums.len() < 2 || nums[1] <= 0.0 {
            continue;
        }
        let (full, design) = (nums[0], nums[1]);
        out.push(SeedCapacity {
            day,
            soh_pct: 100.0 * full / design,
        });
    }
    out
}

/// `M/D/YYYY` (the report's PERIOD format) as days since the Unix epoch.
fn parse_report_date(s: &str) -> Option<u32> {
    let mut it = s.trim().split('/');
    let month: u32 = it.next()?.trim().parse().ok()?;
    let day: u32 = it.next()?.trim().parse().ok()?;
    let year: i64 = it.next()?.trim().parse().ok()?;
    days_from_civil(year, month, day)
}

/// Strip thousands separators and parse a capacity figure.
fn parse_capacity(s: &str) -> Option<f64> {
    s.replace(',', "").trim().parse::<f64>().ok()
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's
/// `days_from_civil`), so report dates line up with `clock::LocalNow.day`
/// without a date-time dependency.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<u32> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1970..=9999).contains(&year) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(month) + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    u32::try_from(era * 146_097 + doe - 719_468).ok()
}

/// Generate and read the report HTML. `None` when the tool is unavailable
/// or fails (off-Windows hosts, restricted shells); the temp output file is
/// always removed.
fn battery_report_html() -> Option<String> {
    let path = std::env::temp_dir().join(format!("battery-viz-report-{}.html", std::process::id()));
    let output = Command::new("powercfg")
        .args(["/batteryreport", "/output"])
        .arg(&path)
        .output()
        .ok()?;
    let html = if output.status.success() {
        std::fs::read_to_string(&path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&path);
    html
}

// The collector is stateless, so a fresh instance per call is free and the
// adapter holds no duplicate state.
impl Collector for WindowsCapabilities {
    fn source(&self) -> SourceKind {
        WindowsCollector::new().source()
    }

    fn sample(&mut self) -> Result<crate::collect::types::BatterySample, String> {
        WindowsCollector::new().sample()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maker_detection_matches_manufacturer_strings() {
        for (s, expected) in [
            ("Dell Inc.", Maker::Dell),
            ("HP", Maker::Hp),
            ("Hewlett-Packard", Maker::Hp),
            ("LENOVO", Maker::Lenovo),
            ("ASUSTeK COMPUTER INC.", Maker::Asus),
            ("Microsoft Corporation", Maker::Surface),
            ("Contoso Ltd.", Maker::Unknown),
            ("To Be Filled By O.E.M.", Maker::Unknown),
            ("System manufacturer", Maker::Unknown),
        ] {
            assert_eq!(detect_maker(s), expected, "manufacturer {s:?}");
        }
    }

    #[test]
    fn every_maker_module_matches_its_maker_and_gives_cap_advice() {
        for maker in [
            Maker::Dell,
            Maker::Hp,
            Maker::Lenovo,
            Maker::Asus,
            Maker::Surface,
            Maker::Unknown,
        ] {
            let m = module(maker);
            assert_eq!(m.maker, maker);
            let g = m.cap_guidance.expect("cap guidance");
            assert!(!g.steps.is_empty(), "{maker:?} cap guidance is empty");
            assert!(!g.tool.is_empty(), "{maker:?} cap guidance has no tool");
        }
    }

    #[test]
    fn unknown_maker_claims_no_cap_options() {
        let caps = WindowsCapabilities::with_maker(Maker::Unknown);
        assert!(caps.cap_options().is_none());
        // …but still answers all three questions (FR-0 acceptance: a working
        // app with generic advice on an unknown maker).
        assert!(!caps.readable_metrics().is_empty());
        assert!(caps.guidance(SettingKind::ChargeCap).is_some());
    }

    #[test]
    fn declared_metrics_match_wmi_parse() {
        let caps = WindowsCapabilities::with_maker(Maker::Dell);
        // Same Win32_Battery field values the old Format-List text parser
        // test used, now fed through the in-process WMI decode path.
        let raw = crate::collect::windows::RawWmiBattery {
            design_capacity_mwh: Some(60000.0),
            full_charge_capacity_mwh: Some(48000.0),
            design_voltage_mv: Some(11400.0),
            estimated_charge_remaining_pct: Some(65.0),
            battery_status: Some(2),
        };
        let s = crate::collect::windows::build_sample(raw).expect("parse");
        for m in super::super::ALL_METRICS {
            assert_eq!(
                m.is_present(&s),
                caps.readable_metrics().contains(m),
                "metric {m:?} declaration vs WMI parse mismatch"
            );
        }
    }

    const REPORT_FIXTURE: &str = r#"<html><body>
    <h2>Battery capacity history</h2>
    <table>
    <tr><th>PERIOD</th><th>FULL CHARGE CAPACITY</th><th>DESIGN CAPACITY</th></tr>
    <tr><td>11/18/2026</td><td>44,900</td><td>50,000</td></tr>
    <tr><td>11/19/2026</td><td>44,700</td><td>50,000</td></tr>
    <tr><td>not a date</td><td>1</td><td>2</td></tr>
    </table>
    <h2>Recent usage</h2><table><tr><td>11/20/2026</td><td>9,999</td><td>50,000</td></tr></table>
    </body></html>"#;

    #[test]
    fn battery_report_capacity_history_becomes_seed_rows() {
        let seeds = parse_battery_report(REPORT_FIXTURE);
        assert_eq!(seeds.len(), 2, "header and non-date rows skipped; later tables excluded");
        assert_eq!(seeds[0].day, days_from_civil(2026, 11, 18).unwrap());
        assert!((seeds[0].soh_pct - 100.0 * 44_900.0 / 50_000.0).abs() < 1e-9);
        assert!((seeds[1].soh_pct - 100.0 * 44_700.0 / 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn battery_report_without_capacity_history_seeds_nothing() {
        assert!(parse_battery_report("<html>no report here</html>").is_empty());
        assert!(parse_battery_report("").is_empty());
    }

    #[test]
    fn report_dates_map_to_epoch_days() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2026, 11, 20), Some(20_777));
        assert_eq!(days_from_civil(2024, 2, 29), Some(19_782), "leap day");
        assert_eq!(days_from_civil(2026, 13, 1), None);
        assert_eq!(days_from_civil(2026, 0, 1), None);
        assert_eq!(parse_report_date("1/5/2026"), parse_report_date("01/05/2026"));
    }
}
