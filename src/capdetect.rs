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

/// Folds samples into a cap verdict. No I/O; the clock is injected.
#[derive(Debug)]
pub struct CapMonitor {
    status: CapStatus,
    /// Unix second the current plugged-in stretch began, if any.
    plugged_since: Option<u64>,
    /// Highest state of charge seen during the current stretch.
    stretch_max_soc: Option<f64>,
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
            return;
        }
        let since = *self.plugged_since.get_or_insert(now_s);
        let soc = sample.state_of_charge;

        // Reaching full on AC is positive evidence there is no cap: a capped
        // pack is held below full by firmware. `Full` without a state of
        // charge also counts — both collectors only report `Full` when the
        // pack really is full.
        if state == ChargingState::Full || soc.is_some_and(|s| s >= NEAR_FULL_SOC) {
            self.status = CapStatus::Uncapped;
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
        }
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

/// One-time prompt bookkeeping. Once answered, never reset — the prompt is
/// shown at most once per machine (spec FR-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptRecord {
    pub answered: Option<CapPromptAnswer>,
}

/// Should the one-time setup prompt be on screen right now? Only positive
/// no-cap evidence (the pack reached full on AC) triggers it: until then the
/// machine might be capped, and asking would be guessing the other way.
pub fn should_prompt(status: CapStatus, record: PromptRecord) -> bool {
    matches!(status, CapStatus::Uncapped) && record.answered.is_none()
}

/// Load the persisted record. A missing or unreadable file means unanswered:
/// a two-line record cannot be made corrupt enough to matter, and the worst
/// case is one more showing — it is the answer, not the showing, that must
/// never repeat.
pub fn load_record(path: Option<&Path>) -> PromptRecord {
    let Some(path) = path else {
        return PromptRecord::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return PromptRecord::default();
    };
    let answered = text.lines().nth(1).and_then(CapPromptAnswer::from_token);
    PromptRecord { answered }
}

/// Persist an answered record atomically (temp sibling + fsync + rename, the
/// history store's pattern), so a crash mid-write cannot half-answer.
/// Unanswered records are never written.
pub fn save_record(path: &Path, record: PromptRecord) -> Result<(), String> {
    let Some(answer) = record.answered else {
        return Ok(());
    };
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> Result<(), String> {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        write!(f, "battery-viz-cap-setup-1\n{}\n", answer.token())
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
