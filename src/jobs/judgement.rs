//! What a judged synthesis reply becomes: moments. Links come from `associate::link_shown`.
//! Best-effort throughout: a judgement that cannot be applied is a warning, never a lost capture.

use crate::core::Core;
use crate::core::moments::{
    DEFAULT_HOUR, Intent, default_zone_name, intent_refused, validate_rule, zone,
};
use crate::error::Result;
use crate::infer::Judgement;
use crate::store::moments::{Kind, NewMoment, Source};

/// Best-effort: a journal row that failed to write is a warning, never a lost reminder.
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

/// Idempotent per re-read: read rows are replaced; done, set and refused rows are kept.
pub async fn apply(core: &Core, corpus_id: &str, anchor_id: &str, j: &Judgement) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let tz_name = src.metadata["tz"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| default_zone_name(&core.time.default_tz));
    let tz = zone(Some(&tz_name));
    // Spelled as the zone table spells it: the due band reads the wall clock back out of it.
    let tz_name = tz.name().to_string();

    // No weekday correction: the JUDGE block states the capture's weekday. Recorded before anything
    // is withdrawn; later reads of the operator's word use the returned `meta`, since a press can
    // land after `src` was read.
    let meta = record_intent(core, corpus_id, j.intent.as_deref()).await?;
    // Events only: due rows are withdrawn in the `remind` arm, where several paths return without a
    // replacement.
    core.store.delete_read_events(anchor_id).await?;

    // Dates the note states without being the reminder: the day page's rows.
    // Each section below fails on its own, and the reminder comes last.
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

    let forced = meta["intent"].as_str();
    // A forced `remind` (`engram -r`) outranks the model; only the operator's refusal, checked in
    // the arm, overrules it.
    let read_as = match forced {
        Some("remind") => Some("remind"),
        _ => j.intent.as_deref(),
    };
    match read_as {
        Some("remind") => {
            if intent_refused(&meta, Intent::Remind) {
                core.store.delete_read_due(anchor_id).await?;
                return Ok(());
            }
            // A past instant is no reminder, measured against the clock: an idle model echoes the
            // prompt's `Current local time`, which postdates the capture. A day-only `when` is
            // checked by day, as its `DEFAULT_HOUR` is ours.
            let now = core.clock.now();
            let at = j
                .when
                .as_deref()
                .and_then(|w| parse_stated(w, tz))
                .filter(|s| {
                    if s.timed {
                        s.at > now
                    } else {
                        day_not_past(s.at, now, tz)
                    }
                })
                .map(|s| s.at);
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
            // A date only the rule carries, anchored at the later of capture and clock: a rule with
            // no date can never fire (`uncovered`) nor arm its successor (`complete_moment`).
            let at = at.or_else(|| {
                valid_rule
                    .as_deref()
                    .and_then(|r| first_occurrence(r, src.created_at.max(now), tz))
            });
            let rule = valid_rule
                // A rule with one occurrence is the date `when` already carries, not a repetition.
                .filter(|r| !single_occurrence(r));
            // An undated judged reminder stays a plain capture; a forced one files undated and the
            // band asks for the date.
            let forced_remind = forced == Some("remind");
            // A rule with no date is no better.
            if at.is_none() && !forced_remind {
                // The previous reading stands: an undatable re-read does not say the standing date
                // was wrong.
                tracing::debug!(
                    corpus_id,
                    "a judged reminder with no date is left as a capture"
                );
                return Ok(());
            }
            // `None` included: a finished undated row is one `delete_read_due` keeps and must not
            // be re-filed.
            if core.store.has_moment_at(anchor_id, Kind::Due, at).await? {
                // The instant guard cannot see a re-read that finds the same date now repeats.
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
            // `delete_read_due` keeps a pushed or put-aside row, so filing here would add a second
            // open row beside it.
            if core.store.has_acted_on_due(anchor_id).await? {
                tracing::debug!(
                    corpus_id,
                    "the reminder on this artifact has already been pushed or snoozed; \
                     the re-read adds nothing"
                );
                return Ok(());
            }
            // An undated forced remind never replaces a dated row: `uncovered` needs `m.at IS NOT
            // NULL`.
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
            // A date the operator moved outranks any re-reading of the prose.
            if core.store.has_operator_moment(anchor_id, Kind::Due).await? {
                tracing::debug!(
                    corpus_id,
                    "the reminder on this artifact was moved or set by hand; the re-read adds nothing"
                );
                return Ok(());
            }
            // Delete, then insert: the artifact never holds two open readings of the same prose.
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
            // Arming un-retires the note: an open reminder whose source is hidden from rail and
            // search has no way back.
            if core.store.is_retired(corpus_id).await? {
                core.store.unretire_corpus(corpus_id).await?;
                tracing::info!(
                    corpus_id,
                    "the note was retired by a completed reminder; a new one brings it back"
                );
            }
            let art = core.store.get_artifact(anchor_id).await?;
            if let Err(e) = confirm_created(core, &art, at, tz).await {
                tracing::warn!(error = %e, "could not push the capture-time confirmation");
            }
        }
        // Any other intent, known or not, says this note is not a reminder.
        _ => {
            core.store.delete_read_due(anchor_id).await?;
        }
    }
    Ok(())
}

