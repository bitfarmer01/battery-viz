//! Deterministic simulator collector: produces a plausible, slowly drifting
//! sample stream so the UI, benchmark, and tests run on any dev platform.

use crate::collect::types::{BatterySample, ChargingState, SourceKind};
use crate::collect::Collector;

pub struct SimCollector {
    tick: u64,
}

impl SimCollector {
    pub fn new() -> Self {
        SimCollector { tick: 0 }
    }
}

impl Collector for SimCollector {
    fn source(&self) -> SourceKind {
        SourceKind::Simulator
    }

    fn sample(&mut self) -> Result<BatterySample, String> {
        self.tick += 1;
        let t = self.tick as f64;
        // Gentle sinusoidal drift around healthy values.
        let soc = 55.0 + 25.0 * (t / 240.0).sin(); // 30..80 %
        let temp = 27.0 + 6.0 * (t / 90.0).sin(); // 21..33 °C
        let watts = 18.0 + 10.0 * (t / 30.0).sin(); // discharge
        Ok(BatterySample {
            collected_at: std::time::SystemTime::now(),
            source: SourceKind::Simulator,
            state_of_charge: Some(soc),
            design_capacity: Some(58_030.0),
            full_charge_capacity: Some(53_300.0), // ~92% SoH
            full_charge_capacity_key: Some("simulated"),
            cycle_limit: Some(1000),
            cycle_count: Some(187),
            temperature_c: Some(temp),
            voltage_mv: Some(11_550.0),
            power_w: Some(watts),
            charging_state: Some(ChargingState::Discharging),
            high_soc_dwell_s: None,
            design_voltage_mv: Some(11_550.0),
            condition_flags: Vec::new(),
        })
    }
}
