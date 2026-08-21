//! The situation a page view happened in: the clock it happened on, the bundle
//! the browser sent, and the fixed-length vector both become.
//!
//! Everything here is a function of its arguments. That is deliberate: the
//! encoder is the one part of this feature that can be silently wrong — a block
//! that quietly outweighs another produces plausible recommendations for the
//! wrong reason — and a pure function is the only shape that can be pinned by a
//! table of cases.

/// Where this feature reads the time. `System` everywhere but the tests, which
/// need a seventh Friday at 14:52 to exist on demand.
///
/// Held by the sweep, the encoder's entry point and the endpoint, and by
/// nothing else. The other `now()` call sites in the tree are not touched:
/// they work, and rewriting them would be a diff across the tree for nothing.
#[derive(Debug, Clone, Copy)]
pub enum Clock {
    System,
    Fixed(i64),
}

impl Clock {
    pub fn now(&self) -> i64 {
        match self {
            Clock::System => crate::store::now(),
            Clock::Fixed(t) => *t,
        }
    }
}

/// A moment as the device experienced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTime {
    /// Fractional, 0.0..24.0 — 15:30 is 15.5. Fractional because the hour is
    /// encoded as an angle, and rounding to the hour would put 14:55 and 15:05
    /// ten minutes apart on a circle they are five minutes apart on.
    pub hour: f32,
    /// 0 = Monday.
    pub weekday: u32,
    /// 1-based day of the month.
    pub day: u32,
    /// 1-based. Only the display reads it — "like 08.08., 15:04" — but it is
    /// derived here because this is the one place that knows which zone the
    /// moment is being read in.
    pub month: u32,
    pub days_in_month: u32,
}

/// The device's own reading of `at`.
///
/// The zone comes from the client, never from config:
/// `Intl.DateTimeFormat().resolvedOptions().timeZone` is correct per device and
/// carries DST with it, which a stored offset cannot. The offset is the fallback
/// for a browser that reports one and no zone; UTC is the fallback for a row
/// that has neither, which is every row written before this feature existed.
/// That last case is not a pretence that the operator lives in London — it is
/// the only reading available, and §12 leans on it: an old event still carries
/// a weekday and an hour, and those two blocks are what stand from the first
/// sweep.
pub fn local_time(at: i64, tz: Option<&str>, offset_mins: Option<i32>) -> LocalTime {
    use chrono::{DateTime, Datelike, TimeZone, Timelike};

    let utc = DateTime::from_timestamp(at, 0).unwrap_or_default();
    let naive = match tz.and_then(|z| z.parse::<chrono_tz::Tz>().ok()) {
        Some(z) => z.from_utc_datetime(&utc.naive_utc()).naive_local(),
        None => match offset_mins.and_then(|m| chrono::FixedOffset::east_opt(m * 60)) {
            Some(o) => o.from_utc_datetime(&utc.naive_utc()).naive_local(),
            None => utc.naive_utc(),
        },
    };
    LocalTime {
        hour: naive.hour() as f32 + naive.minute() as f32 / 60.0,
        weekday: naive.weekday().num_days_from_monday(),
        day: naive.day(),
        month: naive.month(),
        days_in_month: days_in_month(naive.year(), naive.month()),
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    use chrono::NaiveDate;
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next = NaiveDate::from_ymd_opt(y, m, 1);
    match (first, next) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        // Unreachable for a date chrono just produced; 30 rather than a panic,
        // because a month length is a scaling factor for one block worth 0.0 by
        // default and nothing here is worth taking a page view down for.
        _ => 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_does_not_move() {
        assert_eq!(Clock::Fixed(1_000).now(), 1_000);
        assert_eq!(Clock::Fixed(1_000).now(), 1_000);
    }

    // 2026-08-21T13:52:00Z is a Friday.
    const FRIDAY_1352_UTC: i64 = 1_787_320_320;

    #[test]
    fn an_iana_zone_beats_a_stored_offset() {
        // Berlin is UTC+2 in August. The offset argument is deliberately
        // wrong: a zone, when there is one, is the authority.
        let t = local_time(FRIDAY_1352_UTC, Some("Europe/Berlin"), Some(0));
        assert_eq!(t.weekday, 4, "Friday");
        assert!((t.hour - 15.866_666).abs() < 0.001, "15:52, got {}", t.hour);
    }

    #[test]
    fn an_offset_answers_when_there_is_no_zone() {
        let t = local_time(FRIDAY_1352_UTC, None, Some(120));
        assert!((t.hour - 15.866_666).abs() < 0.001, "got {}", t.hour);
    }

    #[test]
    fn an_unknown_zone_falls_back_rather_than_failing() {
        // A device can send anything. Nothing here may panic on it, and UTC is
        // the honest answer rather than a guess.
        let t = local_time(FRIDAY_1352_UTC, Some("Mars/Olympus"), None);
        assert!((t.hour - 13.866_666).abs() < 0.001, "got {}", t.hour);
    }

    #[test]
    fn the_month_is_carried_with_its_length() {
        let t = local_time(FRIDAY_1352_UTC, Some("UTC"), None);
        assert_eq!(t.day, 21);
        assert_eq!(t.month, 8);
        assert_eq!(t.days_in_month, 31);
    }
}
