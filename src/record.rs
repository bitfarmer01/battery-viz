//! Long-term daily health record (FR-7, longevity roadmap).
//!
//! One summary row per local day, kept indefinitely: state-of-health
//! capacity, cycle count, hours at high charge, warm hours. Where the
//! 1-minute history (`history.rs`) expires after 7 days, this store is what
//! can show capacity fade a year later.
//!
//! Bounded by construction: a row is a fixed-width number JSON array
//! (~80 bytes), so a year of daily rows is roughly 30 KB; a generous hard
//! cap ([`MAX_DAYS`]) keeps the file small even after a decade of use.
//!
//! Persistence mirrors `history.rs`: a single versioned JSON file in the
//! platform data dir, written atomically, with an unreadable file preserved
//! as `<name>.corrupt` and the failure surfaced, never silently discarded.
//! The tokenizer is shared with the history parser.
//!
//! Windows seeds the record on first run from `powercfg /batteryreport`
//! (which already holds capacity history) — through the FR-0 capability
//! adapter, never parsed by feature code. Nothing is invented where the
//! adapter reports nothing: an unseeded record simply starts empty.

use std::io::Write;
use std::path::Path;

use crate::collect::types::{state_of_health_pct, BatterySample, ChargingState};
use crate::history::LoadError;

/// A sample counts as "at high charge" only when it sits meaningfully above
/// the top of the longevity band — the dwell convention's threshold
/// (80% + hysteresis margin, re-audit N3), so a machine correctly holding
/// exactly AT its 80% cap accrues no high-charge time, in the daily record
/// just as in the live dwell total. Re-exported, not re-declared: the daily
/// record must track the H4 dwell convention by construction, never by a
/// copied constant.
pub use crate::dwell::dwell_threshold as high_charge_threshold;
/// A sample counts as a warm hour at this pack temperature or above
/// (degrees Celsius) — the same "warm" floor FR-4 counts weekly.
pub use crate::warm::WARM_THRESHOLD_C as WARM_TEMP_C;

/// Hard cap on stored days: ten years plus margin. Daily rows are kept
/// "indefinitely" in practice — the cap exists so the file stays bounded
/// even under pathological clocks, evicting oldest first.
pub const MAX_DAYS: usize = 10 * 366;

/// Per-interval sanity cap on credited wall-clock time (re-audit N5). The
/// dwell convention's 60 s cap exists to keep stale qualification from
/// backfilling a SCORE factor, but it can never credit a sleep gap — sleep
/// arrives as ONE multi-hour interval between the last pre-sleep and first
/// post-wake samples, which is exactly the time N5 says must be counted.
/// The record's cap is therefore sized to cover an overnight session
/// (generous) while still bounding a pathological gap (machine off for
/// days, wedged clock) to a bounded over-credit in the SAFE direction —
/// under-counting high-charge time, never inventing it.
const MAX_INTERVAL_S: f64 = 12.0 * 3600.0;

/// On-disk schema version. A mismatch is treated as a corrupt file.
/// Version 5 adds the wall-clock second accumulators (`high_s`, `warm_s`,
/// re-audit N5): sample counts assumed 1 Hz, but a sleeping machine
/// produces no samples, so high-charge/warm time now accrues from the
/// wall-clock delta between consecutive samples. Migration at load: v4
/// rows were counted under the SAME threshold at a documented 1 Hz
/// cadence, so their `high_n`/`warm_n` convert exactly to seconds; v1–v3
/// rows keep the wave-1a retirements (`high_n` → 0, different threshold
/// era) and their warm counts also convert exactly (the warm threshold
/// never changed). Version 4 kept version 3's row layout: the bump marks
/// the threshold era — from v4 on, `high_n` counts samples above the N3
/// dwell threshold (band top + hysteresis) rather than at-or-above exactly
/// 80%. Version 3 added per-day unplugged-discharge tracking (`disch_n`,
/// `disch_min`, `disch_max`) for FR-2's usage-based cap recommendation;
/// version 2 rows (9 fields) still parse with those fields absent, and
/// version 1 rows (7 fields) with the SoC fields absent too. Known
/// versions are matched exactly.
const SCHEMA_VERSION: f64 = 5.0;

