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
    let (mut rows, mut live) = (Vec::new(), false);
    for id in &candidates {
        let Some(note) = notes.iter().find(|c| &c.id == id && c.in_results()) else {
            continue;
        };
        live = true;
        match answers(core, &query, note).await? {
            Some(yes) => rows.push((id.clone(), yes)),
            None => tracing::info!(event_id, artifact_id = %id, "no room to label this candidate"),
        }
    }
    if live && rows.is_empty() {
        return step_aside(core, event_id).await;
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

/// Marks a search none of whose live candidates fits one request as labelled, so it leaves the
/// queue: one marker row, `artifact_id = ''`, that no figure reads. Never written on a failure.
async fn step_aside(core: &Core, event_id: &str) -> Result<()> {
    core.store
        .put_search_labels(
            event_id,
            &[(String::new(), 0.0)],
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

    /// `n` searches of `k` fresh notes each, recorded long ago so the base is quiet; oldest first.
    async fn old_searches(core: &Core, n: usize, k: usize) -> Vec<String> {
        let mut out = Vec::new();
        for s in 0..n {
            let mut c = Vec::new();
            for i in 0..k {
                c.push(note(core, &format!("s{s} n{i}")).await);
            }
            let id = search(core, &format!("search {s}"), &c).await;
            let at = 1_000 + s as i64;
            sqlx::query!(
                "UPDATE search_events SET created_at = ? WHERE id = ?",
                at,
                &id
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
            out.push(id);
        }
        out
    }

    #[tokio::test]
    async fn at_the_defaults_one_whole_search_is_labelled_per_tick() {
        let core = test_core().await;
        assert_eq!(core.decide_cfg.max_per_tick, 60);
        let ids = old_searches(&core, 2, 20).await;
        // A retired candidate is never asked, so it costs nothing.
        core.deprecate(&note(&core, "spare").await).await.unwrap();

        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.labelled, 1, "the first search may overrun the quarter");
        assert!(core.store.live_job(Stage::Label, &ids[0]).await.unwrap());
        assert!(!core.store.live_job(Stage::Label, &ids[1]).await.unwrap());

        let again = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(again.labelled, 0, "while it is pending, nothing more");
    }

    async fn long_note(core: &Core) -> String {
        let text: String = (0..2000).map(|i| format!("word{i} ")).collect();
        let src = core.store.insert_corpus(&text, "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.trim().into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn a_search_whose_candidates_do_not_fit_steps_aside_and_no_figure_reads_it() {
        let mut core = test_core().await;
        let dec = labeller(0.95);
        dec.set_context_tokens(1024);
        core.decider = dec.clone();
        let long = long_note(&core).await;
        let id = search(&core, "mount loop", std::slice::from_ref(&long)).await;
        core.store
            .judge_hit(&id, &long, Labeller::Confirm)
            .await
            .unwrap();

        run(&core, &id).await.unwrap();

        assert_eq!(dec.calls(), 0, "a cut text is not judged");
        assert!(
            core.store
                .unlabelled_searches(version(), "rule-decider", 10)
                .await
                .unwrap()
                .is_empty(),
            "it steps aside"
        );
        let s = &core.store;
        assert_eq!(
            s.model_retrieval(0.0, version(), "rule-decider")
                .await
                .unwrap(),
            Default::default()
        );
        assert_eq!(
            s.label_agreement(0.0, version(), "rule-decider")
                .await
                .unwrap(),
            Default::default()
        );
        core.store.unjudge(&id, Labeller::Confirm).await.unwrap();
        assert_eq!(s.model_expected(&id, 0.0).await.unwrap(), None);
    }

    /// Makes `event_id`'s label unit due and drains the queue once.
    async fn retry(core: &Core, event_id: &str) {
        sqlx::query!(
            "UPDATE jobs SET run_after = 0 WHERE stage = 'label' AND target_id = ?",
            event_id
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();
        while crate::jobs::run_one(core).await.unwrap() {}
    }

    #[tokio::test]
    async fn a_search_the_model_keeps_refusing_is_retried_and_does_not_hold_the_queue() {
        let mut core = test_core().await;
        core.decider = Arc::new(RuleDecider::new(|_, _| {
            Err(Error::InferenceRejected {
                role: "decide",
                detail: "HTTP 401".into(),
            })
        }));
        let ids = old_searches(&core, 2, 20).await;
        assert_eq!(
            crate::jobs::decisions::run(&core).await.unwrap().labelled,
            1
        );
        for _ in 0..crate::store::jobs::MAX_ATTEMPTS {
            retry(&core, &ids[0]).await;
        }

        // Not marked: no row, still unlabelled, still queued.
        assert!(labelled(&core, &ids[0]).await.is_empty());
        assert_eq!(
            core.store
                .unlabelled_searches(version(), "rule-decider", 10)
                .await
                .unwrap(),
            ids
        );
        assert!(core.store.live_job(Stage::Label, &ids[0]).await.unwrap());
        // At its ceiling it no longer holds the tick: the search behind it is armed.
        assert_eq!(
            crate::jobs::decisions::run(&core).await.unwrap().labelled,
            1
        );
        assert!(core.store.live_job(Stage::Label, &ids[1]).await.unwrap());

        // The endpoint recovers: the queue's retry labels it, and the figures read it.
        core.decider = labeller(0.95);
        retry(&core, &ids[0]).await;
        assert_eq!(labelled(&core, &ids[0]).await.len(), 20);
        let r = core
            .store
            .model_retrieval(0.9, version(), "rule-decider")
            .await
            .unwrap();
        assert!(r.searches >= 1, "{r:?}");
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
