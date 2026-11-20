//! FR-4: heat as accumulated warm hours — weekly digest logic.
//!
//! A 45 °C spike rule never fires on a fanless laptop, yet the pack can
//! spend a third of its life in the warm range. This module reads the
//! stored 1-minute history (see [`crate::history`]) and counts the week's
//! warm time: every bucket whose mean temperature reached
//! [`WARM_THRESHOLD_C`] is one warm minute.
//!
//! Attribution is deliberately modest: a likely cause, from the same
//! buckets — charging while warm (the pack reported `Charging` for most of
//! the bucket's samples) versus time of day (warm minutes cluster in a few
//! local hours). When neither pattern dominates the answer is
//! [`WarmCause::Mixed`], never a forced guess.
//!
//! The output feeds the weekly digest channel only. It is never a real-time
//! alert; the sustained ≥45 °C alert in [`crate::alert`] stays the sole
//! real-time heat channel. Platform logic lives in the FR-0 capability
//! adapter: callers gate this feature on `MetricId::Temperature` being
//! readable, and the report itself is `None` when the stored history has no
//! temperature readings at all — nothing is invented where the machine
//! reports Unavailable.

use crate::history::{HistoryStore, MinuteBucket, RETENTION_MINUTES};

/// A bucket whose mean temperature reaches this is a warm minute (spec:
/// "hours above about 30 °C").
pub const WARM_THRESHOLD_C: f64 = 30.0;

/// Warm-minute share above which the cause reads as "charging while warm".
const CHARGING_CAUSE_SHARE: f64 = 0.6;

/// Share of the week's warm minutes in the single busiest local hour above
/// which the cause reads as "time of day".
const PEAK_CAUSE_SHARE: f64 = 0.4;

/// What most likely made the pack warm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmCause {
    /// Most warm minutes happened while the pack was charging.
    ChargingUnderLoad,
    /// Warm minutes cluster in a few local hours — ambient heat, sun load,
    /// a hot room.
    TimeOfDay,
    /// No single cause dominates.
    Mixed,
}

/// One week's warm-hour accounting, straight from the stored history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarmHoursReport {
    /// Buckets (≈ minutes) at or above the warm threshold.
    pub warm_minutes: u32,
    /// Of those, buckets that were mostly charging.
    pub charging_warm_minutes: u32,
    /// Local hour of day (0–23) holding the most warm minutes; the earliest
    /// on a tie. `None` when there were no warm minutes.
    pub peak_hour: Option<u8>,
    /// Warm minutes in the peak hour.
    pub peak_hour_warm_minutes: u32,
    /// Buckets in the window that carry any temperature reading — the
    /// denominator that makes the count reconcilable with the history.
    pub covered_minutes: u32,
}

impl WarmHoursReport {
    /// Warm time as hours (each retained bucket is one minute).
    pub fn warm_hours(&self) -> f64 {
        self.warm_minutes as f64 / 60.0
    }

    /// Hours of recorded history the count covers.
    pub fn covered_hours(&self) -> f64 {
        self.covered_minutes as f64 / 60.0
    }

    /// The likely cause. Meaningful only when `warm_minutes > 0`; an empty
    /// week has no cause to attribute.
    pub fn cause(&self) -> WarmCause {
        let warm = self.warm_minutes as f64;
        let charging_share = self.charging_warm_minutes as f64 / warm;
        let peak_share = self.peak_hour_warm_minutes as f64 / warm;
        if charging_share >= CHARGING_CAUSE_SHARE {
            WarmCause::ChargingUnderLoad
        } else if peak_share >= PEAK_CAUSE_SHARE {
            WarmCause::TimeOfDay
        } else {
            WarmCause::Mixed
        }
    }

