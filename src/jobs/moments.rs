//! The moments stage: read time out of one artifact, cheapest first.
//!
//! Runs once an artifact's embedding has landed, so the vector the classifier
//! compares already exists. Idempotent: it replaces what it read last time and
//! never touches a row somebody set. The one model call is behind a fired
//! `remind` intent, and a base with no chat model reads the date by rule.

use crate::core::moments::{
    absolute_dates, classify, clock_offset, cue, default_zone_name, nearest, relative_date,
    validate_rule, weak_cue, zone, Found, Intent, DEFAULT_HOUR,
};
use crate::core::Core;
use crate::error::Result;
use crate::infer::prompt;
use crate::store::moments::{Kind, NewMoment, Source};
use chrono::TimeZone;

/// Origins the journal cue and classifier may rewrite. Not `api` or `mcp`:
/// a program that wanted an entry says so with `origin`.
pub const JOURNALABLE: &[&str] = &[crate::core::ingest::ORIGIN_WEB, "ui", "cli", crate::core::ingest::ORIGIN_SHARE, "extension"];

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let art = core.store.get_artifact(artifact_id).await?;
    let Some(cid) = art.corpus_id.as_deref() else { return Ok(()) };
    let src = core.store.get_corpus(cid).await?;
    let tz_name = src.metadata["tz"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| default_zone_name(&core.time.default_tz));
    let tz = zone(Some(&tz_name));
    // The zone as the zone table spells it, and never as the metadata spelled
    // it. What is stored on the moment is what the day page and the recurrence
    // read the wall-clock back out of, so a name that did not resolve must not
    // be written down as though it had: the dates were read in UTC, and the row
    // has to say UTC.
    let tz_name = tz.name().to_string();
    let month_first = src.metadata["locale"].as_str().is_some_and(|l| l.eq_ignore_ascii_case("en-US"));
    let first = art.ordinal == 0;

    core.store.delete_read_moments(artifact_id).await?;

    // 3. Absolute dates, every artifact, no model. A date a kept row already
    // covers is not inserted again: `delete_read_moments` leaves behind what
    // has been done, pushed or snoozed, and re-reading the same prose must not
    // put a second copy of it on the page.
    let found = absolute_dates(&art.text, src.created_at, tz, month_first);
    for f in &found {
        if core.store.has_moment_at(artifact_id, Kind::Event, Some(f.at)).await? {
            continue;
        }
        core.store
            .insert_moment(&NewMoment {
                artifact_id: artifact_id.into(),
                kind: Kind::Event,
                at: Some(f.at),
                tz: tz_name.clone(),
                rule: None,
                source: Source::Extracted,
                span: Some(f.span.clone()),
            })
            .await?;
    }
    if !first {
        return Ok(());
    }

    // 1–4. Intent, in order of how much the reading is worth: forced by the
    // door, a cue that decides on its own, the classifier, and last a weak cue
    // — a day word, which used to overrule the vector on the strength of being
    // the first word in the note. See `moments::Strength`. A door's say-so is
    // recorded as a cue: explicit, and still the stage's own row, so a re-read
    // replaces it rather than doubling it.
    let mut score = None;
    let (intent, source, by) = if src.metadata["intent"].as_str() == Some("remind") {
        (Some(Intent::Remind), Source::Cue, "forced")
    } else if let Some(i) = cue(&art.text) {
        (Some(i), Source::Cue, "cue")
    } else {
        let protos = core.prototypes().await?;
        match core.vectors.dense_of(artifact_id).await? {
            Some(v) => {
                score = nearest(&v, protos).map(|(_, s)| s);
                match classify(&v, protos) {
                    Some((i, _)) => (Some(i), Source::Classified, "classified"),
                    // The vector placed it nowhere. Only here does a day word
                    // get to speak, and only for the recall it was in the
                    // table for: a note resembling nothing that opens with
                    // *Heute* is still an entry.
                    None => (weak_cue(&art.text), Source::Cue, "weak cue"),
                }
            }
            None => (None, Source::Classified, "unembedded"),
        }
    };
    record_intent(core, cid, &src, intent, by, score).await?;

    // What the operator has already said this note is not. The stage derives
    // the intent again on every re-embed, so a refusal that did not outlive
    // one would be overruled by a reindex or a switched embed model — on a
    // note somebody had already put back. `Core::set_entry` and
    // `Core::set_reminder` are what write it.
    if intent.is_some_and(|i| crate::core::moments::intent_refused(&src.metadata, i)) {
        return Ok(());
    }

    match intent {
        Some(Intent::Journal) if JOURNALABLE.contains(&src.origin.as_str()) => {
            core.set_entry(cid, true).await?;
        }
        Some(Intent::Remind) => {
            let (at, rule) = date_reminder(core, &art.text, src.created_at, tz, &tz_name, &found).await;
            // A reminder the operator did not ask for has to be corroborated
            // by a date, and this is where that is cheapest to ask: every date
            // path has already run. A *cued* remind is somebody typing "remind
            // me", and an undated one is a question the band asks them. A
            // *classified* remind with no date anywhere in the note is the
            // weak case twice over — a guess about a note that names no time —
            // and it used to become an undated row nagging for a date it never
            // had. It stays an ordinary capture instead.
            if source == Source::Classified && at.is_none() && rule.is_none() {
                tracing::debug!(artifact_id, "a classified reminder with no date is left as a capture");
                return Ok(());
            }
            // Undated included: `None` is an instant the guard understands, and
            // an undated reminder that was finished is exactly the row
            // `delete_read_moments` keeps and this must not read back fresh.
            if core.store.has_moment_at(artifact_id, Kind::Due, at).await? {
                return Ok(());
            }
            core.store
                .insert_moment(&NewMoment {
                    artifact_id: artifact_id.into(),
                    kind: Kind::Due,
                    at,
                    tz: tz_name.clone(),
                    rule,
                    source,
                    span: None,
                })
                .await?;
            core.store.rearm_remind().await?;
            if let Err(e) = confirm_created(core, &art, at, tz).await {
                // Best-effort: a note that failed to say "reminder set" is
                // still a reminder that was set, and the badge on the
                // artifact says so regardless of whether a push channel is
                // configured or reachable.
                tracing::warn!(error = %e, "could not push the capture-time confirmation");
            }
        }
        Some(Intent::Journal) | None => {}
    }
    Ok(())
}

