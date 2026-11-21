//! FR-1 (longevity roadmap): behavioural charge-cap detection and the
//! one-time setup prompt.
//!
//! No platform exposes the charge cap's state to apps, so the cap is
//! recognised from behaviour alone: the pack sits on AC, held below full,
//! and not charging, for hours. Everything platform-specific — what is
//! readable, where the setting lives — is asked of the FR-0 capability
//! adapter; when the adapter answers "unavailable", the feature degrades to
//! advice or hides itself and nothing is guessed.
//!
//! The app never changes the cap itself on any platform: FR-1 detects and
//! points the user at the exact per-machine steps, nothing more.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::caps::{Capabilities, Detectability, MetricId, SettingKind};
use crate::collect::types::{BatterySample, ChargingState};

/// How long the pack must sit on AC, held below full and not charging,
/// before the hold is read as an active cap (spec: "for hours"; far inside
/// the 24 h recognition budget).
pub const CAP_SUSTAIN_SECS: u64 = 3 * 60 * 60;

/// At or above this state of charge the pack counts as full for behavioural
/// purposes (reported 100%s can wobble slightly).
pub const NEAR_FULL_SOC: f64 = 99.5;

/// Re-audit N2: one not-charging sample is a transient (a momentary
/// charging pause can look identical); the hold must be seen in at least
/// this many qualifying samples before it reads as a cap.
pub const MIN_HOLD_SAMPLES: u32 = 3;

/// Review (wave 1b, Medium): a momentary not-charging pause inside a
/// genuine charge must not be able to compose the hold verdict. The
/// verdict therefore also requires no actively-charging (below full)
/// sample in at least this long before the window's end; a genuine hold
/// has none at all, so this adds no latency to it.
pub const ACTIVE_CHARGE_QUIET_SECS: u64 = 10 * 60;

/// Re-audit N2: positive no-cap evidence needs more than one near-full
/// reading too — the hold must span [`FULL_HOLD_REEVAL_SECS`] with at
/// least this many readings, so a first-launch calibration charge does
/// not read as uncapped.
pub const MIN_NEAR_FULL_SAMPLES: u32 = 2;

/// A restored stretch whose last observed sample is older than this is
/// dropped: the machine was unplugged (or asleep) for too long for the hold
/// to have been continuous.
pub const STRETCH_GAP_BREAK_SECS: u64 = 30 * 60;

/// The detection stretch is checkpointed to disk at most once per this many
/// seconds per stretch, bounding both write rate and restart staleness.
pub const STRETCH_PERSIST_SECS: u64 = 15 * 60;

/// FR-3: how long a capped machine may sit at/near full on AC before the
/// hold is read as the cap being gone rather than the OS recalibrating its
/// gauge. Same "hours" scale as [`CAP_SUSTAIN_SECS`] — a genuine no-cap
/// machine stays full for hours once plugged in, while a calibration
/// excursion is brief. Anything shorter is ignored.
pub const FULL_HOLD_REEVAL_SECS: u64 = 3 * 60 * 60;

/// Where the one-time prompt record lives, given the history path: a sibling
/// of `history.json` in `<data dir>/battery-viz/`.
pub fn prompt_path_for(history_path: &Path) -> PathBuf {
    history_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("cap-setup.json")
}

/// Detection outcome. While detection is running (`Undetermined`) the
/// verdicts below resolve once — but they are not terminal: FR-3 owns
/// re-evaluation, so a cap that appears or disappears later moves the
/// status back and forth (see [`CapMonitor::observe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CapStatus {
    /// The adapter cannot support detection on this machine — the feature
    /// hides itself: no status row, no prompt.
    #[default]
    Hidden,
    /// Detection is running; not enough evidence either way yet.
    Undetermined,
    /// Cap behaviour seen: on AC, held below full and not charging for
    /// `CAP_SUSTAIN_SECS` (measured from the first not-charging sample),
    /// corroborated across several samples (re-audit N2 — never one).
    Capped,
    /// Positive no-cap evidence: the pack held at/near full on AC long
    /// enough to be a pattern, not a brief calibration charge (N2).
    Uncapped,
}

impl CapStatus {
    /// Persisted token. Only settled verdicts serialize — `Undetermined`
    /// and `Hidden` never persist.
    fn token(self) -> Option<&'static str> {
        match self {
            CapStatus::Capped => Some("capped"),
            CapStatus::Uncapped => Some("uncapped"),
            _ => None,
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "capped" => Some(CapStatus::Capped),
            "uncapped" => Some(CapStatus::Uncapped),
            _ => None,
        }
    }
}

