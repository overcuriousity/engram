//! Insights: what is true about this installation, and what needs a person.
//!
//! Two halves. The maintenance half is Housekeeping relocated — hidden, stale
//! and retrying artifacts, the merge undo log, tokens, sources — plus the
//! surfaces that used to sit on Capture, which is now a verb rather than a
//! page. The measures half reads aggregates over tables that already exist.
//!
//! `/ui/ops` redirects here rather than answering 404: it is in bookmarks, in
//! the quiet link at the bottom of the workspace, and in at least one runbook.
//! The `POST /ui/ops/...` actions keep their paths and stay in `ui.rs`, where
//! `artifact_changed` and `ReturnTo` serve handlers all over the file — moving
//! the page is the surgical cut, moving those would drag shared machinery
//! across a boundary for nothing.

use crate::tenants::Tenant;
use askama::Template;
use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use crate::error::Result;
use crate::fmt::{ago, fmt_duration, fmt_elapsed, fmt_time};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{SourceRow, row_label, row_subtitle, source_rows, sweep_label, tally_sweep};
use crate::web::ui_error::UiResult;

/// The retrieval measure, flattened for the template.
///
/// The two figures arrive as `f64` and are rendered to two places here rather
/// than in the markup: every decision this page makes is made in Rust, so the
/// template holds no logic and a change of precision touches one line.
///
/// Not an `Option`, and the em dash is resolved here. There are three states —
/// measured, recording but nothing judged, not recording — and the card says
/// the same two rows in all three. As an `Option` with a branch inside it the
/// template carried four copies of the label-and-gloss markup, so the wording
/// of a gloss was a four-place edit.
struct Retrieval {
    recall_at_10: String,
    mrr: String,
    /// The line under the two figures: which of the three states this is, in
    /// words. Rendered `|safe` — it is built here and its only variable part
    /// is a count.
    note: String,
}

/// The old door. It takes an `Identity` like every other `/ui` route: a
/// redirect that answers before the session is checked is a route that tells
/// an anonymous caller which paths exist.
async fn moved(_: Tenant) -> Response {
    Redirect::to("/ui/insights").into_response()
}

/// Rows of one housekeeping table before it says there are more.
///
/// These tables are read to answer "what happened to X", and the answer to
/// that is a search for X rather than a scroll — so the cap is stated and the
/// rest arrive as these are cleared, instead of growing a pager nobody would
/// page through.
const TABLE_CAP: i64 = 25;

/// The same, for the one table that is also an undo.
///
/// Deeper than the rest on purpose. A vacuous verdict retires two artifacts
/// where it is found (`jobs::dedupe::discard_both`), and this list is where an
/// operator finds them again — every search path in the UI passes
/// `include_deprecated: false`, so a row that falls off the end is reachable
/// only by a link someone would have to already have. The first sweep over a
/// backlog can put hundreds here at once.
const DEPRECATED_CAP: i64 = 50;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/insights", get(page))
        .route("/ui/ops", get(moved))
}

/// Work that hit something and is waiting to try again by itself.
#[derive(serde::Serialize)]
pub struct RetryingRow {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub due: String,
    pub last_error: String,
}

/// A parked capture, with enough of the corpus it resembles to decide without
/// opening both.
pub struct ParkedRow {
    pub id: String,
    pub title: String,
    pub bytes: usize,
    pub other_id: String,
    pub other_title: String,
    pub percent: i64,
}

/// An artifact the sweep hid, with the one it lost to.
pub struct SupersededRow {
    pub id: String,
    pub title: String,
    /// Whether `title` is a name somebody wrote or the opening of the text
    /// standing in for one — see `ui::RowLabel`.
    pub named: bool,
    /// When it was written and how it opens. Two artifacts can carry the same
    /// title — a merge of two documents that named a section identically
    /// produces exactly that — and a table of them is unreadable without
    /// something that differs between the rows.
    pub subtitle: String,
    pub winner_id: String,
    pub winner_title: String,
    /// Whether `winner_title` is a name somebody wrote — see `ui::RowLabel`.
    /// The winner goes through `row_label` like every other row, so it can be
    /// the opening of a passage that has no name, or the literal `(deleted)`
    /// standing in for a winner that has since gone; neither is a name and
    /// neither may be set in the place one goes.
    pub winner_named: bool,
}

/// An artifact flagged stale with no specific replacement.
pub struct DeprecatedRow {
    pub id: String,
    pub title: String,
    /// Whether `title` is a name somebody wrote or the opening of the text
    /// standing in for one — see `ui::RowLabel`.
    pub named: bool,
}

/// One buried artifact, for the Reaped section.
pub struct GraveRow {
    pub id: String,
    pub title: String,
    /// Always true here, and stated rather than left to be noticed: the
    /// graveyard keeps the title as it stood when the row was buried and has
    /// no provenance column to say whether that name was the text's own. A
    /// buried passage is therefore still listed under its section's heading.
    pub named: bool,
    pub ago: String,
    pub reason: Option<String>,
}

/// An active artifact nobody has confirmed or retrieved in a while.
pub struct StaleRow {
    pub id: String,
    pub title: String,
    /// Whether `title` is a name somebody wrote or the opening of the text
    /// standing in for one — see `ui::RowLabel`.
    pub named: bool,
    pub last_verified: String,
}

/// One phrase of the last day: "412 links forgotten".
#[derive(serde::Serialize)]
pub(crate) struct SweepCount {
    n: i64,
    what: String,
}

/// One recorded run, as the history renders it.
#[derive(serde::Serialize)]
pub(crate) struct SweepRunRow {
    when: String,
    /// The stage in words. The identifier it was worded from is on the cell as
    /// a `title`, because the log and the config still call it that and a
    /// reader who greps for `arm_dedupe` should find it here too.
    stage: String,
    stage_id: String,
    /// Empty unless it failed, in which case it is why.
    error: String,
    took: String,
    /// The counts, already worded. Empty for a run that did nothing.
    counts: Vec<SweepCount>,
}

#[derive(Template)]
#[template(path = "insights.html")]
struct InsightsTemplate {
    /// Decisions waiting on a person. Empty renders nothing at all. Grouped,
    /// because one artifact against three others is one decision and arrived
    /// as three — see `group_pairs`.
    ///
    /// It used to sit on Capture, "where the work arrives". Capture is a verb
    /// now and not a page, and this was never work *with* the base anyway —
    /// it is work on it, which is what this page is.
    pairs: Vec<crate::web::ops::PairCluster>,
    /// How many more are behind the ones shown. Said once under the list, so a
    /// short list does not read as an empty one when it is a capped one.
    more_pairs: i64,
    /// How much is held, and how densely.
    held: crate::store::insights::Held,
    /// How much use is standing on the base, bucketed in units of an open.
    used: Vec<crate::store::insights::Bucket>,
    /// recall@10 and MRR, read from the ranks judged searches actually gave,
    /// with an em dash where nothing is being recorded — an empty measure is
    /// worse than no measure, because a zero reads as a score.
    retrieval: Retrieval,
    /// What the base did while nobody was there. `None` before a generation
    /// exists, like `evolve`.
    sleep: Option<SleepView>,
    /// What the base did to its own ranking. `None` before a generation
    /// exists, which is a base whose boot path has not run yet.
    evolve: Option<EvolveView>,
    /// The holes, one row each: a group the sweep named, or a question it has
    /// not grouped yet, shown under itself. Empty when feedback is off.
    gaps: Vec<crate::web::ui::GapGroup>,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    artifact_count: i64,
    vector_count: u64,
    retrying: Vec<RetryingRow>,
    /// Everything the base has set aside, in one list. See [`SetAsideRow`] for
    /// what this replaced and why.
    set_aside: Vec<SetAsideRow>,
    /// Any of the reads behind the list hit its cap, so there are rows this
    /// page is not showing. Said out loud, because a list that stops without
    /// saying so reads as a list of everything there is.
    set_aside_capped: bool,
    /// `None` when nothing is being learned, which renders nothing at all: a
    /// count of links on a base that records no searches is a line about a
    /// feature that is switched off.
    links: Option<crate::store::links::LinkCounts>,
    /// Recent pursuits, only when the feature is on. A count and not a table:
    /// a pursuit that ended unsatisfied is a hole in the base and belongs on
    /// the one list of those, not on a second list of its own; one that ended
    /// satisfied needs nobody; and one that was written up is in `generated`
    /// above.
    pursuit_enabled: bool,
    pursuit_recent: usize,
    pursuit_unsatisfied: usize,
    /// What the sweeps did in the last twenty-four hours, added up. Not "last
    /// night": units that reschedule themselves on their own periods do not
    /// line up into one cycle, and there is no cycle identity to group them by.
    last_day: Vec<SweepCount>,
    /// Runs in the last day that failed. Said separately, because a summary of
    /// what got done cannot report what did not.
    last_day_failures: usize,
    /// The runs themselves, newest first. What a single overwritten summary
    /// could never give: whether this started yesterday or has been going
    /// wrong for a week.
    sweep_history: Vec<SweepRunRow>,
    /// Shown against clicked, by rung. Empty when the offer is switched off, or
    /// when it has been on and never had anything to say — either way there is
    /// no table, because a heading over no rows is a claim that something is
    /// being measured when nothing is.
    offer_rates: Vec<crate::store::pursuits::OfferRate>,
}

