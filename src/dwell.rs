//! High-charge dwell accumulation (audit H4).
//!
//! The benchmark's high-charge-dwell factor needs the time the pack spends
//! parked at or above [`DWELL_SOC_PCT`] while plugged in — a quantity no
//! collector reports. The app layer derives it from the incoming sample
//! stream here: each sample credits the elapsed time since the previous one
//! when the pack qualifies, and the running total is persisted by the history
//! store so it survives restarts.
//!
//! Time injection mirrors the alert engine's clock seam: callers pass a
//! monotonic `now_s`, keeping the math pure and deterministic to test.

use crate::collect::types::{BatterySample, ChargingState};

/// Dwell counts at or above this state of charge, percent.
pub const DWELL_SOC_PCT: f64 = 80.0;

/// A single interval credits at most this many seconds. The 1 Hz stream
/// normally ticks 1 s apart, so this only binds on a stall (machine asleep,
/// collector wedged): stale qualification must not backfill hours of dwell
/// after the gap, but a bounded grace period keeps short hiccups honest.
const MAX_INTERVAL_S: f64 = 60.0;

/// True when the pack is drawing wall power (charging, full, or held on AC).
pub fn plugged_in(state: Option<ChargingState>) -> bool {
    matches!(
        state,
        Some(ChargingState::Charging | ChargingState::Full | ChargingState::OnAcNotCharging)
    )
}

/// Seconds of qualifying dwell between the previous sample (at monotonic
/// `prev_s`) and this one. Zero unless both samples qualify the pack and the
/// clock moved forward; first-ever samples credit nothing.
pub fn dwell_credit(prev_s: Option<f64>, now_s: f64, s: &BatterySample) -> f64 {
    let Some(prev) = prev_s else {
        return 0.0;
    };
    if now_s <= prev {
        return 0.0;
    }
    let at_or_above = s.state_of_charge.is_some_and(|soc| soc >= DWELL_SOC_PCT);
    if !(at_or_above && plugged_in(s.charging_state)) {
        return 0.0;
    }
    (now_s - prev).min(MAX_INTERVAL_S)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(soc: Option<f64>, state: Option<ChargingState>) -> BatterySample {
        BatterySample {
            state_of_charge: soc,
            charging_state: state,
            ..Default::default()
        }
    }

    /// A simulated 1 Hz sample stream: 85% and charging for a stretch, with
    /// interruptions (unplugged, below threshold, first sample) that must not
    /// credit anything.
    #[test]
    fn simulated_stream_credits_only_qualifying_intervals() {
        // t=0: the first sample qualifies but has no predecessor.
        let first = sample(Some(85.0), Some(ChargingState::Charging));
        let mut prev = None;
        let mut total = dwell_credit(prev, 0.0, &first);
        prev = Some(0.0);

        // 10 qualifying seconds at 85% while charging.
        for t in 1..=10 {
            let s = sample(Some(85.0), Some(ChargingState::Charging));
            total += dwell_credit(prev, t as f64, &s);
            prev = Some(t as f64);
        }
        assert_eq!(total, 10.0);

        // Unplugged at the same SoC: no credit.
        let s = sample(Some(85.0), Some(ChargingState::Discharging));
        total += dwell_credit(prev, 11.0, &s);
        prev = Some(11.0);
        assert_eq!(total, 10.0);

        // On AC but below the threshold: no credit.
        let s = sample(Some(60.0), Some(ChargingState::Charging));
        total += dwell_credit(prev, 12.0, &s);
        prev = Some(12.0);
        assert_eq!(total, 10.0);

        // Qualifying again resumes the accumulation exactly where it was.
        let s = sample(Some(90.0), Some(ChargingState::OnAcNotCharging));
        total += dwell_credit(prev, 13.0, &s);
        assert_eq!(total, 11.0);
    }

    #[test]
    fn full_and_on_ac_count_as_plugged_discharging_does_not() {
        assert!(plugged_in(Some(ChargingState::Charging)));
        assert!(plugged_in(Some(ChargingState::Full)));
        assert!(plugged_in(Some(ChargingState::OnAcNotCharging)));
        assert!(!plugged_in(Some(ChargingState::Discharging)));
        // Unknown state is not proof of wall power — never guessed.
        assert!(!plugged_in(Some(ChargingState::Unknown)));
        assert!(!plugged_in(None));
    }

    #[test]
    fn stalled_stream_credits_a_bounded_interval_not_the_gap() {
        let s = sample(Some(100.0), Some(ChargingState::Full));
        // A laptop asleep for two hours at 100% on AC must not backfill two
        // hours of dwell from stale qualification.
        assert_eq!(dwell_credit(Some(0.0), 7200.0, &s), MAX_INTERVAL_S);
        // Backwards clock steps never credit.
        assert_eq!(dwell_credit(Some(10.0), 5.0, &s), 0.0);
    }

    #[test]
    fn missing_soc_or_state_credits_nothing() {
        let no_soc = sample(None, Some(ChargingState::Charging));
        assert_eq!(dwell_credit(Some(0.0), 1.0, &no_soc), 0.0);
        let no_state = sample(Some(95.0), None);
        assert_eq!(dwell_credit(Some(0.0), 1.0, &no_state), 0.0);
    }
}