/// Folds samples into a cap verdict. No I/O; the clock is injected. The
/// running stretch survives a restart via [`CapMonitor::restore_stretch`] and
/// a resolved verdict via [`CapMonitor::restore_verdict`] — persistence is
/// the app's job ([`STRETCH_PERSIST_SECS`] cadence).
#[derive(Debug)]
pub struct CapMonitor {
    status: CapStatus,
    /// Unix second the current plugged-in stretch began, if any.
    plugged_since: Option<u64>,
    /// Highest state of charge seen during the current stretch.
    stretch_max_soc: Option<f64>,
    /// Unix second of the most recent plugged-in sample of the stretch —
    /// what a restored stretch is validated against.
    stretch_last_seen: Option<u64>,
    /// Re-audit N2: unix second the current not-charging-below-full hold
    /// began (the first qualifying sample of the stretch). Active charging
    /// below full does not reset it — only a near-full excursion or
    /// unplugging does — so firmware top-up pulses inside a hold and the
    /// initial top-up after plug-in stay tolerated.
    hold_since: Option<u64>,
    /// Re-audit N2: qualifying not-charging-below-full samples seen since
    /// `hold_since` — the corroboration that the hold is a pattern, not a
    /// transient. Only known-SOC (below near-full) samples count.
    hold_samples: u32,
    /// Review (wave 1b, Medium): the most recent actively-charging (below
    /// full) sample of the current stretch. The verdict requires this to
    /// be at least [`ACTIVE_CHARGE_QUIET_SECS`] old, so a genuine
    /// multi-hour charge with recurring momentary not-charging pauses
    /// cannot read as a hold while it is still charging.
    last_active_charge_at: Option<u64>,
    /// Re-audit N2: unix second the current at/near-full observation began,
    /// while detection is running. `None` when the pack is not at/near full.
    near_full_since: Option<u64>,
    /// Re-audit N2: near-full readings since `near_full_since`.
    near_full_samples: u32,
    /// FR-3: unix second the current at/near-full hold began, while the
    /// status is `Capped` — the calibration-excursion watch. `None` when
    /// the pack is not at/near full or the status is not `Capped`.
    full_since: Option<u64>,
}

impl CapMonitor {
    /// Detection runs only where the adapter says the cap state is
    /// detectable at all and the machine reports a charging state to read
    /// behaviour from; otherwise the feature hides itself.
    pub fn for_adapter(caps: &dyn Capabilities) -> Self {
        let detectable =
            caps.setting_detectability(SettingKind::ChargeCap) != Detectability::AdviceOnly;
        let reports_power_state = caps.readable_metrics().contains(&MetricId::ChargingState);
        let status = if detectable && reports_power_state {
            CapStatus::Undetermined
        } else {
            CapStatus::Hidden
        };
        CapMonitor {
            status,
            plugged_since: None,
            stretch_max_soc: None,
            stretch_last_seen: None,
            hold_since: None,
            hold_samples: 0,
            last_active_charge_at: None,
            near_full_since: None,
            near_full_samples: 0,
            full_since: None,
        }
    }

    pub fn status(&self) -> CapStatus {
        self.status
    }

    /// FR-3: resume a persisted verdict after a restart, so a capped machine
    /// does not re-fire charge alerts during the 3 h re-detection window.
    /// A `Hidden` adapter stays hidden — detection is impossible on that
    /// machine, and nothing may resurrect it.
    pub fn restore_verdict(&mut self, verdict: CapStatus) {
        if matches!(self.status, CapStatus::Undetermined)
            && matches!(verdict, CapStatus::Capped | CapStatus::Uncapped)
        {
            self.status = verdict;
        }
    }

    /// Fold one sample in. `now_s` is unix time (injected; stays pure). A
    /// missing charging state is an explicit "unavailable" — the sample is
    /// skipped, never guessed from other fields.
    pub fn observe(&mut self, sample: &BatterySample, now_s: u64) {
        match self.status {
            // Hidden has nothing to run.
            CapStatus::Hidden => {}
            // FR-3: a settled verdict keeps re-evaluating — a cap that
            // appears or disappears later moves the status back and forth.
            CapStatus::Capped => self.reevaluate_capped(sample, now_s),
            CapStatus::Undetermined | CapStatus::Uncapped => {
                self.detect(sample, now_s);
            }
        }
    }

    /// FR-3 re-evaluation while `Capped`. The OS occasionally charges a
    /// capped pack to full to recalibrate its gauge; a *brief* excursion to
    /// full is that calibration, not evidence the cap is gone, and must
    /// neither retire the verdict nor trigger the setup prompt. Only a
    /// sustained hold at/near full on AC ([`FULL_HOLD_REEVAL_SECS`]) reads
    /// as the cap having been removed — which re-enables the charge alerts.
    fn reevaluate_capped(&mut self, sample: &BatterySample, now_s: u64) {
        let Some(state) = sample.charging_state else {
            return; // Unavailable: the sample is skipped, never guessed.
        };
        let plugged = matches!(
            state,
            ChargingState::Charging | ChargingState::Full | ChargingState::OnAcNotCharging
        );
        let near_full = sample.state_of_charge.is_some_and(|s| s >= NEAR_FULL_SOC);
        if !(plugged && near_full) {
            // Not a full-charge hold: any open observation ends here. This
            // is the brief excursion ending — the cap verdict stands.
            self.full_since = None;
            return;
        }
        let since = self.full_since.get_or_insert(now_s);
        if now_s.saturating_sub(*since) >= FULL_HOLD_REEVAL_SECS {
            self.status = CapStatus::Uncapped;
            self.full_since = None;
        }
    }