impl InsightsTemplate {
    /// Which entry in the top row and the tab bar is the one you are inside.
    ///
    /// Read by `layout.html` to set `aria-current="page"`. The empty string is
    /// "none of them", which is a real answer for a page that hangs off no
    /// section.
    fn section(&self) -> &'static str {
        "insights"
    }
}

/// One generated artifact on Ops.
pub(crate) struct GeneratedRow {
    id: String,
    title: String,
    /// Whether `title` is a name somebody wrote or the opening of the text
    /// standing in for one — see `ui::RowLabel`.
    pub named: bool,
    subtitle: String,
    cues: Vec<String>,
    sources: Vec<SourceRow>,
}

pub(crate) struct MergedRow {
    id: String,
    title: String,
    /// Whether `title` is a name somebody wrote or the opening of the text
    /// standing in for one — see `ui::RowLabel`.
    pub named: bool,
    /// See `SupersededRow::subtitle`: what tells two rows with one title apart.
    subtitle: String,
    /// What it was written from, in the order the lineage stores them.
    sources: Vec<SourceRow>,
    /// True when a source has been deleted since, so the artifact claims less
    /// provenance than its text carries.
    orphaned: bool,
}

/// One thing the base has set aside for a person.
///
/// Seven tables stood here — Merged, Generated, Hidden as stale, Reaped, Worth
/// a second look, Hidden as near-identical, and Captures waiting on a decision
/// — each with a heading, a paragraph explaining its mechanism, and its own
/// column layout. They were the same shape: a thing, why the base touched it,
/// what it put beside it, and the button that takes it back. Seven paragraphs
/// of that is a page about the machine's internal categories; one table with a
/// reason on each row is a page about what is waiting.
///
/// The reads are unchanged — each source still runs its own query with its own
/// cap — and this is the fold. `kind` is what the row is called; `why` is the
/// sentence that used to be the section's paragraph, said per row because it
/// differs per row.
pub(crate) struct SetAsideRow {
    href: String,
    /// What the row's actions name. Which thing that is depends on `kind` —
    /// a corpus for `parked`, the artifact for every other kind, `merged`
    /// included, whose undo route takes the artifact the merge wrote and not
    /// the journal action that wrote it — and a client reads it against
    /// `kind` rather than taking it apart. Carried beside `href` rather than
    /// parsed out of it: a link is a route, not an identity.
    ///
    /// Not an identity on its own either: two of the seven questions can be
    /// true of one artifact at once, so a row is identified by `kind` and
    /// `subject_id` together. Under one `kind` a subject appears once — see
    /// `Store::artifacts_by_status`, which is where that is kept true.
    pub(crate) subject_id: String,
    /// The artifact to open where the row is about one. `None` for a parked
    /// capture, which is a corpus.
    pub(crate) artifact_id: Option<String>,
    pub(crate) title: String,
    /// See `ui::RowLabel::named`. A label that is the artifact's own opening
    /// is set as text, not in the place a name would go.
    pub(crate) named: bool,
    /// What tells two rows with one title apart. Empty where nothing does.
    pub(crate) subtitle: String,
    /// The one-word name for what put this row here, as a badge.
    pub(crate) kind: &'static str,
    /// The sentence. Never a mechanism the reader has to already know: "written
    /// from 3 others" rather than "the dedupe pass wrote this".
    pub(crate) why: String,
    /// What the base put beside it: the sources a merge came from, the artifact
    /// a near-duplicate lost to, the capture a park collided with.
    pub(crate) beside: Vec<crate::web::ui::SourceRow>,
    /// A note under the row for the one thing that is not simply reversible.
    pub(crate) caveat: Option<String>,
    actions: Vec<SetAsideAction>,
}

/// One button on a set-aside row.
pub(crate) struct SetAsideAction {
    action: String,
    label: &'static str,
    /// The name/value pair the three-way park decision posts. Empty for every
    /// other row, whose action is the whole of what it says.
    field: Option<(&'static str, &'static str)>,
    /// Why the button is there, for a pointer and for a screen reader. The
    /// icons these replaced carried it in a `title`, which is nowhere on a
    /// phone; the labels carry it now and this is the long form.
    hint: &'static str,
}

impl SetAsideAction {
    fn new(action: String, label: &'static str, hint: &'static str) -> Self {
        Self {
            action,
            label,
            field: None,
            hint,
        }
    }
}