/// Recorded so that a later argument about a filing is a measurement.
async fn record_intent(
    core: &Core,
    corpus_id: &str,
    intent: Option<&str>,
) -> Result<serde_json::Value> {
    // Read fresh: `set_reminder` writes this column too, and a stale copy overwrites an operator's
    // press.
    let mut meta = core.store.get_corpus(corpus_id).await?.metadata;
    // Indexing a non-object `Value` panics, and a column can hold a JSON scalar.
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

/// For a null `when`. `next_after` is strict: "every Monday", written on a Monday, starts next
/// Monday.
fn first_occurrence(rule: &str, created_at: i64, tz: chrono_tz::Tz) -> Option<i64> {
    use chrono::TimeZone;
    let day = tz.timestamp_opt(created_at, 0).single()?.date_naive();
    let anchor = crate::core::moments::resolve_local(day.and_hms_opt(DEFAULT_HOUR, 0, 0)?, tz)?;
    crate::core::moments::next_after(rule, anchor, tz)
}

/// `COUNT=1` only: a rule whose `UNTIL` leaves one occurrence still says it repeats.
fn single_occurrence(rule: &str) -> bool {
    rule.split(';').any(|part| {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        k.trim().eq_ignore_ascii_case("COUNT") && v.trim() == "1"
    })
}

/// The "reminder set" push at capture time, independent of the due-time ladder.
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

/// A local wall clock or date in `tz`. A stated offset is honoured: models append `Z` to anything
/// ISO-shaped.
pub(crate) fn parse_local(s: &str, tz: chrono_tz::Tz) -> Option<i64> {
    parse_stated(s, tz).map(|s| s.at)
}

/// An instant a judgement stated, and whether it named a time of day.
pub(crate) struct Stated {
    pub at: i64,
    /// Read before any offset: `2026-09-04Z` is still a day.
    pub timed: bool,
}

pub(crate) fn parse_stated(s: &str, tz: chrono_tz::Tz) -> Option<Stated> {
    let s = s.trim();
    match split_offset(s) {
        Some((head, off)) => {
            use chrono::TimeZone;
            let (n, timed) = naive(head)?;
            Some(Stated {
                at: off.from_local_datetime(&n).single()?.timestamp(),
                timed,
            })
        }
        None => {
            let (n, timed) = naive(s)?;
            Some(Stated {
                at: crate::core::moments::resolve_local(n, tz)?,
                timed,
            })
        }
    }
}

/// "Past" for a day-only answer: whether the day, not `DEFAULT_HOUR`, has gone by.
fn day_not_past(at: i64, now: i64, tz: chrono_tz::Tz) -> bool {
    use chrono::TimeZone;
    let day = |t: i64| Some(tz.timestamp_opt(t, 0).single()?.date_naive());
    match (day(at), day(now)) {
        (Some(a), Some(n)) => a >= n,
        _ => false,
    }
}

/// A bare date means `DEFAULT_HOUR`. Fractional seconds are accepted: models send `09:00:00.000Z`.
fn naive(s: &str) -> Option<(chrono::NaiveDateTime, bool)> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .map(|n| (n, true))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| {
                (
                    d.and_time(chrono::NaiveTime::MIN)
                        + chrono::Duration::hours(DEFAULT_HOUR as i64),
                    false,
                )
            })
        })
        .ok()
}

