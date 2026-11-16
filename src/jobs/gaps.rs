//! Group the open gaps and name the new groups: one efficient-tier call per new group of two or
//! more.
//! Membership is identity, so the same members are never named twice.

use crate::core::Core;
use crate::core::gaps::{MIN_CLUSTER, cluster, cluster_key, terms_label};
use crate::core::judging::band::era;
use crate::core::judging::catalogue::answers_query;
use crate::error::Result;
use crate::store::gaps::{CoveredBy, GapCluster, GapKind};

/// Small enough that captures never load the vector store, large enough that the wait is the
/// slowest query.
const COVER_IN_FLIGHT: usize = 16;

/// Process-wide, not per pass: `settle_corpus` starts a check per document, so a fifty-document
/// ingest would put eight hundred queries in the air. Permits are held for one search only.
static COVER_SLOTS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(COVER_IN_FLIGHT));

/// `open_gaps` caps five kinds at `MAX_OPEN_GAPS` each, so one capture could ask thousands of
/// queries of the store the search box uses. Taken newest-first, so older gaps go unchecked by this
/// capture.
const COVER_MAX_GAPS: usize = 200;

/// Model calls one capture may make to cover gaps: one light call each, outside the tick's budget.
/// A note the model already said does not answer a gap is not asked about again and takes none.
pub const COVER_ASK_MAX: usize = 8;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Groups of `MIN_CLUSTER` or more; a lone gap is left to the ungrouped list.
    pub clusters: usize,
    pub named: usize,
    pub removed: usize,
}

/// A hit that just clears the weak line is not noise, not yet an answer. `repair_once` drops
/// coverage recorded under a line that has since moved up.
pub const COVER_MARGIN: f32 = 0.05;

pub fn cover_line(core: &Core) -> f32 {
    core.weak_below() + COVER_MARGIN
}

/// What the box's link reaches: the search a capture was typed from, and the pursuit it was
/// clustered into.
struct TypedFrom {
    /// Both readings count: whether the row is `unmatched` or `search` can change while a capture
    /// settles.
    event: Option<String>,
    /// Resolved here: a pursuit is keyed on its cluster and carries no event ids
    /// (`Store::pursuit_gaps_of_query`). Pursuit gaps need no low similarity, so they are the wider
    /// hole.
    pursuits: Vec<String>,
}

impl TypedFrom {
    async fn of(core: &Core, event: Option<String>) -> Result<Self> {
        let Some(event) = event else {
            return Ok(Self {
                event: None,
                pursuits: vec![],
            });
        };
        // A search retention has taken leaves nothing to reach through: not a failure.
        let pursuits = match core.store.search_query(&event).await? {
            Some(q) => {
                core.store
                    .pursuit_gaps_of_query(core.embedder.model(), &q)
                    .await?
            }
            None => vec![],
        };
        Ok(Self {
            event: Some(event),
            pursuits,
        })
    }

    fn matches(&self, g: &crate::store::gaps::GapVec) -> bool {
        match g.gap.kind {
            GapKind::Search | GapKind::Unmatched => {
                self.event.as_deref() == Some(g.gap.id.as_str())
            }
            GapKind::Pursuit => self.pursuits.contains(&g.gap.id),
            _ => false,
        }
    }
}

/// Closes, silently and reversibly, each open gap this corpus's own artifacts answer: its best hit
/// above `weak_below`, asked `answers_query` alone, says yes at `act_at`. A no is remembered and
/// not asked twice (`Store::reject_gap_note`). A failure is a warning: a stored capture is stored.
pub async fn cover(core: &Core, corpus_id: &str) -> Result<usize> {
    // What closes the gap the capture was typed from is the link, not a score (`close_line`).
    let typed_from =
        crate::core::ingest::typed_from(&core.store.get_corpus(corpus_id).await?.metadata)
            .map(str::to_string);
    cover_answering(core, corpus_id, typed_from).await
}

