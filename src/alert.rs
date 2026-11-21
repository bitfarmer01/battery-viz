//! Pure alert state machine for battery-longevity coaching.
//!
//! Rules (from the spec's locked decisions):
//! - `HighTemp` — pack ≥ 45 °C sustained 60 s; resolves below 42 °C
//!   (hysteresis); 15-minute cooldown; max 3/day.
//! - `DwellCoach` — ≥ 95 % charge for 10 min; 6-hour cooldown; max 1/day.
//! - `BandCoach` — plugged in and ≥ 80 %; one unplug nudge per day. Machines
//!   sitting on AC, not charging, at or near the threshold are treated as
//!   already running an externally-set charge cap and are not nudged.
//! - Quiet hours 22:00–08:00 local suppress everything (no critical alerts
//!   exist); a suppressed fire is dropped, never queued, and consumes neither
//!   cooldown nor daily cap.
//! - FR-3: while a charge cap is detected (FR-1), the two charge coaches
//!   (`DwellCoach`, `BandCoach`) are retired via
//!   [`AlertEngine::set_charge_coaching_enabled`]; the heat alert is never
//!   gated. Re-arming after the cap disappears resets the charge coaches,
//!   so no stale window fires.
//!
//! The engine is a constant-memory `Copy`-free zone: `tick` allocates nothing
//! (all state is plain integers, all messages are `&'static str`). Time is
//! injected, so every rule is deterministic and unit-testable.

use crate::collect::types::{BatterySample, ChargingState};

/// Pack temperature at or above which the heat alert arms (°C).
const HIGH_TEMP_TRIGGER_C: f64 = 45.0;
/// Temperature below which an active heat alert resolves (hysteresis band).
const HIGH_TEMP_RESUME_C: f64 = 42.0;
/// How long the pack must stay at/below the trigger before the alert fires.
const HIGH_TEMP_SUSTAIN_S: f64 = 60.0;
/// Minimum spacing between two heat-alert fires.
const HIGH_TEMP_COOLDOWN_S: f64 = 15.0 * 60.0;
const HIGH_TEMP_MAX_PER_DAY: u32 = 3;

/// Time parked at ≥ 95 % charge before the dwell coach fires.
const DWELL_SUSTAIN_S: f64 = 10.0 * 60.0;
/// Charge level at or above which the dwell coach arms (the same threshold
/// its outcome watch (FR-6) treats as the condition still being active).
const DWELL_TRIGGER_SOC_PCT: f64 = 95.0;
/// Minimum spacing between two dwell-coach fires.
const DWELL_COOLDOWN_S: f64 = 6.0 * 60.0 * 60.0;
const DWELL_MAX_PER_DAY: u32 = 1;

/// Charge band above which the unplug nudge applies while on AC.
const BAND_SOC_PCT: f64 = 80.0;
const BAND_MAX_PER_DAY: u32 = 1;

/// Local wall-clock input for one engine tick. `now_s` is monotonic (any
/// origin); `day` is any daily-incrementing counter; `hour` is local 0–23.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockInput {
    pub now_s: f64,
    /// Local hour of day, 0–23.
    pub hour: u8,
    /// Day counter (e.g. days since epoch); rollover resets daily caps.
    pub day: u32,
}

impl ClockInput {
    /// Quiet hours: 22:00–08:00 local (22:00 ≤ hour < 24 or 0 ≤ hour < 8).
    pub fn is_quiet_hours(&self) -> bool {
        self.hour >= 22 || self.hour < 8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    HighTemp,
    DwellCoach,
    BandCoach,
}

impl AlertKind {
    /// Short label used as the notification title.
    pub fn label(self) -> &'static str {
        match self {
            AlertKind::HighTemp => "Battery running hot",
            AlertKind::DwellCoach => "Charge coaching",
            AlertKind::BandCoach => "Charge coaching",
        }
    }

    /// Distinct name for the dashboard's per-kind outcome rows (`label`
    /// deliberately shares one title across the two coaches).
    pub fn name(self) -> &'static str {
        match self {
            AlertKind::HighTemp => "High temperature",
            AlertKind::DwellCoach => "High-charge dwell",
            AlertKind::BandCoach => "Charge band",
        }
    }

    /// Pure predicate (FR-6): has the condition this alert coaches on
    /// ended? One definition per kind, right next to the thresholds that
    /// define the condition. Missing telemetry is unknown, never ended.
    pub fn condition_ended(self, s: &BatterySample) -> bool {
        match self {
            // The heat alert resolves on the same hysteresis it fires with.
            AlertKind::HighTemp => {
                s.temperature_c.is_some_and(|t| t < HIGH_TEMP_RESUME_C)
            }
            // The dwell alert acted on = the pack left the high-charge zone.
            AlertKind::DwellCoach => s
                .state_of_charge
                .is_some_and(|c| c < DWELL_TRIGGER_SOC_PCT),
            // The band alert acted on = unplugged, or a known state of
            // charge below the band. A plugged-in sample with unknown SOC
            // is unknown, not ended.
            AlertKind::BandCoach => {
                !is_plugged(s)
                    || s.state_of_charge.is_some_and(|c| c < BAND_SOC_PCT)
            }
        }
    }
}

/// One alert. `message` always carries exactly one concrete action.
#[derive(Debug, Clone, Copy)]
pub struct Alert {
    pub kind: AlertKind,
    pub message: &'static str,
}