/// The sign is searched after the `T`, so the date's own dashes are not taken for one.
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
        ProposedArtifact, SegmentInput, SegmentReply, SynthesisBudget, Synthesizer,
    };
    use crate::jobs::test_support::drain;
    use async_trait::async_trait;

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
                tags: vec![],
                corpus_lines: None,
                caveats: vec![],
            }])
        }
        async fn segment_judged(
            &self,
            input: SegmentInput<'_>,
        ) -> crate::error::Result<SegmentReply> {
            // As the real writer: no judgement unless the judged schema was asked for.
            let asked = input.judge.is_some();
            Ok(SegmentReply {
                artifacts: self.segment(input).await?,
                judgement: asked.then(|| {
                    self.0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                }),
            })
        }
        fn budget(&self) -> SynthesisBudget {
            FAKE_BUDGET
        }
    }

    fn judged_core_reply(j: Judgement) -> std::sync::Arc<JudgedFake> {
        std::sync::Arc::new(JudgedFake(std::sync::Mutex::new(j)))
    }

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
        // Such a note holds nothing to write an artifact about, yet the moment must be filed.
        let mut core = test_core().await;
        core.synthesizer = Some(std::sync::Arc::new(JudgementOnly(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T13:45".into()),
            rule: None,
            events: vec![],
        })));
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
        // No artifact exists, so the moment hangs on the verbatim passage.
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
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T13:45".into()),
            rule: Some("FREQ=WEEKLY;BYDAY=FR;COUNT=1;INTERVAL=1".into()),
            events: vec![],
        }));
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
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: Some("FREQ=WEEKLY;BYDAY=FR;COUNT=1".into()),
            events: vec![],
        }));
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
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T09:00".into()),
            rule: None,
            events: vec!["2099-09-12T20:00".into()],
        }));
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
        // Neither re-read lands on an exact instant, so only the operator guard stops a second open
        // row.
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
        }));
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
            },
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "the correction stands alone: {after:?}");
        assert_eq!(after[0].moment.at, Some(moved_to));
    }

    #[tokio::test]
    async fn a_vaguer_re_reading_does_not_take_away_the_reminder_it_cannot_replace() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
        }));
        let out = core
            .ingest("erinnere mich freitag, /mnt/backup prüfen", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (at, anchor) = (rows[0].moment.at, rows[0].moment.artifact_id.clone());

        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("remind".into()),
                when: None,
                rule: None,
                events: vec![],
            },
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "the reminder stands: {after:?}");
        assert_eq!(after[0].moment.at, at);

        // A reading that says it is not a reminder still withdraws it.
        apply(
            &core,
            &out.id,
            &anchor,
            &Judgement {
                intent: Some("none".into()),
                when: None,
                rule: None,
                events: vec![],
            },
        )
        .await
        .unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_remind_judgement_becomes_a_due_moment() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T09:00".into()),
            rule: None,
            events: vec![],
        }));
        core.ingest("remind me to send the invoice on friday", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].moment.source, Source::Classified);
        assert!(rows[0].moment.at.is_some());
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
    async fn a_reminder_still_arms_from_the_writers_when() {
        use crate::infer::decide::{Answer, Answers, Kind, Question};
        let mut core = test_core().await;
        // The pre-read says "a reminder" and "no date": the date is the writer's alone.
        let dec = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(
            |_, qs: &[Question]| {
                let answers = qs
                    .iter()
                    .map(|q| {
                        let a = match &q.kind {
                            Kind::Noul if q.key == "asks_reminder" => Answer::Noul { yes: 0.97 },
                            Kind::Noul => Answer::Noul { yes: 0.0 },
                            Kind::Choice { options } => Answer::Choice {
                                pick: options[0].0.clone(),
                                probabilities: vec![1.0 / options.len() as f64; options.len()],
                                confidence: 0.0,
                            },
                            Kind::Score { levels } => Answer::Score {
                                score: 0.0,
                                probabilities: vec![1.0 / levels.len() as f64; levels.len()],
                                confidence: 0.0,
                            },
                        };
                        (q.key.clone(), a)
                    })
                    .collect();
                Ok(Answers {
                    answers,
                    model: "rule-decider".into(),
                    input_tokens: 0,
                })
            },
        ));
        core.decider = dec.clone();
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T09:00".into()),
            rule: None,
            events: vec![],
        }));
        let out = core
            .ingest("remind me to send the invoice", "web", None)
            .await
            .unwrap();
        drain(&core).await;
        assert!(
            dec.requests()
                .iter()
                .any(|(_, qs)| qs.iter().any(|q| q.key == "asks_reminder")),
            "the window was pre-read"
        );
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let tz = crate::core::moments::zone(Some(&crate::core::moments::default_zone_name(
            &core.time.default_tz,
        )));
        assert_eq!(rows[0].moment.at, parse_local("2099-09-04T09:00", tz));
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().metadata["pre_read"]["has_date"],
            0.0
        );
    }

    #[tokio::test]
    async fn a_judged_reminder_already_in_the_past_is_not_a_date() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2020-01-01T09:00".into()),
            rule: None,
            events: vec![],
        }));
        core.ingest(
            "erinnere mich, dass ich um 18 Uhr einkaufen gehen muss.",
            "web",
            None,
        )
        .await
        .unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert!(
            rows.is_empty(),
            "a reading whose only date has passed leaves the note a capture: {rows:?}"
        );
    }

    #[tokio::test]
    async fn a_bare_date_is_past_only_when_the_day_is() {
        // 2026-09-15T15:00Z, and the same day at `DEFAULT_HOUR` is behind it.
        const AFTERNOON: i64 = 1_789_484_400;
        for (when, filed) in [
            ("2026-09-15", true),
            // The prompt's own clock handed back: timed, and past.
            ("2026-09-15T14:00", false),
            ("2026-09-14", false),
        ] {
            let mut core = test_core().await;
            core.clock = crate::core::context::Clock::Fixed(AFTERNOON);
            core.synthesizer = Some(judged_core_reply(Judgement {
                intent: Some("remind".into()),
                when: Some(when.into()),
                rule: None,
                events: vec![],
            }));
            let mut c = Capture::new("erinnere mich heute an die Anmeldung", "web");
            c.metadata["tz"] = serde_json::Value::String("UTC".into());
            core.ingest_capture(c).await.unwrap();
            drain(&core).await;
            let rows = core.store.open_due(0, i64::MAX).await.unwrap();
            assert_eq!(rows.len(), usize::from(filed), "when = {when}: {rows:?}");
            if filed {
                use chrono::{TimeZone, Timelike};
                let at = rows[0].moment.at.expect("the day was filed with its hour");
                let local = chrono_tz::UTC.timestamp_opt(at, 0).single().unwrap();
                assert_eq!(local.hour(), crate::core::moments::DEFAULT_HOUR);
                assert!(
                    at < AFTERNOON,
                    "and the hour it names is already behind the clock, which the \
                     band draws as due at once"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_recurrence_older_than_the_clock_advances_instead_of_dying() {
        let mut core = test_core().await;
        let late = crate::store::now() + 10 * 365 * 24 * 3600;
        core.clock = crate::core::context::Clock::Fixed(late);
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: Some("FREQ=WEEKLY;BYDAY=FR".into()),
            events: vec![],
        }));
        let mut c = Capture::new("freitags den müll rausstellen", "web");
        c.metadata["tz"] = serde_json::Value::String("UTC".into());
        core.ingest_capture(c).await.unwrap();
        drain(&core).await;

        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let at = rows[0]
            .moment
            .at
            .expect("a rule with no date is a dead row");
        assert!(
            at > late,
            "the recurrence advanced past the clock rather than being dropped"
        );
        use chrono::{Datelike, TimeZone};
        assert_eq!(
            chrono_tz::UTC
                .timestamp_opt(at, 0)
                .single()
                .unwrap()
                .weekday(),
            chrono::Weekday::Fri
        );
        assert_eq!(
            rows[0].moment.rule.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=FR"),
            "and it is still the repetition the note states"
        );
    }

    #[tokio::test]
    async fn a_judged_reminder_with_no_date_stays_a_plain_capture() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: None,
            events: vec![],
        }));
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
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: None,
            rule: None,
            events: vec![],
        }));
        let mut c = Capture::new("remind me about the gutters", "web");
        c.metadata["intent"] = serde_json::Value::String("remind".into());
        let out = core.ingest_capture(c).await.unwrap();
        drain(&core).await;
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "a forced remind stands, even undated");
        assert_eq!(rows[0].moment.source, Source::Cue);

        let aid = rows[0].moment.artifact_id.clone();
        core.set_reminder(&aid, false).await.unwrap();
        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
        };
        apply(&core, &out.id, &aid, &j).await.unwrap();
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "the refusal outlived the re-read; {:?}",
            core.store.get_corpus(&out.id).await.unwrap().metadata
        );
    }

    #[tokio::test]
    async fn a_door_that_says_remind_me_outranks_a_model_that_says_otherwise() {
        for answered in ["none", "anything else"] {
            let mut core = test_core().await;
            core.synthesizer = Some(judged_core_reply(Judgement {
                intent: Some(answered.into()),
                when: Some("2099-01-01T09:00".into()),
                rule: None,
                events: vec![],
            }));
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

    #[tokio::test]
    async fn metadata_that_is_not_an_object_does_not_take_the_worker_down() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
        }));
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
        };
        apply(&core, &out.id, &aid, &j)
            .await
            .expect("the reading is filed rather than panicking");
        let meta = core.store.get_corpus(&out.id).await.unwrap().metadata;
        assert_eq!(meta["intent_read"], "remind");
    }

    #[tokio::test]
    async fn a_note_read_as_a_reminder_again_comes_back_out_of_retirement() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-01-01T09:00".into()),
            rule: None,
            events: vec![],
        }));
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

        let j = Judgement {
            intent: Some("remind".into()),
            when: Some("2099-03-01T09:00".into()),
            rule: None,
            events: vec![],
        };
        apply(&core, &out.id, &aid, &j).await.unwrap();
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

    #[tokio::test]
    async fn a_re_read_adds_nothing_beside_a_reminder_that_already_pushed() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
        }));
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
            },
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "one reading, one row: {after:?}");
        assert_eq!(after[0].moment.at, rows[0].moment.at);
    }

    #[tokio::test]
    async fn a_forced_remind_with_no_date_leaves_a_standing_date_alone() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
        }));
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
            },
        )
        .await
        .unwrap();

        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0].moment.at, dated, "the date survived the re-read");
    }

    #[tokio::test]
    async fn a_re_read_that_recognises_the_recurrence_keeps_it() {
        let mut core = test_core().await;
        core.synthesizer = Some(judged_core_reply(Judgement {
            intent: Some("remind".into()),
            when: Some("2099-09-04T14:00".into()),
            rule: None,
            events: vec![],
        }));
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
            },
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
    async fn events_still_land_from_the_writer() {
        let core = test_core().await;
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
        };
        apply(&core, &out.id, &anchor, &j).await.unwrap();

        let events = core.store.event_moments_between(0, i64::MAX).await.unwrap();
        assert!(
            events.iter().any(|r| r.moment.artifact_id == anchor),
            "{events:?}"
        );
        assert!(
            core.store.links_touching(&anchor).await.unwrap().is_empty(),
            "the writer names no links"
        );
    }

    mod shown {
        use super::*;
        use crate::infer::decide::{Answer, Answers, Kind as QKind};
        use crate::infer::fake::RuleDecider;
        use crate::store::links::{LinkKind, LinkState};
        use std::collections::HashMap;
        use std::sync::Arc;

        /// The fake budget shows no neighbours; this one does, and still leaves the window room.
        const SHOWING: SynthesisBudget = SynthesisBudget {
            context: crate::infer::context::ContextBudget {
                neighbors: 512,
                ..FAKE_BUDGET.context
            },
            ..FAKE_BUDGET
        };

        /// One artifact, the window as it is, titled by its first word; judged when asked.
        struct Showing;

        #[async_trait]
        impl Synthesizer for Showing {
            async fn segment(
                &self,
                input: SegmentInput<'_>,
            ) -> crate::error::Result<Vec<ProposedArtifact>> {
                Ok(vec![ProposedArtifact {
                    text: input.core.to_string(),
                    title: input.core.split_whitespace().next().map(String::from),
                    tags: vec![],
                    corpus_lines: None,
                    caveats: vec![],
                }])
            }
            async fn segment_judged(
                &self,
                input: SegmentInput<'_>,
            ) -> crate::error::Result<SegmentReply> {
                let asked = input.judge.is_some();
                Ok(SegmentReply {
                    artifacts: self.segment(input).await?,
                    judgement: asked.then(|| Judgement {
                        intent: Some("none".into()),
                        ..Default::default()
                    }),
                })
            }
            fn budget(&self) -> SynthesisBudget {
                SHOWING
            }
        }

        /// Every note may ask a reminder, so a capture is judged. `link_kind` favours `kind` at
        /// 0.95 and its swapped twin says `twin`; the note that says "general" comes first,
        /// whichever label it carries; any other choice its first option.
        fn linking(kind: &'static str, twin: &'static str) -> Arc<RuleDecider> {
            Arc::new(RuleDecider::new(move |state, qs| {
                let a_block = state.split("\n\nB: ").next().unwrap_or_default();
                let mut answers = HashMap::new();
                for q in qs {
                    let a = match &q.kind {
                        QKind::Noul => Answer::Noul { yes: 0.97 },
                        QKind::Choice { options } => {
                            let is_kind = options.iter().any(|(n, _)| n == "unrelated");
                            let favoured = if options.iter().any(|(n, _)| n == "a_first") {
                                if a_block.contains("general") {
                                    "a_first"
                                } else {
                                    "b_first"
                                }
                            } else if is_kind {
                                kind
                            } else {
                                options[0].0.as_str()
                            };
                            let said = if is_kind && q.key.ends_with('r') {
                                twin
                            } else {
                                favoured
                            };
                            let rest = 0.05 / (options.len() - 1) as f64;
                            Answer::Choice {
                                pick: said.into(),
                                probabilities: options
                                    .iter()
                                    .map(|(n, _)| if n == favoured { 0.95 } else { rest })
                                    .collect(),
                                confidence: 0.95,
                            }
                        }
                        QKind::Score { levels } => Answer::Score {
                            score: 0.0,
                            probabilities: (0..levels.len())
                                .map(|i| if i == 0 { 1.0 } else { 0.0 })
                                .collect(),
                            confidence: 0.95,
                        },
                    };
                    answers.insert(q.key.clone(), a);
                }
                Ok(Answers {
                    answers,
                    model: "rule-decider".into(),
                    input_tokens: 0,
                })
            }))
        }

        /// A neighbour in the base, then a judged capture that is shown it: (anchor, neighbour).
        async fn captured_beside_a_neighbour(core: &mut Core) -> (String, String) {
            core.synthesizer = Some(Arc::new(Showing));
            // The fake embedder's cosines are noise; every neighbour is worth showing here.
            core.weak_floor = -1.0;
            let first = core
                .ingest(
                    "general invoice workflow: open it, check it, pay it",
                    "web",
                    None,
                )
                .await
                .unwrap();
            drain(core).await;
            let neighbour = live(core, &first.id).await;
            let out = core
                .ingest("paying the march invoice from acme", "web", None)
                .await
                .unwrap();
            drain(core).await;
            (live(core, &out.id).await, neighbour)
        }

        /// The written artifact of a capture.
        async fn live(core: &Core, corpus_id: &str) -> String {
            core.store
                .artifacts_for_corpus(corpus_id)
                .await
                .unwrap()
                .into_iter()
                .find(|c| {
                    c.in_results() && c.provenance != crate::store::artifacts::Provenance::Passage
                })
                .expect("the window wrote an artifact")
                .id
        }

        /// The `link_kind` decision on these two, if they were asked about.
        async fn decision(
            core: &Core,
            x: &str,
            y: &str,
        ) -> Option<crate::store::decision_log::Logged> {
            let (a, b) = crate::store::links::canonical(x, y);
            core.store
                .last_logged(
                    "link_kind",
                    a,
                    Some(b),
                    &crate::jobs::associate::link_era(core),
                )
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn a_neighbour_shown_at_capture_is_linked_when_the_decision_model_is_sure() {
            let mut core = test_core().await;
            core.decider = linking("example", "example");
            let (anchor, neighbour) = captured_beside_a_neighbour(&mut core).await;

            let l = core
                .store
                .get_link(&anchor, &neighbour)
                .await
                .unwrap()
                .expect("the shown neighbour was linked");
            assert_eq!(l.state, LinkState::Related);
            assert_eq!(l.kind, Some(LinkKind::Example));
            assert_eq!(l.reason, None, "no text of the model's is stored");
            // The general workflow comes first, and held with the notes exchanged.
            let (a, _) = crate::store::links::canonical(&anchor, &neighbour);
            assert_eq!(l.first_is_a, Some(a == neighbour));
            let from_anchor = core
                .store
                .links_from(
                    std::slice::from_ref(&anchor),
                    &[LinkState::Related],
                    core.associate.half_life_days,
                    crate::store::now(),
                    core.associate.show_min,
                    10,
                )
                .await
                .unwrap();
            let line = from_anchor
                .iter()
                .find(|t| t.other == neighbour)
                .and_then(|t| t.reason.clone());
            assert_eq!(line.as_deref(), Some("What this is an example of"));
            let d = decision(&core, &anchor, &neighbour).await.expect("asked");
            assert_eq!(d.band, crate::core::judging::band::Band::Act);
            assert_eq!(d.subject_kind, "shown");
        }

        #[tokio::test]
        async fn a_neighbour_it_calls_unrelated_gets_no_link() {
            let mut core = test_core().await;
            core.decider = linking("unrelated", "unrelated");
            let (anchor, neighbour) = captured_beside_a_neighbour(&mut core).await;
            let d = decision(&core, &anchor, &neighbour).await.expect("asked");
            assert_eq!(d.pick, "unrelated");
            assert!(
                core.store
                    .get_link(&anchor, &neighbour)
                    .await
                    .unwrap()
                    .is_none(),
                "an unrelated neighbour leaves no row"
            );
        }

        #[tokio::test]
        async fn a_torn_neighbour_is_an_unsure_row_and_a_pick_creates_the_link() {
            let mut core = test_core().await;
            core.decider = linking("example", "background");
            let (anchor, neighbour) = captured_beside_a_neighbour(&mut core).await;
            assert!(
                core.store
                    .get_link(&anchor, &neighbour)
                    .await
                    .unwrap()
                    .is_none()
            );
            let d = decision(&core, &anchor, &neighbour).await.expect("asked");
            assert_eq!(d.band, crate::core::judging::band::Band::Ask);
            assert_eq!(
                d.state,
                crate::store::decision_log::State::Open,
                "an Unsure row"
            );
            assert!(
                crate::core::judging::Applier::unchanged(
                    &crate::jobs::associate::LinkApplier,
                    &core,
                    &d
                )
                .await
                .unwrap(),
                "no link row yet is not a change"
            );

            crate::core::judging::answer(&core, &d.id, Some("alternative"))
                .await
                .unwrap();
            let l = core
                .store
                .get_link(&anchor, &neighbour)
                .await
                .unwrap()
                .expect("the pick made the link");
            assert_eq!(l.state, LinkState::Related);
            assert_eq!(l.kind, Some(LinkKind::Alternative));
            assert!(
                l.weight >= core.associate.show_min * 2.0,
                "shown above the floor: {}",
                l.weight
            );
        }

        #[tokio::test]
        async fn link_shown_spends_no_more_than_the_judging_budget() {
            let mut core = test_core().await;
            core.decider = linking("example", "example");
            let (anchor, neighbour) = captured_beside_a_neighbour(&mut core).await;
            core.decide_cfg.max_per_tick = 0;
            let other = crate::core::test_support::seed_artifact(&core, "elsewhere").await;
            let made =
                crate::jobs::associate::link_shown(&core, &anchor, std::slice::from_ref(&other))
                    .await
                    .unwrap();
            assert_eq!(made, 0);
            assert!(
                decision(&core, &anchor, &other).await.is_none(),
                "a call was spent past the budget"
            );
            // Not asked now, and not forgotten: it waits as a learning link for the sweep.
            let waiting = core
                .store
                .get_link(&anchor, &other)
                .await
                .unwrap()
                .expect("the neighbour was recorded for the sweep");
            assert_eq!(waiting.state, LinkState::Learning);
            assert!(waiting.shown);
            let _ = neighbour;
        }
    }
}
