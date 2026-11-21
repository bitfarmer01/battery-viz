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

/// Where the one-time prompt record lives, given the history path: a sibling
/// of `history.json` in `<data dir>/battery-viz/`.
pub fn prompt_path_for(history_path: &Path) -> PathBuf {
    history_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("cap-setup.json")
}

/// Detection outcome. Both verdicts are terminal for this feature: once
/// evidence exists either way, further samples cannot un-see it (FR-3 owns
/// re-evaluation if a cap later disappears).
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

/// Folds samples into a cap verdict. No I/O; the clock is injected. The
/// running stretch survives a restart via [`CapMonitor::restore_stretch`] —
/// persistence is the app's job ([`STRETCH_PERSIST_SECS`] cadence).
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
        }
    }

    pub fn status(&self) -> CapStatus {
        self.status
    }

    /// Fold one sample in. `now_s` is unix time (injected; stays pure). A
    /// missing charging state is an explicit "unavailable" — the sample is
    /// skipped, never guessed from other fields.
    pub fn observe(&mut self, sample: &BatterySample, now_s: u64) {
        if !matches!(self.status, CapStatus::Undetermined) {
            return; // Hidden has nothing to run; both verdicts are sticky.
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

/// One-time prompt bookkeeping plus the persisted detection stretch. Once
/// answered, never reset — the prompt is shown at most once per machine
/// (spec FR-1). The stretch rides in the same file so a restart resumes a
/// running detection instead of resetting it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PromptRecord {
    pub answered: Option<CapPromptAnswer>,
    pub stretch: Option<StretchState>,
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
/// style):
///
/// ```text
/// battery-viz-cap-setup-1
/// <answered|dismissed|empty>
/// stretch <started> <last_seen> <max_soc|->
/// ```
fn parse_record(text: &str) -> Result<PromptRecord, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.first() != Some(&RECORD_MAGIC) {
        return Err(format!(
            "unrecognized format, wanted magic {RECORD_MAGIC:?}"
        ));
    }
    if lines.len() > 3 {
        return Err("too many lines".into());
    }
    let answered = match lines.get(1).map(|l| l.trim()) {
        None | Some("") => None,
        Some(token) => Some(
            CapPromptAnswer::from_token(token)
                .ok_or_else(|| format!("unknown answer token {token:?}"))?,
        ),
    };
    let stretch = match lines.get(2).copied().map(str::trim) {
        None | Some("") => None,
        Some(line) => {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 4 || parts[0] != "stretch" {
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
            Some(StretchState {
                started,
                last_seen,
                max_soc,
            })
        }
    };
    Ok(PromptRecord { answered, stretch })
}

/// Persist the record atomically (temp sibling + fsync + rename, the history
/// store's pattern), so a crash mid-write cannot half-write it. A fully
/// empty record removes the file instead — there is nothing to keep.
pub fn save_record(path: &Path, record: PromptRecord) -> Result<(), String> {
    if record.answered.is_none() && record.stretch.is_none() {
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
        let answer = record
            .answered
            .map_or(String::new(), |a| format!("{}\n", a.token()));
        write!(f, "{RECORD_MAGIC}\n{answer}{stretch}")
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