/// For a capture whose text the base already held: `ingest` stores nothing, so `cover` never runs,
/// yet the link is a person's claim and still closes the hole.
pub async fn cover_answering(
    core: &Core,
    corpus_id: &str,
    typed_from: Option<String>,
) -> Result<usize> {
    if !core.learn.enabled {
        return Ok(0);
    }
    let mut open = core
        .store
        .open_gaps(core.embedder.model(), core.weak_below())
        .await?;
    if open.gaps.is_empty() {
        return Ok(0);
    }
    let typed_from = TypedFrom::of(core, typed_from).await?;
    // Linked gaps first, all of them, before the cap bites: `cover` runs once. Stable, so the rest
    // keep `open_gaps`' order.
    open.gaps.sort_by_key(|g| !typed_from.matches(g));
    if open.gaps.len() > COVER_MAX_GAPS {
        tracing::debug!(
            corpus_id,
            open = open.gaps.len(),
            checked = COVER_MAX_GAPS,
            "more open gaps than one capture is checked against; the oldest are left out"
        );
        open.gaps.truncate(COVER_MAX_GAPS);
    }
    let filter = crate::vector::SearchFilter {
        tags: Vec::new(),
        category: None,
        include_superseded: false,
        include_deprecated: false,
        corpus_id: Some(corpus_id.to_string()),
    };
    // Indexed by gap, so concurrent answers land in read order and the writes are deterministic.
    let mut best: Vec<Option<crate::vector::SearchHit>> = vec![None; open.gaps.len()];
    let mut failure: Option<crate::error::Error> = None;
    let mut inflight = tokio::task::JoinSet::new();
    let mut next = 0usize;
    loop {
        while inflight.len() < COVER_IN_FLIGHT && next < open.gaps.len() {
            let at = next;
            next += 1;
            let vectors = core.vectors.clone();
            let vec = open.gaps[at].vec.clone();
            let filter = filter.clone();
            // A static semaphore is never closed.
            let permit = COVER_SLOTS
                .acquire()
                .await
                .map_err(|_| crate::error::Error::Internal("the coverage budget closed".into()))?;
            inflight.spawn(async move {
                let out = vectors.search(&vec, &Default::default(), 1, &filter).await;
                // Released before the answer is read: the budget limits load on the store.
                drop(permit);
                (at, out)
            });
        }
        let Some(joined) = inflight.join_next().await else {
            break;
        };
        match joined {
            Ok((at, Ok(hits))) => best[at] = hits.into_iter().next(),
            Ok((_, Err(e))) => {
                failure.get_or_insert(e);
            }
            // A panic in the vector client or a shutdown: a search that did not happen, not a gap
            // that stayed open.
            Err(e) => {
                failure.get_or_insert(crate::error::Error::Internal(e.to_string()));
            }
        }
    }
    let mut closed = 0;
    // The linked gaps first, before anything that can fail: they need no model.
    let mut found = Vec::new();
    for (g, hit) in open.gaps.iter().zip(best) {
        // `gap_coverage` names the answering artifact, so even the linked gap needs a hit.
        let Some(hit) = hit else { continue };
        // On the linked gap the score is recorded, not read, and nothing is asked.
        if typed_from.matches(g) {
            closed += record(core, corpus_id, g, &hit, CoveredBy::Capture, &mut failure).await;
            continue;
        }
        // A hit above the weak line is found; `None` is "no opinion" and finds nothing.
        if let Some(sim) = hit.similarity.filter(|s| *s >= core.weak_below()) {
            found.push((g, hit, sim));
        }
    }
    // Whether a found note covers is the model's to say, the note alone against the gap's
    // question; the best found are asked, and the rest wait for a later capture.
    found.sort_by(|a, b| b.2.total_cmp(&a.2));
    if !found.is_empty() {
        match crate::jobs::label::relevant_at(core).await {
            Err(e) => {
                failure.get_or_insert(e);
            }
            Ok(relevant_at) => {
                let asked = answers_query("");
                let era = era(core.decider.model(), asked.version);
                let mut calls = 0;
                for (g, hit, _) in found {
                    if calls == COVER_ASK_MAX {
                        break;
                    }
                    // What the model said of this note against this gap stands until one of them,
                    // the era or the line changes: it takes no place among the asked.
                    match core
                        .store
                        .gap_note_rejected(&g.gap, &hit.payload.artifact_id, &era, relevant_at)
                        .await
                    {
                        Ok(false) => {}
                        Ok(true) => continue,
                        // Unknown whether it would be a repeat: not asked by this capture.
                        Err(e) => {
                            failure.get_or_insert(e);
                            continue;
                        }
                    }
                    let note = match core.store.get_artifact(&hit.payload.artifact_id).await {
                        Ok(note) if note.in_results() => note,
                        Ok(_) => continue,
                        // The vector store can name an artifact SQLite no longer has: not the
                        // model's failure, so the others are still asked.
                        Err(e) => {
                            failure.get_or_insert(e);
                            continue;
                        }
                    };
                    calls += 1;
                    match crate::jobs::label::answers(core, &g.gap.text, &note).await {
                        Ok(Some(yes)) if yes >= relevant_at => {
                            closed +=
                                record(core, corpus_id, g, &hit, CoveredBy::Model, &mut failure)
                                    .await;
                        }
                        // Remembered, so the next capture does not put the same note to it again.
                        Ok(Some(yes)) => {
                            if let Err(e) = core
                                .store
                                .reject_gap_note(&g.gap, &note.id, note.embed_rev, &era, yes)
                                .await
                            {
                                tracing::warn!(gap_id = %g.gap.id, error = %e, "could not remember that a note does not answer a gap");
                                failure.get_or_insert(e);
                            }
                        }
                        Ok(None) => {
                            tracing::info!(gap_id = %g.gap.id, artifact_id = %note.id, "the note is too long to ask about whole; the gap stays open");
                        }
                        // Never closed on an error, and a model that is down is asked no more.
                        Err(e) => {
                            tracing::warn!(gap_id = %g.gap.id, error = %e, "could not ask whether a capture answers a gap");
                            failure.get_or_insert(e);
                            break;
                        }
                    }
                }
            }
        }
    }
    if closed > 0 {
        tracing::info!(corpus_id, closed, "a capture answered open gaps");
    }
    // Reported only after every answered gap is written: `cover` gets no second pass.
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(closed)
}

