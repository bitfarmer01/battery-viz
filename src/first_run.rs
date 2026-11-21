//! FR-8: first-run history import (longevity roadmap).
//!
//! A new install knows nothing for 7 days. On first run the app imports
//! whatever long-term record the machine already holds and uses it for
//! the first recommendations:
//!
//! - Windows: dated capacity rows from `powercfg /batteryreport`
//!   (`Capabilities::capacity_seed`, FR-7's seed), folded into the FR-7
//!   daily record — the machine's own measured fade is readable on day
//!   one.
//! - macOS: the battery's own lifetime record
//!   (`Capabilities::lifetime_record`, the undocumented ioreg
//!   `LifetimeData` counters) — pack age is known on day one.
//!
//! Platform-neutral by the roadmap's design rule: this module never
//! sniffs the platform and parses nothing itself; every platform answer
//! crossed the FR-0 adapter. Nothing here invents data where the adapter
//! reported none — with no import there is no note, no error, and no
//! placeholder (the acceptance for a parse failure: silence).
//!
//! Boundary: the note is context, not a nudge. The app's one charge-cap
//! interruption stays FR-1's single setup prompt; this module never
//! duplicates it, so a fresh install still sees at most one cap prompt.

use crate::caps::LifetimeRecord;
use crate::record::DailyRecord;

/// What the capability adapter returned for the first-run import: the
/// capacity rows the machine already holds (Windows: the battery report)
/// plus, when the machine keeps one, the battery's own lifetime record
/// (macOS: the ioreg `LifetimeData` counters). Both fields degrade to
/// "unavailable" independently; nothing here invents either.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    pub seeds: Vec<crate::caps::SeedCapacity>,
    pub lifetime: Option<LifetimeRecord>,
}

/// One day-one finding from the imported record, or `None` when nothing
/// was imported — never a placeholder. Windows machines typically show
/// the fade clause, macOS machines the age clause, machines with both
/// sources both, and a machine with no import at all shows nothing.
pub fn first_run_note(record: &DailyRecord, lifetime: Option<LifetimeRecord>) -> Option<String> {
    let fade = fade_clause(record);
    let age = lifetime.map(lifetime_clause);
    match (fade, age) {
        (Some(f), Some(a)) => Some(format!("{f} {a}")),
        (Some(f), None) => Some(f),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// The imported capacity rows as one fade sentence.
///
/// Needs at least two days with SoH readings — a single point states no
/// trend, and a one-row import is reported as nothing rather than as a
/// "trend" from a point to itself.
fn fade_clause(record: &DailyRecord) -> Option<String> {
    // Seed-only rows (`n == 0`): live 1 Hz samples can fold into today's
    // row while the import runs, and their readings are not battery-report
    // data — attributing them to the report would overclaim provenance.
    let with_soh: Vec<(u32, f64)> = record
        .rows
        .iter()
        .filter(|r| r.n == 0)
        .filter_map(|r| r.soh_mean().map(|s| (r.day, s)))
        .collect();
    let (first_day, first_soh) = *with_soh.first()?;
    let (last_day, last_soh) = *with_soh.last()?;
    if first_day == last_day {
        return None;
    }
    Some(format!(
        "imported {} capacity readings from this machine's battery report, \
         spanning {} days — full-charge capacity has moved from {:.0}% to {:.0}%",
        with_soh.len(),
        last_day - first_day,
        first_soh,
        last_soh,
    ))
}

/// The battery's own lifetime clock as one age sentence.
fn lifetime_clause(record: LifetimeRecord) -> String {
    // Mean tropical year (365.2425 d) — the clock counts every hour,
    // asleep or not, so "years of operation" means age, not usage.
    const HOURS_PER_YEAR: f64 = 24.0 * 365.2425;
    let years = record.operating_hours / HOURS_PER_YEAR;
    format!(
        "the battery's own record shows {:.1} years of operation \
         ({} h); its fade trend appears here as daily records accumulate",
        years, record.operating_hours,
    )
}
