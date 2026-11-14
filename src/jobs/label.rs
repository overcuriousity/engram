//! `Stage::Label`: each candidate of a recorded search is asked alone, "would this note help someone
//! who searched for …?". Labels are stored in their own table; they are not decisions (no band, no
//! log, no Unsure row) and never the person's verdict.

use crate::core::Core;
use crate::core::judging::catalogue::answers_query;
use crate::core::judging::state;
use crate::error::{Error, Result};
use crate::infer::decide::Answer;
use crate::store::artifacts::Chunk;
use crate::store::jobs::Stage;

/// Idle-only, as `read::arm`: a queued label keeps its attempts.
pub async fn arm(core: &Core, event_id: &str) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Label, "search", event_id, 0)
        .await
}

/// `answers_query`'s line: from it a note is relevant to a search.
pub async fn relevant_at(core: &Core) -> Result<f64> {
    Ok(core.thresholds(&answers_query("")).await?.act_at)
}

/// The yes of "would this note help someone who searched for `query`?", the note alone in the
/// state; `None` when the whole note does not fit one request, since a cut text is not judged.
pub async fn answers(core: &Core, query: &str, note: &Chunk) -> Result<Option<f64>> {
    let asked = answers_query(query);
    let room = state::room(core, &[&asked]);
    let shown = state::one(
        &state::Note {
            title: note.title.as_deref(),
            day: None,
            text: &note.text,
        },
        &core.counter,
        room,
    );
    let Some(shown) = shown else {
        return Ok(None);
    };
    let permit = core.gate.background_light().await;
    let got = core
        .decider
        .decide(&shown, std::slice::from_ref(&asked.question))
        .await;
    permit.finished();
    match got?.answers.get(&asked.question.key) {
        Some(Answer::Noul { yes }) => Ok(Some(*yes)),
        _ => Err(Error::InferenceRejected {
            role: "decide",
            detail: "the answer has none for \"answers_query\"".into(),
        }),
    }
}

/// Each candidate still in results is asked alone; one that is gone or does not fit is skipped. A
/// call that fails stores nothing, and the unit is retried.
pub async fn run(core: &Core, event_id: &str) -> Result<()> {
    let Some((query, candidates)) = core.store.search_to_label(event_id).await? else {
        return Ok(());
    };
    let notes = core.store.artifacts_by_ids(&candidates).await?;
    let mut rows = Vec::new();
    for id in &candidates {
        let Some(note) = notes.iter().find(|c| &c.id == id && c.in_results()) else {
            continue;
        };
        match answers(core, &query, note).await? {
            Some(yes) => rows.push((id.clone(), yes)),
            None => tracing::info!(event_id, artifact_id = %id, "no room to label this candidate"),
        }
    }
    core.store
        .put_search_labels(
            event_id,
            &rows,
            answers_query("").version,
            core.decider.model(),
        )
        .await
}

/// Retrieval by the model's labels, how far they agree with the person, and decide's bearing record.
#[derive(Debug, Clone, Default)]
pub struct Figures {
    pub retrieval: crate::store::search_labels::ModelRetrieval,
    pub agreement: crate::store::search_labels::Agreement,
    pub bears: crate::store::search_labels::BearsRecord,
    pub counts: bool,
}

impl Core {
    pub async fn label_figures(&self) -> Result<Figures> {
        let (at, version, model) = (
            relevant_at(self).await?,
            answers_query("").version,
            self.decider.model(),
        );
        Ok(Figures {
            retrieval: self.store.model_retrieval(at, version, model).await?,
            agreement: self.store.label_agreement(at, version, model).await?,
            bears: self.store.bears_record(model).await?,
            counts: self.labels_count().await?,
        })
    }

