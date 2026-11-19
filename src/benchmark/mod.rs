//! Weighted battery-optimization benchmark.
//!
//! Scores the researched health parameters (0..=100 each), combines them with
//! fixed weights, and reports per-parameter detail so the UI can show *why*
//! the score is what it is. Missing data never silently helps: unavailable
//! parameters are excluded and the weights renormalized, with the coverage
//! reported alongside the score.

use crate::collect::types::{state_of_health_pct, BatterySample, ChargingState};

pub mod weights {
    pub const SOH: f64 = 0.30;
    pub const THERMAL: f64 = 0.20;
    pub const SOC_BAND: f64 = 0.15;
    pub const HIGH_DWELL: f64 = 0.15;
    pub const CYCLES: f64 = 0.10;
    pub const CHARGE_RATE: f64 = 0.10;
}

/// Scored result for one parameter.
#[derive(Debug, Clone)]
pub struct ParameterScore {
    pub name: &'static str,
    pub score: f64,
    pub weight: f64,
    /// Short human explanation of the score.
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub total: f64,
    /// Fraction of weight actually scored (missing parameters are excluded).
    pub coverage: f64,
    pub parameters: Vec<ParameterScore>,
}

/// Grade bands for the UI.
pub fn grade(total: f64) -> &'static str {
    match total {
        t if t >= 90.0 => "Excellent",
        t if t >= 75.0 => "Healthy",
        t if t >= 55.0 => "Aging",
        _ => "Stressed",
    }
}

