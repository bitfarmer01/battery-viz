//! FR-9: weekly digest — one quiet weekly summary.
//!
//! Real-time alerts are the wrong channel for slow problems. Once a week
//! (never more, see [`DigestSchedule`]), the app summarizes what aged the
//! battery over the last seven local days and names the single action
//! worth taking. The digest **absorbs** two channels that otherwise nag:
//! warm hours (FR-4) and alert types retired by FR-6, whose recurring
//! conditions report here instead of in real time.
//!
//! Data honesty is the same as everywhere else on the roadmap: a
//! contributor the FR-0 adapter reports Unavailable is simply absent from
//! the digest — never a zero presented as a reading. The digest hides
//! entirely when the week's record has nothing to speak from.
//!
//! Platform rule (FR-0): this module contains no platform-specific logic.
//! Delivery goes through `crate::notify` (macOS: the menu-bar agent's
//! notification; Windows: a toast), and every availability question is
//! answered by the adapter before the digest is built.

use std::path::Path;

use crate::alert::{AlertKind, ClockInput};
use crate::capdetect::CapStatus;
use crate::caps::SettingGuidance;
use crate::history::{atomic_write, quarantine};
use crate::outcomes::OutcomeStore;
use crate::record::{DailyRecord, DailyRow};
use crate::warm::{WarmCause, WarmHoursReport, WARM_THRESHOLD_C};

/// Local hour the digest becomes deliverable: the first hour after the
/// alert engine's quiet hours end (22:00–08:00 local), so a due digest
/// goes out between 08:00 and 08:59 local.
pub const DIGEST_SEND_HOUR: u8 = 8;

/// A week, in the same local-day counter the daily record uses.
pub const DIGEST_WEEK_DAYS: u32 = 7;

/// A retired alert kind (FR-6) whose condition still recurred this week —
/// the row the FR-6 store hands over as digest-channel data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsorbedAlert {
    pub kind: AlertKind,
    /// Fires logged in the window, delivered or suppressed: how often the
    /// condition the user kept ignoring still came up.
    pub fires_this_week: u32,
}

/// The one action the week is worth. Ordered by the roadmap's modelled
/// impact: high charge first (66–94% of fade), then heat, then nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAction {
    /// Uncapped (or undetected) with real high-charge time this week.
    /// `guidance` is the adapter's answer for where the user changes it;
    /// `None` degrades to generic advice — no tool is ever invented.
    ChargeCap { guidance: Option<SettingGuidance> },
    /// Warm time accrued mostly while charging.
    ChargeWhileWarm,
    /// Warm time with an ambient/time-of-day pattern.
    KeepCool,
    /// Nothing this week.
    None,
}

impl DigestAction {
    /// One sentence, imperative, quiet — the digest's whole ask.
    pub fn line(self) -> String {
        match self {
            DigestAction::ChargeCap { guidance: Some(g) } => {
                format!("put a charge cap on — {}: {}", g.tool, g.steps)
            }
            // The adapter reports no setting to point at: degrade to advice
            // without naming a tool that may not exist on this machine.
            DigestAction::ChargeCap { guidance: None } => {
                "put a charge cap on — your system's charging settings hold the option".to_string()
            }
            DigestAction::ChargeWhileWarm => {
                "charge on a hard surface out of the sun, and unplug once the pack is full"
                    .to_string()
            }
            DigestAction::KeepCool => {
                "keep it shaded with the vents clear, and avoid charging in the sun".to_string()
            }
            DigestAction::None => "nothing needs action this week".to_string(),
        }
    }
}

/// What aged the battery this week, assembled only from data that exists.
#[derive(Debug, Clone, PartialEq)]
pub struct WeeklyDigest {
    /// First and last mean state-of-health readings of the window, when
    /// the record holds two or more. Measured deltas, never extrapolated.
    pub soh: Option<(f64, f64)>,
    /// Hours at or above the high-charge band in the window. `None` when
    /// the adapter reports state of charge Unavailable.
    pub high_charge_hours: Option<f64>,
    /// The FR-4 warm report for the same window, already gated on the
    /// adapter and on the stored history (FR-4's own hide rules).
    pub warm: Option<WarmHoursReport>,
    /// Full-charge-equivalent cycles added in the window (counter delta).
    /// `None` when the adapter reports cycle count Unavailable.
    pub cycles_added: Option<u32>,
    /// Retired alert kinds that still recurred this week.
    pub absorbed: Vec<AbsorbedAlert>,
    pub action: DigestAction,
}

/// Everything [`WeeklyDigest::build`] needs, all of it already resolved
/// through the FR-0 adapter or the app's own stores.
pub struct DigestInputs<'a> {
    pub record: &'a DailyRecord,
    pub warm: Option<WarmHoursReport>,
    pub soc_available: bool,
    pub cycles_available: bool,
    pub cap_status: CapStatus,
    pub cap_guidance: Option<SettingGuidance>,
    pub outcomes: &'a OutcomeStore,
    /// Current local day (days since the Unix epoch).
    pub now_day: u32,
}