/// The seven things the base set aside, folded into one list, and whether any
/// of their caps bit.
///
/// Extracted from the page so the JSON door answers the same rows: two
/// accounts of what is waiting for a person would differ the first time one of
/// the seven sources changed, and the row carries an undo, so the difference
/// would be about what can still be taken back.
pub(crate) async fn set_aside_rows(tenant: &Tenant) -> Result<(Vec<SetAsideRow>, bool)> {
    // A parked capture is the one corpus state no worker advances. It has to be
    // shown here or it sits unprocessed with nothing saying why.
    let mut parked = Vec::new();
    for c in tenant.core.store.parked_corpora(50).await? {
        let other_id = c.near_dupe_of.clone().unwrap_or_default();
        let other_title = match tenant.core.store.get_corpus(&other_id).await {
            Ok(o) => o.title_hint.unwrap_or_else(|| "untitled".into()),
            Err(_) => "(deleted)".into(),
        };
        parked.push(ParkedRow {
            percent: (c.near_dupe_score.unwrap_or(0.0) * 100.0).round() as i64,
            bytes: c.raw_text.len(),
            title: c.title_hint.clone().unwrap_or_else(|| "untitled".into()),
            id: c.id,
            other_id,
            other_title,
        });
    }

    let mut superseded = Vec::new();
    // One past the cap, so the page can say it is capped rather than truncate
    // in silence — a table that stops at 25 with nothing said reads as a table
    // of everything there is.
    for c in tenant
        .core
        .store
        .superseded_artifacts(TABLE_CAP + 1)
        .await?
    {
        let winner_id = c.superseded_by.clone().unwrap_or_default();
        let winner = match tenant.core.store.get_artifact(&winner_id).await {
            Ok(w) => row_label(&w),
            Err(_) => crate::web::ui::RowLabel {
                text: "(deleted)".to_string(),
                named: false,
            },
        };
        let winner_named = winner.named;
        let winner_title = winner.text;
        let label = row_label(&c);
        superseded.push(SupersededRow {
            named: label.named,
            title: label.text,
            subtitle: row_subtitle(&c),
            id: c.id,
            winner_id,
            winner_title,
            winner_named,
        });
    }

    let mut merged = Vec::new();
    let merged_chunks = tenant.core.store.merged_artifacts(TABLE_CAP + 1).await?;
    // One lineage call per page, not one per row: `roots_of` takes the batch.
    let merged_ids: Vec<String> = merged_chunks.iter().map(|c| c.id.clone()).collect();
    let roots = tenant
        .core
        .store
        .roots_of(&merged_ids)
        .await
        .unwrap_or_default();
    for c in merged_chunks {
        let sources = source_rows(
            &tenant.core.store,
            &c.id,
            roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        let label = row_label(&c);
        merged.push(MergedRow {
            orphaned: c.flags.iter().any(|f| f == "orphaned_source"),
            named: label.named,
            title: label.text,
            subtitle: row_subtitle(&c),
            id: c.id,
            sources,
        });
    }

    let more_merged = merged.len() > TABLE_CAP as usize;
    merged.truncate(TABLE_CAP as usize);

    let mut generated = Vec::new();
    let gen_chunks = tenant.core.store.synthesized_artifacts(TABLE_CAP).await?;
    let gen_ids: Vec<String> = gen_chunks.iter().map(|c| c.id.clone()).collect();
    let gen_roots = tenant
        .core
        .store
        .roots_of(&gen_ids)
        .await
        .unwrap_or_default();
    for c in gen_chunks {
        let sources = source_rows(
            &tenant.core.store,
            &c.id,
            gen_roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        let label = row_label(&c);
        generated.push(GeneratedRow {
            named: label.named,
            title: label.text,
            subtitle: row_subtitle(&c),
            cues: c.cues.clone(),
            id: c.id,
            sources,
        });
    }

    let more_superseded = superseded.len() > TABLE_CAP as usize;
    superseded.truncate(TABLE_CAP as usize);

    // One past the cap, as the two tables above do it: this is the undo for
    // every artifact the judge retires unattended, so a list that stops
    // without saying so reads as "these are all of them".
    let mut deprecated: Vec<DeprecatedRow> = tenant
        .core
        .store
        .artifacts_by_status(
            crate::store::artifacts::ArtifactStatus::Deprecated,
            DEPRECATED_CAP + 1,
        )
        .await?
        .into_iter()
        .map(|c| DeprecatedRow {
            named: row_label(&c).named,
            title: row_label(&c).text,
            id: c.id,
        })
        .collect();
    let more_deprecated = deprecated.len() > DEPRECATED_CAP as usize;
    deprecated.truncate(DEPRECATED_CAP as usize);

    // The graveyard, the same way: the one undo for the one stage that
    // destroys text, and a list that stops without saying so reads as "these
    // are all of them".
    let mut reaped: Vec<GraveRow> = tenant
        .core
        .store
        .graveyard_list(DEPRECATED_CAP + 1)
        .await?
        .into_iter()
        .map(|g| GraveRow {
            named: true,
            title: g.title.unwrap_or_else(|| "(untitled)".to_string()),
            ago: ago(g.reaped_at),
            id: g.id,
            reason: g.reason,
        })
        .collect();
    let more_reaped = reaped.len() > DEPRECATED_CAP as usize;
    reaped.truncate(DEPRECATED_CAP as usize);

    // Read-only candidates: nothing here has been changed, only listed.
    let stale: Vec<StaleRow> = tenant
        .core
        .stale_candidates(50)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "no stale candidates for ops");
            vec![]
        })
        .into_iter()
        .map(|r| StaleRow {
            // A stale candidate is a search result, so the flag is already on
            // it: `borrowed_name` covers a passage carrying its section's
            // heading as well as one that never had a title at all.
            named: !r.borrowed_name && r.title.is_some(),
            title: match r.borrowed_name {
                true => markdown::snippet(&r.text, 60),
                false => r
                    .title
                    .clone()
                    .unwrap_or_else(|| markdown::snippet(&r.text, 60)),
            },
            id: r.artifact_id,
            last_verified: r
                .last_verified_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".to_string()),
        })
        .collect();

    // Seven lists into one. Order is by how much the row wants a person:
    // a parked capture is blocked until it is answered, an unverified artifact
    // is a question, and the rest are the base's own work with the undo left
    // where it can be found.
    let set_aside_capped = more_merged || more_superseded || more_deprecated || more_reaped;
    let mut set_aside: Vec<SetAsideRow> = Vec::new();
    for p_ in parked {
        set_aside.push(SetAsideRow {
            href: format!("/ui/corpora/{}", p_.id),
            subject_id: p_.id.clone(),
            artifact_id: None,
            // A corpus label is always a name: `corpus_label` falls back to
            // "document" or the opening rather than to nothing.
            named: true,
            title: p_.title,
            subtitle: format!("{} B", p_.bytes),
            kind: "parked",
            why: format!("{}% the same as the capture beside it, so nothing has been spent on reading it yet", p_.percent),
            beside: vec![crate::web::ui::SourceRow {
                id: String::new(),
                title: p_.other_title,
                named: true,
                subtitle: String::new(),
                corpus_id: p_.other_id,
            }],
            caveat: None,
            actions: vec![
                SetAsideAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Replace the old one",
                    field: Some(("action", "replace")),
                    hint: "Keep this capture and retire the one beside it",
                },
                SetAsideAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Keep both",
                    field: Some(("action", "keep_both")),
                    hint: "Read this one too; both stay in the base",
                },
                SetAsideAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Discard this",
                    field: Some(("action", "discard")),
                    hint: "Drop this capture and keep the one beside it",
                },
            ],
        });
    }
    for s in stale {
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", s.id),
            subject_id: s.id.clone(),
            artifact_id: Some(s.id.clone()),
            named: s.named,
            title: s.title,
            subtitle: String::new(),
            kind: "unverified",
            why: format!(
                "last confirmed {}, and rarely reached since — nothing has been changed, and this never moves search",
                s.last_verified
            ),
            beside: Vec::new(),
            caveat: None,
            actions: vec![
                SetAsideAction::new(
                    format!("/ui/ops/artifacts/{}/verify", s.id),
                    "Still accurate",
                    "Confirm this is still accurate — it resets the artifact's age, which search reads",
                ),
                SetAsideAction::new(
                    format!("/ui/ops/artifacts/{}/deprecate", s.id),
                    "Hide",
                    "Hide from results — the artifact is kept, and this can be undone",
                ),
            ],
        });
    }
    for m in merged {
        let n = m.sources.len();
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", m.id),
            subject_id: m.id.clone(),
            artifact_id: Some(m.id.clone()),
            named: m.named,
            title: m.title,
            subtitle: m.subtitle,
            kind: "merged",
            why: format!(
                "written from {n} artifact{}, which are still stored — undoing brings them back and retires this",
                if n == 1 { "" } else { "s" }
            ),
            beside: m.sources,
            // Not data loss: the text still says what the deleted source said.
            // It is a claim of provenance the artifact can no longer support.
            caveat: m
                .orphaned
                .then(|| "a source has since been deleted".to_string()),
            actions: vec![SetAsideAction::new(
                format!("/ui/ops/merges/{}/undo", m.id),
                "Undo",
                "Put the sources back in results and retire this merge",
            )],
        });
    }
    for g in generated {
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", g.id),
            subject_id: g.id.clone(),
            artifact_id: Some(g.id.clone()),
            named: g.named,
            title: g.title,
            subtitle: g.subtitle,
            kind: "generated",
            why: match g.cues.is_empty() {
                true => "written after a run of searches the base could not answer".to_string(),
                false => format!(
                    "written after you asked {} — what it was written from stays in results beside it",
                    g.cues
                        .iter()
                        .map(|c| format!("\u{201c}{c}\u{201d}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            beside: g.sources,
            caveat: None,
            actions: vec![SetAsideAction::new(
                format!("/ui/ops/artifacts/{}/deprecate", g.id),
                "Hide",
                "Take it out of results and keep it",
            )],
        });
    }
    for s in superseded {
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", s.id),
            subject_id: s.id.clone(),
            artifact_id: Some(s.id.clone()),
            named: s.named,
            title: s.title,
            subtitle: s.subtitle,
            kind: "hidden",
            why: "near-identical to the one beside it, so it is kept out of results — still stored, still readable".to_string(),
            beside: vec![crate::web::ui::SourceRow {
                id: s.winner_id,
                title: s.winner_title,
                // Carried, not assumed. `row_label` decided this above, and
                // discarding its answer here set a passage's opening — or the
                // `(deleted)` of a winner that has itself gone — in the place
                // a name goes, which is the one thing `RowLabel` exists to
                // stop.
                named: s.winner_named,
                subtitle: String::new(),
                corpus_id: String::new(),
            }],
            caveat: None,
            actions: vec![SetAsideAction::new(
                format!("/ui/ops/artifacts/{}/unsupersede", s.id),
                "Undo",
                "Return it to results",
            )],
        });
    }
    for d in deprecated {
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", d.id),
            subject_id: d.id.clone(),
            artifact_id: Some(d.id.clone()),
            named: d.named,
            title: d.title,
            subtitle: String::new(),
            kind: "hidden",
            why: "flagged stale with no replacement named — search skips it and Ask does not read it, and it is still at its own link".to_string(),
            beside: Vec::new(),
            caveat: None,
            actions: vec![SetAsideAction::new(
                format!("/ui/ops/artifacts/{}/reactivate", d.id),
                "Reactivate",
                "Return it to results",
            )],
        });
    }
    for g in reaped {
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", g.id),
            subject_id: g.id.clone(),
            artifact_id: Some(g.id.clone()),
            named: g.named,
            title: g.title,
            subtitle: String::new(),
            kind: "buried",
            why: match g.reason {
                Some(r) => format!(
                    "buried {} · {r} — out of search and out of the index, text kept",
                    g.ago
                ),
                None => format!(
                    "buried {} — out of search and out of the index, text kept",
                    g.ago
                ),
            },
            beside: Vec::new(),
            caveat: None,
            actions: vec![SetAsideAction::new(
                format!("/ui/ops/artifacts/{}/reactivate", g.id),
                "Restore",
                "Return it to results and embed it again",
            )],
        });
    }
    Ok((set_aside, set_aside_capped))
}

