//! Long-term daily health record (FR-7, longevity roadmap).
//!
//! One summary row per local day, kept indefinitely: state-of-health
//! capacity, cycle count, hours at high charge, warm hours. Where the
//! 1-minute history (`history.rs`) expires after 7 days, this store is what
//! can show capacity fade a year later.
//!
//! Bounded by construction: a row is a fixed 7-number JSON array
//! (~55 bytes), so a year of daily rows is roughly 20 KB; a generous hard
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

use crate::collect::types::{state_of_health_pct, BatterySample};
use crate::history::LoadError;

/// A sample counts as "at high charge" at this state of charge or above —
/// the top of the longevity band the scorecard and coaching use (80%).
/// Re-exported, not re-declared: the daily record must track the H4 dwell
/// convention by construction, never by a copied constant.
pub use crate::dwell::DWELL_SOC_PCT as HIGH_CHARGE_SOC_PCT;
/// A sample counts as a warm hour at this pack temperature or above
/// (degrees Celsius) — the same "warm" floor FR-4 counts weekly.
pub use crate::warm::WARM_THRESHOLD_C as WARM_TEMP_C;

/// Hard cap on stored days: ten years plus margin. Daily rows are kept
/// "indefinitely" in practice — the cap exists so the file stays bounded
/// even under pathological clocks, evicting oldest first.
pub const MAX_DAYS: usize = 10 * 366;

/// On-disk schema version. A mismatch is treated as a corrupt file.
const SCHEMA_VERSION: f64 = 1.0;

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
    /// Samples at or above `HIGH_CHARGE_SOC_PCT`.
    pub high_n: u32,
    /// Samples at or above `WARM_TEMP_C`.
    pub warm_n: u32,
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

    /// Hours at high charge that day. The collector samples at 1 Hz, so a
    /// count of samples is a count of seconds. Consumed from FR-4 (warm
    /// hours) and FR-9 (digest) onward; stored now so the record is
    /// complete from day one.
    #[allow(dead_code)]
    pub fn high_hours(&self) -> f64 {
        self.high_n as f64 / 3600.0
    }

    /// Warm hours that day, same 1 Hz conversion. Consumed from FR-4
    /// (warm hours) and FR-9 (digest) onward.
    #[allow(dead_code)]
    pub fn warm_hours(&self) -> f64 {
        self.warm_n as f64 / 3600.0
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
    pub fn fold(&mut self, s: &BatterySample, day: u32) -> bool {
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
        if let Some(v) = state_of_health_pct(s) {
            row.soh_sum += v;
            row.soh_n += 1;
        }
        if let Some(c) = s.cycle_count {
            row.cycles_last = Some(c);
        }
        if s.state_of_charge
            .is_some_and(|soc| soc >= HIGH_CHARGE_SOC_PCT)
        {
            row.high_n += 1;
        }
        if s.temperature_c.is_some_and(|t| t >= WARM_TEMP_C) {
            row.warm_n += 1;
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

    /// Serialize to the on-disk JSON schema:
    /// `{"version":1,"rows":[[day,n,soh_sum,soh_n,cycles,high_n,warm_n],...]}`
    /// (cycles is NaN when unknown). Rust's f64 Display round-trips.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(80 * self.rows.len() + 32);
        out.push_str("{\"version\":1,\"rows\":[");
        for (i, r) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "[{},{},{},{},{},{},{}]",
                r.day,
                r.n,
                r.soh_sum,
                r.soh_n,
                r.cycles_last.map(|c| c as f64).unwrap_or(f64::NAN),
                r.high_n,
                r.warm_n,
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
        if version != SCHEMA_VERSION {
            return Err(format!("unsupported schema version {version}"));
        }
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
            if f.len() != 7 {
                return Err(format!("row arity {} != 7", f.len()));
            }
            let cycles = f[4];
            if !cycles.is_nan() && (cycles < 0.0 || cycles.fract() != 0.0) {
                return Err(format!("invalid cycle count {cycles}"));
            }
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
                high_n: f[5] as u32,
                warm_n: f[6] as u32,
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
    fn hours_at_high_charge_and_warm_hours_count_seconds() {
        let mut rec = DailyRecord::default();
        // Two high-charge samples (1 s each at 1 Hz), one warm, one neither.
        rec.fold(&sample(85.0, Some(31.0)), 7);
        rec.fold(&sample(95.0, Some(45.0)), 7);
        rec.fold(&sample(50.0, Some(10.0)), 7);
        let r = rec.rows[0];
        assert_eq!(r.high_n, 2);
        assert_eq!(r.warm_n, 2);
        assert!((r.high_hours() - 2.0 / 3600.0).abs() < 1e-12);
        assert!((r.warm_hours() - 2.0 / 3600.0).abs() < 1e-12);
        // Exactly on the thresholds counts as in.
        let mut rec = DailyRecord::default();
        rec.fold(&sample(HIGH_CHARGE_SOC_PCT, Some(WARM_TEMP_C)), 7);
        assert_eq!(rec.rows[0].high_n, 1);
        assert_eq!(rec.rows[0].warm_n, 1);
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
