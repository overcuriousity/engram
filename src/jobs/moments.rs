//! The moments stage: read time out of one artifact, cheapest first.
//!
//! Runs once an artifact's embedding has landed, so the vector the classifier
//! compares already exists. Idempotent: it replaces what it read last time and
//! never touches a row somebody set. The one model call is behind a fired
//! `remind` intent, and a base with no chat model reads the date by rule.

use crate::core::moments::{
    absolute_dates, classify, cue, relative_date, validate_rule, zone, Found, Intent, DEFAULT_HOUR,
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
        .unwrap_or_else(|| core.time.default_tz.clone());
    let tz = zone(Some(&tz_name));
    let tz_name = if tz_name.is_empty() { "UTC".to_string() } else { tz_name };
    let month_first = src.metadata["locale"].as_str().is_some_and(|l| l.eq_ignore_ascii_case("en-US"));
    let first = art.ordinal == 0;

    core.store.delete_read_moments(artifact_id).await?;

    // 3. Absolute dates, every artifact, no model. A date a kept row already
    // covers is not inserted again: `delete_read_moments` leaves behind what
    // has been done, pushed or snoozed, and re-reading the same prose must not
    // put a second copy of it on the page.
    let found = absolute_dates(&art.text, src.created_at, tz, month_first);
    for f in &found {
        if core.store.has_moment_at(artifact_id, Kind::Event, f.at).await? {
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

    // 1–2. Intent: forced by the door, the cue table, then the classifier. A
    // door's say-so is recorded as a cue — explicit, and still the stage's
    // own row, so a re-read replaces it rather than doubling it.
    let (intent, source) = if src.metadata["intent"].as_str() == Some("remind") {
        (Some(Intent::Remind), Source::Cue)
    } else if let Some(i) = cue(&art.text) {
        (Some(i), Source::Cue)
    } else {
        let protos = core.prototypes().await?;
        match core.vectors.dense_of(artifact_id).await? {
            Some(v) => (classify(&v, &protos.vectors, protos.line).map(|(i, _)| i), Source::Classified),
            None => (None, Source::Classified),
        }
    };

    match intent {
        // The operator's undo outranks the cue. Un-filing restores an origin
        // that is in `JOURNALABLE` again, and this stage re-runs on every
        // embed over text that still carries the same journal cue — so
        // without this the day page's "not an entry" and the capture
        // receipt's undo held only until the next reindex or embed-model
        // switch. See `ingest::ENTRY_DECLINED`.
        Some(Intent::Journal)
            if JOURNALABLE.contains(&src.origin.as_str())
                && src.metadata[crate::core::ingest::ENTRY_DECLINED] != serde_json::Value::Bool(true) =>
        {
            core.set_entry(cid, true).await?;
        }
        Some(Intent::Remind) => {
            let (at, rule) = date_reminder(core, &art.text, src.created_at, tz, &tz_name, &found).await;
            if let Some(at) = at
                && core.store.has_moment_at(artifact_id, Kind::Due, at).await?
            {
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
        }
        Some(Intent::Journal) | None => {}
    }
    Ok(())
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
                // The rule survives a model that would not commit to a
                // date. Returning only on `at` dropped it on the way to the
                // table below — "remind me every Friday to send the
                // invoice", answered `{"when": null, "rule":
                // "FREQ=WEEKLY;BYDAY=FR"}`, was stored as a one-off that
                // fired once and never came back.
                if at.is_some() {
                    return (at, rule);
                }
                if rule.is_some() {
                    return (at_by_rule(text, captured_at, tz, found), rule);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not date the reminder with the model; reading it by rule")
            }
        }
    }
    (at_by_rule(text, captured_at, tz, found), None)
}

/// The date without a model: the relative table, else what step 3 found; the
/// nearest future date wins. Also the anchor for a recurrence the model
/// described but would not date.
fn at_by_rule(text: &str, captured_at: i64, tz: chrono_tz::Tz, found: &[Found]) -> Option<i64> {
    let mut candidates: Vec<i64> = found.iter().map(|f| f.at).filter(|a| *a > captured_at).collect();
    if let Some(r) = relative_date(text, captured_at, tz) {
        candidates.push(r.at);
    }
    candidates.into_iter().min()
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

    /// A model that describes the recurrence but will not commit to a first
    /// date. The rule used to fall out on the way to the relative table, so
    /// "remind me every Friday to send the invoice" was stored as a one-off:
    /// it fired once and never came back.
    #[tokio::test]
    async fn a_rule_survives_a_model_that_would_not_name_the_date() {
        let mut core = test_core().await;
        core.reminder = Some(std::sync::Arc::new(FakeCompleter {
            reply: Some(r#"{"when":null,"rule":"FREQ=WEEKLY;BYDAY=FR","what":"send the invoice"}"#.into()),
        }));
        let (_, aid) =
            first_passage(&core, "Remind me every friday to send the invoice tomorrow", "ui", Some("Europe/Berlin"))
                .await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows[0].moment.rule.as_deref(), Some("FREQ=WEEKLY;BYDAY=FR"), "the recurrence is not the date");
        assert!(rows[0].moment.at.is_some(), "and the table anchored it");
    }

    /// The same, with nothing to anchor it: the rule is still what the note
    /// says, and is there for whoever sets the date.
    #[tokio::test]
    async fn an_undated_recurrence_still_carries_its_rule() {
        let mut core = test_core().await;
        core.reminder = Some(std::sync::Arc::new(FakeCompleter {
            reply: Some(r#"{"when":null,"rule":"FREQ=WEEKLY;BYDAY=FR","what":"send the invoice"}"#.into()),
        }));
        let (_, aid) = first_passage(&core, "Remind me to send the invoice regularly", "ui", None).await;
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows[0].moment.rule.as_deref(), Some("FREQ=WEEKLY;BYDAY=FR"));
        assert!(rows[0].moment.at.is_none(), "nothing in the text to anchor it");
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
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
    }

    /// "Not an entry" on the day page, and the undo on the capture receipt,
    /// have to outlive the reading that filed it. Un-filing restores an origin
    /// that is in `JOURNALABLE` again, and this stage runs on every embed —
    /// so a reindex or a change of embed model used to file the note straight
    /// back under the same cue that filed it the first time.
    #[tokio::test]
    async fn a_note_the_operator_took_out_of_the_journal_stays_out() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Vandaag eindelijk de tuin gedaan.", "cli")).await.unwrap();
        core.store.set_corpus_origin(&out.id, "cli").await.unwrap();
        drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");

        core.set_entry(&out.id, false).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "cli");
        run(&core, &aid).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "cli", "re-read, re-filed");

        // Filing it again by hand is the operator overruling their own undo,
        // and the refusal goes with it.
        core.set_entry(&out.id, true).await.unwrap();
        core.set_entry(&out.id, false).await.unwrap();
        core.set_entry(&out.id, true).await.unwrap();
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "journal");
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
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        run(&core, &aid).await.unwrap();
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.source, Source::Cue);
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
