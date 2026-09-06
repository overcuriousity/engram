//! Instants and spans, in the words a person reads them in.
//!
//! Here rather than in `web::ui`, which is where all of this grew, because
//! two of its callers are not the web at all: `core::search` stamps a hit's
//! `due_in`, and the terminal client's `--status` prints when a reminder
//! lands. Both were reaching into a page module for a date formatter, which
//! made a ranking pipeline and a TUI depend on the HTTP layer to print
//! "in 2 days".
//!
//! Nothing here touches a template, a request or a store row. The clock is
//! the one exception, and only `ago` and `ago_or_ahead` read it.

/// A wait, coarsely. "in 4h" is the whole of what a reader needs from a backoff
/// — the exact second is noise, and the point of the line is that nobody has to
/// do anything about it.
pub fn fmt_duration(secs: i64) -> String {
    match secs {
        s if s <= 0 => "now".into(),
        s if s < 90 => format!("in {s}s"),
        s if s < 5400 => format!("in {}m", (s + 59) / 60),
        s => format!("in {}h", (s + 3599) / 3600),
    }
}

/// How long something took, past tense.
///
/// `fmt_duration` above answers a different question — when does this run next
/// — and says "now" for zero and "in 5m" for three hundred. Housekeeping spent
/// it on the TOOK column, so every sweep in the history claimed to have taken
/// "now", and a sweep that genuinely ran for five minutes would have claimed
/// to be about to happen.
pub fn fmt_elapsed(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// Unix seconds as an ISO-ish UTC stamp, computed directly so the project does
/// not pull in a date library for one display string.
pub fn fmt_time(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant's algorithm), epoch shifted to 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Roughly how long ago, in the words someone would use out loud. Precision
/// past "days" would suggest the timestamp matters; it is here to jog a memory.
pub(crate) fn ago(then: i64) -> String {
    let days = (crate::store::now() - then).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        n if n < 30 => format!("{n} days ago"),
        n => format!("{} months ago", n / 30),
    }
}

/// A due time relative to now, either way: "in 2 h", "in 3 days", "1 h ago".
/// Hours under a day, days from there; a reminder's precision.
pub(crate) fn ago_or_ahead(at: i64) -> String {
    // Saturating, and `abs` on the saturated value: `i64::MIN.abs()` panics
    // under overflow checks, and a row is not a place to find that out. The
    // door refuses such an instant (`api::set_moment`); a row already stored
    // is still drawn.
    let delta = at.saturating_sub(crate::store::now());
    let (ahead, span) = (delta >= 0, delta.saturating_abs());
    let words = match span {
        s if s < 3_600 => "under an hour".to_string(),
        s if s < 86_400 => format!("{} h", s / 3_600),
        s => format!(
            "{} day{}",
            s / 86_400,
            if s / 86_400 == 1 { "" } else { "s" }
        ),
    };
    match ahead {
        true => format!("in {words}"),
        false => format!("{words} ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_that_took_no_time_does_not_say_it_happens_now() {
        // Every row of Housekeeping's TOOK column read "now", because the
        // column spends `fmt_duration` — which answers when something runs
        // next, not how long it took.
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(3), "3s");
        assert_eq!(fmt_elapsed(75), "1m 15s");
        assert_eq!(fmt_elapsed(3600), "1h 0m");
        assert_eq!(fmt_elapsed(-5), "0s", "a clock that went backwards");
        // And the future-tense helper keeps its own meaning.
        assert_eq!(fmt_duration(0), "now");
        assert_eq!(fmt_duration(300), "in 5m");
    }

    #[test]
    fn timestamps_render_as_a_readable_date() {
        // 2026-04-08T07:00:00Z
        assert_eq!(fmt_time(1_775_631_600), "2026-04-08 07:00");
        assert_eq!(fmt_time(0), "1970-01-01 00:00");
    }
}
