//! What a judged synthesis reply becomes: moments, a journal filing, links.
//!
//! The one reader of time since the 2026-09 capture reshape. The synthesis
//! call that rewrites a small capture also judges it — reminder, journal
//! entry, or plain note, with the dates it names and the artifacts it relates
//! to — and this module writes those judgements down. Everything here is
//! best-effort against artifacts that already stand: a judgement that cannot
//! be applied is a warning, never a lost capture.

use crate::core::Core;
use crate::core::moments::{
    DEFAULT_HOUR, Intent, default_zone_name, intent_refused, validate_rule, zone,
};
use crate::error::Result;
use crate::infer::Judgement;
use crate::store::moments::{Kind, NewMoment, Source};

/// Apply one judgement to the capture it was made about.
///
/// `anchor_id` is the first live synthesized artifact — where the moments
/// hang, the way the old stage hung them on the first artifact. `shown` is
/// the neighbor ids the model was actually shown: a link to anything else is
/// dropped, because the model can only relate what was on the table.
///
/// Idempotent per re-synthesis: read rows are replaced, done and set rows
/// are kept, and an operator's refusal (`intent_refused`) outlives any
/// number of re-reads.
pub async fn apply(
    core: &Core,
    corpus_id: &str,
    anchor_id: &str,
    j: &Judgement,
    shown: &[String],
    window_text: &str,
) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let tz_name = src.metadata["tz"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| default_zone_name(&core.time.default_tz));
    let tz = zone(Some(&tz_name));
    // The zone as the zone table spells it — see the due band, which reads
    // the wall-clock back out of what is stored here.
    let tz_name = tz.name().to_string();

    // The note's own weekday, read once for the reminder below. Not for the
    // events: the weekday witness corrects a date the model computed for the
    // reminder, and a note reading "Friday I pick up the car; the concert is
    // 2026-09-12" states that second date outright. Applied to the events it
    // rewrote every one of them onto the same Friday, past dates included.
    // See `onto_named_weekday`.
    //
    // Read from the *judged window* and not from `src.raw_text`. This runs once
    // per window, and the corpus is every window at once: a weekday word in a
    // lecture's third page corrected the reminder read out of its tenth, which
    // is a witness to nothing. The window is what the model was shown and what
    // it answered about, so it is the only text whose weekday says anything
    // about the date that came back.
    let named_day = weekday_named(window_text);
    let created_at = src.created_at;
    let reconcile = move |at: i64| match named_day {
        // Far enough out that the weekday word cannot be where the date came
        // from. The correction exists for "Freitag", captured on a Wednesday
        // and resolved to a Saturday — one wrong step of calendar arithmetic
        // over a date the note does not spell out. A note that also states
        // "Antragsfrist: 2026-11-30" gives the model nothing to compute, and
        // moving *that* onto the coming Friday is not a correction, it is
        // three months of the reminder being wrong in the one direction that
        // fires early and then never again. Past the horizon the model's date
        // stands, because past the horizon it is the note's date.
        Some(_) if at > created_at + WITNESS_HORIZON => at,
        Some(d) => {
            let moved = onto_named_weekday(at, d, created_at, tz);
            if moved != at {
                tracing::warn!(
                    corpus_id,
                    from = at,
                    to = moved,
                    weekday = ?d,
                    "the model's date fell on another weekday than the note names; moved"
                );
            }
            moved
        }
        None => at,
    };

    // Recorded before anything is withdrawn, so a store error here costs
    // nothing: `?` used to abort `apply` with the previous reading already
    // deleted and no replacement written.
    record_intent(core, corpus_id, &src, j.intent.as_deref()).await?;
    // Events only. The due rows are withdrawn where the new reading actually
    // replaces them — see the `remind` arm below — because several paths
    // through it decide the reading names no reminder they can file and
    // `return`, and a delete up here meant each of those destroyed a standing
    // reminder and put nothing back. A window retry whose second reply is
    // vaguer than the first is enough to walk into one.
    core.store.delete_read_events(anchor_id).await?;

    // Dates the note states without being the reminder: the day page's rows.
    //
    // Each of the three sections below is independent, and each one fails on
    // its own. They used to fail on each other: `?` here aborted `apply`, and
    // the reminder — the section this call is most often made for — sits last.
    // `shown` comes out of vector payloads, which can name a row the reaper or
    // a supersession has since taken away, so one foreign-key error on a link
    // to a dead id silently cost the whole judgement. Best-effort is what the
    // caller already assumes: it logs a failed `apply` and lets the artifacts
    // stand.
    for e in &j.events {
        let Some(at) = parse_local(e, tz) else {
            continue;
        };
        match core
            .store
            .has_moment_at(anchor_id, Kind::Event, Some(at))
            .await
        {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(corpus_id, error = %err, "could not check for an existing event");
                continue;
            }
        }
        if let Err(err) = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: anchor_id.into(),
                kind: Kind::Event,
                at: Some(at),
                tz: tz_name.clone(),
                rule: None,
                source: Source::Classified,
                span: None,
            })
            .await
        {
            tracing::warn!(corpus_id, error = %err, "could not record a judged event");
        }
    }

    // Relations to what the model was shown. Dedup and supersession stay
    // with the sweeps; the model proposes no merges.
    for l in &j.links {
        if l.artifact_id == anchor_id || !shown.iter().any(|s| s == &l.artifact_id) {
            continue;
        }
        if let Err(err) = core
            .store
            .relate_synthesized(anchor_id, &l.artifact_id, &l.reason)
            .await
        {
            tracing::warn!(corpus_id, other = %l.artifact_id, error = %err, "could not record a judged link");
        }
    }

    let forced = src.metadata["intent"].as_str();
    match j.intent.as_deref() {
        Some("journal")
            if JOURNALABLE.contains(&src.origin.as_str())
                && !intent_refused(&src.metadata, Intent::Journal) =>
        {
            // The reading says this is an entry and not a reminder, so the
            // reminder the previous reading filed is withdrawn — the delete
            // that used to happen unconditionally at the top of `apply`, moved
            // to the one arm that is actually saying it.
            core.store.delete_read_due(anchor_id).await?;
            core.set_entry(corpus_id, true).await?;
        }
        Some("remind") => {
            if intent_refused(&src.metadata, Intent::Remind) {
                // The operator has said this is not a reminder. Their word,
                // not the model's, and it takes the read rows with it.
                core.store.delete_read_due(anchor_id).await?;
                return Ok(());
            }
            let at = j
                .when
                .as_deref()
                .and_then(|w| parse_local(w, tz))
                .map(reconcile);
            let valid_rule = j
                .rule
                .clone()
                .filter(|r| match validate_rule(r) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(rule = %r, error = %e, "rule outside the subset; the reminder is single");
                        false
                    }
                });
            // A date the rule carries and `when` does not. The rule below may
            // be dropped as a single occurrence, and dropping it threw away
            // the only date the answer had: `when: null` with
            // `FREQ=WEEKLY;BYDAY=FR;COUNT=1` left `at` and `rule` both unset
            // and the reminder was filed away as an ordinary capture.
            let at = at.or_else(|| {
                valid_rule
                    .as_deref()
                    .and_then(|r| first_occurrence(r, src.created_at, tz))
            });
            let rule = valid_rule
                // A rule that yields one occurrence is not a repetition, it is
                // the date `when` already carries. Asked to judge "Freitag
                // 13:45" the configured model answers
                // `FREQ=WEEKLY;BYDAY=FR;COUNT=1`, and stored, that made the
                // band offer to repeat a note that never said it repeats.
                .filter(|r| !single_occurrence(r));
            // A judged reminder with no date anywhere stays an ordinary
            // capture — a guess about a note that names no time used to
            // become an undated row nagging for a date it never had. A
            // *forced* remind is somebody saying "remind me" at the door,
            // and an undated one is a question the band asks them.
            let forced_remind = forced == Some("remind");
            if at.is_none() && rule.is_none() && !forced_remind {
                // And the previous reading stands. This is the arm the window
                // retry walks into when its second reply is vaguer than the
                // first: "a reminder, but I cannot date it" is not a statement
                // that the date already on the artifact was wrong, and taking
                // the standing reminder away on the strength of it was a
                // silent loss with a `debug!` line for a record.
                tracing::debug!(
                    corpus_id,
                    "a judged reminder with no date is left as a capture"
                );
                return Ok(());
            }
            // Undated included: `None` is an instant the guard understands,
            // and a finished undated reminder is exactly the row
            // `delete_read_due` keeps and this must not read back fresh. It
            // now also catches the previous reading landing on the same
            // instant, which is the cheapest possible answer to a re-read that
            // changes nothing: no delete, no insert, no churn.
            if core.store.has_moment_at(anchor_id, Kind::Due, at).await? {
                return Ok(());
            }
            // And a date the operator moved outranks this reading of the prose
            // it came from, whatever the reading is this time. `has_moment_at`
            // only catches a re-read landing back on the instant they moved
            // away from; a third reading put a second open row beside the
            // correction, and both of them pushed.
            if core.store.has_moved_moment(anchor_id, Kind::Due).await? {
                tracing::debug!(
                    corpus_id,
                    "the reminder on this artifact was moved by hand; the re-read adds nothing"
                );
                return Ok(());
            }
            // Every guard is past and this reading has a reminder to file, so
            // now the previous one is genuinely replaced rather than merely
            // discarded. Delete and insert, in that order, so no instant in
            // between leaves the artifact with two open readings of the same
            // prose.
            core.store.delete_read_due(anchor_id).await?;
            core.store
                .insert_moment(&NewMoment {
                    artifact_id: anchor_id.into(),
                    kind: Kind::Due,
                    at,
                    tz: tz_name.clone(),
                    rule,
                    source: if forced_remind {
                        Source::Cue
                    } else {
                        Source::Classified
                    },
                    span: None,
                })
                .await?;
            core.store.rearm_remind().await?;
            let art = core.store.get_artifact(anchor_id).await?;
            if let Err(e) = confirm_created(core, &art, at, tz).await {
                // Best-effort: a note that failed to say "reminder set" is
                // still a reminder that was set.
                tracing::warn!(error = %e, "could not push the capture-time confirmation");
            }
        }
        // Every other reading — an intent of `none`, or a `journal` on an
        // origin that may not be filed as one — says outright that this note
        // is not a reminder, and the previous reading's rows go with it. The
        // arms above are the two that had something of their own to say first.
        _ => {
            core.store.delete_read_due(anchor_id).await?;
        }
    }
    Ok(())
}

