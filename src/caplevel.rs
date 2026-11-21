//! FR-2 (longevity roadmap): cap-level recommendation.
//!
//! From the recorded daily history (FR-7's store), measure how deep the
//! user actually drains while unplugged — the deepest day's unplugged
//! discharge range — and recommend the lowest cap level that leaves a 20%
//! reserve above that usage. Which levels exist on a machine is never
//! guessed here: the FR-0 capability adapter reports the cap options
//! (macOS 80–100% in 5% steps; Windows: whatever the maker module offers).
//! When the adapter reports no cap control, or no offered level covers the
//! observed usage, the feature degrades to advice or hides itself —
//! nothing is invented.
//!
//! (Re-audit N1: the recommendation is anchored to daily *usage*, not to
//! the lowest charge ever reached. A user draining 100% → 8% daily is not
//! covered by any cap — that usage exceeds every level and says so
//! (`NoFit`) — and a user who never unplugs has no usage to cover, so the
//! floor governs, not the parked charge.)
//!
//! Pure logic only: no I/O, no clock, no platform branches. The app calls
//! [`assess`] with the current record and the adapter's [`CapOptions`].

use crate::caps::CapOptions;
use crate::record::DailyRecord;

/// A day counts as "normal" — its minimum trustworthy for the statistic —
/// only when at least this many samples carried a state of charge that day
/// (1 Hz collector: one hour of observed charge behaviour). Below that, the
/// day's minimum is too likely to have missed the real dip; such days are
/// excluded. Exclusion only ever raises the target (the statistic is a
/// minimum over days), so it errs toward the safer, higher cap.
pub const MIN_DAY_SAMPLES: u32 = 3600;

/// Spec acceptance: at least this many days of history before recommending.
pub const MIN_HISTORY_DAYS: u32 = 7;

/// Spec acceptance: the recommendation never sits below this state of
/// charge, however light the recorded usage was.
pub const FLOOR_PCT: f64 = 20.0;

/// Re-audit N1: headroom above the deepest daily usage — the cap must
/// cover a user's actual drain *plus* this reserve, so a full day away
/// from the wall never ends below the cap.
pub const RESERVE_PCT: f64 = 20.0;

/// A cap-level recommendation: the level to set, the deepest daily
/// unplugged discharge the recorded history observed (before any reserve
/// or floor), and how many qualified days backed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapRecommendation {
    /// The lowest offered cap level that still covers the usage, percent.
    pub level_pct: u8,
    /// Deepest daily unplugged discharge observed, in percentage points
    /// (the day's unplugged high minus its unplugged low, deepest day).
    pub usage_pct: f64,
    /// Qualified days the statistic is built from (≥ [`MIN_HISTORY_DAYS`]).
    pub days: u32,
}

/// What FR-2 has to say on this machine right now. Every non-recommending
/// variant is a designed degradation, never a guess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapAssessment {
    /// The adapter reports no cap control at all — the feature hides itself.
    Unsupported,
    /// History is shorter than [`MIN_HISTORY_DAYS`] qualified days.
    NeedHistory {
        /// Qualified days recorded so far.
        days: u32,
    },
    /// The recorded usage is deeper than any cap level the machine offers;
    /// recommending one would under-cover it, so the app says so instead.
    NoFit {
        /// Deepest daily unplugged discharge observed, percentage points.
        usage_pct: f64,
        /// Qualified days the statistic is built from.
        days: u32,
    },
    /// The recommendation itself.
    Recommend(CapRecommendation),
}

/// Assess the machine's history against its offered cap levels.
pub fn assess(record: &DailyRecord, options: &CapOptions) -> CapAssessment {
    let levels = offered_levels(options);
    if levels.is_empty() {
        return CapAssessment::Unsupported;
    }
    // Qualified normal days gate the history, exactly as before: a day with
    // too few SoC readings never counts, and exclusion of thin days only
    // lowers the day count (more `NeedHistory`), never the target.
    //
    // The usage statistic (re-audit N1): the deepest single day's unplugged
    // discharge range — how far below its unplugged peak the user actually
    // took the pack. A machine that never unplugs has no range on any day
    // and reads as zero usage; a parked level is not consumption.
    let mut days = 0u32;
    let mut deepest: Option<f64> = None;
    for row in &record.rows {
        if row.soc_n < MIN_DAY_SAMPLES {
            continue;
        }
        days += 1;
        if let (Some(max), Some(min)) = (row.disch_max, row.disch_min) {
            let usage = max - min;
            deepest = Some(deepest.map_or(usage, |d: f64| d.max(usage)));
        }
    }
    if days < MIN_HISTORY_DAYS {
        return CapAssessment::NeedHistory { days };
    }
    let usage = deepest.unwrap_or(0.0);
    // The cap covers the deepest daily usage plus a 20% reserve — and the
    // floor still governs: the recommendation never endorses a cap below
    // 20%, on any history.
    let target = (usage + RESERVE_PCT).max(FLOOR_PCT);
    match levels.iter().copied().find(|&level| level as f64 >= target) {
        Some(level_pct) => CapAssessment::Recommend(CapRecommendation {
            level_pct,
            usage_pct: usage,
            days,
        }),
        None => CapAssessment::NoFit {
            usage_pct: usage,
            days,
        },
    }
}

