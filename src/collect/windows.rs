//! Windows collector: WMI battery classes over an in-process COM connection.
//!
//! `Win32_Battery` gives capacity fields on most OEM firmware. The query
//! runs in-process through the `wmi` crate on the collector thread — one
//! connection, reused, no child process per sample. Native
//! `IOCTL_BATTERY_QUERY_*` support (cycle count, temperature) is the
//! planned upgrade path behind the same interface.

use crate::collect::types::{BatterySample, ChargingState, SourceKind};
use crate::collect::Collector;

#[allow(dead_code)]
#[derive(Default)]
pub struct WindowsCollector;

#[allow(dead_code)]
impl WindowsCollector {
    pub fn new() -> Self {
        WindowsCollector
    }
}

impl Collector for WindowsCollector {
    fn source(&self) -> SourceKind {
        SourceKind::WindowsWmi
    }

    #[cfg(windows)]
    fn sample(&mut self) -> Result<BatterySample, String> {
        use std::cell::RefCell;

        thread_local! {
            // WMIConnection owns raw COM interface pointers and is not
            // Send, so it must not live in `self`: collectors are moved
            // onto the sampling thread (`Collector: Send`). Keep one
            // connection per sampling thread in thread-local storage —
            // created lazily, reused by every sample on that thread.
            static WMI_CON: RefCell<Option<wmi::WMIConnection>> = const { RefCell::new(None) };
        }

        WMI_CON.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let com = wmi::COMLibrary::new().map_err(|e| format!("COM init failed: {e}"))?;
                let con =
                    wmi::WMIConnection::new(com).map_err(|e| format!("WMI connect failed: {e}"))?;
                *slot = Some(con);
            }
            let con = slot.as_ref().expect("wmi connection just created");
            build_sample(super_windows_query(con)?)
        })
    }

    #[cfg(not(windows))]
    fn sample(&mut self) -> Result<BatterySample, String> {
        Err("Windows collector requires Windows (WMI)".into())
    }
}

/// The Win32_Battery fields the decoder consumes, normalized to plain
/// numbers so the decoding logic below is testable off-Windows.
/// Off-Windows the live query is compiled out and only tests construct it.
#[derive(Debug, Clone, Default, PartialEq)]
#[allow(dead_code)]
pub struct RawWmiBattery {
    pub design_capacity_mwh: Option<f64>,
    pub full_charge_capacity_mwh: Option<f64>,
    pub design_voltage_mv: Option<f64>,
    pub estimated_charge_remaining_pct: Option<f64>,
    pub battery_status: Option<u32>,
}

/// Decode Win32_Battery.BatteryStatus. It is an *enumerated* code from the
/// DMTF Portable Battery profile, not a bitfield (Win32_Battery class docs,
/// Microsoft Learn): 1 discharging, 2 on AC (not necessarily charging),
/// 3 fully charged, 4 low, 5 critical, 6-9 charging variants, 10 undefined,
/// 11 partially charged.
#[allow(dead_code)] // reached only by the cfg(windows) live path off-tests
pub fn decode_battery_status(status: u32) -> ChargingState {
    match status {
        1 | 4 | 5 => ChargingState::Discharging,
        2 => ChargingState::OnAcNotCharging,
        3 => ChargingState::Full,
        6..=9 => ChargingState::Charging,
        // Partially charged (11) and undefined/undocumented codes carry no
        // charge-direction information; report Unknown, never a guess.
        _ => ChargingState::Unknown,
    }
}

/// Build a sample from raw WMI fields. Errors only when no battery data at
/// all is present; missing individual fields stay `None` ("unknown", not
/// "healthy").
#[allow(dead_code)] // reached only by the cfg(windows) live path off-tests
pub fn build_sample(raw: RawWmiBattery) -> Result<BatterySample, String> {
    if raw.design_capacity_mwh.is_none()
        && raw.full_charge_capacity_mwh.is_none()
        && raw.estimated_charge_remaining_pct.is_none()
    {
        return Err("no battery reported by WMI".into());
    }

    Ok(BatterySample {
        collected_at: std::time::SystemTime::now(),
        source: SourceKind::WindowsWmi,
        state_of_charge: raw.estimated_charge_remaining_pct,
        design_capacity: raw.design_capacity_mwh,
        full_charge_capacity: raw.full_charge_capacity_mwh,
        // N10: the WMI field name is the provenance — the label names it
        // exactly only when the figure actually came from it.
        full_charge_capacity_key: raw.full_charge_capacity_mwh.map(|_| "FullChargeCapacity"),
        cycle_limit: None,
        cycle_count: None,
        temperature_c: None,
        voltage_mv: raw.design_voltage_mv,
        power_w: None,
        charging_state: raw.battery_status.map(decode_battery_status),
        high_soc_dwell_s: None,
        design_voltage_mv: raw.design_voltage_mv,
        condition_flags: Vec::new(),
    })
}