/// The disclosure at the foot of Insights, as data: what the machine is
/// doing, for the phone.
#[derive(serde::Serialize)]
pub(crate) struct Machine {
    pub artifacts: i64,
    pub vectors: u64,
    pub jobs: Vec<(String, i64)>,
    pub oldest_pending_secs: Option<i64>,
    pub links: Option<crate::store::links::LinkCounts>,
    pub last_day: Vec<SweepCount>,
    pub last_day_failures: usize,
    pub sweep_history: Vec<SweepRunRow>,
    pub offer_rates: Vec<crate::store::pursuits::OfferRate>,
    pub retrying: Vec<RetryingRow>,
}

pub(crate) async fn machine(tenant: &Tenant) -> Result<Machine> {
    use sqlx::Row;
    let artifacts: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?
        .get("n");
    let retrying: Vec<RetryingRow> = tenant
        .core
        .store
        .retrying_jobs(50)
        .await?
        .into_iter()
        .map(|j| RetryingRow {
            stage: j.stage,
            target_id: j.target_id,
            attempts: j.attempts,
            due: fmt_duration(j.next_attempt_secs),
            last_error: j.last_error.unwrap_or_else(|| "—".into()),
        })
        .collect();
    let day = tenant
        .core
        .store
        .sweep_runs_since(crate::store::now() - 86_400, 500)
        .await
        .unwrap_or_default();
    let last_day_failures = day.iter().filter(|r| r.outcome == "failed").count();
    let mut totals: Vec<(String, i64)> = Vec::new();
    for r in &day {
        tally_sweep(&r.stage, &r.detail, &mut totals);
    }
    let last_day: Vec<SweepCount> = totals
        .into_iter()
        .map(|(what, n)| SweepCount { n, what })
        .collect();
    let sweep_history: Vec<SweepRunRow> = tenant
        .core
        .store
        .sweep_history(TABLE_CAP)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let mut counts = Vec::new();
            tally_sweep(&r.stage, &r.detail, &mut counts);
            SweepRunRow {
                when: fmt_time(r.started_at),
                error: match r.outcome == "failed" {
                    true => serde_json::from_str::<serde_json::Value>(&r.detail)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
                        .unwrap_or_else(|| "it failed".into()),
                    false => String::new(),
                },
                took: fmt_elapsed(r.ended_at - r.started_at),
                stage: sweep_label(&r.stage).to_string(),
                stage_id: r.stage,
                counts: counts
                    .into_iter()
                    .map(|(what, n)| SweepCount { n, what })
                    .collect(),
            }
        })
        .collect();
    Ok(Machine {
        artifacts,
        vectors: tenant.core.vectors.count().await.unwrap_or(0),
        jobs: tenant.core.store.job_counts().await?,
        oldest_pending_secs: tenant.core.store.oldest_pending_age().await?,
        links: match tenant.core.associating() {
            true => Some(tenant.core.store.link_counts().await?),
            false => None,
        },
        last_day,
        last_day_failures,
        sweep_history,
        offer_rates: match tenant.core.recommends() {
            true => tenant
                .core
                .store
                .offer_rates(crate::store::now() - 30 * 86_400)
                .await
                .unwrap_or_default(),
            false => Vec::new(),
        },
        retrying,
    })
}

/// What the base did on its own, in the sentences Insights says: last night,
/// the ranking, and the pursuits line. Disclosure, not control — the tuning
/// offer stays on the web, where the person who may apply it is at a keyboard.
#[derive(serde::Serialize)]
pub(crate) struct Report {
    pub sleep: Option<SleepView>,
    pub evolve: Option<EvolveView>,
    /// Runs of searches that went quiet, and how many are on the gap list.
    /// Null while `[learn]` is off.
    pub pursuits: Option<(usize, usize)>,
    /// How many pairs are waiting beyond the ones `GET /pairs` lists.
    pub more_pairs: i64,
}

pub(crate) async fn report(tenant: &Tenant) -> Result<Report> {
    let (_, more_pairs) = crate::web::ops::pair_rows(tenant).await?;
    let pursuits = match tenant.core.learn.enabled {
        true => {
            let recent = tenant.core.store.recent_pursuits(50).await?;
            let on_the_gap_list = tenant
                .core
                .store
                .open_pursuit_gap_ids(tenant.core.embedder.model())
                .await
                .unwrap_or_default();
            let unsatisfied = recent
                .iter()
                .filter(|p| p.state == "unsatisfied" && on_the_gap_list.contains(&p.id))
                .count();
            Some((recent.len(), unsatisfied))
        }
        false => None,
    };
    Ok(Report {
        sleep: sleep_view(&tenant.core).await?,
        evolve: evolve_view(&tenant.core).await?,
        pursuits,
        more_pairs,
    })
}