/// Every cap level this machine can actually be set to, ascending, deduped:
/// the fixed presets plus the configurable range's grid (min to max in
/// `step` increments — and the max itself, an endpoint the tool exposes
/// even when the step does not land on it).
fn offered_levels(options: &CapOptions) -> Vec<u8> {
    let mut levels = options.fixed.to_vec();
    if let Some(range) = options.range {
        let mut v = range.min;
        loop {
            levels.push(v);
            if v >= range.max || range.step == 0 {
                break;
            }
            v = v.saturating_add(range.step).min(range.max);
        }
    }
    levels.sort_unstable();
    levels.dedup();
    levels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{CapOptions, CapRange};

    /// A sample observed while unplugged (discharging).
    fn unplugged(soc: f64) -> crate::collect::BatterySample {
        crate::collect::BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(crate::collect::types::ChargingState::Discharging),
            ..Default::default()
        }
    }

    /// A record with `days` normal days, each draining unplugged from `top`
    /// down to `bottom` — the day's usage is `top − bottom` — then topping
    /// back up on AC long enough to qualify.
    fn record_with_usage(days: u32, top: f64, bottom: f64) -> DailyRecord {
        let mut rec = DailyRecord::default();
        for day in 0..days {
            fold_usage_day(&mut rec, day, top, bottom);
        }
        rec
    }

    /// Fold one normal day: an unplugged session draining `top` → `bottom`
    /// (≥ `MIN_DAY_SAMPLES` SoC samples, so the last sample reaches
    /// `bottom` exactly), then a plugged-in stretch at 100.
    fn fold_usage_day(rec: &mut DailyRecord, day: u32, top: f64, bottom: f64) {
        for i in 0..MIN_DAY_SAMPLES {
            let soc = top - (top - bottom) * i as f64 / (MIN_DAY_SAMPLES - 1) as f64;
            rec.fold(&unplugged(soc), day);
        }
        let s = crate::collect::BatterySample {
            state_of_charge: Some(100.0),
            ..Default::default()
        };
        for _ in 0..MIN_DAY_SAMPLES {
            rec.fold(&s, day);
        }
    }

    /// `days` days that observe some charge but fewer than
    /// `MIN_DAY_SAMPLES` samples — not "normal" days by definition.
    fn record_with_thin_days(days: u32) -> DailyRecord {
        let mut rec = DailyRecord::default();
        for day in 0..days {
            let mut s = crate::collect::BatterySample {
                state_of_charge: Some(10.0),
                ..Default::default()
            };
            rec.fold(&s, day);
            s.state_of_charge = Some(99.0);
            for _ in 0..(MIN_DAY_SAMPLES - 2) {
                rec.fold(&s, day);
            }
        }
        rec
    }

    const MAC: CapOptions = CapOptions {
        fixed: &[],
        range: Some(CapRange {
            min: 80,
            max: 100,
            step: 5,
        }),
        adaptive: true,
    };

    #[test]
    fn offered_levels_cover_fixed_and_range_grids() {
        assert_eq!(
            offered_levels(&CapOptions {
                fixed: &[60, 80],
                range: None,
                adaptive: false,
            }),
            vec![60, 80]
        );
        // Step does not land on the max: the endpoint is still offered.
        assert_eq!(
            offered_levels(&CapOptions {
                fixed: &[],
                range: Some(CapRange {
                    min: 50,
                    max: 100,
                    step: 30
                }),
                adaptive: false,
            }),
            vec![50, 80, 100]
        );
        assert_eq!(
            offered_levels(&CapOptions {
                fixed: &[],
                range: Some(CapRange {
                    min: 80,
                    max: 100,
                    step: 5
                }),
                adaptive: true,
            }),
            vec![80, 85, 90, 95, 100]
        );
        // Degenerate adapter data (zero step) cannot spin forever.
        assert_eq!(
            offered_levels(&CapOptions {
                fixed: &[],
                range: Some(CapRange {
                    min: 80,
                    max: 100,
                    step: 0
                }),
                adaptive: false,
            }),
            vec![80]
        );
    }

    #[test]
    fn no_cap_options_hides_the_feature() {
        assert_eq!(
            assess(&record_with_usage(10, 80.0, 40.0), &CapOptions::none()),
            CapAssessment::Unsupported
        );
    }

    #[test]
    fn fewer_than_seven_qualified_days_never_recommends() {
        assert_eq!(
            assess(&record_with_usage(6, 80.0, 40.0), &MAC),
            CapAssessment::NeedHistory { days: 6 }
        );
        // Thin days do not count toward the gate even in quantity.
        assert_eq!(
            assess(&record_with_thin_days(20), &MAC),
            CapAssessment::NeedHistory { days: 0 }
        );
    }

    #[test]
    fn recommends_the_lowest_offered_level_covering_usage_plus_reserve() {
        // Deepest daily drain 80 → 60 (usage 20) → target 20 + 20 = 40 →
        // macOS grid (80–100 step 5): the lowest covering level is 80.
        let mut rec = record_with_usage(7, 80.0, 60.0);
        fold_usage_day(&mut rec, 8, 75.0, 60.0); // an 8th day, shallower — must not matter
        assert_eq!(
            assess(&rec, &MAC),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 80,
                usage_pct: 20.0,
                days: 8
            })
        );
        // ASUS fixed presets: drain 78 → 40 (usage 38) → target 58 → 60.
        let asus = CapOptions {
            fixed: &[60, 80],
            range: None,
            adaptive: false,
        };
        assert_eq!(
            assess(&record_with_usage(7, 78.0, 40.0), &asus),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 60,
                usage_pct: 38.0,
                days: 7
            })
        );
    }

    /// Re-audit N1, the headline case: a user draining 100% → 8% daily is
    /// NOT covered by any cap — usage 92 + 20 reserve = 112 exceeds every
    /// level, so the feature says `NoFit` instead of endorsing a cap that
    /// under-covers the day.
    #[test]
    fn deep_daily_usage_no_level_covers_degrades_to_no_fit() {
        assert_eq!(
            assess(&record_with_usage(7, 100.0, 8.0), &MAC),
            CapAssessment::NoFit {
                usage_pct: 92.0,
                days: 7
            }
        );
    }

    /// The reserve leaves headroom over the drain: usage 40 + 20 = 60, so
    /// the 50% preset — which the old dip-anchored rule would have picked —
    /// must not be chosen.
    #[test]
    fn reserve_leaves_headroom_above_the_deepest_drain() {
        let machine = CapOptions {
            fixed: &[50, 60, 80],
            range: None,
            adaptive: false,
        };
        assert_eq!(
            assess(&record_with_usage(7, 70.0, 30.0), &machine),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 60,
                usage_pct: 40.0,
                days: 7
            })
        );
    }

    #[test]
    fn the_recommendation_never_sits_below_twenty_percent() {
        // A machine that never unplugs has zero recorded usage: the floor
        // governs, the recommendation sits at 20 where the machine offers
        // it, never lower — never at the parked charge (the old rule told
        // such a user 100%).
        let sub_twenty = CapOptions {
            fixed: &[],
            range: Some(CapRange {
                min: 15,
                max: 100,
                step: 5,
            }),
            adaptive: false,
        };
        let mut rec = DailyRecord::default();
        for day in 0..7u32 {
            let s = crate::collect::BatterySample {
                state_of_charge: Some(100.0),
                ..Default::default()
            };
            for _ in 0..MIN_DAY_SAMPLES {
                rec.fold(&s, day);
            }
        }
        assert_eq!(
            assess(&rec, &sub_twenty),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 20,
                usage_pct: 0.0,
                days: 7
            })
        );
    }

    #[test]
    fn days_without_state_of_charge_never_contribute() {
        let mut rec = record_with_usage(7, 80.0, 50.0);
        // An eighth day of pure no-SoC samples: counted neither as history
        // nor as usage.
        let mut blank = crate::collect::BatterySample::default();
        for _ in 0..(MIN_DAY_SAMPLES + 1) {
            rec.fold(&blank, 8);
        }
        blank.state_of_charge = Some(5.0);
        rec.fold(&blank, 8);
        assert_eq!(
            assess(&rec, &MAC),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 80,
                usage_pct: 30.0,
                days: 7
            })
        );
    }
}