    /// FR-1 detection, shared by `Undetermined` and (FR-3) `Uncapped`: the
    /// cap signature is a long hold on AC below full, not charging; the
    /// no-cap signature is a near-full pack on AC that *stays* there. An
    /// `Uncapped` machine can therefore re-enter `Capped` when a cap
    /// appears later.
    ///
    /// Re-audit N2: neither verdict may resolve from a single sample. The
    /// hold must sustain across [`CAP_SUSTAIN_SECS`] (measured from the
    /// first not-charging sample, not from plug-in) and appear in at least
    /// [`MIN_HOLD_SAMPLES`] samples; the near-full state must persist
    /// across [`FULL_HOLD_REEVAL_SECS`] with at least
    /// [`MIN_NEAR_FULL_SAMPLES`] readings — the same hours-scale FR-3
    /// uses to separate a brief calibration charge from a genuinely
    /// uncapped machine.
    fn detect(&mut self, sample: &BatterySample, now_s: u64) {
        if !matches!(self.status, CapStatus::Undetermined | CapStatus::Uncapped) {
            return; // Hidden has nothing to run; Capped re-evaluates above.
        }
        let Some(state) = sample.charging_state else {
            return;
        };
        let plugged = matches!(
            state,
            ChargingState::Charging | ChargingState::Full | ChargingState::OnAcNotCharging
        );
        if !plugged {
            // Unplugged: any open stretch and both evidence watches are
            // broken, nothing carries over.
            self.clear_stretch();
            return;
        }
        // Open (or keep) the plugged-in stretch; the hold clock below, not
        // time since plug-in, decides when the hold is long enough.
        self.plugged_since.get_or_insert(now_s);
        self.stretch_last_seen = Some(self.stretch_last_seen.map_or(now_s, |l| l.max(now_s)));
        let soc = sample.state_of_charge;

        // Reaching full on AC is positive evidence there is no cap: a capped
        // pack is held below full by firmware. The `Full` charging state
        // alone is NOT enough — it can be derived from a firmware
        // "charge complete" signal that also fires on a capped pack — so a
        // near-full state of charge must corroborate it. And one
        // corroborating reading is still not a verdict (N2): a first-launch
        // calibration charge also parks the pack at full briefly. The
        // near-full hold must persist (or recur across it) for
        // [`FULL_HOLD_REEVAL_SECS`] with at least [`MIN_NEAR_FULL_SAMPLES`]
        // readings before it reads as `Uncapped`.
        if soc.is_some_and(|s| s >= NEAR_FULL_SOC) {
            let since_full = *self.near_full_since.get_or_insert(now_s);
            self.near_full_samples = self.near_full_samples.saturating_add(1);
            if now_s.saturating_sub(since_full) >= FULL_HOLD_REEVAL_SECS
                && self.near_full_samples >= MIN_NEAR_FULL_SAMPLES
            {
                self.status = CapStatus::Uncapped;
                self.clear_stretch();
            }
            return;
        }

        // The pack left (or never reached) near-full: the no-cap watch
        // resets, and the stretch cannot survive the excursion either — a
        // full-charge episode inside the window means the below-full hold
        // must be re-corroborated from scratch. An initial top-up after
        // plug-in never reaches here (it is a `Charging` sample below
        // full), so it does not disqualify a stretch.
        if self.near_full_since.take().is_some() {
            self.near_full_samples = 0;
            self.clear_stretch();
            return;
        }

        if let Some(s) = soc {
            self.stretch_max_soc = Some(self.stretch_max_soc.map_or(s, |m| m.max(s)));
        }

        // The cap signature: on AC, below full, not charging now — held
        // long enough, and seen often enough, to be a pattern. The hold's
        // clock is the first not-charging sample of the stretch, so time
        // spent actively charging (the initial top-up, or a firmware
        // top-up pulse that keeps the pack below full) does not count
        // toward it; only a near-full excursion or unplugging resets it.
        if state == ChargingState::Charging {
            // Review (wave 1b, Medium): an active-charge reading is the
            // opposite of a hold. It does not reset `hold_since` (the
            // initial top-up must stay tolerated), but it pushes the
            // recency bound the verdict below requires — so a genuine
            // multi-hour charge with recurring momentary pauses cannot
            // compose the hold out of its pauses while still charging.
            self.last_active_charge_at = Some(now_s);
        }
        if state == ChargingState::OnAcNotCharging {
            let since_hold = *self.hold_since.get_or_insert(now_s);
            // Review (wave 1b, Low): an unknown-SOC not-charging sample is
            // a blind reading — it neither corroborates the hold nor
            // resets it.
            if soc.is_some() {
                self.hold_samples = self.hold_samples.saturating_add(1);
            }
            let held_below_full = self.stretch_max_soc.is_some_and(|max| max < NEAR_FULL_SOC);
            let sustained = now_s.saturating_sub(since_hold) >= CAP_SUSTAIN_SECS;
            let charge_quiet = self
                .last_active_charge_at
                .is_none_or(|t| now_s.saturating_sub(t) >= ACTIVE_CHARGE_QUIET_SECS);
            if sustained && held_below_full && charge_quiet && self.hold_samples >= MIN_HOLD_SAMPLES
            {
                self.status = CapStatus::Capped;
                self.clear_stretch();
            }
        }
    }

