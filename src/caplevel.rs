//! FR-2 (longevity roadmap): cap-level recommendation.
//!
//! From the recorded daily history (FR-7's store), find the lowest charge
//! reached on a normal day and recommend the lowest cap level that still
//! covers it. Which levels exist on a machine is never guessed here: the
//! FR-0 capability adapter reports the cap options (macOS 80–100% in 5%
//! steps; Windows: whatever the maker module offers). When the adapter
//! reports no cap control, or no offered level covers the observed usage,
//! the feature degrades to advice or hides itself — nothing is invented.
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

/// A cap-level recommendation: the level to set, the lowest charge the
/// recorded history actually reached (before the [`FLOOR_PCT`] floor), and
/// how many qualified days backed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapRecommendation {
    /// The lowest offered cap level that still covers the usage, percent.
    pub level_pct: u8,
    /// Lowest charge reached on any qualified normal day, percent.
    pub lowest_pct: f64,
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
        /// Lowest charge reached on any qualified normal day, percent.
        lowest_pct: f64,
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
    // The lowest charge reached, over qualified normal days: the minimum of
    // the days' minima — the deepest dip the user actually took. Days with
    // no state-of-charge readings (or too few) never contribute.
    let mut days = 0u32;
    let mut lowest: Option<f64> = None;
    for row in &record.rows {
        if row.soc_n < MIN_DAY_SAMPLES {
            continue;
        }
        let Some(min) = row.soc_min else {
            continue;
        };
        days += 1;
        lowest = Some(lowest.map_or(min, |l: f64| l.min(min)));
    }
    if days < MIN_HISTORY_DAYS {
        return CapAssessment::NeedHistory { days };
    }
    let lowest = lowest.unwrap_or(FLOOR_PCT);
    // The floor governs when the recorded usage ran deeper than it: the
    // recommendation never endorses a cap below 20%, on any history.
    let target = lowest.max(FLOOR_PCT);
    match levels.iter().copied().find(|&level| level as f64 >= target) {
        Some(level_pct) => CapAssessment::Recommend(CapRecommendation {
            level_pct,
            lowest_pct: lowest,
            days,
        }),
        None => CapAssessment::NoFit {
            lowest_pct: lowest,
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

    /// A record with `days` normal days, each having dipped to `dip` percent.
    fn record_with_dips(days: u32, dip: f64) -> DailyRecord {
        let mut rec = DailyRecord::default();
        for day in 0..days {
            fold_day(&mut rec, day, dip);
        }
        rec
    }

    /// Fold one normal day (≥ `MIN_DAY_SAMPLES` SoC samples) whose minimum
    /// is `dip`, peaking at 100.
    fn fold_day(rec: &mut DailyRecord, day: u32, dip: f64) {
        let mut s = crate::collect::BatterySample {
            state_of_charge: Some(dip),
            ..Default::default()
        };
        rec.fold(&s, day);
        s.state_of_charge = Some(100.0);
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
            assess(&record_with_dips(10, 40.0), &CapOptions::none()),
            CapAssessment::Unsupported
        );
    }

    #[test]
    fn fewer_than_seven_qualified_days_never_recommends() {
        assert_eq!(
            assess(&record_with_dips(6, 40.0), &MAC),
            CapAssessment::NeedHistory { days: 6 }
        );
        // Thin days do not count toward the gate even in quantity.
        assert_eq!(
            assess(&record_with_thin_days(20), &MAC),
            CapAssessment::NeedHistory { days: 0 }
        );
    }

    #[test]
    fn recommends_the_lowest_offered_level_that_covers_the_deepest_dip() {
        // Deepest dip 62% → target 62 → macOS grid (80–100 step 5): the
        // lowest covering level offered is 80.
        let mut rec = record_with_dips(7, 62.0);
        fold_day(&mut rec, 8, 64.0); // an 8th day, shallower — must not matter
        assert_eq!(
            assess(&rec, &MAC),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 80,
                lowest_pct: 62.0,
                days: 8
            })
        );
        // ASUS fixed presets: dip 58 → 60.
        let asus = CapOptions {
            fixed: &[60, 80],
            range: None,
            adaptive: false,
        };
        assert_eq!(
            assess(&record_with_dips(7, 58.0), &asus),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 60,
                lowest_pct: 58.0,
                days: 7
            })
        );
    }

    #[test]
    fn the_recommendation_never_sits_below_twenty_percent() {
        // A recorded day ran to 8%: the floor governs, the recommendation
        // stays at 20 where the machine offers it, never lower.
        let sub_twenty = CapOptions {
            fixed: &[],
            range: Some(CapRange {
                min: 15,
                max: 100,
                step: 5,
            }),
            adaptive: false,
        };
        assert_eq!(
            assess(&record_with_dips(7, 8.0), &sub_twenty),
            CapAssessment::Recommend(CapRecommendation {
                level_pct: 20,
                lowest_pct: 8.0,
                days: 7
            })
        );
    }

    #[test]
    fn no_offered_level_that_covers_degrades_to_no_fit_not_a_guess() {
        // Usage never dipped below 95% — an 80-max machine has no cap that
        // covers it; the feature says so instead of recommending a level
        // that under-covers the user.
        assert_eq!(
            assess(
                &record_with_dips(7, 95.0),
                &CapOptions {
                    fixed: &[60, 80],
                    range: None,
                    adaptive: false,
                }
            ),
            CapAssessment::NoFit {
                lowest_pct: 95.0,
                days: 7
            }
        );
    }

    #[test]
    fn days_without_state_of_charge_never_contribute() {
        let mut rec = record_with_dips(7, 50.0);
        // An eighth day of pure no-SoC samples: counted neither as history
        // nor as a dip.
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
                lowest_pct: 50.0,
                days: 7
            })
        );
    }
}