/// How far past the capture a weekday the note names is still evidence about
/// the date the model returned.
///
/// One week, because that is the reach of the word: "Freitag" said on a
/// Wednesday means the Friday three days out, and no speaker of any of the ten
/// prompt languages means the Friday in November by it. A date beyond this is
/// one the note stated outright or the model read off something it stated, and
/// the weekday standing beside it is describing a different sentence.
const WITNESS_HORIZON: i64 = 7 * 86_400;

/// Origins the judgement may file as a journal entry. Not `api` or `mcp`:
/// a program that wanted an entry says so with `origin`.
pub const JOURNALABLE: &[&str] = &[
    crate::core::ingest::ORIGIN_WEB,
    "ui",
    "cli",
    crate::core::ingest::ORIGIN_SHARE,
    "extension",
];

/// Write down what was read. Cheap, and it makes every later argument about
/// a filing a measurement instead of an opinion.
async fn record_intent(
    core: &Core,
    corpus_id: &str,
    src: &crate::store::corpora::Corpus,
    intent: Option<&str>,
) -> Result<()> {
    let mut meta = src.metadata.clone();
    meta["intent_read"] = serde_json::Value::String(intent.unwrap_or("none").to_string());
    meta["intent_by"] = serde_json::Value::String("synthesis".to_string());
    if let Some(m) = meta.as_object_mut() {
        m.remove("intent_score");
    }
    core.store.set_corpus_metadata(corpus_id, &meta).await
}

