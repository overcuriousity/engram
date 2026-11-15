//! Learning what belongs together: a pure-SQLite sweep over the search log, and a queued decision
//! per link: what kind of relation it is, rendered as a line, never written by a model.

use crate::core::Core;
use crate::error::Result;
use crate::store::links::normalize_query;

/// Last `search_events.created_at` folded into links.
pub const EVENTS_AFTER: &str = "associate.events_after";
/// Last `search_events.judged_at` folded into links.
pub const JUDGED_AFTER: &str = "associate.judged_after";
/// A ceiling, not a budget: a base offline for a month catches up over a few sweeps rather than
/// holding a worker for minutes.
const REPLAY_LIMIT: i64 = 2_000;

/// A bound on one tick's work; the next sweep prunes what the cap leaves.
const PRUNE_SCAN_LIMIT: i64 = 5_000;

/// Canonical, so the same pair never gets two units.
pub fn link_target(a: &str, b: &str) -> String {
    let (a, b) = crate::store::links::canonical(a, b);
    format!("{a}|{b}")
}

/// The exact complement of `record_search`'s fold predicate (`at - created <= coalesce_secs`), so
/// no event is both foldable and replayed.
fn settled_cutoff(at: i64, coalesce_secs: i64) -> i64 {
    at - coalesce_secs.max(0)
}

/// Watermarks are stamps, not rows, so a full batch leaves its last second for the next sweep
/// rather than strand rows sharing it. A full batch within one second is replayed whole and the
/// skip is logged at `warn`.
fn replayable(stamps: &[i64], limit: usize) -> usize {
    if stamps.len() < limit {
        return stamps.len();
    }
    let last = match stamps.last() {
        Some(s) => *s,
        None => return 0,
    };
    let end = stamps.partition_point(|s| *s < last);
    if end == 0 {
        tracing::warn!(
            stamp = last,
            read = stamps.len(),
            "more rows share one second than a single sweep reads; \
             the remainder of that second will not be replayed"
        );
        return stamps.len();
    }
    end
}

/// What one association sweep did, kept in the sweep account so a thinning sweep shows on Ops.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Report {
    pub events: usize,
    pub verdicts: usize,
    pub forgotten: u64,
    /// Judged links reopened because one side was re-embedded.
    pub reopened: u64,
    /// Judge units armed; the calls happen later, one unit at a time.
    pub armed: i64,
}

pub async fn run(core: &Core) -> Result<Report> {
    if !core.associating() {
        return Ok(Report::default());
    }
    let at = crate::store::now();
    let bound = replay_events(core, at).await?;
    let confirmed = replay_verdicts(core, at).await?;

    let forgotten = core
        .store
        .prune_learning_links(
            core.associate.prune_below,
            core.associate.half_life_days,
            at,
            PRUNE_SCAN_LIMIT,
        )
        .await?;
    // Reopened before anything is armed, so a changed link is re-asked in this same sweep.
    let reopened = core
        .store
        .reopen_stale_judged_links(PRUNE_SCAN_LIMIT)
        .await?;

    // A decision unit: armed only inside this tick's decision budget.
    let budget = core
        .associate
        .judge_per_sweep
        .min(i64::try_from(core.judging_budget().await?).unwrap_or(i64::MAX));
    let to_judge = core
        .store
        .links_to_judge(
            &link_era(core),
            core.associate.judge_min,
            core.associate.judge_min_queries,
            core.associate.half_life_days,
            at,
            budget,
        )
        .await?;
    let mut armed = 0;
    for l in to_judge {
        let target = link_target(&l.a_id, &l.b_id);
        // Re-arming a queued link is a no-op that costs another link its turn.
        if core
            .store
            .live_job(crate::store::jobs::Stage::LinkJudge, &target)
            .await?
        {
            continue;
        }
        core.store
            .rearm_idle_seq(crate::store::jobs::Stage::LinkJudge, "link", &target, armed)
            .await?;
        armed += 1;
    }

    tracing::info!(
        events = bound,
        verdicts = confirmed,
        forgotten,
        reopened,
        armed,
        "association sweep"
    );
    Ok(Report {
        events: bound,
        verdicts: confirmed,
        forgotten,
        reopened,
        armed,
    })
}

/// Only settled events: one inside `feedback.coalesce_secs` may still fold into a longer query.
async fn replay_events(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let settled = settled_cutoff(at, core.feedback.coalesce_secs);

    let events = sqlx::query!(
        r#"SELECT id AS "id!", query, created_at FROM search_events
          WHERE created_at > ? AND created_at < ?
          ORDER BY created_at ASC, id ASC LIMIT ?"#,
        after,
        settled,
        REPLAY_LIMIT
    )
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events.iter().map(|e| e.created_at).collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.id.clone();
        let cue = normalize_query(&e.query);
        let shown = shown_candidates(core, &id).await?;
        // One transaction per event: pairs are quadratic in what was shown, and each transaction is
        // exclusive.
        let pairs: Vec<(&str, &str)> = (0..shown.len())
            .flat_map(|i| ((i + 1)..shown.len()).map(move |j| (i, j)))
            .map(|(i, j)| (shown[i].as_str(), shown[j].as_str()))
            .collect();
        core.store
            .bump_links(&pairs, 1.0, Some(&cue), core.associate.half_life_days, at)
            .await?;
        // Committed per finished second: a failure propagates, and an unmoved watermark would count
        // the bumps twice.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(EVENTS_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// The watermark may only advance to a second every row of which has been folded in.
fn last_of_its_second(stamps: &[i64], idx: usize, end: usize) -> bool {
    idx + 1 == end || stamps[idx + 1] > stamps[idx]
}

/// Its own cursor, because a verdict arrives days after its event. `gap` and `discard` say nothing
/// about a pair.
async fn replay_verdicts(core: &Core, at: i64) -> Result<usize> {
    let after: i64 = core
        .store
        .meta_get(JUDGED_AFTER)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Bounded above at `at`: a verdict written later in the same second would otherwise fail `> at`
    // forever. Verdicts do not coalesce, so `settled_cutoff` is not needed.
    let events = sqlx::query!(
        r#"SELECT id AS "id!", expect_id AS "expect_id!", judged_at AS "judged_at!" FROM search_events
          WHERE judged_at > ? AND judged_at < ?
            AND verdict = 'hit' AND expect_id IS NOT NULL
          ORDER BY judged_at ASC, id ASC LIMIT ?"#,
        after,
        at,
        REPLAY_LIMIT
    )
    .fetch_all(&core.store.pool)
    .await?;

    let stamps: Vec<i64> = events.iter().map(|e| e.judged_at).collect();
    let end = replayable(&stamps, REPLAY_LIMIT as usize);

    for (idx, e) in events[..end].iter().enumerate() {
        let id: String = e.id.clone();
        let expect: String = e.expect_id.clone();
        let shown = shown_candidates(core, &id).await?;
        // Only a shown pair containing the answer binds harder: a find has no shown pair, and
        // binding it would invent a co-retrieval.
        if shown.contains(&expect) {
            // No cue: the event's words were already folded in when its co-appearance was replayed.
            let pairs: Vec<(&str, &str)> = shown
                .iter()
                .filter(|c| **c != expect)
                .map(|other| (expect.as_str(), other.as_str()))
                .collect();
            core.store
                .bump_links(&pairs, 2.0, None, core.associate.half_life_days, at)
                .await?;
        }
        // Raised even for a find: a confirmed artifact the ranking never returned is the most
        // valuable signal.
        core.store
            .bump_activation(
                std::slice::from_ref(&expect),
                core.activation.confirmed,
                core.activation.half_life_days,
                at,
            )
            .await?;
        crate::jobs::promote::maybe_promote(core, std::slice::from_ref(&expect), at).await?;
        // Same as `replay_events`: a verdict counted twice raises activation twice.
        if last_of_its_second(&stamps, idx, end) {
            core.store
                .meta_set(JUDGED_AFTER, &stamps[idx].to_string())
                .await?;
        }
    }
    Ok(end)
}

/// The ranked list only: reading the band's recalled artifacts would let a link strengthen itself
/// by having fired.
async fn shown_candidates(core: &Core, event_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar!(
        "SELECT artifact_id FROM search_candidates
          WHERE event_id = ? AND shown = 1 AND band = 0 ORDER BY rank",
        event_id
    )
    .fetch_all(&core.store.pool)
    .await?)
}

use crate::core::judging::band::Band;
use crate::core::judging::{self, Applier, Subjects, catalogue, state};
use crate::error::Error;
use crate::infer::decide::decide_swapped;
use crate::store::actions::Job;
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::decision_log::{Logged, Outcome};
use crate::store::links::{Link, LinkKind, LinkState};
use crate::store::pairs::{DecidedBy, Net};
use async_trait::async_trait;