fn clamp100(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

/// SoH: ≥90% excellent (100), linear to 0 at 50%.
fn score_soh(s: &BatterySample) -> Option<ParameterScore> {
    let soh = state_of_health_pct(s)?;
    let score = clamp100((soh - 50.0) / 40.0 * 100.0);
    Some(ParameterScore {
        name: "State of health",
        score,
        weight: weights::SOH,
        detail: format!("Full-charge capacity is {soh:.1}% of design"),
    })
}

/// Temperature: 20–30°C ideal band (100), linear penalties outside, hard
/// penalty above 45°C.
fn score_thermal(s: &BatterySample) -> Option<ParameterScore> {
    let t = s.temperature_c?;
    let score = if (20.0..=30.0).contains(&t) {
        100.0
    } else if t < 20.0 {
        clamp100(100.0 - (20.0 - t) * 5.0)
    } else if t <= 45.0 {
        clamp100(100.0 - (t - 30.0) * 4.0)
    } else {
        clamp100(40.0 - (t - 45.0) * 8.0)
    };
    Some(ParameterScore {
        name: "Temperature",
        score,
        weight: weights::THERMAL,
        detail: format!("Pack at {t:.1}°C (ideal 20–30°C)"),
    })
}

/// SoC band: in the 20–80% longevity band (100), penalty for extremes.
fn score_soc_band(s: &BatterySample) -> Option<ParameterScore> {
    let soc = s.state_of_charge?;
    let score = if (20.0..=80.0).contains(&soc) {
        100.0
    } else if soc > 80.0 {
        clamp100(100.0 - (soc - 80.0) * 3.0)
    } else if soc < 20.0 {
        clamp100(100.0 - (20.0 - soc) * 4.0)
    } else {
        clamp100(100.0 - (20.0 - soc) * 4.0)
    };
    Some(ParameterScore {
        name: "Charge band",
        score,
        weight: weights::SOC_BAND,
        detail: format!("State of charge {soc:.0}% (longevity band 20–80%)"),
    })
}

/// High-SoC dwell: session time at ≥95% charge, decayed; unknown when not tracked.
fn score_high_dwell(s: &BatterySample) -> Option<ParameterScore> {
    let dwell_s = s.high_soc_dwell_s?;
    // 0 s → 100; every 30 min at ≥95% costs ~8 points, floor at 20.
    let score = clamp100(100.0 - (dwell_s / 1800.0) * 8.0).max(20.0);
    let detail = if dwell_s < 60.0 {
        "Not parked at high charge".to_string()
    } else {
        format!("{:.0} min at ≥95% charge this session", dwell_s / 60.0)
    };
    Some(ParameterScore {
        name: "High-charge dwell",
        score,
        weight: weights::HIGH_DWELL,
        detail,
    })
}

/// Cycles: against the OEM cycle limit when known, else a 1000-cycle default.
fn score_cycles(s: &BatterySample) -> Option<ParameterScore> {
    let cycles = s.cycle_count?;
    let limit = s.cycle_limit.unwrap_or(1000) as f64;
    let ratio = cycles as f64 / limit.max(1.0);
    let score = clamp100(100.0 * (1.0 - ratio));
    Some(ParameterScore {
        name: "Cycle count",
        score,
        weight: weights::CYCLES,
        detail: format!("{cycles} cycles (rated limit {limit:.0})"),
    })
}

/// Charge rate: power normalized against a 1C estimate from capacity and
/// design voltage; only meaningful while charging.
fn score_charge_rate(s: &BatterySample) -> Option<ParameterScore> {
    if s.charging_state != Some(ChargingState::Charging) {
        return None; // Not applicable while discharging; excluded from score.
    }
    let watts = -s.power_w?; // charging power is negative in our convention
    if watts <= 0.0 {
        return None;
    }
    let voltage_v = s.design_voltage_mv.or(s.voltage_mv)? / 1000.0;
    let capacity_ah = s.full_charge_capacity? / 1000.0 / voltage_v.max(1.0);
    if capacity_ah <= 0.0 {
        return None;
    }
    let c_rate = watts / (capacity_ah * voltage_v);
    // 0.5C → 100, 1C → 85, 2C → 0.
    let score = clamp100(100.0 - (c_rate - 0.5) * 60.0);
    Some(ParameterScore {
        name: "Charge rate",
        score,
        weight: weights::CHARGE_RATE,
        detail: format!("Charging at {watts:.1} W ≈ {c_rate:.2}C"),
    })
}

/// Score one sample. Excluded (missing or N/A) parameters renormalize weight.
pub fn evaluate(s: &BatterySample) -> BenchmarkResult {
    let scored: Vec<ParameterScore> = [
        score_soh(s),
        score_thermal(s),
        score_soc_band(s),
        score_high_dwell(s),
        score_cycles(s),
        score_charge_rate(s),
    ]
    .into_iter()
    .flatten()
    .collect();

    let total_weight: f64 = scored.iter().map(|p| p.weight).sum();
    let total = if total_weight > 0.0 {
        scored.iter().map(|p| p.score * p.weight).sum::<f64>() / total_weight
    } else {
        0.0
    };

    BenchmarkResult {
        total,
        coverage: total_weight,
        parameters: scored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_sample() -> BatterySample {
        BatterySample {
            state_of_charge: Some(50.0),
            design_capacity: Some(50_000.0),
            full_charge_capacity: Some(47_500.0), // 95% SoH
            cycle_limit: Some(1000),
            cycle_count: Some(100),
            temperature_c: Some(25.0),
            voltage_mv: Some(11_500.0),
            power_w: Some(15.0),
            charging_state: Some(ChargingState::Discharging),
            high_soc_dwell_s: Some(0.0),
            ..Default::default()
        }
    }

    #[test]
    fn healthy_sample_scores_high() {
        let r = evaluate(&full_sample());
        assert!(r.total > 90.0, "expected >90, got {}", r.total);
        // Discharging: charge-rate is N/A and excluded, so coverage is 0.9.
        assert!((r.coverage - 0.90).abs() < 1e-9);
    }

    #[test]
    fn hot_sample_penalizes_thermal() {
        let mut s = full_sample();
        s.temperature_c = Some(48.0);
        let hot = evaluate(&s);
        let base = evaluate(&full_sample());
        assert!(hot.total < base.total);
        let thermal = hot.parameters.iter().find(|p| p.name == "Temperature").unwrap();
        assert!(thermal.score < 40.0, "thermal score {}", thermal.score);
    }

    #[test]
    fn missing_parameters_renormalize_weight() {
        let mut s = full_sample();
        s.temperature_c = None; // no thermal data
        let r = evaluate(&s);
        // 0.9 for excluded charge-rate, minus 0.2 for missing thermal.
        assert!((r.coverage - 0.70).abs() < 1e-9);
        assert!(!r.parameters.iter().any(|p| p.name == "Temperature"));
        assert!(r.total > 0.0);
    }

    #[test]
    fn charge_rate_only_scores_while_charging() {
        let mut s = full_sample();
        s.charging_state = Some(ChargingState::Discharging);
        let r = evaluate(&s);
        assert!(!r.parameters.iter().any(|p| p.name == "Charge rate"));
        assert!((r.coverage - 0.90).abs() < 1e-9);

        s.charging_state = Some(ChargingState::Charging);
        s.power_w = Some(-30.0); // fast charge
        let r2 = evaluate(&s);
        let rate = r2.parameters.iter().find(|p| p.name == "Charge rate").unwrap();
        assert!(rate.score < 100.0);
    }

    #[test]
    fn deep_discharge_penalizes_band() {
        let mut s = full_sample();
        s.state_of_charge = Some(5.0);
        let r = evaluate(&s);
        let band = r.parameters.iter().find(|p| p.name == "Charge band").unwrap();
        assert!(band.score < 50.0);
    }

    #[test]
    fn grades_match_bands() {
        assert_eq!(grade(95.0), "Excellent");
        assert_eq!(grade(80.0), "Healthy");
        assert_eq!(grade(60.0), "Aging");
        assert_eq!(grade(30.0), "Stressed");
    }
}