/// The first instant a rule names after the capture, at `DEFAULT_HOUR`.
///
/// Only for a judgement whose `when` is null: the rule then carries the only
/// date in the answer, and the time of day is the one the prompt names for a
/// note that states none. `next_after` is strict, so a rule naming the
/// capture's own weekday means the next one — the reading
/// `onto_named_weekday` already takes.
fn first_occurrence(rule: &str, created_at: i64, tz: chrono_tz::Tz) -> Option<i64> {
    use chrono::TimeZone;
    let day = tz.timestamp_opt(created_at, 0).single()?.date_naive();
    let anchor = crate::core::moments::resolve_local(day.and_hms_opt(DEFAULT_HOUR, 0, 0)?, tz)?;
    crate::core::moments::next_after(rule, anchor, tz)
}

/// Does this RRULE describe exactly one occurrence?
///
/// `COUNT=1` says so outright, and it is the shape a model reaches for when it
/// is asked for a rule and has only the one date to give. Nothing else is
/// decided here: a rule whose `UNTIL` happens to leave one occurrence is still
/// a note that says it repeats, and the recurrence code reads it.
fn single_occurrence(rule: &str) -> bool {
    rule.split(';').any(|part| {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        k.trim().eq_ignore_ascii_case("COUNT") && v.trim() == "1"
    })
}