fn day(at: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

fn note(c: &Chunk) -> state::Note<'_> {
    state::Note {
        title: c.title.as_deref(),
        day: day(c.created_at),
        text: &c.text,
    }
}

pub(crate) fn link_era(core: &Core) -> String {
    crate::core::judging::band::era(core.decider.model(), catalogue::link_kind().version)
}

/// The subject kind of a `link_kind` decision on a neighbour shown at capture; a link found by
/// searches is `"link"`. The decision carries it to where a pair is filed: what a shown neighbour
/// hands to consolidation may not act alone.
pub const SHOWN: &str = "shown";

/// What a shown neighbour's link weighs while it waits to be asked: under `show_min`, so nothing
/// shows it before an answer, and over `prune_below`, so it lasts until the sweep reaches it.
fn waiting_weight(core: &Core) -> f64 {
    (core.associate.prune_below + core.associate.show_min) / 2.0
}

/// After a judged capture: ask `link_kind` between the anchor and each neighbour it was shown;
/// only an act makes the link here, an Unsure row's pick makes it later. A neighbour the tick has
/// no call left for, or whose question failed, waits as a learning link. Returns the links made.
pub async fn link_shown(core: &Core, anchor_id: &str, shown: &[String]) -> Result<usize> {
    let anchor = core.store.get_artifact(anchor_id).await?;
    if !anchor.in_results() {
        return Ok(0);
    }
    let mut budget = core.judging_budget().await?;
    let mut made = 0;
    for id in shown {
        if id == anchor_id {
            continue;
        }
        // One neighbour that cannot be read or asked about does not cost the others their question.
        match ask_shown(core, &anchor, id, &mut budget).await {
            Ok(true) => made += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                anchor = anchor_id,
                neighbour = %id,
                error = %e,
                "a shown neighbour could not be asked about"
            ),
        }
    }
    Ok(made)
}

/// One shown neighbour; true when its answer made the link.
async fn ask_shown(core: &Core, anchor: &Chunk, id: &str, budget: &mut usize) -> Result<bool> {
    let other = match core.store.get_artifact(id).await {
        Ok(c) if c.in_results() => c,
        Ok(_) | Err(Error::NotFound) => return Ok(false),
        Err(e) => return Err(e),
    };
    // A link already ruled on, or dismissed by a person, is not asked about again here.
    if core
        .store
        .get_link(&anchor.id, id)
        .await?
        .is_some_and(|l| l.state != LinkState::Learning)
    {
        return Ok(false);
    }
    let (a, b) = if anchor.id <= other.id {
        (anchor, &other)
    } else {
        (&other, anchor)
    };
    if asked_already(core, a, b).await? {
        return Ok(false);
    }
    let waits = || {
        core.store.record_shown_link(
            &a.id,
            &b.id,
            waiting_weight(core),
            core.associate.half_life_days,
        )
    };
    if *budget == 0 {
        waits().await?;
        return Ok(false);
    }
    *budget -= 1;
    if let Err(e) = ask_link(core, SHOWN, a, b, &[]).await {
        waits().await?;
        return Err(e);
    }
    Ok(core
        .store
        .get_link(&a.id, &b.id)
        .await?
        .is_some_and(|l| l.state == LinkState::Related))
}

/// Asked in this era about both texts as they are now, and not put back since. `a` and `b` in
/// canonical order, as every link question is asked.
pub(crate) async fn asked_already(core: &Core, a: &Chunk, b: &Chunk) -> Result<bool> {
    let last = core
        .store
        .last_logged("link_kind", &a.id, Some(&b.id), &link_era(core))
        .await?;
    Ok(last.is_some_and(|d| {
        d.rev_a == Some(a.embed_rev)
            && d.rev_b == Some(b.embed_rev)
            && !matches!(d.reason.as_deref(), Some("asked_again" | "changed"))
    }))
}

/// `Stage::LinkJudge`, target `"<a_id>|<b_id>"`: ask `link_kind`, with `link_direction` in the same
/// request, and settle the answer.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    // A unit queued before `[learn]` was switched off must not spend a call after the operator
    // said stop.
    if !core.associating() {
        return Ok(());
    }
    let (a_id, b_id) = target.split_once('|').ok_or(Error::NotFound)?;
    let Some(link) = core.store.get_link(a_id, b_id).await? else {
        // Pruned, or one side deleted, while the unit waited.
        return Ok(());
    };
    if link.state != LinkState::Learning {
        // Answered by an operator's dismissal, or by a sibling unit.
        return Ok(());
    }
    let a = core.store.get_artifact(&link.a_id).await?;
    let b = core.store.get_artifact(&link.b_id).await?;
    // Re-checked here: a side can be superseded while this waits, and no one will be shown it.
    if !a.in_results() || !b.in_results() {
        return Ok(());
    }
    // A retried or second unit must not ask what is already answered.
    if asked_already(core, &a, &b).await? {
        return Ok(());
    }
    let cues: Vec<String> = link.cues.iter().map(|c| c.q.clone()).collect();
    // A neighbour shown at capture that waited for the sweep is still that.
    let source = if link.shown { SHOWN } else { "link" };
    ask_link(core, source, &a, &b, &cues).await?;
    Ok(())
}

/// The one way a link is asked about: `link_kind` with `link_direction` in the same request, the
/// direction checked with the notes exchanged, then settled. `a` and `b` in canonical order.
pub(crate) async fn ask_link(
    core: &Core,
    subject_kind: &'static str,
    a: &Chunk,
    b: &Chunk,
    cues: &[String],
) -> Result<Logged> {
    let d = read_link(core, subject_kind, a, b, cues).await?;
    judging::settle(core, d).await
}

/// `ask_link` up to its settling: the decision as the two reads leave it.
async fn read_link(
    core: &Core,
    subject_kind: &'static str,
    a: &Chunk,
    b: &Chunk,
    cues: &[String],
) -> Result<Logged> {
    let (kind, direction) = (catalogue::link_kind(), catalogue::link_direction());
    let room = state::room(core, &[&kind, &direction]);
    let s = state::link(&note(a), &note(b), cues, &core.counter, room);
    let subjects = Subjects {
        kind: subject_kind,
        a: a.id.clone(),
        b: Some(b.id.clone()),
        rev_a: Some(a.embed_rev),
        rev_b: Some(b.embed_rev),
    };
    let d = judging::ask_with(
        core,
        &kind,
        &[("direction", &direction)],
        subjects,
        s.as_deref(),
    )
    .await?;
    exchanged(core, d, a, b, cues, room).await
}

/// A direction the pick could store is asked once more with the notes exchanged: option order
/// cannot show a pull to the note labelled A, this can. Its pick and whether its own swapped read
/// agreed are kept beside the direction's read. `room` is the first read's: the same searches fit.
async fn exchanged(
    core: &Core,
    d: Logged,
    a: &Chunk,
    b: &Chunk,
    cues: &[String],
    room: usize,
) -> Result<Logged> {
    let directional = LinkKind::parse(&d.pick).is_some_and(LinkKind::directional);
    let mut answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap_or_default();
    let agreed = answer["direction"]["agree"].as_bool() == Some(true);
    if d.band == Band::Leave || !directional || !agreed {
        return Ok(d);
    }
    let direction = catalogue::link_direction();
    let Some(s) = state::link(&note(b), &note(a), cues, &core.counter, room) else {
        return Ok(d);
    };
    let permit = core.gate.background_light().await;
    let got = decide_swapped(
        core.decider.as_ref(),
        &s,
        std::slice::from_ref(&direction.question),
    )
    .await;
    permit.finished();
    let got = got?;
    let key = &direction.question.key;
    let read = got
        .answers
        .answers
        .get(key)
        .ok_or_else(|| Error::InferenceRejected {
            role: "decide",
            detail: format!("the answer has none for \"{key}\""),
        })?;
    let (again, _) = crate::core::judging::band::pick_and_top(&direction.question, read);
    answer["direction"]["exchanged"] = again.into();
    answer["direction"]["exchanged_agree"] = serde_json::json!(got.agree.get(key));
    core.store
        .set_decision_answer(&d.id, &answer.to_string())
        .await?;
    core.store.logged(&d.id).await?.ok_or(Error::NotFound)
}

/// Whether the decision's `subject_a` comes first: only when the direction's swapped read agreed,
/// the exchanged read's own swapped read agreed and, with the notes exchanged, the other label was
/// named.
fn first_is_a(d: &Logged) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(&d.answer).ok()?;
    let r = v.get("direction")?;
    if !r.get("agree")?.as_bool()? || !r.get("exchanged_agree")?.as_bool()? {
        return None;
    }
    match (r.get("pick")?.as_str()?, r.get("exchanged")?.as_str()?) {
        ("a_first", "b_first") => Some(true),
        ("b_first", "a_first") => Some(false),
        _ => None,
    }
}