/// Write down what was read and how sure it was.
///
/// `classify` returned a score and the stage dropped it, so nothing in the
/// base recorded whether a verdict was a walkover or a hair over the line —
/// and a near-miss, the reading that would have been most useful to see, left
/// no trace at all. Three keys on the corpus, written whether or not anything
/// fired. Cheap, and it makes every later argument about `time.intent_at` an
/// measurement instead of an opinion.
async fn record_intent(
    core: &Core,
    corpus_id: &str,
    src: &crate::store::corpora::Corpus,
    intent: Option<Intent>,
    by: &str,
    score: Option<f32>,
) -> Result<()> {
    let mut meta = src.metadata.clone();
    meta["intent_read"] = serde_json::Value::String(
        intent.map(|i| i.as_str().to_string()).unwrap_or_else(|| "none".into()),
    );
    meta["intent_by"] = serde_json::Value::String(by.to_string());
    match score.and_then(|s| serde_json::Number::from_f64(f64::from(s))) {
        Some(n) => meta["intent_score"] = serde_json::Value::Number(n),
        None => {
            if let Some(m) = meta.as_object_mut() {
                m.remove("intent_score");
            }
        }
    }
    core.store.set_corpus_metadata(corpus_id, &meta).await
}

/// Step 4: the model if there is one, else the relative table, else what step
/// 3 found; the nearest future date wins. `(None, None)` is an undated reminder.
async fn date_reminder(
    core: &Core,
    text: &str,
    captured_at: i64,
    tz: chrono_tz::Tz,
    tz_name: &str,
    found: &[Found],
) -> (Option<i64>, Option<String>) {
    if let Some(model) = core.reminder.clone() {
        let now_local = tz
            .timestamp_opt(captured_at, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d %H:%M (%A)").to_string())
            .unwrap_or_default();
        let permit = core.gate.background().await;
        let reply = model.complete(prompt::REMIND_SYSTEM, &prompt::remind_prompt(&now_local, tz_name, text)).await;
        permit.finished();
        match reply.and_then(|r| prompt::parse_remind(&r)) {
            Ok(r) => {
                let at = r.when.as_deref().and_then(|w| parse_local(w, tz));
                let rule = r.rule.filter(|rule| match validate_rule(rule) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(rule = %rule, error = %e, "rule outside the subset; the reminder is single");
                        false
                    }
                });
                if at.is_some() {
                    return (at, rule);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not date the reminder with the model; reading it by rule")
            }
        }
    }
    let mut candidates: Vec<i64> = found.iter().map(|f| f.at).filter(|a| *a > captured_at).collect();
    if let Some(r) = relative_date(text, captured_at, tz) {
        candidates.push(r.at);
    }
    // An offset inside the day — *in 10 minuten*, *in einer halben stunde*.
    // Behind the model deliberately: the model reads a typo, a quarter of an
    // hour and a wording no table lists, and this only covers the plain forms.
    // It is here because without it the rule path cannot express the shape at
    // all: `relative_date` considers dates after today and nothing shorter.
    if let Some(o) = clock_offset(text, captured_at) {
        candidates.push(o.at);
    }
    (candidates.into_iter().min(), None)
}

