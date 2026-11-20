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
    CapOptions, Capabilities, Detectability, Maker, MetricId, Platform, SettingGuidance,
    SettingKind,
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
}