impl WeeklyDigest {
    /// Assemble the week's digest. Returns `None` when the window has no
    /// recorded days at all — a fresh install says nothing rather than
    /// presenting a placeholder as a summary.
    pub fn build(input: DigestInputs<'_>) -> Option<Self> {
        // Today plus the previous six local days, matching the 7-day
        // retention of the 1-minute store FR-4 reads.
        let since = input.now_day.saturating_sub(DIGEST_WEEK_DAYS - 1);
        let window: Vec<&DailyRow> = input
            .record
            .rows
            .iter()
            .filter(|r| r.day >= since && r.n > 0)
            .collect();
        if window.is_empty() {
            return None;
        }

        // Fade: first → last mean SoH across the window's rows, only when
        // two or more readings exist on distinct days.
        let soh: Vec<(u32, f64)> = window
            .iter()
            .filter_map(|r| r.soh_mean().map(|s| (r.day, s)))
            .collect();
        let soh = match (soh.first(), soh.last()) {
            (Some((_, a)), Some((_, b))) if soh.len() >= 2 => Some((*a, *b)),
            _ => None,
        };

        // High-charge time: every sample in the row carried a state of
        // charge (the row counts only ≥80% samples), so the sum is real
        // whenever the adapter reads state of charge at all.
        let high_charge_hours = if input.soc_available {
            Some(window.iter().map(|r| r.high_hours()).sum())
        } else {
            None
        };

        // Cycling: the cumulative counter's rise across the window. A reset
        // (max < min, e.g. a replaced pack) is Unavailable, not negative.
        let cycles_added = if input.cycles_available {
            let cs: Vec<u32> = window.iter().filter_map(|r| r.cycles_last).collect();
            match (cs.iter().min(), cs.iter().max()) {
                (Some(min), Some(max)) if cs.len() >= 2 && max >= min => Some(max - min),
                _ => None,
            }
        } else {
            None
        };

        let absorbed = [
            AlertKind::HighTemp,
            AlertKind::DwellCoach,
            AlertKind::BandCoach,
        ]
        .into_iter()
        .filter(|k| input.outcomes.is_retired(*k))
        .map(|kind| AbsorbedAlert {
            kind,
            fires_this_week: input.outcomes.fire_count_since_day(kind, since),
        })
        .collect();

        let action = if high_charge_hours.is_some_and(|h| h > 0.0)
            && input.cap_status != CapStatus::Capped
        {
            DigestAction::ChargeCap {
                guidance: input.cap_guidance,
            }
        } else if input
            .warm
            .is_some_and(|w| w.cause() == WarmCause::ChargingWhileWarm)
        {
            DigestAction::ChargeWhileWarm
        } else if input.warm.is_some_and(|w| w.warm_minutes > 0) {
            DigestAction::KeepCool
        } else {
            DigestAction::None
        };

        Some(WeeklyDigest {
            soh,
            high_charge_hours,
            warm: input.warm,
            cycles_added,
            absorbed,
            action,
        })
    }

    /// Contributor lines for the dashboard panel, in the roadmap's modelled
    /// impact order: high charge, heat, cycling.
    pub fn panel_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(h) = self.high_charge_hours {
            lines.push(format!(
                "{} at ≥ 80% charge — the biggest single ager.",
                crate::warm::format_hours(h)
            ));
        }
        if let Some(w) = &self.warm {
            lines.push(warm_summary_line(w));
        }
        if let Some(cycles) = self.cycles_added {
            lines.push(format!("{cycles} full-charge cycles added this week."));
        }
        if let Some((a, b)) = self.soh {
            lines.push(format!(
                "State of health {a:.1}% → {b:.1}% this week ({:+.2} pts).",
                b - a
            ));
        }
        for a in &self.absorbed {
            lines.push(format!(
                "{}: retired from real-time alerts — {} this week; its coaching lives here.",
                a.kind.name(),
                if a.fires_this_week > 0 {
                    format!("the condition came up {}×", a.fires_this_week)
                } else {
                    "no recurrences".to_string()
                }
            ));
        }
        lines
    }

    /// The notification body: a one-line summary plus the single action.
    /// Retired-alert rows stay in the panel — the notification stays short.
    pub fn notification_body(&self) -> String {
        let mut summary = Vec::new();
        if let Some(h) = self.high_charge_hours {
            summary.push(format!("{} above 80% charge", crate::warm::format_hours(h)));
        }
        if let Some(w) = &self.warm {
            if w.warm_minutes > 0 {
                summary.push(format!(
                    "{} above {:.0} °C",
                    crate::warm::format_hours(w.warm_hours()),
                    WARM_THRESHOLD_C
                ));
            }
        }
        if let Some(cycles) = self.cycles_added {
            summary.push(format!("{cycles} cycles added"));
        }
        if let Some((a, b)) = self.soh {
            summary.push(format!("state of health {a:.1}% → {b:.1}%"));
        }
        if summary.is_empty() {
            return format!(
                "This week: nothing notable recorded. {}.",
                self.action.line()
            );
        }
        if self.action == DigestAction::None {
            return format!("This week: {}. Nothing needs action.", summary.join(", "));
        }
        format!(
            "This week: {}. One action worth taking: {}.",
            summary.join(", "),
            self.action.line()
        )
    }
}

