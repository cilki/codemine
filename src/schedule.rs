//! The optional daily window turns may start in. Off by default; when on,
//! the main loop holds turns until the local clock is back inside it. The
//! window is read against the process's timezone, so a deployment picks its
//! quiet hours with `TZ` (or the container's `/etc/localtime`) and thinks in
//! wall-clock time.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

const MINUTES_PER_DAY: u32 = 24 * 60;
const SECONDS_PER_DAY: u32 = MINUTES_PER_DAY * 60;

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct Schedule {
    /// Off by default: turns run around the clock until this is turned on.
    pub enabled: bool,
    /// Minutes past local midnight the window opens and closes. A start
    /// after the end wraps midnight, which is the usual shape of it — 22:00
    /// to 06:00 is one window, not two.
    pub start_minute: u32,
    pub end_minute: u32,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            enabled: false,
            start_minute: 22 * 60,
            end_minute: 6 * 60,
        }
    }
}

impl Schedule {
    pub fn validate(&self) -> Result<()> {
        for minute in [self.start_minute, self.end_minute] {
            if minute >= MINUTES_PER_DAY {
                bail!("schedule times must be within a day");
            }
        }
        if self.enabled && self.start_minute == self.end_minute {
            bail!("schedule start and end must differ");
        }
        Ok(())
    }

    /// How long to hold turns, in seconds, or None when one may start: the
    /// schedule is off, or `second` (past local midnight) is inside the
    /// window. A hand-edited config whose start equals its end is taken as
    /// the whole day rather than none of it, so a bad edit can't silently
    /// stop the runner forever.
    pub fn hold(&self, second: u32) -> Option<u64> {
        if !self.enabled || self.start_minute == self.end_minute {
            return None;
        }
        let start = self.start_minute * 60;
        let end = self.end_minute * 60;
        let open = match start < end {
            true => (start..end).contains(&second),
            false => second >= start || second < end,
        };
        if open {
            return None;
        }
        // Time to the next opening, wrapping over midnight; the zero case
        // can't arrive here, since the moment the window opens is open.
        Some(((start + SECONDS_PER_DAY - second) % SECONDS_PER_DAY) as u64)
    }
}

/// Seconds past midnight in the process's timezone.
pub fn local_second_of_day() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let time = now as libc::time_t;
    // The zone comes from $TZ (or /etc/localtime), which libc loads on the
    // first conversion and keeps; a deployment sets it once at startup.
    // SAFETY: every field of tm is an integer or a pointer, all of which
    // are valid zeroed, and localtime_r overwrites it anyway.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r reads `time` and writes our own `tm`, both live
    // for the call.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        // Only a broken zone database gets here; UTC beats no schedule.
        return (now % SECONDS_PER_DAY as u64) as u32;
    }
    let second = tm.tm_hour.max(0) as u32 * 3600
        + tm.tm_min.max(0) as u32 * 60
        + tm.tm_sec.max(0) as u32;
    // A leap second lands on 60, which belongs to the day that is ending.
    second.min(SECONDS_PER_DAY - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32) -> u32 {
        hour * 3600 + minute * 60
    }

    fn schedule(start: u32, end: u32) -> Schedule {
        Schedule {
            enabled: true,
            start_minute: start,
            end_minute: end,
        }
    }

    #[test]
    fn a_disabled_schedule_never_holds() {
        let mut off = schedule(22 * 60, 6 * 60);
        off.enabled = false;
        assert_eq!(off.hold(at(12, 0)), None);
        assert_eq!(Schedule::default().hold(at(12, 0)), None);
    }

    #[test]
    fn a_daytime_window_holds_outside_it() {
        let day = schedule(9 * 60, 17 * 60);
        assert_eq!(day.hold(at(9, 0)), None);
        assert_eq!(day.hold(at(16, 59)), None);
        // The closing minute is out, and reopens tomorrow morning.
        assert_eq!(day.hold(at(17, 0)), Some(16 * 3600));
        assert_eq!(day.hold(at(8, 0)), Some(3600));
    }

    #[test]
    fn an_overnight_window_wraps_midnight() {
        let night = schedule(22 * 60, 6 * 60);
        assert_eq!(night.hold(at(22, 0)), None);
        assert_eq!(night.hold(at(23, 30)), None);
        assert_eq!(night.hold(at(0, 0)), None);
        assert_eq!(night.hold(at(5, 59)), None);
        assert_eq!(night.hold(at(6, 0)), Some(16 * 3600));
        assert_eq!(night.hold(at(21, 30)), Some(1800));
    }

    #[test]
    fn an_edited_degenerate_window_runs_all_day() {
        assert_eq!(schedule(60, 60).hold(at(3, 0)), None);
    }

    #[test]
    fn validation_rejects_bad_windows() {
        assert!(schedule(22 * 60, 6 * 60).validate().is_ok());
        assert!(schedule(0, 24 * 60).validate().is_err());
        assert!(schedule(60, 60).validate().is_err());
        let mut off = schedule(60, 60);
        off.enabled = false;
        // A disabled schedule is never consulted, so its times may match.
        assert!(off.validate().is_ok());
    }

    #[test]
    fn the_local_clock_lands_inside_a_day() {
        assert!(local_second_of_day() < SECONDS_PER_DAY);
    }
}