impl Alert {
    fn message(kind: AlertKind) -> &'static str {
        match kind {
            AlertKind::HighTemp => {
                "Battery is hot — heat accelerates aging. Move the laptop to a hard, cool surface and unblock the vents."
            }
            AlertKind::DwellCoach => {
                "Battery has sat above 95% charge for 10 minutes — unplug it now, or enable optimized charging to limit time at full charge."
            }
            AlertKind::BandCoach => {
                "You are charging past 80% on AC — unplug once convenient, or set an 80% charge limit to slow calendar aging."
            }
        }
    }

    fn of(kind: AlertKind) -> Self {
        Alert { kind, message: Self::message(kind) }
    }
}

/// Per-day fire counter that resets on day rollover.
#[derive(Debug, Default, Clone, Copy)]
struct DailyCounter {
    day: u32,
    count: u32,
}

impl DailyCounter {
    fn roll(&mut self, day: u32) {
        if self.day != day {
            self.day = day;
            self.count = 0;
        }
    }

    fn exhausted(&self, max: u32) -> bool {
        self.count >= max
    }
}

#[derive(Debug, Clone, Copy)]
struct HighTempState {
    /// Monotonic time the pack most recently rose to the trigger level.
    above_since: Option<f64>,
    /// An alert is active until the pack cools below the resume threshold.
    active: bool,
    last_fired: Option<f64>,
    daily: DailyCounter,
}

#[derive(Debug, Clone, Copy)]
struct DwellState {
    /// Monotonic time the pack most recently rose to 95 %.
    above_since: Option<f64>,
    /// One fire per high-charge episode; re-armed when the pack drops below.
    latched: bool,
    last_fired: Option<f64>,
    daily: DailyCounter,
}

#[derive(Debug, Clone, Copy)]
struct BandState {
    /// One fire per plugged-in-above-band episode; re-armed on exit.
    latched: bool,
    daily: DailyCounter,
}

/// Shared, constant-memory alert engine. Feed one sample per tick.
#[derive(Debug, Clone, Copy)]
pub struct AlertEngine {
    high_temp: HighTempState,
    dwell: DwellState,
    band: BandState,
    /// FR-3: the charge coaches (DwellCoach, BandCoach) are retired while a
    /// charge cap is active. Heat coaching is never gated.
    charge_coaching_enabled: bool,
}

impl Default for AlertEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AlertEngine {
    pub fn new() -> Self {
        AlertEngine {
            high_temp: HighTempState {
                above_since: None,
                active: false,
                last_fired: None,
                daily: DailyCounter::default(),
            },
            dwell: DwellState {
                above_since: None,
                latched: false,
                last_fired: None,
                daily: DailyCounter::default(),
            },
            band: BandState { latched: false, daily: DailyCounter::default() },
            // Alerts are active until FR-1 detects a cap.
            charge_coaching_enabled: true,
        }
    }

    /// FR-3: retire (or re-arm) the charge coaches when a cap is detected
    /// (or disappears). Idempotent — the app calls this every tick with the
    /// current gate, and a repeated `true` must never disturb a coach
    /// mid-sustain. The suppressed→active transition resets both charge
    /// coaches so no stale sustain window or latch from the suppressed
    /// period fires on re-enable; the heat alert keeps its state untouched.
    pub fn set_charge_coaching_enabled(&mut self, enabled: bool) {
        if self.charge_coaching_enabled == enabled {
            return;
        }
        self.charge_coaching_enabled = enabled;
        if enabled {
            self.dwell = DwellState {
                above_since: None,
                latched: false,
                last_fired: None,
                daily: DailyCounter::default(),
            };
            self.band = BandState {
                latched: false,
                daily: DailyCounter::default(),
            };
        }
    }

    /// Advance the machine by one sample. Returns the alert to deliver, if
    /// any (at most one per tick). Never allocates.
    pub fn tick(&mut self, sample: &BatterySample, clock: ClockInput) -> Option<Alert> {
        self.tick_high_temp(sample.temperature_c, clock)
            .or_else(|| self.tick_dwell(sample.state_of_charge, clock))
            .or_else(|| self.tick_band(sample, clock))
    }

    fn tick_high_temp(
        &mut self,
        temp_c: Option<f64>,
        clock: ClockInput,
    ) -> Option<Alert> {
        let st = &mut self.high_temp;
        st.daily.roll(clock.day);

        // Missing data never arms the alert; it resets the sustain window.
        match temp_c {
            Some(t) if t >= HIGH_TEMP_TRIGGER_C => {
                st.above_since = Some(st.above_since.unwrap_or(clock.now_s));
            }
            _ => st.above_since = None,
        }

        // Hysteresis resolve: an active alert clears only below the resume
        // threshold, so a pack hovering at the trigger cannot nag.
        if st.active && temp_c.is_some_and(|t| t < HIGH_TEMP_RESUME_C) {
            st.active = false;
        }

        let sustained =
            st.above_since.is_some_and(|since| clock.now_s - since >= HIGH_TEMP_SUSTAIN_S);
        let cooled_down = st
            .last_fired
            .is_none_or(|t| clock.now_s - t >= HIGH_TEMP_COOLDOWN_S);
        if st.active || !sustained || !cooled_down || clock.is_quiet_hours() {
            return None;
        }
        if st.daily.exhausted(HIGH_TEMP_MAX_PER_DAY) {
            return None;
        }

        st.active = true;
        st.last_fired = Some(clock.now_s);
        st.daily.count += 1;
        Some(Alert::of(AlertKind::HighTemp))
    }