/// One day's summary. Sums + counts per metric so a partially-populated
/// day (missing temperature on Windows, say) never poisons the mean.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DailyRow {
    /// Days since the local Unix epoch (same counter as `clock::LocalNow.day`).
    pub day: u32,
    /// Samples folded in, regardless of which metrics they carried.
    pub n: u32,
    pub soh_sum: f64,
    pub soh_n: u32,
    /// Last cycle count seen that day (cumulative counter, not a mean).
    pub cycles_last: Option<u32>,
    /// Samples meaningfully above the high-charge threshold
    /// (`high_charge_threshold()` — the dwell convention).
    pub high_n: u32,
    /// Wall-clock seconds spent above the high-charge threshold that day
    /// (re-audit N5). Sample counts assumed 1 Hz, but a sleeping machine
    /// produces no samples — hours derive from elapsed time, not counts.
    pub high_s: f64,
    /// Samples at or above `WARM_TEMP_C`.
    pub warm_n: u32,
    /// Wall-clock seconds at or above `WARM_TEMP_C` that day (re-audit N5).
    pub warm_s: f64,
    /// Samples that carried a state of charge (FR-2: the quality gate for
    /// the day's minimum — a day with too few readings contributes none).
    pub soc_n: u32,
    /// Lowest state of charge observed that day, percent. `None` when no
    /// sample carried a state of charge.
    pub soc_min: Option<f64>,
    /// Samples observed while unplugged (discharging) that carried a state
    /// of charge — the quality gate for the day's discharge range.
    pub disch_n: u32,
    /// Lowest state of charge observed while unplugged that day, percent.
    /// `None` when no unplugged sample carried a state of charge.
    pub disch_min: Option<f64>,
    /// Highest state of charge observed while unplugged that day, percent.
    /// `None` when no unplugged sample carried a state of charge.
    pub disch_max: Option<f64>,
    /// Whether this day's format ever tracked unplugged discharge. Rows
    /// written by the tracking build (schema v3+) carry it; days parsed
    /// from v1/v2 files observed no discharge tracking, and a blind day
    /// must never qualify the recommendation gate on data it does not
    /// have (review MEDIUM 1). Derived at parse/fold time, never
    /// serialized.
    pub disch_tracked: bool,
}

impl DailyRow {
    fn new(day: u32) -> Self {
        DailyRow {
            day,
            n: 0,
            soh_sum: 0.0,
            soh_n: 0,
            cycles_last: None,
            high_n: 0,
            warm_n: 0,
            high_s: 0.0,
            warm_s: 0.0,
            soc_n: 0,
            soc_min: None,
            disch_n: 0,
            disch_min: None,
            disch_max: None,
            disch_tracked: true,
        }
    }

    /// Mean state-of-health capacity recorded that day. Consumed from
    /// FR-8 (import) onward; stored now so the record is complete from
    /// day one.
    pub fn soh_mean(&self) -> Option<f64> {
        if self.soh_n == 0 {
            None
        } else {
            Some(self.soh_sum / self.soh_n as f64)
        }
    }

    /// Hours at high charge that day, from the accumulated wall-clock
    /// seconds (`high_s`, re-audit N5) — not a sample count: a sleeping
    /// machine produces no samples, yet its high-charge hours are real.
    /// Consumed from FR-4 (warm hours) and FR-9 (digest) onward; stored now
    /// so the record is complete from day one.
    #[allow(dead_code)]
    pub fn high_hours(&self) -> f64 {
        self.high_s / 3600.0
    }

    /// Warm hours that day, same wall-clock conversion (`warm_s`).
    /// Consumed from FR-4 (warm hours) and FR-9 (digest) onward.
    #[allow(dead_code)]
    pub fn warm_hours(&self) -> f64 {
        self.warm_s / 3600.0
    }
}

/// One summary row per day, ordered by day.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DailyRecord {
    pub rows: Vec<DailyRow>,
}

/// Outcome of [`DailyRecord::load`]: the record to start from, plus a
/// non-fatal load failure when the existing file was unreadable.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecordLoad {
    pub record: DailyRecord,
    pub load_error: Option<LoadError>,
}

impl DailyRecord {
    /// Fold one sample into its day row. Returns true when a new row was
    /// created (a checkpoint trigger, like the history store's).
    ///
    /// Counts the sample but credits no wall-clock time — the shape tests
    /// and callers that have no interval to speak of use. The live sample
    /// stream calls [`Self::fold_timed`], which is where re-audit N5's
    /// wall-clock accounting lives.
    #[allow(dead_code)] // test convenience; the live stream calls fold_timed
    pub fn fold(&mut self, s: &BatterySample, day: u32) -> bool {
        self.fold_timed(s, day, 0.0)
    }

