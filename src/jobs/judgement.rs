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

    core.store.delete_read_moments(anchor_id).await?;
    record_intent(core, corpus_id, &src, j.intent.as_deref()).await?;

    // Dates the note states without being the reminder: the day page's rows.
    for e in &j.events {
        let Some(at) = parse_local(e, tz) else {
            continue;
        };
        if core
            .store
            .has_moment_at(anchor_id, Kind::Event, Some(at))
            .await?
        {
            continue;
        }
        core.store
            .insert_moment(&NewMoment {
                artifact_id: anchor_id.into(),
                kind: Kind::Event,
                at: Some(at),
                tz: tz_name.clone(),
                rule: None,
                source: Source::Classified,
                span: None,
            })
            .await?;
    }

    // Relations to what the model was shown. Dedup and supersession stay
    // with the sweeps; the model proposes no merges.
    for l in &j.links {
        if l.artifact_id == anchor_id || !shown.iter().any(|s| s == &l.artifact_id) {
            continue;
        }
        core.store
            .relate_synthesized(anchor_id, &l.artifact_id, &l.reason)
            .await?;
    }

    let forced = src.metadata["intent"].as_str();
    match j.intent.as_deref() {
        Some("journal")
            if JOURNALABLE.contains(&src.origin.as_str())
                && !intent_refused(&src.metadata, Intent::Journal) =>
        {
            core.set_entry(corpus_id, true).await?;
        }
        Some("remind") => {
            if intent_refused(&src.metadata, Intent::Remind) {
                return Ok(());
            }
            let at = j.when.as_deref().and_then(|w| parse_local(w, tz));
            let rule = j.rule.clone().filter(|r| match validate_rule(r) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(rule = %r, error = %e, "rule outside the subset; the reminder is single");
                    false
                }
            });
            // A judged reminder with no date anywhere stays an ordinary
            // capture — a guess about a note that names no time used to
            // become an undated row nagging for a date it never had. A
            // *forced* remind is somebody saying "remind me" at the door,
            // and an undated one is a question the band asks them.
            let forced_remind = forced == Some("remind");
            if at.is_none() && rule.is_none() && !forced_remind {
                tracing::debug!(
                    corpus_id,
                    "a judged reminder with no date is left as a capture"
                );
                return Ok(());
            }
            // Undated included: `None` is an instant the guard understands,
            // and a finished undated reminder is exactly the row
            // `delete_read_moments` keeps and this must not read back fresh.
            if core.store.has_moment_at(anchor_id, Kind::Due, at).await? {
                return Ok(());
            }
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
        _ => {}
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

/// `2026-09-04T09:00` or `2026-09-04` in the reader's zone.
pub(crate) fn parse_local(s: &str, tz: chrono_tz::Tz) -> Option<i64> {
    let dt = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(DEFAULT_HOUR, 0, 0).unwrap())
        })
        .ok()?;
    crate::core::moments::resolve_local(dt, tz)
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
        apply(&core, &out.id, &aid, &j, &[]).await.unwrap();
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