/// The push that says a reminder was just set, at capture time and
/// independent of the due-time ladder.
async fn confirm_created(
    core: &Core,
    art: &crate::store::artifacts::Chunk,
    at: Option<i64>,
    tz: chrono_tz::Tz,
) -> Result<()> {
    let opening = art
        .text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect::<String>();
    let title = art
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| opening.clone());
    let message = match at {
        Some(at) => format!(
            "{opening}\n{}",
            crate::web::due::when_words(at, core.clock.now(), tz)
        ),
        None => opening,
    };
    crate::jobs::remind::notify_now(core, &format!("Reminder set: {title}"), &message).await
}

/// `2026-09-04T09:00` or `2026-09-04` in the reader's zone — and the same
/// instants carrying an offset of their own.
///
/// The prompt asks for a local wall clock, and the schema puts no `pattern` on
/// `when`, so it does not get one. A model that answers `2026-09-04T09:00Z` or
/// `…+02:00` — which is what a model does the moment anything in its context
/// looks like an ISO instant — parsed as nothing at all, and the reminder was
/// dropped in silence. An offset that is stated is honoured rather than
/// discarded: it says what instant was meant, and the reader's zone is then
/// none of the answer's business.
pub(crate) fn parse_local(s: &str, tz: chrono_tz::Tz) -> Option<i64> {
    let s = s.trim();
    match split_offset(s) {
        Some((head, off)) => {
            use chrono::TimeZone;
            Some(off.from_local_datetime(&naive(head)?).single()?.timestamp())
        }
        None => crate::core::moments::resolve_local(naive(s)?, tz),
    }
}

/// The weekday a note names, in any of the ten prompt languages.
///
/// `None` when it names none, and `None` when it names two different ones —
/// "Montag oder Freitag" is a question, not a date. Whole words only: the
/// text is split on anything that is not a letter, digit or hyphen, so
/// "sundays" is not "sunday" and "monday-ish" is not a date either. The
/// hyphen stays a word character for the Portuguese "sexta-feira".
pub(crate) fn weekday_named(text: &str) -> Option<chrono::Weekday> {
    use chrono::Weekday::*;
    #[rustfmt::skip]
    const NAMES: &[(&str, chrono::Weekday)] = &[
        ("monday", Mon), ("montag", Mon), ("lunes", Mon), ("lundi", Mon), ("lunedì", Mon),
        ("lunedi", Mon), ("maandag", Mon), ("poniedziałek", Mon), ("segunda-feira", Mon),
        ("segunda", Mon), ("понедельник", Mon), ("pazartesi", Mon),
        ("tuesday", Tue), ("dienstag", Tue), ("martes", Tue), ("mardi", Tue), ("martedì", Tue),
        ("martedi", Tue), ("dinsdag", Tue), ("wtorek", Tue), ("terça-feira", Tue), ("terça", Tue),
        ("вторник", Tue), ("salı", Tue),
        ("wednesday", Wed), ("mittwoch", Wed), ("miércoles", Wed), ("miercoles", Wed),
        ("mercredi", Wed), ("mercoledì", Wed), ("mercoledi", Wed), ("woensdag", Wed), ("środa", Wed),
        ("quarta-feira", Wed), ("quarta", Wed), ("среда", Wed), ("çarşamba", Wed),
        ("thursday", Thu), ("donnerstag", Thu), ("jueves", Thu), ("jeudi", Thu), ("giovedì", Thu),
        ("giovedi", Thu), ("donderdag", Thu), ("czwartek", Thu), ("quinta-feira", Thu),
        ("quinta", Thu), ("четверг", Thu), ("perşembe", Thu),
        ("friday", Fri), ("freitag", Fri), ("viernes", Fri), ("vendredi", Fri), ("venerdì", Fri),
        ("venerdi", Fri), ("vrijdag", Fri), ("piątek", Fri), ("sexta-feira", Fri), ("sexta", Fri),
        ("пятница", Fri), ("cuma", Fri),
        ("saturday", Sat), ("samstag", Sat), ("sonnabend", Sat), ("sábado", Sat), ("sabado", Sat),
        ("samedi", Sat), ("sabato", Sat), ("zaterdag", Sat), ("sobota", Sat), ("суббота", Sat),
        ("cumartesi", Sat),
        ("sunday", Sun), ("sonntag", Sun), ("domingo", Sun), ("dimanche", Sun), ("domenica", Sun),
        ("zondag", Sun), ("niedziela", Sun), ("воскресенье", Sun), ("pazar", Sun),
    ];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    let mut found: Option<chrono::Weekday> = None;
    for (name, day) in NAMES {
        if words.iter().any(|w| w == name) {
            if found.is_some_and(|f| f != *day) {
                return None;
            }
            found = Some(*day);
        }
    }
    found
}