    /// End the stretch and both evidence watches — unplugged, a verdict
    /// resolved, or a near-full excursion in the middle of the window.
    fn clear_stretch(&mut self) {
        self.plugged_since = None;
        self.stretch_max_soc = None;
        self.stretch_last_seen = None;
        self.hold_since = None;
        self.hold_samples = 0;
        self.last_active_charge_at = None;
        self.near_full_since = None;
        self.near_full_samples = 0;
    }

    /// The open stretch, for persistence: `(started, last_seen, max SoC)`.
    /// `None` when no stretch is open (verdict resolved, feature hidden, or
    /// the machine is unplugged).
    pub fn stretch_state(&self) -> Option<(u64, u64, Option<f64>)> {
        match (self.plugged_since, self.stretch_last_seen) {
            (Some(started), Some(last_seen)) => Some((started, last_seen, self.stretch_max_soc)),
            _ => None,
        }
    }

    /// Resume a stretch restored from disk. Only meaningful while detection
    /// is still running; a stretch whose last observed sample is older than
    /// [`STRETCH_GAP_BREAK_SECS`] is discarded (the hold was not continuous).
    /// A restored max at or above near-full is one corroborated near-full
    /// reading — but one reading is not a verdict (re-audit N2: this used to
    /// flip straight to `Uncapped` and fire the one-time prompt), so it
    /// anchors the near-full watch instead and `detect` decides once the
    /// evidence persists.
    pub fn restore_stretch(&mut self, stretch: StretchState, now_s: u64) {
        if !matches!(self.status, CapStatus::Undetermined) {
            return;
        }
        let StretchState {
            started,
            last_seen,
            max_soc,
        } = stretch;
        if last_seen < started || now_s.saturating_sub(last_seen) > STRETCH_GAP_BREAK_SECS {
            return;
        }
        if max_soc.is_some_and(|m| m >= NEAR_FULL_SOC) {
            self.near_full_since = Some(last_seen);
            self.near_full_samples = 1;
        }
        self.plugged_since = Some(started);
        self.stretch_max_soc = max_soc;
        self.stretch_last_seen = Some(last_seen);
    }
}

/// The user's answer to the one-time prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapPromptAnswer {
    /// The user opened the setting (or says it is already set) — acted.
    Acted,
    /// The user dismissed the prompt.
    Dismissed,
}

impl CapPromptAnswer {
    fn token(self) -> &'static str {
        match self {
            CapPromptAnswer::Acted => "acted",
            CapPromptAnswer::Dismissed => "dismissed",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "acted" => Some(CapPromptAnswer::Acted),
            "dismissed" => Some(CapPromptAnswer::Dismissed),
            _ => None,
        }
    }
}

/// One-time prompt bookkeeping plus the persisted detection state: the
/// running stretch, and (FR-3) the last settled cap verdict so a restart
/// resumes retirement instead of re-detecting for 3 h. `answered`, once
/// set, is never reset — the prompt is shown at most once per machine
/// (spec FR-1).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PromptRecord {
    pub answered: Option<CapPromptAnswer>,
    pub stretch: Option<StretchState>,
    /// Last settled verdict (`Capped`/`Uncapped`), persisted for FR-3
    /// re-evaluation across restarts. `None` = still undetermined.
    pub verdict: Option<CapStatus>,
}

/// A plugged-in hold in progress: unix seconds it began and was last seen,
/// plus the highest state of charge observed in it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StretchState {
    pub started: u64,
    pub last_seen: u64,
    pub max_soc: Option<f64>,
}

/// Should the one-time setup prompt be on screen right now? Only positive
/// no-cap evidence (the pack reached full on AC) triggers it: until then the
/// machine might be capped, and asking would be guessing the other way.
pub fn should_prompt(status: CapStatus, record: PromptRecord) -> bool {
    matches!(status, CapStatus::Uncapped) && record.answered.is_none()
}

const RECORD_MAGIC: &str = "battery-viz-cap-setup-1";

/// Load the persisted record. A missing file means unanswered. A file that
/// exists but cannot be parsed is corruption, not absence: it is quarantined
/// to `<name>.corrupt` (the PR #7 discipline — never overwritten in place)
/// and a notice is returned so the app can surface it. A file that cannot be
/// read at all is surfaced without quarantine — its bytes cannot be
/// preserved — and reads as unanswered.
pub fn load_record(path: Option<&Path>) -> (PromptRecord, Option<String>) {
    let Some(path) = path else {
        return (PromptRecord::default(), None);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // No file: first run. Anything else (permissions, …) is surfaced.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (PromptRecord::default(), None);
        }
        Err(e) => {
            return (
                PromptRecord::default(),
                Some(format!(
                    "charge-cap state could not be read ({}); treated as unanswered: {e}",
                    path.display()
                )),
            )
        }
    };
    match parse_record(&text) {
        Ok(record) => (record, None),
        Err(message) => {
            let err = crate::history::quarantine(
                path,
                format!(
                    "charge-cap state file could not be read and was preserved as a .corrupt sibling; treated as unanswered: {message}"
                ),
            );
            (PromptRecord::default(), Some(err.message))
        }
    }
}