#[cfg(windows)]
#[allow(dead_code)] // reached via Collector::sample; check-build reachability is noisy like the baseline
fn super_windows_query(con: &wmi::WMIConnection) -> Result<RawWmiBattery, String> {
    use std::collections::HashMap;
    use wmi::Variant;

    let rows: Vec<HashMap<String, Variant>> = con
        .raw_query(
            "SELECT DesignCapacity, FullChargeCapacity, DesignVoltage, \
             EstimatedChargeRemaining, BatteryStatus FROM Win32_Battery",
        )
        .map_err(|e| format!("WMI query failed: {e}"))?;
    let row = rows
        .into_iter()
        .next()
        .ok_or("no battery reported by WMI")?;

    Ok(RawWmiBattery {
        design_capacity_mwh: num_field(&row, "DesignCapacity"),
        full_charge_capacity_mwh: num_field(&row, "FullChargeCapacity"),
        design_voltage_mv: num_field(&row, "DesignVoltage"),
        estimated_charge_remaining_pct: num_field(&row, "EstimatedChargeRemaining"),
        battery_status: num_field(&row, "BatteryStatus").map(|v| v as u32),
    })
}

#[cfg(windows)]
#[allow(dead_code)] // reached via super_windows_query
/// WMI numeric properties arrive as one of several VARIANT numeric types
/// (uint16 BatteryStatus, uint32 DesignCapacity, uint64 DesignVoltage, ...);
/// accept any of them.
fn num_field(row: &std::collections::HashMap<String, wmi::Variant>, key: &str) -> Option<f64> {
    use wmi::Variant;
    match row.get(key)? {
        Variant::I1(v) => Some(f64::from(*v)),
        Variant::I2(v) => Some(f64::from(*v)),
        Variant::I4(v) => Some(f64::from(*v)),
        Variant::I8(v) => Some(*v as f64),
        Variant::UI1(v) => Some(f64::from(*v)),
        Variant::UI2(v) => Some(f64::from(*v)),
        Variant::UI4(v) => Some(f64::from(*v)),
        Variant::UI8(v) => Some(*v as f64),
        Variant::R4(v) => Some(f64::from(*v)),
        Variant::R8(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Audit M4: no real Windows machine is available in this sandbox, so
    // this fixture is SYNTHESIZED and labeled as such. Field semantics
    // (units, the enumerated BatteryStatus codes) follow the Win32_Battery
    // class documentation on Microsoft Learn, retrieved 2026-11-20.
    const WIN32_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/win32_battery_sample.txt"
    ));

    fn parse_fixture(text: &str) -> RawWmiBattery {
        let mut raw = RawWmiBattery::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once(':').expect("fixture line has key: value");
            let value: u64 = value.trim().parse().expect("fixture value is an integer");
            match key.trim() {
                "DesignCapacity" => raw.design_capacity_mwh = Some(value as f64),
                "FullChargeCapacity" => raw.full_charge_capacity_mwh = Some(value as f64),
                "DesignVoltage" => raw.design_voltage_mv = Some(value as f64),
                "EstimatedChargeRemaining" => {
                    raw.estimated_charge_remaining_pct = Some(value as f64)
                }
                "BatteryStatus" => raw.battery_status = Some(value as u32),
                other => panic!("unknown fixture key: {other}"),
            }
        }
        raw
    }

    #[test]
    fn fixture_fields_reach_the_sample() {
        let s = build_sample(parse_fixture(WIN32_FIXTURE)).expect("parse");
        assert_eq!(s.design_capacity, Some(60000.0));
        assert_eq!(s.full_charge_capacity, Some(48000.0));
        assert_eq!(s.state_of_charge, Some(65.0));
        assert_eq!(s.voltage_mv, Some(11400.0));
        assert_eq!(s.design_voltage_mv, Some(11400.0));
    }

    #[test]
    fn status_2_is_on_ac_not_charging() {
        // The old bitfield decode read status 2 as Discharging; the docs
        // say "the system has access to AC so no battery is being
        // discharged", with no implication that it is charging either.
        let s = build_sample(parse_fixture(WIN32_FIXTURE)).expect("parse");
        assert_eq!(s.charging_state, Some(ChargingState::OnAcNotCharging));
    }

    #[test]
    fn battery_status_enum_is_decoded_per_documentation() {
        let cases = [
            (1, ChargingState::Discharging),
            (2, ChargingState::OnAcNotCharging),
            (3, ChargingState::Full),
            (4, ChargingState::Discharging),
            (5, ChargingState::Discharging),
            (6, ChargingState::Charging),
            (7, ChargingState::Charging),
            (9, ChargingState::Charging),
            (10, ChargingState::Unknown),
            (11, ChargingState::Unknown),
            (0, ChargingState::Unknown),
        ];
        for (status, expected) in cases {
            assert_eq!(decode_battery_status(status), expected, "status {status}");
        }
    }

    #[test]
    fn fully_charged_status_decodes_to_full() {
        let mut raw = parse_fixture(WIN32_FIXTURE);
        raw.battery_status = Some(3);
        let s = build_sample(raw).expect("parse");
        assert_eq!(s.charging_state, Some(ChargingState::Full));
    }

    #[test]
    fn missing_fields_error() {
        assert!(build_sample(RawWmiBattery::default()).is_err());
    }

    #[test]
    fn missing_status_leaves_state_unknown() {
        let raw = RawWmiBattery {
            estimated_charge_remaining_pct: Some(65.0),
            ..Default::default()
        };
        let s = build_sample(raw).expect("parse");
        assert_eq!(s.charging_state, None);
    }
}