/// The instant the model resolved, moved onto the weekday the note names.
///
/// The model does the calendar arithmetic and gets it wrong by a day often
/// enough — "Freitag" on a Wednesday came back as the Saturday. The note
/// itself is the stronger witness: when it names a weekday and the resolved
/// instant falls on another, the date becomes the first such weekday after
/// the capture, at the time of day the model resolved. A weekday that is the
/// capture's own day means next week. `at` unchanged when the two agree or
/// the zone cannot place either instant.
pub(crate) fn onto_named_weekday(
    at: i64,
    named: chrono::Weekday,
    now: i64,
    tz: chrono_tz::Tz,
) -> i64 {
    use chrono::{Datelike, Duration, TimeZone};
    let Some(local) = tz.timestamp_opt(at, 0).single() else {
        return at;
    };
    if local.weekday() == named {
        return at;
    }
    let Some(today) = tz.timestamp_opt(now, 0).single().map(|d| d.date_naive()) else {
        return at;
    };
    let ahead = (i64::from(named.num_days_from_monday())
        - i64::from(today.weekday().num_days_from_monday()))
    .rem_euclid(7);
    let ahead = if ahead == 0 { 7 } else { ahead };
    let date = today + Duration::days(ahead);
    tz.from_local_datetime(&date.and_time(local.time()))
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(at)
}

/// The three wall-clock spellings, with a bare date meaning `DEFAULT_HOUR`.
fn naive(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(DEFAULT_HOUR, 0, 0).unwrap())
        })
        .ok()
}