    fn tick_dwell(&mut self, soc: Option<f64>, clock: ClockInput) -> Option<Alert> {
        // FR-3: retired while a charge cap is active — nothing accumulates.
        if !self.charge_coaching_enabled {
            return None;
        }
        let st = &mut self.dwell;
        st.daily.roll(clock.day);

        match soc {
            Some(s) if s >= DWELL_TRIGGER_SOC_PCT => {
                st.above_since = Some(st.above_since.unwrap_or(clock.now_s));
            }
            // Leaving the band re-arms the latch for the next episode.
            _ => {
                st.above_since = None;
                st.latched = false;
            }
        }

        let sustained =
            st.above_since.is_some_and(|since| clock.now_s - since >= DWELL_SUSTAIN_S);
        let cooled_down = st
            .last_fired
            .is_none_or(|t| clock.now_s - t >= DWELL_COOLDOWN_S);
        if st.latched || !sustained || !cooled_down || clock.is_quiet_hours() {
            return None;
        }
        if st.daily.exhausted(DWELL_MAX_PER_DAY) {
            return None;
        }

        st.latched = true;
        st.last_fired = Some(clock.now_s);
        st.daily.count += 1;
        Some(Alert::of(AlertKind::DwellCoach))
    }

    fn tick_band(&mut self, sample: &BatterySample, clock: ClockInput) -> Option<Alert> {
        // FR-3: retired while a charge cap is active — nothing accumulates.
        if !self.charge_coaching_enabled {
            return None;
        }
        let st = &mut self.band;
        st.daily.roll(clock.day);

        let in_band = is_plugged(sample)
            && sample.state_of_charge.is_some_and(|s| s >= BAND_SOC_PCT);
        if !in_band {
            st.latched = false;
            return None;
        }
        // An externally-set charge cap parks a machine on AC, not charging,
        // at the limit. Nudging it to "set an 80% limit" would nag it about
        // the cap it already has, so the nudge is suppressed — without
        // consuming the daily cap or latching (audit finding H5). Lightweight
        // cap signal only; full cap detection is roadmap FR-1.
        if is_detected_cap(sample) {
            return None;
        }
        if st.latched || clock.is_quiet_hours() || st.daily.exhausted(BAND_MAX_PER_DAY) {
            return None;
        }

        st.latched = true;
        st.daily.count += 1;
        Some(Alert::of(AlertKind::BandCoach))
    }
}

/// "Plugged in" for coaching purposes: charging, full, or on AC without
/// charging. Unknown state never counts as plugged in.
fn is_plugged(sample: &BatterySample) -> bool {
    matches!(
        sample.charging_state,
        Some(ChargingState::Charging)
            | Some(ChargingState::Full)
            | Some(ChargingState::OnAcNotCharging)
    )
}