    /// Fold one sample, crediting `elapsed_s` — the wall-clock seconds
    /// since the previous sample (re-audit N5) — to the day's high-charge
    /// and warm totals when THIS sample, the gap's closing edge, qualifies.
    /// The closing edge governs: a laptop asleep on the charger overnight
    /// produces ONE gap whose closing sample is still Full/onAC at high
    /// charge, so the night lands in the day row where it belongs; a wake
    /// mid-drain (closing sample unplugged, below threshold) credits
    /// nothing. Credited time is bounded by [`MAX_INTERVAL_S`] so a
    /// multi-day outage cannot backfill a week into one row.
    pub fn fold_timed(&mut self, s: &BatterySample, day: u32, elapsed_s: f64) -> bool {
        let credit = if elapsed_s.is_finite() {
            elapsed_s.clamp(0.0, MAX_INTERVAL_S)
        } else {
            0.0
        };
        // One row per day, ever: a clock that revisits an already-recorded
        // day (timezone change, NTP step back then forward) folds into that
        // row — a duplicate would split the day's samples silently and
        // permanently, both rows serializing to disk.
        let (idx, created) = match self.rows.binary_search_by_key(&day, |r| r.day) {
            Ok(i) => (i, false),
            Err(i) => {
                // A clock step backwards lands out of order; keep the rows
                // sorted by day regardless.
                self.rows.insert(i, DailyRow::new(day));
                (i, true)
            }
        };
        let row = &mut self.rows[idx];
        row.n += 1;
        // The tracking build observes this day from here on: even a day
        // first written by an older schema now carries real discharge
        // tracking (review MEDIUM 1).
        row.disch_tracked = true;
        if let Some(v) = s.state_of_charge {
            row.soc_n += 1;
            // FR-2: the day's lowest charge. A one-sample sensor glitch
            // would drag this down, and the cap-level recommendation errs
            // high (a lower minimum means a higher covering cap) — the
            // safe direction, at the cost of some savings.
            row.soc_min = Some(row.soc_min.map_or(v, |m: f64| m.min(v)));
            // Re-audit N1: the day's unplugged discharge range — the raw
            // material of "how deep does this user actually drain". Only
            // discharging samples count: a machine parked at 100% on AC
            // never drains, and that must read as zero usage, not as the
            // parked level.
            if s.charging_state == Some(ChargingState::Discharging) {
                row.disch_n += 1;
                row.disch_min = Some(row.disch_min.map_or(v, |m: f64| m.min(v)));
                row.disch_max = Some(row.disch_max.map_or(v, |m: f64| m.max(v)));
            }
        }
        if let Some(v) = state_of_health_pct(s) {
            row.soh_sum += v;
            row.soh_n += 1;
        }
        if let Some(c) = s.cycle_count {
            row.cycles_last = Some(c);
        }
        if s.state_of_charge
            .is_some_and(|soc| soc >= high_charge_threshold())
        {
            row.high_n += 1;
            row.high_s += credit;
        }
        if s.temperature_c.is_some_and(|t| t >= WARM_TEMP_C) {
            row.warm_n += 1;
            row.warm_s += credit;
        }
        self.prune();
        created
    }

    /// Enforce the hard cap by evicting the oldest days.
    fn prune(&mut self) {
        if self.rows.len() > MAX_DAYS {
            let extra = self.rows.len() - MAX_DAYS;
            self.rows.drain(..extra);
        }
    }

    /// Seed the record from capacity history the machine already holds
    /// (Windows: `powercfg /batteryreport`, offered by the capability
    /// adapter). Fills only days with no row and never overwrites sampled
    /// rows; returns how many rows were added. Empty seeds add nothing.
    pub fn seed(&mut self, seeds: &[crate::caps::SeedCapacity]) -> usize {
        let mut added = 0;
        for s in seeds {
            if self.rows.iter().any(|r| r.day == s.day) {
                continue;
            }
            let mut row = DailyRow::new(s.day);
            row.soh_sum = s.soh_pct;
            row.soh_n = 1;
            let idx = self.rows.partition_point(|r| r.day < s.day);
            self.rows.insert(idx, row);
            added += 1;
        }
        self.rows.sort_by_key(|r| r.day);
        self.prune();
        added
    }

