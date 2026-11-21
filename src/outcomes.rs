//! Alert outcome tracking (FR-6, longevity roadmap).
//!
//! Nothing about the alert engine (FR: alerts) records whether an alert
//! changed behaviour, so ignored alerts keep nagging forever. This module
//! closes that loop, pure and platform-free:
//!
//! - Every delivered alert fire is logged and starts an outcome watch.
//! - The alert counts as **acted on** when the condition it coaches on ends
//!   within [`OUTCOME_WINDOW_S`] of the fire (each `AlertKind` owns its own
//!   end-of-condition predicate, next to the thresholds that define the
//!   condition). A window that elapses with the condition still active
//!   counts as **ignored**.
//! - After [`DEFAULT_RETIRE_AFTER_IGNORES`] consecutive ignored fires
//!   (configurable per store, persisted), the alert type is **retired**:
//!   it stops firing in real time and its row becomes digest-channel data
//!   (the FR-9 weekly digest absorbs retired alerts). An acted-on outcome
//!   always resets the ignore streak.
//!
//! Persistence mirrors the daily record (FR-7): one versioned JSON file in
//! the platform data dir, written atomically, an unreadable file preserved
//! as `<name>.corrupt` with the failure surfaced — never silently discarded.
//! The store is bounded ([`MAX_FIRES`]); with at most five real-time fires a
//! day across all kinds, the cap is a decade-scale ceiling, not a working
//! limit.

use std::io::Write;
use std::path::Path;

use crate::alert::AlertKind;
use crate::collect::types::BatterySample;
use crate::history::LoadError;

/// How long after a fire the targeted condition may end for the alert to
/// count as acted on. Long enough to walk to a charger or unblock vents,
/// short enough that a coincidental end does not masquerade as action.
pub const OUTCOME_WINDOW_S: u64 = 30 * 60;

/// Default for the configurable ignore streak (`N` in the acceptance
/// criterion) that retires an alert type from real-time alerting.
pub const DEFAULT_RETIRE_AFTER_IGNORES: u32 = 3;

/// Trailing window over which the dashboard's acted-on rate is computed.
pub const RATE_WINDOW_S: u64 = 7 * 24 * 3_600;

/// Hard cap on logged fires, oldest evicted first. At the engine's caps
/// (3 + 1 + 1 fires/day) this is years of data; the rate only needs the
/// trailing [`RATE_WINDOW_S`].
pub const MAX_FIRES: usize = 2_000;

/// Number of alert kinds indexed side-by-side below.
const KINDS: usize = 3;

/// Outcome of one delivered alert fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The watch is open: the condition has not yet ended and the window
    /// has not elapsed.
    Pending,
    /// The targeted condition ended inside the outcome window.
    Acted,
    /// The window elapsed with the condition still active.
    Ignored,
}

impl Outcome {
    fn index(self) -> u8 {
        match self {
            Outcome::Pending => 0,
            Outcome::Acted => 1,
            Outcome::Ignored => 2,
        }
    }

    fn from_index(i: u8) -> Result<Self, String> {
        match i {
            0 => Ok(Outcome::Pending),
            1 => Ok(Outcome::Acted),
            2 => Ok(Outcome::Ignored),
            other => Err(format!("invalid outcome {other}")),
        }
    }
}

fn kind_index(kind: AlertKind) -> usize {
    match kind {
        AlertKind::HighTemp => 0,
        AlertKind::DwellCoach => 1,
        AlertKind::BandCoach => 2,
    }
}

fn kind_from_index(i: usize) -> Result<AlertKind, String> {
    match i {
        0 => Ok(AlertKind::HighTemp),
        1 => Ok(AlertKind::DwellCoach),
        2 => Ok(AlertKind::BandCoach),
        other => Err(format!("invalid alert kind {other}")),
    }
}

/// One logged alert fire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fire {
    pub kind: AlertKind,
    /// Local day counter of the fire (same counter as `clock::LocalNow.day`).
    pub day: u32,
    /// Unix seconds of the fire.
    pub fired_at: u64,
    /// False when the fire was suppressed because the kind is retired —
    /// logged so the digest can still see how often the condition recurs.
    pub delivered: bool,
    pub outcome: Outcome,
}