/// Writes one coverage row; a failure is a warning, kept for the caller to report.
async fn record(
    core: &Core,
    corpus_id: &str,
    g: &crate::store::gaps::GapVec,
    hit: &crate::vector::SearchHit,
    by: CoveredBy,
    failure: &mut Option<crate::error::Error>,
) -> usize {
    let sim = hit.similarity.unwrap_or_default();
    // Skipped, not returned: the vector store can name an artifact SQLite no longer has, and
    // `cover` runs only once.
    match core
        .store
        .cover_gap(
            g.gap.kind,
            &g.gap.id,
            corpus_id,
            &hit.payload.artifact_id,
            sim,
            by,
        )
        .await
    {
        Ok(()) => 1,
        Err(e) => {
            tracing::warn!(gap_id = %g.gap.id, error = %e, "could not record that a capture answered a gap");
            failure.get_or_insert(e);
            0
        }
    }
}

pub async fn sweep(core: &Core) -> Result<SweepReport> {
    let open = core
        .store
        .open_gaps(core.embedder.model(), core.weak_below())
        .await?;
    let link_at = core.link_at();
    let vecs: Vec<&[f32]> = open.gaps.iter().map(|g| g.vec.as_slice()).collect();
    let groups = cluster(&vecs, link_at);

    let existing = core.store.cluster_keys().await?;
    let mut report = SweepReport::default();
    let mut live_keys = Vec::with_capacity(groups.len());

    for group in &groups {
        // A group of one is a question; naming it buys a restatement, and storing it would re-key
        // whenever a gap joins.
        if group.len() < MIN_CLUSTER {
            continue;
        }
        report.clusters += 1;
        let members: Vec<_> = group
            .iter()
            .map(|&i| (open.gaps[i].gap.kind, open.gaps[i].gap.id.clone()))
            .collect();
        let key = cluster_key(&members);
        live_keys.push(key.clone());
        let known = existing
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.labelled_by.as_str());
        if known == Some("model") {
            continue;
        }
        // Newest first, the order `gap_label_prompt` keeps when it caps a call.
        let texts: Vec<&str> = group
            .iter()
            .map(|&i| open.gaps[i].gap.text.as_str())
            .collect();
        let (label, labelled_by) = match name(core, &texts).await {
            Some(l) => (l, "model"),
            None => (terms_label(&texts), "terms"),
        };
        if labelled_by == "model" {
            report.named += 1;
        }
        core.store
            .put_cluster(&GapCluster {
                key,
                label,
                labelled_by: labelled_by.into(),
                members,
            })
            .await?;
    }

    // Past the cap an unseen key may only be unread, so it keeps its label. An unseen key sharing a
    // member with a group stored this pass was re-keyed by a dismissal and goes, or `gap_rows`
    // renders its survivors twice.
    let clustered_now: std::collections::HashSet<(GapKind, &str)> = groups
        .iter()
        .filter(|g| g.len() >= MIN_CLUSTER)
        .flat_map(|g| g.iter())
        .map(|&i| (open.gaps[i].gap.kind, open.gaps[i].gap.id.as_str()))
        .collect();
    let stale: Vec<String> = existing
        .into_iter()
        .filter(|c| !live_keys.contains(&c.key))
        .filter(|c| {
            !open.capped
                || c.members
                    .iter()
                    .any(|(k, id)| clustered_now.contains(&(*k, id.as_str())))
        })
        .map(|c| c.key)
        .collect();
    if open.capped {
        tracing::info!(
            removing = stale.len(),
            "open gaps past the cap; keeping every group this pass could not \
             reach rather than reading a partial pass as a set of dismissals"
        );
    }
    report.removed = stale.len();
    core.store.delete_clusters(&stale).await?;
    Ok(report)
}