async fn page(tenant: Tenant) -> UiResult<Response> {
    use sqlx::Row;

    let (pairs, more_pairs) = crate::web::ops::pair_rows(&tenant).await?;
    let pairs = crate::web::ops::group_pairs(pairs);

    // Read, never computed: the page shows what the sweep grouped and named,
    // and whatever has been judged since sits under itself until the next
    // pass. Nothing here embeds or calls a model.
    let gaps = if tenant.core.learn.enabled {
        let (rows, loose) = tenant
            .core
            .store
            .gap_rows(tenant.core.embedder.model(), tenant.core.weak_below())
            .await?;
        // A group and a lone question are one row each and read the same:
        // what the sweep called the group, or what somebody typed. Which of
        // the two it is matters to nobody deciding what to do about it.
        rows.into_iter()
            .map(|r| crate::web::ui::GapGroup {
                label: r.label,
                members: r
                    .members
                    .into_iter()
                    .map(crate::web::ui::gap_member)
                    .collect(),
            })
            .chain(loose.into_iter().map(|g| crate::web::ui::GapGroup {
                label: g.text.clone(),
                members: vec![crate::web::ui::gap_member(g)],
            }))
            .collect()
    } else {
        vec![]
    };

    let artifact_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?
        .get("n");

    // Not a queue of chores: work that hit something and is waiting to try
    // again on its own. Nothing here needs a person.
    let retrying: Vec<RetryingRow> = tenant
        .core
        .store
        .retrying_jobs(50)
        .await?
        .into_iter()
        .map(|j| RetryingRow {
            stage: j.stage,
            target_id: j.target_id,
            attempts: j.attempts,
            due: fmt_duration(j.next_attempt_secs),
            last_error: j.last_error.unwrap_or_else(|| "—".into()),
        })
        .collect();

    let (set_aside, set_aside_capped) = set_aside_rows(&tenant).await?;

    let pursuit_enabled = tenant.core.learn.enabled;
    let recent = match pursuit_enabled {
        true => tenant.core.store.recent_pursuits(50).await?,
        false => Vec::new(),
    };
    let pursuit_recent = recent.len();
    // The ones the sentence below can honestly point at. `unsatisfied` is how a
    // run of searches *ended*, and a capture that answers one afterwards leaves
    // that word alone deliberately — coverage never rewrites what happened — so
    // counting the state sent the operator to a gap list that had already
    // dropped half of them.
    let on_the_gap_list = match pursuit_enabled {
        true => tenant
            .core
            .store
            .open_pursuit_gap_ids(tenant.core.embedder.model())
            .await
            .unwrap_or_default(),
        false => Default::default(),
    };
    let pursuit_unsatisfied = recent
        .iter()
        .filter(|p| p.state == "unsatisfied" && on_the_gap_list.contains(&p.id))
        .count();
    // What the memory did while nobody was looking. The last day as one
    // sentence, and under it the runs themselves — which is the half a single
    // overwritten summary could never give.
    let day = tenant
        .core
        .store
        .sweep_runs_since(crate::store::now() - 86_400, 500)
        .await
        .unwrap_or_default();
    let last_day_failures = day.iter().filter(|r| r.outcome == "failed").count();
    let mut totals: Vec<(String, i64)> = Vec::new();
    for r in &day {
        tally_sweep(&r.stage, &r.detail, &mut totals);
    }
    let last_day: Vec<SweepCount> = totals
        .into_iter()
        .map(|(what, n)| SweepCount { n, what })
        .collect();
    let sweep_history: Vec<SweepRunRow> = tenant
        .core
        .store
        .sweep_history(TABLE_CAP)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let mut counts = Vec::new();
            tally_sweep(&r.stage, &r.detail, &mut counts);
            SweepRunRow {
                when: fmt_time(r.started_at),
                error: match r.outcome == "failed" {
                    true => serde_json::from_str::<serde_json::Value>(&r.detail)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
                        .unwrap_or_else(|| "it failed".into()),
                    false => String::new(),
                },
                took: fmt_elapsed(r.ended_at - r.started_at),
                stage: sweep_label(&r.stage).to_string(),
                stage_id: r.stage,
                counts: counts
                    .into_iter()
                    .map(|(what, n)| SweepCount { n, what })
                    .collect(),
            }
        })
        .collect();

    Ok(HtmlTemplate(InsightsTemplate {
        sleep: sleep_view(&tenant.core).await?,
        evolve: evolve_view(&tenant.core).await?,
        held: tenant.core.store.held().await?,
        used: tenant
            .core
            .store
            .used(tenant.core.activation.half_life_days, crate::store::now())
            .await?,
        // Read only where searches are being recorded at all. The measure is
        // read off judged searches, and on an installation that records none
        // the honest answer is that there is nothing to say — not 0.00.
        retrieval: match tenant.core.learn.enabled {
            true => {
                let f = tenant
                    .core
                    .store
                    .feedback_stats(tenant.core.weak_below())
                    .await?;
                match f.judged {
                    0 => Retrieval {
                        recall_at_10: "—".into(),
                        mrr: "—".into(),
                        note: format!(
                            "nothing judged yet, from {} recorded — answer \
                             <em>Was this what you were looking for?</em> under a result",
                            f.captured
                        ),
                    },
                    judged => Retrieval {
                        recall_at_10: format!("{:.2}", f.recall_at_10),
                        mrr: format!("{:.2}", f.mrr),
                        note: format!(
                            "from {judged} judged search{}{}",
                            if judged == 1 { "" } else { "es" },
                            match f.pending {
                                0 => String::new(),
                                p => format!(", {p} waiting"),
                            }
                        ),
                    },
                }
            }
            false => Retrieval {
                recall_at_10: "—".into(),
                mrr: "—".into(),
                note: "not recording searches, so there is nothing to measure".into(),
            },
        },
        pairs,
        more_pairs,
        gaps,
        retrying,
        set_aside,
        set_aside_capped,
        job_counts: tenant.core.store.job_counts().await?,
        oldest_pending_secs: tenant.core.store.oldest_pending_age().await?,
        artifact_count,
        // Qdrant being briefly unreachable must not blank the ops page, which
        // is exactly where you look when something is wrong.
        vector_count: tenant.core.vectors.count().await.unwrap_or(0),
        links: match tenant.core.associating() {
            true => Some(tenant.core.store.link_counts().await?),
            false => None,
        },
        pursuit_enabled,
        pursuit_recent,
        pursuit_unsatisfied,
        last_day,
        last_day_failures,
        sweep_history,
        // The last month rather than the last day: a weekly pattern needs
        // weeks, so a hit rate measured over a day would be a number nobody
        // could act on. Read like `vector_count` — a failure here must not
        // blank the page you open when something is wrong.
        offer_rates: match tenant.core.recommends() {
            true => tenant
                .core
                .store
                .offer_rates(crate::store::now() - 30 * 86_400)
                .await
                .unwrap_or_default(),
            false => Vec::new(),
        },
    })
    .into_response())
}

fn cap_str(c: Option<usize>) -> String {
    c.map_or("none".to_string(), |n| n.to_string())
}

// ── Last night ──────────────────────────────────────────────────────────────

/// The `_sleep.html` block: what the base did while nobody was there, in
/// words, and what nothing has ever asked for.
#[derive(serde::Serialize)]
pub(crate) struct SleepView {
    /// One sentence chain per sleep, newest first.
    runs: Vec<String>,
    /// How long a base has to be quiet before it sleeps, for the empty state.
    idle_mins: i64,
    /// Artifacts nothing has asked for: the count, and the oldest few.
    unrehearsed_count: i64,
    unrehearsed: Vec<(String, String)>,
}

/// One sleep as a sentence chain. Every number a person can act on has a
/// page: conflicts are on the pair set_aside, adoptions and undos on the evolve
/// block below this one.
fn sleep_sentence(r: &crate::store::sleep_runs::SleepRun) -> String {
    let mut s = format!("{} — ", ago(r.started));
    match r.stopped.as_str() {
        "suspended" => s.push_str("suspended: observations no longer agree with verdicts. "),
        "no_evidence" => s.push_str("nothing moved: no evidence on either side. "),
        "activity" => s.push_str("stopped: you came back. "),
        "budget" => s.push_str("budget spent. "),
        _ => {}
    }
    // No conflict count. Nothing writes conflicts any more — the detector was
    // deleted after it misfired — so the number was always zero and "waiting
    // for you" pointed at a list that would never hold anything.
    s.push_str(&format!(
        "Integrated {} — {} new, {} known. Rehearsed {} probe{}, {} found.",
        r.integrated,
        r.novel,
        r.known,
        r.rehearsed,
        if r.rehearsed == 1 { "" } else { "s" },
        r.found
    ));
    // What it did to the ranking, without the id. A ULID tail is not something
    // a person can act on or look up — the generation it names is spelled out
    // in full one block below, under Ranking, which is where somebody who
    // wants the parameters is going anyway.
    if r.adopted.is_some() {
        s.push_str(" Adopted a new ranking — see Ranking below.");
    }
    if r.reverted.is_some() {
        s.push_str(" Took the ranking back to what it was.");
    }
    if r.refused.is_some() {
        s.push_str(" Refused a proposed ranking on the base's own probes.");
    }
    if r.undone + r.restored > 0 {
        s.push_str(&format!(
            " Took {} corpus action{} back; restored {}.",
            r.undone,
            if r.undone == 1 { "" } else { "s" },
            r.restored
        ));
    }
    if r.interference > 0 {
        // "Saw", not "Filed". The rule files nothing: retrieval competition is
        // a fact about ranking, and the list this used to write to makes
        // claims about meaning. A sentence promising pairs sent a reader to a
        // page that would never show them.
        s.push_str(&format!(
            " Saw {} artifact{} outranked in every rehearsal.",
            r.interference,
            if r.interference == 1 { "" } else { "s" }
        ));
    }
    if r.condensed > 0 {
        s.push_str(&format!(" Condensed {}.", r.condensed));
    }
    if r.budget > 0 {
        s.push_str(&format!(
            " {} of {} actions this week.",
            r.budget_used, r.budget
        ));
    }
    s
}

