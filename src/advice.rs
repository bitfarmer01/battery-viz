//! FR-5: timed full charge check (longevity roadmap).
//!
//! Some users need 100% on days away from power, so a plain 80% cap would
//! leave them running flat — for them, the OS/maker adaptive charging is the
//! intervention that still avoids sitting at full charge all night.
//!
//! Platform-neutral by design (roadmap design rule): every platform-specific
//! answer comes from the FR-0 capability adapter:
//! 1. can the setting's state be read? — `setting_detectability`
//! 2. where does the user change it? — `guidance`
//!
//! On every shipping platform the adapter answers `BehavioralOnly` for
//! adaptive charging, so this feature never claims to know whether the
//! setting is on. The card asks the user to check, pointed at the adapter's
//! guidance; a maker module with no adaptive equivalent (guidance `None`)
//! hides the feature entirely. Nothing here invents data the adapter did
//! not report.
//!
//! "Say so once": the card is shown at most one time ever (see
//! `app::App::mark_full_charge_advice_shown`), and only for users whose
//! stored history shows the pattern that would benefit.

use crate::caps::{Capabilities, Detectability, SettingKind};
use crate::clock::LocalNow;
use crate::history::MinuteBucket;

/// Local daytime window used to spot deep discharges, 9:00–17:00 exclusive.
/// The boundary is derived from the *current* UTC offset (see
/// [`local_offset_secs`]), so it can be off by the timezone's sub-hour
/// remainder — irrelevant at the 40-point swing this detector looks for.
pub const DAYTIME_START_HOUR: u8 = 9;
/// Inclusive last daytime hour.
pub const DAYTIME_END_HOUR: u8 = 16;

/// Daytime minutes of data that make a day observable. One hour of samples
/// is enough to see how far the charge fell; less is treated as no data for
/// that day rather than as a well-behaved plugged-in day.
pub const MIN_DAYTIME_BUCKETS: usize = 60;

/// A daytime charge swing this large (e.g. 95% → 50%) means the user is
/// away from power and running the pack far down during the day.
pub const DEEP_SWING_PCT: f64 = 40.0;
/// …and it must reach this low. A swing that never drops below 45% is a
/// shallow top-up cycle, not a day that needs 100%.
pub const DEEP_FLOOR_PCT: f64 = 45.0;

/// "Regular" = at least this many qualifying days in the retained window.
/// The history store keeps 7 days, so 3+ deep days means the pattern, not
/// an outlier trip.
pub const MIN_QUALIFYING_DAYS: usize = 3;
/// At least this many observable days must exist before "regular" means
/// anything — a fresh install never shows this advice.
pub const MIN_OBSERVABLE_DAYS: usize = 3;

/// The one-time advice card, built entirely from the adapter's guidance.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingAdvice {
    pub message: String,
    /// Deep link from the adapter, when one exists.
    pub url: Option<&'static str>,
}

/// What the one-time card should say, or `None` to hide the feature on this
/// machine (the adapter exposes no adaptive charging to point at).
pub fn adaptive_charging_advice(caps: &dyn Capabilities) -> Option<SettingAdvice> {
    let g = caps.guidance(SettingKind::AdaptiveCharging)?;
    // The setting's state is not readable through the FR-0 trait on any
    // shipping platform (every adapter answers BehavioralOnly), so the
    // message asks the user to check rather than claiming to know. When an
    // adapter becomes `Readable`, read the state here and only advise when
    // it is off — FR-5's "if it is off … say so once".
    let note = match caps.setting_detectability(SettingKind::AdaptiveCharging) {
        Detectability::Readable => String::new(),
        _ => " Battery Viz cannot read its state, so please verify it yourself.".to_string(),
    };
    Some(SettingAdvice {
        // "Check {tool}" (not "make sure it is on"): some guidance text, like
        // Surface's Smart Charging, describes an automatic feature with no
        // setting to flip, and an on/off imperative would contradict it.
        message: format!(
            "Your history shows regular deep daytime discharges, so a plain charge cap would \
             leave you short on days away from power. Adaptive charging avoids that — check \
             {tool}: {steps}.{note}",
            tool = g.tool,
            steps = g.steps
        ),
        url: g.url,
    })
}

/// Does the retained history show regular deep daytime discharge?
///
/// Buckets are grouped into local days via the current UTC offset; a day is
/// observable when it has enough daytime data, and qualifies when the charge
/// swung deeply during daytime hours. The pattern is regular when enough
/// qualifying days exist in the window.
pub fn regular_deep_daytime_discharge(buckets: &[MinuteBucket], local: LocalNow) -> bool {
    let offset = local_offset_secs(local);
    let mut days: Vec<DaySummary> = Vec::new();
    for b in buckets {
        let Some(soc) = b.soc_mean() else { continue };
        let local_secs = b.minute as i64 * 60 + offset;
        let hour = (local_secs.rem_euclid(86_400) / 3_600) as u8;
        if !(DAYTIME_START_HOUR..=DAYTIME_END_HOUR).contains(&hour) {
            continue;
        }
        let day = local_secs.div_euclid(86_400);
        let entry = match days.last_mut() {
            // Buckets arrive in ascending minute order (the store keeps them
            // sorted), so the current day is always the last one seen.
            Some(last) if last.day == day => last,
            _ => {
                days.push(DaySummary {
                    day,
                    max_soc: soc,
                    min_soc: soc,
                    n: 0,
                });
                days.last_mut().expect("just pushed")
            }
        };
        entry.max_soc = entry.max_soc.max(soc);
        entry.min_soc = entry.min_soc.min(soc);
        entry.n += 1;
    }
    let mut observable = 0;
    let mut qualifying = 0;
    for d in &days {
        if d.n < MIN_DAYTIME_BUCKETS {
            continue;
        }
        observable += 1;
        let swing = d.max_soc - d.min_soc;
        if swing >= DEEP_SWING_PCT && d.min_soc <= DEEP_FLOOR_PCT {
            qualifying += 1;
        }
    }
    observable >= MIN_OBSERVABLE_DAYS && qualifying >= MIN_QUALIFYING_DAYS
}

/// Current UTC offset in seconds, derived from the single clock read `local`
/// carries: its local hour and the unix instant it was taken at. Deriving
/// both from one read keeps an hour tick between two reads from shifting the
/// daytime window (review M1). Sub-hour timezones (e.g. +5:30) keep a
/// residual of up to 30 minutes; the daytime window tolerates it.
fn local_offset_secs(local: LocalNow) -> i64 {
    let utc_hour = local.unix_secs.rem_euclid(86_400) / 3_600;
    (local.hour as i64 - utc_hour) * 3_600
}

/// Per-local-day daytime charge summary.
struct DaySummary {
    day: i64,
    max_soc: f64,
    min_soc: f64,
    n: usize,
}
