//! `Stage::Read`: every note is read once by the decision model after it is embedded — its kind,
//! whether it records a decision, whether it will go out of date, whether it stands alone.
//! Readings are stored at a fixed line; they are not decisions: no band, no Unsure row, no waiting.

use crate::core::Core;
use crate::core::judging::band::pick_and_top;
use crate::core::judging::catalogue::{category, pinned, self_contained, time_bound};
use crate::core::judging::{Asked, state};
use crate::error::{Error, Result};
use crate::store::jobs::Stage;

/// `pinned` is stored and has no effect: the only pin that ranks is a tag a person sets.
pub const PINNED_AT: f64 = 0.90;
pub const TIME_BOUND_AT: f64 = 0.5;
pub const SELF_CONTAINED_AT: f64 = 0.5;

/// The four questions the catalogue still holds (time_bound may have been dropped by the replay).
pub fn reading_questions() -> Vec<Asked> {
    vec![category(), pinned(), time_bound(), self_contained()]
}

/// Idle-only, as `relate::arm`: a queued read keeps its attempts.
pub async fn arm(core: &Core, artifact_id: &str) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Read, "artifact", artifact_id, 0)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let me = core.store.get_artifact(artifact_id).await?;
    if !me.in_results() {
        return Ok(());
    }
    let asked = reading_questions();
    let refs: Vec<&Asked> = asked.iter().collect();
    let room = state::room(core, &refs);
    let note = state::Note {
        title: me.title.as_deref(),
        day: None,
        text: &me.text,
    };
    // A reading describes and does not act, so here, and only here, a cut text is read: its opening.
    let whole = state::one(&note, &core.counter, usize::MAX).unwrap_or_default();
    let Some(shown) = core.counter.cut(&whole, room, true) else {
        tracing::info!(artifact_id, "no room to read this note");
        return Ok(());
    };
    let questions: Vec<_> = asked.iter().map(|a| a.question.clone()).collect();
    let permit = core.gate.background_light().await;
    let got = core.decider.decide(&shown, &questions).await;
    permit.finished();
    let answers = got?.answers;
    let rows = asked
        .iter()
        .map(|a| {
            answers
                .get(&a.question.key)
                .map(|answer| (a, answer))
                .ok_or_else(|| Error::InferenceRejected {
                    role: "decide",
                    detail: format!("the answer has none for \"{}\"", a.id),
                })
        })
        .collect::<Result<Vec<_>>>()?;
    core.store
        .put_readings(artifact_id, &rows, core.decider.model())
        .await?;

    // `category` is asked first. Its top pick is taken whatever its probability: a kind describes.
    let (pick, _) = pick_and_top(&rows[0].0.question, rows[0].1);
    let kind = crate::infer::prompt::normalize_category(&pick);
    if core.store.fill_category(artifact_id, &kind).await? {
        let chunk = core.store.get_artifact(artifact_id).await?;
        // A pending embed writes the whole payload anyway.
        if chunk.embed_state == crate::store::artifacts::EmbedState::Embedded {
            core.vectors
                .set_payload(&crate::vector::VectorPayload::edited(&chunk))
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::catalogue::pinned;
    use crate::core::test_support::{seed_artifact, test_core, test_core_without_synthesizer};
    use crate::infer::decide::{Answer, Answers, Kind, Question};
    use crate::infer::fake::RuleDecider;
    use crate::jobs::test_support::drain;
    use crate::store::artifacts::{Chunk, NewArtifact, NewMerged};
    use crate::store::jobs::Stage;
    use std::collections::HashMap;
    use std::sync::Arc;

    const KEYS: [&str; 4] = ["category", "pinned", "time_bound", "self_contained"];

    /// `category` picks `kind` at `top`; `pinned` says `pin`; anything else is asked a flat answer,
    /// so no other stage acts on it.
    fn reader(kind: &'static str, top: f64, pin: f64) -> Arc<RuleDecider> {
        Arc::new(RuleDecider::new(move |_, qs: &[Question]| {
            let mut answers = HashMap::new();
            for q in qs {
                let a = match (&q.kind, q.key.as_str()) {
                    (Kind::Choice { options }, "category") => {
                        let rest = (1.0 - top) / (options.len() - 1) as f64;
                        Answer::Choice {
                            pick: kind.into(),
                            probabilities: options
                                .iter()
                                .map(|(n, _)| if n == kind { top } else { rest })
                                .collect(),
                            confidence: top,
                        }
                    }
                    (_, "pinned") => Answer::Noul { yes: pin },
                    (Kind::Noul, _) => Answer::Noul { yes: 0.5 },
                    (Kind::Choice { options }, _) => Answer::Choice {
                        pick: options[0].0.clone(),
                        probabilities: vec![1.0 / options.len() as f64; options.len()],
                        confidence: 0.0,
                    },
                    (Kind::Score { levels }, _) => Answer::Score {
                        score: 0.0,
                        probabilities: vec![1.0 / levels.len() as f64; levels.len()],
                        confidence: 0.0,
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

    fn reads(d: &RuleDecider) -> Vec<(String, Vec<Question>)> {
        d.requests()
            .into_iter()
            .filter(|(_, qs)| qs.iter().any(|q| q.key == "category"))
            .collect()
    }

    async fn category_by(core: &Core, id: &str) -> Option<String> {
        sqlx::query_scalar!("SELECT category_by FROM artifacts WHERE id = ?", id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    async fn capture(core: &Core, text: &str) -> Vec<Chunk> {
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(text, "ui"))
            .await
            .unwrap();
        drain(core).await;
        core.store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .filter(Chunk::in_results)
            .collect()
    }

    async fn note(core: &Core, text: &str) -> String {
        let src = core.store.insert_corpus(text, "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: text.into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn a_new_note_is_read_once_after_it_is_embedded() {
        let mut core = test_core().await;
        let dec = reader("procedure", 0.8, 0.1);
        core.decider = dec.clone();
        let live = capture(&core, "To rotate the key, run vault operator rotate.").await;
        assert!(!live.is_empty());
        let asked = reads(&dec);
        for (_, qs) in &asked {
            let keys: Vec<&str> = qs.iter().map(|q| q.key.as_str()).collect();
            assert_eq!(keys, KEYS, "one request with the reading questions");
        }
        let mut read = 0;
        for c in &live {
            let rows = core.store.readings_of(&c.id).await.unwrap();
            assert_eq!(rows.len(), 4, "{}", c.id);
            assert!(rows.iter().all(|r| r.model == "rule-decider"));
            read += 1;
        }
        assert_eq!(asked.len(), read, "each note is read once");
        drain(&core).await;
        assert_eq!(
            reads(&dec).len(),
            read,
            "a drained queue reads nothing again"
        );
    }

    #[tokio::test]
    async fn a_passage_is_read_too_and_gets_a_category() {
        let mut core = test_core_without_synthesizer().await;
        core.decider = reader("reference", 0.7, 0.1);
        let live = capture(&core, "Opening hours: Mon to Fri, 9 to 17.").await;
        assert!(!live.is_empty());
        for c in &live {
            assert_eq!(c.provenance, crate::store::artifacts::Provenance::Passage);
            assert_eq!(c.category.as_deref(), Some("reference"), "{}", c.id);
            assert_eq!(category_by(&core, &c.id).await.as_deref(), Some("reading"));
        }
    }

    #[tokio::test]
    async fn the_top_category_is_taken_whatever_its_probability() {
        let mut core = test_core_without_synthesizer().await;
        core.decider = reader("procedure", 0.21, 0.1);
        let live = capture(&core, "Something that is hard to place.").await;
        assert!(!live.is_empty());
        let ids: Vec<String> = live.iter().map(|c| c.id.clone()).collect();
        let payloads = core.vectors.payloads_of(&ids).await.unwrap();
        for c in &live {
            assert_eq!(c.category.as_deref(), Some("procedure"));
            assert_eq!(category_by(&core, &c.id).await.as_deref(), Some("reading"));
            assert_eq!(
                payloads[&c.id].category.as_deref(),
                Some("procedure"),
                "the vector payload follows"
            );
        }
    }

    #[tokio::test]
    async fn a_reading_never_overwrites_a_category_a_person_set() {
        let mut core = test_core().await;
        let id = seed_artifact(&core, "Keys").await;
        core.store
            .update_artifact_category_by(&id, Some("procedure"), "person")
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &id).await.unwrap();
        let c = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(c.category.as_deref(), Some("procedure"));
        assert_eq!(category_by(&core, &id).await.as_deref(), Some("person"));
        assert_eq!(core.store.readings_of(&id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_category_a_person_cleared_stays_cleared() {
        let mut core = test_core().await;
        let id = seed_artifact(&core, "Keys").await;
        core.store
            .update_artifact_category_by(&id, None, "person")
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &id).await.unwrap();
        assert_eq!(core.store.get_artifact(&id).await.unwrap().category, None);
        assert_eq!(category_by(&core, &id).await.as_deref(), Some("person"));
    }

    #[tokio::test]
    async fn a_category_a_writer_gave_is_left() {
        let mut core = test_core().await;
        let roots = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("first wording", [1.0, 0.0]),
                ("second wording", [0.99, 0.05]),
            ],
        )
        .await;
        let m = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    title: Some("merged".into()),
                    text: "both wordings".into(),
                    category: Some("reference".into()),
                    ..Default::default()
                },
                &roots,
            )
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &m.id).await.unwrap();
        let c = core.store.get_artifact(&m.id).await.unwrap();
        assert_eq!(c.category.as_deref(), Some("reference"));
        assert_eq!(category_by(&core, &m.id).await, None);
        assert_eq!(core.store.readings_of(&m.id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn pinned_is_stored_and_changes_no_tag_and_no_ranking() {
        let mut core = test_core().await;
        core.decider = reader("reference", 0.8, 0.01);
        capture(&core, "The proxy listens on port 8080.").await;
        capture(&core, "The proxy restarts every night at two.").await;
        let q = crate::core::search::SearchQuery {
            q: "proxy".into(),
            limit: 10,
            tags: vec![],
            category: None,
            mark: false,
            rerank: false,
            explain: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let seen = |hits: Vec<crate::core::search::SearchResult>| {
            hits.into_iter()
                .map(|h| (h.artifact_id, h.tags, h.category))
                .collect::<Vec<_>>()
        };
        let door = crate::store::feedback::Door::Cli;
        let before = seen(core.search(&q, door).await.unwrap());
        assert!(!before.is_empty());

        core.decider = reader("reference", 0.8, 0.99);
        for (id, _, _) in &before {
            run(&core, id).await.unwrap();
            assert_eq!(
                core.store.reading_yes(id, &pinned()).await.unwrap(),
                Some(0.99)
            );
        }
        let after = seen(core.search(&q, door).await.unwrap());
        assert_eq!(after, before, "a pinned reading moved a result or a tag");
    }

    #[tokio::test]
    async fn a_note_too_long_for_one_read_is_read_from_its_opening() {
        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        dec.set_context_tokens(1024);
        core.decider = dec.clone();
        let text: String = (0..2000).map(|i| format!("word{i} ")).collect();
        let id = note(&core, text.trim()).await;
        run(&core, &id).await.unwrap();
        let asked = reads(&dec);
        assert_eq!(asked.len(), 1);
        let state = &asked[0].0;
        assert!(state.starts_with("untitled\nword0 word1"), "{state:.40}");
        assert!(state.len() < text.len());
        assert!(text.starts_with(state.trim_start_matches("untitled\n")));
        assert_eq!(core.store.readings_of(&id).await.unwrap().len(), 4);
        assert_eq!(
            core.store
                .get_artifact(&id)
                .await
                .unwrap()
                .category
                .as_deref(),
            Some("reference")
        );
    }

    #[tokio::test]
    async fn a_retired_note_is_not_read() {
        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        core.decider = dec.clone();
        let id = seed_artifact(&core, "Old").await;
        core.deprecate(&id).await.unwrap();
        run(&core, &id).await.unwrap();
        assert_eq!(dec.calls(), 0);
        assert!(core.store.readings_of(&id).await.unwrap().is_empty());
        assert_eq!(core.store.get_artifact(&id).await.unwrap().category, None);
    }

    #[tokio::test]
    async fn a_two_word_capture_is_read_and_nothing_is_flagged() {
        let mut core = test_core().await;
        let dec = reader("other", 0.4, 0.1);
        core.decider = dec.clone();
        let live = capture(&core, "Buy milk").await;
        assert!(!live.is_empty());
        for c in &live {
            assert!(c.category.is_some(), "{}", c.id);
            assert!(c.flags.is_empty(), "{:?}", c.flags);
            assert_eq!(core.store.readings_of(&c.id).await.unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn the_read_unit_takes_the_light_turn_and_is_not_held_with_the_generators() {
        use std::time::Duration;
        assert!(!Stage::GENERATORS.contains(&Stage::Read));
        assert!(!Stage::Read.calls_a_generator());
        assert_eq!(Stage::Read.class(), 1);
        assert!(!Stage::Read.reads_a_capture());
        assert_eq!(Stage::ALL[24], Stage::Read);
        assert_eq!(Stage::parse("read"), Some(Stage::Read));

        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        core.decider = dec.clone();
        core.gate = Arc::new(crate::infer::gate::InferenceGate::new(Duration::from_secs(
            600,
        )));
        core.gate.call_finished();
        let (a, b) = (
            seed_artifact(&core, "A").await,
            seed_artifact(&core, "B").await,
        );
        tokio::time::timeout(Duration::from_secs(10), run(&core, &a))
            .await
            .expect("a reading waited out the cooldown")
            .unwrap();
        assert_eq!(dec.calls(), 1);

        let lease = core.gate.interactive();
        let c = core.clone();
        let waiting = tokio::spawn(async move { run(&c, &b).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "a reading ran past the lease");
        drop(lease);
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the lease's end did not let it through")
            .unwrap()
            .unwrap();
        assert_eq!(dec.calls(), 2);

        core.store
            .enqueue(Stage::Read, "artifact", &a)
            .await
            .unwrap();
        let (_, job) = core
            .store
            .control
            .claim_job_holding(&Stage::GENERATORS)
            .await
            .unwrap()
            .expect("a read unit is held with the generators");
        assert_eq!(job.stage, Stage::Read);
    }

    #[test]
    fn the_reading_questions_are_the_four() {
        let ids: Vec<&str> = reading_questions().iter().map(|a| a.id).collect();
        assert_eq!(ids, KEYS);
    }
}