    /// One digest sentence: the number, the likely cause, one action.
    pub fn digest_line(&self) -> String {
        if self.warm_minutes == 0 {
            return format!(
                "No time above {:.0} °C this week ({:.1} h of history). Keep it up: heat is the quietest battery ager.",
                WARM_THRESHOLD_C,
                self.covered_hours()
            );
        }
        let hours = format_hours(self.warm_hours());
        match self.cause() {
            WarmCause::ChargingUnderLoad => format!(
                "{hours} above {:.0} °C this week, mostly while charging — charge on a hard surface out of the sun, and unplug once full.",
                WARM_THRESHOLD_C
            ),
            WarmCause::TimeOfDay => {
                let hour = self.peak_hour.unwrap_or(0);
                format!(
                    "{hours} above {:.0} °C this week, clustering around {}:00–{}:00 local — likely ambient heat; keep the machine shaded and vented then.",
                    WARM_THRESHOLD_C,
                    hour,
                    (hour + 2) % 24
                )
            }
            WarmCause::Mixed => format!(
                "{hours} above {:.0} °C this week, with no single cause — keep the vents clear and avoid charging in the sun.",
                WARM_THRESHOLD_C
            ),
        }
    }
}

/// Hours rendered as whole hours above one ("3 h") or one decimal below
/// ("0.5 h") — a digest line, not a gauge.
fn format_hours(h: f64) -> String {
    if h >= 10.0 {
        format!("{h:.0} h")
    } else if h >= 1.0 {
        format!("{h:.1} h")
    } else {
        format!("{:.0} min", h * 60.0)
    }
}

/// Was this bucket mostly charging? Majority of its samples, so a bucket
/// that straddles a plug-in keeps an honest signal either way.
fn mostly_charging(b: &MinuteBucket) -> bool {
    b.n > 0 && b.charging_share() >= 0.5
}

/// Local hour of day (0–23) of a bucket minute, given the current local−UTC
/// offset in whole hours. Approximate across a DST switch inside the window
/// (±1 h, one week a year) — attribution is a likelihood, not a clock.
fn local_hour(bucket_minute: u64, utc_offset_hours: i64) -> u8 {
    let local_secs = bucket_minute as i64 * 60 + utc_offset_hours * 3600;
    local_secs.div_euclid(3600).rem_euclid(24) as u8
}