/// Parse the record text. Format (line-based, the history store's minimal
/// style; the `status` line is the FR-3 addition and is optional, so the
/// pre-FR-3 three-line format parses unchanged):
///
/// ```text
/// battery-viz-cap-setup-1
/// <answered|dismissed|empty>
/// stretch <started> <last_seen> <max_soc|->
/// status <capped|uncapped>
/// ```
fn parse_record(text: &str) -> Result<PromptRecord, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.first() != Some(&RECORD_MAGIC) {
        return Err(format!(
            "unrecognized format, wanted magic {RECORD_MAGIC:?}"
        ));
    }
    if lines.len() > 4 {
        return Err("too many lines".into());
    }
    let mut record = PromptRecord::default();
    for (i, raw) in lines.iter().enumerate().skip(1) {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[0] {
            "stretch" => {
                if parts.len() != 4 {
                    return Err(format!("malformed stretch line {line:?}"));
                }
                let started = parts[1]
                    .parse::<u64>()
                    .map_err(|e| format!("stretch start: {e}"))?;
                let last_seen = parts[2]
                    .parse::<u64>()
                    .map_err(|e| format!("stretch last-seen: {e}"))?;
                let max_soc = match parts[3] {
                    "-" => None,
                    value => Some(
                        value
                            .parse::<f64>()
                            .map_err(|e| format!("stretch max SoC: {e}"))?,
                    ),
                };
                record.stretch = Some(StretchState {
                    started,
                    last_seen,
                    max_soc,
                });
            }
            "status" => {
                if parts.len() != 2 {
                    return Err(format!("malformed status line {line:?}"));
                }
                record.verdict = Some(
                    CapStatus::from_token(parts[1])
                        .ok_or_else(|| format!("unknown status token {:?}", parts[1]))?,
                );
            }
            // The original format's unprefixed answer line (always the
            // first line after the magic).
            token if i == 1 => {
                record.answered = Some(
                    CapPromptAnswer::from_token(token)
                        .ok_or_else(|| format!("unknown answer token {token:?}"))?,
                );
            }
            _ => return Err(format!("unexpected line {line:?}")),
        }
    }
    Ok(record)
}