/// A trailing `Z`, `+02:00` or `-0500`, split from the wall clock in front of
/// it. The sign is searched for after the `T` so that the date's own dashes
/// cannot be mistaken for one.
fn split_offset(s: &str) -> Option<(&str, chrono::FixedOffset)> {
    if let Some(head) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((head, chrono::FixedOffset::east_opt(0)?));
    }
    let time_at = s.find(['T', 't'])?;
    let at = s[time_at..].rfind(['+', '-'])? + time_at;
    let (head, tail) = s.split_at(at);
    Some((head, tail.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::infer::fake::FAKE_BUDGET;
    use crate::infer::{
        ProposedArtifact, ProposedLink, SegmentInput, SegmentReply, SynthesisBudget, Synthesizer,
    };
    use crate::jobs::test_support::drain;
    use async_trait::async_trait;

    #[test]
    fn a_weekday_is_read_in_any_of_the_ten_languages_and_only_when_unambiguous() {
        use chrono::Weekday;
        assert_eq!(
            weekday_named("erinnere mich an den Termin, Freitag 13:45 uhr."),
            Some(Weekday::Fri)
        );
        assert_eq!(
            weekday_named("call the dentist on Tuesday"),
            Some(Weekday::Tue)
        );
        assert_eq!(weekday_named("rappelle-moi jeudi"), Some(Weekday::Thu));
        assert_eq!(weekday_named("cuma günü toplantı"), Some(Weekday::Fri));
        assert_eq!(weekday_named("pazartesi sabah"), Some(Weekday::Mon));
        assert_eq!(
            weekday_named("lembra-me na sexta-feira"),
            Some(Weekday::Fri)
        );
        // Two different days named: no single answer.
        assert_eq!(weekday_named("Montag oder Freitag"), None);
        // No day named.
        assert_eq!(weekday_named("morgen um 9"), None);
        // A weekday inside another word is not a weekday.
        assert_eq!(weekday_named("the monday-ish feeling"), None);
        assert_eq!(weekday_named("sundays"), None);
    }

    #[test]
    fn a_resolved_date_is_moved_onto_the_named_weekday() {
        use chrono::{TimeZone, Weekday};
        let tz = chrono_tz::Europe::Berlin;
        let now = tz
            .with_ymd_and_hms(2026, 9, 2, 14, 43, 0)
            .unwrap()
            .timestamp(); // Wednesday
        let sat = tz
            .with_ymd_and_hms(2026, 9, 5, 13, 45, 0)
            .unwrap()
            .timestamp();
        let fri = tz
            .with_ymd_and_hms(2026, 9, 4, 13, 45, 0)
            .unwrap()
            .timestamp();
        assert_eq!(onto_named_weekday(sat, Weekday::Fri, now, tz), fri);
        // Already right: untouched.
        assert_eq!(onto_named_weekday(fri, Weekday::Fri, now, tz), fri);
        // Named the day of capture itself: next week, not a past hour today.
        let next_wed = tz
            .with_ymd_and_hms(2026, 9, 9, 13, 45, 0)
            .unwrap()
            .timestamp();
        assert_eq!(onto_named_weekday(sat, Weekday::Wed, now, tz), next_wed);
    }

    /// The horizon under the witness.
    ///
    /// It corrects one wrong step of calendar arithmetic over a date the note
    /// does not spell out — "Freitag", said on a Wednesday, resolved to a
    /// Saturday. Left unbounded it also moved a date the note *does* spell out:
    /// a deadline three months away, judged correctly, dragged onto the coming
    /// Friday because the same note happened to mention one. Early, and then
    /// never again.
    #[test]
    fn the_witness_reaches_a_week_and_no_further() {
        use chrono::{TimeZone, Weekday};
        let tz = chrono_tz::Europe::Berlin;
        let made = tz
            .with_ymd_and_hms(2026, 9, 2, 14, 43, 0)
            .unwrap()
            .timestamp(); // Wednesday
        let sat = tz
            .with_ymd_and_hms(2026, 9, 5, 13, 45, 0)
            .unwrap()
            .timestamp();
        let far = tz
            .with_ymd_and_hms(2026, 11, 30, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert!(sat <= made + WITNESS_HORIZON, "the near date is inside");
        assert!(far > made + WITNESS_HORIZON, "the far one is not");
        // Inside: corrected, as it always was.
        assert_ne!(onto_named_weekday(sat, Weekday::Fri, made, tz), sat);
        // Outside: `apply`'s guard is the horizon, and the mover is never
        // reached — but assert what the mover *would* have done, so the reason
        // the guard exists is written down beside it.
        let moved = onto_named_weekday(far, Weekday::Fri, made, tz);
        assert!(
            moved < made + WITNESS_HORIZON,
            "unbounded, the correction pulls a November date into this week"
        );
    }

    /// A synthesizer whose judged reply is set by the test after it knows the
    /// ids it wants to link to.
    struct JudgedFake(std::sync::Mutex<Judgement>);

    #[async_trait]
    impl Synthesizer for JudgedFake {
        async fn segment(
            &self,
            input: SegmentInput<'_>,
        ) -> crate::error::Result<Vec<ProposedArtifact>> {
            Ok(vec![ProposedArtifact {
                text: input.core.to_string(),
                title: Some("judged".into()),
                category: Some("other".into()),
                tags: vec![],
                corpus_lines: None,
                caveats: vec![],
                pinned: false,
            }])
        }
        async fn segment_judged(
            &self,
            input: SegmentInput<'_>,
        ) -> crate::error::Result<SegmentReply> {
            Ok(SegmentReply {
                artifacts: self.segment(input).await?,
                judgement: Some(self.0.lock().unwrap().clone()),
            })
        }
        fn budget(&self) -> SynthesisBudget {
            FAKE_BUDGET
        }
    }

    fn judged_core_reply(j: Judgement) -> std::sync::Arc<JudgedFake> {
        std::sync::Arc::new(JudgedFake(std::sync::Mutex::new(j)))
    }

    /// A synthesizer that answers a bare reminder the way a model held to the
    /// prompt does: the judgement, and no artifact at all.
    struct JudgementOnly(Judgement);

    #[async_trait]
    impl Synthesizer for JudgementOnly {
        async fn segment(
            &self,
            _input: SegmentInput<'_>,
        ) -> crate::error::Result<Vec<ProposedArtifact>> {
            Ok(Vec::new())
        }
        async fn segment_judged(
            &self,
            _input: SegmentInput<'_>,
        ) -> crate::error::Result<SegmentReply> {
            Ok(SegmentReply {
                artifacts: Vec::new(),
                judgement: Some(self.0.clone()),
            })
        }
        fn budget(&self) -> SynthesisBudget {
            FAKE_BUDGET
        }
    }

    #[tokio::test]
    async fn a_note_that_is_only_a_reminder_still_gets_its_moment() {
        // "erinnere mich an den Termin, Freitag 13:45" holds nothing the
        // prompt lets the model write an artifact about — the intent and the
        // date belong to `moment`. The window used to drop the reply on the
        // floor for having no artifacts, retry the same call to exhaustion,
        // and leave the corpus `partial` with no reminder anywhere.
        let mut core = test_core().await;
        core.synthesizer = std::sync::Arc::new(JudgementOnly(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T13:45".into()),
            rule: None,
            events: vec![],
            links: vec![],
        }));
        let out = core
            .ingest(
                "erinnere mich an den Gastroentereologentermin, Freitag 13:45 uhr.",
                "web",
                None,
            )
            .await
            .unwrap();
        drain(&core).await;

        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "the reminder is the whole point: {rows:?}");
        assert!(rows[0].moment.at.is_some());
        // The verbatim passage is the record, and it is what the moment hangs
        // on: there is no artifact for it to hang on.
        let held = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(
            held.iter().any(|c| c.id == rows[0].moment.artifact_id),
            "the moment is anchored in this corpus: {held:?}"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            crate::store::corpora::CorpusStatus::Ready,
            "a note the model answered is not a half-finished capture"
        );
    }

    #[tokio::test]
    async fn a_recurrence_of_one_occurrence_is_not_a_recurrence() {
        // Asked to judge "Freitag 13:45", the configured model answers
        // `FREQ=WEEKLY;BYDAY=FR;COUNT=1;INTERVAL=1` — a rule that describes
        // the single date it already gave in `when`. Stored, it makes the
        // band offer to repeat a note that says nothing about repeating.
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T13:45".into()),
            rule: Some("FREQ=WEEKLY;BYDAY=FR;COUNT=1;INTERVAL=1".into()),
            events: vec![],
            links: vec![],
        });
        core.ingest("den Termin am Freitag 13:45", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].moment.rule, None,
            "a COUNT=1 rule is the one date, not a repetition"
        );
    }

    #[tokio::test]
    async fn a_single_occurrence_rule_still_gives_the_reminder_its_date() {
        // `when: null` with a COUNT=1 rule: dropping the rule as no
        // repetition threw away the only date the answer carried, and the
        // reminder was filed away as an ordinary capture.
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: Some("FREQ=WEEKLY;BYDAY=FR;COUNT=1".into()),
            events: vec![],
            links: vec![],
        });
        core.ingest("den Termin am Freitag", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].moment.rule, None, "still not a repetition");
        let at = rows[0].moment.at.expect("the rule carried the date");
        let tz = crate::core::moments::zone(Some(&rows[0].moment.tz));
        use chrono::{Datelike, TimeZone, Timelike};
        let local = tz.timestamp_opt(at, 0).single().unwrap();
        assert_eq!(local.weekday(), chrono::Weekday::Fri);
        assert_eq!(local.hour(), crate::core::moments::DEFAULT_HOUR);
    }

    #[tokio::test]
    async fn an_event_the_note_dates_outright_keeps_its_date() {
        // The weekday witness corrects the *reminder*. Applied to the events
        // it moved every date the note states onto the same weekday, and a
        // date already past was rewritten into the future.
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T09:00".into()),
            rule: None,
            events: vec!["2099-09-12T20:00".into()],
            links: vec![],
        });
        core.ingest(
            "friday i pick up the car; the concert is on 2099-09-12",
            "web",
            None,
        )
        .await
        .unwrap();
        drain(&core).await;
        let events: Vec<_> = core
            .store
            .moments_between(0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.moment)
            .filter(|m| m.kind == Kind::Event)
            .collect();
        assert_eq!(events.len(), 1, "{events:?}");
        let tz = crate::core::moments::zone(Some(&events[0].tz));
        use chrono::TimeZone;
        assert_eq!(
            tz.timestamp_opt(events[0].at.unwrap(), 0)
                .single()
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2099-09-12 20:00",
            "the date the note states is not moved onto the named weekday"
        );
    }

    #[tokio::test]
    async fn a_reminder_the_operator_moved_is_not_doubled_by_a_third_reading() {
        // Read Friday 14:00, corrected to 16:00 by hand, re-read as 15:00:
        // the exact-instant guard misses both times and a second open row
        // appeared beside the correction, with both of them pushing.
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest("den wagen abholen, freitag", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let moved_to = rows[0].moment.at.unwrap() + 7_200;
        core.store
            .move_moment(&rows[0].moment.id, moved_to, &rows[0].moment.tz)
            .await
            .unwrap();

        // The same prose, read a third way.
        let src = core.store.get_corpus(&out.id).await.unwrap();
        let anchor = rows[0].moment.artifact_id.clone();
        apply(
            &core,
            &src.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: Some("2099-09-04T15:00".into()),
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
            &src.raw_text,
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "the correction stands alone: {after:?}");
        assert_eq!(after[0].moment.at, Some(moved_to));
    }

    /// The delete used to happen at the top of `apply`, before any of the
    /// decisions below it. A window retry whose second reply is vaguer than the
    /// first walks straight into the "no date anywhere" arm, and that arm
    /// returns — so the standing reminder was destroyed and nothing was put
    /// back, with a `debug!` line for a record.
    #[tokio::test]
    async fn a_vaguer_re_reading_does_not_take_away_the_reminder_it_cannot_replace() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest("erinnere mich freitag, /mnt/backup prüfen", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (at, anchor) = (rows[0].moment.at, rows[0].moment.artifact_id.clone());

        // The same prose, read again as a reminder it cannot date.
        let src = core.store.get_corpus(&out.id).await.unwrap();
        apply(
            &core,
            &src.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: None,
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
            &src.raw_text,
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "the reminder stands: {after:?}");
        assert_eq!(after[0].moment.at, at);

        // And a reading that says outright it is *not* a reminder still
        // withdraws it — the delete moved, it did not go away.
        apply(
            &core,
            &src.id,
            &anchor,
            &Judgement {
                intent: Some("none".into()),
                when: None,
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
            &src.raw_text,
        )
        .await
        .unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_remind_judgement_becomes_a_due_moment() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        core.ingest("remind me to send the invoice on friday", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].moment.source, Source::Classified);
        assert!(rows[0].moment.at.is_some());
    }

    #[tokio::test]
    async fn a_judged_reminder_with_no_date_stays_a_plain_capture() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest("the gutters need clearing at some point", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
        let meta = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(meta["intent_read"], "remind", "the reading is recorded");
        assert_eq!(meta["intent_by"], "synthesis");
    }

    #[tokio::test]
    async fn a_forced_remind_stands_undated_and_a_refusal_outlives_a_re_read() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: None,
            events: vec![],
            links: vec![],
        });
        let mut c = Capture::new("remind me about the gutters", "web");
        c.metadata["intent"] = serde_json::Value::String("remind".into());
        let out = core.ingest_capture(c).await.unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "a forced remind stands, even undated");
        assert_eq!(rows[0].moment.source, Source::Cue);

        // The operator says no; a re-application must not put it back.
        let aid = rows[0].moment.artifact_id.clone();
        core.set_reminder(&aid, false).await.unwrap();
        let src = core.store.get_corpus(&out.id).await.unwrap();
        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        };
        apply(&core, &out.id, &aid, &j, &[], &src.raw_text)
            .await
            .unwrap();
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "the refusal outlived the re-read; {:?}",
            src.metadata
        );
    }

    #[tokio::test]
    async fn events_land_and_links_only_to_what_was_shown() {
        let core = test_core().await;
        // A neighbor that exists and was "shown".
        let other = core
            .ingest("the invoice workflow notes", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let neighbor = core
            .store
            .artifacts_for_corpus(&other.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .unwrap()
            .id;

        let out = core
            .ingest("the release lands next month, mark it", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let anchor = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .unwrap()
            .id;

        let j = Judgement {
            intent: Some("none".into()),
            when: None,
            rule: None,
            events: vec!["2099-09-12T00:00".into()],
            links: vec![
                ProposedLink {
                    artifact_id: neighbor.clone(),
                    reason: "same billing flow".into(),
                },
                ProposedLink {
                    artifact_id: "ghost-99".into(),
                    reason: "invented".into(),
                },
            ],
        };
        let src = core.store.get_corpus(&out.id).await.unwrap();
        apply(
            &core,
            &out.id,
            &anchor,
            &j,
            std::slice::from_ref(&neighbor),
            &src.raw_text,
        )
        .await
        .unwrap();

        let events = core.store.event_moments_between(0, i64::MAX).await.unwrap();
        assert!(
            events.iter().any(|r| r.moment.artifact_id == anchor),
            "{events:?}"
        );
        let links = core.store.links_touching(&anchor).await.unwrap();
        assert_eq!(links.len(), 1, "only the shown id landed: {links:?}");
        assert_eq!(links[0].state, crate::store::links::LinkState::Related);
        assert_eq!(links[0].reason.as_deref(), Some("same billing flow"));
    }
}