/// The still-open watch for one kind (at most one per kind: a new fire
/// supersedes any previous open watch).
#[derive(Debug, Clone, Copy, PartialEq)]
struct OpenFire {
    fired_at: u64,
}

/// Outcome log, ignore streaks, and retirement state per alert kind.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeStore {
    /// Configurable streak length that retires a kind (`N`).
    retire_after: u32,
    fires: Vec<Fire>,
    /// One open (Pending) watch per kind, if any.
    open: [Option<OpenFire>; KINDS],
    consecutive_ignores: [u32; KINDS],
    retired: [bool; KINDS],
}

impl Default for OutcomeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OutcomeStore {
    pub fn new() -> Self {
        Self::with_retire_after(DEFAULT_RETIRE_AFTER_IGNORES)
    }

    /// A store that retires a kind after `n` consecutive ignored fires.
    /// `n` is floored at 1 — "never retire" is not an outcome the spec has.
    pub fn with_retire_after(n: u32) -> Self {
        OutcomeStore {
            retire_after: n.max(1),
            fires: Vec::new(),
            open: [None, None, None],
            consecutive_ignores: [0; KINDS],
            retired: [false; KINDS],
        }
    }

    /// The configured streak length (`N`).
    pub fn retire_after(&self) -> u32 {
        self.retire_after
    }

    /// Is this alert kind retired from real-time alerting?
    pub fn is_retired(&self, kind: AlertKind) -> bool {
        self.retired[kind_index(kind)]
    }

    /// Current consecutive-ignored-fire streak for the kind (dashboard
    /// context for how close a kind is to retirement).
    pub fn ignore_streak(&self, kind: AlertKind) -> u32 {
        self.consecutive_ignores[kind_index(kind)]
    }

    /// Feed one sample: resolve open watches. Returns whether anything
    /// changed (a checkpoint hint). Missing telemetry never counts as the
    /// condition having ended — an unresolvable watch simply runs out its
    /// window.
    pub fn observe(&mut self, sample: &BatterySample, now: u64) -> bool {
        let mut changed = false;
        for i in 0..KINDS {
            let Some(open) = self.open[i] else {
                continue;
            };
            let kind = match kind_from_index(i) {
                Ok(kind) => kind,
                // Index is built from the same enum; unreachable.
                Err(_) => continue,
            };
            if kind.condition_ended(sample) {
                self.resolve(i, Outcome::Acted);
                changed = true;
            } else if now.saturating_sub(open.fired_at) >= OUTCOME_WINDOW_S {
                self.resolve(i, Outcome::Ignored);
                changed = true;
            }
        }
        changed
    }

    /// Record an engine fire of `kind` at `now`. Returns whether the alert
    /// should be delivered in real time — `false` once the kind is retired
    /// (the fire is still logged, undelivered, for the digest).
    pub fn on_fire(&mut self, kind: AlertKind, day: u32, now: u64) -> bool {
        if self.retired[kind_index(kind)] {
            self.push(Fire {
                kind,
                day,
                fired_at: now,
                delivered: false,
                outcome: Outcome::Ignored,
            });
            return false;
        }
        // A new fire supersedes any open watch for the kind. Unreachable in
        // practice for delivered fires (a re-fire requires the condition to
        // have ended, which `observe` would have resolved first), but the
        // invariant "at most one open watch per kind" must hold regardless.
        if self.open[kind_index(kind)].is_some() {
            self.resolve(kind_index(kind), Outcome::Ignored);
        }
        self.push(Fire {
            kind,
            day,
            fired_at: now,
            delivered: true,
            outcome: Outcome::Pending,
        });
        self.open[kind_index(kind)] = Some(OpenFire { fired_at: now });
        true
    }

    /// Close the open watch of kind index `i` with `outcome`, updating the
    /// streak and retirement.
    fn resolve(&mut self, i: usize, outcome: Outcome) {
        self.open[i] = None;
        let kind = match kind_from_index(i) {
            Ok(kind) => kind,
            // Index is built from the same enum; unreachable.
            Err(_) => return,
        };
        // At most one open watch per kind; the entry is the last Pending,
        // delivered fire of that kind in the log.
        if let Some(fire) = self
            .fires
            .iter_mut()
            .rev()
            .find(|f| f.kind == kind && f.delivered && f.outcome == Outcome::Pending)
        {
            fire.outcome = outcome;
        }
        match outcome {
            Outcome::Acted => self.consecutive_ignores[i] = 0,
            Outcome::Ignored => {
                self.consecutive_ignores[i] = self.consecutive_ignores[i].saturating_add(1);
                if self.consecutive_ignores[i] >= self.retire_after {
                    self.retired[i] = true;
                }
            }
            Outcome::Pending => {}
        }
    }

