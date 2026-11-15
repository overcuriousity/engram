//! `--replay-decisions`: the built-in questions over what the base already decided. Reads only:
//! the decision model is called directly, nothing is logged, and no pair, link or action moves.

use crate::core::Core;
use crate::core::judging::Asked;
use crate::core::judging::band::{Band, Thresholds, band, pick_and_top};
use crate::core::judging::catalogue;
use crate::core::judging::state::{self, Note};
use crate::error::{Error, Result};
use crate::infer::decide::{Kind as QKind, decide_swapped};
use crate::infer::retry::transiently;
use crate::jobs::consolidate::NEAR_IDENTICAL;
use crate::store::actions::{Action, Kind, UndoneBy};
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::links::{Link, LinkState};
use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PersonSaid {
    Agreed,
    Disagreed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub subjects: (String, Option<String>),
    /// `None`: the subjects alone do not fit.
    pub state: Option<String>,
    /// The past verdict as the question's picks; several, `|`-joined, where an old verdict was coarser.
    pub past: Option<String>,
    pub person: Option<PersonSaid>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pick: String,
    pub top: f64,
    pub swapped_agree: Option<bool>,
    pub band: Band,
    pub past: Option<String>,
    pub person: Option<PersonSaid>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QuestionReport {
    pub id: String,
    pub version: u32,
    pub asked: usize,
    pub skipped_too_long: usize,
    pub skipped_no_text: usize,
    /// Cases a read failed for, or the endpoint refused: said in the log, not counted as asked.
    pub skipped_failed: usize,
    pub left_out: usize,
    pub agrees_with_past: Option<(usize, usize)>,
    pub agrees_with_person: Option<(usize, usize)>,
    /// Act, ask, leave at `bands_at`.
    pub bands: [usize; 3],
    /// Bin floor (0.95 … 0.50, then 0.0 for tops under 0.50), cases, of which agreeing with the past.
    pub by_top: Vec<(f64, usize, usize)>,
    pub swap_disagreed: usize,
    /// With no past verdict: the 10 highest and 10 lowest (P(yes) for a noul), with a title.
    pub extremes: Vec<(f64, String)>,
    pub suggested: Option<Thresholds>,
    /// The thresholds in config the bands were counted at.
    pub bands_at: Thresholds,
    /// `None` for what the base decided; otherwise which unjudged cases these are.
    pub scope: Option<String>,
    /// Unjudged choices: the five highest tops per pick, in option order. Titles only.
    pub confident: Vec<Confident>,
    /// How often each answer was given, most first (a noul: yes, then no).
    pub picks: Vec<(String, usize)>,
    /// Per past verdict (`related` for the old link verdict), the answers given for it.
    pub past_picks: Vec<(String, Vec<(String, usize)>)>,
    /// A reading taken at a fixed line: that line, and how many said yes at or above it.
    pub reading: Option<(f64, usize)>,
    /// `link_direction`: of the cases asked again with the notes exchanged, how many flipped their pick.
    pub exchanged_holds: Option<(usize, usize)>,
    /// Judged choices: the ten most confident answers that disagree with the past verdict.
    pub disagreements: Vec<Miss>,
    /// Why the run ended at this question: the endpoint stopped answering, and what is here is
    /// what was asked until then.
    pub stopped: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Miss {
    pub top: f64,
    pub past: String,
    pub pick: String,
    pub title: String,
}

/// Readings are taken, not banded; these are their fixed lines (`category` takes its top pick).
fn fixed_line(id: &str) -> Option<f64> {
    match id {
        "pinned" => Some(0.90),
        "time_bound" | "self_contained" => Some(0.5),
        "asks_reminder" | "has_date" => Some(0.3),
        _ => None,
    }
}

fn is_reading(id: &str) -> bool {
    id == "category" || fixed_line(id).is_some()
}

fn yes_of(r: &Row) -> f64 {
    if r.pick == "yes" { r.top } else { 1.0 - r.top }
}

/// Counts most first, then by name; none at zero.
fn tally<'a>(picks: impl Iterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for p in picks {
        *n.entry(p).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = n.into_iter().map(|(k, c)| (k.to_string(), c)).collect();
    v.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    v
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Confident {
    pub pick: String,
    pub top: f64,
    pub title: String,
    /// The pair's cosine, where the case is a pair the vector store found.
    pub similarity: Option<f32>,
}

/// Old link verdict `related`: any of the new kinds that show a link.
const RELATED: &str = "continues|example|alternative|background";

/// Pair details that close a pair without the judge's verdict standing: refusals and guards. Read
/// only for a pair no decision was logged for: its row has no column that tells the two apart.
const NOT_A_VERDICT: [&str; 4] = [
    "stay as they are",
    "esolve by hand",
    "already out of results",
    "would have dropped one of them",
];

pub fn unknown_question(id: &str) -> String {
    let ids: Vec<&str> = catalogue::all().iter().map(|a| a.id).collect();
    format!(
        "There is no question \"{id}\"; --question takes one of: {}.",
        ids.join(", ")
    )
}

/// One note as shown.
struct Text {
    title: Option<String>,
    day: Option<String>,
    text: String,
}

impl Text {
    fn note(&self) -> Note<'_> {
        Note {
            title: self.title.as_deref(),
            day: self.day.clone(),
            text: &self.text,
        }
    }
}

fn day(at: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

/// From the graveyard once buried; `None` when there is no text anywhere.
async fn text_of(core: &Core, id: &str) -> Result<Option<Text>> {
    let c = match core.store.get_artifact(id).await {
        Ok(c) => c,
        Err(Error::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = c.text;
    if text.trim().is_empty()
        && let Some((grave, _, _)) = core.store.graveyard_row(id).await?
    {
        text = grave;
    }
    Ok((!text.trim().is_empty()).then(|| Text {
        title: c.title,
        day: day(c.created_at),
        text,
    }))
}

/// What a run reads once, whichever questions and cases need it.
#[derive(Default)]
struct Base {
    journal: tokio::sync::OnceCell<Vec<Action>>,
    /// The graveyard's vectors by note, read when the first buried note needs its neighbours.
    graves: tokio::sync::OnceCell<HashMap<String, Vec<f32>>>,
}

impl Base {
    async fn journal(&self, core: &Core) -> Result<&[Action]> {
        let rows = self
            .journal
            .get_or_try_init(|| core.store.recent_actions(i64::MAX as usize))
            .await?;
        Ok(rows.as_slice())
    }

    async fn grave_vector(&self, core: &Core, id: &str) -> Result<Option<&[f32]>> {
        let all = self
            .graves
            .get_or_try_init(|| async {
                let model = core.embedder.model().to_string();
                let all: HashMap<String, Vec<f32>> = core
                    .store
                    .graveyard_vectors(&model)
                    .await?
                    .into_iter()
                    .collect();
                Ok::<_, Error>(all)
            })
            .await?;
        Ok(all.get(id).map(Vec::as_slice))
    }
}

/// One case's read. A failure is said in the log and counted, and the run goes on: one case must
/// not cost the report of every other.
fn or_skip<T>(read: Result<T>, question: &str, case: &str, failed: &mut usize) -> Option<T> {
    match read {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(question, case, error = %e, "could not read a case; it is skipped");
            *failed += 1;
            None
        }
    }
}

/// A past verdict found in the base, before its text is read.
struct Found {
    at: i64,
    a: String,
    b: Option<String>,
    past: Option<String>,
    person: Option<PersonSaid>,
    /// A link's cues, or the search for `answers_query`.
    words: Vec<String>,
    /// For the rewrite checks, read already: the originals, then the rewrite.
    texts: Vec<Option<Text>>,
    /// Kept out of the neighbours: a rescue's own rewrite.
    not: Option<String>,
    /// The cosine of an unjudged pair.
    sim: Option<f32>,
}

impl Found {
    fn new(at: i64, a: &str, b: Option<&str>, past: Option<&str>) -> Found {
        Found {
            at,
            a: a.to_string(),
            b: b.map(str::to_string),
            past: past.map(str::to_string),
            person: None,
            words: vec![],
            texts: vec![],
            not: None,
            sim: None,
        }
    }
}

/// A person took the row back: they disagreed. A person's own press: they agreed.
fn person_of(rows: &[&Action], by_operator: bool) -> Option<PersonSaid> {
    if rows.iter().any(|r| r.undone_by == Some(UndoneBy::Operator)) {
        Some(PersonSaid::Disagreed)
    } else if by_operator {
        Some(PersonSaid::Agreed)
    } else {
        None
    }
}

fn evidence(r: &Action) -> serde_json::Value {
    serde_json::from_str(&r.evidence_json).unwrap_or_default()
}

/// What the last decision about a pair carried out: the pick, and whether a person chose it.
type Carried = (String, bool);

/// Per pair id, what its last `pair_relation` decision carried out. A decision that only asked,
/// was left, or was closed with a reason carried out nothing and is not here.
async fn pair_decisions(core: &Core) -> Result<HashMap<i64, Carried>> {
    let rows = sqlx::query!(
        r#"SELECT subject_a AS "pair!", pick AS "pick!", band AS "band!", state AS "state!",
                  reason AS "reason?", answered_pick AS "answered?"
             FROM decisions
            WHERE question_id = 'pair_relation' AND subject_kind = 'pair'
            ORDER BY decided_at, rowid"#
    )
    .fetch_all(&core.store.pool)
    .await?;
    let mut out = HashMap::new();
    for r in rows {
        let Ok(id) = r.pair.parse::<i64>() else {
            continue;
        };
        let acted = r.band == "act" && r.state == "done" && r.reason.is_none();
        match r.answered {
            Some(pick) => out.insert(id, (pick, true)),
            None if acted => out.insert(id, (r.pick, false)),
            None => None,
        };
    }
    Ok(out)
}

/// The plan's mapping of a settled pair onto `pair_relation`; `None` leaves it out.
fn pair_past(
    p: &ArtifactPair,
    rows: &[&Action],
    carried: Option<&Carried>,
) -> Option<&'static str> {
    // The base took it back, or consolidate closed it on similarity alone: no judgement either way.
    // Consolidate's close is known by the constant it writes; the row has no column for it.
    if p.decided_by == Some(DecidedBy::Evidence)
        || p.detail
            .as_deref()
            .is_some_and(|d| d.starts_with(NEAR_IDENTICAL))
    {
        return None;
    }
    let of = |k: Kind| rows.iter().filter(move |r| r.kind == k);
    if p.merged_into.is_some() || of(Kind::Merge).next().is_some() {
        return Some("duplicate");
    }
    if let Some(r) = of(Kind::Supersede).next() {
        return match r.subject_id.as_str() {
            s if s == p.a_id => Some("b_replaces_a"),
            s if s == p.b_id => Some("a_replaces_b"),
            _ => None,
        };
    }
    let picked = carried.map(|c| c.0.as_str());
    if !rows.is_empty() {
        // One row where the other side was retired already: the decision says it was both.
        let gone: HashSet<&str> = of(Kind::Discard).map(|r| r.subject_id.as_str()).collect();
        return (picked == Some("vacuous")
            || (gone.contains(p.a_id.as_str()) && gone.contains(p.b_id.as_str())))
        .then_some("vacuous");
    }
    match p.state {
        PairState::Contradiction => Some("conflict"),
        PairState::NoConflict => match picked {
            // Both stayed after another pick: a guard refused it, and that verdict did not stand.
            Some(pick) => (pick == "distinct").then_some("distinct"),
            // No decision was logged: closed before the log, drained from an older state, or
            // refused before it could be asked. Only the detail tells a verdict from a refusal.
            None => {
                let refused = |d: &str| NOT_A_VERDICT.iter().any(|n| d.contains(n));
                (p.decided_by == Some(DecidedBy::Model)
                    && !p.detail.as_deref().is_some_and(refused))
                .then_some("distinct")
            }
        },
        // A person's press is not journaled, and a pick that found both already retired wrote
        // nothing: the decision is the record that neither was found to state anything.
        PairState::Dismissed if picked == Some("vacuous") => Some("vacuous"),
        _ => None,
    }
}

/// Whether both members stand retired by hand, neither behind the other. A base from before the
/// decision log keeps a person's "neither says anything" as a dismissed pair like any other they
/// dismissed, with no journal row: only where the two stand now tells it from one left alone.
async fn both_retired(core: &Core, p: &ArtifactPair) -> Result<bool> {
    for id in [&p.a_id, &p.b_id] {
        let c = match core.store.get_artifact(id).await {
            Ok(c) => c,
            Err(Error::NotFound) => return Ok(false),
            Err(e) => return Err(e),
        };
        if c.status != ArtifactStatus::Deprecated || c.superseded_by.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn pairs(core: &Core, base: &Base, failed: &mut usize) -> Result<Vec<Found>> {
    let journal = base.journal(core).await?;
    let carried = pair_decisions(core).await?;
    let mut by_pair: BTreeMap<i64, Vec<&Action>> = BTreeMap::new();
    for r in journal
        .iter()
        .filter(|r| r.job == crate::store::actions::Job::Dedupe)
    {
        if let Some(id) = evidence(r).get("pair_id").and_then(|v| v.as_i64()) {
            by_pair.entry(id).or_default().push(r);
        }
    }
    let mut out = Vec::new();
    for s in [
        PairState::NoConflict,
        PairState::Contradiction,
        PairState::Dismissed,
    ] {
        for p in core.store.pairs_by_state(s, i64::MAX).await? {
            let rows = by_pair.get(&p.id).map(Vec::as_slice).unwrap_or_default();
            let said = carried.get(&p.id);
            let unrecorded = said.is_none()
                && rows.is_empty()
                && p.state == PairState::Dismissed
                && p.decided_by == Some(DecidedBy::Operator);
            let past = match pair_past(&p, rows, said) {
                Some(past) => past,
                None if unrecorded => {
                    let read = both_retired(core, &p).await;
                    match or_skip(read, "pair_relation", &p.a_id, failed) {
                        Some(true) => "vacuous",
                        _ => continue,
                    }
                }
                None => continue,
            };
            let asked = said.is_some_and(|c| c.1)
                || rows.iter().any(|r| {
                    evidence(r).get("asked_by").and_then(|v| v.as_str()) == Some("operator")
                });
            let mut f = Found::new(p.created_at, &p.a_id, Some(&p.b_id), Some(past));
            f.person = person_of(rows, asked || p.decided_by == Some(DecidedBy::Operator));
            out.push(f);
        }
    }
    Ok(out)
}

/// A judged link's past verdict as `link_kind`'s picks: its kind, where the judge named one. A
/// link judged before kinds has only its line, and no column says more: the hand-over line means
/// `duplicate`, any other link that was shown is the old `related`.
fn link_past(l: &Link) -> Option<&'static str> {
    if let Some(kind) = l.kind {
        return Some(kind.as_str());
    }
    let reason = l.reason.as_deref();
    match l.state {
        LinkState::Related | LinkState::Dismissed => {
            Some(if reason.is_some_and(|r| r.starts_with("same content")) {
                "duplicate"
            } else {
                RELATED
            })
        }
        LinkState::Unrelated if reason != Some("unreadable") => Some("unrelated"),
        _ => None,
    }
}

/// `directional`: the links `link_direction` would be asked of, compared with the order the base
/// stored where one held.
async fn links(core: &Core, directional: bool) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for l in core.store.judged_links().await? {
        let Some(past) = link_past(&l) else {
            continue;
        };
        // Only a shown link can be dismissed, and a person dismissed it.
        let person = (l.state == LinkState::Dismissed && past != "unrelated")
            .then_some(PersonSaid::Disagreed);
        let (past, person) = if directional {
            // A kind with an order, or the old `related`, which may have had one.
            let ordered = l.kind.map_or(past == RELATED, |k| k.directional());
            if !ordered || person.is_some() {
                continue;
            }
            let first = l.first_is_a.map(|a| if a { "a_first" } else { "b_first" });
            (first, None)
        } else {
            (Some(past), person)
        };
        let mut f = Found::new(l.created_at, &l.a_id, Some(&l.b_id), past);
        f.person = person;
        f.words = l.cues.iter().map(|c| c.q.clone()).collect();
        out.push(f);
    }
    Ok(out)
}

/// The rewrites a rescue wrote: "reap rescue" now; "reap rescue: <reason>" on rows written before.
async fn rescues(core: &Core) -> Result<Vec<Chunk>> {
    let mut all = core.store.synthesized_artifacts(i64::MAX).await?;
    all.retain(|s| s.cues.iter().any(|c| c.starts_with("reap rescue")));
    Ok(all)
}

/// What a buried note was replaced by, if anything.
async fn successor_of(core: &Core, id: &str) -> Result<Option<String>> {
    match core.store.get_artifact(id).await {
        Ok(c) => Ok(c.superseded_by),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The notes a rescue replaced that were not buried since.
async fn rescued_by(core: &Core, rescue: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in core.store.artifacts_superseded_by(rescue).await? {
        if core.store.get_artifact(&id).await?.reaped_at.is_none() {
            out.push(id);
        }
    }
    Ok(out)
}

/// Burials say no; a rescue said yes. A rescued note later buried counts once, as the burial.
async fn reaps(core: &Core, base: &Base, failed: &mut usize) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for r in base
        .journal(core)
        .await?
        .iter()
        .filter(|r| r.kind == Kind::Reap)
    {
        let read = successor_of(core, &r.subject_id).await;
        let Some(successor) = or_skip(read, "reap_worth", &r.subject_id, failed) else {
            continue;
        };
        let mut f = Found::new(r.at, &r.subject_id, successor.as_deref(), Some("no"));
        f.person = person_of(&[r], false);
        out.push(f);
    }
    for s in rescues(core).await? {
        let read = rescued_by(core, &s.id).await;
        for id in or_skip(read, "reap_worth", &s.id, failed).unwrap_or_default() {
            // Its successor now is the rescue; shown, it would argue against the rescue.
            let mut f = Found::new(s.created_at, &id, None, Some("yes"));
            f.not = Some(s.id.clone());
            out.push(f);
        }
    }
    Ok(out)
}

async fn live(core: &Core, category: bool) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for c in core
        .store
        .artifacts_by_status(ArtifactStatus::Active, i64::MAX)
        .await?
    {
        match (category, &c.category) {
            (true, Some(cat)) => out.push(Found::new(c.created_at, &c.id, None, Some(cat))),
            (true, None) => {}
            (false, _) => out.push(Found::new(c.created_at, &c.id, None, None)),
        }
    }
    Ok(out)
}

/// One past merge: its sources as they read now, then what the merge wrote.
async fn merge_case(core: &Core, merged: &str, rows: &[&Action]) -> Result<Found> {
    let mut f = Found::new(
        rows.iter().map(|r| r.at).max().unwrap_or(0),
        merged,
        None,
        Some("yes"),
    );
    f.person = person_of(rows, false);
    let mut subjects: Vec<&str> = rows.iter().map(|r| r.subject_id.as_str()).collect();
    subjects.sort();
    subjects.dedup();
    for s in subjects {
        f.texts.push(text_of(core, s).await?);
    }
    // A merge condensed since: its first version is what the merge wrote.
    let first = core.store.versions_of(merged).await?.into_iter().next();
    f.texts.push(match first {
        Some(v) => Some(Text {
            title: v.title,
            day: None,
            text: v.text,
        }),
        None => text_of(core, merged).await?,
    });
    Ok(f)
}

/// One past condensation: the version it retired, then what it wrote. `None` when that version
/// is no longer kept.
async fn condense_case(core: &Core, r: &Action) -> Result<Option<Found>> {
    let versions = core.store.versions_of(&r.subject_id).await?;
    let Some(v) = versions.iter().find(|v| v.action_id == r.id) else {
        return Ok(None);
    };
    let mut f = Found::new(r.at, &r.subject_id, None, Some("yes"));
    f.person = person_of(&[r], false);
    let as_text = |v: &crate::store::versions::Version| Text {
        title: v.title.clone(),
        day: None,
        text: v.text.clone(),
    };
    f.texts.push(Some(as_text(v)));
    f.texts
        .push(match versions.iter().find(|w| w.n == v.n + 1) {
            Some(next) => Some(as_text(next)),
            None => text_of(core, &r.subject_id).await?,
        });
    Ok(Some(f))
}

/// Past merges and condensations: kept, they passed; a person taking one back disagreed.
async fn rewrites(core: &Core, base: &Base, id: &str, failed: &mut usize) -> Result<Vec<Found>> {
    let journal = base.journal(core).await?;
    let mut merges: BTreeMap<&str, Vec<&Action>> = BTreeMap::new();
    for r in journal.iter().filter(|r| r.kind == Kind::Merge) {
        if let Some(m) = r.survivor_id.as_deref() {
            merges.entry(m).or_default().push(r);
        }
    }
    let mut out = Vec::new();
    let by_evidence = |r: &&Action| r.undone_by == Some(UndoneBy::Evidence);
    for (merged, rows) in merges {
        // The base took it back on what use showed: neither the past verdict nor the person's.
        if rows.iter().any(by_evidence) {
            continue;
        }
        let read = merge_case(core, merged, &rows).await;
        out.extend(or_skip(read, id, merged, failed));
    }
    for r in journal.iter().filter(|r| r.kind == Kind::Condense) {
        if by_evidence(&r) {
            continue;
        }
        let read = condense_case(core, r).await;
        out.extend(or_skip(read, id, &r.subject_id, failed).flatten());
    }
    Ok(out)
}

/// Each search a person answered with the note they wanted: that note, yes, in their words.
async fn searches(core: &Core) -> Result<Vec<Found>> {
    Ok(core
        .store
        .named_hits()
        .await?
        .into_iter()
        .map(|(event, query, expect, at)| {
            let mut f = Found::new(at, &expect, Some(&event), Some("yes"));
            f.person = Some(PersonSaid::Agreed);
            f.words = vec![query];
            f
        })
        .collect())
}

/// What this base holds to ask a question about. A question the base has nothing for finds none.
async fn found(core: &Core, base: &Base, id: &str, failed: &mut usize) -> Result<Vec<Found>> {
    match id {
        "pair_relation" => pairs(core, base, failed).await,
        "link_kind" => links(core, false).await,
        "link_direction" => links(core, true).await,
        "reap_worth" => reaps(core, base, failed).await,
        "category" => live(core, true).await,
        "pinned" | "time_bound" | "self_contained" | "asks_reminder" | "has_date" => {
            live(core, false).await
        }
        "complete" | "adds_nothing" => rewrites(core, base, id, failed).await,
        "answers_query" => searches(core).await,
        // `answerable`: the base keeps no record of what a pursuit was given to write from.
        _ => Ok(vec![]),
    }
}

/// Up to five live notes near `id`, from its graveyard vector once buried. A failed read is the
/// caller's to skip the case on: asked without its neighbours, a note would look worth keeping.
async fn neighbours(core: &Core, base: &Base, id: &str, not: Option<&str>) -> Result<Vec<Text>> {
    let buried = core.store.get_artifact(id).await?.reaped_at.is_some();
    let hits = if buried {
        match base.grave_vector(core, id).await? {
            Some(v) => {
                core.vectors
                    .search(v, &Default::default(), 8, &Default::default())
                    .await?
            }
            None => vec![],
        }
    } else {
        core.vectors.neighbours(id, 8).await?
    };
    let mut out = Vec::new();
    for h in hits {
        let hid = h.payload.artifact_id.as_str();
        if hid == id || Some(hid) == not {
            continue;
        }
        if core.store.artifact_in_results(hid).await? == Some(true) {
            out.push(Text {
                title: h.payload.title.clone(),
                day: None,
                text: h.payload.text.clone(),
            });
        }
        if out.len() >= 5 {
            break;
        }
    }
    Ok(out)
}

/// A model-written member's captured roots, dated, as its merge sources.
async fn sources(core: &Core, id: &str) -> Result<Vec<Text>> {
    let c = core.store.get_artifact(id).await?;
    if !c.provenance.is_model_written() {
        return Ok(vec![]);
    }
    let roots = core
        .store
        .roots_of(std::slice::from_ref(&c.id))
        .await?
        .remove(&c.id)
        .unwrap_or_default();
    let mut out = Vec::new();
    for r in roots.iter().filter(|r| *r != id) {
        out.extend(text_of(core, r).await?);
    }
    Ok(out)
}

/// The state and the title, or `None` when a subject has no text anywhere.
async fn shown(
    core: &Core,
    base: &Base,
    a: &Asked,
    f: &Found,
) -> Result<Option<(Option<String>, String)>> {
    let room = state::room(core, &[a]);
    let c = &core.counter;
    let title = |t: &Text| t.title.clone().unwrap_or_else(|| "untitled".into());
    let other = match &f.b {
        Some(b) => text_of(core, b).await?,
        None => None,
    };
    Ok(match a.id {
        "pair_relation" | "link_kind" | "link_direction" => {
            let (Some(x), Some(y)) = (text_of(core, &f.a).await?, other) else {
                return Ok(None);
            };
            let both = format!("{} ↔ {}", title(&x), title(&y));
            let s = if a.id == "pair_relation" {
                let (sa, sb) = (
                    sources(core, &f.a).await?,
                    sources(core, f.b.as_deref().unwrap_or_default()).await?,
                );
                let sa: Vec<Note> = sa.iter().map(Text::note).collect();
                let sb: Vec<Note> = sb.iter().map(Text::note).collect();
                state::pair(&x.note(), &y.note(), &sa, &sb, c, room)
            } else {
                state::link(&x.note(), &y.note(), &f.words, c, room)
            };
            Some((s, both))
        }
        "reap_worth" => {
            let Some(x) = text_of(core, &f.a).await? else {
                return Ok(None);
            };
            let near = neighbours(core, base, &f.a, f.not.as_deref()).await?;
            let near: Vec<Note> = near.iter().map(Text::note).collect();
            let successor = other.as_ref().map(Text::note);
            let s = state::reap(&x.note(), successor.as_ref(), &near, c, room);
            Some((s, title(&x)))
        }
        "asks_reminder" | "has_date" => {
            let Some(x) = text_of(core, &f.a).await? else {
                return Ok(None);
            };
            Some((state::window(&x.text, c, room), title(&x)))
        }
        "complete" | "adds_nothing" => {
            let Some(texts) = f
                .texts
                .iter()
                .map(Option::as_ref)
                .collect::<Option<Vec<&Text>>>()
            else {
                return Ok(None);
            };
            let Some((rewrite, originals)) = texts.split_last() else {
                return Ok(None);
            };
            let originals: Vec<Note> = originals.iter().map(|t| t.note()).collect();
            let s = state::rewrite(&originals, &rewrite.note(), c, room);
            Some((s, title(rewrite)))
        }
        _ => {
            let Some(x) = text_of(core, &f.a).await? else {
                return Ok(None);
            };
            Some((state::one(&x.note(), c, room), title(&x)))
        }
    })
}

struct Item {
    case: Case,
    asked: Asked,
    title: String,
    sim: Option<f32>,
    /// `link_direction`: the same case with B's note under `A:` and A's under `B:`.
    exchanged: Option<String>,
}

/// A direction that holds flips its pick when the notes trade places; the option swap cannot see a pull to `A`.
async fn exchanged(core: &Core, a: &Asked, f: &Found) -> Result<Option<String>> {
    let other = match f.b.as_deref() {
        Some(b) => text_of(core, b).await?,
        None => None,
    };
    let (Some(x), Some(y)) = (text_of(core, &f.a).await?, other) else {
        return Ok(None);
    };
    let room = state::room(core, &[a]);
    Ok(state::link(
        &y.note(),
        &x.note(),
        &f.words,
        &core.counter,
        room,
    ))
}

struct Gathered {
    items: Vec<Item>,
    left_out: usize,
    no_text: usize,
    /// Cases a read failed for, while they were found or while they were laid out.
    failed: usize,
}

async fn gather(core: &Core, base: &Base, a: &Asked, limit: usize) -> Result<Gathered> {
    let mut failed = 0;
    let all = found(core, base, a.id, &mut failed).await?;
    gather_from(core, base, a, all, failed, limit).await
}

/// One live note with its nearest live neighbours at `consolidate.scan_min` or above; a pair
/// already filed in any state is not unjudged.
async fn near_pairs(
    core: &Core,
    c: &Chunk,
    seen: &mut HashSet<(String, String)>,
) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for h in core
        .vectors
        .neighbours(&c.id, core.consolidate.per_point)
        .await?
    {
        let other = h.payload.artifact_id.as_str();
        let Some(sim) = h.similarity.filter(|s| *s >= core.consolidate.scan_min) else {
            continue;
        };
        let (a, b) = crate::store::links::canonical(&c.id, other);
        if a == b || !seen.insert((a.to_string(), b.to_string())) {
            continue;
        }
        if core.store.pair_state_between(a, b).await?.is_some()
            || core.store.artifact_in_results(other).await? != Some(true)
        {
            continue;
        }
        let mut f = Found::new(c.created_at, a, Some(b), None);
        f.sim = Some(sim);
        out.push(f);
    }
    Ok(out)
}

/// Live notes newest first, each with its neighbours as the relate stage reads them. A note whose
/// neighbours cannot be read is counted and the rest go on.
async fn neighbour_pairs(core: &Core, failed: &mut usize) -> Result<Vec<Found>> {
    let (mut seen, mut out) = (HashSet::new(), Vec::new());
    for c in core
        .store
        .artifacts_by_status(ArtifactStatus::Active, i64::MAX)
        .await?
    {
        let read = near_pairs(core, &c, &mut seen).await;
        out.extend(or_skip(read, "pair_relation", &c.id, failed).unwrap_or_default());
    }
    Ok(out)
}

/// Superseded notes no burial or rescue judged, newest retirement first, with what replaced them.
/// A note in the Trash is not asked about: a person threw it away.
async fn retired(core: &Core, base: &Base) -> Result<Vec<Found>> {
    let journal = base.journal(core).await?;
    let mut judged: HashSet<String> = journal
        .iter()
        .filter(|r| r.kind == Kind::Reap)
        .map(|r| r.subject_id.clone())
        .collect();
    for s in rescues(core).await? {
        judged.extend(core.store.artifacts_superseded_by(&s.id).await?);
    }
    Ok(core
        .store
        .superseded_artifacts(i64::MAX)
        .await?
        .into_iter()
        .filter(|c| c.status != ArtifactStatus::Trashed && !judged.contains(&c.id))
        .map(|c| {
            Found::new(
                c.retired_at.unwrap_or(c.created_at),
                &c.id,
                c.superseded_by.as_deref(),
                None,
            )
        })
        .collect())
}

/// The unjudged section a question has, if any: its scope, its cases, and how many could not be
/// read.
async fn unjudged(
    core: &Core,
    base: &Base,
    id: &str,
) -> Result<Option<(&'static str, Vec<Found>, usize)>> {
    Ok(match id {
        "pair_relation" => {
            let mut failed = 0;
            let all = neighbour_pairs(core, &mut failed).await?;
            Some(("unjudged neighbours", all, failed))
        }
        "reap_worth" => Some(("unjudged retired notes", retired(core, base).await?, 0)),
        _ => None,
    })
}

/// The unjudged cases for `pair_relation` or `reap_worth`, newest first, and how many `limit` left out.
pub async fn unjudged_cases(core: &Core, a: &Asked, limit: usize) -> Result<(Vec<Case>, usize)> {
    let base = Base::default();
    let Some((_, all, failed)) = unjudged(core, &base, a.id).await? else {
        return Ok((vec![], 0));
    };
    let g = gather_from(core, &base, a, all, failed, limit).await?;
    Ok((g.items.into_iter().map(|i| i.case).collect(), g.left_out))
}

/// One case as it is asked: its state, its title and, for `link_direction`, the state with the
/// notes exchanged. `None` when a subject has no text anywhere.
async fn laid_out(
    core: &Core,
    base: &Base,
    asked: &Asked,
    f: &Found,
) -> Result<Option<(Option<String>, String, Option<String>)>> {
    let Some((state, title)) = shown(core, base, asked, f).await? else {
        return Ok(None);
    };
    let ex = if asked.id == "link_direction" && state.is_some() {
        exchanged(core, asked, f).await?
    } else {
        None
    };
    Ok(Some((state, title, ex)))
}

async fn gather_from(
    core: &Core,
    base: &Base,
    a: &Asked,
    mut all: Vec<Found>,
    mut failed: usize,
    limit: usize,
) -> Result<Gathered> {
    all.sort_by(|x, y| (y.at, &y.a, &y.b).cmp(&(x.at, &x.a, &x.b)));
    let left_out = all.len().saturating_sub(limit);
    all.truncate(limit);
    let (mut items, mut no_text) = (Vec::new(), 0);
    for f in all {
        // `answers_query` carries its search in the wording.
        let asked = match f.words.first() {
            Some(q) if a.id == "answers_query" => catalogue::answers_query(q),
            _ => a.clone(),
        };
        let read = laid_out(core, base, &asked, &f).await;
        let Some(read) = or_skip(read, a.id, &f.a, &mut failed) else {
            continue;
        };
        let Some((state, title, ex)) = read else {
            no_text += 1;
            continue;
        };
        items.push(Item {
            case: Case {
                subjects: (f.a, f.b),
                state,
                past: f.past,
                person: f.person,
            },
            asked,
            title,
            sim: f.sim,
            exchanged: ex,
        });
    }
    Ok(Gathered {
        items,
        left_out,
        no_text,
        failed,
    })
}

/// The cases for one question, newest first, and how many `limit` left out.
pub async fn cases(core: &Core, a: &Asked, limit: usize) -> Result<(Vec<Case>, usize)> {
    let g = gather(core, &Base::default(), a, limit).await?;
    Ok((g.items.into_iter().map(|i| i.case).collect(), g.left_out))
}

/// How long one call may wait out an endpoint that is up and busy before the run ends there.
const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

/// Straight to the decider, swapped where the question is: no gate, no log.
async fn put(core: &Core, a: &Asked, state: &str) -> Result<(String, f64, Option<bool>)> {
    let questions = [a.question.clone()];
    let key = &a.question.key;
    let (answers, agree) = if a.swapped {
        let s = decide_swapped(core.decider.as_ref(), state, &questions).await?;
        let agree = s.agree.get(key).copied();
        (s.answers, agree)
    } else {
        (core.decider.decide(state, &questions).await?, None)
    };
    let answer = answers
        .answers
        .get(key)
        .ok_or_else(|| Error::InferenceRejected {
            role: "decide",
            detail: format!("the answer has none for \"{key}\""),
        })?;
    let (pick, top) = pick_and_top(&a.question, answer);
    Ok((pick, top, agree))
}

/// `put`, waiting out an endpoint that is up and busy. Any other failure comes back at once: the
/// client has already spent its own retries on it.
async fn put_waiting(core: &Core, a: &Asked, state: &str) -> Result<(String, f64, Option<bool>)> {
    transiently(RETRY_BUDGET, || async move {
        match put(core, a, state).await {
            Err(e @ Error::InferenceBusy { .. }) => Err(e),
            other => Ok(other),
        }
    })
    .await?
}

/// One case's answer and, for `link_direction`, its pick with the notes exchanged. Each read is
/// its own request with its own waiting: the second failing does not ask the first again.
async fn answer(
    core: &Core,
    item: &Item,
    state: &str,
) -> Result<(String, f64, Option<bool>, Option<String>)> {
    let (pick, top, agree) = put_waiting(core, &item.asked, state).await?;
    let again = match &item.exchanged {
        Some(ex) => Some(put_waiting(core, &item.asked, ex).await?.0),
        None => None,
    };
    Ok((pick, top, agree, again))
}

/// The endpoint is not answering: unreachable or failing after the client's retries, or still busy
/// after `RETRY_BUDGET`. Every remaining call would wait as long and fail the same way.
fn not_answering(e: &Error) -> bool {
    matches!(e, Error::Inference { .. } | Error::InferenceBusy { .. })
}

fn agrees(past: &str, pick: &str) -> bool {
    past.split('|').any(|p| p == pick)
}

/// The bin in hundredths; a top under 0.50 (a choice can have one) is bin 0, shown as `<0.50`.
fn floor(top: f64) -> i64 {
    let h = (top * 100.0 + 1e-9).floor() as i64;
    if h < 50 { 0 } else { (h / 5 * 5).min(95) }
}

fn summarize(
    a: &Asked,
    rows: &[Row],
    titles: &[String],
    sims: &[Option<f32>],
    bands_at: Thresholds,
    counts: (usize, usize, usize),
    scope: Option<&str>,
) -> QuestionReport {
    let (skipped_too_long, skipped_no_text, left_out) = counts;
    let agree = |r: &Row| r.past.as_deref().is_some_and(|p| agrees(p, &r.pick));
    let with_past = rows.iter().filter(|r| r.past.is_some()).count();
    let agrees_with_past =
        (with_past > 0).then(|| (rows.iter().filter(|r| agree(r)).count(), with_past));
    let with_person: Vec<bool> = rows
        .iter()
        .filter_map(|r| {
            r.person.map(|p| match p {
                PersonSaid::Agreed => agree(r),
                PersonSaid::Disagreed => !agree(r),
            })
        })
        .collect();
    let agrees_with_person = (!with_person.is_empty()).then(|| {
        (
            with_person.iter().filter(|x| **x).count(),
            with_person.len(),
        )
    });
    let mut bands = [0; 3];
    let mut bins: BTreeMap<i64, (usize, usize)> = BTreeMap::new();
    for r in rows {
        bands[match r.band {
            Band::Act => 0,
            Band::Ask => 1,
            Band::Leave => 2,
        }] += 1;
        let bin = bins.entry(floor(r.top)).or_default();
        bin.0 += 1;
        bin.1 += usize::from(agree(r));
    }
    let by_top: Vec<(f64, usize, usize)> = bins
        .into_iter()
        .rev()
        .map(|(h, (c, n))| (h as f64 / 100.0, c, n))
        .collect();
    let (mut extremes, mut confident) = (Vec::new(), Vec::new());
    if let (Some(_), QKind::Choice { options }) = (scope, &a.question.kind) {
        for (name, _) in options {
            let mut mine: Vec<Confident> = rows
                .iter()
                .zip(titles.iter().zip(sims))
                .filter(|(r, _)| &r.pick == name)
                .map(|(r, (t, sim))| Confident {
                    pick: r.pick.clone(),
                    top: r.top,
                    title: t.clone(),
                    similarity: *sim,
                })
                .collect();
            mine.sort_by(|x, y| y.top.total_cmp(&x.top));
            confident.extend(mine.into_iter().take(5));
        }
    } else if agrees_with_past.is_none() {
        let noul = matches!(a.question.kind, QKind::Noul);
        extremes = rows
            .iter()
            .zip(titles)
            .map(|(r, t)| match (noul, r.pick == "yes") {
                (true, _) => (yes_of(r), t.clone()),
                (false, _) => (r.top, format!("{} · {t}", r.pick)),
            })
            .collect();
        extremes.sort_by(|x, y| y.0.total_cmp(&x.0));
        if extremes.len() > 20 {
            let n = extremes.len();
            extremes.drain(10..n - 10);
        }
    }
    QuestionReport {
        id: a.id.to_string(),
        version: a.version,
        asked: rows.len(),
        skipped_too_long,
        skipped_no_text,
        // The run fills these two: `summarize` sees only what was asked.
        skipped_failed: 0,
        stopped: None,
        left_out,
        suggested: agrees_with_past.and_then(|_| suggest(&by_top)),
        agrees_with_past,
        agrees_with_person,
        bands,
        by_top,
        swap_disagreed: rows
            .iter()
            .filter(|r| r.swapped_agree == Some(false))
            .count(),
        extremes,
        bands_at,
        scope: scope.map(str::to_string),
        confident,
        picks: if matches!(a.question.kind, QKind::Noul) {
            let yes = rows.iter().filter(|r| r.pick == "yes").count();
            vec![("yes".into(), yes), ("no".into(), rows.len() - yes)]
        } else {
            tally(rows.iter().map(|r| r.pick.as_str()))
        },
        past_picks: {
            let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for r in rows {
                if let Some(p) = r.past.as_deref() {
                    by.entry(if p == RELATED { "related" } else { p })
                        .or_default()
                        .push(&r.pick);
                }
            }
            let mut v: Vec<(String, Vec<(String, usize)>)> = by
                .into_iter()
                .map(|(k, ps)| (k.to_string(), tally(ps.into_iter())))
                .collect();
            let total = |x: &[(String, usize)]| x.iter().map(|p| p.1).sum::<usize>();
            v.sort_by(|x, y| total(&y.1).cmp(&total(&x.1)).then_with(|| x.0.cmp(&y.0)));
            v
        },
        exchanged_holds: None,
        disagreements: {
            let mut v: Vec<Miss> = Vec::new();
            if scope.is_none() && matches!(a.question.kind, QKind::Choice { .. }) {
                for (r, t) in rows.iter().zip(titles) {
                    if let Some(p) = r.past.as_deref().filter(|p| !agrees(p, &r.pick)) {
                        v.push(Miss {
                            top: r.top,
                            past: if p == RELATED { "related" } else { p }.to_string(),
                            pick: r.pick.clone(),
                            title: t.clone(),
                        });
                    }
                }
            }
            v.sort_by(|x, y| y.top.total_cmp(&x.top));
            v.truncate(10);
            v
        },
        reading: fixed_line(a.id)
            .map(|at| (at, rows.iter().filter(|r| yes_of(r) >= at - 1e-9).count())),
    }
}

fn estimate(cases: usize) -> String {
    let secs = cases * 6 / 10;
    if secs < 120 {
        format!("{secs} s")
    } else {
        format!("{} min", secs.div_ceil(60))
    }
}

/// Every question, or `question` alone. An unknown id is refused before any call.
pub async fn run(core: &Core, question: Option<&str>, limit: usize) -> Result<Vec<QuestionReport>> {
    let asked = match question {
        Some(id) => {
            vec![catalogue::by_id(id).ok_or_else(|| Error::Validation(unknown_question(id)))?]
        }
        None => catalogue::all(),
    };
    // Each question's judged section, then its unjudged one where it has one.
    let base = Base::default();
    let mut gathered = Vec::new();
    for a in &asked {
        gathered.push((a, None, gather(core, &base, a, limit).await?));
        if let Some((scope, all, failed)) = unjudged(core, &base, a.id).await? {
            let g = gather_from(core, &base, a, all, failed, limit).await?;
            gathered.push((a, Some(scope), g));
        }
    }
    let n: usize = gathered
        .iter()
        .flat_map(|(_, _, g)| &g.items)
        .map(|i| usize::from(i.case.state.is_some()) + usize::from(i.exchanged.is_some()))
        .sum();
    eprintln!("{n} cases to ask, about {} at 0.6 s each", estimate(n));
    let mut out = Vec::new();
    for (a, scope, g) in gathered {
        let (mut holds, mut exchanged_n) = (0, 0);
        let at = core.decide_cfg.bands.of(a.id);
        let (mut rows, mut titles, mut sims, mut too_long) =
            (Vec::new(), Vec::new(), Vec::new(), 0);
        let (mut failed, mut stopped) = (g.failed, None);
        for item in g.items {
            let Some(state) = &item.case.state else {
                too_long += 1;
                continue;
            };
            let (pick, top, agree, again) = match answer(core, &item, state).await {
                Ok(read) => read,
                Err(e) if not_answering(&e) => {
                    stopped = Some(format!(
                        "the decision endpoint did not answer ({e}); the rest of this question \
                         and every question after it were not asked"
                    ));
                    break;
                }
                // The endpoint answered and refused this one: the next case is another request.
                Err(e) => {
                    let case = item.case.subjects.0.as_str();
                    tracing::warn!(
                        question = a.id,
                        case,
                        error = %e,
                        "a case was refused; it is skipped"
                    );
                    failed += 1;
                    continue;
                }
            };
            if let Some(again) = again {
                exchanged_n += 1;
                holds += usize::from(again != pick && (again == "a_first" || again == "b_first"));
            }
            rows.push(Row {
                pick,
                top,
                swapped_agree: agree,
                band: band(top, agree, at),
                past: item.case.past,
                person: item.case.person,
            });
            titles.push(item.title);
            sims.push(item.sim);
        }
        let counts = (too_long, g.no_text, g.left_out);
        let mut r = summarize(a, &rows, &titles, &sims, at, counts, scope);
        if a.id == "link_direction" && scope.is_none() {
            r.exchanged_holds = Some((holds, exchanged_n));
        }
        r.skipped_failed = failed;
        r.stopped = stopped;
        eprintln!(
            "{}{}: {} asked",
            r.id,
            scope.map(|s| format!(" ({s})")).unwrap_or_default(),
            r.asked
        );
        // What was asked until now is the report: every further call would wait and fail alike.
        let ended = r.stopped.clone();
        out.push(r);
        if let Some(why) = ended {
            eprintln!("stopped: {why}");
            break;
        }
    }
    Ok(out)
}

/// The lowest line from 0.80 that holds 95 of 100 over at least 30 cases, and an ask line under it.
pub fn suggest(by_top: &[(f64, usize, usize)]) -> Option<Thresholds> {
    let bins: Vec<(i64, usize, usize)> = by_top
        .iter()
        .map(|&(f, c, a)| ((f * 100.0).round() as i64, c, a))
        .collect();
    let from = |h: i64| {
        bins.iter()
            .filter(|b| b.0 >= h)
            .fold((0, 0), |(c, a), b| (c + b.1, a + b.2))
    };
    let act = (80..=95).step_by(5).find(|&h| {
        let (c, a) = from(h);
        c >= 30 && a * 100 >= c * 95
    })?;
    let act = ((act + 1) / 2 * 2).min(99);
    let ask = (50..=95)
        .step_by(5)
        .find(|&h| {
            let (c, a) = from(h);
            c > 0 && a * 100 >= c * 70
        })
        .unwrap_or(act);
    let ask = ask.min(act - 10).max(50);
    Some(Thresholds {
        act_at: act as f64 / 100.0,
        ask_at: ask as f64 / 100.0,
    })
}

fn render_one(r: &QuestionReport) -> String {
    let scope = r
        .scope
        .as_deref()
        .map(|s| format!(" · {s}"))
        .unwrap_or_default();
    // Named only when there are some: a run with every case read keeps the line the plan draws.
    let failed = match r.skipped_failed {
        0 => String::new(),
        n => format!(" · {n} failed"),
    };
    let mut s = format!(
        "{} v{}{scope} · {} asked · {} too long · {} without text{failed} · {} left out\n",
        r.id, r.version, r.asked, r.skipped_too_long, r.skipped_no_text, r.left_out
    );
    if let Some(why) = &r.stopped {
        s += &format!("  stopped: {why}\n");
    } else if r.asked + r.skipped_too_long + r.skipped_no_text + r.skipped_failed + r.left_out == 0
    {
        s.push_str(if r.id == "answerable" {
            "  not asked: the base keeps nothing this question can be replayed over\n"
        } else {
            "  nothing in this base to ask it about\n"
        });
        return s;
    }
    let line = |label: &str, value: String| format!("  {label:<31}{value}\n");
    if let Some((a, n)) = r.agrees_with_past {
        s += &line("agrees with the past verdict", format!("{a:>3} of {n}"));
    }
    if let Some((a, n)) = r.agrees_with_person {
        s += &line("agrees with the person", format!("{a:>3} of {n}"));
    }
    if catalogue::by_id(&r.id).is_some_and(|a| a.swapped) {
        s += &line(
            "swapped reads disagreed",
            format!("{:>3}", r.swap_disagreed),
        );
    }
    if let Some((n, m)) = r.exchanged_holds {
        s += &format!("  holds when the notes are exchanged   {n} of {m}\n");
    }
    if r.asked == 0 {
        return s;
    }
    if let Some((at, yes)) = r.reading {
        s += &format!("  at {at:.2}   yes {yes} of {}\n", r.asked);
    } else if !is_reading(&r.id) {
        let [act, ask, leave] = r.bands;
        let would = format!("would act {act} · ask {ask} · leave {leave}");
        s += &format!(
            "  {would:<37} (act_at {:.2}, ask_at {:.2})\n",
            r.bands_at.act_at, r.bands_at.ask_at
        );
    }
    let past = r.agrees_with_past.is_some();
    s += if past {
        "  top     cases  agree\n"
    } else {
        "  top     cases\n"
    };
    for (f, c, a) in &r.by_top {
        let label = if *f < 0.5 {
            "<0.50".to_string()
        } else {
            format!("{f:.2}")
        };
        s += &format!("  {label}{c:>w$}", w = 13 - label.len());
        if past {
            s += &format!("{a:>7}");
        }
        s += "\n";
    }
    let list = |ps: &[(String, usize)]| {
        ps.iter()
            .map(|(p, n)| format!("{p} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if !r.picks.is_empty() {
        s += &format!("  picks   {}\n", list(&r.picks));
    }
    for (past, ps) in &r.past_picks {
        s += &format!("  past {past:<12} {}\n", list(ps));
    }
    for m in &r.disagreements {
        s += &format!(
            "  disagrees  {:.2}  past {} → {}  {}\n",
            m.top, m.past, m.pick, m.title
        );
    }
    if past {
        s += &match r.suggested {
            Some(t) => format!(
                "  suggested   act_at {:.2}   ask_at {:.2}\n",
                t.act_at, t.ask_at
            ),
            None => "  suggested   none: under 30 cases, or under 95 of 100, from 0.80\n".into(),
        };
    }
    if !r.extremes.is_empty() {
        let noul = catalogue::by_id(&r.id).is_some_and(|a| matches!(a.question.kind, QKind::Noul));
        s += if noul {
            "  highest and lowest yes\n"
        } else {
            "  most and least sure\n"
        };
        for (v, t) in &r.extremes {
            s += &format!("  {v:.2}  {t}\n");
        }
    }
    if !r.confident.is_empty() {
        s += "  most confident\n";
        for c in &r.confident {
            let sim = c
                .similarity
                .map(|x| format!("  (cosine {x:.2})"))
                .unwrap_or_default();
            s += &format!("  {:<13} {:.2}  {}{sim}\n", c.pick, c.top, c.title);
        }
    }
    s
}

/// One block per question, a blank line between.
pub fn render(reports: &[QuestionReport]) -> String {
    reports
        .iter()
        .map(render_one)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::core::judging::band::Thresholds;
    use crate::core::judging::catalogue;
    use crate::core::test_support::test_core;
    use crate::infer::decide::{Answer, Answers, Kind as QKind, Question};
    use crate::infer::fake::RuleDecider;
    use crate::jobs::consolidate::tests::{seed, seed_titled};
    use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
    use crate::store::decision_log::{NewLogged, Outcome, State};
    use crate::store::links::{LinkKind, LinkState};
    use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};
    use crate::vector::{
        Facets, LifecycleRow, SearchFilter, SearchHit, StoredLifecycle, Touch, VectorPayload,
        VectorPoint, VectorStore, sparse::SparseVector,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rule(f: impl Fn(&str, &Question) -> Answer + Send + Sync + 'static) -> Arc<RuleDecider> {
        Arc::new(RuleDecider::new(move |state, qs| {
            Ok(Answers {
                answers: qs.iter().map(|q| (q.key.clone(), f(state, q))).collect(),
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }))
    }

    /// `name` at `p`, the rest sharing what is left, in this question's own option order.
    fn pick(q: &Question, name: &str, p: f64) -> Answer {
        let QKind::Choice { options } = &q.kind else {
            panic!("not a choice")
        };
        let rest = (1.0 - p) / (options.len() - 1) as f64;
        Answer::Choice {
            pick: name.into(),
            probabilities: options
                .iter()
                .map(|(n, _)| if n == name { p } else { rest })
                .collect(),
            confidence: p,
        }
    }

    /// A choice gets its first option at 0.9, a noul yes at 0.9.
    fn anything() -> Arc<RuleDecider> {
        rule(|_, q| match &q.kind {
            QKind::Choice { options } => pick(q, &options[0].0, 0.9),
            _ => Answer::Noul { yes: 0.9 },
        })
    }

    async fn core_with(decider: Arc<RuleDecider>) -> Core {
        let mut core = test_core().await;
        core.decider = decider;
        core
    }

    async fn pair(core: &Core, a: &str, b: &str) -> ArtifactPair {
        core.store.record_pair(a, b, 0.9).await.unwrap();
        core.store.pair_between(a, b).await.unwrap().unwrap()
    }

    fn dedupe_row(
        kind: Kind,
        p: &ArtifactPair,
        subject: &str,
        survivor: Option<&str>,
    ) -> NewAction {
        NewAction {
            job: Job::Dedupe,
            kind,
            subject_id: subject.into(),
            survivor_id: survivor.map(String::from),
            detail: None,
            evidence: serde_json::json!({ "pair_id": p.id, "a": p.a_id, "b": p.b_id }),
            pair_score: Some(p.score),
        }
    }

    async fn pasts(
        core: &Core,
        a: &crate::core::judging::Asked,
    ) -> HashMap<String, Option<String>> {
        let (cases, _) = cases(core, a, 200).await.unwrap();
        cases
            .into_iter()
            .map(|c| {
                (
                    format!("{}|{}", c.subjects.0, c.subjects.1.unwrap_or_default()),
                    c.past,
                )
            })
            .collect()
    }

    fn key(a: &str, b: &str) -> String {
        format!("{a}|{b}")
    }

    /// Bury `id` the way `Store::bury` leaves it, with or without its graveyard row.
    async fn bury(core: &Core, id: &str, grave_text: Option<&str>) {
        if let Some(text) = grave_text {
            sqlx::query!(
                "INSERT INTO graveyard (id, title, text, meta_json, reaped_at) VALUES (?, NULL, ?, '{}', 1)",
                id,
                text
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
        }
        sqlx::query!(
            "UPDATE artifacts SET text = '', reaped_at = 1, status = 'deprecated' WHERE id = ?",
            id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    #[test]
    fn the_suggestion_is_the_lowest_line_that_holds_ninety_five_of_a_hundred_over_thirty_cases() {
        // From 0.80: 60 cases, 49 agree (82 %); from 0.85: 40 cases, 39 agree (97.5 %).
        let by_top = [(0.85, 40, 39), (0.80, 20, 10)];
        assert_eq!(
            suggest(&by_top),
            Some(Thresholds {
                act_at: 0.86,
                ask_at: 0.50
            })
        );
        assert_eq!(suggest(&[(0.80, 40, 39)]).unwrap().act_at, 0.80);
        // Only from 0.95 do 95 of 100 agree; rounded up to the next 0.02.
        assert_eq!(
            suggest(&[(0.95, 40, 40), (0.90, 10, 0)]).unwrap().act_at,
            0.96
        );
    }

    #[test]
    fn too_few_cases_suggest_nothing() {
        assert_eq!(suggest(&[(0.95, 29, 29)]), None);
        assert_eq!(suggest(&[(0.90, 100, 80)]), None, "80 of 100 is not 95");
        assert_eq!(
            suggest(&[(0.75, 100, 100)]),
            None,
            "nothing at 0.80 or above"
        );
        assert_eq!(suggest(&[]), None);
    }

    #[test]
    fn ask_at_stays_a_tenth_under_act_at() {
        // From 0.85: 50 of 100 agree; from 0.90: 40 of 40. Seventy percent first holds at 0.90.
        assert_eq!(
            suggest(&[(0.90, 40, 40), (0.85, 60, 10)]),
            Some(Thresholds {
                act_at: 0.90,
                ask_at: 0.80
            })
        );
    }

    #[tokio::test]
    async fn a_merged_pair_is_a_duplicate_case_and_a_superseded_one_names_its_direction() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [1.0, 0.0]),
                ("five", [1.0, 0.0]),
                ("six", [1.0, 0.0]),
                ("seven", [1.0, 0.0]),
                ("eight", [1.0, 0.0]),
            ],
        )
        .await;
        let merged = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_merged(merged.id, "m1", Some("one"), DecidedBy::Model)
            .await
            .unwrap();
        let sup = pair(&core, &ids[2], &ids[3]).await;
        core.store
            .record_action(&dedupe_row(
                Kind::Supersede,
                &sup,
                &sup.a_id,
                Some(&sup.b_id),
            ))
            .await
            .unwrap();
        core.store
            .set_pair_state(
                sup.id,
                PairState::Dismissed,
                Some("B is newer"),
                DecidedBy::Model,
            )
            .await
            .unwrap();
        let con = pair(&core, &ids[4], &ids[5]).await;
        core.store
            .set_pair_state(
                con.id,
                PairState::Contradiction,
                Some("8080 or 9090"),
                DecidedBy::Model,
            )
            .await
            .unwrap();
        let dis = pair(&core, &ids[6], &ids[7]).await;
        core.store
            .set_pair_state(
                dis.id,
                PairState::NoConflict,
                Some("different subjects"),
                DecidedBy::Model,
            )
            .await
            .unwrap();

        let got = pasts(&core, &catalogue::pair_relation()).await;
        let want: HashMap<String, Option<String>> = [
            (key(&merged.a_id, &merged.b_id), "duplicate"),
            (key(&sup.a_id, &sup.b_id), "b_replaces_a"),
            (key(&con.a_id, &con.b_id), "conflict"),
            (key(&dis.a_id, &dis.b_id), "distinct"),
        ]
        .into_iter()
        .map(|(k, v)| (k, Some(v.to_string())))
        .collect();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn an_undone_action_is_the_person_disagreeing() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [1.0, 0.0]),
            ],
        )
        .await;
        let undone = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .record_action(&dedupe_row(
                Kind::Supersede,
                &undone,
                &undone.b_id,
                Some(&undone.a_id),
            ))
            .await
            .unwrap();
        core.store
            .set_pair_state(undone.id, PairState::Dismissed, None, DecidedBy::Model)
            .await
            .unwrap();
        core.store
            .undo_action_on(
                &undone.b_id,
                Kind::Supersede,
                UndoneBy::Operator,
                "put back",
            )
            .await
            .unwrap();
        let kept = pair(&core, &ids[2], &ids[3]).await;
        core.store
            .set_pair_state(kept.id, PairState::Contradiction, None, DecidedBy::Operator)
            .await
            .unwrap();

        let (cases, _) = cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        let by: HashMap<String, &Case> = cases
            .iter()
            .map(|c| (key(&c.subjects.0, c.subjects.1.as_deref().unwrap()), c))
            .collect();
        let u = by[&key(&undone.a_id, &undone.b_id)];
        assert_eq!(u.past.as_deref(), Some("a_replaces_b"));
        assert_eq!(u.person, Some(PersonSaid::Disagreed));
        let k = by[&key(&kept.a_id, &kept.b_id)];
        assert_eq!(k.past.as_deref(), Some("conflict"));
        assert_eq!(
            k.person,
            Some(PersonSaid::Agreed),
            "an operator's own verdict"
        );
    }

    #[tokio::test]
    async fn a_pair_closed_by_a_refusal_is_not_a_case() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [1.0, 0.0]),
                ("five", [1.0, 0.0]),
                ("six", [1.0, 0.0]),
                ("seven", [1.0, 0.0]),
                ("eight", [1.0, 0.0]),
            ],
        )
        .await;
        for (i, detail) in [
            "This was done to one of these before and taken back, so both stay as they are.",
            "a draft dropped a value one of them states; both stay as they are",
            "these two do not fit one call; both stay as they are",
        ]
        .into_iter()
        .enumerate()
        {
            let p = pair(&core, &ids[2 * i], &ids[2 * i + 1]).await;
            core.store
                .set_pair_state(p.id, PairState::NoConflict, Some(detail), DecidedBy::Model)
                .await
                .unwrap();
        }
        let by_evidence = pair(&core, &ids[6], &ids[7]).await;
        core.store
            .set_pair_state(
                by_evidence.id,
                PairState::Contradiction,
                None,
                DecidedBy::Evidence,
            )
            .await
            .unwrap();

        let (cases, left_out) = cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        assert!(cases.is_empty(), "{cases:?}");
        assert_eq!(left_out, 0);
    }

    #[tokio::test]
    async fn a_buried_member_is_read_from_the_graveyard() {
        let core = core_with(anything()).await;
        let ids = seed(&core, &[("alive", [1.0, 0.0]), ("soon buried", [1.0, 0.0])]).await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_state(p.id, PairState::Contradiction, None, DecidedBy::Model)
            .await
            .unwrap();
        bury(&core, &ids[1], Some("the words the grave keeps")).await;

        let (cases, _) = cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        assert_eq!(cases.len(), 1);
        let state = cases[0].state.as_deref().unwrap();
        assert!(state.contains("the words the grave keeps"), "{state}");
        assert!(state.contains("alive"), "{state}");
    }

    #[tokio::test]
    async fn a_member_with_no_text_anywhere_is_skipped_and_counted() {
        let decider = anything();
        let core = core_with(decider.clone()).await;
        let ids = seed(&core, &[("alive", [1.0, 0.0]), ("gone", [1.0, 0.0])]).await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_state(p.id, PairState::Contradiction, None, DecidedBy::Model)
            .await
            .unwrap();
        bury(&core, &ids[1], None).await;

        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        assert_eq!(r.len(), 2, "judged, then unjudged neighbours");
        assert_eq!((r[0].asked, r[0].skipped_no_text), (0, 1));
        assert_eq!(decider.calls(), 0);
    }

    #[tokio::test]
    async fn a_member_too_long_alone_is_counted_and_never_asked() {
        let decider = anything();
        decider.set_context_tokens(600);
        let core = core_with(decider.clone()).await;
        let long = "word ".repeat(2000);
        let ids = seed(&core, &[(long.as_str(), [1.0, 0.0]), ("short", [1.0, 0.0])]).await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_state(p.id, PairState::Contradiction, None, DecidedBy::Model)
            .await
            .unwrap();

        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        assert_eq!((r[0].asked, r[0].skipped_too_long), (0, 1));
        assert_eq!(decider.calls(), 0);
    }

    #[tokio::test]
    async fn a_burial_is_a_no_case_and_a_rescue_a_yes_case_for_reap_worth() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[("buried note", [1.0, 0.0]), ("rescued note", [0.0, 1.0])],
        )
        .await;
        bury(&core, &ids[0], Some("buried words")).await;
        core.store
            .record_action(&crate::jobs::reap::test_support::row(&ids[0]))
            .await
            .unwrap();
        let rescue = core
            .store
            .insert_synthesized_artifact(
                &crate::store::artifacts::NewSynthesized {
                    text: "the rewrite".into(),
                    title: Some("Rewrite".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["reap rescue: it holds a port".into()],
                },
                std::slice::from_ref(&ids[1]),
            )
            .await
            .unwrap();
        core.store
            .set_superseded_by(&ids[1], Some(&rescue.id))
            .await
            .unwrap();

        let (cases, _) = cases(&core, &catalogue::reap_worth(), 200).await.unwrap();
        let by: HashMap<&str, &Case> = cases.iter().map(|c| (c.subjects.0.as_str(), c)).collect();
        assert_eq!(by.len(), 2, "{cases:?}");
        assert_eq!(by[ids[0].as_str()].past.as_deref(), Some("no"));
        assert!(
            by[ids[0].as_str()]
                .state
                .as_deref()
                .unwrap()
                .contains("buried words")
        );
        let r = by[ids[1].as_str()];
        assert_eq!(r.past.as_deref(), Some("yes"));
        assert!(
            !r.state.as_deref().unwrap().contains("the rewrite"),
            "the rescue it led to is not shown as evidence against it"
        );
    }

    #[tokio::test]
    async fn judged_links_map_onto_the_new_kinds() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.0]),
                ("c", [1.0, 0.0]),
                ("d", [1.0, 0.0]),
                ("e", [1.0, 0.0]),
                ("f", [1.0, 0.0]),
                ("g", [1.0, 0.0]),
                ("h", [1.0, 0.0]),
                ("i", [1.0, 0.0]),
                ("j", [1.0, 0.0]),
            ],
        )
        .await;
        let now = crate::store::now();
        let mut keys = Vec::new();
        for (i, (state, reason)) in [
            (Some(LinkState::Related), Some("one builds on the other")),
            (
                Some(LinkState::Related),
                Some("same content; handed to consolidation"),
            ),
            (Some(LinkState::Unrelated), Some("nothing in common")),
            (Some(LinkState::Related), Some("one explains the other")),
            (None, None),
        ]
        .into_iter()
        .enumerate()
        {
            let (a, b) = crate::store::links::canonical(&ids[2 * i], &ids[2 * i + 1]);
            core.store
                .bump_link(a, b, 1.0, Some("mount loop"), 30.0, now)
                .await
                .unwrap();
            if let Some(state) = state {
                core.store
                    .set_link_state(a, b, state, reason, Some((0, 0)))
                    .await
                    .unwrap();
            }
            keys.push(key(a, b));
        }
        let (a, b) = keys[3].split_once('|').unwrap();
        core.store.dismiss_link(a, b).await.unwrap();

        let (cases, _) = cases(&core, &catalogue::link_kind(), 200).await.unwrap();
        let by: HashMap<String, &Case> = cases
            .iter()
            .map(|c| (key(&c.subjects.0, c.subjects.1.as_deref().unwrap()), c))
            .collect();
        assert_eq!(by.len(), 4, "a learning link is no case: {cases:?}");
        let related = "continues|example|alternative|background";
        assert_eq!(by[&keys[0]].past.as_deref(), Some(related));
        assert_eq!(by[&keys[1]].past.as_deref(), Some("duplicate"));
        assert_eq!(by[&keys[2]].past.as_deref(), Some("unrelated"));
        assert_eq!(by[&keys[3]].past.as_deref(), Some(related));
        assert_eq!(
            by[&keys[3]].person,
            Some(PersonSaid::Disagreed),
            "dismissed by a person"
        );
        assert!(
            by[&keys[0]]
                .state
                .as_deref()
                .unwrap()
                .contains("mount loop")
        );
        assert_eq!(by[&keys[0]].person, None);
    }

    #[tokio::test]
    async fn category_is_compared_with_the_stored_one() {
        let core = core_with(rule(|_, q| pick(q, "procedure", 0.8))).await;
        let ids = seed(
            &core,
            &[("steps", [1.0, 0.0]), ("uncategorised", [1.0, 0.0])],
        )
        .await;
        core.store
            .update_artifact_category_by(&ids[0], Some("procedure"), "writer")
            .await
            .unwrap();

        let r = run(&core, Some("category"), 200).await.unwrap();
        assert_eq!(r[0].asked, 1);
        assert_eq!(r[0].agrees_with_past, Some((1, 1)));
    }

    #[tokio::test]
    async fn questions_with_no_past_verdict_report_their_extremes() {
        let core = core_with(rule(|state, _| {
            let n: f64 = state
                .split_whitespace()
                .last()
                .and_then(|w| w.parse().ok())
                .unwrap_or(0.0);
            Answer::Noul { yes: n / 100.0 }
        }))
        .await;
        let rows: Vec<String> = (0..25).map(|i| format!("note {}", i * 4)).collect();
        let titled: Vec<(String, String)> =
            rows.iter().map(|t| (format!("T{t}"), t.clone())).collect();
        let refs: Vec<(&str, &str, [f32; 2])> = titled
            .iter()
            .map(|(t, x)| (t.as_str(), x.as_str(), [1.0, 0.0]))
            .collect();
        seed_titled(&core, &refs).await;

        let r = run(&core, Some("self_contained"), 200).await.unwrap();
        assert_eq!(r[0].asked, 25);
        assert_eq!(r[0].agrees_with_past, None);
        assert_eq!(r[0].suggested, None);
        assert_eq!(r[0].extremes.len(), 20);
        assert_eq!(r[0].extremes[0], (0.96, "Tnote 96".to_string()));
        assert_eq!(r[0].extremes[19], (0.0, "Tnote 0".to_string()));
    }

    #[tokio::test]
    async fn the_report_counts_agreement_bands_and_swaps() {
        let core = core_with(rule(|state, q| {
            let twin = q.key.ends_with('r');
            if state.contains("alpha") {
                pick(q, "conflict", 0.95)
            } else if state.contains("beta") {
                pick(q, "conflict", 0.70)
            } else if twin {
                pick(q, "duplicate", 0.92)
            } else {
                pick(q, "distinct", 0.92)
            }
        }))
        .await;
        let ids = seed(
            &core,
            &[
                ("alpha one", [1.0, 0.0]),
                ("alpha two", [1.0, 0.0]),
                ("beta one", [1.0, 0.0]),
                ("beta two", [1.0, 0.0]),
                ("gamma one", [1.0, 0.0]),
                ("gamma two", [1.0, 0.0]),
            ],
        )
        .await;
        for i in 0..3 {
            let p = pair(&core, &ids[2 * i], &ids[2 * i + 1]).await;
            core.store
                .set_pair_state(p.id, PairState::Contradiction, None, DecidedBy::Model)
                .await
                .unwrap();
        }

        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        assert_eq!(
            r[..1],
            [QuestionReport {
                id: "pair_relation".into(),
                version: catalogue::pair_relation().version,
                asked: 3,
                skipped_too_long: 0,
                skipped_no_text: 0,
                skipped_failed: 0,
                stopped: None,
                left_out: 0,
                agrees_with_past: Some((2, 3)),
                agrees_with_person: None,
                bands: [1, 1, 1],
                by_top: vec![(0.95, 1, 1), (0.70, 1, 1), (0.0, 1, 0)],
                swap_disagreed: 1,
                extremes: vec![],
                suggested: None,
                scope: None,
                confident: vec![],
                picks: vec![("conflict".into(), 2), ("duplicate".into(), 1)],
                past_picks: vec![(
                    "conflict".into(),
                    vec![("conflict".into(), 2), ("duplicate".into(), 1)]
                )],
                reading: None,
                exchanged_holds: None,
                disagreements: vec![Miss {
                    top: 0.468,
                    past: "conflict".into(),
                    pick: "duplicate".into(),
                    title: "untitled ↔ untitled".into(),
                }],
                bands_at: Thresholds {
                    act_at: 0.90,
                    ask_at: 0.60
                },
            }]
        );
    }

    #[tokio::test]
    async fn the_limit_takes_the_newest_and_counts_the_rest() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("old", [1.0, 0.0]),
                ("middle", [1.0, 0.0]),
                ("new", [1.0, 0.0]),
            ],
        )
        .await;
        for (id, at) in ids.iter().zip([100_i64, 200, 300]) {
            sqlx::query!("UPDATE artifacts SET created_at = ? WHERE id = ?", at, id)
                .execute(&core.store.pool)
                .await
                .unwrap();
        }

        let (cases, left_out) = cases(&core, &catalogue::pinned(), 2).await.unwrap();
        let got: Vec<&str> = cases.iter().map(|c| c.subjects.0.as_str()).collect();
        assert_eq!(got, [ids[2].as_str(), ids[1].as_str()]);
        assert_eq!(left_out, 1);
        let r = run(&core, Some("pinned"), 2).await.unwrap();
        assert_eq!((r[0].asked, r[0].left_out), (2, 1));
    }

    #[tokio::test]
    async fn the_replay_changes_nothing() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [0.0, 1.0]),
            ],
        )
        .await;
        let merged = pair(&core, &ids[0], &ids[1]).await;
        core.store
            .set_pair_merged(merged.id, "m1", None, DecidedBy::Model)
            .await
            .unwrap();
        core.store
            .record_action(&dedupe_row(Kind::Merge, &merged, &ids[0], Some("m1")))
            .await
            .unwrap();
        let (a, b) = crate::store::links::canonical(&ids[1], &ids[2]);
        core.store
            .bump_link(a, b, 1.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(a, b, LinkState::Related, Some("steps"), Some((0, 0)))
            .await
            .unwrap();
        core.store
            .update_artifact_category_by(&ids[2], Some("procedure"), "writer")
            .await
            .unwrap();
        bury(&core, &ids[3], Some("buried")).await;
        core.store
            .record_action(&crate::jobs::reap::test_support::row(&ids[3]))
            .await
            .unwrap();

        async fn counts(core: &Core) -> Vec<i64> {
            let mut out = Vec::new();
            for t in [
                "artifacts",
                "artifact_pairs",
                "artifact_links",
                "corpus_actions",
                "decisions",
                "graveyard",
            ] {
                // runtime: one count per table name, in a test.
                let n: i64 =
                    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {t}")))
                        .fetch_one(&core.store.pool)
                        .await
                        .unwrap();
                out.push(n);
            }
            out
        }
        let before = counts(&core).await;
        let pairs_before = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        let r = run(&core, None, 200).await.unwrap();
        assert!(r.iter().map(|q| q.asked).sum::<usize>() > 0, "{r:?}");
        assert_eq!(counts(&core).await, before);
        let pairs_after = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (pairs_before.state, pairs_before.merged_into),
            (pairs_after.state, pairs_after.merged_into)
        );
        assert_eq!(
            r.len(),
            catalogue::all().len() + 2,
            "every question, and two unjudged sections"
        );
    }

    #[tokio::test]
    async fn an_unknown_question_is_refused_before_any_call() {
        let decider = anything();
        let core = core_with(decider.clone()).await;
        seed(&core, &[("one", [1.0, 0.0])]).await;
        let e = run(&core, Some("nope"), 200).await.unwrap_err().to_string();
        assert!(
            e.contains("pair_relation") && e.contains("answers_query"),
            "{e}"
        );
        assert_eq!(decider.calls(), 0);
    }

    #[test]
    fn the_text_report_reads_as_the_plan_draws_it() {
        let r = QuestionReport {
            id: "pair_relation".into(),
            version: 1,
            asked: 183,
            skipped_too_long: 9,
            skipped_no_text: 4,
            left_out: 0,
            agrees_with_past: Some((151, 183)),
            agrees_with_person: Some((11, 14)),
            bands: [61, 88, 34],
            by_top: vec![(0.95, 40, 40), (0.90, 21, 20)],
            swap_disagreed: 12,
            extremes: vec![],
            suggested: Some(Thresholds {
                act_at: 0.86,
                ask_at: 0.62,
            }),
            scope: None,
            confident: vec![],
            picks: vec![],
            past_picks: vec![],
            reading: None,
            exchanged_holds: None,
            disagreements: vec![],
            skipped_failed: 0,
            stopped: None,
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        assert_eq!(
            render(&[r]),
            "pair_relation v1 · 183 asked · 9 too long · 4 without text · 0 left out
  agrees with the past verdict   151 of 183
  agrees with the person          11 of 14
  swapped reads disagreed         12
  would act 61 · ask 88 · leave 34      (act_at 0.90, ask_at 0.60)
  top     cases  agree
  0.95       40     40
  0.90       21     20
  suggested   act_at 0.86   ask_at 0.62
"
        );
    }

    #[test]
    fn a_question_with_no_past_verdict_shows_its_extremes_and_no_agreement() {
        let r = QuestionReport {
            id: "time_bound".into(),
            version: 1,
            asked: 2,
            skipped_too_long: 0,
            skipped_no_text: 0,
            left_out: 0,
            agrees_with_past: None,
            agrees_with_person: None,
            bands: [1, 0, 1],
            by_top: vec![(0.95, 1, 0), (0.50, 1, 0)],
            swap_disagreed: 0,
            extremes: vec![(0.97, "Port moved".into()), (0.48, "Spuren".into())],
            suggested: None,
            scope: None,
            confident: vec![],
            picks: vec![],
            past_picks: vec![],
            reading: Some((0.5, 1)),
            exchanged_holds: None,
            disagreements: vec![],
            skipped_failed: 0,
            stopped: None,
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        assert_eq!(
            render(&[r]),
            "time_bound v1 · 2 asked · 0 too long · 0 without text · 0 left out
  at 0.50   yes 1 of 2
  top     cases
  0.95        1
  0.50        1
  highest and lowest yes
  0.97  Port moved
  0.48  Spuren
"
        );
    }

    #[tokio::test]
    async fn a_pair_consolidate_closed_as_near_identical_is_not_a_case() {
        let core = core_with(anything()).await;
        let ids = seed(&core, &[("one", [1.0, 0.0]), ("one.", [1.0, 0.0])]).await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        let detail = format!(
            "{}; {} kept",
            crate::jobs::consolidate::NEAR_IDENTICAL,
            ids[0]
        );
        core.store
            .set_pair_state(p.id, PairState::NoConflict, Some(&detail), DecidedBy::Model)
            .await
            .unwrap();
        let (cases, _) = cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        assert!(
            cases.is_empty(),
            "a heuristic closed it, not a judgement: {cases:?}"
        );
    }

    #[tokio::test]
    async fn the_window_readings_are_asked_over_the_newest_live_notes() {
        let core = core_with(anything()).await;
        seed_titled(
            &core,
            &[
                ("Call", "remind me to call Anna", [1.0, 0.0]),
                ("Ports", "use 8080", [1.0, 0.0]),
            ],
        )
        .await;
        for id in ["asks_reminder", "has_date"] {
            let r = run(&core, Some(id), 200).await.unwrap();
            assert_eq!(r[0].asked, 2, "{id}");
            assert_eq!(r[0].agrees_with_past, None, "{id}");
            assert_eq!(r[0].extremes.len(), 2, "{id}");
        }
    }

    #[test]
    fn answerable_says_plainly_that_it_is_not_asked() {
        let r = QuestionReport {
            id: "answerable".into(),
            version: 1,
            asked: 0,
            skipped_too_long: 0,
            skipped_no_text: 0,
            left_out: 0,
            agrees_with_past: None,
            agrees_with_person: None,
            bands: [0, 0, 0],
            by_top: vec![],
            swap_disagreed: 0,
            extremes: vec![],
            suggested: None,
            scope: None,
            confident: vec![],
            picks: vec![],
            past_picks: vec![],
            reading: None,
            exchanged_holds: None,
            disagreements: vec![],
            skipped_failed: 0,
            stopped: None,
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        assert_eq!(
            render(&[r]),
            "answerable v1 · 0 asked · 0 too long · 0 without text · 0 left out
  not asked: the base keeps nothing this question can be replayed over
"
        );
    }

    // Real backoff, kept short: a paused clock starves the SQLite pool.
    #[tokio::test]
    async fn a_busy_endpoint_is_waited_out_and_the_run_finishes() {
        let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = busy.clone();
        let decider = Arc::new(RuleDecider::new(move |_, qs| {
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                return Err(crate::error::Error::InferenceBusy {
                    role: "decide",
                    detail: "HTTP 429 Too Many Requests".into(),
                });
            }
            Ok(Answers {
                answers: qs
                    .iter()
                    .map(|q| (q.key.clone(), Answer::Noul { yes: 0.9 }))
                    .collect(),
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }));
        let core = core_with(decider.clone()).await;
        seed(&core, &[("one", [1.0, 0.0]), ("two", [1.0, 0.0])]).await;
        let r = run(&core, Some("pinned"), 200).await.unwrap();
        assert_eq!(r[0].asked, 2);
        assert_eq!(decider.calls(), 4, "two refusals, then both asked");
    }

    #[test]
    fn tops_under_a_half_get_their_own_row() {
        let r = QuestionReport {
            id: "pair_relation".into(),
            version: 1,
            asked: 2,
            skipped_too_long: 0,
            skipped_no_text: 0,
            left_out: 0,
            agrees_with_past: Some((1, 2)),
            agrees_with_person: None,
            bands: [0, 0, 2],
            by_top: vec![(0.50, 1, 1), (0.0, 1, 0)],
            swap_disagreed: 1,
            extremes: vec![],
            suggested: None,
            scope: None,
            confident: vec![],
            picks: vec![],
            past_picks: vec![],
            reading: None,
            exchanged_holds: None,
            disagreements: vec![],
            skipped_failed: 0,
            stopped: None,
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        let text = render(&[r]);
        assert!(
            text.contains("\n  0.50        1      1\n  <0.50       1      0\n"),
            "{text}"
        );
        assert_eq!(
            suggest(&[(0.80, 40, 40), (0.0, 100, 0)]).unwrap().act_at,
            0.80,
            "counted from 0.50 up"
        );
    }

    #[tokio::test]
    async fn rewrites_the_base_took_back_itself_are_left_out() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.0]),
                ("m1", [1.0, 0.0]),
                ("c", [1.0, 0.0]),
                ("d", [1.0, 0.0]),
                ("m2", [1.0, 0.0]),
                ("long note here", [1.0, 0.0]),
            ],
        )
        .await;
        let row = |subject: &str, survivor: &str| NewAction {
            job: Job::Dedupe,
            kind: Kind::Merge,
            subject_id: subject.into(),
            survivor_id: Some(survivor.into()),
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: None,
        };
        for (s, m) in [(0, 2), (1, 2), (3, 5), (4, 5)] {
            core.store
                .record_action(&row(&ids[s], &ids[m]))
                .await
                .unwrap();
        }
        core.store
            .undo_action_on(&ids[0], Kind::Merge, UndoneBy::Evidence, "use showed")
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[3], Kind::Merge, UndoneBy::Operator, "put back")
            .await
            .unwrap();
        core.store
            .condense_artifact(&ids[6], None, "short", None, &[], serde_json::json!({}))
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[6], Kind::Condense, UndoneBy::Evidence, "use showed")
            .await
            .unwrap();

        let (cases, _) = cases(&core, &catalogue::complete(), 200).await.unwrap();
        assert_eq!(cases.len(), 1, "{cases:?}");
        assert_eq!(cases[0].subjects.0, ids[5]);
        assert_eq!(cases[0].person, Some(PersonSaid::Disagreed));
    }

    async fn titled_at(core: &Core, rows: &[(&str, [f32; 2])]) -> Vec<String> {
        let rows: Vec<(&str, &str, [f32; 2])> = rows.iter().map(|(t, v)| (*t, *t, *v)).collect();
        seed_titled(core, &rows).await
    }

    #[tokio::test]
    async fn unjudged_neighbours_are_found_once_and_filed_pairs_are_not() {
        let core = core_with(anything()).await;
        let ids = titled_at(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.1]),
                ("c", [0.0, 1.0]),
                ("d", [0.1, 1.0]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[2], &ids[3], 0.99)
            .await
            .unwrap();

        let (cases, left_out) = unjudged_cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        let got: Vec<(String, Option<String>)> = cases.iter().map(|c| c.subjects.clone()).collect();
        let (x, y) = crate::store::links::canonical(&ids[0], &ids[1]);
        assert_eq!(
            got,
            [(x.to_string(), Some(y.to_string()))],
            "a↔b once; c↔d is filed"
        );
        assert_eq!(left_out, 0);
        assert!(cases[0].past.is_none() && cases[0].person.is_none());
        let state = cases[0].state.as_deref().unwrap();
        assert!(
            state.starts_with("A: ") && state.contains("(captured "),
            "{state}"
        );
    }

    #[tokio::test]
    async fn a_pair_under_the_net_is_not_asked() {
        let decider = anything();
        let core = core_with(decider.clone()).await;
        titled_at(&core, &[("a", [1.0, 0.0]), ("e", [0.6, 0.8])]).await;
        let (cases, _) = unjudged_cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        assert!(cases.is_empty(), "cosine 0.60 is under scan_min: {cases:?}");
    }

    #[tokio::test]
    async fn the_unjudged_limit_takes_the_newest_and_counts_the_rest() {
        let core = core_with(anything()).await;
        let ids = titled_at(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.0]),
                ("c", [0.0, 1.0]),
                ("d", [0.0, 1.0]),
            ],
        )
        .await;
        for (id, at) in ids.iter().zip([100_i64, 100, 300, 300]) {
            sqlx::query!("UPDATE artifacts SET created_at = ? WHERE id = ?", at, id)
                .execute(&core.store.pool)
                .await
                .unwrap();
        }
        let (cases, left_out) = unjudged_cases(&core, &catalogue::pair_relation(), 1)
            .await
            .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].subjects.0, ids[2].clone().min(ids[3].clone()));
        assert_eq!(left_out, 1);
    }

    #[tokio::test]
    async fn the_most_confident_are_listed_per_pick_with_titles_and_cosine() {
        let core = core_with(rule(|state, q| {
            if state.contains("alpha") {
                pick(q, "duplicate", 0.9)
            } else {
                pick(q, "distinct", 0.8)
            }
        }))
        .await;
        titled_at(
            &core,
            &[
                ("alpha one", [1.0, 0.0]),
                ("alpha two", [1.0, 0.0]),
                ("beta one", [0.0, 1.0]),
                ("beta two", [0.0, 1.0]),
            ],
        )
        .await;

        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        let u = &r[1];
        assert_eq!(u.scope.as_deref(), Some("unjudged neighbours"));
        assert_eq!(u.asked, 2);
        let got: Vec<(&str, f64, &str)> = u
            .confident
            .iter()
            .map(|c| (c.pick.as_str(), c.top, c.title.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("duplicate", 0.9, "alpha one ↔ alpha two"),
                ("distinct", 0.8, "beta one ↔ beta two")
            ]
        );
        assert!(
            u.confident
                .iter()
                .all(|c| c.similarity.is_some_and(|s| s > 0.99))
        );
        assert!(u.extremes.is_empty());
    }

    #[test]
    fn at_most_five_are_listed_per_pick() {
        let rows: Vec<Row> = (0..7)
            .map(|i| Row {
                pick: "distinct".into(),
                top: 0.5 + f64::from(i) / 20.0,
                swapped_agree: Some(true),
                band: Band::Leave,
                past: None,
                person: None,
            })
            .collect();
        let titles: Vec<String> = (0..7).map(|i| format!("t{i}")).collect();
        let sims = vec![Some(0.8); 7];
        let r = summarize(
            &catalogue::pair_relation(),
            &rows,
            &titles,
            &sims,
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            Some("unjudged neighbours"),
        );
        let tops: Vec<&str> = r.confident.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(tops, ["t6", "t5", "t4", "t3", "t2"]);
    }

    #[tokio::test]
    async fn superseded_notes_are_unjudged_reap_cases_with_their_successor() {
        let core = core_with(anything()).await;
        let ids = titled_at(&core, &[("old port", [1.0, 0.0]), ("new port", [1.0, 0.0])]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        let (cases, _) = unjudged_cases(&core, &catalogue::reap_worth(), 200)
            .await
            .unwrap();
        assert_eq!(cases.len(), 1, "{cases:?}");
        assert_eq!(cases[0].subjects, (ids[0].clone(), Some(ids[1].clone())));
        let state = cases[0].state.as_deref().unwrap();
        assert!(
            state.contains("Retired note: old port") && state.contains("Its replacement: new port"),
            "{state}"
        );

        let r = run(&core, Some("reap_worth"), 200).await.unwrap();
        assert_eq!(r.len(), 2);
        let u = &r[1];
        assert_eq!(u.scope.as_deref(), Some("unjudged retired notes"));
        assert_eq!(
            (u.asked, u.agrees_with_past, u.agrees_with_person),
            (1, None, None)
        );
        assert_eq!(u.suggested, None);
        assert_eq!(u.extremes, [(0.9, "old port".to_string())]);
    }

    #[tokio::test]
    async fn the_unjudged_sections_carry_no_agreement_and_no_suggestion() {
        let core = core_with(anything()).await;
        titled_at(&core, &[("a", [1.0, 0.0]), ("b", [1.0, 0.0])]).await;
        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        assert_eq!(r[0].scope, None, "the judged section comes first");
        let u = &r[1];
        assert_eq!(u.asked, 1);
        assert_eq!(
            (u.agrees_with_past, u.agrees_with_person, u.suggested),
            (None, None, None)
        );
        let text = render(&r[1..]);
        assert!(
            !text.contains("agrees with")
                && !text.contains("cases  agree")
                && !text.contains("suggested"),
            "{text}"
        );
    }

    #[test]
    fn an_unjudged_section_reads_with_its_scope_and_most_confident() {
        let r = QuestionReport {
            id: "pair_relation".into(),
            version: 1,
            asked: 2,
            skipped_too_long: 0,
            skipped_no_text: 0,
            left_out: 3,
            agrees_with_past: None,
            agrees_with_person: None,
            bands: [1, 0, 1],
            by_top: vec![(0.90, 1, 0), (0.80, 1, 0)],
            swap_disagreed: 0,
            extremes: vec![],
            suggested: None,
            scope: Some("unjudged neighbours".into()),
            confident: vec![
                Confident {
                    pick: "duplicate".into(),
                    top: 0.93,
                    title: "Ports ↔ Ports v2".into(),
                    similarity: Some(0.91),
                },
                Confident {
                    pick: "distinct".into(),
                    top: 0.81,
                    title: "Spuren ↔ Hash".into(),
                    similarity: Some(0.72),
                },
            ],
            picks: vec![],
            past_picks: vec![],
            reading: None,
            exchanged_holds: None,
            disagreements: vec![],
            skipped_failed: 0,
            stopped: None,
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        assert_eq!(
            render(&[r]),
            "pair_relation v1 · unjudged neighbours · 2 asked · 0 too long · 0 without text · 3 left out
  swapped reads disagreed          0
  would act 1 · ask 0 · leave 1         (act_at 0.90, ask_at 0.60)
  top     cases
  0.90        1
  0.80        1
  most confident
  duplicate     0.93  Ports ↔ Ports v2  (cosine 0.91)
  distinct      0.81  Spuren ↔ Hash  (cosine 0.72)
"
        );
    }

    fn row(pick: &str, top: f64, past: Option<&str>) -> Row {
        Row {
            pick: pick.into(),
            top,
            swapped_agree: Some(true),
            band: Band::Leave,
            past: past.map(String::from),
            person: None,
        }
    }

    #[test]
    fn the_picks_are_counted_and_broken_down_by_past_verdict() {
        let rows = [
            row("background", 0.9, Some(RELATED)),
            row("background", 0.8, Some(RELATED)),
            row("unrelated", 0.7, Some(RELATED)),
            row("unrelated", 0.9, Some("unrelated")),
        ];
        let titles = vec!["t".to_string(); 4];
        let r = summarize(
            &catalogue::link_kind(),
            &rows,
            &titles,
            &[None; 4],
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            None,
        );
        assert_eq!(
            r.picks,
            [("background".to_string(), 2), ("unrelated".to_string(), 2)]
        );
        assert_eq!(
            r.past_picks,
            [
                (
                    "related".to_string(),
                    vec![("background".to_string(), 2), ("unrelated".to_string(), 1)]
                ),
                ("unrelated".to_string(), vec![("unrelated".to_string(), 1)]),
            ]
        );
        let text = render(&[r]);
        assert!(
            text.contains(
                "  0.70        1      0\n  picks   background 2 · unrelated 2\n  past related      background 2 · unrelated 1\n  past unrelated    unrelated 1\n  disagrees  0.70  past related → unrelated  t\n  suggested"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_noul_names_yes_then_no_and_a_reading_shows_its_fixed_line_not_bands() {
        let rows = [row("yes", 0.95, None), row("no", 0.7, None)];
        let titles = vec!["Port moved".to_string(), "Spuren".to_string()];
        let r = summarize(
            &catalogue::pinned(),
            &rows,
            &titles,
            &[None; 2],
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            None,
        );
        assert_eq!(r.picks, [("yes".to_string(), 1), ("no".to_string(), 1)]);
        assert_eq!(r.reading, Some((0.90, 1)));
        let text = render(&[r]);
        assert!(!text.contains("would act"), "{text}");
        assert!(
            text.contains("\n  at 0.90   yes 1 of 2\n  top     cases\n"),
            "{text}"
        );
        assert!(
            text.contains("  picks   yes 1 · no 1\n  highest and lowest yes\n"),
            "{text}"
        );
        for (id, at) in [
            ("time_bound", 0.5),
            ("self_contained", 0.5),
            ("asks_reminder", 0.3),
            ("has_date", 0.3),
        ] {
            let r = summarize(
                &catalogue::by_id(id).unwrap(),
                &rows,
                &titles,
                &[None; 2],
                Thresholds {
                    act_at: 0.9,
                    ask_at: 0.6,
                },
                (0, 0, 0),
                None,
            );
            assert_eq!(r.reading.map(|x| x.0), Some(at), "{id}");
        }
    }

    #[test]
    fn category_shows_its_picks_and_no_bands() {
        let rows = [
            row("procedure", 0.9, Some("procedure")),
            row("concept", 0.6, Some("procedure")),
        ];
        let titles = vec!["t".to_string(); 2];
        let r = summarize(
            &catalogue::category(),
            &rows,
            &titles,
            &[None; 2],
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            None,
        );
        assert_eq!(r.reading, None);
        let text = render(&[r]);
        assert!(
            !text.contains("would act") && !text.contains("  at "),
            "{text}"
        );
        assert!(
            text.contains(
                "  picks   concept 1 · procedure 1\n  past procedure    concept 1 · procedure 1\n"
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_two_note_choice_lists_both_titles_as_most_and_least_sure() {
        let core = core_with(rule(|_, q| pick(q, "a_first", 0.8))).await;
        let ids = titled_at(&core, &[("Steps", [1.0, 0.0]), ("Why", [1.0, 0.0])]).await;
        let (a, b) = crate::store::links::canonical(&ids[0], &ids[1]);
        core.store
            .bump_link(a, b, 1.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(a, b, LinkState::Related, Some("steps"), Some((0, 0)))
            .await
            .unwrap();

        let r = run(&core, Some("link_direction"), 200).await.unwrap();
        assert_eq!(r[0].extremes, [(0.8, "a_first · Steps ↔ Why".to_string())]);
        let text = render(&r);
        assert!(
            text.contains("  most and least sure\n  0.80  a_first · Steps ↔ Why\n"),
            "{text}"
        );
    }

    async fn a_related_link(core: &Core) {
        let ids = titled_at(core, &[("Steps", [1.0, 0.0]), ("Why", [1.0, 0.0])]).await;
        let (a, b) = crate::store::links::canonical(&ids[0], &ids[1]);
        core.store
            .bump_link(a, b, 1.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();
        core.store
            .set_link_state(a, b, LinkState::Related, Some("steps"), Some((0, 0)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_direction_drawn_to_the_first_note_does_not_hold_when_the_notes_are_exchanged() {
        let decider = rule(|_, q| pick(q, "a_first", 0.8));
        let core = core_with(decider.clone()).await;
        a_related_link(&core).await;
        let r = run(&core, Some("link_direction"), 200).await.unwrap();
        assert_eq!(r[0].exchanged_holds, Some((0, 1)));
        assert_eq!(
            r[0].picks,
            [("a_first".to_string(), 1)],
            "picks describe the first read"
        );
        assert_eq!(decider.calls(), 2, "each case is asked twice");
        assert!(render(&r).contains("  swapped reads disagreed          0\n  holds when the notes are exchanged   0 of 1\n"), "{}", render(&r));
    }

    #[tokio::test]
    async fn a_direction_that_follows_the_text_holds_when_the_notes_are_exchanged() {
        let decider = rule(|state, q| {
            if state.starts_with("A: Steps") {
                pick(q, "a_first", 0.8)
            } else {
                pick(q, "b_first", 0.8)
            }
        });
        let core = core_with(decider.clone()).await;
        a_related_link(&core).await;
        let r = run(&core, Some("link_direction"), 200).await.unwrap();
        assert_eq!(r[0].exchanged_holds, Some((1, 1)));
        let other = run(&core, Some("link_kind"), 200).await.unwrap();
        assert_eq!(
            other[0].exchanged_holds, None,
            "only link_direction is exchanged"
        );
    }

    #[test]
    fn the_most_confident_disagreements_are_listed_with_past_and_pick() {
        let mut rows: Vec<Row> = (0..12)
            .map(|i| row("unrelated", 0.6 + f64::from(i) / 50.0, Some(RELATED)))
            .collect();
        rows.push(row("background", 0.99, Some(RELATED)));
        let titles: Vec<String> = (0..13).map(|i| format!("A{i} ↔ B{i}")).collect();
        let r = summarize(
            &catalogue::link_kind(),
            &rows,
            &titles,
            &[None; 13],
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            None,
        );
        assert_eq!(r.disagreements.len(), 10);
        let first = &r.disagreements[0];
        assert_eq!(
            (
                first.past.as_str(),
                first.pick.as_str(),
                first.title.as_str()
            ),
            ("related", "unrelated", "A11 ↔ B11")
        );
        assert!((first.top - 0.82).abs() < 1e-9);
        assert!(
            r.disagreements.iter().all(|d| d.pick != "background"),
            "an agreement is no disagreement"
        );
        let text = render(&[r]);
        assert!(text.contains("  past related      unrelated 12 · background 1\n  disagrees  0.82  past related → unrelated  A11 ↔ B11\n"), "{text}");
        let u = summarize(
            &catalogue::pair_relation(),
            &[row("distinct", 0.9, None)],
            &["x".to_string()],
            &[None],
            Thresholds {
                act_at: 0.9,
                ask_at: 0.6,
            },
            (0, 0, 0),
            Some("unjudged neighbours"),
        );
        assert!(u.disagreements.is_empty());
    }

    #[tokio::test]
    async fn a_trashed_note_is_no_unjudged_retired_case() {
        let core = core_with(anything()).await;
        let ids = titled_at(&core, &[("old", [1.0, 0.0]), ("new", [1.0, 0.0])]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        core.store.trash_artifact(&ids[0], 1).await.unwrap();
        let (cases, _) = unjudged_cases(&core, &catalogue::reap_worth(), 200)
            .await
            .unwrap();
        assert!(cases.is_empty(), "{cases:?}");
    }

    /// A `pair_relation` decision about `p` as the log keeps it once it is done.
    async fn decided(core: &Core, p: &ArtifactPair, pick: &str, band: Band) -> String {
        core.store
            .log_decision(&NewLogged {
                question_id: "pair_relation".into(),
                question_version: catalogue::pair_relation().version,
                model: "rule-decider".into(),
                subject_kind: "pair",
                subject_a: p.id.to_string(),
                subject_b: None,
                rev_a: None,
                rev_b: None,
                answer: "{}".into(),
                top: 0.9,
                pick: pick.into(),
                swapped_agree: Some(true),
                band,
                reason: None,
                state: State::Done,
            })
            .await
            .unwrap()
    }

    async fn pair_cases(core: &Core) -> HashMap<String, Case> {
        let (cases, _) = cases(core, &catalogue::pair_relation(), 200).await.unwrap();
        cases
            .into_iter()
            .map(|c| (key(&c.subjects.0, c.subjects.1.as_deref().unwrap()), c))
            .collect()
    }

    #[tokio::test]
    async fn a_persons_vacuous_answer_is_their_verdict_though_nothing_was_journaled() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("newer", [1.0, 0.0]),
            ],
        )
        .await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        // One side is behind another note already, so only the decision says what was answered.
        core.store
            .set_superseded_by(&p.a_id, Some(&ids[2]))
            .await
            .unwrap();
        let d = decided(&core, &p, "distinct", Band::Ask).await;
        crate::jobs::dedupe::carry_out(&core, &p, "vacuous", DecidedBy::Operator, &d)
            .await
            .unwrap();
        core.store
            .set_decision_outcome(&d, Outcome::Corrected, Some("vacuous"))
            .await
            .unwrap();
        assert!(core.store.recent_actions(10).await.unwrap().is_empty());

        let by = pair_cases(&core).await;
        let c = &by[&key(&p.a_id, &p.b_id)];
        assert_eq!(c.past.as_deref(), Some("vacuous"));
        assert_eq!(
            c.person,
            Some(PersonSaid::Agreed),
            "the person's own verdict"
        );
    }

    #[tokio::test]
    async fn a_pair_a_person_dismissed_is_vacuous_only_where_both_notes_were_retired() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [1.0, 0.0]),
            ],
        )
        .await;
        // As a base from before the decision log holds the two presses: no decision, no row.
        let retired = pair(&core, &ids[0], &ids[1]).await;
        crate::jobs::dedupe::discard_both(&core, &retired, None, DecidedBy::Operator, None)
            .await
            .unwrap();
        let left = pair(&core, &ids[2], &ids[3]).await;
        core.store
            .set_pair_state(left.id, PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();

        let by = pair_cases(&core).await;
        assert_eq!(
            by.len(),
            1,
            "a pair left alone says nothing of the two: {by:?}"
        );
        let c = &by[&key(&retired.a_id, &retired.b_id)];
        assert_eq!(c.past.as_deref(), Some("vacuous"));
        assert_eq!(c.person, Some(PersonSaid::Agreed));
    }

    #[tokio::test]
    async fn a_vacuous_pick_that_retired_one_note_is_vacuous_by_its_decision() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("newer", [1.0, 0.0]),
            ],
        )
        .await;
        let p = pair(&core, &ids[0], &ids[1]).await;
        // One side went behind another note while the decision waited: only the other is retired.
        core.store
            .set_superseded_by(&p.a_id, Some(&ids[2]))
            .await
            .unwrap();
        let d = decided(&core, &p, "vacuous", Band::Act).await;
        let rows = crate::jobs::dedupe::carry_out(&core, &p, "vacuous", DecidedBy::Model, &d)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        let by = pair_cases(&core).await;
        let c = &by[&key(&p.a_id, &p.b_id)];
        assert_eq!(c.past.as_deref(), Some("vacuous"));
        assert_eq!(c.person, None);
    }

    #[tokio::test]
    async fn a_refusal_is_known_by_the_decision_it_left_not_by_its_wording() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [1.0, 0.0]),
                ("three", [1.0, 0.0]),
                ("four", [1.0, 0.0]),
                ("five", [1.0, 0.0]),
                ("six", [1.0, 0.0]),
            ],
        )
        .await;
        // A guard no phrase names closed a sure `duplicate` with both kept.
        let refused = pair(&core, &ids[0], &ids[1]).await;
        decided(&core, &refused, "duplicate", Band::Act).await;
        let kept = pair(&core, &ids[2], &ids[3]).await;
        decided(&core, &kept, "distinct", Band::Act).await;
        // Only asked, and closed elsewhere: the decision carried out nothing.
        let asked = pair(&core, &ids[4], &ids[5]).await;
        decided(&core, &asked, "duplicate", Band::Ask).await;
        for p in [&refused, &kept, &asked] {
            core.store
                .set_pair_state(
                    p.id,
                    PairState::NoConflict,
                    Some("kept apart"),
                    DecidedBy::Model,
                )
                .await
                .unwrap();
        }

        let by = pair_cases(&core).await;
        assert!(
            !by.contains_key(&key(&refused.a_id, &refused.b_id)),
            "{by:?}"
        );
        for p in [&kept, &asked] {
            assert_eq!(by[&key(&p.a_id, &p.b_id)].past.as_deref(), Some("distinct"));
        }
    }

    /// Give a buried note the vector its point carried.
    async fn grave_vector(core: &Core, id: &str, v: &[f32]) {
        let (blob, model) = (
            crate::store::feedback::vec_to_blob(v),
            core.embedder.model().to_string(),
        );
        sqlx::query!(
            "UPDATE graveyard SET vec = ?, embed_model = ? WHERE id = ?",
            blob,
            model,
            id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_run_reads_the_journal_and_the_graveyard_vectors_once() {
        let core = core_with(anything()).await;
        let ids = seed(
            &core,
            &[
                ("still here", [1.0, 0.0]),
                ("first buried", [1.0, 0.0]),
                ("second buried", [1.0, 0.0]),
                ("buried without a vector", [1.0, 0.0]),
            ],
        )
        .await;
        for id in &ids[1..] {
            bury(&core, id, Some("buried words")).await;
        }
        for id in &ids[1..3] {
            grave_vector(&core, id, &[1.0, 0.0]).await;
        }
        let base = Base::default();
        let near = neighbours(&core, &base, &ids[1], None).await.unwrap();
        assert_eq!(near.len(), 1, "the one live note");
        assert_eq!(near[0].text, "still here");
        assert!(base.journal(&core).await.unwrap().is_empty());

        sqlx::query!("UPDATE graveyard SET vec = NULL")
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.store
            .record_action(&crate::jobs::reap::test_support::row(&ids[1]))
            .await
            .unwrap();
        let near = neighbours(&core, &base, &ids[2], None).await.unwrap();
        assert_eq!(near.len(), 1, "from the vectors read for the first");
        let none = neighbours(&core, &base, &ids[3], None).await.unwrap();
        assert!(none.is_empty(), "no vector was kept for it");
        assert!(
            base.journal(&core).await.unwrap().is_empty(),
            "every question reads the journal as the first one found it"
        );
    }

    /// Yes at 0.9 to the first `ok` calls, then an endpoint that is gone.
    fn gone_after(ok: usize) -> Arc<RuleDecider> {
        let calls = AtomicUsize::new(0);
        Arc::new(RuleDecider::new(move |_, qs| {
            if calls.fetch_add(1, Ordering::SeqCst) >= ok {
                return Err(Error::Inference {
                    role: "decide",
                    detail: "connection refused".into(),
                });
            }
            crate::jobs::reap::test_support::answers(qs, 0.9)
        }))
    }

    #[tokio::test]
    async fn a_dead_endpoint_ends_the_run_at_its_first_failed_call_with_what_was_asked() {
        let decider = gone_after(1);
        let core = core_with(decider.clone()).await;
        seed(
            &core,
            &[
                ("one", [1.0, 0.0]),
                ("two", [0.0, 1.0]),
                ("three", [-1.0, 0.0]),
            ],
        )
        .await;

        let r = run(&core, None, 200).await.unwrap();
        assert_eq!(decider.calls(), 2, "the failed call is the last one made");
        let (last, before) = r.split_last().unwrap();
        assert_eq!(last.id, "pinned", "the first question with a note to ask");
        assert_eq!((last.asked, last.skipped_failed), (1, 0));
        let why = last.stopped.as_deref().unwrap();
        assert!(why.contains("connection refused"), "{why}");
        assert!(before.iter().all(|q| q.stopped.is_none() && q.asked == 0));
        let text = render(&r);
        assert!(
            text.contains(" · 1 asked · ")
                && text.contains("\n  stopped: the decision endpoint did not answer"),
            "{text}"
        );
        assert!(!text.contains("time_bound"), "{text}");
    }

    #[tokio::test]
    async fn a_case_the_endpoint_refuses_is_counted_and_the_next_is_asked() {
        let decider = Arc::new(RuleDecider::new(|state, qs| {
            if state.contains("poison") {
                return Err(Error::InferenceRejected {
                    role: "decide",
                    detail: "HTTP 400".into(),
                });
            }
            crate::jobs::reap::test_support::answers(qs, 0.9)
        }));
        let core = core_with(decider.clone()).await;
        seed(
            &core,
            &[
                ("fine", [1.0, 0.0]),
                ("poison", [0.0, 1.0]),
                ("fine too", [-1.0, 0.0]),
            ],
        )
        .await;

        let r = run(&core, Some("pinned"), 200).await.unwrap();
        assert_eq!((r[0].asked, r[0].skipped_failed), (2, 1));
        assert_eq!(r[0].stopped, None);
        assert_eq!(decider.calls(), 3);
        let text = render(&r);
        assert!(
            text.contains(" · 2 asked · 0 too long · 0 without text · 1 failed · 0 left out\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn the_exchanged_read_waits_on_its_own_and_the_first_is_not_asked_again() {
        let calls = AtomicUsize::new(0);
        let decider = Arc::new(RuleDecider::new(move |_, qs| {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(Error::InferenceBusy {
                    role: "decide",
                    detail: "HTTP 429 Too Many Requests".into(),
                });
            }
            Ok(Answers {
                answers: qs
                    .iter()
                    .map(|q| (q.key.clone(), pick(q, "a_first", 0.8)))
                    .collect(),
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }));
        let core = core_with(decider.clone()).await;
        a_related_link(&core).await;

        let r = run(&core, Some("link_direction"), 200).await.unwrap();
        assert_eq!((r[0].asked, r[0].exchanged_holds), (1, Some((0, 1))));
        let states: Vec<String> = decider.requests().into_iter().map(|r| r.0).collect();
        assert_eq!(
            states.len(),
            3,
            "the first read, then the exchanged one twice"
        );
        assert_ne!(states[0], states[1]);
        assert_eq!(states[1], states[2]);
    }

    /// The store beneath, except that these notes' neighbours cannot be read.
    struct NoNeighbours {
        inner: Arc<dyn VectorStore>,
        of: Vec<String>,
    }

    #[async_trait::async_trait]
    impl VectorStore for NoNeighbours {
        async fn ensure_collection(&self, dim: usize) -> Result<()> {
            self.inner.ensure_collection(dim).await
        }
        async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
            self.inner.upsert(points).await
        }
        async fn set_payload(&self, payload: &VectorPayload) -> Result<()> {
            self.inner.set_payload(payload).await
        }
        async fn set_lifecycle(
            &self,
            artifact_id: &str,
            status: ArtifactStatus,
            superseded_by: Option<&str>,
        ) -> Result<()> {
            self.inner
                .set_lifecycle(artifact_id, status, superseded_by)
                .await
        }
        async fn set_last_verified_at(
            &self,
            artifact_id: &str,
            at: i64,
            reset_hits: bool,
        ) -> Result<()> {
            self.inner
                .set_last_verified_at(artifact_id, at, reset_hits)
                .await
        }
        async fn stale_candidates(
            &self,
            older_than: i64,
            max_hits: i64,
            limit: usize,
        ) -> Result<Vec<SearchHit>> {
            self.inner
                .stale_candidates(older_than, max_hits, limit)
                .await
        }
        async fn search(
            &self,
            vector: &[f32],
            sparse: &SparseVector,
            limit: usize,
            filter: &SearchFilter,
        ) -> Result<Vec<SearchHit>> {
            self.inner.search(vector, sparse, limit, filter).await
        }
        async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
            self.inner.touch(targets, seen_at).await
        }
        async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()> {
            self.inner.apply_lifecycle(rows).await
        }
        async fn lifecycle_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, StoredLifecycle>> {
            self.inner.lifecycle_of(artifact_ids).await
        }
        async fn all_artifact_ids(&self) -> Result<Vec<String>> {
            self.inner.all_artifact_ids().await
        }
        async fn payloads_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, VectorPayload>> {
            self.inner.payloads_of(artifact_ids).await
        }
        async fn resurface(
            &self,
            limit: usize,
            older_than: i64,
            unseen_since: i64,
        ) -> Result<Vec<SearchHit>> {
            self.inner.resurface(limit, older_than, unseen_since).await
        }
        async fn facets(&self, limit: usize) -> Result<Facets> {
            self.inner.facets(limit).await
        }
        async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
            if self.of.iter().any(|id| id == artifact_id) {
                return Err(Error::Vector("connection refused".into()));
            }
            self.inner.neighbours(artifact_id, limit).await
        }
        async fn set_context_vectors(
            &self,
            artifact_id: &str,
            vectors: Vec<Vec<f32>>,
        ) -> Result<()> {
            self.inner.set_context_vectors(artifact_id, vectors).await
        }
        async fn context_query(
            &self,
            vector: &[f32],
            limit: usize,
            filter: &SearchFilter,
        ) -> Result<Vec<SearchHit>> {
            self.inner.context_query(vector, limit, filter).await
        }
        async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
            self.inner.delete_artifacts(artifact_ids).await
        }
        async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
            self.inner.delete_by_corpus(corpus_id).await
        }
        async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
            self.inner.sample(limit).await
        }
        async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
            self.inner.dense_of(artifact_id).await
        }
        async fn count(&self) -> Result<u64> {
            self.inner.count().await
        }
    }

    fn without_neighbours_of(core: &mut Core, ids: &[&String]) {
        core.vectors = Arc::new(NoNeighbours {
            inner: core.vectors.clone(),
            of: ids.iter().map(|id| (*id).clone()).collect(),
        });
    }

    #[tokio::test]
    async fn a_retired_note_whose_neighbours_cannot_be_read_is_counted_and_the_rest_are_asked() {
        let decider = anything();
        let mut core = core_with(decider.clone()).await;
        let ids = titled_at(
            &core,
            &[
                ("old one", [1.0, 0.0]),
                ("new one", [1.0, 0.0]),
                ("old two", [0.0, 1.0]),
                ("new two", [0.0, 1.0]),
            ],
        )
        .await;
        for (old, new) in [(0, 1), (2, 3)] {
            core.store
                .set_superseded_by(&ids[old], Some(&ids[new]))
                .await
                .unwrap();
        }
        without_neighbours_of(&mut core, &[&ids[0]]);

        let r = run(&core, Some("reap_worth"), 200).await.unwrap();
        let u = &r[1];
        assert_eq!(u.scope.as_deref(), Some("unjudged retired notes"));
        assert_eq!(
            (u.asked, u.skipped_failed, u.stopped.as_ref()),
            (1, 1, None)
        );
        assert_eq!(u.extremes, [(0.9, "old two".to_string())]);
        assert_eq!(decider.calls(), 1, "never asked without its neighbours");
        let text = render(&r[1..]);
        assert!(
            text.contains(" · 1 asked · 0 too long · 0 without text · 1 failed · 0 left out\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_live_note_whose_neighbours_cannot_be_read_is_counted_and_the_other_pairs_are_asked()
    {
        let mut core = core_with(anything()).await;
        let ids = titled_at(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.0]),
                ("c", [0.0, 1.0]),
                ("d", [0.0, 1.0]),
            ],
        )
        .await;
        without_neighbours_of(&mut core, &[&ids[0], &ids[1]]);

        let r = run(&core, Some("pair_relation"), 200).await.unwrap();
        let u = &r[1];
        assert_eq!(u.scope.as_deref(), Some("unjudged neighbours"));
        assert_eq!(
            (u.asked, u.skipped_failed, u.stopped.as_ref()),
            (1, 2, None)
        );
        let titles: Vec<&str> = u.confident.iter().map(|c| c.title.as_str()).collect();
        assert!(titles == ["c ↔ d"] || titles == ["d ↔ c"], "{titles:?}");
    }

    #[tokio::test]
    async fn a_filed_pair_is_not_unjudged_whichever_of_its_notes_is_met_first() {
        let core = core_with(anything()).await;
        let ids = titled_at(
            &core,
            &[
                ("a", [1.0, 0.0]),
                ("b", [1.0, 0.0]),
                ("c", [0.0, 1.0]),
                ("d", [0.0, 1.0]),
                ("e", [-1.0, 0.0]),
                ("f", [-1.0, 0.0]),
            ],
        )
        .await;
        // Newest first: one filed pair is met from its first note, the others from their second.
        let (a, b) = crate::store::links::canonical(&ids[0], &ids[1]);
        let (c, d) = crate::store::links::canonical(&ids[2], &ids[3]);
        let (e, f) = crate::store::links::canonical(&ids[4], &ids[5]);
        for (id, at) in [
            (a, 600_i64),
            (b, 500),
            (c, 300),
            (d, 400),
            (e, 100),
            (f, 200),
        ] {
            sqlx::query!("UPDATE artifacts SET created_at = ? WHERE id = ?", at, id)
                .execute(&core.store.pool)
                .await
                .unwrap();
        }
        core.store.record_pair(a, b, 0.99).await.unwrap();
        core.store.record_pair(d, c, 0.99).await.unwrap();

        let (cases, left_out) = unjudged_cases(&core, &catalogue::pair_relation(), 200)
            .await
            .unwrap();
        let got: Vec<(String, Option<String>)> = cases.into_iter().map(|c| c.subjects).collect();
        assert_eq!(
            got,
            [(e.to_string(), Some(f.to_string()))],
            "the unfiled pair once, in the order a filed pair has"
        );
        assert_eq!(left_out, 0);
    }

    #[tokio::test]
    async fn a_reading_exactly_at_its_line_counts_as_yes() {
        let core = core_with(rule(|state, _| {
            let n: f64 = state
                .split_whitespace()
                .last()
                .and_then(|w| w.parse().ok())
                .unwrap_or(0.0);
            Answer::Noul { yes: n / 100.0 }
        }))
        .await;
        seed(
            &core,
            &[
                ("note 29", [1.0, 0.0]),
                ("note 30", [0.0, 1.0]),
                ("note 31", [-1.0, 0.0]),
            ],
        )
        .await;
        for id in ["asks_reminder", "has_date"] {
            let r = run(&core, Some(id), 200).await.unwrap();
            assert_eq!(r[0].reading, Some((0.3, 2)), "{id}: 0.30 and 0.31");
            let text = render(&r);
            assert!(text.contains("\n  at 0.30   yes 2 of 3\n"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_link_judged_with_a_kind_is_compared_by_that_kind_and_its_stored_order() {
        let decider = anything();
        let core = core_with(decider.clone()).await;
        let texts: Vec<String> = (0..14).map(|i| format!("note {i}")).collect();
        let rows: Vec<(&str, [f32; 2])> = texts.iter().map(|t| (t.as_str(), [1.0, 0.0])).collect();
        let ids = seed(&core, &rows).await;
        let now = crate::store::now();
        let judged = [
            (Some(LinkKind::Continues), Some(true), LinkState::Related),
            (Some(LinkKind::Example), Some(false), LinkState::Related),
            (Some(LinkKind::Background), None, LinkState::Related),
            (Some(LinkKind::Alternative), None, LinkState::Related),
            (Some(LinkKind::Contradicts), None, LinkState::Related),
            (Some(LinkKind::Unrelated), None, LinkState::Unrelated),
            // A duplicate keeps no kind: its hand-over line is the record.
            (None, None, LinkState::Related),
        ];
        let mut keys = Vec::new();
        for (i, (kind, first, state)) in judged.into_iter().enumerate() {
            let (a, b) = crate::store::links::canonical(&ids[2 * i], &ids[2 * i + 1]);
            core.store
                .bump_link(a, b, 1.0, Some("mount loop"), 30.0, now)
                .await
                .unwrap();
            match kind {
                Some(_) => core
                    .store
                    .set_link_kind(a, b, state, kind, first, Some((0, 0)))
                    .await
                    .unwrap(),
                None => core
                    .store
                    .set_link_state(
                        a,
                        b,
                        state,
                        Some("same content; handed to consolidation"),
                        Some((0, 0)),
                    )
                    .await
                    .unwrap(),
            }
            keys.push(key(a, b));
        }
        let (a, b) = keys[3].split_once('|').unwrap();
        core.store.dismiss_link(a, b).await.unwrap();

        let (kinds, _) = cases(&core, &catalogue::link_kind(), 200).await.unwrap();
        let by: HashMap<String, &Case> = kinds
            .iter()
            .map(|c| (key(&c.subjects.0, c.subjects.1.as_deref().unwrap()), c))
            .collect();
        let want_kinds = [
            "continues",
            "example",
            "background",
            "alternative",
            "contradicts",
            "unrelated",
            "duplicate",
        ];
        for (k, past) in keys.iter().zip(want_kinds) {
            assert_eq!(by[k].past.as_deref(), Some(past));
        }
        assert_eq!(by[&keys[3]].person, Some(PersonSaid::Disagreed));
        assert_eq!(by[&keys[0]].person, None);

        let (directions, _) = cases(&core, &catalogue::link_direction(), 200)
            .await
            .unwrap();
        let got: HashMap<String, Option<String>> = directions
            .into_iter()
            .map(|c| (key(&c.subjects.0, &c.subjects.1.unwrap()), c.past))
            .collect();
        let want: HashMap<String, Option<String>> = [
            (keys[0].clone(), Some("a_first".to_string())),
            (keys[1].clone(), Some("b_first".to_string())),
            (keys[2].clone(), None),
        ]
        .into();
        assert_eq!(got, want, "only a kind with an order is asked for one");

        let r = run(&core, Some("link_kind"), 200).await.unwrap();
        let asked = catalogue::link_kind();
        assert_eq!((r[0].asked, r[0].version), (7, asked.version));
        let pasts: Vec<&str> = r[0].past_picks.iter().map(|p| p.0.as_str()).collect();
        let mut named = want_kinds.to_vec();
        named.sort();
        assert_eq!(pasts, named, "each kind by its own name, none as `related`");
        let sent = &decider.requests()[0].1[0];
        assert_eq!(sent.instructions, asked.question.instructions);
    }
}