/// Bounded, background lane. Any failure is `None`, and the caller falls back to terms.
async fn name(core: &Core, questions: &[&str]) -> Option<String> {
    let namer = core.gap_namer.clone()?;
    let permit = core.gate.background().await;
    let reply = namer
        .complete(
            crate::infer::prompt::GAP_LABEL_SYSTEM,
            &crate::infer::prompt::gap_label_prompt(questions),
        )
        .await;
    permit.finished();
    match reply.and_then(|r| crate::infer::prompt::parse_gap_label(&r)) {
        Ok(label) => Some(label),
        Err(e) => {
            tracing::warn!(error = %e, "could not name a knowledge gap; using its terms");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::asks::{AskVerdict, NewAsk};

    async fn nothing_here(core: &Core, q: &str, vec: Vec<f32>) -> String {
        let id = core
            .store
            .record_ask(NewAsk {
                question: q.into(),
                filters: "{}".into(),
                query_vec: vec,
                embed_model: core.embedder.model().to_string(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                ..Default::default()
            })
            .await
            .unwrap();
        core.store
            .judge_ask(&id, AskVerdict::NothingHere)
            .await
            .unwrap();
        id
    }

    async fn gap_search(core: &Core, q: &str, vec: Vec<f32>) -> String {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
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
        id
    }

    async fn captured(core: &Core, text: &str) -> String {
        let src = core.ingest(text, "web", None).await.unwrap();
        while crate::jobs::run_one(core).await.unwrap() {}
        core.background.wait_idle().await;
        src.id
    }

    async fn unmatched_search(core: &Core, q: &str, vec: Vec<f32>) -> String {
        core.store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: Some("me".into()),
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    async fn captured_from(core: &Core, text: &str, event_id: &str) -> String {
        let src = core
            .ingest_capture(crate::core::ingest::Capture::new(text, "web").with_search(event_id))
            .await
            .unwrap();
        while crate::jobs::run_one(core).await.unwrap() {}
        core.background.wait_idle().await;
        src.id
    }

    #[tokio::test]
    async fn a_capture_closes_the_search_it_was_typed_from() {
        // The box searches while you type, so the sentence being captured is recorded as a search
        // that found nothing; the stored artifact need not land near it, so only the link closes
        // it.
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = unmatched_search(
            &core,
            "Erinnerung Termin Foto Dienstausweis Mittwoch 0900 Zimmer A323",
            v.clone(),
        )
        .await;
        // Out of reach of the measurement: only the link can close it.
        core.set_weak_below(1.0);

        let corpus = captured_from(
            &core,
            "Erinnerung Termin Foto Dienstausweis Mittwoch 0900 Zimmer A323",
            &id,
        )
        .await;

        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .all(|g| g.gap.id != id),
            "the query is still on the capture page as a hole in the base"
        );
        let covered = core.store.gaps_covered_by(&corpus).await.unwrap();
        assert_eq!(covered.len(), 1, "the capture cannot say what it answered");
        assert_eq!(
            covered[0].text,
            "Erinnerung Termin Foto Dienstausweis Mittwoch 0900 Zimmer A323"
        );
    }

    #[tokio::test]
    async fn a_capture_closes_the_pursuit_its_own_typing_became() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let q = "Netcat can serve a file to one client, then exits";
        let id = unmatched_search(&core, q, v.clone()).await;
        let pid = core
            .store
            .insert_pursuit(
                1,
                &["Netcat can serve".into(), q.into()],
                &[],
                Some((v.as_slice(), core.embedder.model())),
            )
            .await
            .unwrap();
        core.store
            .close_pursuit(&pid, "unsatisfied", "nothing engaged", 1)
            .await
            .unwrap();
        // Out of reach of the measurement: only the link can close either row.
        core.set_weak_below(1.0);

        let corpus = captured_from(&core, q, &id).await;

        let open = core
            .store
            .open_gaps(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(
            open.gaps.iter().all(|g| g.gap.id != pid),
            "the paste is still on the capture page as a hole in the base: {:?}",
            open.gaps.iter().map(|g| &g.gap).collect::<Vec<_>>()
        );
        assert!(
            core.store
                .gaps_covered_by(&corpus)
                .await
                .unwrap()
                .iter()
                .any(|g| g.id == pid),
            "the capture cannot say it answered the pursuit"
        );
    }

    #[tokio::test]
    async fn a_capture_the_base_already_had_still_closes_the_hole_it_was_typed_into() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let text = "Erinnerung Termin Foto Dienstausweis Mittwoch 0900 Zimmer A323";
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let first = unmatched_search(&core, text, v.clone()).await;
        // Out of reach of the measurement: only the link can close anything.
        core.set_weak_below(1.0);
        let corpus = captured_from(&core, text, &first).await;

        let again = unmatched_search(&core, text, v).await;
        let same = captured_from(&core, text, &again).await;
        assert_eq!(same, corpus, "the stored corpus, and no second one");

        let open = core
            .store
            .open_gaps(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(
            open.gaps.iter().all(|g| g.gap.id != again),
            "the second query is still a hole the page would ask back at them"
        );
    }

    #[tokio::test]
    async fn the_repair_pass_keeps_what_a_capture_answered() {
        // A capture typed from the query is not a measurement, so moving the line must not drop its
        // coverage.
        let mut core = test_core().await;
        core.learn.enabled = true;
        let id = unmatched_search(&core, "when is the ID photo", vec![1.0, 0.0, 0.0, 0.0]).await;
        core.set_weak_below(1.0);
        captured_from(&core, "The ID photo is on Wednesday at 09:00.", &id).await;

        core.store.trim_gap_coverage(1.0).await.unwrap();

        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .all(|g| g.gap.id != id),
            "the repair pass reopened a hole a capture had answered"
        );
    }

    async fn best_similarity(core: &Core, v: &[f32], corpus_id: &str) -> f32 {
        core.vectors
            .search(
                v,
                &Default::default(),
                1,
                &crate::vector::SearchFilter {
                    corpus_id: Some(corpus_id.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .first()
            .and_then(|h| h.similarity)
            .expect("the document embedded nothing")
    }

    #[tokio::test]
    async fn a_capture_that_answers_a_gap_closes_it_and_says_which() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01 - COVER_MARGIN);
        answering(&mut core, 0.95, 0.90);

        let closed = cover(&core, &corpus).await.unwrap();

        assert_eq!(closed, 1);
        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .all(|g| g.gap.id != id),
            "the gap is still open"
        );
        let covered = core.store.gaps_covered_by(&corpus).await.unwrap();
        assert_eq!(covered.len(), 1, "the capture page cannot say which");
        assert_eq!(covered[0].text, "how do I mount an E01");
    }

    #[tokio::test]
    async fn a_capture_that_answers_nothing_closes_nothing() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Trimming a systemd journal.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await + 0.01);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert!(
            core.store
                .gaps_covered_by(&corpus)
                .await
                .unwrap()
                .is_empty()
        );
    }

    fn saying(yes: f64) -> std::sync::Arc<crate::infer::fake::RuleDecider> {
        use crate::infer::decide::{Answer, Answers, Question};
        std::sync::Arc::new(crate::infer::fake::RuleDecider::new(
            move |_, qs: &[Question]| {
                Ok(Answers {
                    answers: qs
                        .iter()
                        .map(|q| (q.key.clone(), Answer::Noul { yes }))
                        .collect(),
                    model: "rule-decider".into(),
                    input_tokens: 0,
                })
            },
        ))
    }

    /// The model says `yes` of every note, and a note answers from `relevant_at`: both stated, so
    /// no test leans on where a fake's answer and the configured line happen to sit.
    fn answering(
        core: &mut Core,
        yes: f64,
        relevant_at: f64,
    ) -> std::sync::Arc<crate::infer::fake::RuleDecider> {
        core.decide_cfg.bands.per.insert(
            "answers_query".into(),
            crate::config::BandOverride {
                act_at: Some(relevant_at),
                ask_at: None,
            },
        );
        let dec = saying(yes);
        core.decider = dec.clone();
        dec
    }

    fn asked_about(d: &crate::infer::fake::RuleDecider, q: &str) -> usize {
        d.requests()
            .iter()
            .filter(|(_, qs)| {
                qs.len() == 1
                    && qs[0].key == "answers_query"
                    && qs[0].instructions.contains(&format!("«{q}»"))
            })
            .count()
    }

    #[tokio::test]
    async fn a_gap_closes_when_a_new_note_answers_its_question() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        // Found, but under the old line: only the answer can close it.
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01);
        let dec = answering(&mut core, 0.95, 0.90);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);
        assert_eq!(asked_about(&dec, "how do I mount an E01"), 1);
        let covered = core.store.gaps_covered_by(&corpus).await.unwrap();
        assert_eq!(covered.len(), 1);
        assert_eq!(covered[0].id, id);
        // A model's answer is not a distance: moving the line does not reopen it.
        core.store.trim_gap_coverage(1.0).await.unwrap();
        assert_eq!(core.store.gaps_covered_by(&corpus).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_similar_note_that_does_not_answer_leaves_the_gap_open() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01 - COVER_MARGIN);
        let dec = answering(&mut core, 0.2, 0.90);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(asked_about(&dec, "how do I mount an E01"), 1);
        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .any(|g| g.gap.id == id)
        );
    }

    #[tokio::test]
    async fn a_note_answers_from_the_line_and_not_a_hair_under_it() {
        for (yes, closes) in [(0.80, 1), (0.79, 0)] {
            let mut core = test_core().await;
            core.learn.enabled = true;
            let v = vec![1.0, 0.0, 0.0, 0.0];
            gap_search(&core, "how do I mount an E01", v.clone()).await;
            core.set_weak_below(1.0);
            let corpus = captured(&core, "Mounting an E01 image read-only.").await;
            core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01);
            answering(&mut core, yes, 0.80);

            assert_eq!(cover(&core, &corpus).await.unwrap(), closes, "{yes}");
        }
    }

    const MOUNT: &str = "how do I mount an E01";

    /// A gap worded `MOUNT`, and a capture found near it that the model said does not answer it:
    /// (gap, corpus, the decider that said so).
    async fn said_no_once(
        core: &mut Core,
    ) -> (
        String,
        String,
        std::sync::Arc<crate::infer::fake::RuleDecider>,
    ) {
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(core, MOUNT, v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(core, &v, &corpus).await - 0.01);
        let dec = answering(core, 0.2, 0.90);
        assert_eq!(cover(core, &corpus).await.unwrap(), 0);
        assert_eq!(asked_about(&dec, MOUNT), 1);
        (id, corpus, dec)
    }

    #[tokio::test]
    async fn a_note_the_model_said_no_of_is_not_put_to_the_same_gap_again() {
        let mut core = test_core().await;
        let (id, corpus, dec) = said_no_once(&mut core).await;

        // The corpus settles again, and the box closes over it again: nothing more is asked.
        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(cover_answering(&core, &corpus, None).await.unwrap(), 0);

        assert_eq!(dec.calls(), 1, "the same note, the same question, twice");
        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .any(|g| g.gap.id == id),
            "a no closes nothing"
        );
    }

    #[tokio::test]
    async fn a_note_that_changed_is_put_to_the_gap_again() {
        let mut core = test_core().await;
        let (_, corpus, dec) = said_no_once(&mut core).await;
        // The note that was asked about: the capture's nearest to the gap.
        let note = core
            .vectors
            .search(
                &[1.0, 0.0, 0.0, 0.0],
                &Default::default(),
                1,
                &crate::vector::SearchFilter {
                    corpus_id: Some(corpus.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .remove(0)
            .payload
            .artifact_id;
        core.store
            .update_artifact_text(&note, "Mount an E01 read-only with ewfmount.")
            .await
            .unwrap();

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(asked_about(&dec, MOUNT), 2);
        // And what it said of the new text stands in turn.
        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(dec.calls(), 2);
    }

    #[tokio::test]
    async fn a_gap_worded_anew_is_asked_about_again() {
        let mut core = test_core().await;
        let (id, corpus, dec) = said_no_once(&mut core).await;
        // The box folds a longer typing onto the search it grew from: the same row, other words.
        sqlx::query!(
            "UPDATE search_events SET query = 'mount an E01 image read-only' WHERE id = ?",
            id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(asked_about(&dec, "mount an E01 image read-only"), 1);
        assert_eq!(dec.calls(), 2);
    }

    #[tokio::test]
    async fn another_model_is_asked_what_the_last_one_said_no_to() {
        let mut core = test_core().await;
        let (_, corpus, dec) = said_no_once(&mut core).await;
        // Another model is another era; the line is where it was.
        core.decider = std::sync::Arc::new(crate::infer::fake::FakeDecider::saying(0.95));

        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);
        assert_eq!(dec.calls(), 1, "the first model was not asked again");
    }

    #[tokio::test]
    async fn a_line_that_came_down_under_what_the_model_said_asks_again() {
        let mut core = test_core().await;
        let (_, corpus, _) = said_no_once(&mut core).await;
        // The same model saying the same 0.2, and a line it now clears.
        let dec = answering(&mut core, 0.2, 0.15);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);
        assert_eq!(asked_about(&dec, MOUNT), 1);
    }

    #[tokio::test]
    async fn a_call_that_fails_leaves_the_gap_open() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01 - COVER_MARGIN);
        core.decider = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(crate::error::Error::Internal(
                "the decision model is down".into(),
            ))
        }));

        assert!(cover(&core, &corpus).await.is_err());
        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .any(|g| g.gap.id == id)
        );
    }

    #[tokio::test]
    async fn a_capture_asks_about_its_best_eight_gaps_and_no_more() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let vecs: Vec<Vec<f32>> = (0..12)
            .map(|i| vec![1.0, i as f32 * 0.3, (12 - i) as f32 * 0.05, 0.0])
            .collect();
        for (i, v) in vecs.iter().enumerate() {
            gap_search(&core, &format!("gap number {i}"), v.clone()).await;
        }
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        let mut sims = Vec::new();
        for (i, v) in vecs.iter().enumerate() {
            sims.push((best_similarity(&core, v, &corpus).await, i));
        }
        let floor = sims.iter().map(|s| s.0).fold(f32::MAX, f32::min);
        core.set_weak_below(floor - 0.01);
        let dec = answering(&mut core, 0.2, 0.90);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);

        let asked: Vec<f32> = dec
            .requests()
            .iter()
            .map(|(_, qs)| {
                let i = (0..12)
                    .find(|i| qs[0].instructions.contains(&format!("«gap number {i}»")))
                    .unwrap();
                sims.iter().find(|s| s.1 == i).unwrap().0
            })
            .collect();
        assert_eq!(asked.len(), COVER_ASK_MAX);
        assert!(
            asked.windows(2).all(|w| w[0] >= w[1]),
            "best first: {asked:?}"
        );
        sims.sort_by(|a, b| b.0.total_cmp(&a.0));
        assert!(
            asked.last().unwrap() >= &sims[COVER_ASK_MAX - 1].0,
            "{asked:?} {sims:?}"
        );
    }

    #[tokio::test]
    async fn what_one_capture_did_not_get_to_ask_the_next_one_does() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let vecs: Vec<Vec<f32>> = (0..12)
            .map(|i| vec![1.0, i as f32 * 0.3, (12 - i) as f32 * 0.05, 0.0])
            .collect();
        for (i, v) in vecs.iter().enumerate() {
            gap_search(&core, &format!("gap number {i}"), v.clone()).await;
        }
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        let mut floor = f32::MAX;
        for v in &vecs {
            floor = floor.min(best_similarity(&core, v, &corpus).await);
        }
        core.set_weak_below(floor - 0.01);
        let dec = answering(&mut core, 0.2, 0.90);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(dec.calls(), COVER_ASK_MAX);
        // The eight it said no of take no place: the four left are asked, and then nothing is.
        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(dec.calls(), 12);
        for i in 0..12 {
            assert_eq!(
                asked_about(&dec, &format!("gap number {i}")),
                1,
                "gap number {i}"
            );
        }
        assert_eq!(cover(&core, &corpus).await.unwrap(), 0);
        assert_eq!(dec.calls(), 12);
    }

    #[tokio::test]
    async fn a_first_call_that_fails_asks_no_more_and_closes_nothing() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        for i in 0..3 {
            gap_search(
                &core,
                &format!("how do I mount an E01, take {i}"),
                v.clone(),
            )
            .await;
        }
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01);
        let dec = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(crate::error::Error::Internal(
                "the decision model is down".into(),
            ))
        }));
        core.decider = dec.clone();

        assert!(cover(&core, &corpus).await.is_err());
        assert_eq!(dec.calls(), 1);
        assert!(
            core.store
                .gaps_covered_by(&corpus)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_linked_gap_is_covered_while_the_model_is_down() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        gap_search(&core, "how do I mount an E01", v.clone()).await;
        let typed = unmatched_search(&core, "the ID photo is on Wednesday", v.clone()).await;
        core.set_weak_below(0.0);
        let dec = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Err(crate::error::Error::Internal(
                "the decision model is down".into(),
            ))
        }));
        core.decider = dec.clone();
        let src = core
            .ingest_capture(
                crate::core::ingest::Capture::new("The ID photo is on Wednesday at 09:00.", "web")
                    .with_search(&typed),
            )
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}
        core.background.wait_idle().await;

        let covered = core.store.gaps_covered_by(&src.id).await.unwrap();
        assert_eq!(
            covered.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            [typed.as_str()],
            "the link closes its gap; the other waits for a model"
        );
        assert!(
            asked_about(&dec, "how do I mount an E01") >= 1,
            "the other gap was found and its call failed"
        );
    }

    #[tokio::test]
    async fn a_decide_gap_closes_the_same_way() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = core
            .store
            .record_decision(crate::store::decisions::NewDecision {
                question: "Keep the E01 mounted?".into(),
                questions: "{}".into(),
                situation: None,
                scope: None,
                filters: "{}".into(),
                answers: None,
                prior: "{}".into(),
                standing: r#"{"verdict":"not_held"}"#.into(),
                not_held: true,
                weak: false,
                model: "clef".into(),
                decider: "clef".into(),
                read: 0,
                dropped: 0,
                query_vec: v.clone(),
                embed_model: core.embedder.model().to_string(),
                evidence: vec![],
            })
            .await
            .unwrap();
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01);
        let dec = answering(&mut core, 0.95, 0.90);

        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);
        assert_eq!(asked_about(&dec, "Keep the E01 mounted?"), 1);
        assert!(
            core.store
                .gaps_covered_by(&corpus)
                .await
                .unwrap()
                .iter()
                .any(|g| g.id == id && g.kind == GapKind::Decide)
        );
    }

    #[tokio::test]
    async fn one_gap_the_capture_cannot_record_does_not_cost_the_others() {
        // The vector store names an artifact SQLite no longer has, and `gap_coverage` has a foreign
        // key onto it.
        let mut core = test_core().await;
        core.learn.enabled = true;
        let dangling = vec![0.0, 1.0, 0.0, 0.0];
        let answerable = vec![1.0, 0.0, 0.0, 0.0];
        gap_search(&core, "what does a torn write look like", dangling.clone()).await;
        let answerable_id = gap_search(&core, "how do I mount an E01", answerable.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(
            best_similarity(&core, &answerable, &corpus).await - 0.01 - COVER_MARGIN,
        );
        answering(&mut core, 0.95, 0.90);

        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: dangling.clone(),
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: "no-such-artifact".into(),
                    corpus_id: corpus.clone(),
                    text: "a torn write".into(),
                    created_at: crate::store::now(),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();

        assert!(cover(&core, &corpus).await.is_err());

        let covered = core.store.gaps_covered_by(&corpus).await.unwrap();
        assert!(
            covered.iter().any(|g| g.id == answerable_id),
            "the gap the capture did answer was thrown away with the one it could not"
        );
    }

    #[tokio::test]
    async fn deleting_the_capture_that_closed_a_gap_reopens_it() {
        let mut core = test_core().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        let id = gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01 - COVER_MARGIN);
        answering(&mut core, 0.95, 0.90);
        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);

        core.delete_corpus(&corpus).await.unwrap();

        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .gaps
                .iter()
                .any(|g| g.gap.id == id),
            "the judgement went with the capture that answered it"
        );
    }

    #[tokio::test]
    async fn the_coverage_check_embeds_nothing() {
        // An embedding call here would pay for a stored vector again, per open gap, per capture.
        let (mut core, embedder) =
            crate::core::test_support::test_core_counting_embed_calls().await;
        core.learn.enabled = true;
        let v = vec![1.0, 0.0, 0.0, 0.0];
        gap_search(&core, "how do I mount an E01", v.clone()).await;
        core.set_weak_below(1.0);
        let corpus = captured(&core, "Mounting an E01 image read-only.").await;
        core.set_weak_below(best_similarity(&core, &v, &corpus).await - 0.01 - COVER_MARGIN);
        answering(&mut core, 0.95, 0.90);
        let before = embedder.calls();

        assert_eq!(cover(&core, &corpus).await.unwrap(), 1);
        assert_eq!(
            embedder.calls(),
            before,
            "the coverage check embedded something"
        );
    }

    #[tokio::test]
    async fn a_new_cluster_is_named_once_and_a_vanished_one_is_removed() {
        let core = test_core().await;
        let a = nothing_here(&core, "mount an E01", vec![1.0, 0.0]).await;
        nothing_here(&core, "mounting E01 images", vec![0.95, 0.05]).await;
        nothing_here(&core, "E01 read only mount", vec![0.9, 0.1]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;

        let r = sweep(&core).await.unwrap();
        assert_eq!(
            r,
            SweepReport {
                clusters: 1,
                named: 1,
                removed: 0
            },
            "the lone 'FAT entries' gap is not a group and costs no call"
        );
        let (rows, loose) = core
            .store
            .gap_rows(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].members.len(), 3);
        assert!(rows[0].label == "Fake topic" && rows[0].labelled_by == "model");
        assert_eq!(
            loose.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            vec!["FAT entries"],
            "an ungrouped gap is shown under its own words, not under a name \
             bought for it"
        );

        assert_eq!(sweep(&core).await.unwrap().named, 0);

        core.store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named, r.removed), (1, 1, 1));
    }

    #[tokio::test]
    async fn a_gap_on_its_own_is_never_named() {
        let core = test_core().await;
        nothing_here(&core, "mount an E01", vec![1.0, 0.0]).await;
        nothing_here(&core, "FAT entries", vec![0.0, 1.0]).await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
        let (rows, loose) = core
            .store
            .gap_rows(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert_eq!(loose.len(), 2);
    }

    #[tokio::test]
    async fn without_a_readable_model_a_cluster_is_named_by_its_terms_and_retried_later() {
        let mut core = test_core().await;
        core.gap_namer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some("not json".into()),
        }));
        nothing_here(&core, "mount an E01 image", vec![1.0]).await;
        nothing_here(&core, "E01 mount read only", vec![1.0]).await;
        let r = sweep(&core).await.unwrap();
        assert_eq!((r.clusters, r.named), (1, 0));
        let (rows, _) = core
            .store
            .gap_rows(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert_eq!(rows[0].labelled_by, "terms");
        assert!(rows[0].label.contains("e01"), "{}", rows[0].label);

        core.gap_namer = Some(std::sync::Arc::new(crate::infer::fake::FakeCompleter {
            reply: Some(r#"{"label":"Image mounting"}"#.into()),
        }));
        assert_eq!(sweep(&core).await.unwrap().named, 1);
        assert_eq!(
            core.store
                .gap_rows(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .0[0]
                .label,
            "Image mounting"
        );
    }

    #[tokio::test]
    async fn a_partial_pass_is_not_read_as_a_set_of_dismissals() {
        let core = test_core().await;
        for i in 0..crate::store::gaps::MAX_OPEN_GAPS + 1 {
            nothing_here(&core, &format!("gap {i}"), vec![1.0, 0.0]).await;
        }
        sweep(&core).await.unwrap();

        core.store
            .put_cluster(&GapCluster {
                key: "beyond-the-cap".into(),
                label: "Older holes".into(),
                labelled_by: "model".into(),
                members: vec![(GapKind::Ask, "an-older-gap".into())],
            })
            .await
            .unwrap();

        let after = sweep(&core).await.unwrap();
        assert_eq!(after.removed, 0, "nothing was dismissed");
        assert!(
            core.store
                .cluster_keys()
                .await
                .unwrap()
                .iter()
                .any(|c| c.key == "beyond-the-cap"),
            "the group the partial pass could not see was deleted as stale"
        );
    }

    #[tokio::test]
    async fn a_partial_pass_still_removes_the_key_it_replaced() {
        let core = test_core().await;
        let mut ids = Vec::new();
        for i in 0..crate::store::gaps::MAX_OPEN_GAPS + 2 {
            ids.push(nothing_here(&core, &format!("gap {i}"), vec![1.0, 0.0]).await);
        }
        sweep(&core).await.unwrap();
        let before = core.store.cluster_keys().await.unwrap();
        assert_eq!(before.len(), 1, "one group over everything the pass read");

        core.store
            .dismiss_gap(GapKind::Ask, ids.last().unwrap())
            .await
            .unwrap();
        let after = sweep(&core).await.unwrap();
        assert!(
            core.store
                .open_gaps(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .capped,
            "the fixture must keep the cap biting, or this proves nothing"
        );
        assert_eq!(after.removed, 1, "the re-keyed group left one key behind");
        let keys = core.store.cluster_keys().await.unwrap();
        assert!(
            keys.iter().all(|c| c.key != before[0].key),
            "the key this pass replaced was kept alongside the one that replaced it"
        );

        let (rows, _) = core
            .store
            .gap_rows(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        for r in &rows {
            for m in &r.members {
                assert!(
                    seen.insert((m.kind, m.id.clone())),
                    "a gap rendered under two headings at once"
                );
            }
        }
    }

    #[tokio::test]
    async fn no_gaps_means_no_clusters_and_no_calls() {
        let core = test_core().await;
        assert_eq!(sweep(&core).await.unwrap(), SweepReport::default());
    }
}
