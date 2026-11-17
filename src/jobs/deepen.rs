//! `Stage::Deepen`: for a search or question that found nothing, the decision model is asked the
//! labeller's question about notes further down than search showed. A find leaves a trail and,
//! for a gap, covers it.

use crate::core::Core;
use crate::core::judging::band::era;
use crate::core::judging::catalogue::answers_query;
use crate::error::Result;
use crate::jobs::label;
use crate::store::deep_looks::{DeepKind, DeepSource};
use crate::store::feedback::blob_to_vec;
use crate::store::gaps::{CoveredBy, Gap, GapKind};
use crate::store::jobs::{MAX_ATTEMPTS, Stage, backoff_secs};
use crate::store::search_labels::LABEL_GIVE_UP;
use crate::store::trails::TrailSource;

/// How far down the replay reads.
pub const REPLAY_LIMIT: usize = 50;
/// The replay's first this many bring their linked notes into the pool.
pub const LINKED_OF: usize = 5;

/// Idle-only, as `label::arm`: a queued look keeps its attempts.
pub async fn arm(core: &Core, s: &DeepSource) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Deepen, "deep", &s.target(), 0)
        .await
}

/// What a source is asked with: its text, its stored vector, and the notes to read first.
pub(crate) struct Brief {
    pub text: String,
    pub vec: Vec<f32>,
    pub first: Vec<String>,
    pub pool: bool,
}

/// `None`: the source is gone, or its vector is empty or under another embed model.
pub(crate) async fn brief(core: &Core, s: &DeepSource) -> Result<Option<Brief>> {
    Ok(match s.kind {
        DeepKind::Gap(k) => {
            // No store call reads one open gap by id with its vector; the open ones are few
            // enough to read whole, and that also says it is still open.
            let open = core
                .store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await?;
            let Some(g) = open
                .gaps
                .into_iter()
                .find(|g| g.gap.kind == k && g.gap.id == s.id && !g.vec.is_empty())
            else {
                return Ok(None);
            };
            let first = if k == GapKind::Decide {
                joint_evidence(core, &s.id).await?.unwrap_or_default()
            } else {
                Vec::new()
            };
            Some(Brief {
                text: g.gap.text,
                vec: g.vec,
                first,
                pool: true,
            })
        }
        DeepKind::WeakRight | DeepKind::SecondReading => {
            let (Some((text, vec)), Some(first)) = (
                pairing(core, TrailSource::Decide, &s.id).await?,
                joint_evidence(core, &s.id).await?,
            ) else {
                return Ok(None);
            };
            Some(Brief {
                text,
                vec,
                first,
                pool: s.kind == DeepKind::WeakRight,
            })
        }
        DeepKind::Labelled => pairing(core, TrailSource::Label, &s.id)
            .await?
            .map(|(text, vec)| Brief {
                text,
                vec,
                first: Vec::new(),
                pool: true,
            }),
    })
}

/// The event's words and vector, when the vector is usable under the live embed model.
async fn pairing(core: &Core, source: TrailSource, id: &str) -> Result<Option<(String, Vec<f32>)>> {
    let Some(p) = core.store.trail_pairing(source, id).await? else {
        return Ok(None);
    };
    let vec = blob_to_vec(&p.vec);
    Ok((p.embed_model == core.embedder.model() && !vec.is_empty()).then_some((p.query, vec)))
}

/// The notes a decision weighed together, in retrieval order; `None` when the decision is gone.
async fn joint_evidence(core: &Core, id: &str) -> Result<Option<Vec<String>>> {
    Ok(core.store.decision(id).await?.map(|d| {
        d.evidence
            .into_iter()
            .filter(|e| e.joint)
            .map(|e| e.artifact_id)
            .collect()
    }))
}

