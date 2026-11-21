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
    /// Cap behaviour seen: on AC, held below full for `CAP_SUSTAIN_SECS`.
    Capped,
    /// Positive no-cap evidence: the pack reached full while on AC.
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
    /// no-cap signature is the pack reaching full on AC. An `Uncapped`
    /// machine can therefore re-enter `Capped` when a cap appears later.
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
            // Unplugged: any open stretch is broken, nothing carries over.
            self.plugged_since = None;
            self.stretch_max_soc = None;
            self.stretch_last_seen = None;
            return;
        }
        let since = *self.plugged_since.get_or_insert(now_s);
        self.stretch_last_seen = Some(self.stretch_last_seen.map_or(now_s, |l| l.max(now_s)));
        let soc = sample.state_of_charge;

        // Reaching full on AC is positive evidence there is no cap: a capped
        // pack is held below full by firmware. The `Full` charging state
        // alone is NOT enough — it can be derived from a firmware
        // "charge complete" signal that also fires on a capped pack — so a
        // near-full state of charge must corroborate it. A `Full` without a
        // corroborating reading is inconclusive and only folds into the
        // stretch like any other plugged sample.
        if soc.is_some_and(|s| s >= NEAR_FULL_SOC) {
            self.status = CapStatus::Uncapped;
            self.plugged_since = None;
            self.stretch_max_soc = None;
            self.stretch_last_seen = None;
            return;
        }

        if let Some(s) = soc {
            self.stretch_max_soc = Some(self.stretch_max_soc.map_or(s, |m| m.max(s)));
        }

        // The cap signature: a long hold on AC below full, not charging now.
        // An initial top-up after plug-in (79% → 80% on an 80% cap) does not
        // disqualify the stretch — firmware holding below full for hours
        // while the pack sits uncharged is what a cap looks like.
        let now_holding = state == ChargingState::OnAcNotCharging;
        let held_below_full = self.stretch_max_soc.is_some_and(|max| max < NEAR_FULL_SOC);
        let stretched = now_s.saturating_sub(since) >= CAP_SUSTAIN_SECS;
        if now_holding && held_below_full && stretched {
            self.status = CapStatus::Capped;
            self.plugged_since = None;
            self.stretch_max_soc = None;
            self.stretch_last_seen = None;
        }
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
    /// [`STRETCH_GAP_BREAK_SECS`] is discarded (the hold was not continuous),
    /// and a restored max at or above near-full is itself positive no-cap
    /// evidence — it is a corroborated state of charge reading.
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
            self.status = CapStatus::Uncapped;
            return;
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
        // No-cap evidence: full on AC.
        m.observe(&st(100.0, ChargingState::Full), 0);
        assert_eq!(m.status(), CapStatus::Uncapped);
        // Later the user sets a cap: the FR-1 signature resolves again.
        let mut now = 0u64;
        while now <= CAP_SUSTAIN_SECS {
            m.observe(&st(80.0, ChargingState::OnAcNotCharging), now);
            now += 60;
        }
        assert_eq!(m.status(), CapStatus::Capped);
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
