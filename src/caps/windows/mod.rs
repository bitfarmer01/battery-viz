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
/// Real reports (verified against a captured Windows 10 report; see PR):
/// the heading is title-case, `<tr>`/`<td>` tags can carry class attributes,
/// PERIOD cells are ISO `YYYY-MM-DD` dates — a weekly range or a single
/// day, not the US `M/D/YYYY` the first parser version expected — and
/// capacity figures carry a ` mWh` suffix with a locale-dependent thousands
/// separator (`,` US, `.` de, space/nbsp fr). The parser is tolerant of all
/// of it; rows that still do not parse are skipped, never guessed.
pub fn parse_battery_report(html: &str) -> Vec<SeedCapacity> {
    // ASCII-lowercased probe for the heading (case-insensitive without
    // shifting byte offsets), but slice the ORIGINAL so header cells keep
    // their case for the column detection below.
    let Some(start) = html.to_ascii_lowercase().find("battery capacity history") else {
        return Vec::new();
    };
    let rest = &html[start..];
    let table = &rest[..rest.find("</table>").unwrap_or(rest.len())];

    // Column positions from the header row when one is present (real
    // reports carry `PERIOD | FULL CHARGE CAPACITY | DESIGN CAPACITY`);
    // with no recognizable header, fall back to the positional layout
    // (the date leads, then the two figures).
    let mut full_col: Option<usize> = None;
    let mut design_col: Option<usize> = None;

    let mut out = Vec::new();
    for chunk in table.split("<tr").skip(1) {
        let cells = row_cells(chunk);
        let lower: Vec<String> = cells.iter().map(|c| c.to_ascii_lowercase()).collect();
        if let Some(f) = lower
            .iter()
            .position(|c| c.contains("full charge capacity"))
        {
            full_col = Some(f);
            design_col = lower.iter().position(|c| c.contains("design capacity"));
            continue; // header row, not data
        }
        // The PERIOD cell is the first cell that parses as a date; the
        // figures follow it. A weekly-range cell (`2019-06-03 -
        // 2019-06-10`) parses to its leading day.
        let Some((day_idx, day)) = cells
            .iter()
            .enumerate()
            .find_map(|(i, c)| parse_report_date(c).map(|d| (i, d)))
        else {
            continue;
        };
        let parsed = match (full_col, design_col) {
            (Some(f), Some(d)) if f > day_idx && d > day_idx => {
                (parse_capacity(&cells[f]), parse_capacity(&cells[d]))
            }
            // No header seen: first two numeric cells after the date.
            _ => {
                let nums: Vec<f64> = cells[day_idx + 1..]
                    .iter()
                    .filter_map(|c| parse_capacity(c))
                    .collect();
                if nums.len() < 2 {
                    continue;
                }
                (Some(nums[0]), Some(nums[1]))
            }
        };
        let (Some(full), Some(design)) = parsed else {
            continue;
        };
        if design <= 0.0 {
            continue;
        }
        out.push(SeedCapacity {
            day,
            soh_pct: 100.0 * full / design,
        });
    }
    out
}

/// The text of every `<td>`/`<th>` cell in one `<tr>` chunk (the chunk
/// starts after `<tr`, which may itself carry attributes). Attribute
/// variants like `<td class="mw">` are handled by skipping to the tag's
/// closing `>`; malformed cells yield what is there.
fn row_cells(row_chunk: &str) -> Vec<String> {
    row_chunk
        .split("<td")
        .chain(row_chunk.split("<th").skip(1))
        .skip(1)
        .filter_map(|c| {
            let after_tag = c.split_once('>')?.1;
            let text = after_tag.split("</").next().unwrap_or("");
            Some(text.trim().to_string())
        })
        .collect()
}

/// The report's PERIOD cell as days since the Unix epoch. Real reports use
/// ISO `YYYY-MM-DD` — a single day or the leading end of a weekly range
/// (`2019-06-03 - 2019-06-10`), possibly with a trailing time; older
/// locale-rendered reports use `M/D/YYYY`. Whichever shape, the leading
/// date wins.
fn parse_report_date(s: &str) -> Option<u32> {
    let t = s.trim();
    let bytes = t.as_bytes();
    // ISO: dddd-dd-dd (leading 10 chars of "2019-06-03 …").
    if t.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        if let (Ok(year), Ok(month), Ok(day)) = (
            t[..4].parse::<i64>(),
            t[5..7].parse::<u32>(),
            t[8..10].parse::<u32>(),
        ) {
            if let Some(day_num) = days_from_civil(year, month, day) {
                return Some(day_num);
            }
        }
    }
    // US fallback: M/D/YYYY.
    let mut it = t.split('/');
    let month: u32 = it.next()?.trim().parse().ok()?;
    let day: u32 = it.next()?.trim().parse().ok()?;
    let year: i64 = it.next()?.trim().parse().ok()?;
    days_from_civil(year, month, day)
}