/// `first`, then the replay and the notes linked to its head, each once; what was already asked
/// of this source, what is out of results and what does not fit one request are dropped.
pub(crate) async fn pool(core: &Core, s: &DeepSource, b: &Brief) -> Result<Vec<String>> {
    let mut ids = b.first.clone();
    if b.pool {
        let replay = replay(core, b).await?;
        let head: Vec<String> = replay.iter().take(LINKED_OF).cloned().collect();
        ids.extend(replay);
        let limit = i64::try_from(core.decide_cfg.deep_max).unwrap_or(i64::MAX);
        let links = core
            .store
            .links_from(
                &head,
                &[
                    crate::store::links::LinkState::Learning,
                    crate::store::links::LinkState::Related,
                ],
                core.associate.half_life_days,
                crate::store::now(),
                core.associate.show_min,
                limit,
            )
            .await?;
        ids.extend(links.into_iter().map(|l| l.other));
    }
    let (version, model) = (answers_query("").version, core.decider.model());
    let mut asked = core.store.deep_asked(s, version, model).await?;
    // Only a search event has labels; another source's id matches none.
    asked.extend(core.store.labelled_notes(&s.id, version, model).await?);
    // The person saw these and said none of them answers.
    if s.kind == DeepKind::Gap(GapKind::Search) {
        asked.extend(core.store.shown_notes(&s.id).await?);
    }
    let mut seen: std::collections::HashSet<String> = asked.into_iter().collect();
    ids.retain(|id| seen.insert(id.clone()));
    let notes = core.store.artifacts_by_ids(&ids).await?;
    let gap = gap_of(s, b);
    let (era, relevant_at) = (era(model, version), label::relevant_at(core).await?);
    let mut out = Vec::new();
    for id in ids {
        if out.len() == core.decide_cfg.deep_max {
            break;
        }
        let Some(note) = notes.iter().find(|n| n.id == id && n.in_results()) else {
            continue;
        };
        if !label::fits(core, &b.text, note) {
            continue;
        }
        if let Some(g) = &gap
            && core
                .store
                .gap_note_rejected(g, &id, &era, relevant_at)
                .await?
        {
            continue;
        }
        out.push(id);
    }
    Ok(out)
}

/// The source's stored vector, replayed through search as the sweep replays a pair.
async fn replay(core: &Core, b: &Brief) -> Result<Vec<String>> {
    core.remember_query_vector(&b.text, b.vec.clone());
    let params = *core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let q = crate::core::search::SearchQuery {
        q: b.text.clone(),
        limit: REPLAY_LIMIT,
        tags: vec![],
        category: None,
        // A look is not someone reading their notes.
        mark: false,
        rerank: false,
        explain: false,
        include_deprecated: false,
        include_superseded: false,
    };
    let origin = crate::store::feedback::Origin::from(crate::store::feedback::Door::Judge);
    let (results, _) = core.search_with_ranking(&q, params, origin).await?;
    Ok(results.into_iter().map(|r| r.artifact_id).collect())
}

fn gap_of(s: &DeepSource, b: &Brief) -> Option<Gap> {
    match s.kind {
        DeepKind::Gap(kind) => Some(Gap {
            kind,
            id: s.id.clone(),
            text: b.text.clone(),
        }),
        _ => None,
    }
}

/// Looks once per era. A failure stores nothing and is handled as `label::run` handles one: the
/// queue retries, and from `LABEL_GIVE_UP` refusals the source is given up for the era.
pub async fn run(core: &Core, target: &str) -> Result<()> {
    let Some(s) = DeepSource::parse(target) else {
        tracing::warn!(target, "not a deep look's target");
        return Ok(());
    };
    let Err(e) = look(core, &s).await else {
        return Ok(());
    };
    let refused = !e.retryable();
    // As long as the queue waits, as `label::run` reckons it.
    let attempts = core
        .store
        .live_job_attempts(Stage::Deepen, target)
        .await?
        .unwrap_or(1);
    let floor = if refused {
        attempts.max(MAX_ATTEMPTS)
    } else {
        attempts
    };
    let retry_at = crate::store::now() + backoff_secs(floor) * 3 / 2;
    let refusals = match core
        .store
        .deep_look_failed(
            &s,
            answers_query("").version,
            core.decider.model(),
            refused,
            retry_at,
        )
        .await
    {
        Ok(n) => n,
        Err(unrecorded) => {
            tracing::warn!(target, error = %unrecorded, "could not record a failed deep look");
            return Err(e);
        }
    };
    if refusals >= LABEL_GIVE_UP {
        tracing::warn!(target, refusals, error = %e, "the decision model keeps refusing this deep look; it is not looked at until the question or the model changes");
        return Ok(());
    }
    Err(e)
}