pub(crate) async fn sleep_view(core: &crate::core::Core) -> Result<Option<SleepView>> {
    if core.store.live_generation().await?.is_none() {
        return Ok(None);
    }
    let runs = core
        .store
        .sleep_runs(7)
        .await?
        .iter()
        .map(sleep_sentence)
        .collect();
    let (unrehearsed_count, list) = core.store.unrehearsed(20).await?;
    Ok(Some(SleepView {
        runs,
        idle_mins: core.evolve.idle_secs / 60,
        unrehearsed_count,
        unrehearsed: list
            .into_iter()
            .map(|(id, title)| {
                let label = title.unwrap_or_else(|| short(&id).to_string());
                (id, label)
            })
            .collect(),
    }))
}

// ── What the base did on its own ────────────────────────────────────────────

/// The `_evolve.html` block: the state of the self-tuning loop, in words.
#[derive(serde::Serialize)]
pub(crate) struct EvolveView {
    /// Why the loop is not moving, when it is not. Said before anything else.
    suspended: Option<String>,
    /// Which mode the base is in, in every mode. `standing` below says it only
    /// where autonomy is *not* moving the ranking, so the default — "ranking"
    /// — was the one setting the page never named.
    mode: String,
    /// The generation in force, and how long it has been.
    live: String,
    /// Its parameters, as one line of `name value` pairs.
    ///
    /// It used to be the middle of `live`, which made that line
    /// "live generation 247c9618 · recency 0.05, cap 3, pool ×3, half-life
    /// 180d, lift 0, spread 3, rerank off, review 0.88 · since today" — a
    /// parameter dump at body size, under no heading, between two sentences.
    /// The identity and the age are what the line is for; the numbers are for
    /// whoever came to read numbers, and they are folded.
    params: String,
    /// Whether it is under watch and what it promised, or that autonomy is off,
    /// or that the base is free to propose.
    standing: String,
    /// What the live generation scores on the base's own probes, in words —
    /// and that it is a comparison, not a score.
    rehearsed: String,
    /// Recent generations, newest first, each with how it came to be and how
    /// it ended.
    history: Vec<String>,
    /// What the base did to the corpus lately, newest first, with evidence
    /// undos and operator undos told apart.
    actions: Vec<String>,
    /// What the two corpus rules did the last time they ran, or `None` where
    /// they have not run yet.
    rules: Option<String>,
}

/// One corpus action as a sentence.
fn action_str(a: &crate::store::actions::Action) -> String {
    use crate::store::actions::{Kind, UndoneBy};
    let other = |s: &Option<String>| short(s.as_deref().unwrap_or("?")).to_string();
    let what = match a.kind {
        Kind::Merge => format!(
            "merged {} into {}",
            short(&a.subject_id),
            other(&a.survivor_id)
        ),
        Kind::Supersede => format!(
            "hid {} in favour of {}",
            short(&a.subject_id),
            other(&a.survivor_id)
        ),
        Kind::Discard => format!("discarded {}", short(&a.subject_id)),
        Kind::Reap => format!("buried {}", short(&a.subject_id)),
        Kind::Promote => format!("promoted window {}", a.subject_id),
        Kind::Moment => format!("filed a reminder, moment {}", short(&a.subject_id)),
        Kind::Condense => format!(
            "condensed {} ({})",
            short(&a.subject_id),
            a.detail.as_deref().unwrap_or("a version retired")
        ),
    };
    let ended = match a.undone_by {
        Some(UndoneBy::Evidence) => " — taken back on evidence",
        Some(UndoneBy::Operator) => " — undone by you",
        None => "",
    };
    format!("{} — {}{}", ago(a.at), what, ended)
}

/// The rules' last run, as the pass wrote it to `meta`.
async fn rules_str(core: &crate::core::Core) -> Result<Option<String>> {
    let Some(raw) = core.store.meta_get(crate::jobs::retract::LAST_RUN).await? else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    Ok(Some(format!(
        "Last quiet period, {}, the base reconsidered {} of what it hid, took {} back, and restored {} for a search given up on.",
        ago(n("at")),
        n("reconsidered"),
        n("undone"),
        n("restored")
    )))
}

fn params_str(p: &crate::store::generations::GenerationParams) -> String {
    format!(
        "recency {:.2}, cap {}, pool ×{}, half-life {}d, lift {}, sitting {}, spread {}, rerank {}, review {:.2}",
        p.recency_weight,
        cap_str(p.per_source_cap),
        p.candidate_multiplier,
        p.recency_half_life_days,
        p.prime_lift,
        if p.sitting_prime { "on" } else { "off" },
        p.spread_max,
        if p.rerank { "on" } else { "off" },
        p.review_min
    )
}

/// The tail of an id. Ids are ULIDs, so two minted in one sitting share their
/// head; the tail is what tells them apart.
pub(crate) fn short(id: &str) -> &str {
    &id[id.len().saturating_sub(8)..]
}

pub(crate) async fn evolve_view(core: &crate::core::Core) -> Result<Option<EvolveView>> {
    let Some(live) = core.store.live_generation().await? else {
        return Ok(None);
    };
    let suspended = match crate::eval::anchor::agreement(core).await? {
        Some(a) if !crate::eval::anchor::trustworthy(&a) => Some(format!(
            "Over the judged searches that also left an observation, the base's own evidence agreed with your verdicts {} time{} and disagreed {} — no better than chance. It adopts nothing and takes nothing back until that changes; it keeps recording.",
            a.agreed,
            if a.agreed == 1 { "" } else { "s" },
            a.disagreed
        )),
        _ => None,
    };
    let standing = match (&live.parent_id, live.predicted) {
        (Some(parent_id), Some(predicted)) => {
            let new = crate::eval::lived::lived(core, &live.id).await?;
            let old = match core.store.generation(parent_id).await? {
                Some(parent) => crate::eval::lived::lived(core, &parent.id).await?,
                None => crate::eval::lived::Lived {
                    positives: 0,
                    negatives: 0.0,
                    observations: 0,
                },
            };
            if crate::eval::lived::settled(&new, &old) {
                format!(
                    "adopted by the base and held: {} positive of {} observations, against {} of {} for the generation before it.",
                    new.positives, new.observations, old.positives, old.observations
                )
            } else {
                format!(
                    "under watch — it promised MRR {:+.3} over the replay and has earned {} positive of {} observations so far, against {} of {} for the generation before it. Nothing else is proposed until this is decided.",
                    predicted, new.positives, new.observations, old.positives, old.observations
                )
            }
        }
        _ => "set by hand or at boot; the base may propose a change when it has been quiet."
            .to_string(),
    };
    // Said in every mode, including the default. The block only ever spoke
    // when autonomy was *not* moving the ranking, so the one setting a reader
    // is most likely to be on — "ranking", the default — was the one the page
    // never named, and there was nothing on screen to tell it from "off".
    let mode = format!(
        "Autonomy is {}: {}",
        core.evolve.autonomous.as_str(),
        match core.evolve.autonomous.moves_ranking() {
            true => "the base may propose a ranking of its own and adopt it once it has earned it.",
            false => "the file is in force, and the base proposes nothing on its own.",
        }
    );
    // Three modes, and the difference between the last two is the one an
    // operator has to be able to see: only "full" moves the review threshold
    // and acts on the corpus, and neither of those is undone by taking a
    // generation back. "ranking" is the reversible half, and now actually is.
    let mode = match (
        core.evolve.autonomous.moves_ranking(),
        core.evolve.autonomous.acts_on_corpus(),
    ) {
        (true, true) => format!(
            "{mode} It may also merge, hide and shorten artifacts, and move the review \
             threshold. Those are not undone by taking a generation back."
        ),
        (true, false) => format!("{mode} It changes nothing in the corpus."),
        _ => mode,
    };
    let rehearsed = crate::eval::rehearsed::rehearsed_live(core, &live.id).await?;
    let rehearsed = if rehearsed.probes == 0 {
        "No probe has been rehearsed under this generation yet.".to_string()
    } else {
        format!(
            "On the base's own probes: {} of {} found, MRR {:.3}. A comparison between generations, not a score — a probe is the wording of a later capture, not a question anyone asked.",
            rehearsed.found, rehearsed.probes, rehearsed.mrr
        )
    };
    let history = core
        .store
        .generation_history(10)
        .await?
        .iter()
        .filter(|g| g.id != live.id)
        .map(|g| {
            let how = match (&g.run_id, &g.parent_id) {
                (Some(_), _) => format!(
                    "adopted by the base, promising MRR {:+.3}",
                    g.predicted.unwrap_or(0.0)
                ),
                (None, Some(_)) if g.predicted.is_some() => format!(
                    "adopted by the base on what the band earned, at a use rate of {:.2}",
                    g.predicted.unwrap_or(0.0)
                ),
                (None, Some(_)) => "set by hand".to_string(),
                (None, None) => "starting point".to_string(),
            };
            let ended = match g.state.as_str() {
                "reverted" => "taken back",
                "refused" => "refused on the base's own probes",
                _ => "superseded",
            };
            format!(
                "{} — {} · {} · {} — {}",
                ago(g.created_at),
                short(&g.id),
                params_str(&g.params),
                how,
                ended
            )
        })
        .collect();
    let actions = core
        .store
        .recent_actions(10)
        .await?
        .iter()
        .map(action_str)
        .collect();
    let rules = rules_str(core).await?;
    Ok(Some(EvolveView {
        actions,
        rules,
        suspended,
        mode,
        live: format!(
            "Live generation {} · since {}",
            short(&live.id),
            ago(live.created_at)
        ),
        params: params_str(&live.params),
        standing,
        rehearsed,
        history,
    }))
}

