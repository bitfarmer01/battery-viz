//! Simulator capability adapter: models a typical modern laptop (the
//! reference MacBook Air from the research) so FR-1+ features are testable
//! on any host without touching platform APIs.

use super::{
    CapOptions, CapRange, Capabilities, Detectability, Maker, MetricId, Platform, SettingGuidance,
    SettingKind,
};

const SIM_METRICS: &[MetricId] = &[
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

pub struct SimCapabilities;

impl SimCapabilities {
    pub fn new() -> Self {
        SimCapabilities
    }
}

impl Capabilities for SimCapabilities {
    fn platform(&self) -> Platform {
        Platform::Simulator
    }

    fn maker(&self) -> Maker {
        // Models the reference MacBook Air from the longevity research.
        Maker::Apple
    }

    fn readable_metrics(&self) -> &'static [MetricId] {
        SIM_METRICS
    }

    fn setting_detectability(&self, kind: SettingKind) -> Detectability {
        // The simulator behaves like an uncapped macOS machine: state is
        // inferred from its sample stream, not read from an OS.
        match kind {
            SettingKind::ChargeCap | SettingKind::AdaptiveCharging => Detectability::BehavioralOnly,
        }
    }

    fn cap_options(&self) -> CapOptions {
        CapOptions {
            fixed: &[],
            range: Some(CapRange {
                min: 80,
                max: 100,
                step: 5,
            }),
            adaptive: true,
        }
    }

    fn guidance(&self, _kind: SettingKind) -> Option<SettingGuidance> {
        Some(SettingGuidance {
            tool: "Simulator",
            steps: "Nothing to change — this machine is simulated",
            url: None,
        })
    }
}