/// Asks the pool in order, to the first find; a second reading asks every note and finds nothing.
async fn look(core: &Core, s: &DeepSource) -> Result<()> {
    let (version, model) = (answers_query("").version, core.decider.model());
    if core.store.deep_looked(s, version, model).await? {
        return Ok(());
    }
    let Some(b) = brief(core, s).await? else {
        return Ok(());
    };
    let ids = pool(core, s, &b).await?;
    let relevant_at = label::relevant_at(core).await?;
    let notes = core.store.artifacts_by_ids(&ids).await?;
    let mut answers = Vec::new();
    let mut found = None;
    for id in &ids {
        let Some(note) = notes.iter().find(|n| &n.id == id) else {
            continue;
        };
        let Some(yes) = label::answers(core, &b.text, note).await? else {
            continue;
        };
        answers.push((id.clone(), yes));
        if yes >= relevant_at && s.kind != DeepKind::SecondReading {
            found = Some((note, yes));
            break;
        }
    }
    match found {
        Some((note, yes)) => on_find(core, s, &b, note, yes).await,
        None => {
            if let Some(g) = gap_of(s, &b) {
                let era = era(model, version);
                for (id, yes) in &answers {
                    let Some(note) = notes.iter().find(|n| &n.id == id) else {
                        continue;
                    };
                    if let Err(e) = core
                        .store
                        .reject_gap_note(&g, id, note.embed_rev, &era, *yes)
                        .await
                    {
                        tracing::warn!(target = %s.target(), error = %e, "could not remember that a note does not answer a gap");
                    }
                }
            }
        }
    }
    core.store.put_deep_look(s, &answers, version, model).await
}

