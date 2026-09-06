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
/// The corpus journal's row for a moment this reading filed. Best-effort,
/// like everything on this path: a row that failed to write is a warning,
/// never a lost reminder.
async fn journal(core: &Core, moment_id: &str, anchor_id: &str, what: &str) {
    if let Err(err) = core
        .store
        .record_action(&crate::store::actions::NewAction {
            job: crate::store::actions::Job::Judgement,
            kind: crate::store::actions::Kind::Moment,
            subject_id: moment_id.to_string(),
            survivor_id: None,
            detail: Some(what.to_string()),
            evidence: serde_json::json!({ "artifact": anchor_id }),
            pair_score: None,
        })
        .await
    {
        tracing::warn!(moment_id, error = %err, "could not journal a filed moment");
    }
}

pub async fn apply(
    core: &Core,
    corpus_id: &str,
    anchor_id: &str,
    j: &Judgement,
    shown: &[String],
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

    // The model's date is the note's date. There was a weekday witness here —
    // the note's own weekday scanned out of the judged window, and any
    // reminder inside a week of the capture that fell on another weekday moved
    // onto the named one. It is gone, and the reason is worth keeping.
    //
    // What it was for was the model's calendar arithmetic: "Freitag" on a
    // Wednesday coming back as the Saturday. But the JUDGE block already
    // states the capture's weekday — `build_judge_ask` formats the local time
    // as `%Y-%m-%d %H:%M (%A)` — so the step the witness corrected is one the
    // model is handed the answer to. What was left was a heuristic correction
    // reading a heuristic detector, and it had already produced this exact bug
    // once: a Portuguese ordinal read as Monday, overriding a date the note
    // stated outright. Narrowing the word list did not reach the case where
    // the word really is a weekday — "Mittwoch Zahnarzt; Rechnung fällig
    // 2026-09-11", captured on a Monday, had its 09-11 reminder silently moved
    // to 09-09 and never fired on the day it was for.
    //
    // The two failures are not the same size. A model that gets a weekday
    // wrong leaves a reminder a few days off, on the band, with a button that
    // moves it. The witness getting it wrong rewrote a date nobody misread,
    // silently, in the one direction that fires early and then never again.
    // Recorded before anything is withdrawn, so a store error here costs
    // nothing: `?` used to abort `apply` with the previous reading already
    // deleted and no replacement written.
    // Returns the metadata it wrote, and every read of an operator's word
    // below is taken from *that* and not from `src`: `src` was read at the top
    // of `apply`, and a "not a reminder" press landing in between was both
    // clobbered by this write and then invisible to the `intent_refused`
    // checks — so the arm went on to re-arm the very row the operator refused.
    let meta = record_intent(core, corpus_id, j.intent.as_deref()).await?;
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
        match core
            .store
            .insert_moment(&NewMoment {
                artifact_id: anchor_id.into(),
                kind: Kind::Event,
                at: Some(at),
                tz: tz_name.clone(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
        {
            Ok(moment_id) => journal(core, &moment_id, anchor_id, "event").await,
            Err(err) => {
                tracing::warn!(corpus_id, error = %err, "could not record a judged event");
            }
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

    let forced = meta["intent"].as_str();
    // The door outranks the model. `engram -r` is a person saying "remind
    // me", and the forcing was only ever consumed *inside* the `remind` arm —
    // so a model answering `none` (or `journal`) for an explicit reminder fell
    // into the catch-all below, wrote no due row at all, and took the previous
    // reading's reminder with it. The operator's own refusal is still checked
    // inside the arm; nothing else overrules the door.
    let read_as = match forced {
        Some("remind") => Some("remind"),
        _ => j.intent.as_deref(),
    };
    match read_as {
        Some("journal")
            if JOURNALABLE.contains(&src.origin.as_str())
                && !intent_refused(&meta, Intent::Journal) =>
        {
            // The reading says this is an entry and not a reminder, so the
            // reminder the previous reading filed is withdrawn — the delete
            // that used to happen unconditionally at the top of `apply`, moved
            // to the one arm that is actually saying it.
            core.store.delete_read_due(anchor_id).await?;
            core.set_entry(corpus_id, true).await?;
        }
        Some("remind") => {
            if intent_refused(&meta, Intent::Remind) {
                // The operator has said this is not a reminder. Their word,
                // not the model's, and it takes the read rows with it.
                core.store.delete_read_due(anchor_id).await?;
                return Ok(());
            }
            let at = j.when.as_deref().and_then(|w| parse_local(w, tz));
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
                // One thing a second reading of the same prose can still add,
                // and the instant comparison above cannot see: the same Friday
                // now read as *every* Friday. Returning on the instant alone
                // dropped the recurrence, and the reminder stayed a one-shot
                // however plainly the note said it repeats.
                if let Some(r) = rule.as_deref()
                    && core.store.set_rule_of_open_due(anchor_id, at, r).await?
                {
                    core.store.rearm_remind().await?;
                    tracing::debug!(
                        corpus_id,
                        rule = r,
                        "the standing reminder repeats after all"
                    );
                }
                return Ok(());
            }
            // A row this base has already spoken about — pushed, or put aside
            // — is not a re-read's to replace. `delete_read_due` keeps such a
            // row on purpose, so the delete below would find nothing and the
            // insert would add a *second* open row beside it: two readings of
            // one piece of prose, both on the ladder, both pushing.
            if core.store.has_acted_on_due(anchor_id).await? {
                tracing::debug!(
                    corpus_id,
                    "the reminder on this artifact has already been pushed or snoozed; \
                     the re-read adds nothing"
                );
                return Ok(());
            }
            // And an undated *forced* remind never outranks a date already
            // standing here. The undated row is the band's question "when?",
            // which is the right answer where there is no date to be had and
            // the wrong one where there is: `uncovered` filters on
            // `m.at IS NOT NULL`, so replacing a dated row with it stopped the
            // reminder firing without saying so anywhere.
            if at.is_none()
                && rule.is_none()
                && let Some(open) = core.store.open_due_for_artifact(anchor_id).await?
                && open.at.is_some()
            {
                tracing::debug!(
                    corpus_id,
                    "a forced remind with no date leaves the standing dated reminder alone"
                );
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
            let moment_id = core
                .store
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
                    series_id: None,
                })
                .await?;
            journal(core, &moment_id, anchor_id, "due").await;
            core.store.rearm_remind().await?;
            // A note a completed reminder retired, being read as a reminder
            // again. `complete_moment` retires the corpus so a finished
            // reminder stops being one of the last things you kept — but an
            // open reminder standing on a note that is missing from the rail
            // and demoted below the search cliff is a reminder nobody can see
            // the source of, with no undo anywhere offering to bring it back.
            // Arming takes the retirement back, and only arming does: this is
            // past every guard above, so the row genuinely exists.
            if core.store.is_retired(corpus_id).await? {
                core.store.unretire_corpus(corpus_id).await?;
                tracing::info!(
                    corpus_id,
                    "the note was retired by a completed reminder; a new one brings it back"
                );
            }
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
    intent: Option<&str>,
) -> Result<serde_json::Value> {
    // Read here rather than reusing `apply`'s snapshot from the top of the
    // call: this is a read-modify-write of a column `set_reminder` and
    // `set_entry` also write, and the further back the read is, the wider the
    // window in which an operator's press is overwritten by a stale clone.
    let mut meta = core.store.get_corpus(corpus_id).await?.metadata;
    // Indexing a `Value` that is not an object panics, and this one comes
    // straight out of a column — the guard `describe::park_failed` and
    // `extract` both carry. This runs at the very top of `apply`, so a corpus
    // whose metadata is a JSON scalar took the worker down before any of the
    // judgement was filed.
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    meta["intent_read"] = serde_json::Value::String(intent.unwrap_or("none").to_string());
    meta["intent_by"] = serde_json::Value::String("synthesis".to_string());
    if let Some(m) = meta.as_object_mut() {
        m.remove("intent_score");
    }
    core.store.set_corpus_metadata(corpus_id, &meta).await?;
    Ok(meta)
}

/// The first instant a rule names after the capture, at `DEFAULT_HOUR`.
///
/// Only for a judgement whose `when` is null: the rule then carries the only
/// date in the answer, and the time of day is the one the prompt names for a
/// note that states none. `next_after` is strict, so a rule naming the
/// capture's own weekday means the next one, which is what the word means:
/// "every Monday", written on a Monday, starts with the Monday to come.
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

/// The wall-clock spellings, with a bare date meaning `DEFAULT_HOUR`.
///
/// Fractional seconds included: `2026-09-04T09:00:00.000Z` is what a model
/// hands back often enough, `split_offset` takes the `Z` off it, and the
/// remaining `.000` matched none of the three formats — so the reminder was
/// dropped on the floor with a `debug!` line for a record.
fn naive(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
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

    /// `2026-09-04T09:00:00.000Z` is a spelling models hand back, and
    /// `split_offset` takes the `Z` off before `naive` ever sees it. Without
    /// the fractional format the remainder parsed as nothing at all and the
    /// reminder was dropped with a `debug!` line for a record.
    #[test]
    fn a_when_with_fractional_seconds_is_a_date() {
        let utc = chrono_tz::UTC;
        let plain = parse_local("2026-09-04T09:00:00Z", utc).expect("the plain spelling");
        assert_eq!(parse_local("2026-09-04T09:00:00.000Z", utc), Some(plain));
        assert_eq!(parse_local("2026-09-04T09:00:00.123456Z", utc), Some(plain));
        assert_eq!(
            parse_local("2026-09-04T09:00:00.5+00:00", utc),
            Some(plain),
            "and beside a stated offset, which is the other door into `naive`"
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
        let anchor = rows[0].moment.artifact_id.clone();
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: Some("2099-09-04T15:00".into()),
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
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
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: None,
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
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
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("none".into()),
                when: None,
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
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
        // The journal names the moment the reading filed.
        let journal = core
            .store
            .open_actions(&[crate::store::actions::Kind::Moment], 10)
            .await
            .unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].subject_id, rows[0].moment.id);
        assert_eq!(journal[0].detail.as_deref(), Some("due"));
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
        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        };
        apply(&core, &out.id, &aid, &j, &[]).await.unwrap();
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "the refusal outlived the re-read; {:?}",
            core.store.get_corpus(&out.id).await.unwrap().metadata
        );
    }

    /// The door outranks the model. `engram -r` is a person saying "remind
    /// me", and the forcing used to be consumed only *inside* the `remind`
    /// arm — so a model answering `none` for an explicit reminder fell into
    /// the catch-all, wrote no due row at all, and took the previous
    /// reading's reminder with it.
    #[tokio::test]
    async fn a_door_that_says_remind_me_outranks_a_model_that_says_otherwise() {
        for answered in ["none", "journal"] {
            let mut core = test_core().await;
            core.synthesizer = judged_core_reply(Judgement {
                intent: Some(answered.into()),
                when: Some("2099-01-01T09:00".into()),
                rule: None,
                events: vec![],
                links: vec![],
            });
            let mut c = Capture::new("remind me about the gutters on the first", "web");
            c.metadata["intent"] = serde_json::Value::String("remind".into());
            core.ingest_capture(c).await.unwrap();
            drain(&core).await;
            let rows = core.store.open_due(0, i64::MAX).await.unwrap();
            assert_eq!(
                rows.len(),
                1,
                "the door asked for a reminder and the model said {answered}"
            );
            assert_eq!(rows[0].moment.source, Source::Cue);
        }
    }

    /// Metadata comes straight out of a column, and indexing a `Value` that
    /// is not an object panics. `record_intent` runs at the very top of
    /// `apply`, so a corpus whose metadata is a JSON scalar took the worker
    /// down before any of the judgement was filed — the guard
    /// `describe::park_failed` and `extract` both carry.
    #[tokio::test]
    async fn metadata_that_is_not_an_object_does_not_take_the_worker_down() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest_capture(Capture::new("water the gutters on the first", "web"))
            .await
            .unwrap();
        drain(&core).await;
        core.store
            .set_corpus_metadata(&out.id, &serde_json::json!("a scalar"))
            .await
            .unwrap();
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.in_results())
            .expect("a live artifact")
            .id;
        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-02-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        };
        apply(&core, &out.id, &aid, &j, &[])
            .await
            .expect("the reading is filed rather than panicking");
        let meta = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(meta["intent_read"], "remind");
    }

    /// Completing a reminder retires the note behind it, so it stops being one
    /// of the last things you kept. A later reading that arms a *new* reminder
    /// on that note has to take the retirement back: an open reminder whose
    /// note is missing from the rail and demoted below the search cliff is a
    /// reminder nobody can see the source of, and no undo anywhere offers to
    /// bring it back.
    #[tokio::test]
    async fn a_note_read_as_a_reminder_again_comes_back_out_of_retirement() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest_capture(Capture::new("clear the gutters before the frost", "web"))
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "the first reading armed one");
        let aid = rows[0].moment.artifact_id.clone();
        assert!(core.complete_moment(&rows[0].moment.id).await.unwrap());
        assert!(
            core.store.is_retired(&out.id).await.unwrap(),
            "the fixture must actually retire the note"
        );

        // A later reading of the same prose, landing on a different date.
        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-03-01T09:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        };
        apply(&core, &out.id, &aid, &j, &[]).await.unwrap();
        assert_eq!(
            core.store.open_due(0, i64::MAX).await.unwrap().len(),
            1,
            "a reminder stands again"
        );
        assert!(
            !core.store.is_retired(&out.id).await.unwrap(),
            "so the note it is about is back among the recent captures"
        );
        assert!(
            core.store
                .recent_captures(20)
                .await
                .unwrap()
                .iter()
                .any(|(id, ..)| *id == out.id)
        );
    }

    /// `delete_read_due` keeps a row that has already been pushed — it has a
    /// history, and a row with a history outlives the reading that made it.
    /// The stage deleted anyway, found nothing to delete, and inserted: two
    /// open rows for one reading of one note, both climbing the ladder.
    #[tokio::test]
    async fn a_re_read_adds_nothing_beside_a_reminder_that_already_pushed() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest("die rechnung schicken", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        // The 48 h rung fires, which is what the delete refuses to undo.
        core.store
            .mark_notified(&[rows[0].moment.id.clone()], 1_000)
            .await
            .unwrap();

        let anchor = rows[0].moment.artifact_id.clone();
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: Some("2099-09-04T15:00".into()),
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "one reading, one row: {after:?}");
        assert_eq!(after[0].moment.at, rows[0].moment.at);
    }

    /// A forced remind with no date is the band asking "when?", which is the
    /// right question where there is no date and the wrong one where a date is
    /// already standing: `uncovered` filters undated rows out, so the swap
    /// stopped the reminder firing with nothing said anywhere.
    #[tokio::test]
    async fn a_forced_remind_with_no_date_leaves_a_standing_date_alone() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let mut c = Capture::new("die reifen wechseln, freitag", "web");
        c.metadata["intent"] = serde_json::Value::String("remind".into());
        let out = core.ingest_capture(c).await.unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let dated = rows[0].moment.at;
        assert!(dated.is_some());

        let anchor = rows[0].moment.artifact_id.clone();
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: None,
                rule: None,
                events: vec![],
                links: vec![],
            },
            &[],
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0].moment.at, dated, "the date survived the re-read");
    }

    /// The instant guard compares instants and nothing else, which is right
    /// for the duplicate it prevents and wrong for the one thing a second
    /// reading can add: the same Friday, now read as every Friday.
    #[tokio::test]
    async fn a_re_read_that_recognises_the_recurrence_keeps_it() {
        let mut core = test_core().await;
        core.synthesizer = judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
            links: vec![],
        });
        let out = core
            .ingest("freitags den müll rausstellen", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].moment.rule, None);

        let anchor = rows[0].moment.artifact_id.clone();
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: Some("2099-09-04T14:00".into()),
                rule: Some("FREQ=WEEKLY;BYDAY=FR".into()),
                events: vec![],
                links: vec![],
            },
            &[],
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "no second row: {after:?}");
        assert_eq!(
            after[0].moment.rule.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=FR"),
            "the recurrence reached the row that was already there"
        );
        assert!(
            after[0].moment.series_id.is_some(),
            "and it heads its own series, which is what COUNT is counted over"
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
        apply(&core, &out.id, &anchor, &j, std::slice::from_ref(&neighbor))
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
