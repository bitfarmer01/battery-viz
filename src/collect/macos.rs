//! macOS collector: reads AppleSmartBattery from the I/O Registry.
//!
//! `ioreg -r -n AppleSmartBattery -d 1 -a -x` emits an XML property list with
//! CycleCount, DesignCapacity, FullChargeCapacity/NominalChargeCapacity,
//! Temperature (hundredths of °C), Voltage (mV) and ExternalConnected. No root
//! required on stock macOS.

use std::io::Cursor;
use std::process::Command;

use crate::collect::types::{BatterySample, ChargingState, ConditionFlag, SourceKind};
use crate::collect::Collector;

#[allow(dead_code)]
pub struct MacCollector;

#[allow(dead_code)]
impl MacCollector {
    pub fn new() -> Self {
        MacCollector
    }
}

impl Collector for MacCollector {
    fn source(&self) -> SourceKind {
        SourceKind::MacIoreg
    }

    fn sample(&mut self) -> Result<BatterySample, String> {
        let out = Command::new("ioreg")
            .args(["-r", "-n", "AppleSmartBattery", "-d", "1", "-a", "-x"])
            .output()
            .map_err(|e| format!("failed to spawn ioreg: {e}"))?;
        if !out.status.success() {
            return Err(format!("ioreg exited {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr)));
        }
        parse_ioreg_plist(&out.stdout)
    }
}

/// Parse the XML plist emitted by ioreg into a sample.
pub fn parse_ioreg_plist(xml: &[u8]) -> Result<BatterySample, String> {
    let value: plist::Value =
        plist::Value::from_reader(Cursor::new(xml)).map_err(|e| format!("ioreg plist parse: {e}"))?;
    // `ioreg -r -d 1` emits an array of dicts (or a single dict on some versions).
    let dict = value
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .or_else(|| Some(value.clone()))
        .ok_or("empty ioreg output")?
        .as_dictionary()
        .ok_or("ioreg output is not a dictionary")?
        .clone();

    // ioreg emits integers; `as_real` only reads Real values, so fall back.
    let get_f64 = |key: &str| {
        dict.get(key).and_then(|v| {
            v.as_real().or_else(|| v.as_signed_integer().map(|i| i as f64))
        })
    };
    let get_u32 =
        |key: &str| dict.get(key).and_then(plist::Value::as_signed_integer).map(|v| v as u32);

    // Firmware-dependent naming: modern packs report NominalChargeCapacity.
    let full_charge = get_f64("FullChargeCapacity").or_else(|| get_f64("NominalChargeCapacity"));

    let temperature_c = get_f64("Temperature").map(|t| t / 100.0);

    let external_connected = dict
        .get("ExternalConnected")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false);
    let is_charging = dict
        .get("IsCharging")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false);
    let fully_charged = dict
        .get("FullyCharged")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false);

    let charging_state = if fully_charged && external_connected {
        ChargingState::Full
    } else if is_charging {
        ChargingState::Charging
    } else if external_connected {
        ChargingState::OnAcNotCharging
    } else {
        ChargingState::Discharging
    };

    // Amperage (mA) x Voltage (mV) / 1e9 = kW; sign: negative while charging.
    let power_w = match (get_f64("Amperage"), get_f64("Voltage")) {
        (Some(ma), Some(mv)) => {
            let w = ma * mv / 1.0e9;
            Some(if is_charging { -w } else { w })
        }
        _ => None,
    };

    let mut flags = Vec::new();
    if dict
        .get("PermanentFailureStatus")
        .and_then(plist::Value::as_signed_integer)
        .map(|v| v != 0)
        .unwrap_or(false)
    {
        flags.push(ConditionFlag::Failure);
    }
    if get_u32("CycleCount").unwrap_or(0) > 0 && full_charge.is_none() {
        // Heuristic placeholder replaced by OEM service heuristics later.
    }
    if dict
        .get("ServiceRecommended")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false)
    {
        flags.push(ConditionFlag::ServiceRecommended);
    }

    Ok(BatterySample {
        collected_at: std::time::SystemTime::now(),
        source: SourceKind::MacIoreg,
        state_of_charge: dict
            .get("CurrentCapacity")
            .and_then(plist::Value::as_signed_integer)
            .map(|v| v as f64)
            .or_else(|| get_f64("MaxCapacity").zip(full_charge).map(|(max, fcc)| 100.0 * max / fcc)),
        design_capacity: get_f64("DesignCapacity"),
        full_charge_capacity: full_charge,
        cycle_limit: get_u32("CycleLimit"),
        cycle_count: get_u32("CycleCount"),
        temperature_c,
        voltage_mv: get_f64("Voltage"),
        power_w,
        charging_state: Some(charging_state),
        high_soc_dwell_s: None,
        design_voltage_mv: None,
        condition_flags: flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ioreg_fixture(pairs: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
  <dict>
{pairs}
  </dict>
</array>
</plist>"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_core_ioreg_fields() {
        let xml = ioreg_fixture(
            "    <key>CycleCount</key><integer>214</integer>\n\
             \x20   <key>DesignCapacity</key><integer>58030</integer>\n\
             \x20   <key>NominalChargeCapacity</key><integer>53120</integer>\n\
             \x20   <key>Temperature</key><integer>2912</integer>\n\
             \x20   <key>Voltage</key><integer>12531</integer>\n\
             \x20   <key>CurrentCapacity</key><integer>73</integer>\n\
             \x20   <key>ExternalConnected</key><false/>\n\
             \x20   <key>IsCharging</key><false/>",
        );
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert_eq!(s.cycle_count, Some(214));
        assert_eq!(s.design_capacity, Some(58030.0));
        assert_eq!(s.full_charge_capacity, Some(53120.0));
        assert!((s.temperature_c.unwrap() - 29.12).abs() < 1e-9);
        assert_eq!(s.voltage_mv, Some(12531.0));
        assert_eq!(s.state_of_charge, Some(73.0));
        assert_eq!(s.charging_state, Some(ChargingState::Discharging));
    }

    #[test]
    fn temperature_uses_hundredths_of_degree() {
        let xml = ioreg_fixture("    <key>Temperature</key><integer>3100</integer>");
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert!((s.temperature_c.unwrap() - 31.0).abs() < 1e-9);
    }

    #[test]
    fn charging_state_reads_external_power() {
        let xml = ioreg_fixture(
            "    <key>ExternalConnected</key><true/>\n    <key>IsCharging</key><true/>",
        );
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert_eq!(s.charging_state, Some(ChargingState::Charging));
    }
}