/// Parse a capacity figure: integer mWh with a locale-dependent thousands
/// separator (`,` US, `.` de, space/nbsp fr, `'` ch) and an optional
/// `mWh` suffix. Digits survive; everything else is separator or noise —
/// a cell that is not a figure parses to `None` and the row is skipped.
fn parse_capacity(s: &str) -> Option<f64> {
    let digits: String = s
        .trim()
        .trim_end_matches("mWh")
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<f64>().ok()
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
        assert_eq!(
            seeds.len(),
            2,
            "header and non-date rows skipped; later tables excluded"
        );
        assert_eq!(seeds[0].day, days_from_civil(2026, 11, 18).unwrap());
        assert!((seeds[0].soh_pct - 100.0 * 44_900.0 / 50_000.0).abs() < 1e-9);
        assert!((seeds[1].soh_pct - 100.0 * 44_700.0 / 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn battery_report_without_capacity_history_seeds_nothing() {
        assert!(parse_battery_report("<html>no report here</html>").is_empty());
        assert!(parse_battery_report("").is_empty());
    }

    // N11c: the shapes a REAL report has (verified against a captured
    // Windows 10 report): class attributes on tr/td, ISO PERIOD dates
    // (weekly ranges and single days), ` mWh` suffixes, and a
    // dot-as-thousands-separator locale. The first parser version
    // (bare `<td>` tags, US dates, comma separators) matched none of it.
    const REAL_REPORT_FIXTURE: &str = r#"<html><body>
    <h2>Battery capacity history</h2>
    <table>
    <tr class="label"><th>PERIOD </th><th>FULL CHARGE CAPACITY </th><th>DESIGN CAPACITY </th></tr>
    <tr class="even"><td>2019-06-03
    - 2019-06-10</td><td class="mw">37.318 mWh</td><td class="mw">38.152 mWh</td></tr>
    <tr class="odd"><td>2019-09-16</td><td class="mw">35.452 mWh</td><td class="mw">38.152 mWh</td></tr>
    <tr class="even"><td colspan="3">no row here</td></tr>
    </table>
    <h2>Battery life estimates</h2><table>
    <tr><th>PERIOD</th><th>FULL CHARGE CAPACITY</th><th>DESIGN CAPACITY</th></tr>
    <tr><td>2019-09-28</td><td class="mw">1 mWh</td><td class="mw">2 mWh</td></tr>
    </table>
    </body></html>"#;

    #[test]
    fn real_report_shapes_parse_with_iso_dates_and_locale_figures() {
        let seeds = parse_battery_report(REAL_REPORT_FIXTURE);
        assert_eq!(
            seeds.len(),
            2,
            "header and non-date rows skipped; later tables excluded"
        );
        assert_eq!(
            seeds[0].day,
            days_from_civil(2019, 6, 3).unwrap(),
            "weekly range → leading day"
        );
        assert_eq!(
            seeds[1].day,
            days_from_civil(2019, 9, 16).unwrap(),
            "single ISO day"
        );
        assert!((seeds[0].soh_pct - 100.0 * 37_318.0 / 38_152.0).abs() < 1e-9);
        assert!((seeds[1].soh_pct - 100.0 * 35_452.0 / 38_152.0).abs() < 1e-9);
    }

    #[test]
    fn report_dates_map_to_epoch_days() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2026, 11, 20), Some(20_777));
        assert_eq!(days_from_civil(2024, 2, 29), Some(19_782), "leap day");
        assert_eq!(days_from_civil(2026, 13, 1), None);
        assert_eq!(days_from_civil(2026, 0, 1), None);
        assert_eq!(
            parse_report_date("1/5/2026"),
            parse_report_date("01/05/2026")
        );
        // N11c: real reports lead with ISO dates (single day or a weekly
        // range's first end).
        assert_eq!(
            parse_report_date("2019-06-03"),
            parse_report_date("06/03/2019")
        );
        assert_eq!(
            parse_report_date("2019-06-03 - 2019-06-10"),
            parse_report_date("2019-06-03")
        );
        assert_eq!(
            parse_report_date("2019-06-03 08:00:00"),
            parse_report_date("2019-06-03"),
            "trailing time ignored"
        );
    }
}