/// Persist the record atomically (temp sibling + fsync + rename, the history
/// store's pattern), so a crash mid-write cannot half-write it. A fully
/// empty record removes the file instead — there is nothing to keep.
pub fn save_record(path: &Path, record: PromptRecord) -> Result<(), String> {
    if record.answered.is_none() && record.stretch.is_none() && record.verdict.is_none() {
        // Nothing worth keeping: an empty record must not shadow a corrupt
        // quarantine with a fresh blank file.
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        }
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> Result<(), String> {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        let stretch = record.stretch.map_or_else(String::new, |st| {
            format!(
                "stretch {} {} {}\n",
                st.started,
                st.last_seen,
                st.max_soc
                    .map_or_else(|| "-".to_string(), |m| format!("{m}"))
            )
        });
        let status = record
            .verdict
            .and_then(CapStatus::token)
            .map_or(String::new(), |t| format!("status {t}\n"));
        let answer = record
            .answered
            .map_or(String::new(), |a| format!("{}\n", a.token()));
        write!(f, "{RECORD_MAGIC}\n{answer}{stretch}{status}")
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertEngine, AlertKind};
    use crate::caps::{
        CapOptions, Capabilities, Detectability, Maker, MetricId, Platform, SettingGuidance,
        SettingKind,
    };

    /// Minimal adapter stub: behavioral cap detection with a charging-state
    /// stream to read. FR-3 asks the FR-0 adapter, never the OS directly.
    struct StubCaps;

    impl Capabilities for StubCaps {
        fn platform(&self) -> Platform {
            Platform::Simulator
        }
        fn maker(&self) -> Maker {
            Maker::Unknown
        }
        fn readable_metrics(&self) -> &'static [MetricId] {
            &[MetricId::ChargingState, MetricId::StateOfCharge]
        }
        fn setting_detectability(&self, kind: SettingKind) -> Detectability {
            match kind {
                SettingKind::ChargeCap => Detectability::BehavioralOnly,
                _ => Detectability::AdviceOnly,
            }
        }
        fn cap_options(&self) -> CapOptions {
            CapOptions::none()
        }
        fn guidance(&self, _kind: SettingKind) -> Option<SettingGuidance> {
            None
        }
    }

    fn st(soc: f64, state: ChargingState) -> BatterySample {
        BatterySample {
            state_of_charge: Some(soc),
            charging_state: Some(state),
            ..Default::default()
        }
    }

    fn capped_machine() -> CapMonitor {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        assert_eq!(m.status(), CapStatus::Undetermined);
        // Plugged in on AC, parked at the 80 % cap, not charging: the FR-1
        // signature resolves to Capped at CAP_SUSTAIN_SECS.
        let mut now = 0u64;
        while now <= CAP_SUSTAIN_SECS {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
        m
    }

    // ---- FR-3: calibration tolerance while Capped --------------------------

    #[test]
    fn fr3_brief_calibration_full_charge_keeps_the_capped_verdict() {
        let mut m = capped_machine();
        // Day 4: the OS recalibrates its gauge — a brief charge to full,
        // ~45 minutes, then back to the cap hold.
        let cal_start = 4 * 24 * 3_600u64;
        let mut now = cal_start;
        while now < cal_start + 45 * 60 {
            let sample = if now < cal_start + 10 * 60 {
                st(100.0, ChargingState::Charging)
            } else {
                st(100.0, ChargingState::Full)
            };
            m.observe(&sample, now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Capped,
            "calibration must not flip the verdict"
        );
        // Back under the cap hold: still capped, and the watch is closed.
        for extra in [0u64, 60, 3_600] {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now + extra);
        }
        assert_eq!(m.status(), CapStatus::Capped);
        // And the one-time prompt must not fire off the calibration either.
        assert!(!should_prompt(m.status(), PromptRecord::default()));
    }

    #[test]
    fn fr3_excursion_timer_resets_when_the_pack_leaves_full() {
        let mut m = capped_machine();
        // Two 2.5 h near-full holds separated by a dip below near-full:
        // neither alone reaches FULL_HOLD_REEVAL_SECS, so the verdict stands.
        let mut now = 0u64;
        while now < FULL_HOLD_REEVAL_SECS - 60 {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
        now += 60;
        while now < 2 * FULL_HOLD_REEVAL_SECS {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
    }

    #[test]
    fn fr3_sustained_full_hold_reads_the_cap_as_gone() {
        let mut m = capped_machine();
        // The user removed the cap: the pack charges to full and stays there
        // for hours on AC. Just under the re-evaluation threshold: still
        // capped.
        let mut now = 0u64;
        while now < FULL_HOLD_REEVAL_SECS {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
        // One more minute past the threshold: the cap is gone.
        m.observe(&st(100.0, ChargingState::Full), now);
        assert_eq!(m.status(), CapStatus::Uncapped);
    }

    #[test]
    fn fr3_unplugged_breaks_the_calibration_watch_only() {
        let mut m = capped_machine();
        // 2 h into a near-full hold, the machine is unplugged for a while,
        // then plugged back in at full. The watch restarts; the verdict has
        // meanwhile not flipped.
        let mut now = 0u64;
        while now < 2 * 3_600 {
            m.observe(&st(100.0, ChargingState::Charging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
        m.observe(&st(60.0, ChargingState::Discharging), now);
        now += 3_600;
        m.observe(&st(100.0, ChargingState::Full), now);
        assert_eq!(m.status(), CapStatus::Capped);
    }

    // ---- FR-3: the cap can also appear later (Uncapped → Capped) ----------

    #[test]
    fn fr3_uncapped_machine_reenters_capped_when_a_cap_appears() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // No-cap evidence: the pack holds full on AC long enough to be a
        // pattern (N2 — one sample no longer flips it).
        let mut now = 0u64;
        while now < FULL_HOLD_REEVAL_SECS {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Undetermined,
            "one sample short of the sustained near-full hold"
        );
        m.observe(&st(100.0, ChargingState::Full), now);
        assert_eq!(m.status(), CapStatus::Uncapped);
        // Later the user sets a cap: the FR-1 signature resolves again.
        let mut now = 0u64;
        while now <= CAP_SUSTAIN_SECS {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
    }

    // ---- Re-audit N2: both verdicts need corroborated evidence -------------

    #[test]
    fn one_not_charging_sample_after_three_hours_plugged_in_is_not_a_cap() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // Plugged in and charging for 3 h (the N2 citation: time since
        // plug-in, not time held), then ONE not-charging sample — a
        // momentary charging pause, not a cap hold.
        let mut now = 0u64;
        while now < CAP_SUSTAIN_SECS {
            m.observe(&st(50.0, ChargingState::Charging), now);
            now += 60;
        }
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
        assert_eq!(
            m.status(),
            CapStatus::Undetermined,
            "a single transient sample must not read as capped"
        );
    }

    #[test]
    fn cap_detected_from_the_hold_clock_with_corroborating_samples() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // A macOS OBC-style hold: ~1 h of active charging up to the 80 %
        // cap, then the pack sits not-charging below full. The verdict
        // must key off the HOLD lasting CAP_SUSTAIN_SECS (with the
        // corroborating samples), not off time since plug-in.
        let mut now = 0u64;
        while now < 3_600 {
            m.observe(&st(79.0, ChargingState::Charging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Undetermined);
        while now <= 3_600 + CAP_SUSTAIN_SECS {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
    }

    #[test]
    fn initial_top_up_after_plug_in_does_not_disqualify_the_hold() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // 79% → 80% top-up pulse right after plug-in, then the hold.
        m.observe(&st(79.0, ChargingState::Charging), 0);
        m.observe(&st(80.0, ChargingState::Charging), 60);
        let mut now = 120u64;
        while now <= 120 + CAP_SUSTAIN_SECS {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Capped,
            "a below-full top-up must not reset the hold"
        );
    }

    #[test]
    fn momentary_pauses_in_a_genuine_charge_do_not_read_capped() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // Review (wave 1b, Medium): a genuine multi-hour charge whose OS
        // reports recurring momentary not-charging pauses. Each pause is
        // a qualifying sample, so the hold clock runs — but the pack is
        // actively charging, and the verdict must wait for it to stop.
        let mut now = 0u64;
        let stop_charging = 4 * 3_600;
        while now <= stop_charging {
            // A pause every 30 minutes; actively charging otherwise.
            let sample = if now.is_multiple_of(1_800) {
                st(60.0, ChargingState::OnAcNotCharging)
            } else {
                st(60.0, ChargingState::Charging)
            };
            m.observe(&sample, now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Undetermined,
            "momentary pauses inside a real charge must not compose the hold"
        );
        // The charge genuinely ends; the pack then sits held below full.
        // The verdict lands once the quiet bound passes — hours, not days.
        while now <= stop_charging + ACTIVE_CHARGE_QUIET_SECS {
            m.observe(&st(60.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Capped,
            "once charging stops, the held-below-full pattern is a cap"
        );
    }

    #[test]
    fn unknown_soc_not_charging_samples_do_not_corroborate_the_hold() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // Review (wave 1b, Low): one known below-full reading, then hours
        // of blind (unknown-SOC) not-charging samples. They neither
        // corroborate nor reset the hold — the verdict needs
        // MIN_HOLD_SAMPLES known readings.
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), 0);
        let blind = BatterySample {
            state_of_charge: None,
            charging_state: Some(ChargingState::OnAcNotCharging),
            ..Default::default()
        };
        let mut now = 60u64;
        while now <= CAP_SUSTAIN_SECS + 3_600 {
            m.observe(&blind, now);
            now += 60;
        }
        assert_eq!(
            m.status(),
            CapStatus::Undetermined,
            "blind samples must not fill the corroboration quota"
        );
        // Known readings resume: the hold is old and quiet, so the third
        // known sample completes the corroboration and the verdict lands.
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), now + 60);
        assert_eq!(m.status(), CapStatus::Capped);
    }

    #[test]
    fn first_launch_calibration_charge_does_not_read_as_uncapped() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // The N2 citation: a calibration charge parks the pack at full for
        // ~45 min at first launch — briefly. It must not flip to Uncapped
        // (and so must not fire the one-time prompt).
        let mut now = 0u64;
        while now < 45 * 60 {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
        assert_eq!(m.status(), CapStatus::Undetermined);
        assert!(!should_prompt(m.status(), PromptRecord::default()));
    }

    #[test]
    fn sustained_near_full_hold_on_ac_reads_as_uncapped() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // A genuinely uncapped machine: plugged in, the pack reaches full
        // and stays there for hours. Just under the threshold: not yet.
        let mut now = 0u64;
        while now < FULL_HOLD_REEVAL_SECS {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Undetermined);
        m.observe(&st(100.0, ChargingState::Full), now);
        assert_eq!(m.status(), CapStatus::Uncapped);
        assert!(should_prompt(m.status(), PromptRecord::default()));
    }

    #[test]
    fn restored_near_full_reading_anchors_the_watch_instead_of_flipping() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        // A stretch restored with a near-full max used to flip straight to
        // Uncapped (and could fire the prompt) off a single persisted
        // reading. It must only start the near-full watch.
        m.restore_stretch(
            StretchState {
                started: 1_000,
                last_seen: 2_000,
                max_soc: Some(100.0),
            },
            2_000,
        );
        assert_eq!(m.status(), CapStatus::Undetermined);
        // The pack drops back below full: the excursion was brief — the
        // stretch restarts and nothing is decided.
        m.observe(&st(80.0, ChargingState::OnAcNotCharging), 2_060);
        assert_eq!(m.status(), CapStatus::Undetermined);
        // But a near-full hold that persists past the restore still resolves.
        let mut m = CapMonitor::for_adapter(&StubCaps);
        m.restore_stretch(
            StretchState {
                started: 1_000,
                last_seen: 2_000,
                max_soc: Some(100.0),
            },
            2_000,
        );
        let mut now = 2_060u64;
        while now <= 2_000 + FULL_HOLD_REEVAL_SECS {
            m.observe(&st(100.0, ChargingState::Full), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Uncapped);
    }

    // ---- FR-3: verdict persistence across restarts -------------------------

    #[test]
    fn fr3_restore_verdict_resumes_without_redetecting_and_hidden_stays_hidden() {
        let mut m = CapMonitor::for_adapter(&StubCaps);
        m.restore_verdict(CapStatus::Capped);
        assert_eq!(m.status(), CapStatus::Capped);

        let hidden = CapMonitor::for_adapter(&AdviceOnlyCaps);
        assert_eq!(hidden.status(), CapStatus::Hidden);
        // A persisted verdict must not resurrect a hidden feature.
        let mut hidden = hidden;
        hidden.restore_verdict(CapStatus::Capped);
        assert_eq!(hidden.status(), CapStatus::Hidden);
    }

    struct AdviceOnlyCaps;

    impl Capabilities for AdviceOnlyCaps {
        fn platform(&self) -> Platform {
            Platform::Simulator
        }
        fn maker(&self) -> Maker {
            Maker::Unknown
        }
        fn readable_metrics(&self) -> &'static [MetricId] {
            &[]
        }
        fn setting_detectability(&self, _kind: SettingKind) -> Detectability {
            Detectability::AdviceOnly
        }
        fn cap_options(&self) -> CapOptions {
            CapOptions::none()
        }
        fn guidance(&self, _kind: SettingKind) -> Option<SettingGuidance> {
            None
        }
    }

    #[test]
    fn fr3_record_roundtrips_verdict_and_stretch() {
        let dir = std::env::temp_dir().join(format!(
            "battery-viz-capdetect-tests-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap-setup.json");
        let record = PromptRecord {
            answered: Some(CapPromptAnswer::Acted),
            stretch: Some(StretchState {
                started: 1_000,
                last_seen: 2_000,
                max_soc: Some(79.9),
            }),
            verdict: Some(CapStatus::Capped),
        };
        save_record(&path, record).expect("save");
        let (loaded, err) = load_record(Some(&path));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(loaded, record);

        // A verdict-only record (no answer, no stretch) survives too.
        let record = PromptRecord {
            verdict: Some(CapStatus::Uncapped),
            ..Default::default()
        };
        save_record(&path, record).expect("save");
        let (loaded, err) = load_record(Some(&path));
        assert!(err.is_none(), "{err:?}");
        assert_eq!(loaded, record);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fr3_pre_fr3_record_format_still_parses() {
        // The old three-line format (magic, answer, stretch) must keep
        // loading — an upgrade must not quarantine existing state.
        let old = format!("{RECORD_MAGIC}\nacted\nstretch 100 200 79.9\n");
        let record = parse_record(&old).expect("old format parses");
        assert_eq!(record.answered, Some(CapPromptAnswer::Acted));
        assert_eq!(record.stretch.map(|s| s.started), Some(100));
        assert_eq!(record.verdict, None);
        // And the old answer-only form.
        let record = parse_record(&format!("{RECORD_MAGIC}\ndismissed\n")).expect("parses");
        assert_eq!(record.answered, Some(CapPromptAnswer::Dismissed));
        assert_eq!(record.verdict, None);
    }

    // ---- FR-3 acceptance: the simulated 7-day capped run ------------------

    /// Spec acceptance: zero charge alerts on a capped machine over a
    /// 7-day run, including one calibration charge. One sample per minute,
    /// both clocks injected.
    #[test]
    fn fr3_seven_day_capped_run_has_zero_charge_alerts() {
        let mut monitor = CapMonitor::for_adapter(&StubCaps);
        let mut engine = AlertEngine::new();
        let minute = 60u64;
        let total = 7 * 24 * 60u64;
        let cal_day4_start = 4 * 24 * 60u64;
        let mut alerts: Vec<AlertKind> = Vec::new();
        for m in 0..total {
            let now = m * minute;
            // Parked at the 80 % cap on AC; day 4 brings one brief OS
            // calibration charge to full (~45 min).
            let sample = if (cal_day4_start..cal_day4_start + 45).contains(&m) {
                st(100.0, ChargingState::Charging)
            } else {
                st(80.0, ChargingState::OnAcNotCharging)
            };
            monitor.observe(&sample, now);
            engine.set_charge_coaching_enabled(!matches!(monitor.status(), CapStatus::Capped));
            let clock = crate::alert::ClockInput {
                now_s: now as f64,
                hour: 10,
                day: (m / (24 * 60)) as u32,
            };
            if let Some(a) = engine.tick(&sample, clock) {
                alerts.push(a.kind);
            }
        }
        assert_eq!(monitor.status(), CapStatus::Capped, "the cap held all week");
        assert!(alerts.is_empty(), "zero charge alerts: {alerts:?}");
    }

    /// The heat alert must be unchanged on the same capped machine: a 50 °C
    /// pack still fires (once — the hysteresis latch holds until it cools).
    #[test]
    fn fr3_heat_alert_unchanged_on_a_capped_machine() {
        let mut monitor = CapMonitor::for_adapter(&StubCaps);
        let mut engine = AlertEngine::new();
        let hot = BatterySample {
            state_of_charge: Some(80.0),
            charging_state: Some(ChargingState::OnAcNotCharging),
            temperature_c: Some(50.0),
            ..Default::default()
        };
        let mut high_temp_fires = 0;
        let mut charge_fires = 0;
        for m in 0..(24 * 60u64) {
            let now = m * 60;
            monitor.observe(&hot, now);
            engine.set_charge_coaching_enabled(!matches!(monitor.status(), CapStatus::Capped));
            let clock = crate::alert::ClockInput {
                now_s: now as f64,
                hour: 10,
                day: (m / (24 * 60)) as u32,
            };
            if let Some(a) = engine.tick(&hot, clock) {
                match a.kind {
                    AlertKind::HighTemp => high_temp_fires += 1,
                    _ => charge_fires += 1,
                }
            }
        }
        assert_eq!(
            high_temp_fires, 1,
            "heat alert fires exactly as before the gate"
        );
        assert_eq!(charge_fires, 0, "no charge alerts on a capped machine");
    }
}