/// A trail on the found note, and for a gap its coverage. Never fails the look.
async fn on_find(
    core: &Core,
    s: &DeepSource,
    b: &Brief,
    note: &crate::store::artifacts::Chunk,
    yes: f64,
) {
    let target = s.target();
    if let Err(e) = core
        .mint_trail(
            &note.id,
            &b.text,
            &b.vec,
            core.embedder.model(),
            TrailSource::Deep,
            &target,
        )
        .await
    {
        tracing::warn!(target, error = %e, "could not leave a trail for a deep look's find");
    }
    let DeepKind::Gap(k) = s.kind else {
        return;
    };
    // `gap_coverage` names the capture; a note that belongs to none cannot cover.
    let Some(corpus) = note.corpus_id.as_deref() else {
        tracing::info!(target, artifact_id = %note.id, "the found note belongs to no capture; the gap stays open");
        return;
    };
    if let Err(e) = core
        .store
        .cover_gap(k, &s.id, corpus, &note.id, yes as f32, CoveredBy::Model)
        .await
    {
        tracing::warn!(target, error = %e, "could not record that a deep look answered a gap");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::{TEST_DIM, test_core};
    use crate::error::Error;
    use crate::infer::decide::{Answer, Answers, Question};
    use crate::infer::fake::RuleDecider;
    use crate::store::trails::Tier;
    use std::sync::Arc;

    const M: &str = "fake-embed";
    const MARK: &str = "MARKER";

    /// Yes 0.95 of a note whose text holds `mark`, 0.05 of any other.
    fn decider(mark: &'static str) -> Arc<RuleDecider> {
        Arc::new(RuleDecider::new(move |state: &str, qs: &[Question]| {
            let yes = if state.contains(mark) { 0.95 } else { 0.05 };
            Ok(Answers {
                answers: qs
                    .iter()
                    .map(|q| (q.key.clone(), Answer::Noul { yes }))
                    .collect(),
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }))
    }

    fn query_vec() -> Vec<f32> {
        let mut v = vec![0.0; TEST_DIM];
        v[0] = 1.0;
        v
    }

    /// A note whose vector leans `off` away from the query's.
    async fn note(core: &Core, text: &str, off: f32) -> String {
        let src = core.store.insert_corpus(text, "web", None).await.unwrap();
        let a = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0);
        let mut vector = query_vec();
        vector[1] = off;
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector,
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: a.id.clone(),
                    corpus_id: src.id.clone(),
                    text: a.text.clone(),
                    created_at: crate::store::now(),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();
        a.id
    }

    /// Twelve notes nearer the query than the marked one, which comes thirteenth, and two after
    /// it: (core, fillers, marked).
    async fn base(mark: &'static str) -> (Core, Arc<RuleDecider>, Vec<String>, String) {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.decide_cfg.bands.per.insert(
            "answers_query".into(),
            crate::config::BandOverride {
                act_at: Some(0.9),
                ask_at: None,
            },
        );
        let dec = decider(mark);
        core.decider = dec.clone();
        let mut fillers = Vec::new();
        for i in 0..12 {
            fillers.push(note(&core, &format!("filler {i}"), 0.05 * (i + 1) as f32).await);
        }
        let marked = note(&core, &format!("the {MARK} note"), 1.0).await;
        for i in 0..2 {
            note(&core, &format!("tail {i}"), 2.0 + i as f32).await;
        }
        (core, dec, fillers, marked)
    }

    async fn gap(core: &Core) -> DeepSource {
        gap_showing(core, &[]).await
    }

    /// A search that showed `shown`, which a person then called a gap.
    async fn gap_showing(core: &Core, shown: &[String]) -> DeepSource {
        let candidates = shown
            .iter()
            .map(|a| crate::store::feedback::NewCandidate {
                artifact_id: a.clone(),
                shown: true,
                ..Default::default()
            })
            .collect();
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "where did it go".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: query_vec(),
                    embed_model: M.into(),
                    candidates,
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(
                &id,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        DeepSource {
            kind: DeepKind::Gap(GapKind::Search),
            id,
        }
    }

    /// A weak decision whose joint evidence is `evidence`.
    async fn decision(core: &Core, evidence: &[String]) -> String {
        core.store
            .record_decision(crate::store::decisions::NewDecision {
                question: "where did it go".into(),
                questions: "[]".into(),
                situation: None,
                scope: None,
                filters: "{}".into(),
                answers: Some("{}".into()),
                prior: "{}".into(),
                standing: "{}".into(),
                not_held: false,
                weak: true,
                model: "rule-decider".into(),
                decider: "rule-decider".into(),
                read: evidence.len(),
                dropped: 0,
                query_vec: query_vec(),
                embed_model: M.into(),
                evidence: evidence
                    .iter()
                    .enumerate()
                    .map(|(i, a)| crate::store::decisions::NewEvidence {
                        rank: i,
                        artifact_id: a.clone(),
                        bears: 0.1,
                        bearing: false,
                        joint: true,
                        n: Some(i + 1),
                        read: "{}".into(),
                    })
                    .collect(),
            })
            .await
            .unwrap()
    }

    async fn open_gap_ids(core: &Core) -> Vec<String> {
        core.store
            .open_gaps(M, core.weak_below())
            .await
            .unwrap()
            .gaps
            .into_iter()
            .map(|g| g.gap.id)
            .collect()
    }

    async fn rejections(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT count(*) FROM gap_rejections")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    async fn coverage(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT count(*) FROM gap_coverage")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_gap_whose_answer_search_did_not_show_is_covered_and_leaves_a_trail() {
        let (core, dec, _, marked) = base(MARK).await;
        let s = gap(&core).await;

        run(&core, &s.target()).await.unwrap();

        assert!(open_gap_ids(&core).await.is_empty());
        let t = core.store.live_trails(M).await.unwrap();
        assert_eq!(
            (t.len(), t[0].tier, t[0].artifact_id.as_str()),
            (1, Tier::Model, marked.as_str())
        );
        assert_eq!(
            dec.calls(),
            13,
            "the twelve above it, then the find; none after"
        );
    }

    #[tokio::test]
    async fn a_note_the_person_saw_and_rejected_is_not_asked_about() {
        let (core, _, _, marked) = base(MARK).await;
        let s = gap_showing(&core, std::slice::from_ref(&marked)).await;

        run(&core, &s.target()).await.unwrap();

        assert_eq!(open_gap_ids(&core).await, std::slice::from_ref(&s.id));
        assert!(core.store.live_trails(M).await.unwrap().is_empty());
        let (v, m) = (answers_query("").version, core.decider.model());
        let asked = core.store.deep_asked(&s, v, m).await.unwrap();
        assert!(!asked.is_empty() && !asked.contains(&marked));
    }

    #[tokio::test]
    async fn a_second_run_asks_nothing() {
        let (core, dec, _, _) = base("ABSENT").await;
        let s = gap(&core).await;
        run(&core, &s.target()).await.unwrap();
        let calls = dec.calls();
        assert!(calls > 0);

        run(&core, &s.target()).await.unwrap();

        assert_eq!(dec.calls(), calls);
    }

    #[tokio::test]
    async fn nothing_found_leaves_the_gap_and_rejections_so_coverage_does_not_ask_again() {
        let (core, dec, fillers, _) = base("ABSENT").await;
        let s = gap(&core).await;

        run(&core, &s.target()).await.unwrap();

        assert_eq!(open_gap_ids(&core).await, std::slice::from_ref(&s.id));
        assert_eq!(rejections(&core).await, dec.calls() as i64);
        let g = Gap {
            kind: GapKind::Search,
            id: s.id.clone(),
            text: "where did it go".into(),
        };
        let era = era("rule-decider", answers_query("").version);
        assert!(
            core.store
                .gap_note_rejected(&g, &fillers[0], &era, 0.9)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_look_stops_at_its_first_find_and_asks_at_most_deep_max() {
        let (mut core, dec, _, _) = base(MARK).await;
        core.decide_cfg.deep_max = 3;
        let s = gap(&core).await;

        run(&core, &s.target()).await.unwrap();

        assert_eq!(dec.calls(), 3);
        assert_eq!(open_gap_ids(&core).await, [s.id]);
    }

    #[tokio::test]
    async fn a_weak_decision_judged_right_gets_a_trail_and_no_coverage() {
        let (core, dec, fillers, marked) = base(MARK).await;
        let id = decision(&core, &fillers[..3]).await;
        core.store
            .judge_decision(&id, crate::store::asks::AskVerdict::Right, None)
            .await
            .unwrap();
        let s = DeepSource {
            kind: DeepKind::WeakRight,
            id,
        };

        run(&core, &s.target()).await.unwrap();

        let t = core.store.live_trails(M).await.unwrap();
        assert_eq!((t.len(), t[0].artifact_id.as_str()), (1, marked.as_str()));
        assert_eq!(coverage(&core).await, 0);
        assert_eq!(
            dec.calls(),
            13,
            "the weighed three were the replay's first three"
        );
    }

    #[tokio::test]
    async fn a_second_reading_reads_the_evidence_only_and_makes_nothing() {
        let (core, dec, fillers, marked) = base(MARK).await;
        let evidence = vec![marked, fillers[5].clone(), fillers[9].clone()];
        let s = DeepSource {
            kind: DeepKind::SecondReading,
            id: decision(&core, &evidence).await,
        };

        run(&core, &s.target()).await.unwrap();

        assert_eq!(dec.calls(), 3, "past a yes, and nothing further down");
        assert!(core.store.live_trails(M).await.unwrap().is_empty());
        assert_eq!(coverage(&core).await, 0);
        let mut asked = core
            .store
            .deep_asked(&s, answers_query("").version, "rule-decider")
            .await
            .unwrap();
        asked.sort();
        let mut want = evidence;
        want.sort();
        assert_eq!(asked, want);
    }

    #[tokio::test]
    async fn a_linked_note_outside_the_replay_is_in_the_pool() {
        let (core, dec, fillers, _) = base("LINKED").await;
        // No vector: the replay cannot return it; only the link from its head brings it in.
        let src = core
            .store
            .insert_corpus("linked", "web", None)
            .await
            .unwrap();
        let linked = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "the LINKED note".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        let (half_life, now) = (core.associate.half_life_days, crate::store::now());
        core.store
            .bump_link(&fillers[0], &linked, 5.0, None, half_life, now)
            .await
            .unwrap();
        core.store
            .set_link_state(
                &fillers[0],
                &linked,
                crate::store::links::LinkState::Related,
                None,
                None,
            )
            .await
            .unwrap();
        let s = gap(&core).await;

        run(&core, &s.target()).await.unwrap();

        let t = core.store.live_trails(M).await.unwrap();
        assert_eq!((t.len(), t[0].artifact_id.as_str()), (1, linked.as_str()));
        assert_eq!(
            dec.calls(),
            16,
            "the fifteen of the replay, then the linked note"
        );
        assert!(open_gap_ids(&core).await.is_empty());
    }

    #[tokio::test]
    async fn a_source_deleted_before_the_look_ends_quietly() {
        let (core, dec, _, _) = base(MARK).await;
        assert!(run(&core, "labelled:gone").await.is_ok());
        assert_eq!(dec.calls(), 0);
        let s = DeepSource::parse("labelled:gone").unwrap();
        assert!(
            !core
                .store
                .deep_looked(&s, answers_query("").version, "rule-decider")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_failed_call_stores_nothing_and_is_counted() {
        let (mut core, _, _, _) = base(MARK).await;
        core.decider = Arc::new(RuleDecider::new(|_, _| {
            Err(Error::InferenceRejected {
                role: "decide",
                detail: "HTTP 400".into(),
            })
        }));
        let s = gap(&core).await;

        assert!(run(&core, &s.target()).await.is_err());

        let (version, model) = (answers_query("").version, "rule-decider");
        assert!(!core.store.deep_looked(&s, version, model).await.unwrap());
        assert!(
            core.store
                .deep_asked(&s, version, model)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(rejections(&core).await, 0);
        assert!(!core.store.deep_look_due(&s, version, model).await.unwrap());
        let refused = core
            .store
            .deep_look_failed(&s, version, model, false, 0)
            .await
            .unwrap();
        assert_eq!(refused, 1, "the refusal was counted");
    }
}