/// Count the last week's warm minutes from the stored 1-minute history.
///
/// `now_minute` is the current unix minute; `utc_offset_hours` places
/// historical minutes into local hours (see [`crate::clock`]).
///
/// Returns `None` when the history holds no temperature readings in the
/// window — an explicit Unavailable, never a zero presented as a reading.
pub fn weekly_report(
    store: &HistoryStore,
    now_minute: u64,
    utc_offset_hours: i64,
) -> Option<WarmHoursReport> {
    let cutoff = now_minute.saturating_sub(RETENTION_MINUTES);
    let mut report = WarmHoursReport {
        warm_minutes: 0,
        charging_warm_minutes: 0,
        peak_hour: None,
        peak_hour_warm_minutes: 0,
        covered_minutes: 0,
    };
    // Warm minutes per local hour; ties keep the earliest hour.
    let mut per_hour = [0u32; 24];
    for b in &store.buckets {
        if b.minute < cutoff {
            continue;
        }
        let Some(temp) = b.temp_mean() else {
            continue; // no reading in this bucket: not counted, not guessed
        };
        report.covered_minutes += 1;
        if temp < WARM_THRESHOLD_C {
            continue;
        }
        report.warm_minutes += 1;
        if mostly_charging(b) {
            report.charging_warm_minutes += 1;
        }
        per_hour[local_hour(b.minute, utc_offset_hours) as usize] += 1;
    }
    if report.covered_minutes == 0 {
        return None;
    }
    let mut best = 0usize;
    for hour in 1..24 {
        if per_hour[hour] > per_hour[best] {
            best = hour;
        }
    }
    if per_hour[best] > 0 {
        report.peak_hour = Some(best as u8);
        report.peak_hour_warm_minutes = per_hour[best];
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::types::{BatterySample, ChargingState};

    /// Any minute-aligned unix time; arithmetic in the tests stays relative.
    /// MIN0 / 60 = 1 800 minutes = 30 h since epoch → local hour 6 (UTC).
    const MIN0: u64 = 1_800 * 60;
    const OFFSET: i64 = 0;

    fn sample(temp: Option<f64>, charging: bool) -> BatterySample {
        BatterySample {
            temperature_c: temp,
            charging_state: Some(if charging {
                ChargingState::Charging
            } else {
                ChargingState::Discharging
            }),
            ..Default::default()
        }
    }

    /// A store with one bucket per minute: `temps[i]` is the mean temperature
    /// of minute `start + i` (None = no reading that minute).
    fn store(temps: &[Option<f64>], charging: &[bool], start: u64) -> HistoryStore {
        let mut store = HistoryStore::default();
        for (i, temp) in temps.iter().enumerate() {
            store.push(&sample(*temp, charging[i]), (start + i as u64) * 60 + 15);
        }
        store
    }

    /// A single warm bucket at an arbitrary minute, inserted directly —
    /// `push` would prune buckets outside the retention window, and the
    /// window-cutoff test needs one that only `weekly_report` can exclude.
    fn warm_bucket_at(store: &mut HistoryStore, minute: u64) {
        store.buckets.push(MinuteBucket {
            minute,
            n: 1,
            soc_sum: 0.0,
            soc_n: 0,
            temp_sum: 35.0,
            temp_n: 1,
            watts_sum: 0.0,
            watts_n: 0,
            soh_sum: 0.0,
            soh_n: 0,
            chg_n: 0,
            cycles_last: None,
        });
    }

    #[test]
    fn warm_count_reconciles_with_stored_history() {
        // 3 warm of 5 recorded minutes: the report must match the store
        // exactly — this is the acceptance criterion.
        let temps = [Some(31.0), Some(29.9), Some(30.0), Some(35.5), Some(22.0)];
        let s = store(&temps, &[false; 5], MIN0);
        let report = weekly_report(&s, MIN0 + 4, OFFSET).expect("temperature data exists");
        assert_eq!(report.warm_minutes, 3);
        assert_eq!(report.warm_hours(), 0.05);
        assert_eq!(report.covered_minutes, 5);
    }

    #[test]
    fn threshold_is_inclusive_at_thirty() {
        let s = store(&[Some(30.0), Some(29.999)], &[false; 2], MIN0);
        let report = weekly_report(&s, MIN0 + 1, OFFSET).unwrap();
        assert_eq!(report.warm_minutes, 1);
    }

    #[test]
    fn only_the_week_window_is_counted() {
        // The stale bucket is inserted directly (push would prune it) so the
        // test exercises weekly_report's own window cutoff, not the pruner's.
        let now_minute = 2_000_000_u64; // far enough past the window to subtract
        let mut s = HistoryStore::default();
        warm_bucket_at(&mut s, now_minute - RETENTION_MINUTES - 1); // just outside
        warm_bucket_at(&mut s, now_minute); // inside
        let report = weekly_report(&s, now_minute, OFFSET).unwrap();
        assert_eq!(
            report.warm_minutes, 1,
            "buckets past the 7-day window are excluded"
        );
        assert_eq!(report.covered_minutes, 1);
    }

    #[test]
    fn no_temperature_data_reports_none() {
        let temps = [None, None];
        let s = store(&temps, &[false; 2], MIN0);
        assert!(weekly_report(&s, MIN0 + 1, OFFSET).is_none());
        assert!(weekly_report(&HistoryStore::default(), MIN0, OFFSET).is_none());
    }

    #[test]
    fn zero_warm_minutes_is_a_real_report_with_a_calm_line() {
        let s = store(&[Some(25.0), Some(26.0)], &[false; 2], MIN0);
        let report = weekly_report(&s, MIN0 + 1, OFFSET).unwrap();
        assert_eq!(report.warm_minutes, 0);
        assert!(report.peak_hour.is_none());
        let line = report.digest_line();
        assert!(line.contains("No time above 30 °C"), "{line}");
        assert!(line.contains("(0.0 h of history)"), "{line}");
    }

    #[test]
    fn mostly_charging_warm_buckets_attribute_to_charging() {
        // 3 warm minutes, all while charging → ChargingUnderLoad.
        let temps = [Some(33.0), Some(34.0), Some(35.0), Some(25.0)];
        let charging = [true, true, true, false];
        let s = store(&temps, &charging, MIN0);
        let report = weekly_report(&s, MIN0 + 3, OFFSET).unwrap();
        assert_eq!(report.charging_warm_minutes, 3);
        assert_eq!(report.cause(), WarmCause::ChargingUnderLoad);
        let line = report.digest_line();
        assert!(line.contains("mostly while charging"), "{line}");
    }

    #[test]
    fn warm_minutes_clustered_in_hours_attribute_to_time_of_day() {
        // 4 warm minutes, none charging: 3 in one local hour (16), one in
        // the next hour. The peak hour holds 3 of 4 warm minutes ≥ 40 %.
        let mut s = HistoryStore::default();
        // Minute 2 400 = 40 h since epoch → hour 16.
        for minute in [2_400, 2_401, 2_402] {
            warm_bucket_at(&mut s, minute);
        }
        // Minute 2 460 = 41 h → hour 17, outside the peak bin.
        warm_bucket_at(&mut s, 2_460);
        let report = weekly_report(&s, 2_460, OFFSET).unwrap();
        assert_eq!(report.warm_minutes, 4);
        assert_eq!(report.peak_hour, Some(16));
        assert_eq!(report.peak_hour_warm_minutes, 3);
        assert_eq!(report.cause(), WarmCause::TimeOfDay);
        let line = report.digest_line();
        assert!(line.contains("16:00–18:00"), "{line}");
    }

    #[test]
    fn scattered_warm_minutes_without_a_pattern_read_mixed() {
        // 4 warm minutes across 4 different hours, not charging: neither
        // charging share nor peak share reaches its threshold.
        let mut s = HistoryStore::default();
        for hour in 0..4u64 {
            s.push(&sample(Some(33.0), false), MIN0 + hour * 3600 + 60);
        }
        // weekly_report takes the current MINUTE: 4 hours after MIN0.
        let now_minute = MIN0 / 60 + 4 * 60;
        let report = weekly_report(&s, now_minute, OFFSET).unwrap();
        assert_eq!(report.warm_minutes, 4);
        assert_eq!(report.cause(), WarmCause::Mixed);
    }

    #[test]
    fn local_hour_maps_buckets_through_the_utc_offset() {
        // MIN0 = 30 h since epoch → UTC hour 6.
        assert_eq!(local_hour(MIN0 / 60, 0), 6);
        assert_eq!(local_hour(MIN0 / 60, 2), 8);
        assert_eq!(local_hour(MIN0 / 60, -5), 1);
        // Day boundary wraps, never panics or goes negative.
        assert_eq!(local_hour(MIN0 / 60, 18), 0);
        assert_eq!(local_hour(MIN0 / 60, 19), 1);
    }

    #[test]
    fn legacy_buckets_without_charging_signal_do_not_invent_a_cause() {
        // Pre-FR-4 buckets load with chg_n = 0: warm time then attributes to
        // time of day or mixed, never to charging.
        let text = format!(
            "{{\"version\":1,\"buckets\":[[{},{}]]}}",
            MIN0,
            [
                "1", // n
                "50.0", "1", // soc
                "33.0", "1", // temp
                "NaN", "0.0", // watts
                "90.0", "1",   // soh
                "NaN", // cycles
            ]
            .join(",")
        );
        let s = HistoryStore::from_json(&text).expect("legacy parse");
        let report = weekly_report(&s, MIN0, OFFSET).unwrap();
        assert_eq!(report.warm_minutes, 1);
        assert_eq!(report.charging_warm_minutes, 0);
    }

    #[test]
    fn digest_line_hrs_formatting_brackets() {
        assert_eq!(format_hours(0.5), "30 min");
        assert_eq!(format_hours(1.0), "1.0 h");
        assert_eq!(format_hours(9.99), "10.0 h");
        assert_eq!(format_hours(168.0), "168 h");
    }

    #[test]
    fn covered_minutes_skip_temperatureless_buckets() {
        // A warm count is only reconcilable when the denominator counts the
        // buckets that actually carried a reading.
        let temps = [Some(31.0), None, Some(31.0)];
        let charging = [false, false, false];
        let s = store(&temps, &charging, MIN0);
        let report = weekly_report(&s, MIN0 + 2, OFFSET).unwrap();
        assert_eq!(report.covered_minutes, 2);
        assert_eq!(report.warm_minutes, 2);
    }
}
