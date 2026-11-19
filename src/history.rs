//! Long-horizon history: downsampled 1-minute buckets with JSON persistence.
//!
//! The live dashboard keeps a 10-minute 1 Hz ring (see `app::History`); this
//! module is the long-term counterpart. Every incoming sample is folded into
//! a 1-minute bucket (mean per metric); buckets older than the retention
//! window are dropped and the store is hard-capped so the backing file cannot
//! grow unbounded.
//!
//! Persistence is a single JSON file in the platform data dir, rewritten
//! after each new bucket (write-behind on the UI thread, no background
//! thread). A missing or corrupt file starts fresh — telemetry history is
//! disposable, never worth crashing over.
//!
//! JSON is produced and parsed by hand because the only permitted new
//! dependency is `dirs`: the on-disk schema is a fixed versioned shape, so a
//! small focused parser is testable and dependency-free.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::collect::types::{state_of_health_pct, BatterySample};

/// Retention window: 7 days of 1-minute buckets.
pub const RETENTION_MINUTES: u64 = 7 * 24 * 60;
/// Hard cap on stored buckets: the retention window plus half a day of
/// margin, so clock jitter across restarts cannot evict fresh data.
pub const MAX_BUCKETS: usize = RETENTION_MINUTES as usize + 480;

/// On-disk schema version. A mismatch is treated as a corrupt file.
const SCHEMA_VERSION: f64 = 1.0;

/// One 1-minute bucket. Sums + counts per metric so means survive
/// partially-populated samples (a missing metric never poisons the mean).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinuteBucket {
    /// Unix time divided by 60 — the bucket key.
    pub minute: u64,
    /// Samples folded in, regardless of which metrics they carried.
    pub n: u32,
    pub soc_sum: f64,
    pub soc_n: u32,
    pub temp_sum: f64,
    pub temp_n: u32,
    pub watts_sum: f64,
    pub watts_n: u32,
    pub soh_sum: f64,
    pub soh_n: u32,
    /// Last cycle count seen in the bucket (cumulative counter, not a mean).
    pub cycles_last: Option<u32>,
}

impl MinuteBucket {
    fn new(minute: u64) -> Self {
        MinuteBucket {
            minute,
            n: 0,
            soc_sum: 0.0,
            soc_n: 0,
            temp_sum: 0.0,
            temp_n: 0,
            watts_sum: 0.0,
            watts_n: 0,
            soh_sum: 0.0,
            soh_n: 0,
            cycles_last: None,
        }
    }

    fn add(&mut self, s: &BatterySample) {
        self.n += 1;
        if let Some(v) = s.state_of_charge {
            self.soc_sum += v;
            self.soc_n += 1;
        }
        if let Some(v) = s.temperature_c {
            self.temp_sum += v;
            self.temp_n += 1;
        }
        if let Some(v) = s.power_w {
            self.watts_sum += v;
            self.watts_n += 1;
        }
        if let Some(v) = state_of_health_pct(s) {
            self.soh_sum += v;
            self.soh_n += 1;
        }
        if let Some(c) = s.cycle_count {
            self.cycles_last = Some(c);
        }
    }

    pub fn soc_mean(&self) -> Option<f64> {
        mean(self.soc_sum, self.soc_n)
    }
    pub fn temp_mean(&self) -> Option<f64> {
        mean(self.temp_sum, self.temp_n)
    }
    pub fn watts_mean(&self) -> Option<f64> {
        mean(self.watts_sum, self.watts_n)
    }
    pub fn soh_mean(&self) -> Option<f64> {
        mean(self.soh_sum, self.soh_n)
    }
}

fn mean(sum: f64, n: u32) -> Option<f64> {
    if n == 0 {
        None
    } else {
        Some(sum / n as f64)
    }
}

/// Downsampled long-term store, ordered by bucket minute.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistoryStore {
    pub buckets: Vec<MinuteBucket>,
}

impl HistoryStore {
    /// Fold one sample into its minute bucket. Returns true when a new
    /// bucket was created (the persistence checkpoint trigger).
    pub fn push(&mut self, s: &BatterySample, unix_secs: u64) -> bool {
        let minute = unix_secs / 60;
        let mut created = false;
        match self.buckets.last_mut() {
            // Common case: the live feed appends to the newest bucket.
            Some(b) if b.minute == minute => b.add(s),
            _ => {
                let mut b = MinuteBucket::new(minute);
                b.add(s);
                // A clock step backwards across restarts lands out of order;
                // keep the vector sorted by minute regardless.
                let idx = self
                    .buckets
                    .partition_point(|existing| existing.minute < minute);
                self.buckets.insert(idx, b);
                created = true;
            }
        }
        self.prune(minute);
        created
    }

    /// Drop buckets older than the retention window, then enforce the hard cap.
    fn prune(&mut self, now_minute: u64) {
        let cutoff = now_minute.saturating_sub(RETENTION_MINUTES);
        self.buckets.retain(|b| b.minute >= cutoff);
        if self.buckets.len() > MAX_BUCKETS {
            let extra = self.buckets.len() - MAX_BUCKETS;
            self.buckets.drain(..extra);
        }
    }

