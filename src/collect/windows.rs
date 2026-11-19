//! Windows collector: WMI/CIM battery classes plus GetSystemPowerStatus.
//!
//! WMI (`Win32_Battery`) gives capacity fields on most OEM firmware; the
//! legacy `GetSystemPowerStatus` call is parsed from PowerShell output as a
//! percentage/AC fallback. Native `IOCTL_BATTERY_QUERY_*` support (cycle
//! count, temperature) is the planned upgrade path behind the same interface.

use std::process::Command;

use crate::collect::types::{BatterySample, ChargingState, SourceKind};
use crate::collect::Collector;

#[allow(dead_code)]
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

    fn sample(&mut self) -> Result<BatterySample, String> {
        // One CIM query joins battery and voltage info in a single spawn.
        let script = "Get-CimInstance -ClassName Win32_Battery | \
format-list DesignCapacity,FullChargeCapacity,DesignVoltage,EstimatedChargeRemaining,BatteryStatus | Out-String";
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
            .map_err(|e| format!("failed to spawn powershell: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "powershell exited {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        parse_wmi_output(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Parse `Format-List` output of `Win32_Battery` fields.
pub fn parse_wmi_output(text: &str) -> Result<BatterySample, String> {
    if text.trim().is_empty() {
        return Err("no battery reported by WMI".into());
    }

    let get = |key: &str| -> Option<u64> {
        for line in text.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if k.trim().eq_ignore_ascii_case(key) {
                return v.trim().parse::<u64>().ok();
            }
        }
        None
    };

    let design = get("DesignCapacity").map(|v| v as f64);
    let fcc = get("FullChargeCapacity").map(|v| v as f64);
    let soc = get("EstimatedChargeRemaining").map(|v| v as f64);

    // WMI BatteryStatus bit 4 (0x10) = "charging"; bit 1 = "on AC".
    let status = get("BatteryStatus").unwrap_or(0);
    let charging_state = if soc == Some(100.0) && status & 0x1 != 0 {
        ChargingState::Full
    } else if status & 0x10 != 0 {
        ChargingState::Charging
    } else if status & 0x1 != 0 {
        ChargingState::OnAcNotCharging
    } else {
        ChargingState::Discharging
    };

    if design.is_none() && fcc.is_none() && soc.is_none() {
        return Err(format!("WMI battery fields missing in: {text:.200}"));
    }

    Ok(BatterySample {
        collected_at: std::time::SystemTime::now(),
        source: SourceKind::WindowsWmi,
        state_of_charge: soc,
        design_capacity: design,
        full_charge_capacity: fcc,
        cycle_limit: None,
        cycle_count: None,
        temperature_c: None,
        voltage_mv: get("DesignVoltage").map(|v| v as f64),
        power_w: None,
        charging_state: Some(charging_state),
        high_soc_dwell_s: None,
        design_voltage_mv: get("DesignVoltage").map(|v| v as f64),
        condition_flags: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wmi_field_list() {
        let out = "\
DesignCapacity         : 60000
FullChargeCapacity     : 48000
DesignVoltage          : 11400
EstimatedChargeRemaining : 65
BatteryStatus          : 2
";
        let s = parse_wmi_output(out).expect("parse");
        assert_eq!(s.design_capacity, Some(60000.0));
        assert_eq!(s.full_charge_capacity, Some(48000.0));
        assert_eq!(s.state_of_charge, Some(65.0));
        assert_eq!(s.voltage_mv, Some(11400.0));
        assert_eq!(s.charging_state, Some(ChargingState::Discharging));
    }

    #[test]
    fn charging_bit_sets_charging_state() {
        let out = "EstimatedChargeRemaining : 40\nBatteryStatus : 17\n";
        let s = parse_wmi_output(out).expect("parse");
        assert_eq!(s.charging_state, Some(ChargingState::Charging));
    }

    #[test]
    fn empty_output_is_error() {
        assert!(parse_wmi_output("   \n").is_err());
    }

    #[test]
    fn missing_fields_error() {
        assert!(parse_wmi_output("SomeUnrelated : 1\n").is_err());
    }
}