    /// Append and enforce the cap, oldest first.
    fn push(&mut self, fire: Fire) {
        self.fires.push(fire);
        if self.fires.len() > MAX_FIRES {
            let extra = self.fires.len() - MAX_FIRES;
            self.fires.drain(..extra);
        }
    }

    /// Acted-on rate for the kind over the trailing [`RATE_WINDOW_S`]:
    /// `(acted, delivered_total)`, or `None` when nothing was delivered in
    /// the window. Suppressed (retired) fires never count — they were not
    /// shown, so they cannot have acted on anything.
    pub fn acted_on_rate(&self, kind: AlertKind, now: u64) -> Option<(u32, u32)> {
        let cutoff = now.saturating_sub(RATE_WINDOW_S);
        let mut acted = 0u32;
        let mut total = 0u32;
        for f in &self.fires {
            if f.kind != kind || !f.delivered || f.fired_at < cutoff {
                continue;
            }
            total += 1;
            if f.outcome == Outcome::Acted {
                acted += 1;
            }
        }
        (total > 0).then_some((acted, total))
    }

    /// Serialize to the on-disk JSON schema:
    /// `{"version":1,"retire_after":3,"consecutive":[..],"retired":[..],
    ///   "fires":[[kind,day,fired_at,delivered,outcome],...]}`
    /// (booleans as 0/1). Rust's f64 Display round-trips.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(40 * self.fires.len() + 96);
        out.push_str(&format!(
            "{{\"version\":1,\"retire_after\":{},\"consecutive\":[{},{},{}],\"retired\":[{},{},{}],\"fires\":[",
            self.retire_after,
            self.consecutive_ignores[0],
            self.consecutive_ignores[1],
            self.consecutive_ignores[2],
            self.retired[0] as u8,
            self.retired[1] as u8,
            self.retired[2] as u8,
        ));
        for (i, f) in self.fires.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "[{},{},{},{},{}]",
                kind_index(f.kind),
                f.day,
                f.fired_at,
                f.delivered as u8,
                f.outcome.index(),
            ));
        }
        out.push_str("]}");
        out
    }

    /// Parse the schema written by [`Self::to_json`]. Any deviation — wrong
    /// version, wrong arity, malformed numbers — is an error, which callers
    /// turn into a fresh store.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut p = crate::history::Parser::new(text);
        p.skip_ws();
        p.expect(b'{')?;
        p.expect_key("version")?;
        let version = p.number()?;
        if version != 1.0 {
            return Err(format!("unsupported schema version {version}"));
        }
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("retire_after")?;
        let retire_after = p.number()?;
        if !(1.0..=1.0e6).contains(&retire_after) || retire_after.fract() != 0.0 {
            return Err(format!("invalid retire_after {retire_after}"));
        }
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("consecutive")?;
        p.skip_ws();
        p.expect(b'[')?;
        let consecutive_f = p.number_list()?;
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("retired")?;
        p.skip_ws();
        p.expect(b'[')?;
        let retired_f = p.number_list()?;
        if consecutive_f.len() != KINDS || retired_f.len() != KINDS {
            return Err("per-kind arrays must have 3 entries".to_string());
        }
        let mut consecutive = [0u32; KINDS];
        let mut retired = [false; KINDS];
        for (i, &v) in consecutive_f.iter().enumerate() {
            if v < 0.0 || v.fract() != 0.0 {
                return Err(format!("invalid ignore streak {v}"));
            }
            consecutive[i] = v as u32;
        }
        for (i, &v) in retired_f.iter().enumerate() {
            match v {
                0.0 => retired[i] = false,
                1.0 => retired[i] = true,
                other => return Err(format!("invalid retired flag {other}")),
            }
        }
        p.skip_ws();
        p.expect(b',')?;
        p.expect_key("fires")?;
        p.skip_ws();
        p.expect(b'[')?;
        let mut fires = Vec::new();
        let mut open = [None, None, None];
        loop {
            p.skip_ws();
            if p.eat(b']') {
                break;
            }
            p.expect(b'[')?;
            let f: Vec<f64> = p.number_list()?;
            if f.len() != 5 {
                return Err(format!("fire arity {} != 5", f.len()));
            }
            let kind = kind_from_index(f[0] as usize)?;
            let outcome = Outcome::from_index(f[4] as u8)?;
            let fire = Fire {
                kind,
                day: f[1] as u32,
                fired_at: f[2] as u64,
                delivered: f[3] == 1.0,
                outcome,
            };
            if f[3] != 0.0 && f[3] != 1.0 {
                return Err(format!("invalid delivered flag {}", f[3]));
            }
            // Rebuild the open-watch state: a still-Pending delivered fire
            // resumes its watch (its window keeps running across restarts).
            if fire.delivered && fire.outcome == Outcome::Pending {
                open[kind_index(kind)] = Some(OpenFire {
                    fired_at: fire.fired_at,
                });
            }
            fires.push(fire);
            p.skip_ws();
            if !p.eat(b',') {
                p.expect(b']')?;
                break;
            }
        }
        p.skip_ws();
        p.expect(b'}')?;
        Ok(OutcomeStore {
            retire_after: retire_after as u32,
            fires,
            open,
            consecutive_ignores: consecutive,
            retired,
        })
    }

    /// Load from disk. A missing file yields an empty store; a file that
    /// cannot be read or parsed starts fresh too, but is first moved aside
    /// to `<name>.corrupt` (kept, never deleted) and the failure reported —
    /// a silently discarded record looks identical to a never-recorded one.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::new());
            }
            Err(e) => return Err(crate::history::quarantine(path, format!("read: {e}"))),
        };
        match Self::from_json(&text) {
            Ok(store) => Ok(store),
            Err(e) => Err(crate::history::quarantine(path, format!("parse: {e}"))),
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
                .map_err(|e| format!("write outcomes: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync outcomes: {e}"))
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
    use crate::collect::types::ChargingState;

    fn sample(temp_c: Option<f64>, soc: Option<f64>, state: ChargingState) -> BatterySample {
        BatterySample {
            temperature_c: temp_c,
            state_of_charge: soc,
            charging_state: Some(state),
            ..Default::default()
        }
    }

    const DAY: u32 = 20_000;

    #[test]
    fn acted_within_window_resets_the_ignore_streak() {
        let mut s = OutcomeStore::with_retire_after(2);
        let fire_t = 1_000_000u64;
        assert!(s.on_fire(AlertKind::HighTemp, DAY, fire_t));
        // Condition ends (pack cools below the resume threshold) in-window.
        assert!(s.observe(
            &sample(Some(40.0), None, ChargingState::Discharging),
            fire_t + 60.0 as u64
        ));
        let (acted, total) = s
            .acted_on_rate(AlertKind::HighTemp, fire_t)
            .expect("delivered fire");
        assert_eq!((acted, total), (1, 1));
        assert_eq!(s.ignore_streak(AlertKind::HighTemp), 0);
        assert!(!s.is_retired(AlertKind::HighTemp));
    }

    #[test]
    fn window_expiry_counts_as_ignored_and_builds_the_streak() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        assert!(s.on_fire(AlertKind::BandCoach, DAY, t));
        // Still plugged in above the band through the whole window.
        let hot_charging = sample(None, Some(90.0), ChargingState::Charging);
        assert!(
            !s.observe(&hot_charging, t + OUTCOME_WINDOW_S - 1),
            "not yet due"
        );
        assert!(
            s.observe(&hot_charging, t + OUTCOME_WINDOW_S),
            "window elapsed"
        );
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 1);
        assert!(
            !s.is_retired(AlertKind::BandCoach),
            "one ignore is below the streak of 3"
        );
    }

    #[test]
    fn n_consecutive_ignores_retire_the_kind_and_stop_real_time_fires() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        for round in 0..3 {
            assert!(s.on_fire(AlertKind::BandCoach, DAY, t + round * 10_000));
            assert!(
                s.observe(&plugged, t + round * 10_000 + OUTCOME_WINDOW_S),
                "ignore {round} resolves at window expiry"
            );
        }
        assert!(
            s.is_retired(AlertKind::BandCoach),
            "3 consecutive ignores must retire"
        );
        // After retirement the alert no longer fires in real time...
        assert!(!s.on_fire(AlertKind::BandCoach, DAY, t + 40_000));
        // ...but the fire is still logged, undelivered, for the digest.
        let logged = s
            .acted_on_rate(AlertKind::BandCoach, t + 40_000)
            .map(|(_, total)| total)
            .unwrap_or(0);
        assert_eq!(logged, 3, "suppressed fire must not count as delivered");
    }

    #[test]
    fn acted_on_outcome_interrupts_the_streak() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        // Two ignores...
        for round in 0..2 {
            s.on_fire(AlertKind::BandCoach, DAY, t + round * 10_000);
            s.observe(&plugged, t + round * 10_000 + OUTCOME_WINDOW_S);
        }
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 2);
        // ...then an acted-on alert resets the streak: no retirement.
        assert!(s.on_fire(AlertKind::BandCoach, DAY, t + 20_000));
        assert!(s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            t + 20_100
        ));
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 0);
        assert!(!s.is_retired(AlertKind::BandCoach));
        // Streak restarts from zero: two more ignores still do not retire.
        for round in 3..5 {
            s.on_fire(AlertKind::BandCoach, DAY, t + round * 10_000);
            s.observe(&plugged, t + round * 10_000 + OUTCOME_WINDOW_S);
        }
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 2);
        assert!(!s.is_retired(AlertKind::BandCoach));
    }

    #[test]
    fn acted_on_rate_is_a_trailing_week_and_only_over_delivered_fires() {
        let mut s = OutcomeStore::with_retire_after(2);
        let t = 1_000_000u64;
        let week = RATE_WINDOW_S;
        // Outside the window: one delivered acted-on fire, a week and an
        // hour ago.
        s.on_fire(AlertKind::DwellCoach, DAY, t - 3_600);
        s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            t - 3_540,
        );
        // Inside the window: one acted on, one ignored, one suppressed.
        s.on_fire(AlertKind::DwellCoach, DAY, t + week - 100);
        s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            t + week - 90,
        );
        s.on_fire(AlertKind::DwellCoach, DAY, t + week - 50);
        s.observe(
            &sample(None, Some(98.0), ChargingState::Charging),
            t + week - 40 + OUTCOME_WINDOW_S,
        );
        // Retire DwellCoach so the next fire is suppressed.
        s.on_fire(AlertKind::DwellCoach, DAY, t + week - 30);
        s.observe(
            &sample(None, Some(98.0), ChargingState::Charging),
            t + week - 20 + OUTCOME_WINDOW_S,
        );
        assert!(s.is_retired(AlertKind::DwellCoach));
        s.on_fire(AlertKind::DwellCoach, DAY, t + week - 10);

        let (acted, total) = s
            .acted_on_rate(AlertKind::DwellCoach, t + week)
            .expect("fires inside the window");
        assert_eq!(
            (acted, total),
            (1, 3),
            "suppressed and out-of-window fires excluded"
        );
        // Sliding the window past every fire empties the rate.
        assert_eq!(
            s.acted_on_rate(AlertKind::DwellCoach, t + week + 200 + RATE_WINDOW_S),
            None
        );
        // Other kinds stay empty.
        assert_eq!(s.acted_on_rate(AlertKind::HighTemp, t + week), None);
    }

    #[test]
    fn json_round_trip_preserves_store_including_open_watches() {
        let mut s = OutcomeStore::with_retire_after(4);
        let t = 1_000_000u64;
        // HighTemp: acted on. DwellCoach: ignored twice (not yet retired at 4).
        s.on_fire(AlertKind::HighTemp, DAY, t);
        s.observe(
            &sample(Some(30.0), None, ChargingState::Discharging),
            t + 60,
        );
        let plugged = sample(None, Some(98.0), ChargingState::Charging);
        s.on_fire(AlertKind::DwellCoach, DAY, t + 100);
        s.observe(&plugged, t + 100 + OUTCOME_WINDOW_S);
        s.on_fire(AlertKind::DwellCoach, DAY, t + 200);
        s.observe(&plugged, t + 200 + OUTCOME_WINDOW_S);
        // BandCoach: fire left pending — the watch must survive the trip.
        s.on_fire(AlertKind::BandCoach, DAY, t + 300);

        let round = OutcomeStore::from_json(&s.to_json()).expect("parse");
        assert_eq!(round, s);
        // The pending watch resolves across the round trip: the machine
        // stays plugged in, the window expires → ignored (streak reaches 3).
        let mut round = round;
        assert!(round.observe(&plugged, t + 300 + OUTCOME_WINDOW_S));
        assert_eq!(round.ignore_streak(AlertKind::BandCoach), 1);
    }

    #[test]
    fn persistence_round_trip_through_file() {
        let dir = std::env::temp_dir().join(format!("battery-viz-outcomes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("alert-outcomes.json");
        let mut s = OutcomeStore::with_retire_after(5);
        let t = 1_000_000u64;
        s.on_fire(AlertKind::HighTemp, DAY, t);
        s.observe(
            &sample(Some(30.0), None, ChargingState::Discharging),
            t + 60,
        );
        s.save(&path).expect("save");

        let loaded = OutcomeStore::load(&path).expect("load");
        assert_eq!(loaded, s);
        assert_eq!(loaded.retire_after(), 5);

        // Atomic save consumed the temp sibling.
        let files: Vec<_> = dir
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_store_preserved_and_starts_fresh() {
        let dir = std::env::temp_dir().join(format!(
            "battery-viz-outcomes-corrupt-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("alert-outcomes.json");
        std::fs::write(&path, "{\"version\":1,\"retire_after\":oops").unwrap();
        let err = OutcomeStore::load(&path).expect_err("corrupt store must surface");
        assert!(err.backup_path.exists(), "bytes preserved as .corrupt");
        assert!(!path.exists(), "unreadable file moved aside");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_fresh_without_error() {
        let missing = std::env::temp_dir()
            .join("battery-viz-outcomes-missing")
            .join("nope")
            .join("alert-outcomes.json");
        let s = OutcomeStore::load(&missing).expect("missing file is a fresh start");
        assert_eq!(s, OutcomeStore::new());
    }

    #[test]
    fn bounded_store_evicts_oldest_fires() {
        let mut s = OutcomeStore::new();
        let t = 1_000_000u64;
        // HighTemp has no retire interference here: on_fire into a fresh
        // store repeatedly (the engine's caps do not apply to the store).
        for i in 0..(MAX_FIRES + 50) {
            s.on_fire(AlertKind::HighTemp, DAY, t + i as u64);
            // Resolve as acted-on so it stays deliverable.
            s.observe(
                &sample(Some(30.0), None, ChargingState::Discharging),
                t + i as u64,
            );
        }
        assert_eq!(s.fires.len(), MAX_FIRES);
        assert_eq!(s.fires[0].fired_at, t + 50, "oldest fires evicted first");
    }

    #[test]
    fn retire_streak_is_configurable_and_floored_at_one() {
        let mut s = OutcomeStore::with_retire_after(1);
        assert_eq!(s.retire_after(), 1);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        assert!(s.on_fire(AlertKind::BandCoach, DAY, t));
        s.observe(&plugged, t + OUTCOME_WINDOW_S);
        assert!(
            s.is_retired(AlertKind::BandCoach),
            "N=1 retires after one ignore"
        );

        assert_eq!(OutcomeStore::with_retire_after(7).retire_after(), 7);
        assert_eq!(
            OutcomeStore::with_retire_after(0).retire_after(),
            1,
            "floored at 1"
        );
        assert_eq!(
            OutcomeStore::new().retire_after(),
            DEFAULT_RETIRE_AFTER_IGNORES
        );
    }

    #[test]
    fn missing_telemetry_never_resolves_a_watch_as_acted_on() {
        let mut s = OutcomeStore::new();
        let t = 1_000_000u64;
        s.on_fire(AlertKind::HighTemp, DAY, t);
        // A sample with no temperature is unknown, not "cooled down".
        assert!(!s.observe(&sample(None, None, ChargingState::Discharging), t + 60));
        // The watch is still open: the window expiry later resolves Ignored,
        // not Acted.
        assert!(s.observe(
            &sample(None, None, ChargingState::Discharging),
            t + OUTCOME_WINDOW_S
        ));
        let (acted, _) = s
            .acted_on_rate(AlertKind::HighTemp, t)
            .expect("delivered fire");
        assert_eq!(acted, 0);
        assert_eq!(s.ignore_streak(AlertKind::HighTemp), 1);
    }
}