/// The pair a duplicate or a contradiction hands to consolidation; score zero, as no cosine was
/// measured. From a shown neighbour it is filed restricted: nearness at capture is no ground to
/// merge or supersede without a person. A pair already there keeps what it has. True when filed.
async fn hand_over(core: &Core, x: &str, y: &str, shown: bool) -> Result<bool> {
    if shown {
        core.store
            .record_pair_in(x, y, 0.0, Some(Net::Shown), Some("link"))
            .await
    } else {
        core.store.record_pair_with_detail(x, y, 0.0, "link").await
    }
}

/// What a kind does to a link, the same for a link found by searches and one from a shown
/// neighbour. No journal row: a link hides nothing.
async fn carry_out(
    core: &Core,
    link: &Link,
    a: &Chunk,
    b: &Chunk,
    kind: LinkKind,
    d: &Logged,
) -> Result<()> {
    let (x, y) = (link.a_id.as_str(), link.b_id.as_str());
    // The revisions read: a text changed since reopens the link at the next sweep.
    let revs = d.rev_a.zip(d.rev_b);
    let shown = d.subject_kind == SHOWN;
    // Never over passages: they are not consolidation's material.
    let passages = [a, b]
        .iter()
        .filter(|c| c.provenance == Provenance::Passage)
        .count();
    match kind {
        LinkKind::Duplicate => {
            // Consolidation owns every decision that hides an artifact. The line is shown verbatim,
            // so it claims no handover that did not happen and calls only a passage stored source
            // text. No kind is kept: the line says it.
            let reason = match passages {
                2 => "same content, and both are stored source text",
                1 => "same content, and one of these is stored source text",
                _ if hand_over(core, x, y, shown).await? => "same content; handed to consolidation",
                _ => "same content; consolidation already has this pair",
            };
            core.store
                .set_link_state(x, y, LinkState::Related, Some(reason), revs)
                .await
        }
        LinkKind::Contradicts => {
            if passages == 0 {
                hand_over(core, x, y, shown).await?;
            }
            core.store
                .set_link_kind(x, y, LinkState::Related, Some(kind), None, revs)
                .await
        }
        LinkKind::Unrelated => {
            core.store
                .set_link_kind(x, y, LinkState::Unrelated, Some(kind), None, revs)
                .await
        }
        _ => {
            let first = first_is_a(d).filter(|_| kind.directional());
            core.store
                .set_link_kind(x, y, LinkState::Related, Some(kind), first, revs)
                .await
        }
    }
}

/// A decision that reached `apply` with nothing left to carry out: marked as `changed`, so it is
/// not read as an answer that was carried out.
async fn left_alone(core: &Core, d: &Logged) -> Result<Vec<String>> {
    core.store.set_decision_reason(&d.id, "changed").await?;
    Ok(vec![])
}

