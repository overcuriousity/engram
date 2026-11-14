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
use crate::store::artifacts::ArtifactStatus;
use crate::store::links::LinkState;
use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};
use std::collections::{BTreeMap, HashSet};

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
}

/// Old link verdict `related`: any of the new kinds that show a link.
const RELATED: &str = "continues|example|alternative|background";

/// Pair details that close a pair without the judge's verdict standing: refusals and guards.
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

/// The plan's mapping of a settled pair onto `pair_relation`; `None` leaves it out.
fn pair_past(p: &ArtifactPair, rows: &[&Action]) -> Option<&'static str> {
    // The base took it back, or consolidate closed it on similarity alone: no judgement either way.
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
    if !rows.is_empty() {
        let gone: HashSet<&str> = of(Kind::Discard).map(|r| r.subject_id.as_str()).collect();
        return (gone.contains(p.a_id.as_str()) && gone.contains(p.b_id.as_str()))
            .then_some("vacuous");
    }
    let refused = |d: &str| NOT_A_VERDICT.iter().any(|n| d.contains(n));
    match p.state {
        PairState::Contradiction => Some("conflict"),
        PairState::NoConflict
            if p.decided_by == Some(DecidedBy::Model)
                && !p.detail.as_deref().is_some_and(refused) =>
        {
            Some("distinct")
        }
        _ => None,
    }
}

async fn journal(core: &Core) -> Result<Vec<Action>> {
    core.store.recent_actions(i64::MAX as usize).await
}

async fn pairs(core: &Core) -> Result<Vec<Found>> {
    let journal = journal(core).await?;
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
            let Some(past) = pair_past(&p, rows) else {
                continue;
            };
            let asked = rows
                .iter()
                .any(|r| evidence(r).get("asked_by").and_then(|v| v.as_str()) == Some("operator"));
            let mut f = Found::new(p.created_at, &p.a_id, Some(&p.b_id), Some(past));
            f.person = person_of(rows, asked || p.decided_by == Some(DecidedBy::Operator));
            out.push(f);
        }
    }
    Ok(out)
}

/// `directional`: the links `link_direction` would be asked of, with no past order to compare.
async fn links(core: &Core, directional: bool) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for l in core.store.judged_links().await? {
        let shown = if l
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("same content"))
        {
            "duplicate"
        } else {
            RELATED
        };
        let (past, person) = match l.state {
            LinkState::Related => (shown, None),
            LinkState::Unrelated if l.reason.as_deref() != Some("unreadable") => {
                ("unrelated", None)
            }
            // Only a shown link can be dismissed, and a person dismissed it.
            LinkState::Dismissed => (shown, Some(PersonSaid::Disagreed)),
            _ => continue,
        };
        if directional && (past != RELATED || person.is_some()) {
            continue;
        }
        let mut f = Found::new(
            l.created_at,
            &l.a_id,
            Some(&l.b_id),
            (!directional).then_some(past),
        );
        f.person = person.filter(|_| !directional);
        f.words = l.cues.iter().map(|c| c.q.clone()).collect();
        out.push(f);
    }
    Ok(out)
}

