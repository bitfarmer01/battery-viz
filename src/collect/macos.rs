//! macOS collector: reads AppleSmartBattery from the I/O Registry.
//!
//! `ioreg -r -n AppleSmartBattery -d 1 -a -x` emits an XML property list.
//! On Apple silicon the real layout (re-audit N7, observed on a MacBook
//! Air M4, macOS 26.6.2) carries Temperature (hundredths of °C), Voltage
//! (mV), CurrentCapacity and the capacity/cycle firmware data via
//! NominalChargeCapacity and DesignCycleCount9C, partly duplicated inside
//! a BatteryData child dict; there is no top-level FullChargeCapacity or
//! CycleLimit key. Those two legacy names stay as tolerant fallbacks for
//! other machines (Intel-era SMC layouts) — presence there is unverified,
//! not denied.

use std::io::Cursor;
use std::process::Command;

use crate::collect::Collector;
use crate::collect::types::{BatterySample, ChargingState, ConditionFlag, SourceKind};

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
            return Err(format!(
                "ioreg exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        parse_ioreg_plist(&out.stdout)
    }
}

/// Parse the XML plist emitted by ioreg into a sample.
pub fn parse_ioreg_plist(xml: &[u8]) -> Result<BatterySample, String> {
    let value: plist::Value = plist::Value::from_reader(Cursor::new(xml))
        .map_err(|e| format!("ioreg plist parse: {e}"))?;
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

    // Apple-silicon firmware data is partly nested in a BatteryData child
    // dict. Look top-level first, then inside BatteryData (re-audit N7).
    let battery_data = dict
        .get("BatteryData")
        .and_then(plist::Value::as_dictionary);
    let lookup = |key: &str| {
        dict.get(key)
            .or_else(|| battery_data.and_then(|d| d.get(key)))
    };

    // ioreg emits integers; `as_real` only reads Real values, so fall back.
    let get_f64 = |key: &str| {
        lookup(key).and_then(|v| {
            v.as_real()
                .or_else(|| v.as_signed_integer().map(|i| i as f64))
        })
    };
    let get_u32 = |key: &str| {
        lookup(key)
            .and_then(plist::Value::as_signed_integer)
            .map(|v| v as u32)
    };

    // Capacity: NominalChargeCapacity is the key Apple silicon emits;
    // FullChargeCapacity is a tolerant fallback for Intel-era SMC layouts
    // and is NOT assumed present on Apple silicon (re-audit N7).
    let full_charge = get_f64("NominalChargeCapacity").or_else(|| get_f64("FullChargeCapacity"));

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

    // Amperage (mA) x Voltage (mV) / 1e6 = W. ioreg signs the current
    // itself — negative while the pack discharges, positive while it
    // charges (real MacBook Air capture, published 2026-10-13) — which is
    // the OPPOSITE of our sample convention (positive = discharging), so
    // negate the product. Do not re-sign from `is_charging`: the same
    // capture shows IsCharging stuck Yes while a weak adapter lets the
    // pack drain.
    let power_w = match (get_f64("Amperage"), get_f64("Voltage")) {
        (Some(ma), Some(mv)) => Some(-(ma * mv / 1.0e6)),
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
            .or_else(|| {
                get_f64("MaxCapacity")
                    .zip(full_charge)
                    .map(|(max, fcc)| 100.0 * max / fcc)
            }),
        design_capacity: get_f64("DesignCapacity"),
        full_charge_capacity: full_charge,
        // DesignCycleCount9C is the real Apple-silicon key (re-audit N7 —
        // CycleLimit does not exist on a measured MacBook Air M4).
        // CycleLimit stays a tolerant fallback for other machines; its
        // real-world presence there is unverified.
        cycle_limit: get_u32("DesignCycleCount9C").or_else(|| get_u32("CycleLimit")),
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

    // Re-audit N7: parser fixtures. The XML files are SYNTHETIC — the
    // sandbox has no Mac and nothing here verifies real-hardware
    // behavior — but their key layout mirrors what real Apple-silicon
    // macOS emits as described in the re-audit (MacBook Air M4, macOS
    // 26.6.2): Temperature present, NominalChargeCapacity and
    // DesignCycleCount9C instead of FullChargeCapacity/CycleLimit, and
    // BatteryData-nested copies of the firmware values. Each file
    // carries its full labeling in a header comment.
    const FIXTURE_CHARGING: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ioreg_applesmartbattery_charging.xml"
    ));
    const FIXTURE_DRAINING: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ioreg_applesmartbattery_draining.xml"
    ));

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

    #[test]
    fn synthetic_charging_fixture_matches_real_layout() {
        // 738 mA flowing INTO the pack at 11845 mV while charging. Our
        // convention: negative while charging. The old /1e9 code reported
        // this as -0.0087 W (1000x off).
        let s = parse_ioreg_plist(FIXTURE_CHARGING.as_bytes()).expect("parse");
        assert_eq!(s.charging_state, Some(ChargingState::Charging));
        assert_eq!(s.voltage_mv, Some(11845.0));
        assert_eq!(s.state_of_charge, Some(55.0));
        assert_eq!(s.cycle_count, Some(628));
        assert_eq!(s.design_capacity, Some(4629.0));
        // Capacity comes from NominalChargeCapacity — there is no
        // top-level FullChargeCapacity in the real layout.
        assert_eq!(s.full_charge_capacity, Some(3976.0));
        // Cycle limit comes from DesignCycleCount9C, not CycleLimit.
        assert_eq!(s.cycle_limit, Some(1000));
        // Real macOS emits Temperature (hundredths of °C).
        assert!((s.temperature_c.unwrap() - 29.60).abs() < 1e-9);
        let power = s.power_w.expect("power");
        assert!((power - -(738.0 * 11845.0 / 1.0e6)).abs() < 1e-9);
        assert!(power < 0.0, "charging must be negative, got {power}");
    }

    #[test]
    fn synthetic_draining_fixture_locks_power_sign_quirk() {
        // Deliberate synthetic quirk: IsCharging stayed Yes while the
        // pack lost charge (-1415 mA) under a weak adapter. The power
        // sign must come from the Amperage data, not the IsCharging
        // flag: positive = discharging in our convention. The old code
        // negated based on IsCharging and would have reported -16.3 W
        // ("charging") here.
        let s = parse_ioreg_plist(FIXTURE_DRAINING.as_bytes()).expect("parse");
        let power = s.power_w.expect("power");
        assert!((power - 1415.0 * 11509.0 / 1.0e6).abs() < 1e-9);
        assert!(power > 0.0, "discharging must be positive, got {power}");
        // charging_state reflects the fixture's own (quirky) IsCharging flag.
        assert_eq!(s.charging_state, Some(ChargingState::Charging));
        assert!((s.temperature_c.unwrap() - 30.10).abs() < 1e-9);
        assert_eq!(s.full_charge_capacity, Some(3925.0));
        assert_eq!(s.cycle_limit, Some(1000));
    }

    #[test]
    fn full_charge_capacity_is_a_tolerant_fallback() {
        // Intel-era SMC layouts report FullChargeCapacity. Its presence
        // there is unverified, not denied, so it stays readable when
        // NominalChargeCapacity is absent.
        let xml = ioreg_fixture(
            "    <key>FullChargeCapacity</key><integer>3849</integer>\n\
             \x20   <key>CycleLimit</key><integer>1000</integer>",
        );
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert_eq!(s.full_charge_capacity, Some(3849.0));
        assert_eq!(s.cycle_limit, Some(1000));
    }

    #[test]
    fn battery_data_nested_values_are_read_when_top_level_is_absent() {
        // Some real packs duplicate the firmware values only inside the
        // BatteryData child dict; the reader must find them there.
        let xml = ioreg_fixture(
            "    <key>BatteryData</key>\n\
             \x20   <dict>\n\
             \x20     <key>NominalChargeCapacity</key><integer>3976</integer>\n\
             \x20     <key>DesignCycleCount9C</key><integer>1000</integer>\n\
             \x20   </dict>",
        );
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert_eq!(s.full_charge_capacity, Some(3976.0));
        assert_eq!(s.cycle_limit, Some(1000));
    }

    #[test]
    fn top_level_keys_win_over_battery_data_copies() {
        // When both levels carry a key, the top-level reading is used.
        let xml = ioreg_fixture(
            "    <key>NominalChargeCapacity</key><integer>3976</integer>\n\
             \x20   <key>BatteryData</key>\n\
             \x20   <dict>\n\
             \x20     <key>NominalChargeCapacity</key><integer>1</integer>\n\
             \x20   </dict>",
        );
        let s = parse_ioreg_plist(&xml).expect("parse");
        assert_eq!(s.full_charge_capacity, Some(3976.0));
    }
}