/// The warm contributor's panel line: the number and the likely cause,
/// without an action sentence — the digest carries exactly one action.
/// A zero count has no cause to attribute: the clause is omitted.
fn warm_summary_line(w: &WarmHoursReport) -> String {
    let cause = if w.warm_minutes == 0 {
        String::new()
    } else {
        match w.cause() {
            WarmCause::ChargingWhileWarm => ", mostly while charging".to_string(),
            WarmCause::TimeOfDay => {
                let hour = w.peak_hour.unwrap_or(0);
                format!(
                    ", clustering around approximately {hour}:00–{}:00 local",
                    (hour + 2) % 24
                )
            }
            WarmCause::Mixed => ", with no single cause".to_string(),
        }
    };
    format!(
        "{} above {:.0} °C this week{}.",
        crate::warm::format_hours(w.warm_hours()),
        WARM_THRESHOLD_C,
        cause
    )
}

/// Persisted delivery schedule: the local day the last digest went out.
/// A fresh install anchors on its first day, so the first digest is due a
/// week later — after the record has a week to speak from.
pub struct DigestSchedule {
    last_sent_day: u32,
}

/// On-disk schema: `{"version":1,"last_sent_day":N}` — deliberately the
/// same single-file, atomic-write, quarantine-on-corrupt shape as the
/// other stores beside it. Named for the app layer, which derives the
/// full path (the uninstall wipe covers the whole directory).
pub const DIGEST_STATE_FILE: &str = "digest-state.json";

impl DigestSchedule {
    /// Load the schedule. A missing file anchors on `today` (first run); a
    /// corrupt one is quarantined (M2) and re-anchored, with the notice
    /// returned for the dashboard — never silent, never a reset that hides
    /// the break. `path` is `None` when no data dir exists: the schedule
    /// then lives in memory only and never persists.
    pub fn load(path: Option<&Path>, today: u32) -> (Self, Option<String>) {
        let Some(path) = path else {
            return (
                Self {
                    last_sent_day: today,
                },
                None,
            );
        };
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let schedule = Self {
                    last_sent_day: today,
                };
                let notice = schedule
                    .save(path)
                    .err()
                    .map(|e| format!("digest schedule could not be created: {e}"));
                (schedule, notice)
            }
            Ok(text) => match parse_last_sent_day(&text) {
                Some(day) => (Self { last_sent_day: day }, None),
                None => {
                    let err = quarantine(path, "digest schedule: unrecognized marker".into());
                    (
                        Self {
                            last_sent_day: today,
                        },
                        Some(format!(
                            "digest schedule could not be read ({}) — the weekly clock \
                             restarts; preserved at {}",
                            err.message,
                            err.backup_path.display()
                        )),
                    )
                }
            },
            Err(e) => (
                Self {
                    last_sent_day: today,
                },
                Some(format!("digest schedule could not be read: {e}")),
            ),
        }
    }

    /// Due when a week has passed since the last digest and the local hour
    /// is outside quiet hours. The send hour is the alert engine's
    /// quiet-hours end (22:00–08:00 local), re-used via
    /// [`ClockInput::is_quiet_hours`], not re-declared: a due digest waits
    /// for 08:00 local and never fires during quiet hours.
    pub fn due(&self, hour: u8, day: u32) -> bool {
        if hour < DIGEST_SEND_HOUR {
            return false;
        }
        if (ClockInput {
            now_s: 0.0,
            hour,
            day: 0,
        })
        .is_quiet_hours()
        {
            return false;
        }
        day >= self.last_sent_day.saturating_add(DIGEST_WEEK_DAYS)
    }

    /// Record that a digest was sent this local day, persisting first so a
    /// crash can never double-send ("at most one a week" must survive a
    /// restart, not just a tick).
    pub fn mark_sent(&mut self, path: Option<&Path>, day: u32) -> Result<(), String> {
        self.last_sent_day = day;
        match path {
            Some(path) => self.save(path),
            None => Ok(()),
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        atomic_write(
            path,
            format!("{{\"version\":1,\"last_sent_day\":{}}}", self.last_sent_day).as_bytes(),
        )
    }
}

/// Read `last_sent_day` out of the schedule file's exact written shape.
/// Unknown fields are ignored; anything else is not a recognized marker.
fn parse_last_sent_day(text: &str) -> Option<u32> {
    let marker = "\"last_sent_day\":";
    let start = text.find(marker)? + marker.len();
    let rest = text[start..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    let (digits, _) = rest.split_at(end);
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}