async fn artifact(core: &Core, id: &str) -> Result<Option<Chunk>> {
    match core.store.get_artifact(id).await {
        Ok(c) => Ok(Some(c)),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What `link_kind`'s answers do to a link.
pub struct LinkApplier;

#[async_trait]
impl Applier for LinkApplier {
    fn question_id(&self) -> &'static str {
        "link_kind"
    }

    fn label(&self) -> &'static str {
        "Related how?"
    }

    fn job(&self) -> Option<Job> {
        None
    }

    async fn buttons(&self, _: &Core, _: &Logged) -> Result<Vec<(String, String)>> {
        Ok([
            ("continues", "Next step"),
            ("example", "Example"),
            ("alternative", "Alternative"),
            ("background", "Background"),
            ("contradicts", "Contradicts"),
            ("duplicate", "Duplicate"),
            ("unrelated", "Unrelated"),
        ]
        .into_iter()
        .map(|(p, l)| (p.to_string(), l.to_string()))
        .collect())
    }

    async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for id in [Some(&d.subject_a), d.subject_b.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(c) = artifact(core, id).await? {
                out.push((c.id.clone(), crate::web::ui::row_label(&c).text));
            }
        }
        Ok(out)
    }

    async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool> {
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(false);
        };
        match core.store.get_link(&d.subject_a, b_id).await? {
            // Ruled on or dismissed since: no longer awaiting its kind.
            Some(l) if l.state != LinkState::Learning => return Ok(false),
            Some(_) => {}
            // A shown neighbour has no row until its answer makes one.
            None if d.subject_kind == SHOWN => {}
            // A searched link's row is gone.
            None => return Ok(false),
        }
        for (id, rev) in [(d.subject_a.as_str(), d.rev_a), (b_id, d.rev_b)] {
            let Some(c) = artifact(core, id).await? else {
                return Ok(false);
            };
            if !c.in_results() || Some(c.embed_rev) != rev {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn apply(
        &self,
        core: &Core,
        d: &Logged,
        pick: &str,
        by: DecidedBy,
    ) -> Result<Vec<String>> {
        let kind = LinkKind::parse(pick)
            .ok_or_else(|| Error::Validation(format!("no link kind is called {pick:?}")))?;
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(vec![]);
        };
        let shown = d.subject_kind == SHOWN;
        if core.store.get_link(&d.subject_a, b_id).await?.is_none() {
            // A searched link's row is gone.
            if !shown {
                return left_alone(core, d).await;
            }
            // A shown neighbour the model calls unrelated leaves no row. A person's answer keeps
            // one, as a searched link does: it is what asking again puts back.
            if kind == LinkKind::Unrelated && by == DecidedBy::Model {
                return Ok(vec![]);
            }
        }
        let (Some(a), Some(b)) = (
            artifact(core, &d.subject_a).await?,
            artifact(core, b_id).await?,
        ) else {
            return left_alone(core, d).await;
        };
        if shown {
            // The answer makes the row, or lifts the one that waited for the sweep. Twice
            // `show_min`: a decided relation stays visible for one half-life, then fades.
            core.store
                .record_shown_link(
                    &a.id,
                    &b.id,
                    core.associate.show_min * 2.0,
                    core.associate.half_life_days,
                )
                .await?;
        }
        let link = match core.store.get_link(&a.id, &b.id).await? {
            Some(link) if link.state == LinkState::Learning => link,
            // Settled since it was asked, by a person's dismissal or a sibling. `unchanged` keeps
            // such a decision from here; one that arrives all the same carries nothing out.
            _ => return left_alone(core, d).await,
        };
        carry_out(core, &link, &a, &b, kind, d).await?;
        Ok(vec![])
    }

    async fn reopen(&self, core: &Core, d: &Logged) -> Result<()> {
        let Some(b_id) = d.subject_b.as_deref() else {
            return Ok(());
        };
        match core.store.get_link(&d.subject_a, b_id).await? {
            // Only what this decision's own answer set is taken back, read off the revisions it
            // stamped. A link still learning is open already; one a person dismissed, or anything
            // ruled on it since, stays.
            Some(l) => {
                let own = matches!(d.outcome, Some(Outcome::Confirmed | Outcome::Corrected))
                    && d.reason.as_deref() != Some("changed")
                    && matches!(l.state, LinkState::Related | LinkState::Unrelated)
                    && (l.judged_rev_a, l.judged_rev_b) == (d.rev_a, d.rev_b);
                if !own {
                    return Ok(());
                }
                core.store
                    .set_link_kind(&d.subject_a, b_id, LinkState::Learning, None, None, None)
                    .await
            }
            // A shown neighbour whose question was never answered has no row for the sweep to
            // find: one is recorded while both notes are still in results.
            None if d.subject_kind == SHOWN => {
                for id in [d.subject_a.as_str(), b_id] {
                    if !artifact(core, id).await?.is_some_and(|c| c.in_results()) {
                        return Ok(());
                    }
                }
                core.store
                    .record_shown_link(
                        &d.subject_a,
                        b_id,
                        waiting_weight(core),
                        core.associate.half_life_days,
                    )
                    .await
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::band::Band;
    use crate::core::test_support::test_core;
    use crate::infer::decide::{Answer, Answers, Kind as QKind};
    use crate::infer::fake::RuleDecider;
    use crate::store::artifacts::NewArtifact;
    use crate::store::decision_log::State;
    use crate::store::feedback::{Door, NewCandidate, NewEvent, Verdict};
    use crate::store::links::LinkState;
    use std::sync::Arc;

    async fn seed(core: &Core, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let src = core
                .store
                .insert_corpus(&format!("raw {i}"), "web", None)
                .await
                .unwrap();
            let made = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[NewArtifact {
                        text: format!("artifact {i}"),
                        title: Some(format!("t{i}")),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    async fn record(core: &Core, query: &str, shown: &[&String], unshown: &[&String]) -> String {
        let candidates = shown
            .iter()
            .map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 1.0,
                similarity: Some(0.9),
                shown: true,
                ..Default::default()
            })
            .chain(unshown.iter().map(|id| NewCandidate {
                artifact_id: (*id).clone(),
                score: 0.1,
                similarity: Some(0.2),
                ..Default::default()
            }))
            .collect();
        core.store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.0],
                    embed_model: "fake".into(),
                    candidates,
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    async fn settle(core: &Core) {
        sqlx::query!(
            "UPDATE search_events
                SET created_at = created_at - 3600,
                    judged_at = judged_at - 3600"
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    async fn on(core: &mut Core) {
        core.learn.enabled = true;
    }

    #[tokio::test]
    async fn two_searches_showing_the_same_pair_bind_it_twice() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32 cluster", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "ntfs journal", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6, "weight was {}", l.weight);
        assert_eq!(l.queries, 2, "two different questions bound this pair");
        assert_eq!(l.state, LinkState::Learning);
    }

    #[tokio::test]
    async fn the_same_question_asked_twice_is_one_binding_query() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat32", &[&ids[0], &ids[1]], &[]).await;
        // Coalescing is off in `record`, so this is a second event: a second use of one question.
        record(&core, "  FAT32  ", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 2.0).abs() < 1e-6);
        assert_eq!(l.queries, 1);
    }

    #[tokio::test]
    async fn only_what_the_searcher_saw_fires_together() {
        // The stored pool is wider than what was shown, for evaluation's sake.
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[&ids[2]]).await;
        settle(&core).await;

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[2])
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_event_still_folding_is_left_for_the_next_sweep() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;

        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );

        settle(&core).await;
        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_event_exactly_at_the_fold_boundary_waits_one_more_sweep() {
        // `run()` reads `now()` itself; computing `created_at` from a second read races it across a
        // second boundary, so this derives `at` from the stamped value.
        let mut core = test_core().await;
        on(&mut core).await;
        core.feedback.coalesce_secs = 15;
        let ids = seed(&core, 2).await;
        let ev = record(&core, "fat", &[&ids[0], &ids[1]], &[]).await;
        let created_at: i64 =
            sqlx::query_scalar!("SELECT created_at FROM search_events WHERE id = ?", &ev)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();

        let at = created_at + core.feedback.coalesce_secs;
        replay_events(&core, at).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "an event still exactly at the fold boundary was replayed early"
        );

        replay_events(&core, at + 1).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "an event one second past the boundary was still withheld"
        );
    }

    #[test]
    fn settled_cutoff_is_the_exact_complement_of_the_fold_predicate() {
        let at = 1_000_000;
        let coalesce_secs = 15;
        let cutoff = settled_cutoff(at, coalesce_secs);

        let is_fresh = |created: i64| at - created <= coalesce_secs;
        let is_settled = |created: i64| created < cutoff;

        for created in (at - coalesce_secs - 2)..=(at - coalesce_secs + 2) {
            assert_eq!(
                is_settled(created),
                !is_fresh(created),
                "created_at {created} disagreed with the fold predicate"
            );
        }

        assert_eq!(settled_cutoff(at, 0), at);
        assert_eq!(settled_cutoff(at, -5), at);
    }

    #[tokio::test]
    async fn a_replayed_event_is_never_replayed_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        settle(&core).await;

        run(&core).await.unwrap();
        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!(
            (l.weight - 1.0).abs() < 1e-6,
            "the event was replayed twice"
        );
    }

    #[tokio::test]
    async fn a_confirmed_answer_binds_harder_and_raises_its_activation() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        core.store
            .judge_hit(&ev, &ids[0], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        // +1 for co-appearance, +2 more for the pairs containing the answer.
        let with = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        let without = core
            .store
            .get_link(&ids[1], &ids[2])
            .await
            .unwrap()
            .unwrap();
        assert!(
            (with.weight - 3.0).abs() < 1e-6,
            "weight was {}",
            with.weight
        );
        assert!((without.weight - 1.0).abs() < 1e-6);

        let act = core.store.activation_of(&ids).await.unwrap();
        assert!(
            act[&ids[0]].0 > act[&ids[1]].0,
            "the confirmed answer gained no activation"
        );
    }

    #[tokio::test]
    async fn an_answer_the_search_never_showed_raises_activation_but_binds_nothing() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        let ev = record(&core, "q", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge_hit(&ev, &ids[2], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[2], &ids[0])
                .await
                .unwrap()
                .is_none(),
            "a find bound a pair that was never shown together"
        );
        assert!(
            core.store
                .get_link(&ids[2], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "a find bound a pair that was never shown together"
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some()
        );

        let act = core.store.activation_of(&ids).await.unwrap();
        assert!(
            act[&ids[2]].0 > act[&ids[1]].0,
            "the find gained no activation"
        );
    }

    #[tokio::test]
    async fn a_gap_and_a_discard_teach_nothing_about_pairs() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        let g = record(&core, "nothing about this", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge(&g, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        settle(&core).await;

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert!((l.weight - 1.0).abs() < 1e-6, "weight was {}", l.weight);
    }

    #[tokio::test]
    async fn a_faded_link_is_forgotten_and_a_judged_one_is_not() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 4).await;
        core.store
            .bump_link(&ids[0], &ids[1], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .bump_link(&ids[2], &ids[3], 1.0, Some("q"), 30.0, 0)
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[2],
                &ids[3],
                LinkState::Related,
                Some("why"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none(),
            "a link last used at the epoch has decayed to nothing"
        );
        assert!(
            core.store
                .get_link(&ids[2], &ids[3])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_strong_cross_corpus_link_is_armed_for_the_judge_exactly_once() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        // Five, not four: four sit exactly on `judge_min`, and a second ticking before the sweep
        // decays them just under it.
        for q in ["one", "two", "three", "four", "five"] {
            core.store
                .bump_link(&ids[0], &ids[1], 1.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }

        run(&core).await.unwrap();
        let target = link_target(&ids[0], &ids[1]);
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::LinkJudge, &target)
                .await
                .unwrap()
        );

        // The guard against re-arming is `rearm_idle_seq`'s `WHERE jobs.state = 'done'`, not
        // `live_job`; this only pins the attempts.
        run(&core).await.unwrap();
        let mut seen = 0;
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::LinkJudge {
                seen += 1;
                assert_eq!(j.attempts, 1, "the unit was re-armed underneath itself");
            }
        }
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn a_judged_link_is_reopened_when_its_text_changes_under_it() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 9.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                LinkState::Unrelated,
                Some("coincidence"),
                Some((0, 0)),
            )
            .await
            .unwrap();
        core.store
            .update_artifact_text(&ids[0], "rewritten")
            .await
            .unwrap();

        run(&core).await.unwrap();

        let l = core
            .store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            l.state,
            LinkState::Learning,
            "the judge read text that is gone"
        );
    }

    #[test]
    fn a_batch_cut_inside_one_second_leaves_that_second_for_the_next_sweep() {
        assert_eq!(
            replayable(&[1, 2, 3, 3], 4),
            2,
            "a second the batch may have cut in half was replayed anyway"
        );
        // More rows stamped 4 may sit just past the limit.
        assert_eq!(replayable(&[1, 2, 3, 4], 4), 3);
        assert_eq!(replayable(&[1, 2, 3, 3], 8), 4);
        assert_eq!(replayable(&[], 8), 0);
        assert_eq!(replayable(&[7, 7, 7], 3), 3);
    }

    #[tokio::test]
    async fn the_watermark_advances_only_over_seconds_that_were_replayed_whole() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "a", &[&ids[0], &ids[1]], &[]).await;
        record(&core, "b", &[&ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        sqlx::query!("UPDATE search_events SET created_at = 1000")
            .execute(&core.store.pool)
            .await
            .unwrap();

        replay_events(&core, 100_000).await.unwrap();

        assert_eq!(
            core.store.meta_get(EVENTS_AFTER).await.unwrap().as_deref(),
            Some("1000")
        );
        assert!(
            core.store
                .get_link(&ids[1], &ids[2])
                .await
                .unwrap()
                .is_some(),
            "the second event of the second was skipped"
        );
    }

    #[test]
    fn the_watermarks_named_here_are_the_ones_migrate_seeds() {
        // `Store::migrate` seeds these keys spelled out, as `store` does not depend on `jobs`.
        assert_eq!(EVENTS_AFTER, "associate.events_after");
        assert_eq!(JUDGED_AFTER, "associate.judged_after");
    }

    /// How the decider reads a link: the kind and how sure, its reversed read's kind, which note
    /// comes first given the state, whether the direction's reversed read names the other note
    /// (always, or only when exchanged), the words where a pick goes, and a state it fails on.
    #[derive(Clone, Copy)]
    struct Says {
        kind: &'static str,
        top: f64,
        kind_twin: Option<&'static str>,
        first: fn(&str) -> &'static str,
        torn_direction: bool,
        torn_exchange: bool,
        wording: Option<&'static str>,
        down_for: Option<&'static str>,
    }

    /// Always the note labelled A: the pull the exchanged read exists to catch.
    fn the_label(_: &str) -> &'static str {
        "a_first"
    }

    /// The note titled t0 comes first, whichever label it carries.
    fn the_text(state: &str) -> &'static str {
        if state.starts_with("A: t0") {
            "a_first"
        } else {
            "b_first"
        }
    }

    fn says(kind: &'static str) -> Says {
        Says {
            kind,
            top: 0.95,
            kind_twin: None,
            first: the_text,
            torn_direction: false,
            torn_exchange: false,
            wording: None,
            down_for: None,
        }
    }

    fn asks_direction(q: &crate::infer::decide::Question) -> bool {
        matches!(&q.kind, QKind::Choice { options } if options.iter().any(|(n, _)| n == "a_first"))
    }

    fn deciding(core: &mut Core, s: Says) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(move |state, qs| {
            if s.down_for.is_some_and(|t| state.contains(t)) {
                return Err(Error::Validation("the decider is down".into()));
            }
            // The exchanged request asks the direction alone.
            let exchange = qs.iter().all(asks_direction);
            let mut answers = std::collections::HashMap::new();
            for q in qs {
                let QKind::Choice { options } = &q.kind else {
                    return Err(Error::Validation("a choice only".into()));
                };
                let direction = asks_direction(q);
                let (favoured, top) = if direction {
                    ((s.first)(state), 0.95)
                } else {
                    (s.kind, s.top)
                };
                let twin = q.key.ends_with('r');
                let said = match (direction, twin) {
                    (true, true) if s.torn_direction || (s.torn_exchange && exchange) => {
                        if favoured == "a_first" {
                            "b_first"
                        } else {
                            "a_first"
                        }
                    }
                    (false, true) => s.kind_twin.unwrap_or(favoured),
                    _ => favoured,
                };
                let rest = (1.0 - top) / (options.len() - 1) as f64;
                answers.insert(
                    q.key.clone(),
                    Answer::Choice {
                        pick: s.wording.unwrap_or(said).into(),
                        probabilities: options
                            .iter()
                            .map(|(n, _)| if n == favoured { top } else { rest })
                            .collect(),
                        confidence: top,
                    },
                );
            }
            Ok(Answers {
                answers,
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }));
        core.decider = d.clone();
        d
    }

    /// Two artifacts from different documents, bound by one search.
    async fn bound(core: &Core) -> Vec<String> {
        let ids = seed(core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        ids
    }

    /// The line a reader of `from` sees beside the other note.
    async fn line_from(core: &Core, from: &str) -> Option<String> {
        let got = core
            .store
            .links_from(
                &[from.to_string()],
                &[LinkState::Related],
                30.0,
                crate::store::now(),
                0.0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        got[0].reason.clone()
    }

    async fn link_of(core: &Core, ids: &[String]) -> crate::store::links::Link {
        core.store
            .get_link(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
    }

    async fn decision(core: &Core, ids: &[String]) -> crate::store::decision_log::Logged {
        let (a, b) = crate::store::links::canonical(&ids[0], &ids[1]);
        core.store
            .last_logged("link_kind", a, Some(b), &link_era(core))
            .await
            .unwrap()
            .expect("the link was asked")
    }

    async fn to_judge(core: &Core) -> Vec<crate::store::links::Link> {
        core.store
            .links_to_judge(&link_era(core), 4.0, 1, 30.0, crate::store::now(), 10)
            .await
            .unwrap()
    }

    #[test]
    fn each_kind_renders_a_line_for_each_side() {
        use crate::store::links::{LinkKind, link_line};
        // A is first; read from A (`from_a`), then from B.
        let both = |k| {
            (
                link_line(k, Some(true), true),
                link_line(k, Some(true), false),
            )
        };
        assert_eq!(
            both(LinkKind::Continues),
            (
                Some("The next step after this"),
                Some("The step before this")
            )
        );
        assert_eq!(
            both(LinkKind::Example),
            (
                Some("An example of this"),
                Some("What this is an example of")
            )
        );
        assert_eq!(
            both(LinkKind::Background),
            (
                Some("Uses what this explains"),
                Some("Explains what this uses")
            )
        );
        // B first: every directional kind's lines trade sides.
        for k in [LinkKind::Continues, LinkKind::Example, LinkKind::Background] {
            let (from_first, from_second) = both(k);
            assert_eq!(link_line(k, Some(false), false), from_first, "{k:?}");
            assert_eq!(link_line(k, Some(false), true), from_second, "{k:?}");
            assert_ne!(from_first, from_second, "{k:?}");
        }
        assert_eq!(
            link_line(LinkKind::Continues, Some(false), true),
            Some("The step before this")
        );
        assert_eq!(
            link_line(LinkKind::Example, Some(false), true),
            Some("What this is an example of")
        );
        assert_eq!(
            link_line(LinkKind::Background, Some(false), true),
            Some("Explains what this uses")
        );
        for side in [true, false] {
            assert_eq!(
                link_line(LinkKind::Alternative, None, side),
                Some("Another way to do this")
            );
            assert_eq!(
                link_line(LinkKind::Contradicts, None, side),
                Some("States this differently")
            );
            assert_eq!(link_line(LinkKind::Unrelated, None, side), None);
            assert_eq!(link_line(LinkKind::Duplicate, None, side), None);
            for k in [LinkKind::Continues, LinkKind::Example, LinkKind::Background] {
                assert_eq!(link_line(k, None, side), Some("Related to this"));
            }
        }
        for k in [
            LinkKind::Continues,
            LinkKind::Example,
            LinkKind::Alternative,
            LinkKind::Background,
            LinkKind::Contradicts,
            LinkKind::Duplicate,
            LinkKind::Unrelated,
        ] {
            assert_eq!(LinkKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LinkKind::parse("related"), None);
    }

    #[tokio::test]
    async fn a_sure_kind_is_stored_and_shown_as_its_line() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("example"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Example));
        let (a, _) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(l.first_is_a, Some(a == ids[0]), "t0 is the general one");
        assert_eq!(l.reason, None, "no text of the model's is stored");
        assert_eq!(l.judged_rev_a, Some(0));
        // t0 is the description, t1 the instance.
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("An example of this")
        );
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("What this is an example of")
        );
        assert_eq!(decider.calls(), 2, "the request, then the exchanged one");
        let d = decision(&core, &ids).await;
        assert_eq!((d.pick.as_str(), d.band), ("example", Band::Act));
        let answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap();
        assert!(answer.get("direction").is_some(), "{answer}");
    }

    #[tokio::test]
    async fn a_directional_kind_whose_direction_is_torn_shows_the_neutral_line() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                torn_direction: true,
                ..says("continues")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Continues));
        assert_eq!(l.first_is_a, None);
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("Related to this")
            );
        }
        assert_eq!(decider.calls(), 1, "a torn direction is not asked again");
    }

    #[tokio::test]
    async fn a_direction_that_follows_the_label_under_exchange_is_stored_undirected() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                first: the_label,
                ..says("continues")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Continues));
        assert_eq!(l.first_is_a, None, "A was named first both ways round");
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("Related to this")
            );
        }
        assert_eq!(decider.calls(), 2);
        let exchanged = &decider.requests()[1];
        assert!(exchanged.0.starts_with("A: t1"), "{}", exchanged.0);
        assert!(
            exchanged.1.iter().all(|q| q.instructions
                == crate::core::judging::catalogue::link_direction()
                    .question
                    .instructions),
            "the exchanged request asks only the direction"
        );
    }

    #[tokio::test]
    async fn a_direction_that_follows_the_text_under_exchange_is_stored_with_first() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("continues"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        let (a, _) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(l.first_is_a, Some(a == ids[0]));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("The next step after this")
        );
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("The step before this")
        );
    }

    #[tokio::test]
    async fn a_kind_with_no_direction_spends_no_exchanged_request() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(decider.calls(), 1);
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("Another way to do this")
        );
    }

    #[tokio::test]
    async fn a_queued_unit_spends_no_call_once_search_recording_is_switched_off() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(decider.calls(), 0);
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Learning);
    }

    #[test]
    fn a_link_names_itself_the_same_way_round_however_it_is_armed() {
        assert_eq!(link_target("b", "a"), link_target("a", "b"));
        assert_eq!(link_target("a", "b"), "a|b");
    }

    #[tokio::test]
    async fn a_related_verdict_names_the_relation_and_stops_the_decay() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("Another way to do this")
        );
        assert_eq!(l.judged_rev_a, Some(0));
    }

    #[tokio::test]
    async fn a_verdict_with_no_line_stores_no_reason_rather_than_an_empty_one() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.reason, None, "an empty reason was stored as Some(\"\")");
    }

    #[tokio::test]
    async fn an_unrelated_verdict_is_stored_so_it_is_not_asked_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("unrelated"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Unrelated);
        assert_eq!(l.reason, None);
        run(&core).await.unwrap();
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::LinkJudge,
                    &link_target(&ids[0], &ids[1])
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_torn_link_is_an_unsure_row_and_stays_learning() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(
            &mut core,
            Says {
                kind_twin: Some("unrelated"),
                ..says("alternative")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let d = decision(&core, &ids).await;
        assert_eq!(d.band, Band::Ask);
        assert_eq!(d.state, State::Open, "the row is seated");
        assert_eq!(d.subject_kind, "link");
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!(l.kind, None);
    }

    #[tokio::test]
    async fn a_person_picking_a_kind_sets_it_and_can_ask_again() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(
            &mut core,
            Says {
                kind_twin: Some("alternative"),
                ..says("unrelated")
            },
        );
        let ids = bound(&core).await;
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        let d = decision(&core, &ids).await;
        assert_eq!(d.state, State::Open);
        let applier = core.appliers.get("link_kind").expect("registered");
        assert_eq!(applier.label(), "Related how?");
        assert!(applier.job().is_none());

        crate::core::judging::answer(&core, &d.id, Some("alternative"))
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Alternative));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("Another way to do this")
        );
        assert!(to_judge(&core).await.is_empty());

        crate::core::judging::ask_again(&core, &d.id).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Learning);
        assert_eq!((l.kind, l.first_is_a), (None, None));
        assert_eq!(to_judge(&core).await.len(), 1, "asked again means asked");
    }

    #[tokio::test]
    async fn a_contradiction_between_linked_notes_is_handed_to_consolidation() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("contradicts"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&ids[0], &ids[1])
                .await
                .unwrap(),
            Some(crate::store::pairs::PairState::Pending),
            "consolidation was never told"
        );
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(crate::store::links::LinkKind::Contradicts));
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("States this differently")
            );
        }
    }

    #[tokio::test]
    async fn a_disguised_duplicate_is_handed_to_consolidation_and_still_shown() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let pair = core
            .store
            .pair_state_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .expect("consolidation was never told");
        assert_eq!(pair, crate::store::pairs::PairState::Pending);
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert!(l.reason.as_deref().unwrap().contains("consolidation"));
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("same content; handed to consolidation")
        );
    }

    async fn seed_passages(core: &Core, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            let src = core
                .store
                .insert_corpus(&format!("skript {i}"), "web", None)
                .await
                .unwrap();
            let made = core
                .store
                .insert_artifacts_with_provenance(
                    &src.id,
                    &[NewArtifact {
                        text: format!("Spuren sind materielle Veraenderungen {i}"),
                        title: Some(format!("t{i}")),
                        segment_idx: Some(0),
                        ..Default::default()
                    }],
                    crate::store::artifacts::Provenance::Passage,
                )
                .await
                .unwrap();
            ids.push(made[0].id.clone());
        }
        ids
    }

    #[tokio::test]
    async fn a_duplicate_verdict_over_passages_is_not_handed_to_consolidation() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let ids = seed_passages(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&ids[0], &ids[1])
                .await
                .unwrap(),
            None,
            "a passage is substrate on this path too"
        );
        let l = link_of(&core, &ids).await;
        assert_eq!(
            l.state,
            LinkState::Related,
            "the reader should still see the connection"
        );
        assert_eq!(
            l.reason.as_deref(),
            Some("same content, and both are stored source text"),
            "two passages are both source text, and the line may say so"
        );
    }

    #[tokio::test]
    async fn a_contradiction_over_passages_is_shown_and_not_handed_over() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("contradicts"));
        let ids = seed_passages(&core, 2).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&ids[0], &ids[1])
                .await
                .unwrap(),
            None
        );
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Related);
    }

    #[tokio::test]
    async fn a_duplicate_over_one_passage_does_not_call_the_other_one_source_text() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let passage = seed_passages(&core, 1).await.remove(0);
        let captured = seed(&core, 1).await.remove(0);
        core.store
            .bump_link(
                &passage,
                &captured,
                5.0,
                Some("q"),
                30.0,
                crate::store::now(),
            )
            .await
            .unwrap();

        judge(&core, &link_target(&passage, &captured))
            .await
            .unwrap();

        assert_eq!(
            core.store
                .pair_state_between(&passage, &captured)
                .await
                .unwrap(),
            None,
            "the handover is refused whichever side the passage is on"
        );
        assert_eq!(
            core.store
                .get_link(&passage, &captured)
                .await
                .unwrap()
                .unwrap()
                .reason
                .as_deref(),
            Some("same content, and one of these is stored source text"),
            "the line claimed something untrue about the captured artifact"
        );
    }

    #[tokio::test]
    async fn a_link_that_has_already_been_answered_costs_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.store
            .set_link_state(&ids[0], &ids[1], LinkState::Dismissed, None, None)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(link_of(&core, &ids).await.state, LinkState::Dismissed);
        assert_eq!(decider.calls(), 0);
    }

    #[tokio::test]
    async fn a_unit_already_queued_when_the_feature_is_switched_off_spends_no_call() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;
        core.learn.enabled = false;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(
            decider.calls(),
            0,
            "a call was spent after the feature was off"
        );
        assert_eq!(
            link_of(&core, &ids).await.state,
            LinkState::Learning,
            "an unjudged link stays visible as learning"
        );
    }

    #[tokio::test]
    async fn a_link_is_asked_once_per_era_and_again_when_a_text_changes() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                kind_twin: Some("unrelated"),
                ..says("alternative")
            },
        );
        let ids = bound(&core).await;
        assert_eq!(to_judge(&core).await.len(), 1);

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Learning);
        assert!(to_judge(&core).await.is_empty(), "asked in this era");
        // A second unit for the same link asks nothing.
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(decider.calls(), 1);

        core.store
            .update_artifact_text(&ids[0], "rewritten")
            .await
            .unwrap();
        assert_eq!(to_judge(&core).await.len(), 1, "the text it read is gone");
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        assert_eq!(decider.calls(), 2);
    }

    #[tokio::test]
    async fn a_link_judged_before_this_change_keeps_its_written_reason() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = bound(&core).await;
        core.store
            .set_link_state(
                &ids[0],
                &ids[1],
                LinkState::Related,
                Some("the tool and the error it prints"),
                Some((0, 0)),
            )
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.kind, None);
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("the tool and the error it prints")
            );
        }
    }

    #[tokio::test]
    async fn the_searches_that_bound_them_are_in_the_state() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let (state, questions) = &decider.requests()[0];
        assert!(
            state.contains("Found together by these searches: q"),
            "{state}"
        );
        // Both questions, each with its reversed twin, in the one request.
        assert_eq!(questions.len(), 4);
    }

    #[tokio::test]
    async fn links_are_armed_inside_the_judging_budget() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 2).await;
        for q in ["one", "two", "three", "four", "five"] {
            core.store
                .bump_link(&ids[0], &ids[1], 1.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }
        core.decide_cfg.max_per_tick = 0;

        let r = run(&core).await.unwrap();

        assert_eq!(r.armed, 0);
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::LinkJudge,
                    &link_target(&ids[0], &ids[1])
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_sweep_does_nothing_at_all_while_nothing_is_recorded() {
        let core = test_core().await; // feedback off, associate on
        let ids = seed(&core, 2).await;
        run(&core).await.unwrap();
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(core.store.meta_get(EVENTS_AFTER).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_verdict_recorded_in_the_second_the_sweep_ran_is_not_skipped() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;

        let first = record(&core, "one", &[&ids[0], &ids[1]], &[]).await;
        core.store
            .judge_hit(&first, &ids[0], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET judged_at = 1000 WHERE id = ?",
            &first
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        replay_verdicts(&core, 1000).await.unwrap();

        let second = record(&core, "two", &[&ids[0], &ids[2]], &[]).await;
        core.store
            .judge_hit(&second, &ids[2], crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET judged_at = 1000 WHERE id = ?",
            &second
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        replay_verdicts(&core, 1001).await.unwrap();

        assert!(
            core.store
                .get_link(&ids[0], &ids[2])
                .await
                .unwrap()
                .is_some(),
            "the verdict recorded inside the sweep's own second was lost"
        );
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "the verdict the sweep did see was lost too"
        );
    }

    #[tokio::test]
    async fn a_duplicate_verdict_claims_no_handoff_when_the_pair_is_already_settled() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let ids = bound(&core).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        sqlx::query!("UPDATE artifact_pairs SET state = 'dismissed'")
            .execute(&core.store.pool)
            .await
            .unwrap();

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert!(
            !l.reason
                .as_deref()
                .unwrap_or_default()
                .contains("handed to consolidation"),
            "the link claims a handoff that never happened: {:?}",
            l.reason
        );
    }

    #[tokio::test]
    async fn one_unbindable_pair_does_not_cost_an_event_its_other_pairs() {
        let mut core = test_core().await;
        on(&mut core).await;
        let ids = seed(&core, 3).await;
        record(&core, "three shown", &[&ids[0], &ids[1], &ids[2]], &[]).await;
        settle(&core).await;
        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &ids[2])
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        // The premise: the pairs naming the deleted side really did fail.
        for gone in [&ids[0], &ids[1]] {
            assert!(
                core.store.get_link(gone, &ids[2]).await.unwrap().is_none(),
                "a link was written against a deleted artifact"
            );
        }
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_some(),
            "a pair that could not be bound cost the event its other pairs"
        );
    }

    /// The two notes in the order every link question names them.
    async fn in_order(core: &Core, x: &str, y: &str) -> (Chunk, Chunk) {
        let (a, b) = crate::store::links::canonical(x, y);
        (
            core.store.get_artifact(a).await.unwrap(),
            core.store.get_artifact(b).await.unwrap(),
        )
    }

    async fn pair_of(core: &Core, ids: &[String]) -> crate::store::pairs::ArtifactPair {
        core.store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .expect("consolidation was never told")
    }

    /// The line of a state that names the searches, if it kept any.
    fn searches(state: &str) -> Option<&str> {
        state
            .lines()
            .find(|l| l.starts_with("Found together by these searches:"))
    }

    /// A decider torn on the kind, so the question becomes an Unsure row.
    fn torn(kind: &'static str) -> Says {
        Says {
            kind_twin: Some(if kind == "unrelated" {
                "alternative"
            } else {
                "unrelated"
            }),
            ..says(kind)
        }
    }

    #[tokio::test]
    async fn a_torn_exchanged_read_confirms_no_direction() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(
            &mut core,
            Says {
                torn_exchange: true,
                ..says("continues")
            },
        );
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        assert_eq!(decider.calls(), 2, "the exchanged read was asked");
        let d = decision(&core, &ids).await;
        let answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap();
        // The exchanged read still names the other note on average; its two halves disagree.
        let read = &answer["direction"];
        assert_eq!(read["agree"], true, "{answer}");
        assert_ne!(read["pick"], read["exchanged"], "{answer}");
        assert_eq!(read["exchanged_agree"], false, "{answer}");
        let l = link_of(&core, &ids).await;
        assert_eq!(l.kind, Some(LinkKind::Continues));
        assert_eq!(
            l.first_is_a, None,
            "a torn exchanged read confirmed a direction"
        );
        for id in &ids {
            assert_eq!(
                line_from(&core, id).await.as_deref(),
                Some("Related to this")
            );
        }
    }

    #[tokio::test]
    async fn an_exchanged_read_that_held_says_so_beside_its_pick() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("continues"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let d = decision(&core, &ids).await;
        let answer: serde_json::Value = serde_json::from_str(&d.answer).unwrap();
        assert_eq!(answer["direction"]["exchanged_agree"], true, "{answer}");
        assert!(link_of(&core, &ids).await.first_is_a.is_some());
    }

    #[tokio::test]
    async fn the_exchanged_read_sees_the_searches_the_first_read_saw() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("continues"));
        let ids = seed(&core, 2).await;
        for q in [
            "mount a loop device read only",
            "carve deleted files from a raw image",
            "list the partitions of a disk image",
        ] {
            core.store
                .bump_link(&ids[0], &ids[1], 2.0, Some(q), 30.0, crate::store::now())
                .await
                .unwrap();
        }
        let (a, b) = in_order(&core, &ids[0], &ids[1]).await;
        let cues: Vec<String> = link_of(&core, &ids)
            .await
            .cues
            .iter()
            .map(|c| c.q.clone())
            .collect();
        let (kind, direction) = (catalogue::link_kind(), catalogue::link_direction());
        // A window in which the direction alone leaves room for more searches than both questions.
        let tight = (64..8192)
            .find(|n| {
                decider.set_context_tokens(*n);
                let both = state::link(
                    &note(&a),
                    &note(&b),
                    &cues,
                    &core.counter,
                    state::room(&core, &[&kind, &direction]),
                );
                let alone = state::link(
                    &note(&b),
                    &note(&a),
                    &cues,
                    &core.counter,
                    state::room(&core, &[&direction]),
                );
                matches!((both, alone), (Some(x), Some(y)) if searches(&x) != searches(&y))
            })
            .expect("a window that tells the two rooms apart");
        decider.set_context_tokens(tight);

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let requests = decider.requests();
        assert_eq!(requests.len(), 2, "the request, then the exchanged one");
        assert_eq!(
            searches(&requests[0].0),
            searches(&requests[1].0),
            "the two reads were shown different searches"
        );
    }

    #[tokio::test]
    async fn a_person_confirming_a_directional_kind_stores_the_direction_the_reads_found() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, torn("continues"));
        let ids = bound(&core).await;
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        let d = decision(&core, &ids).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert_eq!(decider.calls(), 2, "an asked direction is exchanged too");
        let l = link_of(&core, &ids).await;
        assert_eq!((l.state, l.first_is_a), (LinkState::Learning, None));

        crate::core::judging::answer(&core, &d.id, Some("continues"))
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(LinkKind::Continues));
        let (a, _) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(
            l.first_is_a,
            Some(a == ids[0]),
            "t0 comes first, as the reads stored it"
        );
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("The next step after this")
        );
        assert_eq!(
            line_from(&core, &ids[1]).await.as_deref(),
            Some("The step before this")
        );
        assert_eq!(decider.calls(), 2, "a person's answer asks nothing");
    }

    #[tokio::test]
    async fn a_waiting_act_applied_by_the_sweep_takes_its_direction_from_the_stored_reads() {
        let mut core = test_core().await;
        on(&mut core).await;
        let decider = deciding(&mut core, says("example"));
        let ids = bound(&core).await;
        let (a, b) = in_order(&core, &ids[0], &ids[1]).await;
        // Asked and read, and the stage went no further: a settled act the sweep finds waiting.
        let d = read_link(&core, "link", &a, &b, &[]).await.unwrap();
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
        core.store.mark_act_settled(&d.id).await.unwrap();
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Learning);
        let calls = decider.calls();

        crate::jobs::decisions::run(&core).await.unwrap();

        assert_eq!(decider.calls(), calls, "the sweep asks nothing again");
        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Related);
        assert_eq!(l.kind, Some(LinkKind::Example));
        assert_eq!(
            l.first_is_a,
            Some(a.id == ids[0]),
            "t0 is the general one, as the reads stored it"
        );
        assert_eq!(
            line_from(&core, &ids[0]).await.as_deref(),
            Some("An example of this")
        );
        assert_eq!(decision(&core, &ids).await.state, State::Done);
    }

    #[tokio::test]
    async fn nothing_the_decision_model_wrote_is_stored_for_a_link() {
        const WRITTEN: &str = "both notes walk through the same procedure";
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(
            &mut core,
            Says {
                wording: Some(WRITTEN),
                ..says("continues")
            },
        );
        // A link found by searches, and a neighbour shown at capture.
        let searched = bound(&core).await;
        judge(&core, &link_target(&searched[0], &searched[1]))
            .await
            .unwrap();
        let shown = seed(&core, 2).await;
        assert_eq!(link_shown(&core, &shown[0], &shown[1..]).await.unwrap(), 1);

        for ids in [&searched, &shown] {
            let l = link_of(&core, ids).await;
            assert_eq!(
                l.kind,
                Some(LinkKind::Continues),
                "the answer was carried out"
            );
            assert_eq!(l.reason, None);
            let d = decision(&core, ids).await;
            assert_eq!(d.pick, "continues", "the pick is the option's name");
            for stored in [format!("{l:?}"), format!("{d:?}")] {
                assert!(
                    !stored.contains(WRITTEN),
                    "the model's words were stored: {stored}"
                );
            }
        }
    }

    #[tokio::test]
    async fn link_shown_asks_while_the_tick_has_calls_and_leaves_the_rest_to_the_sweep() {
        let mut core = test_core().await;
        let decider = deciding(&mut core, says("alternative"));
        let ids = seed(&core, 3).await;
        core.decide_cfg.max_per_tick = 1;

        let made = link_shown(&core, &ids[0], &ids[1..]).await.unwrap();

        assert_eq!(made, 1);
        assert_eq!(decider.calls(), 1, "the tick had one call left");
        let asked = link_of(&core, &ids).await;
        assert_eq!(
            (asked.state, asked.kind),
            (LinkState::Related, Some(LinkKind::Alternative))
        );
        let rest = [ids[0].clone(), ids[2].clone()];
        let waiting = core
            .store
            .get_link(&rest[0], &rest[1])
            .await
            .unwrap()
            .expect("the neighbour past the budget was recorded for the sweep");
        assert_eq!(waiting.state, LinkState::Learning);
        assert!(waiting.shown);
        assert!(
            waiting.weight < core.associate.show_min,
            "nothing shows it before an answer"
        );
        assert!(waiting.weight > core.associate.prune_below);

        // Once the tick has room the sweep arms it, and it is asked as the shown neighbour it is.
        on(&mut core).await;
        core.decide_cfg.max_per_tick = 60;
        let r = run(&core).await.unwrap();
        assert_eq!(r.armed, 1);
        judge(&core, &link_target(&rest[0], &rest[1]))
            .await
            .unwrap();
        assert_eq!(decider.calls(), 2);
        assert_eq!(decision(&core, &rest).await.subject_kind, SHOWN);
        let l = link_of(&core, &rest).await;
        assert_eq!(
            (l.state, l.kind),
            (LinkState::Related, Some(LinkKind::Alternative))
        );
        assert!(
            l.weight >= core.associate.show_min * 2.0,
            "shown above the floor: {}",
            l.weight
        );
        assert_eq!(run(&core).await.unwrap().armed, 0, "asked once");
    }

    #[tokio::test]
    async fn a_shown_neighbour_that_cannot_be_asked_about_does_not_cost_the_next_its_question() {
        let mut core = test_core().await;
        deciding(
            &mut core,
            Says {
                down_for: Some("t1"),
                ..says("alternative")
            },
        );
        let ids = seed(&core, 3).await;

        let made = link_shown(&core, &ids[0], &ids[1..]).await.unwrap();

        assert_eq!(made, 1, "the neighbour after the failed one was not asked");
        let l = link_of(&core, &[ids[0].clone(), ids[2].clone()]).await;
        assert_eq!(l.state, LinkState::Related);
        let failed = link_of(&core, &ids).await;
        assert_eq!(failed.state, LinkState::Learning);
        assert!(failed.shown, "the failed question is left for the sweep");
    }

    #[tokio::test]
    async fn a_shown_neighbour_called_a_duplicate_or_a_contradiction_is_handed_over_restricted() {
        for kind in ["duplicate", "contradicts"] {
            let mut core = test_core().await;
            deciding(&mut core, says(kind));
            let ids = seed(&core, 2).await;

            assert_eq!(link_shown(&core, &ids[0], &ids[1..]).await.unwrap(), 1);

            let pair = pair_of(&core, &ids).await;
            assert_eq!(pair.state, crate::store::pairs::PairState::Pending);
            assert_eq!(pair.net, Some(Net::Shown), "{kind}");
            assert!(
                crate::jobs::dedupe::restricted(&pair, 0.0),
                "a neighbour merely shown at capture could merge without a person"
            );
        }
    }

    #[tokio::test]
    async fn a_searched_link_called_a_duplicate_is_handed_over_unrestricted() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let ids = bound(&core).await;

        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();

        let pair = pair_of(&core, &ids).await;
        assert_eq!(pair.net, None);
        assert!(!crate::jobs::dedupe::restricted(&pair, 1.0));
    }

    #[tokio::test]
    async fn a_shown_neighbour_answered_later_is_still_handed_over_restricted() {
        let mut core = test_core().await;
        deciding(&mut core, torn("duplicate"));
        let ids = seed(&core, 2).await;
        assert_eq!(link_shown(&core, &ids[0], &ids[1..]).await.unwrap(), 0);
        let d = decision(&core, &ids).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert_eq!(d.subject_kind, SHOWN);
        assert!(
            core.store
                .pair_between(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );

        crate::core::judging::answer(&core, &d.id, Some("duplicate"))
            .await
            .unwrap();

        let pair = pair_of(&core, &ids).await;
        assert_eq!(pair.net, Some(Net::Shown), "the source was lost on the way");
        assert!(crate::jobs::dedupe::restricted(&pair, 0.0));
    }

    #[tokio::test]
    async fn a_pair_consolidation_already_has_keeps_the_net_it_was_filed_in() {
        let mut core = test_core().await;
        deciding(&mut core, says("duplicate"));
        let ids = seed(&core, 2).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();

        assert_eq!(link_shown(&core, &ids[0], &ids[1..]).await.unwrap(), 1);

        assert_eq!(
            pair_of(&core, &ids).await.net,
            None,
            "a pair already pending was newly restricted"
        );
        assert_eq!(
            link_of(&core, &ids).await.reason.as_deref(),
            Some("same content; consolidation already has this pair")
        );
    }

    #[tokio::test]
    async fn a_shown_duplicate_is_stored_as_a_searched_one_is() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, says("duplicate"));
        let searched = bound(&core).await;
        judge(&core, &link_target(&searched[0], &searched[1]))
            .await
            .unwrap();
        let shown = seed(&core, 2).await;
        assert_eq!(link_shown(&core, &shown[0], &shown[1..]).await.unwrap(), 1);

        let (s, c) = (
            link_of(&core, &searched).await,
            link_of(&core, &shown).await,
        );
        assert_eq!(
            (c.state, c.kind, c.first_is_a, c.reason.as_deref()),
            (s.state, s.kind, s.first_is_a, s.reason.as_deref())
        );
        assert_eq!(c.kind, None, "the hand-over line says it; no kind is kept");
        assert_eq!(
            c.reason.as_deref(),
            Some("same content; handed to consolidation")
        );
        assert_eq!(
            line_from(&core, &shown[0]).await.as_deref(),
            Some("same content; handed to consolidation")
        );
    }

    #[tokio::test]
    async fn a_persons_unrelated_on_a_shown_neighbour_keeps_a_row_to_ask_again_from() {
        let mut core = test_core().await;
        deciding(&mut core, torn("alternative"));
        let ids = seed(&core, 2).await;
        assert_eq!(link_shown(&core, &ids[0], &ids[1..]).await.unwrap(), 0);
        let d = decision(&core, &ids).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );

        crate::core::judging::answer(&core, &d.id, Some("unrelated"))
            .await
            .unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(
            (l.state, l.kind),
            (LinkState::Unrelated, Some(LinkKind::Unrelated)),
            "stored as a searched link's unrelated is"
        );

        crate::core::judging::ask_again(&core, &d.id).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!((l.state, l.kind), (LinkState::Learning, None));
        assert_eq!(to_judge(&core).await.len(), 1, "asked again means asked");
    }

    #[tokio::test]
    async fn a_link_ruled_on_or_dismissed_since_is_no_longer_what_was_asked_about() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, torn("alternative"));
        let ids = bound(&core).await;
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        let d = decision(&core, &ids).await;
        assert_eq!(d.state, State::Open);
        assert!(LinkApplier.unchanged(&core, &d).await.unwrap());

        // A sibling ruled on it, reading the same texts.
        core.store
            .set_link_kind(
                &ids[0],
                &ids[1],
                LinkState::Related,
                Some(LinkKind::Example),
                None,
                d.rev_a.zip(d.rev_b),
            )
            .await
            .unwrap();
        assert!(
            !LinkApplier.unchanged(&core, &d).await.unwrap(),
            "a link ruled on since was still awaiting its kind"
        );
        LinkApplier.reopen(&core, &d).await.unwrap();
        let l = link_of(&core, &ids).await;
        assert_eq!(
            (l.state, l.kind),
            (LinkState::Related, Some(LinkKind::Example)),
            "a ruling made since was put back"
        );

        core.store.dismiss_link(&ids[0], &ids[1]).await.unwrap();
        assert!(!LinkApplier.unchanged(&core, &d).await.unwrap());
        LinkApplier.reopen(&core, &d).await.unwrap();
        assert_eq!(link_of(&core, &ids).await.state, LinkState::Dismissed);
    }

    #[tokio::test]
    async fn an_answer_reaching_a_link_dismissed_since_carries_nothing_out_and_says_so() {
        let mut core = test_core().await;
        on(&mut core).await;
        deciding(&mut core, torn("alternative"));
        let ids = bound(&core).await;
        judge(&core, &link_target(&ids[0], &ids[1])).await.unwrap();
        let d = decision(&core, &ids).await;
        core.store.dismiss_link(&ids[0], &ids[1]).await.unwrap();

        let rows = LinkApplier
            .apply(&core, &d, "alternative", DecidedBy::Operator)
            .await
            .unwrap();

        assert!(rows.is_empty());
        let l = link_of(&core, &ids).await;
        assert_eq!((l.state, l.kind), (LinkState::Dismissed, None));
        assert_eq!(
            decision(&core, &ids).await.reason.as_deref(),
            Some("changed"),
            "it reads as an answer that was carried out"
        );
    }

    #[tokio::test]
    async fn a_shown_neighbour_whose_question_is_put_back_unanswered_waits_for_the_sweep() {
        let mut core = test_core().await;
        deciding(&mut core, torn("alternative"));
        let ids = seed(&core, 2).await;
        assert_eq!(link_shown(&core, &ids[0], &ids[1..]).await.unwrap(), 0);
        let d = decision(&core, &ids).await;
        assert!(
            core.store
                .get_link(&ids[0], &ids[1])
                .await
                .unwrap()
                .is_none()
        );

        LinkApplier.reopen(&core, &d).await.unwrap();

        let l = link_of(&core, &ids).await;
        assert_eq!(l.state, LinkState::Learning);
        assert!(l.shown, "the sweep has no row to find it by");
    }
}