/// Pure predicate: does this sample look like an externally-set charge cap?
/// True when the machine is on AC, not charging, parked at the band
/// threshold — the state a capped machine settles in. On-AC readings below
/// the threshold never fire anyway (they are outside the band), so no
/// separate tolerance is needed.
fn is_detected_cap(sample: &BatterySample) -> bool {
    sample.charging_state == Some(ChargingState::OnAcNotCharging)
        && sample
            .state_of_charge
            .is_some_and(|s| s >= BAND_SOC_PCT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::types::{BatterySample, ChargingState};

    const DAY: u32 = 20_000;
    /// 10:00 local — inside alerting hours for every test except quiet-hours.
    const NOON_HOUR: u8 = 10;

    fn clock(now_s: f64) -> ClockInput {
        ClockInput { now_s, hour: NOON_HOUR, day: DAY }
    }

    fn sample(temp_c: Option<f64>, soc: Option<f64>, state: ChargingState) -> BatterySample {
        BatterySample {
            temperature_c: temp_c,
            state_of_charge: Some(soc.unwrap_or(50.0)),
            charging_state: Some(state),
            ..Default::default()
        }
    }

    fn tick_at(engine: &mut AlertEngine, sample: &BatterySample, now_s: f64) -> Option<Alert> {
        engine.tick(sample, clock(now_s))
    }

    /// Tick and report only whether `kind` fired (other kinds are ignored —
    /// several coaches can be armed by the same sample).
    fn tick_kind(
        engine: &mut AlertEngine,
        sample: &BatterySample,
        now_s: f64,
        kind: AlertKind,
    ) -> bool {
        matches!(engine.tick(sample, clock(now_s)), Some(a) if a.kind == kind)
    }

    /// Drive `engine` at 1 Hz from 0..=now_s, counting fires of one kind.
    fn run_kind(
        engine: &mut AlertEngine,
        sample: &BatterySample,
        up_to_s: f64,
        kind: AlertKind,
    ) -> usize {
        let mut fired = 0;
        let mut now = 0.0;
        while now <= up_to_s {
            if tick_kind(engine, sample, now, kind) {
                fired += 1;
            }
            now += 1.0;
        }
        fired
    }

    // ---- HighTemp ----------------------------------------------------------

    #[test]
    fn high_temp_does_not_fire_before_sustain_window() {
        let mut e = AlertEngine::new();
        let s = sample(Some(50.0), None, ChargingState::Discharging);
        for step in 0..59 {
            let now = step as f64;
            assert!(tick_at(&mut e, &s, now).is_none(), "fired early at {now}");
        }
    }

    #[test]
    fn high_temp_fires_after_60s_sustained() {
        let mut e = AlertEngine::new();
        let s = sample(Some(50.0), None, ChargingState::Discharging);
        let mut fired_alert = None;
        let mut now = 0.0;
        while now <= 60.0 {
            if let Some(a) = tick_at(&mut e, &s, now) {
                fired_alert = Some(a);
            }
            now += 1.0;
        }
        let a = fired_alert.expect("high-temp alert must fire after 60s sustained");
        assert_eq!(a.kind, AlertKind::HighTemp);
        assert!(a.message.contains("vents"), "message must carry an action");
    }

    #[test]
    fn high_temp_brief_spikes_never_accumulate() {
        let mut e = AlertEngine::new();
        // One-second spikes above 45 separated by cool ticks: the sustain
        // window resets on every cool tick, so it never reaches 60 s.
        for cycle in 0..40 {
            let base = (cycle * 4) as f64;
            assert!(tick_at(&mut e, &sample(Some(47.0), None, ChargingState::Discharging), base).is_none());
            assert!(tick_at(&mut e, &sample(Some(30.0), None, ChargingState::Discharging), base + 1.0).is_none());
            assert!(tick_at(&mut e, &sample(Some(47.0), None, ChargingState::Discharging), base + 2.0).is_none());
            assert!(tick_at(&mut e, &sample(Some(30.0), None, ChargingState::Discharging), base + 3.0).is_none());
        }
    }

    #[test]
    fn high_temp_stays_active_until_below_resume_threshold() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        let warm = sample(Some(43.0), None, ChargingState::Discharging); // between 42 and 45
        run_kind(&mut e, &hot, 60.0, AlertKind::HighTemp);
        // Between 45 and 42: alert stays armed, no second fire, no resolve.
        assert!(tick_at(&mut e, &warm, 120.0).is_none());
        // Above trigger again, sustained: still latched from the first fire.
        assert!(tick_at(&mut e, &hot, 300.0).is_none());
    }

    #[test]
    fn high_temp_resolves_below_42_then_refires_after_cooldown() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        let cool = sample(Some(30.0), None, ChargingState::Discharging);

        // Phase 1: sustained heat fires once (at t=60).
        assert_eq!(run_kind(&mut e, &hot, 60.0, AlertKind::HighTemp), 1);

        // Phase 2: quick re-heat just past the resolve threshold — resolved,
        // re-sustained, but the 15-min cooldown from t=60 blocks the fire.
        assert!(tick_at(&mut e, &cool, 70.0).is_none(), "resolve tick");
        for step in 80..140 {
            let now = step as f64;
            assert!(
                tick_at(&mut e, &hot, now).is_none(),
                "fired inside cooldown at {now}"
            );
        }

        // Phase 3: cool, wait past the cooldown, re-sustain → second fire.
        assert!(tick_at(&mut e, &cool, 150.0).is_none());
        for step in 160..1_000 {
            let now = step as f64;
            assert!(tick_at(&mut e, &cool, now).is_none());
        }
        // Sustain restarts at t=1_000; cooldown from t=60 expired at t=960.
        let mut fired_second = None;
        let mut now = 1_001.0;
        while now <= 1_100.0 {
            if tick_kind(&mut e, &hot, now, AlertKind::HighTemp) {
                fired_second = Some(now);
                break;
            }
            now += 1.0;
        }
        let at = fired_second.expect("no second fire after resolve + cooldown");
        assert_eq!(at, 1_061.0, "must fire at 60 s of new sustain (restarts at t=1001)");
        assert_eq!(run_kind(&mut e, &hot, 1_200.0, AlertKind::HighTemp), 0, "latched after refire");
    }

    #[test]
    fn high_temp_caps_at_three_per_day() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        let cool = sample(Some(30.0), None, ChargingState::Discharging);
        let mut fires = 0;
        // Long hot/cold cycling: each cycle (60 s hot, then cool) is one fire
        // opportunity after cooldown; far more cycles than the daily cap.
        for cycle in 0..20 {
            let base = (cycle * 2_000) as f64;
            for i in 0..70 {
                if tick_at(&mut e, &hot, base + i as f64).is_some() {
                    fires += 1;
                }
            }
            for i in 70..100 {
                assert!(tick_at(&mut e, &cool, base + i as f64).is_none());
            }
        }
        assert_eq!(fires, 3, "daily cap of 3 must hold regardless of cycling");
    }

    #[test]
    fn high_temp_cap_resets_on_day_rollover() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        let cool = sample(Some(30.0), None, ChargingState::Discharging);
        let mut fired_today = 0;
        // Same hot/cool cycling as the caps test: cool ticks resolve the
        // alert so each cycle is a fresh fire opportunity (cooldown allowing).
        for cycle in 0..5 {
            let base = (cycle * 2_000) as f64;
            for i in 0..70 {
                if tick_kind(&mut e, &hot, base + i as f64, AlertKind::HighTemp) {
                    fired_today += 1;
                }
            }
            for i in 70..100 {
                assert!(tick_at(&mut e, &cool, base + i as f64).is_none());
            }
        }
        assert_eq!(fired_today, 3);
        // Next day: the cap is fresh and a fire goes out again.
        let next_day = ClockInput { now_s: 90_000.0, hour: NOON_HOUR, day: DAY + 1 };
        assert!(e.tick(&hot, next_day).is_none(), "sustain window must restart");
        let mut fired_next_day = 0;
        for i in 0..61 {
            let c = ClockInput { now_s: 90_000.0 + i as f64, hour: NOON_HOUR, day: DAY + 1 };
            if e.tick(&hot, c).is_some() {
                fired_next_day += 1;
            }
        }
        assert_eq!(fired_next_day, 1);
    }

    #[test]
    fn quiet_hours_suppress_high_temp_without_consuming_cap() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        // 23:00, sustained past the window: suppressed.
        let mut fired_quiet = 0;
        for i in 0..120 {
            let c = ClockInput { now_s: i as f64, hour: 23, day: DAY };
            if e.tick(&hot, c).is_some() {
                fired_quiet += 1;
            }
        }
        assert_eq!(fired_quiet, 0, "quiet hours must suppress");
        // 08:00 same day, still hot: fires — the suppressed attempts consumed
        // neither the cap nor the cooldown.
        let mut fired_morning = 0;
        for i in 120..200 {
            let c = ClockInput { now_s: i as f64, hour: 8, day: DAY };
            if e.tick(&hot, c).is_some() {
                fired_morning += 1;
            }
        }
        assert_eq!(fired_morning, 1);
    }

    #[test]
    fn quiet_hours_boundary_22_and_8() {
        assert!(ClockInput { now_s: 0.0, hour: 22, day: DAY }.is_quiet_hours());
        assert!(ClockInput { now_s: 0.0, hour: 23, day: DAY }.is_quiet_hours());
        assert!(ClockInput { now_s: 0.0, hour: 0, day: DAY }.is_quiet_hours());
        assert!(ClockInput { now_s: 0.0, hour: 7, day: DAY }.is_quiet_hours());
        assert!(!ClockInput { now_s: 0.0, hour: 8, day: DAY }.is_quiet_hours());
        assert!(!ClockInput { now_s: 0.0, hour: 21, day: DAY }.is_quiet_hours());
    }

    #[test]
    fn high_temp_missing_temp_resets_sustain_window() {
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), None, ChargingState::Discharging);
        let unknown = sample(None, None, ChargingState::Discharging);
        assert!(tick_at(&mut e, &hot, 0.0).is_none());
        assert!(tick_at(&mut e, &unknown, 30.0).is_none());
        // Window restarted: even 40 s later the 60 s sustain is not met.
        assert!(tick_at(&mut e, &hot, 70.0).is_none());
    }

    // ---- DwellCoach --------------------------------------------------------

    #[test]
    fn dwell_fires_after_10_minutes_at_95() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(97.0), ChargingState::Charging);
        for step in 0..599 {
            let now = step as f64;
            assert!(
                !tick_kind(&mut e, &s, now, AlertKind::DwellCoach),
                "dwell fired early at {now}"
            );
        }
        assert!(tick_kind(&mut e, &s, 600.0, AlertKind::DwellCoach));
        // Verify the message on a fresh engine: drive to the first DWELL fire
        // (a BandCoach nudge may fire earlier on this plugged-in sample).
        let mut fresh = AlertEngine::new();
        for step in 0..=600 {
            let now = step as f64;
            if let Some(a) = fresh.tick(&s, clock(now)) {
                if a.kind == AlertKind::DwellCoach {
                    assert!(a.message.contains("unplug"), "message must carry an action");
                    break;
                }
            }
        }
    }

    #[test]
    fn dwell_one_per_day_even_if_still_parked_at_100() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(100.0), ChargingState::Charging);
        // 8 hours parked at 100%: exactly one dwell-coach fire.
        let dwell = run_kind(&mut e, &s, 3_600.0 * 8.0, AlertKind::DwellCoach);
        assert_eq!(dwell, 1, "must not nag twice for one parked episode");
    }

    #[test]
    fn dwell_rearms_and_respects_six_hour_cooldown() {
        let mut e = AlertEngine::new();
        let charged = sample(None, Some(96.0), ChargingState::Charging);
        let used = sample(None, Some(60.0), ChargingState::Discharging);

        assert!(!tick_kind(&mut e, &charged, 0.0, AlertKind::DwellCoach));
        assert!(tick_kind(&mut e, &charged, 600.0, AlertKind::DwellCoach), "first dwell fire");

        // Drop below 95 and come back after 1 h: latch re-arms, but the
        // 6-hour cooldown from the first fire still holds.
        assert!(tick_at(&mut e, &used, 3_600.0).is_none());
        for step in 3_601..7_200 {
            let now = step as f64;
            assert!(
                !tick_kind(&mut e, &charged, now, AlertKind::DwellCoach),
                "fired inside cooldown"
            );
        }
        // Cooldown from the first fire expires at t=22,200. Re-arm the latch
        // (drop below 95) and re-sustain: same day, the cap of 1 blocks even
        // with cooldown long gone.
        assert!(tick_at(&mut e, &used, 22_300.0).is_none());
        for step in 23_000..23_100 {
            let now = step as f64;
            assert!(
                !tick_kind(&mut e, &charged, now, AlertKind::DwellCoach),
                "daily cap of 1 must block same-day re-fire"
            );
        }
        // Next day, still parked: fires once.
        let mut next_day = 0;
        for i in 86_400..87_200 {
            let c = ClockInput { now_s: i as f64, hour: NOON_HOUR, day: DAY + 1 };
            if matches!(e.tick(&charged, c), Some(a) if a.kind == AlertKind::DwellCoach) {
                next_day += 1;
            }
        }
        assert_eq!(next_day, 1);
    }

    #[test]
    fn dwell_below_95_does_not_fire() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(94.9), ChargingState::Charging);
        assert_eq!(run_kind(&mut e, &s, 700.0, AlertKind::DwellCoach), 0);
    }

    // ---- BandCoach ---------------------------------------------------------

    #[test]
    fn band_fires_once_when_plugged_above_80() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(90.0), ChargingState::Charging);
        let mut fired = None;
        for step in 0..10 {
            let now = step as f64;
            if let Some(a) = tick_at(&mut e, &s, now) {
                fired = Some((now, a));
                break;
            }
        }
        let (at, a) = fired.expect("band nudge must fire on a plugged-in sample above 80%");
        assert_eq!(a.kind, AlertKind::BandCoach);
        assert!(a.message.contains("unplug"), "message must carry an action");
        // Stays plugged in: no more nudges today.
        let mut now = at + 1.0;
        while now <= 600.0 {
            assert!(!tick_kind(&mut e, &s, now, AlertKind::BandCoach));
            now += 1.0;
        }
    }

    #[test]
    fn band_rearms_after_unplug_but_once_per_day() {
        let mut e = AlertEngine::new();
        let plugged = sample(None, Some(90.0), ChargingState::Charging);
        let unplugged = sample(None, Some(40.0), ChargingState::Discharging);

        assert!(tick_at(&mut e, &plugged, 1.0).is_some(), "first nudge");
        assert!(tick_at(&mut e, &unplugged, 2.0).is_none());

        // Re-plugged same day: re-armed but daily cap of 1 blocks.
        assert!(tick_at(&mut e, &plugged, 10.0).is_none());

        // Next day: one fresh nudge.
        let c = ClockInput { now_s: 86_400.0, hour: NOON_HOUR, day: DAY + 1 };
        assert!(e.tick(&plugged, c).is_some());
    }

    #[test]
    fn band_requires_plugged_and_above_80() {
        let mut e = AlertEngine::new();
        let unplugged_high =
            sample(None, Some(90.0), ChargingState::Discharging);
        let plugged_low = sample(None, Some(50.0), ChargingState::Charging);
        assert_eq!(run_kind(&mut e, &unplugged_high, 100.0, AlertKind::BandCoach), 0);
        assert_eq!(run_kind(&mut e, &plugged_low, 100.0, AlertKind::BandCoach), 0);
    }

    #[test]
    fn band_full_counts_as_plugged_but_on_ac_not_charging_is_a_cap() {
        let mut e = AlertEngine::new();
        // `Full` counts as plugged in — a full pack with no cap set is
        // exactly who the unplug nudge is for.
        let full = sample(None, Some(95.0), ChargingState::Full);
        assert!(tick_at(&mut e, &full, 1.0).is_some());
        // On AC, not charging, in the band: a detected externally-set cap
        // (audit H5) — never nudged.
        let capped = sample(None, Some(95.0), ChargingState::OnAcNotCharging);
        let mut fresh = AlertEngine::new();
        assert!(!tick_kind(&mut fresh, &capped, 1.0, AlertKind::BandCoach));
    }

    #[test]
    fn band_quiet_hours_suppress_without_consuming_daily_cap() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(90.0), ChargingState::Charging);
        let mut quiet_fires = 0;
        for i in 0..60 {
            let c = ClockInput { now_s: i as f64, hour: 23, day: DAY };
            if e.tick(&s, c).is_some() {
                quiet_fires += 1;
            }
        }
        assert_eq!(quiet_fires, 0);
        // Morning: the nudge still goes out that day.
        let c = ClockInput { now_s: 100.0, hour: 8, day: DAY };
        assert!(e.tick(&s, c).is_some());
    }

    // ---- H5: externally-set charge caps are not nagged ---------------------

    #[test]
    fn band_capped_machine_at_80_on_ac_never_fires() {
        let mut e = AlertEngine::new();
        // Exactly the audited machine: reads 80, on AC, not charging.
        let capped = sample(None, Some(80.0), ChargingState::OnAcNotCharging);
        assert_eq!(run_kind(&mut e, &capped, 3_600.0, AlertKind::BandCoach), 0);
    }

    #[test]
    fn band_capped_machine_suppression_consumes_no_daily_cap() {
        let mut e = AlertEngine::new();
        let capped = sample(None, Some(80.0), ChargingState::OnAcNotCharging);
        assert_eq!(run_kind(&mut e, &capped, 3_600.0, AlertKind::BandCoach), 0);
        // Same day, the machine actively charges past 80: the nudge fires
        // once — the capped episodes consumed nothing.
        let charging = sample(None, Some(85.0), ChargingState::Charging);
        assert!(tick_at(&mut e, &charging, 3_601.0).is_some());
        assert!(!tick_kind(&mut e, &charging, 3_602.0, AlertKind::BandCoach));
    }

    #[test]
    fn detected_cap_predicate_is_threshold_scoped_and_pure() {
        // On AC, not charging, at or above the threshold: a detected cap.
        assert!(is_detected_cap(&sample(None, Some(80.0), ChargingState::OnAcNotCharging)));
        assert!(is_detected_cap(&sample(None, Some(95.0), ChargingState::OnAcNotCharging)));
        // Just below the threshold: outside the band, not a cap signal.
        assert!(!is_detected_cap(&sample(None, Some(79.9), ChargingState::OnAcNotCharging)));
        // Other plugged-in states in the band are not cap signals.
        assert!(!is_detected_cap(&sample(None, Some(85.0), ChargingState::Charging)));
        assert!(!is_detected_cap(&sample(None, Some(85.0), ChargingState::Discharging)));
        assert!(!is_detected_cap(&sample(None, Some(85.0), ChargingState::Full)));
        // Missing SOC is never treated as a cap.
        assert!(!is_detected_cap(&sample(None, None, ChargingState::OnAcNotCharging)));
    }

    #[test]
    fn band_charging_at_threshold_still_fires_once() {
        // Actively charging past the threshold is real coaching, capped or not.
        let mut e = AlertEngine::new();
        let s = sample(None, Some(81.0), ChargingState::Charging);
        assert_eq!(run_kind(&mut e, &s, 600.0, AlertKind::BandCoach), 1);
    }

    #[test]
    fn band_discharging_at_80_never_fires() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(80.0), ChargingState::Discharging);
        assert_eq!(run_kind(&mut e, &s, 600.0, AlertKind::BandCoach), 0);
    }

    // ---- Engine shape ------------------------------------------------------

    #[test]
    fn engine_state_is_constant_memory_no_heap_types() {
        // The tick path cannot allocate: every state type is Copy and holds
        // only plain integers; messages are 'static. This pins the contract —
        // if someone adds a Vec/String/Box to the state it stops compiling.
        fn assert_copy<T: Copy>() {}
        assert_copy::<AlertEngine>();
        assert_copy::<Alert>();
        assert_copy::<ClockInput>();
        // Generous ceiling; the engine is a few dozen bytes.
        assert!(std::mem::size_of::<AlertEngine>() < 200);
    }

    #[test]
    fn session_fire_counts_match_caps_and_cooldowns() {
        // HighTemp across a 1,000-tick (1 Hz) session with resolve and
        // re-sustain: fire #1 at t=60s (sustained); resolve at t=61 (temp
        // drops below the 42 °C hysteresis); re-sustained from t=121, but
        // the 15 min cooldown holds until t=960 — fire #2 lands there, not
        // at t=181. The struct-level "one value per tick" property is
        // guaranteed by `tick` returning an Option; what this verifies is
        // hysteresis re-arming plus cooldown gating. DwellCoach (600 s of
        // ≥95%) and BandCoach (first tick in band) each cap at 1/day.
        let mut e = AlertEngine::new();
        let hot = sample(Some(50.0), Some(100.0), ChargingState::Charging);
        let cool = sample(Some(40.0), Some(100.0), ChargingState::Charging);
        let (mut fires, mut hot_fires) = (Vec::new(), 0);
        for i in 0..1_000 {
            let c = ClockInput { now_s: i as f64, hour: NOON_HOUR, day: DAY };
            let s = if (61..=120).contains(&i) { &cool } else { &hot };
            if let Some(a) = e.tick(s, c) {
                fires.push((i, a.kind));
                if matches!(a.kind, AlertKind::HighTemp) {
                    hot_fires += 1;
                }
            }
        }
        assert_eq!(hot_fires, 2, "fire, resolve, re-sustain, cooldown-gated refire");
        assert!(fires.iter().any(|&(t, k)| k == AlertKind::HighTemp && t >= 960));
        assert!(
            !fires.iter().any(|&(t, k)| k == AlertKind::HighTemp && (181..960).contains(&t)),
            "no HighTemp fire inside the cooldown"
        );
        assert_eq!(fires.iter().filter(|(_, k)| *k == AlertKind::DwellCoach).count(), 1);
        assert_eq!(fires.iter().filter(|(_, k)| *k == AlertKind::BandCoach).count(), 1);
    }

    // ---- FR-6: end-of-condition predicates ----------------------------------

    #[test]
    fn condition_ended_matches_each_kind_own_threshold() {
        // HighTemp: ended only below the 42 °C hysteresis; unknown is never ended.
        assert!(!AlertKind::HighTemp.condition_ended(&sample(
            Some(45.0),
            None,
            ChargingState::Discharging
        )));
        assert!(!AlertKind::HighTemp.condition_ended(&sample(
            Some(43.0),
            None,
            ChargingState::Discharging
        )));
        assert!(AlertKind::HighTemp.condition_ended(&sample(
            Some(41.9),
            None,
            ChargingState::Discharging
        )));
        assert!(!AlertKind::HighTemp.condition_ended(&sample(
            None,
            None,
            ChargingState::Discharging
        )));
        // DwellCoach: ended below the 95 % arm threshold; unknown never ended.
        assert!(!AlertKind::DwellCoach.condition_ended(&sample(
            None,
            Some(95.0),
            ChargingState::Charging
        )));
        assert!(AlertKind::DwellCoach.condition_ended(&sample(
            None,
            Some(94.9),
            ChargingState::Charging
        )));
        // The test helper above defaults SOC to 50 % when told `None`, so
        // the true missing-SOC cases are built by hand: unknown telemetry
        // is unknown, never ended.
        let no_soc = BatterySample {
            temperature_c: None,
            charging_state: Some(ChargingState::Charging),
            ..Default::default()
        };
        assert!(!AlertKind::DwellCoach.condition_ended(&no_soc));
        assert!(!AlertKind::BandCoach.condition_ended(&no_soc));
        // BandCoach: ended when unplugged or below the band.
        assert!(AlertKind::BandCoach.condition_ended(&sample(
            None,
            Some(40.0),
            ChargingState::Discharging
        )));
        assert!(AlertKind::BandCoach.condition_ended(&sample(
            None,
            Some(79.9),
            ChargingState::Charging
        )));
        assert!(!AlertKind::BandCoach.condition_ended(&sample(
            None,
            Some(85.0),
            ChargingState::Charging
        )));
    }

    #[test]
    fn every_kind_has_a_distinct_dashboard_name() {
        let names = [
            AlertKind::HighTemp,
            AlertKind::DwellCoach,
            AlertKind::BandCoach,
        ]
        .map(|k| k.name());
        assert_ne!(names[0], names[1]);
        assert_ne!(names[1], names[2]);
        assert_ne!(names[0], names[2]);
    }

    // ---- FR-3: charge-coaching retirement while a cap is on ----------------

    /// The FR-3 gate retires DwellCoach and BandCoach; the heat alert on the
    /// same engine keeps its full behavior.
    #[test]
    fn fr3_gate_retires_charge_coaches_but_not_heat() {
        let mut e = AlertEngine::new();
        e.set_charge_coaching_enabled(false);
        // 24 h parked at 100 % on AC: neither charge coach may fire.
        let parked = sample(None, Some(100.0), ChargingState::Charging);
        let mut charge_fires = 0;
        for i in 0..(24 * 3_600) {
            if matches!(tick_at(&mut e, &parked, i as f64), Some(a) if a.kind != AlertKind::HighTemp)
            {
                charge_fires += 1;
            }
        }
        assert_eq!(charge_fires, 0, "retired charge coaches must never fire");
        // The heat alert on the same engine is untouched by the gate: a hot
        // pack still fires after its usual 60 s sustain.
        let hot = sample(Some(50.0), Some(100.0), ChargingState::Charging);
        assert!(
            tick_kind(&mut e, &hot, 0.0, AlertKind::HighTemp) || {
                let mut fired = false;
                for i in 1..=60 {
                    fired |= tick_kind(&mut e, &hot, i as f64, AlertKind::HighTemp);
                }
                fired
            },
            "heat alert must be unchanged by the cap gate"
        );
    }

    /// Re-arming after the cap disappears resets both charge coaches: no
    /// stale sustain window from the suppressed period may fire at once.
    #[test]
    fn fr3_reenable_resets_stale_charge_windows() {
        let mut e = AlertEngine::new();
        let parked = sample(None, Some(100.0), ChargingState::Charging);
        // Halfway through a qualifying dwell window, the cap is detected.
        for i in 0..300 {
            tick_at(&mut e, &parked, i as f64);
        }
        e.set_charge_coaching_enabled(false);
        // Suppressed period (state frozen), then the cap disappears.
        e.set_charge_coaching_enabled(true);
        // Even though the old window was far past 10 min of clock, the
        // re-armed coach needs a fresh sustain. (BandCoach does fire on
        // this tick — its episode latch was reset too; that is correct.)
        assert!(!tick_kind(&mut e, &parked, 10_000.0, AlertKind::DwellCoach));
        for i in 1..600 {
            assert!(
                !tick_kind(&mut e, &parked, 10_000.0 + i as f64, AlertKind::DwellCoach),
                "stale window fired on re-enable at +{i}"
            );
        }
        assert!(
            tick_kind(&mut e, &parked, 10_600.0, AlertKind::DwellCoach),
            "a fresh 10-minute sustain fires the coach again"
        );
    }

    /// The app re-applies the gate every tick. A repeated `true` must be a
    /// no-op, never a reset of an in-progress window.
    #[test]
    fn fr3_repeated_enabled_calls_do_not_disturb_a_sustain() {
        let mut e = AlertEngine::new();
        let parked = sample(None, Some(100.0), ChargingState::Charging);
        for i in 0..300 {
            e.set_charge_coaching_enabled(true);
            tick_at(&mut e, &parked, i as f64);
        }
        e.set_charge_coaching_enabled(true);
        // The 5 minutes accumulated while the gate was re-applied still
        // count toward the 10-minute sustain.
        for i in 300..600 {
            assert!(
                !tick_kind(&mut e, &parked, i as f64, AlertKind::DwellCoach),
                "fired early at +{i}"
            );
        }
        assert!(
            tick_kind(&mut e, &parked, 600.0, AlertKind::DwellCoach),
            "idempotent gate re-apply must not reset the window"
        );
    }

    /// The fresh-engine default is charge coaching ON: alerts are active
    /// until FR-1 detects a cap.
    #[test]
    fn fr3_fresh_engine_has_charge_coaching_enabled() {
        let mut e = AlertEngine::new();
        let s = sample(None, Some(90.0), ChargingState::Charging);
        assert!(
            tick_at(&mut e, &s, 1.0).is_some(),
            "no-cap machines keep their nudge"
        );
    }
}