/// Burials say no; a rescue said yes. A rescued note later buried counts once, as the burial.
async fn reaps(core: &Core) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for r in journal(core).await?.iter().filter(|r| r.kind == Kind::Reap) {
        let successor = match core.store.get_artifact(&r.subject_id).await {
            Ok(c) => c.superseded_by,
            Err(Error::NotFound) => None,
            Err(e) => return Err(e),
        };
        let mut f = Found::new(r.at, &r.subject_id, successor.as_deref(), Some("no"));
        f.person = person_of(&[r], false);
        out.push(f);
    }
    for s in core.store.synthesized_artifacts(i64::MAX).await? {
        if !s.cues.iter().any(|c| c.starts_with("reap rescue:")) {
            continue;
        }
        for id in core.store.artifacts_superseded_by(&s.id).await? {
            if core.store.get_artifact(&id).await?.reaped_at.is_some() {
                continue;
            }
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

/// Past merges and condensations: kept, they passed; a person taking one back disagreed.
async fn rewrites(core: &Core) -> Result<Vec<Found>> {
    let journal = journal(core).await?;
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
        let mut f = Found::new(
            rows.iter().map(|r| r.at).max().unwrap_or(0),
            merged,
            None,
            Some("yes"),
        );
        f.person = person_of(&rows, false);
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
        out.push(f);
    }
    for r in journal.iter().filter(|r| r.kind == Kind::Condense) {
        if by_evidence(&r) {
            continue;
        }
        let versions = core.store.versions_of(&r.subject_id).await?;
        let Some(v) = versions.iter().find(|v| v.action_id == r.id) else {
            continue;
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
        out.push(f);
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
async fn found(core: &Core, id: &str) -> Result<Vec<Found>> {
    match id {
        "pair_relation" => pairs(core).await,
        "link_kind" => links(core, false).await,
        "link_direction" => links(core, true).await,
        "reap_worth" => reaps(core).await,
        "category" => live(core, true).await,
        "pinned" | "time_bound" | "self_contained" | "asks_reminder" | "has_date" => {
            live(core, false).await
        }
        "complete" | "adds_nothing" => rewrites(core).await,
        "answers_query" => searches(core).await,
        // `answerable`: the base keeps no record of what a pursuit was given to write from.
        _ => Ok(vec![]),
    }
}

/// Up to five live notes near `id`, from its graveyard vector once buried. A failed read is no
/// neighbours, said in the log: one case must not stop the run.
async fn neighbours(core: &Core, id: &str, not: Option<&str>) -> Result<Vec<Text>> {
    let buried = core.store.get_artifact(id).await?.reaped_at.is_some();
    let hits = if buried {
        let model = core.embedder.model().to_string();
        let vec = core
            .store
            .graveyard_vectors(&model)
            .await?
            .into_iter()
            .find(|(g, _)| g == id);
        match vec {
            Some((_, v)) => {
                core.vectors
                    .search(&v, &Default::default(), 8, &Default::default())
                    .await
            }
            None => Ok(vec![]),
        }
    } else {
        core.vectors.neighbours(id, 8).await
    };
    let hits = hits.unwrap_or_else(|e| {
        tracing::warn!(artifact_id = id, error = %e, "no neighbours for a reap case");
        vec![]
    });
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
async fn shown(core: &Core, a: &Asked, f: &Found) -> Result<Option<(Option<String>, String)>> {
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
            Some((s, title(&x)))
        }
        "reap_worth" => {
            let Some(x) = text_of(core, &f.a).await? else {
                return Ok(None);
            };
            let near = neighbours(core, &f.a, f.not.as_deref()).await?;
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
}

struct Gathered {
    items: Vec<Item>,
    left_out: usize,
    no_text: usize,
}

async fn gather(core: &Core, a: &Asked, limit: usize) -> Result<Gathered> {
    let mut all = found(core, a.id).await?;
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
        let Some((state, title)) = shown(core, &asked, &f).await? else {
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
        });
    }
    Ok(Gathered {
        items,
        left_out,
        no_text,
    })
}

/// The cases for one question, newest first, and how many `limit` left out.
pub async fn cases(core: &Core, a: &Asked, limit: usize) -> Result<(Vec<Case>, usize)> {
    let g = gather(core, a, limit).await?;
    Ok((g.items.into_iter().map(|i| i.case).collect(), g.left_out))
}

/// How long one call may wait out a busy or failing endpoint before the run ends with its error.
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
    bands_at: Thresholds,
    counts: (usize, usize, usize),
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
    let mut extremes = Vec::new();
    if agrees_with_past.is_none() {
        let noul = matches!(a.question.kind, QKind::Noul);
        extremes = rows
            .iter()
            .zip(titles)
            .map(|(r, t)| match (noul, r.pick == "yes") {
                (true, true) => (r.top, t.clone()),
                (true, false) => (1.0 - r.top, t.clone()),
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
    let mut gathered = Vec::new();
    for a in &asked {
        gathered.push(gather(core, a, limit).await?);
    }
    let n: usize = gathered
        .iter()
        .flat_map(|g| &g.items)
        .filter(|i| i.case.state.is_some())
        .count();
    eprintln!("{n} cases to ask, about {} at 0.6 s each", estimate(n));
    let mut out = Vec::new();
    for (a, g) in asked.iter().zip(gathered) {
        let at = core.decide_cfg.bands.of(a.id);
        let (mut rows, mut titles, mut too_long) = (Vec::new(), Vec::new(), 0);
        for item in g.items {
            let Some(state) = &item.case.state else {
                too_long += 1;
                continue;
            };
            let (pick, top, agree) =
                transiently(RETRY_BUDGET, || put(core, &item.asked, state)).await?;
            rows.push(Row {
                pick,
                top,
                swapped_agree: agree,
                band: band(top, agree, at),
                past: item.case.past,
                person: item.case.person,
            });
            titles.push(item.title);
        }
        let r = summarize(a, &rows, &titles, at, (too_long, g.no_text, g.left_out));
        eprintln!("{}: {} asked", r.id, r.asked);
        out.push(r);
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
    let mut s = format!(
        "{} v{} · {} asked · {} too long · {} without text · {} left out\n",
        r.id, r.version, r.asked, r.skipped_too_long, r.skipped_no_text, r.left_out
    );
    if r.asked + r.skipped_too_long + r.skipped_no_text + r.left_out == 0 {
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
    if r.asked == 0 {
        return s;
    }
    let [act, ask, leave] = r.bands;
    let would = format!("would act {act} · ask {ask} · leave {leave}");
    s += &format!(
        "  {would:<37} (act_at {:.2}, ask_at {:.2})\n",
        r.bands_at.act_at, r.bands_at.ask_at
    );
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
        s += "  highest and lowest yes\n";
        for (v, t) in &r.extremes {
            s += &format!("  {v:.2}  {t}\n");
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
    use crate::store::links::LinkState;
    use crate::store::pairs::{ArtifactPair, DecidedBy, PairState};
    use std::collections::HashMap;
    use std::sync::Arc;

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
        assert_eq!(r.len(), 1);
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
            .update_artifact_category(&ids[0], Some("procedure"))
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
            r,
            vec![QuestionReport {
                id: "pair_relation".into(),
                version: 1,
                asked: 3,
                skipped_too_long: 0,
                skipped_no_text: 0,
                left_out: 0,
                agrees_with_past: Some((2, 3)),
                agrees_with_person: None,
                bands: [1, 1, 1],
                by_top: vec![(0.95, 1, 1), (0.70, 1, 1), (0.0, 1, 0)],
                swap_disagreed: 1,
                extremes: vec![],
                suggested: None,
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
            .update_artifact_category(&ids[2], Some("procedure"))
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
            catalogue::all().len(),
            "every question is reported"
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
            bands_at: Thresholds {
                act_at: 0.90,
                ask_at: 0.60,
            },
        };
        assert_eq!(
            render(&[r]),
            "time_bound v1 · 2 asked · 0 too long · 0 without text · 0 left out
  would act 1 · ask 0 · leave 1         (act_at 0.90, ask_at 0.60)
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
}