    /// Whether the model's labels may steer a sweep: over enough searches the person judged, it
    /// finds what they found and does not call everything relevant.
    pub async fn labels_count(&self) -> Result<bool> {
        let g = self
            .store
            .label_agreement(
                relevant_at(self).await?,
                answers_query("").version,
                self.decider.model(),
            )
            .await?;
        let c = &self.decide_cfg;
        Ok(g.judged >= c.label_overlap_min
            && g.hit_rate >= c.label_agree_min
            && g.selectivity <= c.label_select_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::infer::decide::{Answer, Answers, Question};
    use crate::infer::fake::RuleDecider;
    use crate::store::feedback::{Door, Labeller, NewCandidate, NewEvent};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Says `yes` to every `answers_query`.
    fn labeller(yes: f64) -> Arc<RuleDecider> {
        Arc::new(RuleDecider::new(move |_, qs: &[Question]| {
            let answers: HashMap<String, Answer> = qs
                .iter()
                .map(|q| (q.key.clone(), Answer::Noul { yes }))
                .collect();
            Ok(Answers {
                answers,
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }))
    }

    async fn note(core: &Core, title: &str) -> String {
        crate::core::test_support::seed_artifact(core, title).await
    }

    async fn search(core: &Core, query: &str, candidates: &[String]) -> String {
        core.store
            .record_search(
                NewEvent {
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: candidates
                        .iter()
                        .map(|a| NewCandidate {
                            artifact_id: a.clone(),
                            score: 0.5,
                            similarity: Some(0.5),
                            shown: true,
                            ..Default::default()
                        })
                        .collect(),
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    async fn labelled(core: &Core, event_id: &str) -> Vec<(String, f64)> {
        sqlx::query!(
            r#"SELECT artifact_id, yes AS "yes!: f64" FROM search_labels WHERE event_id = ? ORDER BY artifact_id"#,
            event_id
        )
        .fetch_all(&core.store.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.artifact_id, r.yes))
        .collect()
    }

    fn version() -> u32 {
        crate::core::judging::catalogue::answers_query("").version
    }

    #[tokio::test]
    async fn each_candidate_is_asked_alone_about_the_search() {
        let mut core = test_core().await;
        let dec = labeller(0.95);
        core.decider = dec.clone();
        let (a, b) = (
            note(&core, "Loop mounts").await,
            note(&core, "Journals").await,
        );
        let id = search(&core, "mount loop", &[a.clone(), b.clone()]).await;

        run(&core, &id).await.unwrap();

        let asked = dec.requests();
        assert_eq!(asked.len(), 2, "one request per candidate");
        for ((state, qs), title) in asked.iter().zip(["Loop mounts", "Journals"]) {
            assert_eq!(qs.len(), 1);
            assert_eq!(qs[0].key, "answers_query");
            assert!(
                qs[0].instructions.contains("«mount loop»"),
                "{}",
                qs[0].instructions
            );
            assert!(state.starts_with(title), "{state}");
            assert!(!state.contains(if title == "Journals" {
                "Loop"
            } else {
                "Journals"
            }));
        }
        let mut want = vec![(a, 0.95), (b, 0.95)];
        want.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(labelled(&core, &id).await, want);
        assert!(
            core.store
                .unlabelled_searches(version(), "rule-decider", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            core.judging_budget().await.unwrap(),
            core.decide_cfg.max_per_tick,
            "labels are not decisions"
        );
    }

    #[tokio::test]
    async fn a_candidate_since_retired_is_skipped_and_the_search_still_counts() {
        let mut core = test_core().await;
        let dec = labeller(0.95);
        core.decider = dec.clone();
        let (live, gone) = (note(&core, "Live").await, note(&core, "Gone").await);
        let id = search(
            &core,
            "mount loop",
            &[live.clone(), gone.clone(), "deleted".into()],
        )
        .await;
        core.deprecate(&gone).await.unwrap();

        run(&core, &id).await.unwrap();

        assert_eq!(dec.calls(), 1);
        assert_eq!(labelled(&core, &id).await, [(live, 0.95)]);
        assert!(
            core.store
                .unlabelled_searches(version(), "rule-decider", 10)
                .await
                .unwrap()
                .is_empty(),
            "the search counts as labelled"
        );
    }

    #[tokio::test]
    async fn the_persons_verdict_is_never_touched() {
        let mut core = test_core().await;
        core.decider = labeller(0.99);
        let (a, b) = (note(&core, "A").await, note(&core, "B").await);
        let judged = search(&core, "mount loop", &[a.clone(), b.clone()]).await;
        core.store
            .judge_hit(&judged, &b, Labeller::Confirm)
            .await
            .unwrap();
        let open = search(&core, "read journal", &[a.clone(), b.clone()]).await;
        let row = |id: String| {
            let pool = core.store.pool.clone();
            async move {
                sqlx::query!(
                    "SELECT verdict, expect_id, judged_at, judged_by FROM search_events WHERE id = ?",
                    id
                )
                .fetch_one(&pool)
                .await
                .map(|r| (r.verdict, r.expect_id, r.judged_at, r.judged_by))
                .unwrap()
            }
        };
        let (before_j, before_o) = (row(judged.clone()).await, row(open.clone()).await);

        run(&core, &judged).await.unwrap();
        run(&core, &open).await.unwrap();

        assert_eq!(labelled(&core, &open).await.len(), 2);
        assert_eq!(row(judged).await, before_j);
        assert_eq!(row(open.clone()).await, before_o);
        assert_eq!(before_o, (None, None, None, None));
        // Still the person's to answer.
        core.store
            .judge_hit(&open, &a, Labeller::Confirm)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn labelling_takes_at_most_a_quarter_of_the_tick() {
        let mut core = test_core().await;
        core.decide_cfg.max_per_tick = 40;
        let mut searches = Vec::new();
        for s in 0..5 {
            let mut c = Vec::new();
            for i in 0..4 {
                c.push(note(&core, &format!("s{s} n{i}")).await);
            }
            let id = search(&core, &format!("search {s}"), &c).await;
            // Old, so the base is quiet and the scan runs beside the labels.
            sqlx::query!(
                "UPDATE search_events SET created_at = ? WHERE id = ?",
                1_000 + s,
                &id
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
            searches.push(id);
        }
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        // A quarter of 40 is 10: two searches of four candidates; a third would make 12.
        assert_eq!(r.labelled, 2);
        let mut armed = Vec::new();
        for id in &searches {
            armed.push(core.store.live_job(Stage::Label, id).await.unwrap());
        }
        assert_eq!(armed, [true, true, false, false, false], "oldest first");
        assert_eq!(r.scanned, 20, "the scan keeps its half");
        assert_eq!(core.judging_budget().await.unwrap(), 40);

        // The two still queued hold their share: nothing more is armed.
        let again = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(again.labelled, 0);
    }

    #[test]
    fn the_label_unit_is_a_light_background_unit() {
        assert!(!Stage::GENERATORS.contains(&Stage::Label));
        assert!(!Stage::Label.calls_a_generator());
        assert_eq!(Stage::Label.class(), 1);
        assert!(!Stage::Label.reads_a_capture());
        assert_eq!(Stage::ALL[25], Stage::Label);
        assert_eq!(Stage::parse("label"), Some(Stage::Label));
    }
}