    /// Serialize to the on-disk JSON schema:
    /// `{"version":1,"buckets":[[minute,n,...sums/counts...,cycles],...]}`
    /// (cycles is NaN when unknown). Rust's f64 Display round-trips.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(160 * self.buckets.len() + 32);
        out.push_str("{\"version\":1,\"buckets\":[");
        for (i, b) in self.buckets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "[{},{},{},{},{},{},{},{},{},{},{}]",
                b.minute,
                b.n,
                b.soc_sum,
                b.soc_n,
                b.temp_sum,
                b.temp_n,
                b.watts_sum,
                b.watts_n,
                b.soh_sum,
                b.soh_n,
                b.cycles_last
                    .map(|c| c as f64)
                    .unwrap_or(f64::NAN)
            ));
        }
        out.push_str("]}");
        out
    }

    /// Parse the schema written by [`Self::to_json`]. Any deviation — wrong
    /// version, wrong arity, malformed numbers — is an error, which callers
    /// turn into a fresh store.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut p = Parser::new(text);
        p.skip_ws();
        p.expect(b'{')?;
        p.expect_key("version")?;
        let version = p.number()?;
        if version != SCHEMA_VERSION {
            return Err(format!("unsupported schema version {version}"));
        }
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("buckets")?;
        p.skip_ws();
        p.expect(b'[')?;
        let mut buckets = Vec::new();
        loop {
            p.skip_ws();
            if p.eat(b']') {
                break;
            }
            p.expect(b'[')?;
            let f: Vec<f64> = p.number_list()?;
            if f.len() != 11 {
                return Err(format!("bucket arity {} != 11", f.len()));
            }
            let cycle = f[10];
            if !cycle.is_nan() && (cycle < 0.0 || cycle.fract() != 0.0) {
                return Err(format!("invalid cycle count {cycle}"));
            }
            buckets.push(MinuteBucket {
                minute: f[0] as u64,
                n: f[1] as u32,
                soc_sum: f[2],
                soc_n: f[3] as u32,
                temp_sum: f[4],
                temp_n: f[5] as u32,
                watts_sum: f[6],
                watts_n: f[7] as u32,
                soh_sum: f[8],
                soh_n: f[9] as u32,
                cycles_last: if cycle.is_nan() {
                    None
                } else {
                    Some(cycle as u32)
                },
            });
            p.skip_ws();
            if !p.eat(b',') {
                p.expect(b']')?;
                break;
            }
        }
        p.skip_ws();
        p.expect(b'}')?;
        Ok(HistoryStore { buckets })
    }

    /// Load from disk. A missing or unreadable file yields an empty store.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text).unwrap_or_default(),
            Err(_) => HistoryStore::default(),
        }
    }

    /// Rewrite the store to disk after each new bucket.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create data dir: {e}"))?;
        }
        std::fs::write(path, self.to_json()).map_err(|e| format!("write history: {e}"))
    }

    /// Full retained window as CSV: charge%, SoH, temperature, power, cycles.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("minute,charge_pct,soh_pct,temp_c,power_w,cycles\n");
        for b in &self.buckets {
            let field = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_default();
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                b.minute,
                field(b.soc_mean()),
                field(b.soh_mean()),
                field(b.temp_mean()),
                field(b.watts_mean()),
                b.cycles_last
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ));
        }
        out
    }
}