#[cfg(test)]
mod tests {
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn a_generation_says_whether_the_sitting_is_taking_part() {
        // A knob a generation can carry and the page cannot name is a knob
        // nobody can check against what they are reading.
        use crate::store::generations::GenerationParams;
        let on = GenerationParams {
            sitting_prime: true,
            ..Default::default()
        };
        assert!(
            super::params_str(&on).contains("sitting on"),
            "{}",
            super::params_str(&on)
        );
        let off = GenerationParams::default();
        assert!(
            super::params_str(&off).contains("sitting off"),
            "{}",
            super::params_str(&off)
        );
    }

    async fn insights(core: crate::core::Core) -> String {
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        body_of(res).await
    }

    /// Housekeeping's tables are anchors and nothing else: the label is the
    /// whole cell. Emptied for a passage they are links nobody can see or
    /// click, so a passage is listed by how its text opens — set as text, and
    /// without the subtitle repeating that same opening underneath it.
    #[tokio::test]
    async fn a_superseded_passage_is_listed_by_how_its_text_opens() {
        let core = crate::core::test_support::test_core().await;
        let src = core
            .store
            .insert_corpus("one\ntwo", "web", None)
            .await
            .unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                    title: Some("Kapitel 3".into()),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        let winner = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "Wie ein Journal steht".into(),
                    title: Some("Wie ein Journal steht".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        core.store
            .set_superseded_by(&p[0].id, Some(&winner[0].id))
            .await
            .unwrap();

        let html = insights(core).await;
        assert!(!html.contains("Kapitel 3"), "{html}");
        assert!(html.contains("Der Vorgang setzt voraus"), "{html}");
        assert!(
            html.contains("name-opening"),
            "the opening was set as a name: {html}"
        );
        assert_eq!(
            html.matches("Der Vorgang setzt voraus").count(),
            1,
            "the opening stood twice, once under itself: {html}"
        );
    }

    /// Five headings answered with a zero make a base with nothing wrong with
    /// it look like a backlog — the same reasoning the housekeeping summary
    /// already gives for collapsing its own empties into one sentence.
    #[tokio::test]
    async fn insights_over_an_empty_base_is_one_line_and_a_way_back() {
        let core = crate::core::test_support::test_core().await;
        let html = insights(core).await;
        assert!(
            html.contains("Nothing is held yet"),
            "one honest line about an empty base: {html}"
        );
        assert!(
            !html.contains("What this memory is like"),
            "no measures over nothing"
        );
        assert!(
            html.contains(r#"href="/ui""#),
            "and a way back to the one place there is anything to do"
        );
        // Only the measures are gated. A gap is a question the base could not
        // answer, which is exactly what an empty base produces, and the sweeps
        // run whether or not anything was ever captured — a page-wide guard
        // would hide both. What is noisy rather than absent goes behind the
        // disclosure at the foot instead.
        assert!(
            html.contains("What the machine is doing"),
            "what the machine is doing is true of an empty base too"
        );
    }

    /// A heading over a band that loads in empty is a claim that something is
    /// being measured when nothing is — the same reasoning `offer_rates`
    /// already follows on this page. It is the band that draws it, because the
    /// band is what swaps: gated from this side, on a read taken once at page
    /// render, the heading outlived the rows it was a heading for. See
    /// `_due.html` and `due::render`.
    #[tokio::test]
    async fn the_due_heading_is_the_bands_to_draw_and_not_this_pages() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_capture(
            crate::core::ingest::Capture::new("Remind me tomorrow to send the invoice", "ui")
                .with_intent(Some(crate::core::moments::Intent::Remind)),
        )
        .await
        .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let html = insights(core).await;
        assert!(
            !html.contains("<h2>Due</h2>"),
            "nothing is claimed before the band lands: {html}"
        );
        assert!(
            html.contains(r#"id="due""#),
            "the same band the workspace column shows: {html}"
        );
        assert!(
            html.contains(r#"head: "1""#),
            "and it is asked for with its heading: {html}"
        );
    }

    /// A closed disclosure with a neutral summary is not a report. The sweep
    /// failures and the last-error column are the only surfaces that say a
    /// background pipeline has fallen over, and once they moved in here an
    /// instance whose every embed job had failed for a week looked from the
    /// fold exactly like one with nothing to say.
    #[tokio::test]
    async fn a_failing_pipeline_is_not_something_the_page_keeps_to_itself() {
        let core = crate::core::test_support::test_core().await;
        let quiet = insights(core).await;
        assert!(
            quiet.contains(r#"<details class="machine">"#),
            "with nothing wrong the readout stays folded away: {quiet}"
        );

        let core = crate::core::test_support::test_core().await;
        core.store
            .record_sweep_run(
                "consolidate",
                crate::store::now(),
                "failed",
                r#"{"error":"the endpoint was down"}"#,
            )
            .await
            .unwrap();
        let html = insights(core).await;

        assert!(
            html.contains(r#"<details class="machine" open>"#),
            "a failing sweep opens the readout that reports it: {html}"
        );
        let summary = html
            .split_once(r#"<details class="machine" open>"#)
            .expect("the disclosure exists")
            .1
            .split_once("</summary>")
            .expect("and has a summary")
            .0;
        assert!(
            summary.contains("1 failed"),
            "and says so on the line that survives the fold: {summary}"
        );
    }

    /// Two questions, one page: what is in my memory and what needs me, versus
    /// what is the machine doing. The second is operator-grade — stage ids,
    /// target ids, raw error strings — and every user sees this page now.
    #[tokio::test]
    async fn the_machines_own_readout_is_behind_a_disclosure() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        let html = insights(core).await;

        let (above, inside) = html
            .split_once(r#"<details class="machine">"#)
            .expect("the disclosure exists");

        assert!(
            above.contains("What this memory is like"),
            "what is held stays above the fold"
        );
        // The counts sentence rather than a heading: the section carries the
        // disclosure's own name now, so the summary is the heading and the
        // body is what it was always for.
        assert!(
            !above.contains("embedded."),
            "the machine's own readout does not"
        );
        assert!(inside.contains("embedded."), "it is inside the disclosure");
        assert!(
            !html.contains(r#"<details class="machine" open"#),
            "and closed: nobody opened this page to read job counts"
        );
    }

    /// The measures read what the base already recorded.
    #[tokio::test]
    async fn the_measures_read_what_the_base_already_recorded() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        let html = insights(core).await;

        assert!(html.contains("What this memory is like"), "{html}");
        assert!(html.contains("Held"), "how much is held: {html}");
        assert!(html.contains("Use"), "how much use is standing: {html}");
        assert!(
            html.contains("never reached"),
            "the band that is the point: {html}"
        );
    }

    /// Nothing judged is not a score of zero.
    ///
    /// `0.00` beside "recall@10" reads as a measurement, and a base nobody has
    /// judged has not scored badly — it has not been measured. This is the one
    /// figure on the page whose absence must not look like a result.
    #[tokio::test]
    async fn an_unjudged_base_says_so_rather_than_reporting_zero() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        // Something held, because the measures are gated on that now: a base
        // with nothing in it has nothing to measure at all, and the rule this
        // test pins is the narrower one about a base that has content but no
        // verdicts on it.
        core.ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        let html = insights(core).await;
        assert!(html.contains("nothing judged yet"), "{html}");
        assert!(
            !html.contains(">0.00<"),
            "an unmeasured base reports a score: {html}"
        );
    }

    /// Read, never computed at request time.
    ///
    /// The first of the README's three rules holds here too: no embedding
    /// and no model call on a page you open to look at numbers.
    #[tokio::test]
    async fn the_measures_embed_nothing() {
        let (core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        core.ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        let before = embedder.calls();
        let _ = insights(core).await;
        assert_eq!(embedder.calls(), before, "the page embeds something");
    }

    /// The deck is gone: pairs are made at the moment of the search — a result
    /// read, a bar answered, a gap pressed on the rail — and its page answers
    /// like any other path nobody routed.
    #[tokio::test]
    async fn every_judge_route_is_gone() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        for path in ["/ui/judge", "/ui/judge/next"] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("cookie", &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn insights_names_the_live_generation_and_whether_it_is_watched() {
        let (core, parent) = crate::jobs::tune::test_support::adopted_and_watching().await;
        let live = core.store.live_generation().await.unwrap().unwrap();
        let body = insights(core).await;
        assert!(body.contains("under watch"), "{body}");
        assert!(body.contains("Live generation"), "{body}");
        assert!(
            body.contains(super::short(&live.id)),
            "the generation in force is named: {body}"
        );
        assert!(
            body.contains(super::short(&parent)),
            "and the one it replaced is in the history: {body}"
        );
        assert_ne!(
            super::short(&live.id),
            super::short(&parent),
            "two ids, two names"
        );
    }

    #[tokio::test]
    async fn the_evolve_block_tells_an_evidence_undo_from_an_operator_undo_and_says_what_the_rules_did()
     {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let (core, _) = crate::jobs::tune::test_support::adopted_and_watching().await;
        let row = |subject: &str, kind: Kind| NewAction {
            job: Job::Dedupe,
            kind,
            subject_id: subject.into(),
            survivor_id: Some("winner-1234abcd".into()),
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: None,
        };
        core.store
            .record_action(&row("loser-aaaa1111", Kind::Supersede))
            .await
            .unwrap();
        core.store
            .undo_action_on(
                "loser-aaaa1111",
                Kind::Supersede,
                UndoneBy::Evidence,
                "lost",
            )
            .await
            .unwrap();
        core.store
            .record_action(&row("loser-bbbb2222", Kind::Discard))
            .await
            .unwrap();
        core.store
            .undo_action_on(
                "loser-bbbb2222",
                Kind::Discard,
                UndoneBy::Operator,
                "button",
            )
            .await
            .unwrap();
        core.store
            .meta_set(
                crate::jobs::retract::LAST_RUN,
                r#"{"at":1,"reconsidered":3,"undone":1,"restored":2}"#,
            )
            .await
            .unwrap();

        let body = insights(core).await;
        assert!(
            body.contains("what the base did to the corpus (2)"),
            "{body}"
        );
        assert!(
            body.contains("hid aaaa1111 in favour of 1234abcd — taken back on evidence"),
            "{body}"
        );
        assert!(
            body.contains("discarded bbbb2222 — undone by you"),
            "{body}"
        );
        assert!(
            body.contains("reconsidered 3 of what it hid, took 1 back, and restored 2"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn last_night_says_what_the_sleep_did_and_lists_what_nothing_has_asked_for() {
        let (core, _) = crate::jobs::tune::test_support::adopted_and_watching().await;
        let live = core.store.live_generation().await.unwrap().unwrap().id;
        core.store
            .record_sleep_run(&crate::store::sleep_runs::SleepRun {
                id: crate::store::new_id(),
                started: crate::store::now() - 60,
                ended: crate::store::now(),
                stopped: "finished".into(),
                generation_id: live,
                integrated: 12,
                novel: 3,
                known: 8,
                conflicts: 1,
                rehearsed: 340,
                found: 300,
                refused: Some("gen-refused-abcd1234".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let body = insights(core).await;
        assert!(body.contains("Last night"), "{body}");
        assert!(body.contains("Integrated 12 — 3 new, 8 known."), "{body}");
        assert!(body.contains("Rehearsed 340 probes, 300 found."), "{body}");
        assert!(
            !body.contains("conflict"),
            "nothing writes conflicts, so the section must not count them: {body}"
        );
        assert!(
            body.contains("Refused a proposed ranking on the base&#39;s own probes."),
            "{body}"
        );
        // Six artifacts, one probed by the fixture, two opened by the
        // observations: three nothing has asked for.
        assert!(
            body.contains("unrehearsed (3) — nothing has asked for these"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_reaped_section_lists_what_is_buried_with_a_restore_button() {
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "an old note nobody needs".into(),
                    title: Some("Old note".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let id = made[0].id.clone();
        core.store
            .set_artifact_status(&id, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query("UPDATE artifacts SET retired_at = ? WHERE id = ?")
            .bind(crate::store::now() - 400 * 86_400)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.store
            .bury(
                &id,
                r#"{"reason":"nothing new in it"}"#,
                0,
                None,
                None,
                &crate::jobs::reap::test_support::row(&id),
            )
            .await
            .unwrap();

        let body = insights(core).await;
        // The section heading became the row's own word. See `SetAsideRow`.
        assert!(body.contains(">buried<"), "{body}");
        // Once, as buried. The burial keeps the artifact's status, and the
        // hidden list read the status alone, so the same artifact was also
        // listed as hidden and "still at its own link".
        assert_eq!(
            body.matches(&format!("/ui/ops/artifacts/{id}/reactivate"))
                .count(),
            1,
            "{body}"
        );
        assert!(body.contains("Old note"), "{body}");
        assert!(body.contains("nothing new in it"), "{body}");
        assert!(
            body.contains(&format!("/ui/ops/artifacts/{id}/reactivate")),
            "the restore button posts to the existing route: {body}"
        );
    }

    #[tokio::test]
    async fn a_suspended_base_says_so_before_anything_else() {
        let (core, _) = crate::jobs::tune::test_support::suspended().await;
        let body = insights(core).await;
        let suspended_at = body.find("Not moving").expect("said");
        let history_at = body.find("adopted").unwrap_or(usize::MAX);
        assert!(
            suspended_at < history_at,
            "the reason comes before the history"
        );
    }

    #[tokio::test]
    async fn a_base_that_never_moved_says_the_file_is_in_force() {
        let mut core = crate::core::test_support::test_core().await;
        core.evolve.autonomous = crate::config::Autonomy::Off;
        core.store
            .insert_corpus("some text", "web", None)
            .await
            .unwrap();
        let params = *core.ranking.read().unwrap();
        core.store
            .record_generation(&crate::store::generations::NewGeneration {
                params: params.into(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let body = insights(core).await;
        assert!(body.contains("Autonomy is off"), "{body}");
        assert!(!body.contains("under watch"), "{body}");
    }
}
