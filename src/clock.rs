//! Local wall-clock helper: hour-of-day and day index without pulling a
//! date-time crate. Quiet hours are a local-time concept, so the alert engine
//! needs the machine's local hour, not UTC.
//!
//! `day` is "days since the Unix epoch" — any daily-incrementing counter
//! works for the engine's daily caps, and this keeps midnight rollover
//! correct even when a UTC/local boundary disagrees.

/// Local time slice consumed by the alert engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    /// Local hour of day, 0–23.
    pub hour: u8,
    /// Days since the Unix epoch.
    pub day: u32,
    /// Unix seconds of the exact instant this slice was read from. Consumers
    /// that also need "now" in unix time MUST take it from here, not from a
    /// second clock read: two reads can straddle an hour tick and disagree by
    /// a full hour (this once shifted the FR-5 daytime window on the very
    /// evaluation that persists its once-ever flag).
    pub unix_secs: i64,
}

/// Read the OS local time.
pub fn local_now() -> LocalNow {
    #[cfg(unix)]
    {
        unix_local_now().unwrap_or_else(utc_fallback_now)
    }

    #[cfg(windows)]
    {
        let (hour, day, unix_secs) = windows_local_now();
        LocalNow {
            hour,
            day,
            unix_secs,
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        utc_fallback_now()
    }
}

/// Local hour and day from libc `localtime_r`, declared inline so no new
/// dependency is added. The day counter is local-consistent: the epoch
/// seconds are shifted by the timezone offset before dividing, so daily
/// caps roll over at local midnight. Returns `None` if the call fails.
#[cfg(unix)]
fn unix_local_now() -> Option<LocalNow> {
    // Matches `struct tm` on 64-bit Linux (glibc) and macOS.
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }

    type TimeT = i64;

    extern "C" {
        fn time(t: *mut TimeT) -> TimeT;
        fn localtime_r(timep: *const TimeT, result: *mut Tm) -> *mut Tm;
    }

    let now = unsafe { time(std::ptr::null_mut()) };
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    // localtime_r returns the result pointer on success, NULL on failure.
    let ok = unsafe { !localtime_r(&now, &mut tm).is_null() };
    if ok {
        let hour = tm.tm_hour.clamp(0, 23) as u8;
        let local_secs = now + tm.tm_gmtoff;
        let day = local_secs.div_euclid(86_400) as u32;
        // `now` is the single time(2) read; both the local slice and the unix
        // instant come from it.
        Some(LocalNow {
            hour,
            day,
            unix_secs: now,
        })
    } else {
        None
    }
}

/// Local hour, day index (days since local epoch), and unix instant from
/// WinAPI. One clock read: the unix instant is derived from the same
/// GetLocalTime civil time via the zone bias, not from a second clock read.
#[cfg(windows)]
fn windows_local_now() -> (u8, u32, i64) {
    #[repr(C)]
    struct SystemTime {
        w_year: u16,
        w_month: u16,
        w_day_of_week: u16,
        w_day: u16,
        w_hour: u16,
        w_minute: u16,
        w_second: u16,
        w_milliseconds: u16,
    }

    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }

    extern "system" {
        fn GetLocalTime(lp_system_time: *mut SystemTime);
        fn GetTimeZoneInformation(lptimezoneinformation: *mut TimeZoneInformation) -> u32;
    }

    let zeroed = SystemTime {
        w_year: 0,
        w_month: 0,
        w_day_of_week: 0,
        w_day: 0,
        w_hour: 0,
        w_minute: 0,
        w_second: 0,
        w_milliseconds: 0,
    };
    let mut st = SystemTime {
        w_year: 0,
        w_month: 0,
        w_day_of_week: 0,
        w_day: 0,
        w_hour: 0,
        w_minute: 0,
        w_second: 0,
        w_milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    let hour = st.w_hour.clamp(0, 23) as u8;
    let day = days_from_civil(st.w_year as i64, st.w_month as i64, st.w_day as i64) as u32;

    // UTC = local + bias minutes (daylight bias applies when the zone reports
    // TIME_ZONE_ID_DAYLIGHT == 2). A zone read is not a clock read, so this
    // keeps the local slice and the unix instant on the same instant.
    let mut tzi = TimeZoneInformation {
        bias: 0,
        standard_name: [0; 32],
        standard_date: zeroed,
        standard_bias: 0,
        daylight_name: [0; 32],
        daylight_date: zeroed,
        daylight_bias: 0,
    };
    let zone_id = unsafe { GetTimeZoneInformation(&mut tzi) };
    let in_daylight = zone_id == 2;
    let bias_min = tzi.bias
        + if in_daylight {
            tzi.daylight_bias
        } else {
            tzi.standard_bias
        };
    let local_as_utc = days_from_civil(st.w_year as i64, st.w_month as i64, st.w_day as i64)
        * 86_400
        + st.w_hour as i64 * 3_600
        + st.w_minute as i64 * 60
        + st.w_second as i64;
    (hour, day, local_as_utc - bias_min as i64 * 60)
}

/// Days since 1970-01-01 from a civil date (Hinnant's days_from_civil).
#[cfg(windows)]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// No libc/WinAPI available (non-unix, non-Windows builds): fall back to UTC.
/// Quiet hours then follow UTC; acceptable for a host that has neither of
/// the two supported platform APIs.
fn utc_fallback_now() -> LocalNow {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    LocalNow {
        hour: ((secs / 3_600) % 24) as u8,
        day: (secs / 86_400).max(0) as u32,
        unix_secs: secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hour_is_in_range() {
        let now = local_now();
        assert!(now.hour <= 23);
    }

    #[test]
    fn day_is_plausibly_recent() {
        // 2026-01-01 is day ~20454 since the epoch.
        assert!(local_now().day > 20_000);
    }
}