    /// Serialize to the on-disk JSON schema (v5):
    /// `{"version":5,"rows":[[day,n,soh_sum,soh_n,cycles,high_n,warm_n,high_s,warm_s,soc_n,soc_min,disch_n,disch_min,disch_max],...]}`
    /// (cycles, soc_min and the discharge bounds are NaN when unknown).
    /// Rust's f64 Display round-trips.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(130 * self.rows.len() + 32);
        out.push_str(&format!("{{\"version\":{},\"rows\":[", SCHEMA_VERSION));
        for (i, r) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "[{},{},{},{},{},{},{},{},{},{},{},{},{},{}]",
                r.day,
                r.n,
                r.soh_sum,
                r.soh_n,
                r.cycles_last.map(|c| c as f64).unwrap_or(f64::NAN),
                r.high_n,
                r.warm_n,
                r.high_s,
                r.warm_s,
                r.soc_n,
                r.soc_min.unwrap_or(f64::NAN),
                r.disch_n,
                r.disch_min.unwrap_or(f64::NAN),
                r.disch_max.unwrap_or(f64::NAN),
            ));
        }
        out.push_str("]}");
        out
    }

    /// Parse the schema written by [`Self::to_json`]. Any deviation — wrong
    /// version, wrong arity, malformed numbers — is an error, which callers
    /// turn into a fresh record.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut p = crate::history::Parser::new(text);
        p.skip_ws();
        p.expect(b'{')?;
        p.expect_key("version")?;
        let version = p.number()?;
        // Known versions only, exactly (review LOW 2): 1 through 5 ever
        // existed; anything else — past or future — is corruption here.
        if !matches!(version, 1.0 | 2.0 | 3.0 | 4.0 | 5.0) {
            return Err(format!("unsupported schema version {version}"));
        }
        // Version 1 rows carry 7 fields (no SoC tracking); version 2, 9;
        // versions 3 and 4, 12 (adds the unplugged discharge range);
        // version 5, 14 (adds the wall-clock second accumulators, N5).
        let arity = match version {
            1.0 => 7,
            2.0 => 9,
            5.0 => 14,
            _ => 12,
        };
        // Threshold era (review LOW 1): a row written before v4 counted
        // `high_n` at or above exactly 80% — including a correctly capped
        // machine's entire hold. Such hours must not be re-labeled as time
        // above the current threshold, so the legacy stock retires once at
        // load, exactly like the live dwell total.
        let legacy_era = version < 4.0;
        // Discharge tracking (review MEDIUM 1): a v1/v2 day observed none.
        let disch_tracked = version >= 3.0;
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("rows")?;
        p.skip_ws();
        p.expect(b'[')?;
        let mut rows = Vec::new();
        loop {
            p.skip_ws();
            if p.eat(b']') {
                break;
            }
            p.expect(b'[')?;
            let f: Vec<f64> = p.number_list()?;
            if f.len() != arity {
                return Err(format!("row arity {} != {arity}", f.len()));
            }
            let cycles = f[4];
            if !cycles.is_nan() && (cycles < 0.0 || cycles.fract() != 0.0) {
                return Err(format!("invalid cycle count {cycles}"));
            }
            let soc_min = if arity >= 9 {
                let v = f[if arity == 14 { 10 } else { 8 }];
                if v.is_nan() {
                    None
                } else if !(0.0..=100.0).contains(&v) {
                    return Err(format!("invalid state of charge {v}"));
                } else {
                    Some(v)
                }
            } else {
                // A v1 day observed no SoC tracking: no minimum, never a guess.
                None
            };
            // v3: the unplugged discharge range. Older versions observed no
            // discharge tracking — no range, never a guess.
            let pct = |i: usize| -> Result<Option<f64>, String> {
                if arity < 12 {
                    return Ok(None);
                }
                let v = f[i];
                if v.is_nan() {
                    Ok(None)
                } else if !(0.0..=100.0).contains(&v) {
                    Err(format!("invalid state of charge {v}"))
                } else {
                    Ok(Some(v))
                }
            };
            // Field indices shift at v5: the wall-clock second accumulators
            // (high_s, warm_s) are inserted after warm_n, moving every later
            // field two places right (re-audit N5).
            let (i_soc_n, i_disch_n, i_disch_min, i_disch_max) = if arity == 14 {
                (9, 11, 12, 13)
            } else {
                (7, 9, 10, 11)
            };
            rows.push(DailyRow {
                day: f[0] as u32,
                n: f[1] as u32,
                soh_sum: f[2],
                soh_n: f[3] as u32,
                cycles_last: if cycles.is_nan() {
                    None
                } else {
                    Some(cycles as u32)
                },
                high_n: if legacy_era { 0 } else { f[5] as u32 },
                warm_n: f[6] as u32,
                // N5 wall-clock seconds: v5 rows carry them directly.
                // Older rows convert their sample counts exactly — the
                // collectors ran at a documented 1 Hz, and v4 counted under
                // the same threshold; a pre-v4 high count stays retired
                // with its count, never re-labeled as hours it never was.
                high_s: if arity == 14 {
                    f[7]
                } else if !legacy_era {
                    f[5]
                } else {
                    0.0
                },
                warm_s: if arity == 14 { f[8] } else { f[6] },
                soc_n: if arity >= 9 { f[i_soc_n] as u32 } else { 0 },
                soc_min,
                disch_n: if disch_tracked {
                    f[i_disch_n] as u32
                } else {
                    0
                },
                disch_min: pct(i_disch_min)?,
                disch_max: pct(i_disch_max)?,
                disch_tracked,
            });
            p.skip_ws();
            if !p.eat(b',') {
                p.expect(b']')?;
                break;
            }
        }
        p.skip_ws();
        p.expect(b'}')?;
        Ok(DailyRecord { rows })
    }

    /// Load from disk. A missing file yields an empty record; a file that
    /// cannot be read or parsed starts fresh too, but is first moved aside
    /// to `<name>.corrupt` (kept, never deleted) and the failure reported —
    /// a silently discarded record looks identical to a never-recorded one.
    pub fn load(path: &Path) -> RecordLoad {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return RecordLoad::default();
            }
            Err(e) => return Self::fresh_after_load_failure(path, format!("read: {e}")),
        };
        match Self::from_json(&text) {
            Ok(record) => RecordLoad {
                record,
                load_error: None,
            },
            Err(e) => Self::fresh_after_load_failure(path, format!("parse: {e}")),
        }
    }

    fn fresh_after_load_failure(path: &Path, message: String) -> RecordLoad {
        RecordLoad {
            record: DailyRecord::default(),
            load_error: Some(crate::history::quarantine(path, message)),
        }
    }

    /// Atomically rewrite the store to disk (temp file + fsync + rename),
    /// exactly the `HistoryStore::save` discipline.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create data dir: {e}"))?;
        }
        let tmp = crate::history::temp_sibling(path);
        let write = (|| {
            let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create temp: {e}"))?;
            f.write_all(self.to_json().as_bytes())
                .map_err(|e| format!("write record: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync record: {e}"))
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("atomic rename: {e}")
        })
    }
}