/// The push that says a reminder was just set, at capture time and
/// independent of the due-time ladder — the only feedback a note gets today
/// is the ladder itself, which can be days away from the moment somebody
/// typed it.
async fn confirm_created(core: &Core, art: &crate::store::artifacts::Chunk, at: Option<i64>, tz: chrono_tz::Tz) -> Result<()> {
    let opening = art.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect::<String>();
    let title = art.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| opening.clone());
    let message = match at {
        Some(at) => format!("{opening}\n{}", crate::web::due::when_words(at, core.clock.now(), tz)),
        None => opening,
    };
    crate::jobs::remind::notify_now(core, &format!("Reminder set: {title}"), &message).await
}

/// `2026-09-04T09:00` or `2026-09-04` in the reader's zone.
fn parse_local(s: &str, tz: chrono_tz::Tz) -> Option<i64> {
    let dt = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_hms_opt(DEFAULT_HOUR, 0, 0).unwrap())
        })
        .ok()?;
    tz.from_local_datetime(&dt)
        .single()
        .or_else(|| tz.from_local_datetime(&dt).earliest())
        .map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::infer::fake::FakeCompleter;
    use crate::jobs::test_support::drain;
    use crate::store::moments::{Kind, Source};

    /// Capture, run the pipeline until the first passage is embedded, and
    /// return the corpus and that artifact.
    async fn first_passage(core: &Core, text: &str, origin: &str, tz: Option<&str>) -> (String, String) {
        let out = core.ingest_capture(Capture::new(text, origin).with_tz(tz.map(String::from))).await.unwrap();
        drain(core).await;
        let arts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        (out.id, arts[0].id.clone())
    }

    /// Point the classifier straight at an artifact's own vector, so it scores
    /// 1.0 and fires. The prototypes a real base holds are unreachable from a
    /// test with a hash embedder; what is under test here is what the stage
    /// does with a verdict, not how the verdict was reached.
    async fn classifier_fires_on(core: &mut Core, artifact_id: &str, as_intent: Intent) {
        let v = core.vectors.dense_of(artifact_id).await.unwrap().expect("embedded");
        core.protos = std::sync::Arc::new(tokio::sync::OnceCell::new_with(Some(
            crate::core::moments::Protos { vectors: vec![(as_intent, v)], decoys: vec![], line: 0.5 },
        )));
    }

    #[tokio::test]
    async fn a_classified_reminder_needs_a_date_and_a_cued_one_does_not() {
        // The expensive false positive, and the one signal that costs nothing
        // to consult: every date path has already run by the time the intent
        // branch is reached. A guess about a note that names no time used to
        // become an undated row on the band, nagging for a date the note never
        // had. Somebody typing "remind me" is not a guess, so an undated
        // *cued* reminder still stands — there the question is the point.
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) = first_passage(&core, "The gutters need clearing at some point", "ui", None).await;
        classifier_fires_on(&mut core, &aid, Intent::Remind).await;
        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "no date, no reminder");
        let meta = core.store.get_corpus(&core.store.get_artifact(&aid).await.unwrap().corpus_id.unwrap())
            .await
            .unwrap()
            .metadata;
        assert_eq!(meta["intent_read"], "remind", "the reading is still recorded");
        assert_eq!(meta["intent_by"], "classified");

        // The same verdict on a note that does name a time is a reminder.
        let (_, dated) = first_passage(&core, "Clear the gutters tomorrow", "ui", Some("Europe/Berlin")).await;
        classifier_fires_on(&mut core, &dated, Intent::Remind).await;
        run(&core, &dated).await.unwrap();
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1);

        // And a cue with no date anywhere is an undated reminder, as before.
        let (_, cued) = first_passage(&core, "Remind me to clear the gutters", "ui", None).await;
        run(&core, &cued).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.moment.at.is_none() && r.moment.source == Source::Cue));
    }

    #[tokio::test]
    async fn not_a_reminder_takes_the_row_back_and_outlives_a_re_read() {
        // The journal side has had a durable refusal for a while; this is the
        // reminder side of it. Marking a misread row *done* was the only way
        // to be rid of it, which recorded a task finished where there had
        // never been a task — and any re-embed put it straight back.
        let mut core = test_core().await;
        core.reminder = None;
        let (cid, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1);

        core.set_reminder(&aid, false).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "the row is gone");
        assert!(core.store.open_due_for_artifact(&aid).await.unwrap().is_none(), "and not merely out of the band");

        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "a re-read does not overrule it");
        assert!(crate::core::moments::intent_refused(
            &core.store.get_corpus(&cid).await.unwrap().metadata,
            Intent::Remind
        ));

        // The undo hands the note back to the stage rather than restoring a
        // row from memory, so what comes back is what the note says.
        core.set_reminder(&aid, true).await.unwrap();
        drain(&core).await;
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1, "and back it comes");
    }

    #[tokio::test]
    async fn a_refused_entry_and_a_refused_reminder_do_not_stand_in_for_each_other() {
        let mut core = test_core().await;
        core.reminder = None;
        let (cid, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        core.set_reminder(&aid, false).await.unwrap();
        let meta = core.store.get_corpus(&cid).await.unwrap().metadata;
        assert!(crate::core::moments::intent_refused(&meta, Intent::Remind));
        assert!(!crate::core::moments::intent_refused(&meta, Intent::Journal), "one refusal is not the other");
    }

    #[tokio::test]
    async fn a_cue_with_a_relative_date_and_no_model_becomes_a_dated_reminder() {
        let mut core = test_core().await;
        core.reminder = None;
        let (cid, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.source, Source::Cue);
        assert_eq!(rows[0].moment.tz, "Europe/Berlin");
        let captured = core.store.get_corpus(&cid).await.unwrap().created_at;
        let at = rows[0].moment.at.unwrap();
        assert!(at > captured && at < captured + 2 * 86_400, "tomorrow 09:00 lies within two days");
    }

    /// The reported shape, through the whole stage: *erinnere mich in 10
    /// minuten an xy* came back dated to the next day at the right clock time.
    /// The model owns this wording — it reads the typos and the phrasings no
    /// table lists — but where there is none, the rule path can now say a time
    /// inside the day at all, which it could not before.
    #[tokio::test]
    async fn an_offset_inside_the_day_is_dated_without_a_model() {
        let mut core = test_core().await;
        core.reminder = None;
        let (cid, aid) =
            first_passage(&core, "Erinnere mich in 10 Minuten an den Ofen", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        let captured = core.store.get_corpus(&cid).await.unwrap().created_at;
        assert_eq!(
            rows[0].moment.at,
            Some(captured + 600),
            "ten minutes after the second it was captured, and on that day"
        );
    }

    #[tokio::test]
    async fn a_re_read_does_not_resurrect_a_finished_reminder() {
        // Every embed re-arms this stage, so a reindex or an embed-model
        // switch runs it again over the same prose. What the operator already
        // finished must not come back and must not be pushed again.
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        let id = rows[0].moment.id.clone();
        core.store.mark_done(&id, crate::store::now()).await.unwrap();

        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "it stays done");
        assert!(core.store.moment(&id).await.unwrap().unwrap().done_at.is_some(), "and it is the same row");
    }

    #[tokio::test]
    async fn a_re_read_does_not_resurrect_a_finished_undated_reminder() {
        // The dated case above was covered; the undated one was not, and it
        // was the one that broke. `delete_read_moments` keeps a finished row
        // whether or not it has a date, but the guard against reading it back
        // was only consulted when there was a date to compare — so "remind me
        // to call the bank", finished, returned to the band on the next embed
        // and gained another copy of itself on every one after that.
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) = first_passage(&core, "Remind me to call the bank", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].moment.at.is_none(), "undated: there is no date in the prose");
        let id = rows[0].moment.id.clone();
        core.store.mark_done(&id, crate::store::now()).await.unwrap();

        run(&core, &aid).await.unwrap();
        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "it stays done");
        assert!(core.store.moment(&id).await.unwrap().unwrap().done_at.is_some(), "and it is the same row");
    }

    #[tokio::test]
    async fn a_re_read_does_not_double_a_date_already_pushed() {
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) =
            first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let id = core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.id.clone();
        core.store.mark_notified(std::slice::from_ref(&id), crate::store::now()).await.unwrap();

        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "one reminder, not two");
        assert_eq!(rows[0].moment.id, id, "the row that was pushed, not a fresh one owed a push");
    }

    #[tokio::test]
    async fn the_model_dates_a_reminder_when_it_can() {
        let mut core = test_core().await;
        core.reminder = Some(std::sync::Arc::new(FakeCompleter {
            reply: Some(r#"{"when":"2026-09-04T09:00","rule":"FREQ=WEEKLY;BYDAY=FR","what":"send the invoice"}"#.into()),
        }));
        let (_, aid) = first_passage(&core, "Remind me every friday to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows[0].moment.rule.as_deref(), Some("FREQ=WEEKLY;BYDAY=FR"));
        let local = chrono_tz::Tz::Europe__Berlin.timestamp_opt(rows[0].moment.at.unwrap(), 0).unwrap();
        assert_eq!(local.format("%Y-%m-%d %H:%M").to_string(), "2026-09-04 09:00");
    }

    #[tokio::test]
    async fn a_rule_outside_the_subset_is_dropped_and_the_moment_is_single() {
        let mut core = test_core().await;
        core.reminder = Some(std::sync::Arc::new(FakeCompleter {
            reply: Some(r#"{"when":"2026-09-04T09:00","rule":"FREQ=WEEKLY;BYDAY=2FR","what":"x"}"#.into()),
        }));
        let (_, aid) = first_passage(&core, "Remind me to send the invoice", "ui", None).await;
        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.rule.is_none());
    }

    #[tokio::test]
    async fn a_reminder_nobody_could_date_is_kept_undated() {
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) = first_passage(&core, "Remind me to send the invoice", "ui", None).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].moment.at.is_none());
    }

    #[tokio::test]
    async fn the_default_fake_model_says_no_date_and_the_rules_take_over() {
        // `test_core` wires a reminder that answers `when: null`; the relative
        // table is what dates this one.
        let core = test_core().await;
        let (_, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", None).await;
        run(&core, &aid).await.unwrap();
        assert!(core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.at.is_some());
    }

    #[tokio::test]
    async fn absolute_dates_become_events_on_a_note_nobody_flagged() {
        let core = test_core().await;
        let (_, aid) =
            first_passage(&core, "Zahnarzt am 12.9. um 10 Uhr, danach 2026-10-01 Steuer.", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let ev = core.store.event_moments_between(0, i64::MAX).await.unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].moment.kind, Kind::Event);
        assert_eq!(ev[0].moment.span.as_deref(), Some("12.9."));
        assert_eq!(ev[0].moment.source, Source::Extracted);
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty(), "no reminder was meant");
    }

    #[tokio::test]
    async fn a_second_run_replaces_read_rows_and_keeps_a_set_one() {
        let core = test_core().await;
        let (_, aid) = first_passage(&core, "Deadline 2026-10-01", "ui", None).await;
        run(&core, &aid).await.unwrap();
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(1),
                tz: "UTC".into(),
                rule: None,
                source: Source::Set,
                span: None,
            })
            .await
            .unwrap();
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.event_moments_between(0, i64::MAX).await.unwrap().len(), 1, "not two");
        assert_eq!(core.store.open_due(0, i64::MAX).await.unwrap().len(), 1, "the set row stayed");
    }

    #[tokio::test]
    async fn the_occurrence_a_completion_armed_survives_the_next_re_read() {
        // Every embed re-arms this stage. The successor `complete_moment` puts
        // on the band carried the parent's `cue`, so the stage took it for
        // something it had read and deleted it — and the re-read then found the
        // original instant still on the artifact, still done, and armed nothing
        // in its place. The recurrence ended at its first completion, silently.
        let mut core = test_core().await;
        core.reminder = Some(std::sync::Arc::new(FakeCompleter {
            reply: Some(r#"{"when":"2026-09-04T09:00","rule":"FREQ=WEEKLY;BYDAY=FR","what":"water the plants"}"#.into()),
        }));
        let (_, aid) = first_passage(&core, "Remind me every friday to water the plants", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let first = core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.id.clone();
        core.complete_moment(&first).await.unwrap();
        let armed = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(armed.len(), 1, "the completion armed the next friday");
        assert_eq!(armed[0].moment.source, Source::Armed);
        let next = armed[0].moment.id.clone();

        run(&core, &aid).await.unwrap();
        let after = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(after.len(), 1, "still one open occurrence");
        assert_eq!(after[0].moment.id, next, "and it is the one the completion armed");
    }

    #[tokio::test]
    async fn a_date_the_operator_corrected_is_not_read_back_on_the_next_re_read() {
        // Moving a row updates it in place, so nothing is left parked at the
        // instant the stage read. Without `moved_from` the next re-read simply
        // made a second reminder at the date that had just been corrected away
        // from — and that one, not the corrected one, is what pushed.
        let mut core = test_core().await;
        core.reminder = None;
        let (_, aid) = first_passage(&core, "Remind me tomorrow to send the invoice", "ui", Some("Europe/Berlin")).await;
        run(&core, &aid).await.unwrap();
        let row = core.store.open_due(0, i64::MAX).await.unwrap()[0].moment.clone();
        let read_at = row.at.unwrap();
        core.store.move_moment(&row.id, read_at + 3 * 86_400, "Europe/Berlin").await.unwrap();

        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1, "one reminder, on the date the operator meant");
        assert_eq!(rows[0].moment.id, row.id);
        assert_eq!(rows[0].moment.at, Some(read_at + 3 * 86_400));
        assert_eq!(rows[0].moment.moved_from, Some(read_at), "and the misreading is kept");
    }

    #[tokio::test]
    async fn an_undone_journal_filing_is_not_filed_again_by_the_next_re_read() {
        // The undo put `origin` back and nothing recorded that it had happened,
        // so this stage — which derives the intent afresh on every embed — filed
        // the note as an entry again on the next reindex.
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Vandaag eindelijk de tuin gedaan.", "cli")).await.unwrap();
        core.store.set_corpus_origin(&out.id, "cli").await.unwrap();
        drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap().into_iter().find(|c| c.in_results()).expect("a live artifact").id;
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");

        core.set_entry(&out.id, false).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "cli");
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "cli", "the refusal outlives the re-read");

        // And it is a refusal, not a ban: filing it by hand withdraws it.
        core.set_entry(&out.id, true).await.unwrap();
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
    }

    #[tokio::test]
    async fn a_journal_cue_files_the_note_as_an_entry_from_the_stage_too() {
        // The capture door already files a cued entry (see `ingest_capture`);
        // the stage is what files one whose origin the door left alone, and
        // what the classifier would do with a near-prototype note. The fake
        // embedder hashes text, so the classifier itself is pinned in
        // `core::moments` over vectors and not staged here.
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Vandaag eindelijk de tuin gedaan.", "cli")).await.unwrap();
        core.store.set_corpus_origin(&out.id, "cli").await.unwrap();
        drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap().into_iter().find(|c| c.in_results()).expect("a live artifact").id;
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
    }

    #[tokio::test]
    async fn a_forced_intent_skips_the_classifier() {
        let mut core = test_core().await;
        core.reminder = None;
        let out = core
            .ingest_capture(Capture::new("send the invoice tomorrow", "cli").with_intent(Some(Intent::Remind)))
            .await
            .unwrap();
        drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap().into_iter().find(|c| c.in_results()).expect("a live artifact").id;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.source, Source::Cue);
    }

    /// The reported gap: capture returns before the stage has even run, and
    /// the only feedback that ever followed was the due-time ladder — days
    /// away for a reminder set well ahead. This is the second signal, right
    /// when the intent is resolved, independent of that ladder entirely.
    ///
    /// Dated ten days out on purpose: inside the ladder's 48-hour band a
    /// wake would be armed and due immediately, and `drain` would run it
    /// too, leaving this test unable to tell the confirmation apart from
    /// the first rung. Ten days out, only the confirmation fires.
    #[tokio::test]
    async fn a_new_reminder_is_confirmed_right_away_not_only_on_the_ladder() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("send the invoice"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        core.reminder = None;
        core.store
            .control
            .set_notify(&core.store.subject, &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}))
            .await
            .unwrap();
        first_passage(&core, "Remind me in 10 days to send the invoice", "ui", Some("Europe/Berlin")).await;
        // `expect(1)` on the server verifies on drop: exactly one push, from
        // the stage itself, while the ladder job it armed stays asleep.
    }

    #[tokio::test]
    async fn only_the_first_passage_is_read_for_intent() {
        let core = test_core().await;
        let long = format!("An article about scheduling.\n\n{}\n\nRemind me to call.", "Prose. ".repeat(400));
        let out = core.ingest_capture(Capture::new(&long, "ui")).await.unwrap();
        drain(&core).await;
        let arts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(arts.len() > 1, "the fixture needs more than one passage");
        for a in arts {
            run(&core, &a.id).await.unwrap();
        }
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }
}
