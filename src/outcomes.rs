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
//! - Retirement is recoverable (re-audit N4). The heat alert never retires
//!   (heat is a physical condition, not a habit — its cooldown and daily
//!   limit cap it instead). A streak or retirement decays once a full
//!   [`STREAK_DECAY_AFTER_S`] passes with no new ignored fire of that kind
//!   (the condition stopped recurring; a retired kind's own suppressed
//!   fires refresh the anchor, so a still-recurring condition stays
//!   retired), and [`OutcomeStore::reactivate_all`] — wired to the app's
//!   un-pause toggle — clears every streak and retirement on user action.
//!   A condition that recurs after the decay re-fires and can earn a fresh
//!   retirement.
//! - The engine's per-kind daily caps and cooldowns are persisted here
//!   too (schema v2, re-audit N4a): the live engine keeps its own
//!   in-memory gates, and `on_fire` re-applies the persisted ones — so a
//!   restart or pause toggle (which resets the engine) can neither
//!   double-fire a notification nor resolve the previous open watch as a
//!   manufactured Ignored.
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

/// Re-audit N4b: how long an ignore streak (and a retirement built on one)
/// survives with no new ignored fire of that kind. A still-recurring
/// condition keeps generating fires — suppressed ones while retired — that
/// refresh the anchor, so the decay only lifts a retirement once the
/// condition has genuinely stopped for a week.
pub const STREAK_DECAY_AFTER_S: u64 = 7 * 24 * 3_600;

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
    /// Re-audit N4a: persisted engine gates — unix second each kind last
    /// fired (the cooldown re-applied across a restart) and the per-kind
    /// daily-cap counter (`day` is the same local-day counter as `Fire`).
    /// The live engine keeps its own in-memory gates; these are stamped on
    /// every delivered fire and re-checked in [`Self::on_fire`], so a
    /// restart or pause toggle can neither double-fire a notification nor
    /// resolve the previous open watch as a manufactured Ignored.
    last_fired_unix: [Option<u64>; KINDS],
    daily_day: [u32; KINDS],
    daily_count: [u32; KINDS],
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
            last_fired_unix: [None; KINDS],
            daily_day: [0; KINDS],
            daily_count: [0; KINDS],
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

    /// Re-audit N4b: clear every ignore streak and retirement — user
    /// action (re-enabling alerts in the UI) gives a silenced kind a
    /// fresh start. Persisted daily caps and cooldowns are NOT cleared:
    /// re-enabling must not buy a duplicate notification for a condition
    /// already coached today.
    pub fn reactivate_all(&mut self) {
        self.consecutive_ignores = [0; KINDS];
        self.retired = [false; KINDS];
    }

    /// Feed one sample: resolve open watches. Returns whether anything
    /// changed (a checkpoint hint). Missing telemetry never counts as the
    /// condition having ended — an unresolvable watch simply runs out its
    /// window.
    pub fn observe(&mut self, sample: &BatterySample, now: u64) -> bool {
        // Retirement decays here too: this runs on every sample, even
        // while alerts are paused, so a stale retirement lifts without
        // waiting for a fire.
        let decayed = self.decay_stale_streaks(now);
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
        changed || decayed
    }

    /// Record an engine fire of `kind` at `now`. Returns whether the alert
    /// should be delivered in real time. A `false` return has two distinct
    /// meanings: a retired kind logs the fire undelivered (for the digest),
    /// while a blocked persisted gate — a spent daily cap or a running
    /// cooldown (N4a) — drops the fire entirely, logging nothing, because
    /// the original fire is already in the log.
    pub fn on_fire(&mut self, kind: AlertKind, day: u32, now: u64) -> bool {
        let i = kind_index(kind);
        if self.retired[i] {
            self.push(Fire {
                kind,
                day,
                fired_at: now,
                delivered: false,
                outcome: Outcome::Ignored,
            });
            return false;
        }
        // Re-audit N4a: re-apply the persisted engine gates. The engine's
        // own daily cap and cooldown are in-memory; after a restart or a
        // pause toggle (which resets the engine) it fires again for a
        // condition whose daily cap was already spent or whose cooldown is
        // still running. The duplicate is dropped WITHOUT logging — the
        // original fire is already in the log — so the earlier fire is not
        // resolved as a manufactured Ignored and no notification repeats.
        if self.persisted_gate_blocks(i, day, now) {
            return false;
        }
        // A new fire supersedes any open watch for the kind. Unreachable in
        // practice for delivered fires (a re-fire requires the condition to
        // have ended, which `observe` would have resolved first), but the
        // invariant "at most one open watch per kind" must hold regardless.
        if self.open[i].is_some() {
            self.resolve(i, Outcome::Ignored);
        }
        self.push(Fire {
            kind,
            day,
            fired_at: now,
            delivered: true,
            outcome: Outcome::Pending,
        });
        self.open[i] = Some(OpenFire { fired_at: now });
        // Stamp the persisted gates for the next restart.
        self.last_fired_unix[i] = Some(now);
        if self.daily_day[i] == day {
            self.daily_count[i] = self.daily_count[i].saturating_add(1);
        } else {
            self.daily_day[i] = day;
            self.daily_count[i] = 1;
        }
        true
    }

    /// Re-audit N4a: would the persisted daily cap or cooldown block a
    /// fire of kind index `i` at local `day` / unix `now`? The same caps
    /// the live engine enforces, re-applied to the last state that
    /// survived a restart.
    fn persisted_gate_blocks(&self, i: usize, day: u32, now: u64) -> bool {
        let Ok(kind) = kind_from_index(i) else {
            return false; // Index is built from the same enum; unreachable.
        };
        let spent_today = if self.daily_day[i] == day {
            self.daily_count[i]
        } else {
            0
        };
        if spent_today >= kind.daily_cap() {
            return true;
        }
        let cooldown = kind.cooldown_secs();
        if cooldown == 0 {
            return false;
        }
        self.last_fired_unix[i].is_some_and(|t| now.saturating_sub(t) < cooldown)
    }

    /// Re-audit N4b: retirements and streaks decay. A kind's streak dies
    /// (and with it any retirement) once [`STREAK_DECAY_AFTER_S`] passes
    /// with no new ignored fire of that kind — the condition stopped
    /// recurring. A retired kind's own suppressed fires count as ignored
    /// fires and refresh the anchor, so a still-recurring condition stays
    /// retired until the user re-enables or the recurrence stops. Fires
    /// older than the log cap anchor at zero (decade-old history: decay).
    fn decay_stale_streaks(&mut self, now: u64) -> bool {
        let mut changed = false;
        for i in 0..KINDS {
            if self.consecutive_ignores[i] == 0 && !self.retired[i] {
                continue;
            }
            let anchor = self
                .fires
                .iter()
                .rev()
                .find(|f| kind_index(f.kind) == i && f.outcome == Outcome::Ignored)
                .map_or(0, |f| f.fired_at);
            if now.saturating_sub(anchor) >= STREAK_DECAY_AFTER_S {
                self.consecutive_ignores[i] = 0;
                self.retired[i] = false;
                changed = true;
            }
        }
        changed
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
                // Re-audit N4c: heat is a physical condition, not a habit —
                // the heat alert is capped by its cooldown and daily limit
                // and never counts toward (or reaches) retirement.
                if kind.retirable() {
                    self.consecutive_ignores[i] = self.consecutive_ignores[i].saturating_add(1);
                    if self.consecutive_ignores[i] >= self.retire_after {
                        self.retired[i] = true;
                    }
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

    /// FR-9: how many times `kind` fired (delivered or suppressed) on or
    /// after local day `since_day` — the digest's measure of how often a
    /// retired condition still recurs.
    pub fn fire_count_since_day(&self, kind: AlertKind, since_day: u32) -> u32 {
        self.fires
            .iter()
            .filter(|f| f.kind == kind && f.day >= since_day)
            .count() as u32
    }

    /// Serialize to the on-disk JSON schema:
    /// `{"version":2,"retire_after":3,"consecutive":[..],"retired":[..],
    ///   "engine":{"last_fired":[..],"daily_day":[..],"daily_count":[..]},
    ///   "fires":[[kind,day,fired_at,delivered,outcome],...]}`
    /// (booleans as 0/1, never-fired cooldowns as -1). Rust's f64 Display
    /// round-trips. Version 2 adds the persisted engine gates (re-audit
    /// N4a); version 1 files without them still load.
    pub fn to_json(&self) -> String {
        let fired = |t: Option<u64>| t.map_or(-1.0, |v| v as f64);
        let mut out = String::with_capacity(40 * self.fires.len() + 160);
        out.push_str(&format!(
            "{{\"version\":2,\"retire_after\":{},\"consecutive\":[{},{},{}],\"retired\":[{},{},{}],\"engine\":{{\"last_fired\":[{},{},{}],\"daily_day\":[{},{},{}],\"daily_count\":[{},{},{}]}},\"fires\":[",
            self.retire_after,
            self.consecutive_ignores[0],
            self.consecutive_ignores[1],
            self.consecutive_ignores[2],
            self.retired[0] as u8,
            self.retired[1] as u8,
            self.retired[2] as u8,
            fired(self.last_fired_unix[0]),
            fired(self.last_fired_unix[1]),
            fired(self.last_fired_unix[2]),
            self.daily_day[0],
            self.daily_day[1],
            self.daily_day[2],
            self.daily_count[0],
            self.daily_count[1],
            self.daily_count[2],
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
        // v2 adds the persisted engine gates (re-audit N4a); v1 files
        // without them load with the gates defaulted.
        if version != 1.0 && version != 2.0 {
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
        // Schema v2: the persisted engine gates. `-1` encodes "never fired".
        let mut last_fired_unix = [None; KINDS];
        let mut daily_day = [0u32; KINDS];
        let mut daily_count = [0u32; KINDS];
        if version >= 2.0 {
            p.skip_ws();
            p.expect(b',')?;
            p.expect_key("engine")?;
            p.skip_ws();
            p.expect(b'{')?;
            p.expect_key("last_fired")?;
            p.skip_ws();
            p.expect(b'[')?;
            let fired_f = p.number_list()?;
            p.skip_ws();
            p.expect(b',')?;
            p.expect_key("daily_day")?;
            p.skip_ws();
            p.expect(b'[')?;
            let day_f = p.number_list()?;
            p.skip_ws();
            p.expect(b',')?;
            p.expect_key("daily_count")?;
            p.skip_ws();
            p.expect(b'[')?;
            let count_f = p.number_list()?;
            p.skip_ws();
            p.expect(b'}')?;
            if fired_f.len() != KINDS || day_f.len() != KINDS || count_f.len() != KINDS {
                return Err("engine gate arrays must have 3 entries".to_string());
            }
            for i in 0..KINDS {
                if fired_f[i] < -1.0 || fired_f[i].fract() != 0.0 {
                    return Err(format!("invalid last_fired {}", fired_f[i]));
                }
                last_fired_unix[i] = (fired_f[i] >= 0.0).then_some(fired_f[i] as u64);
                for (slot, v, what) in [
                    (&mut daily_day[i], day_f[i], "daily_day"),
                    (&mut daily_count[i], count_f[i], "daily_count"),
                ] {
                    if v < 0.0 || v.fract() != 0.0 {
                        return Err(format!("invalid {what} {v}"));
                    }
                    *slot = v as u32;
                }
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
            last_fired_unix,
            daily_day,
            daily_count,
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
        // One fire per local day (the persisted daily cap is spent after
        // the first): fire, never unplug, let the outcome window expire.
        for round in 0..3u64 {
            let day = DAY + round as u32;
            assert!(s.on_fire(AlertKind::BandCoach, day, t + round * 86_400));
            assert!(
                s.observe(&plugged, t + round * 86_400 + OUTCOME_WINDOW_S),
                "ignore {round} resolves at window expiry"
            );
        }
        assert!(
            s.is_retired(AlertKind::BandCoach),
            "3 consecutive ignores must retire"
        );
        // After retirement the alert no longer fires in real time...
        assert!(!s.on_fire(AlertKind::BandCoach, DAY + 3, t + 4 * 86_400));
        // ...but the fire is still logged, undelivered, for the digest.
        let logged = s
            .acted_on_rate(AlertKind::BandCoach, t + 4 * 86_400)
            .map(|(_, total)| total)
            .unwrap_or(0);
        assert_eq!(logged, 3, "suppressed fire must not count as delivered");
    }

    #[test]
    fn acted_on_outcome_interrupts_the_streak() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        // Two ignores on consecutive days (one fire per day — the daily
        // cap)...
        for round in 0..2u64 {
            s.on_fire(AlertKind::BandCoach, DAY + round as u32, t + round * 86_400);
            s.observe(&plugged, t + round * 86_400 + OUTCOME_WINDOW_S);
        }
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 2);
        // ...then an acted-on alert resets the streak: no retirement.
        assert!(s.on_fire(AlertKind::BandCoach, DAY + 2, t + 2 * 86_400));
        assert!(s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            t + 2 * 86_400 + 100
        ));
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 0);
        assert!(!s.is_retired(AlertKind::BandCoach));
        // Streak restarts from zero: two more ignores (the next two days)
        // still do not retire.
        for round in 3..5u64 {
            s.on_fire(AlertKind::BandCoach, DAY + round as u32, t + round * 86_400);
            s.observe(&plugged, t + round * 86_400 + OUTCOME_WINDOW_S);
        }
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 2);
        assert!(!s.is_retired(AlertKind::BandCoach));
    }

    #[test]
    fn acted_on_rate_is_a_trailing_week_and_only_over_delivered_fires() {
        let mut s = OutcomeStore::with_retire_after(2);
        // The read point for the trailing week; fires are spread over
        // distinct local days (the persisted daily cap is one per day).
        let now = 2_000_000u64;
        let day_len = 86_400u64;
        // Outside the window: one delivered acted-on fire, a week and an
        // hour ago.
        s.on_fire(AlertKind::DwellCoach, DAY, now - 8 * day_len);
        s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            now - 8 * day_len + 60,
        );
        // Inside the window: one acted on, one ignored, one ignored that
        // retires (N=2), then a suppressed fire the next day.
        s.on_fire(AlertKind::DwellCoach, DAY + 1, now - 4 * day_len);
        s.observe(
            &sample(None, Some(50.0), ChargingState::Discharging),
            now - 4 * day_len + 60,
        );
        s.on_fire(AlertKind::DwellCoach, DAY + 2, now - 3 * day_len);
        s.observe(
            &sample(None, Some(98.0), ChargingState::Charging),
            now - 3 * day_len + OUTCOME_WINDOW_S,
        );
        s.on_fire(AlertKind::DwellCoach, DAY + 3, now - 2 * day_len);
        s.observe(
            &sample(None, Some(98.0), ChargingState::Charging),
            now - 2 * day_len + OUTCOME_WINDOW_S,
        );
        assert!(s.is_retired(AlertKind::DwellCoach));
        s.on_fire(AlertKind::DwellCoach, DAY + 4, now - day_len);

        let (acted, total) = s
            .acted_on_rate(AlertKind::DwellCoach, now)
            .expect("fires inside the window");
        assert_eq!(
            (acted, total),
            (1, 3),
            "suppressed and out-of-window fires excluded"
        );
        // Sliding the window past every fire empties the rate.
        assert_eq!(
            s.acted_on_rate(AlertKind::DwellCoach, now + 200 + RATE_WINDOW_S),
            None
        );
        // Other kinds stay empty.
        assert_eq!(s.acted_on_rate(AlertKind::HighTemp, now), None);
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
        // Second ignore the next day (the persisted daily cap is one per
        // day).
        s.on_fire(AlertKind::DwellCoach, DAY + 1, t + 100 + 86_400);
        s.observe(&plugged, t + 100 + 86_400 + OUTCOME_WINDOW_S);
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
        // store repeatedly (the engine's caps do not apply to the store),
        // one per local day so the persisted daily cap and cooldown never
        // gate a fire.
        for i in 0..(MAX_FIRES + 50) {
            s.on_fire(AlertKind::HighTemp, DAY + i as u32, t + i as u64 * 86_400);
            // Resolve as acted-on so it stays deliverable.
            s.observe(
                &sample(Some(30.0), None, ChargingState::Discharging),
                t + i as u64 * 86_400,
            );
        }
        assert_eq!(s.fires.len(), MAX_FIRES);
        assert_eq!(
            s.fires[0].fired_at,
            t + 50 * 86_400,
            "oldest fires evicted first"
        );
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
        // Re-audit N4c: heat never accumulates an ignore streak — it is
        // capped by its cooldown and daily limit instead.
        assert_eq!(s.ignore_streak(AlertKind::HighTemp), 0);
        assert!(!s.is_retired(AlertKind::HighTemp));
    }

    // ---- Re-audit N4: recoverable retirement, persisted gates --------------

    #[test]
    fn heat_alert_never_retires_no_matter_how_often_ignored() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        let hot = sample(None, Some(90.0), ChargingState::Charging);
        // Five long hot sessions on consecutive days, each ignored.
        for round in 0..5u64 {
            assert!(s.on_fire(AlertKind::HighTemp, DAY + round as u32, t + round * 86_400));
            s.observe(&hot, t + round * 86_400 + OUTCOME_WINDOW_S);
        }
        assert!(
            !s.is_retired(AlertKind::HighTemp),
            "heat is a physical condition, not a habit — no retirement"
        );
        assert_eq!(s.ignore_streak(AlertKind::HighTemp), 0);
        // And it still delivers: cooldown (15 min) and the day have passed.
        assert!(s.on_fire(AlertKind::HighTemp, DAY + 5, t + 5 * 86_400));
    }

    #[test]
    fn retirement_decays_after_a_quiet_week_and_the_kind_refires() {
        let mut s = OutcomeStore::with_retire_after(2);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        for round in 0..2u64 {
            s.on_fire(AlertKind::BandCoach, DAY + round as u32, t + round * 86_400);
            s.observe(&plugged, t + round * 86_400 + OUTCOME_WINDOW_S);
        }
        assert!(s.is_retired(AlertKind::BandCoach));
        // The condition stops recurring: a full quiet week later the
        // decay lifts the retirement on the next observe...
        let late = t + 2 * 86_400 + STREAK_DECAY_AFTER_S;
        assert!(s.observe(&sample(None, Some(50.0), ChargingState::Discharging), late));
        assert!(!s.is_retired(AlertKind::BandCoach), "decayed");
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 0);
        // ...and a recurrence fires (and delivers) again.
        let late_day = DAY + 2 + (STREAK_DECAY_AFTER_S / 86_400) as u32;
        assert!(s.on_fire(AlertKind::BandCoach, late_day, late + 60));
    }

    #[test]
    fn a_retired_kind_stays_retired_while_its_condition_keeps_recurring() {
        let mut s = OutcomeStore::with_retire_after(1);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        s.on_fire(AlertKind::BandCoach, DAY, t);
        s.observe(&plugged, t + OUTCOME_WINDOW_S);
        assert!(s.is_retired(AlertKind::BandCoach));
        // The condition recurs daily: suppressed fires are logged (they
        // refresh the decay anchor), so the retirement holds.
        for d in 1..=8u32 {
            assert!(!s.on_fire(AlertKind::BandCoach, DAY + d, t + d as u64 * 86_400));
            s.observe(&plugged, t + d as u64 * 86_400 + OUTCOME_WINDOW_S);
            assert!(
                s.is_retired(AlertKind::BandCoach),
                "recurrence keeps the retirement"
            );
        }
        // A full quiet week after the last suppressed fire: decay lifts it.
        let late = t + 8 * 86_400 + STREAK_DECAY_AFTER_S;
        assert!(s.observe(&sample(None, Some(50.0), ChargingState::Discharging), late));
        assert!(!s.is_retired(AlertKind::BandCoach));
    }

    #[test]
    fn reactivation_clears_every_streak_and_retirement() {
        let mut s = OutcomeStore::with_retire_after(1);
        let t = 1_000_000u64;
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        s.on_fire(AlertKind::BandCoach, DAY, t);
        s.observe(&plugged, t + OUTCOME_WINDOW_S);
        assert!(s.is_retired(AlertKind::BandCoach));
        s.reactivate_all();
        assert!(!s.is_retired(AlertKind::BandCoach));
        assert_eq!(s.ignore_streak(AlertKind::BandCoach), 0);
        // A fresh streak can build and retire again — a re-fire after the
        // condition genuinely recurs is possible.
        let day2 = DAY + 1;
        assert!(s.on_fire(AlertKind::BandCoach, day2, t + 86_400));
        s.observe(&plugged, t + 86_400 + OUTCOME_WINDOW_S);
        assert!(s.is_retired(AlertKind::BandCoach));
    }

    #[test]
    fn persisted_daily_cap_blocks_a_restart_refire_without_manufacturing_an_ignore() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        // Day 1: the band coach fires and is delivered.
        assert!(s.on_fire(AlertKind::BandCoach, DAY, t));
        // "Restart": reload from the serialized store — the engine's
        // in-memory daily counter is gone.
        let mut s2 = OutcomeStore::from_json(&s.to_json()).expect("parse");
        // Same day, the fresh engine fires again: the persisted daily cap
        // blocks it — no duplicate notification, no manufactured ignore.
        assert!(!s2.on_fire(AlertKind::BandCoach, DAY, t + 60));
        assert_eq!(
            s2.acted_on_rate(AlertKind::BandCoach, t + 60)
                .map(|(_, total)| total),
            Some(1),
            "the duplicate is not logged at all"
        );
        // Next day the cap resets and the fire goes out.
        assert!(s2.on_fire(AlertKind::BandCoach, DAY + 1, t + 86_400));
    }

    #[test]
    fn persisted_cooldown_survives_a_restart() {
        let mut s = OutcomeStore::with_retire_after(3);
        let t = 1_000_000u64;
        assert!(s.on_fire(AlertKind::HighTemp, DAY, t));
        let mut s2 = OutcomeStore::from_json(&s.to_json()).expect("parse");
        // Inside the 15-minute cooldown: blocked without logging.
        assert!(!s2.on_fire(AlertKind::HighTemp, DAY, t + 600));
        // Past it (day cap of 3 not spent): allowed.
        assert!(s2.on_fire(AlertKind::HighTemp, DAY, t + 900));
    }

    #[test]
    fn v1_store_without_engine_gates_loads_with_defaults() {
        // A pre-N4 file: no "engine" section, Band retired, Dwell streak 1.
        let text = format!(
            "{{\"version\":1,\"retire_after\":3,\"consecutive\":[0,1,0],\"retired\":[0,0,1],\"fires\":[[2,{DAY},1000000,0,2]]}}"
        );
        let mut s = OutcomeStore::from_json(&text).expect("v1 loads");
        assert!(s.is_retired(AlertKind::BandCoach));
        assert_eq!(s.ignore_streak(AlertKind::DwellCoach), 1);
        // Gates default to "never fired": a live kind's fire goes out.
        assert!(s.on_fire(AlertKind::HighTemp, DAY, 1_000_000));
    }
}