/// Wall-clock seconds the record can honestly account between two samples
/// (re-audit N5): the delta between consecutive samples' unix timestamps.
/// A first-ever sample (no predecessor) and a backward clock step credit
/// nothing; a forward gap is credited in full and bounded per interval by
/// [`MAX_INTERVAL_S`] at fold time. Wall clock, not monotonic: the sleep
/// hours N5 restores do not tick a monotonic clock on every platform.
pub fn interval_seconds(prev_unix: Option<u64>, now_unix: u64) -> f64 {
    match prev_unix {
        Some(prev) if now_unix > prev => (now_unix - prev) as f64,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::types::BatterySample;

    fn sample(soc: f64, temp: Option<f64>) -> BatterySample {
        BatterySample {
            state_of_charge: Some(soc),
            temperature_c: temp,
            design_capacity: Some(50_000.0),
            full_charge_capacity: Some(45_000.0),
            cycle_count: Some(120),
            ..Default::default()
        }
    }

    #[test]
    fn fold_tracks_the_days_lowest_charge() {
        let mut rec = DailyRecord::default();
        rec.fold(&sample(80.0, None), 3);
        rec.fold(&sample(45.5, None), 3);
        rec.fold(&sample(60.0, None), 3);
        let r = rec.rows[0];
        assert_eq!(r.soc_n, 3);
        assert_eq!(r.soc_min, Some(45.5));
        // A day with no state-of-charge readings has no minimum, never zero.
        let mut rec = DailyRecord::default();
        rec.fold(&BatterySample::default(), 3);
        assert_eq!(rec.rows[0].soc_n, 0);
        assert_eq!(rec.rows[0].soc_min, None);
    }

    #[test]
    fn v1_record_parses_with_no_soc_minimum() {
        // A file written by the pre-FR-2 schema: those days contribute no
        // minimum rather than a guessed one.
        let v1 = "{\"version\":1,\"rows\":[[3,2,180.0,2,120,1,0]]}";
        let rec = DailyRecord::from_json(v1).expect("v1 parses");
        assert_eq!(rec.rows.len(), 1);
        assert_eq!(rec.rows[0].soc_n, 0);
        assert_eq!(rec.rows[0].soc_min, None);
        // Legacy-era hours retire at load (review LOW 1): the row counted
        // `high_n` at or above exactly 80% — possibly a correctly capped
        // machine's whole hold — so the stock zeroes rather than being
        // re-labeled under the new threshold's wording.
        assert_eq!(rec.rows[0].high_n, 0);
        // …and folding a new sample into that v1 day starts the minimum
        // from the new sample alone (earlier lows are unknowable).
        let mut rec = rec;
        rec.fold(&sample(70.0, None), 3);
        assert_eq!(rec.rows[0].soc_min, Some(70.0));
    }

    #[test]
    fn v2_round_trip_preserves_the_soc_minimum() {
        let mut rec = DailyRecord::default();
        rec.fold(&sample(80.0, None), 3);
        rec.fold(&sample(41.5, None), 3);
        let round = DailyRecord::from_json(&rec.to_json()).expect("parse");
        assert_eq!(round.rows[0].soc_min, Some(41.5));
        assert_eq!(round.rows[0].soc_n, 2);
    }

    #[test]
    fn unknown_record_schema_version_is_corrupt() {
        // Review LOW 2: known versions only, exactly — 1, 2, 3, 4 and 5
        // ever existed. Anything else, past or future, is corruption here,
        // not a best-effort parse of a format nobody wrote.
        for version in ["6.0", "2.5", "0.9"] {
            let text = format!("{{\"version\":{version},\"rows\":[[3,2,180.0,2,120,1,0]]}}");
            assert!(
                DailyRecord::from_json(&text).is_err(),
                "version {version} must not parse"
            );
        }
        // All five known versions still load.
        for version in ["1.0", "2.0", "3.0", "4.0", "5.0"] {
            let rows = if version == "1.0" {
                "[3,2,180.0,2,120,1,0]"
            } else if version == "2.0" {
                "[3,2,180.0,2,120,1,0,2,45.5]"
            } else if version == "5.0" {
                "[3,2,180.0,2,120,1,0,1.0,0.0,2,45.5,1,60.0,80.0]"
            } else {
                "[3,2,180.0,2,120,1,0,2,45.5,1,60.0,80.0]"
            };
            let text = format!("{{\"version\":{version},\"rows\":[{rows}]}}");
            assert!(
                DailyRecord::from_json(&text).is_ok(),
                "version {version} must parse"
            );
        }
    }

    /// A sample observed while unplugged (discharging).
    fn unplugged(soc: f64) -> BatterySample {
        BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(crate::collect::types::ChargingState::Discharging),
            ..Default::default()
        }
    }

    /// Re-audit N1: the day's unplugged discharge range. A plugged-in day
    /// parked at 100% records no range at all — parked is not usage.
    #[test]
    fn fold_tracks_the_unplugged_discharge_range() {
        let mut rec = DailyRecord::default();
        rec.fold(&unplugged(70.0), 3);
        rec.fold(&unplugged(30.0), 3);
        rec.fold(&unplugged(55.0), 3);
        // Plugged samples never contribute to the unplugged range.
        rec.fold(&sample(100.0, None), 3);
        let r = rec.rows[0];
        assert_eq!(r.disch_n, 3);
        assert_eq!(r.disch_min, Some(30.0));
        assert_eq!(r.disch_max, Some(70.0));

        // A machine that never unplugs: no range, never a guessed one.
        let mut rec = DailyRecord::default();
        rec.fold(&sample(100.0, None), 3);
        let r = rec.rows[0];
        assert_eq!(r.disch_n, 0);
        assert_eq!(r.disch_min, None);
        assert_eq!(r.disch_max, None);
    }

    /// A v2 file (pre-N1 schema) parses with no discharge range rather than
    /// a guessed one; a v3 round trip preserves the range exactly.
    #[test]
    fn v2_parses_without_discharge_range_v3_round_trips_it() {
        let v2 = "{\"version\":2,\"rows\":[[3,2,180.0,2,120,1,0,2,41.5]]}";
        let rec = DailyRecord::from_json(v2).expect("v2 parses");
        assert_eq!(rec.rows[0].soc_min, Some(41.5));
        assert_eq!(rec.rows[0].disch_n, 0);
        assert_eq!(rec.rows[0].disch_min, None);
        assert_eq!(rec.rows[0].disch_max, None);

        let mut rec = DailyRecord::default();
        rec.fold(&unplugged(80.0), 3);
        rec.fold(&unplugged(22.5), 3);
        let round = DailyRecord::from_json(&rec.to_json()).expect("v3 parse");
        assert_eq!(round.rows[0].disch_n, 2);
        assert_eq!(round.rows[0].disch_min, Some(22.5));
        assert_eq!(round.rows[0].disch_max, Some(80.0));
    }

    #[test]
    fn same_day_samples_fold_into_one_row() {
        let mut rec = DailyRecord::default();
        assert!(rec.fold(&sample(50.0, Some(30.0)), 20_000));
        assert!(!rec.fold(&sample(60.0, Some(34.0)), 20_000));
        assert_eq!(rec.rows.len(), 1);
        let r = rec.rows[0];
        assert_eq!(r.n, 2);
        assert_eq!(r.soh_mean(), Some(90.0)); // 45_000 / 50_000
        assert_eq!(r.cycles_last, Some(120));
    }

    #[test]
    fn hours_at_high_charge_and_warm_hours_count_wall_clock_seconds() {
        // Re-audit N5: hours come from the wall-clock seconds credited by
        // fold_timed, not from sample counts — a sleeping machine produces
        // no samples, yet its high-charge time is real.
        let mut rec = DailyRecord::default();
        // Two high-charge samples 1 s apart, one warm, one neither.
        rec.fold_timed(&sample(85.0, Some(31.0)), 7, 0.0); // first sample: no interval yet
        rec.fold_timed(&sample(95.0, Some(45.0)), 7, 1.0);
        rec.fold_timed(&sample(50.0, Some(10.0)), 7, 1.0); // below both: counts, credits nothing
        let r = rec.rows[0];
        assert_eq!(r.high_n, 2);
        assert_eq!(r.warm_n, 2);
        assert!((r.high_hours() - 1.0 / 3600.0).abs() < 1e-12);
        assert!((r.warm_hours() - 1.0 / 3600.0).abs() < 1e-12);
        // Exactly on the thresholds counts as in.
        let mut rec = DailyRecord::default();
        rec.fold_timed(&sample(high_charge_threshold(), Some(WARM_TEMP_C)), 7, 1.0);
        assert_eq!(rec.rows[0].high_n, 1);
        assert_eq!(rec.rows[0].warm_n, 1);
        assert!((rec.rows[0].high_hours() - 1.0 / 3600.0).abs() < 1e-12);
    }

    /// Re-audit N5, the headline scenario: a laptop on the charger
    /// overnight. Sleep produces ONE multi-hour gap — the last pre-sleep
    /// and first post-wake samples are the only evidence — and the wake
    /// sample still qualifies (Full, on AC, at 100%), so the night must
    /// land in the day row as high-charge hours.
    #[test]
    fn an_overnight_sleep_gap_is_credited_as_high_charge_time() {
        let mut rec = DailyRecord::default();
        // 22:00 sample, then nothing until the 08:00 wake sample.
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, 0.0);
        rec.fold_timed(&sample(100.0, Some(26.0)), 7, 8.0 * 3600.0);
        let r = rec.rows[0];
        assert_eq!(r.high_n, 2);
        assert!((r.high_hours() - 8.0).abs() < 1e-9, "{}", r.high_hours());
        // The wake sample is cool: a sleep gap is never credited as warm
        // time — only the closing edge's temperature speaks.
        assert_eq!(r.warm_n, 1);
        assert!((r.warm_hours() - 0.0).abs() < 1e-12);
    }

    /// Re-audit N5: the gap is attributed by the state at the gap's
    /// closing edge. Waking unplugged mid-drain credits nothing, however
    /// the machine was parked when it fell asleep.
    #[test]
    fn a_gap_is_credited_by_the_closing_edge_state() {
        let mut rec = DailyRecord::default();
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, 0.0);
        // Wake unplugged below the threshold: the night was NOT high-charge
        // time from this evidence.
        rec.fold_timed(&unplugged(40.0), 7, 8.0 * 3600.0);
        assert_eq!(rec.rows[0].high_n, 1);
        assert!((rec.rows[0].high_hours() - 0.0).abs() < 1e-12);

        // Waking warm (charger fault overnight, say) credits warm time.
        let mut rec = DailyRecord::default();
        rec.fold_timed(&sample(60.0, Some(30.0)), 7, 0.0);
        rec.fold_timed(&sample(60.0, Some(47.0)), 7, 4.0 * 3600.0);
        assert!((rec.rows[0].warm_hours() - 4.0).abs() < 1e-9);
        assert!((rec.rows[0].high_hours() - 0.0).abs() < 1e-12);
    }

    /// A multi-day outage (machine off, wedged clock) must not backfill a
    /// week into one row: the per-interval sanity cap bounds the credit.
    #[test]
    fn a_pathological_gap_is_capped_not_backfilled() {
        let mut rec = DailyRecord::default();
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, 0.0);
        // Off for a week, then wake on the charger at 100%: at most
        // MAX_INTERVAL_S is credited to this one row.
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, 7.0 * 24.0 * 3600.0);
        assert!(rec.rows[0].high_hours() < 12.5);
        assert_eq!(rec.rows[0].high_hours(), 12.0);
        // NaN or negative elapsed credits nothing rather than poisoning.
        let mut rec = DailyRecord::default();
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, f64::NAN);
        rec.fold_timed(&sample(100.0, Some(30.0)), 7, -50.0);
        assert!((rec.rows[0].high_hours() - 0.0).abs() < 1e-12);
    }

    #[test]
    fn interval_seconds_reads_wall_clock_deltas() {
        assert_eq!(interval_seconds(None, 1_000), 0.0, "first sample");
        assert_eq!(interval_seconds(Some(1_000), 3_600), 2_600.0);
        assert_eq!(interval_seconds(Some(3_600), 1_000), 0.0, "clock back");
        assert_eq!(interval_seconds(Some(1_000), 1_000), 0.0, "same instant");
    }

    /// A v4 record upgrades exactly: its high counts were taken under the
    /// SAME threshold at a documented 1 Hz cadence, so converting them to
    /// seconds is a unit change, not a re-label; a v3 day keeps its
    /// retirement (different threshold era) and its warm seconds.
    #[test]
    fn v4_high_counts_convert_to_seconds_v3_stay_retired() {
        // v4 row: day 3, n=2, soh, 2, cycles 120, high_n=600, warm_n=100,
        // soc fields…
        let v4 = "{\"version\":4,\"rows\":[[3,2,180.0,2,120,600,100,2,45.5,1,60.0,80.0]]}";
        let rec = DailyRecord::from_json(v4).expect("v4 parses");
        assert_eq!(rec.rows[0].high_n, 600);
        assert_eq!(rec.rows[0].high_s, 600.0);
        assert_eq!(rec.rows[0].warm_s, 100.0);
        assert!((rec.rows[0].high_hours() - 600.0 / 3600.0).abs() < 1e-12);

        // v3 row: same threshold era as v1/v2 for high (retired), warm
        // threshold unchanged so warm counts convert exactly.
        let v3 = "{\"version\":3,\"rows\":[[3,2,180.0,2,120,600,100,2,45.5,1,60.0,80.0]]}";
        let rec = DailyRecord::from_json(v3).expect("v3 parses");
        assert_eq!(rec.rows[0].high_n, 0, "pre-v4 high stock retired");
        assert_eq!(rec.rows[0].high_s, 0.0);
        assert_eq!(rec.rows[0].warm_s, 100.0);
    }

    #[test]
    fn missing_metrics_never_poison_the_day() {
        let mut rec = DailyRecord::default();
        // A sample with no capacity, temperature, or cycle data.
        rec.fold(&BatterySample::default(), 5);
        let r = rec.rows[0];
        assert_eq!(r.soh_mean(), None);
        assert_eq!(r.cycles_last, None);
        assert_eq!(r.high_n, 0);
        assert_eq!(r.warm_n, 0);
        // …and a later sample with data still lands cleanly.
        rec.fold(&sample(50.0, Some(30.0)), 5);
        assert_eq!(rec.rows[0].soh_mean(), Some(90.0));
    }

    #[test]
    fn out_of_order_days_stay_sorted() {
        let mut rec = DailyRecord::default();
        assert!(rec.fold(&sample(50.0, None), 100));
        assert!(rec.fold(&sample(50.0, None), 98));
        assert!(rec.fold(&sample(50.0, None), 99));
        let days: Vec<u32> = rec.rows.iter().map(|r| r.day).collect();
        assert_eq!(days, vec![98, 99, 100]);
    }

    #[test]
    fn revisiting_a_recorded_day_folds_into_it_never_duplicates() {
        // Clock moves back across a recorded day, then forward onto it
        // again (timezone change, NTP correction): the day must keep ONE
        // row, folding the late sample in — a duplicate row would split
        // the day's samples silently and permanently on disk.
        let mut rec = DailyRecord::default();
        assert!(rec.fold(&sample(90.0, None), 100));
        assert!(rec.fold(&sample(50.0, None), 99));
        assert!(!rec.fold(&sample(88.0, None), 100));
        assert_eq!(rec.rows.len(), 2, "no duplicate row for day 100");
        assert_eq!(rec.rows[1].day, 100);
        assert_eq!(rec.rows[1].n, 2, "both day-100 samples in one row");
        assert_eq!(rec.rows[0].day, 99);
    }

    #[test]
    fn hard_cap_evicts_oldest_days() {
        let mut rec = DailyRecord::default();
        for day in 0..(MAX_DAYS as u32 + 5) {
            rec.fold(&sample(50.0, None), day);
        }
        assert_eq!(rec.rows.len(), MAX_DAYS);
        assert_eq!(rec.rows[0].day, 5, "oldest days evicted first");
    }

    #[test]
    fn json_round_trip_preserves_rows() {
        let mut rec = DailyRecord::default();
        rec.fold(&sample(41.5, Some(28.0)), 10);
        rec.fold(&BatterySample::default(), 11); // all metrics missing
        let round = DailyRecord::from_json(&rec.to_json()).expect("parse");
        assert_eq!(round, rec);
        assert_eq!(round.rows[1].cycles_last, None);
        assert_eq!(round.rows[1].soh_mean(), None);
    }

    #[test]
    fn persistence_round_trip_through_file() {
        let dir = std::env::temp_dir().join(format!("battery-viz-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("daily-record.json");
        let mut rec = DailyRecord::default();
        rec.fold(&sample(41.5, Some(28.0)), 10);
        rec.save(&path).expect("save");
        let loaded = DailyRecord::load(&path);
        assert_eq!(loaded.load_error, None);
        assert_eq!(loaded.record, rec);

        // Atomic save consumed the temp sibling.
        let mut files: Vec<_> = dir
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, vec![std::ffi::OsString::from("daily-record.json")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_record_preserved_and_starts_fresh() {
        let dir =
            std::env::temp_dir().join(format!("battery-viz-record-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("daily-record.json");
        std::fs::write(&path, "{\"version\":1,\"rows\":[[oops").unwrap();
        let loaded = DailyRecord::load(&path);
        assert!(loaded.record.rows.is_empty());
        let err = loaded.load_error.expect("load error surfaced");
        assert!(err.backup_path.exists(), "bytes preserved as .corrupt");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_fresh_without_error() {
        let missing = std::env::temp_dir()
            .join("battery-viz-record-missing")
            .join("nope")
            .join("daily-record.json");
        let loaded = DailyRecord::load(&missing);
        assert!(loaded.record.rows.is_empty());
        assert_eq!(loaded.load_error, None);
    }

    #[test]
    fn seed_fills_only_empty_days() {
        let mut rec = DailyRecord::default();
        rec.fold(&sample(50.0, None), 20);
        let seeds = vec![
            crate::caps::SeedCapacity {
                day: 19,
                soh_pct: 88.0,
            },
            crate::caps::SeedCapacity {
                day: 20,
                soh_pct: 99.0,
            }, // already sampled — ignored
            crate::caps::SeedCapacity {
                day: 21,
                soh_pct: 87.5,
            },
        ];
        assert_eq!(rec.seed(&seeds), 2);
        let days: Vec<u32> = rec.rows.iter().map(|r| r.day).collect();
        assert_eq!(days, vec![19, 20, 21]);
        // The sampled day kept its own reading, not the seed's.
        assert_eq!(rec.rows[1].soh_mean(), Some(90.0));
        assert_eq!(rec.rows[0].soh_mean(), Some(88.0));
    }

    #[test]
    fn seed_into_empty_record_and_nothing_from_empty_seeds() {
        let mut rec = DailyRecord::default();
        assert_eq!(rec.seed(&[]), 0);
        assert_eq!(
            rec.seed(&[crate::caps::SeedCapacity {
                day: 3,
                soh_pct: 89.0
            }]),
            1
        );
        assert_eq!(rec.rows[0].soh_mean(), Some(89.0));
        // Seeded rows carry no sample-derived fields.
        assert_eq!(rec.rows[0].n, 0);
        assert_eq!(rec.rows[0].high_hours(), 0.0);
        assert_eq!(rec.rows[0].warm_hours(), 0.0);
    }

    #[test]
    fn one_year_of_rows_stays_small() {
        let mut rec = DailyRecord::default();
        for day in 0..366 {
            rec.fold(&sample(50.0, Some(30.0)), day);
        }
        let json = rec.to_json();
        assert_eq!(rec.rows.len(), 366);
        assert!(
            json.len() < 64 * 1024,
            "a year of rows is a fixed small size: {} bytes",
            json.len()
        );
    }
}