/// Current unix time in seconds; pre-epoch clocks (never in practice) read 0.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Minimal tokenizer for the fixed schema above — not a general JSON parser.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            b: text.as_bytes(),
            i: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }

    /// `"<key>":` with exactly the keys this schema uses.
    fn expect_key(&mut self, key: &str) -> Result<(), String> {
        self.skip_ws();
        self.expect(b'"')?;
        for expected in key.bytes() {
            match self.b.get(self.i) {
                Some(&got) if got == expected => self.i += 1,
                _ => return Err(format!("expected key \"{key}\"")),
            }
        }
        self.expect(b'"')?;
        self.skip_ws();
        self.expect(b':')?;
        Ok(())
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip_ws();
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'N' | b'a' | b'n'))
        {
            self.i += 1;
        }
        let token = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad utf8")?;
        token
            .parse::<f64>()
            .map_err(|_| format!("bad number '{token}' at byte {start}"))
    }

    /// Comma-separated numbers up to the closing `]` (consumed).
    fn number_list(&mut self) -> Result<Vec<f64>, String> {
        let mut out = Vec::new();
        loop {
            out.push(self.number()?);
            if self.eat(b',') {
                continue;
            }
            self.expect(b']')?;
            return Ok(out);
        }
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

    // Bucket boundary helper: minute 0 of the synthetic timeline.
    const MIN0: u64 = 1_800 * 60; // any unix time; arithmetic below stays in-minute

    #[test]
    fn bucket_mean_over_samples_in_same_minute() {
        let mut store = HistoryStore::default();
        // Two samples 30 s apart land in the same bucket.
        assert!(store.push(&sample(40.0, Some(30.0)), MIN0));
        assert!(!store.push(&sample(60.0, Some(34.0)), MIN0 + 30));
        assert_eq!(store.buckets.len(), 1);
        let b = store.buckets[0];
        assert_eq!(b.n, 2);
        assert_eq!(b.soc_mean(), Some(50.0));
        assert_eq!(b.temp_mean(), Some(32.0));
        assert_eq!(b.soh_mean(), Some(90.0)); // 45_000 / 50_000
        assert_eq!(b.cycles_last, Some(120));
    }

    #[test]
    fn missing_metric_never_poisons_the_mean() {
        let mut store = HistoryStore::default();
        // First sample has no temperature; second does.
        assert!(store.push(&sample(50.0, None), MIN0));
        assert!(!store.push(&sample(50.0, Some(30.0)), MIN0 + 10));
        let b = store.buckets[0];
        assert_eq!(b.temp_mean(), Some(30.0));
        assert_eq!(b.soc_mean(), Some(50.0));
    }

    #[test]
    fn adjacent_minutes_form_separate_buckets() {
        let mut store = HistoryStore::default();
        assert!(store.push(&sample(50.0, None), MIN0));
        assert!(store.push(&sample(52.0, None), MIN0 + 60));
        assert_eq!(store.buckets.len(), 2);
        assert_eq!(store.buckets[0].minute, MIN0 / 60);
        assert_eq!(store.buckets[1].minute, MIN0 / 60 + 1);
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "battery-viz-tests-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("history.json")
    }

    #[test]
    fn persistence_round_trip_through_file() {
        let path = temp_file("round-trip");
        let mut store = HistoryStore::default();
        store.push(&sample(41.5, Some(28.0)), MIN0);
        store.push(&sample(43.5, Some(29.0)), MIN0 + 60);
        store.save(&path).expect("save");

        let loaded = HistoryStore::load(&path);
        assert_eq!(loaded, store);
        assert_eq!(loaded.buckets[0].soc_mean(), Some(41.5));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupt_or_missing_file_starts_fresh() {
        let corrupt = temp_file("corrupt");
        std::fs::write(&corrupt, "{\"version\":1,\"buckets\":[[not json").unwrap();
        assert!(HistoryStore::load(&corrupt).buckets.is_empty());

        let missing = temp_file("missing-dir").join("nope").join("history.json");
        assert!(HistoryStore::load(&missing).buckets.is_empty());
    }

    #[test]
    fn retention_window_drops_buckets_older_than_seven_days() {
        let mut store = HistoryStore::default();
        // One bucket per minute, half a day past the 7-day window: on a
        // healthy clock the retention prune binds before the hard cap.
        let minutes = RETENTION_MINUTES + 500;
        for m in 0..minutes {
            store.push(&sample(50.0, Some(30.0)), m * 60);
        }
        assert_eq!(store.buckets.len(), (RETENTION_MINUTES + 1) as usize);
        assert_eq!(store.buckets[0].minute, minutes - 1 - RETENTION_MINUTES);
        assert_eq!(store.buckets.last().unwrap().minute, minutes - 1);
    }

    #[test]
    fn hard_cap_trims_oldest_when_retention_cannot_bind() {
        // Defense-in-depth: a clock far in the past (cutoff 0) gives the
        // retention prune nothing to drop, so the hard cap must still bound
        // the store by evicting from the front.
        let mut store = HistoryStore::default();
        let total = MAX_BUCKETS as u64 + 500;
        store.buckets = (0..total).map(MinuteBucket::new).collect();
        store.push(&sample(50.0, Some(30.0)), 0);
        assert_eq!(store.buckets.len(), MAX_BUCKETS);
        assert_eq!(store.buckets[0].minute, 500);
    }

    #[test]
    fn json_survives_nan_cycles_and_special_values() {
        let mut store = HistoryStore::default();
        store.push(&BatterySample::default(), MIN0); // all metrics missing
        let round = HistoryStore::from_json(&store.to_json()).expect("parse");
        assert_eq!(round, store);
        assert_eq!(round.buckets[0].cycles_last, None);
    }

    #[test]
    fn csv_lists_all_columns_for_retained_window() {
        let mut store = HistoryStore::default();
        store.push(&sample(41.0, Some(30.5)), MIN0);
        let csv = store.to_csv();
        let mut lines = csv.lines();
        assert_eq!(
            lines.next(),
            Some("minute,charge_pct,soh_pct,temp_c,power_w,cycles")
        );
        let row = lines.next().unwrap();
        let cols: Vec<&str> = row.split(',').collect();
        assert_eq!(cols.len(), 6);
        assert_eq!(cols[0], (MIN0 / 60).to_string());
        assert_eq!(cols[1], "41.00");
        assert_eq!(cols[2], "90.00");
        assert_eq!(cols[3], "30.50");
        assert_eq!(cols[4], ""); // no power data in the fixture
        assert_eq!(cols[5], "120");
    }
}
