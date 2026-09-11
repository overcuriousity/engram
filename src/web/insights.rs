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
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

use crate::error::Result;
use crate::fmt::{ago, fmt_duration, fmt_elapsed, fmt_time};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::tenant::CanJudge;
use crate::web::ui::{SourceRow, row_label, row_subtitle, source_rows, sweep_label, tally_sweep};
use crate::web::ui_error::UiResult;

/// The retrieval measure, flattened for the template.
///
/// The two figures arrive as `f64` and are rendered to two places here rather
/// than in the markup: every decision this page makes is made in Rust, so the
/// template holds no logic and a change of precision touches one line.
struct Retrieval {
    recall_at_10: String,
    mrr: String,
    judged: i64,
    pending: i64,
    captured: i64,
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
        .route("/ui/insights/tune/{run_id}/apply", post(tune_apply))
        .route("/ui/ops", get(moved))
}

/// Work that hit something and is waiting to try again by itself.
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
pub(crate) struct SweepCount {
    n: i64,
    what: String,
}

/// One recorded run, as the history renders it.
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
    /// short list does not read as an empty queue when it is a capped one.
    more_pairs: i64,
    /// How much is held, and how densely.
    held: crate::store::insights::Held,
    /// How much use is standing on the base, bucketed in units of an open.
    used: Vec<crate::store::insights::Bucket>,
    /// recall@10 and MRR, read from the ranks judged searches actually gave.
    /// `None` where nothing is being recorded — an empty measure is worse than
    /// no measure, because a zero reads as a score.
    retrieval: Option<Retrieval>,
    /// What the sweeps have to say, rendered beside the retrieval figures the
    /// sweep replays. `None` for a user who could not press its button: the
    /// apply route is behind `CanJudge`, and a block offering what a press
    /// would refuse is a lie.
    tune: Option<TuneView>,
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
    /// Everything the base has set aside, in one list. See [`QueueRow`] for
    /// what this replaced and why.
    queue: Vec<QueueRow>,
    /// Any of the reads behind the queue hit its cap, so there are rows this
    /// page is not showing. Said out loud, because a list that stops without
    /// saying so reads as a list of everything there is.
    queue_capped: bool,
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
pub(crate) struct QueueRow {
    href: String,
    title: String,
    /// See `ui::RowLabel::named`. A label that is the artifact's own opening
    /// is set as text, not in the place a name would go.
    named: bool,
    /// What tells two rows with one title apart. Empty where nothing does.
    subtitle: String,
    /// The one-word name for what put this row here, as a badge.
    kind: &'static str,
    /// The sentence. Never a mechanism the reader has to already know: "written
    /// from 3 others" rather than "the dedupe pass wrote this".
    why: String,
    /// What the base put beside it: the sources a merge came from, the artifact
    /// a near-duplicate lost to, the capture a park collided with.
    beside: Vec<crate::web::ui::SourceRow>,
    /// A note under the row for the one thing that is not simply reversible.
    caveat: Option<String>,
    actions: Vec<QueueAction>,
}

/// One button on a queue row.
pub(crate) struct QueueAction {
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

impl QueueAction {
    fn new(action: String, label: &'static str, hint: &'static str) -> Self {
        Self {
            action,
            label,
            field: None,
            hint,
        }
    }
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
    let queue_capped = more_merged || more_superseded || more_deprecated || more_reaped;
    let mut queue: Vec<QueueRow> = Vec::new();
    for p_ in parked {
        queue.push(QueueRow {
            href: format!("/ui/corpora/{}", p_.id),
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
                QueueAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Replace the old one",
                    field: Some(("action", "replace")),
                    hint: "Keep this capture and retire the one beside it",
                },
                QueueAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Keep both",
                    field: Some(("action", "keep_both")),
                    hint: "Read this one too; both stay in the base",
                },
                QueueAction {
                    action: format!("/ui/ops/corpora/{}/resolve", p_.id),
                    label: "Discard this",
                    field: Some(("action", "discard")),
                    hint: "Drop this capture and keep the one beside it",
                },
            ],
        });
    }
    for s in stale {
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", s.id),
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
                QueueAction::new(
                    format!("/ui/ops/artifacts/{}/verify", s.id),
                    "Still accurate",
                    "Confirm this is still accurate — it resets the artifact's age, which search reads",
                ),
                QueueAction::new(
                    format!("/ui/ops/artifacts/{}/deprecate", s.id),
                    "Hide",
                    "Hide from results — the artifact is kept, and this can be undone",
                ),
            ],
        });
    }
    for m in merged {
        let n = m.sources.len();
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", m.id),
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
            actions: vec![QueueAction::new(
                format!("/ui/ops/merges/{}/undo", m.id),
                "Undo",
                "Put the sources back in results and retire this merge",
            )],
        });
    }
    for g in generated {
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", g.id),
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
            actions: vec![QueueAction::new(
                format!("/ui/ops/artifacts/{}/deprecate", g.id),
                "Hide",
                "Take it out of results and keep it",
            )],
        });
    }
    for s in superseded {
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", s.id),
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
            actions: vec![QueueAction::new(
                format!("/ui/ops/artifacts/{}/unsupersede", s.id),
                "Undo",
                "Return it to results",
            )],
        });
    }
    for d in deprecated {
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", d.id),
            named: d.named,
            title: d.title,
            subtitle: String::new(),
            kind: "hidden",
            why: "flagged stale with no replacement named — search skips it and Ask does not read it, and it is still at its own link".to_string(),
            beside: Vec::new(),
            caveat: None,
            actions: vec![QueueAction::new(
                format!("/ui/ops/artifacts/{}/reactivate", d.id),
                "Reactivate",
                "Return it to results",
            )],
        });
    }
    for g in reaped {
        queue.push(QueueRow {
            href: format!("/ui/artifacts/{}", g.id),
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
            actions: vec![QueueAction::new(
                format!("/ui/ops/artifacts/{}/reactivate", g.id),
                "Restore",
                "Return it to results and embed it again",
            )],
        });
    }

    // The column, read live rather than off the tenant snapshot, for the
    // reason `web::tenant::CanJudge` gives at length: an open tenant outlives
    // a grant, and the block and the gate on its button must agree.
    let tune = match tenant.core.store.control.user(&tenant.user.subject).await {
        Ok(Some(u)) if u.can_judge => Some(tune_view(&tenant, "").await?),
        _ => None,
    };

    Ok(HtmlTemplate(InsightsTemplate {
        tune,
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
                Some(Retrieval {
                    recall_at_10: format!("{:.2}", f.recall_at_10),
                    mrr: format!("{:.2}", f.mrr),
                    judged: f.judged,
                    pending: f.pending,
                    captured: f.captured,
                })
            }
            false => None,
        },
        pairs,
        more_pairs,
        gaps,
        retrying,
        queue,
        queue_capped,
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

// ── What the sweeps have to say ─────────────────────────────────────────────

/// A recommendation, ready to read and to take.
pub struct Rec {
    pub id: String,
    /// What would change and what it buys, in one line.
    pub line: String,
    /// The pairs that move under it. Mandatory, never folded away: an
    /// aggregate says something moved, and only this says what.
    pub diff: Vec<String>,
}

pub struct TuneView {
    pub rec: Option<Rec>,
    /// Why there is nothing to offer, when a sweep has run and found nothing.
    /// Empty before the first sweep, where the honest answer is silence.
    pub quiet: String,
    pub applied: Vec<String>,
    /// What the press just before this one did.
    pub flash: String,
}

#[derive(Template)]
#[template(path = "_tune.html")]
struct TuneTemplate {
    tune: Option<TuneView>,
}

fn cap_str(c: Option<usize>) -> String {
    c.map_or("none".to_string(), |n| n.to_string())
}

/// One line naming what changes and what it is worth.
///
/// Every figure is read off the run rather than recomputed: a number and the
/// settings that produced it travel together, which is the whole of what the
/// `eval_runs` row is for.
///
/// "Replayed over N pairs" leads the figures rather than trailing them. They
/// used to end the line, which put `MRR 0.50 → 0.60` immediately under the
/// Retrieval measure's own MRR with nothing between them — two numbers of one
/// name, one read from the ranks the searches actually gave and one from a
/// replay of those searches through a door that skips priming. Neither is
/// wrong; they are not the same quantity, and side by side they invited being
/// read as one.
fn describe(run: &crate::store::eval_runs::EvalRun) -> String {
    let moved = moved_knobs(&run.base_params, &run.best_params);
    let moved = match moved.is_empty() {
        // Nothing in the params differs. Not reachable from a recommendation —
        // the ladder does not offer a candidate equal to its base — but a row
        // can be read back from an older base, and "· replayed over 120 pairs"
        // with nothing before it is not a line.
        true => params_str(&run.best_params),
        false => moved.join(", "),
    };
    format!(
        "{moved} · replayed over {} pairs: MRR {:.2} → {:.2}, recall@10 {:.2} → {:.2}",
        run.pairs_used, run.base_mrr, run.best_mrr, run.base_recall, run.best_recall,
    )
}

/// Every swept knob whose value differs, as `name before → after`.
///
/// All nine, and that is the fix: this line used to name four of them, chosen
/// when four was all the sweep moved. `sitting_prime`, `prime_lift`,
/// `spread_max`, `rerank` and `review_min` joined the ladder afterwards and
/// nothing here learned about them, so an adopted sitting flip rendered as
/// "recency 0.05 → 0.05, cap 3 → 3, pool ×3 → ×3, half-life 180d → 180d" — a
/// change with no visible change, on the one line whose whole job is to say
/// what moved.
///
/// Only what moved, rather than all nine both sides. On a sweep that turns one
/// knob — which is what the ladder does — eight ninths of the full line is the
/// same number twice, and the reader has to find the pair that differs. That
/// is the same work the old line failed at, done by hand.
///
/// `params_str` below prints the full state and stays the place for that; it
/// is what the generation history renders, where there is no "before" to
/// compare against.
fn moved_knobs(
    a: &crate::store::generations::GenerationParams,
    b: &crate::store::generations::GenerationParams,
) -> Vec<String> {
    let on = |v: bool| if v { "on" } else { "off" };
    let mut out = Vec::new();
    if a.recency_weight != b.recency_weight {
        out.push(format!(
            "recency {:.2} → {:.2}",
            a.recency_weight, b.recency_weight
        ));
    }
    if a.per_source_cap != b.per_source_cap {
        out.push(format!(
            "cap {} → {}",
            cap_str(a.per_source_cap),
            cap_str(b.per_source_cap)
        ));
    }
    if a.candidate_multiplier != b.candidate_multiplier {
        out.push(format!(
            "pool ×{} → ×{}",
            a.candidate_multiplier, b.candidate_multiplier
        ));
    }
    if a.recency_half_life_days != b.recency_half_life_days {
        out.push(format!(
            "half-life {}d → {}d",
            a.recency_half_life_days, b.recency_half_life_days
        ));
    }
    if a.prime_lift != b.prime_lift {
        out.push(format!("lift {} → {}", a.prime_lift, b.prime_lift));
    }
    if a.sitting_prime != b.sitting_prime {
        out.push(format!(
            "sitting {} → {}",
            on(a.sitting_prime),
            on(b.sitting_prime)
        ));
    }
    if a.spread_max != b.spread_max {
        out.push(format!("spread {} → {}", a.spread_max, b.spread_max));
    }
    if a.rerank != b.rerank {
        out.push(format!("rerank {} → {}", on(a.rerank), on(b.rerank)));
    }
    if a.review_min != b.review_min {
        out.push(format!("review {:.2} → {:.2}", a.review_min, b.review_min));
    }
    out
}

fn rank_str(r: Option<usize>) -> String {
    r.map_or("not in the first ten".to_string(), |i| {
        format!("position {}", i + 1)
    })
}

async fn tune_view(tenant: &Tenant, flash: &str) -> Result<TuneView> {
    let rec = tenant
        .core
        .store
        .open_recommendation()
        .await?
        .map(|run| Rec {
            line: describe(&run),
            diff: run
                .diff
                .iter()
                .map(|d| format!("{} — {} → {}", d.query, rank_str(d.base), rank_str(d.new)))
                .collect(),
            id: run.id,
        });
    // Only where a sweep has actually run and come back empty. Before the
    // first one there is nothing to explain, and a line explaining nothing is
    // one more thing on a page that has enough.
    let quiet = match (&rec, tenant.core.store.latest_eval_run().await?) {
        (None, Some(last)) if !last.recommended => format!(
            "last sweep {}: no improvement found over {} pairs.",
            ago(last.created_at),
            last.pairs_used
        ),
        _ => String::new(),
    };
    let applied = tenant
        .core
        .store
        .applied_eval_runs(10)
        .await?
        .iter()
        .map(|r| {
            format!(
                "{} — {}",
                ago(r.applied_at.unwrap_or(r.created_at)),
                describe(r)
            )
        })
        .collect();
    Ok(TuneView {
        rec,
        quiet,
        applied,
        flash: flash.to_string(),
    })
}

// ── Last night ──────────────────────────────────────────────────────────────

/// The `_sleep.html` block: what the base did while nobody was there, in
/// words, and what nothing has ever asked for.
struct SleepView {
    /// One sentence chain per sleep, newest first.
    runs: Vec<String>,
    /// How long a base has to be quiet before it sleeps, for the empty state.
    idle_mins: i64,
    /// Artifacts nothing has asked for: the count, and the oldest few.
    unrehearsed_count: i64,
    unrehearsed: Vec<(String, String)>,
}

/// One sleep as a sentence chain. Every number a person can act on has a
/// page: conflicts are on the pair queue, adoptions and undos on the evolve
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
    s.push_str(&format!(
        "Integrated {} — {} new, {} known, {} conflict{} waiting for you. Rehearsed {} probe{}, {} found.",
        r.integrated,
        r.novel,
        r.known,
        r.conflicts,
        if r.conflicts == 1 { "" } else { "s" },
        r.rehearsed,
        if r.rehearsed == 1 { "" } else { "s" },
        r.found
    ));
    if let Some(a) = &r.adopted {
        s.push_str(&format!(" Adopted {}.", short(a)));
    }
    if let Some(a) = &r.reverted {
        s.push_str(&format!(" Took back {}.", short(a)));
    }
    if let Some(a) = &r.refused {
        s.push_str(&format!(" Refused {} on the base's own probes.", short(a)));
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
        // a fact about ranking, and the queue this used to write to makes
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

async fn sleep_view(core: &crate::core::Core) -> Result<Option<SleepView>> {
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
struct EvolveView {
    /// Why the loop is not moving, when it is not. Said before anything else.
    suspended: Option<String>,
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

async fn evolve_view(core: &crate::core::Core) -> Result<Option<EvolveView>> {
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
        _ if !core.evolve.autonomous.moves_ranking() => format!(
            "autonomy is {}: the file is in force, and the base proposes nothing on its own.",
            core.evolve.autonomous.as_str()
        ),
        _ => "set by hand or at boot; the base may propose a change when it has been quiet."
            .to_string(),
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

// ── Taking a recommendation live ────────────────────────────────────────────

/// The tuning block, redrawn, with a line about what just happened.
async fn tune_fragment(tenant: &Tenant, line: &str) -> UiResult<Response> {
    Ok(HtmlTemplate(TuneTemplate {
        tune: Some(tune_view(tenant, line).await?),
    })
    .into_response())
}

/// What an Apply writes: the running parameters, with every knob the run moved
/// set to what it recommends.
///
/// Not the run's `best_params` whole. A run stores the knobs that existed when
/// it was written, and a row from before a knob joined the ladder reads that
/// knob back as the shipped default — on both sides, which is how it says the
/// knob did not move. Applied whole, those defaults went into `config.toml`
/// over whatever the operator had set, for knobs the line above the button
/// never named. Moved is decided the way `moved_knobs` decides it for that
/// line, and the fields are destructured so a knob added later has to be
/// answered for here.
fn applied_over(
    current: crate::core::ranking::RankingParams,
    run: &crate::store::eval_runs::EvalRun,
) -> crate::core::ranking::RankingParams {
    let base = run.base_params;
    let crate::store::generations::GenerationParams {
        recency_weight,
        per_source_cap,
        candidate_multiplier,
        recency_half_life_days,
        prime_lift,
        spread_max,
        rerank,
        review_min,
        sitting_prime,
    } = run.best_params;
    let mut p = current;
    if base.recency_weight != recency_weight {
        p.recency_weight = recency_weight;
    }
    if base.per_source_cap != per_source_cap {
        p.per_source_cap = per_source_cap;
    }
    if base.candidate_multiplier != candidate_multiplier {
        p.candidate_multiplier = candidate_multiplier;
    }
    if base.recency_half_life_days != recency_half_life_days {
        p.recency_half_life_days = recency_half_life_days;
    }
    if base.prime_lift != prime_lift {
        p.prime_lift = prime_lift;
    }
    if base.spread_max != spread_max {
        p.spread_max = spread_max;
    }
    if base.rerank != rerank {
        p.rerank = rerank;
    }
    if base.review_min != review_min {
        p.review_min = review_min;
    }
    if base.sitting_prime != sitting_prime {
        p.sitting_prime = sitting_prime;
    }
    p
}

/// Apply the open recommendation: the file first, then the running parameters,
/// then the stamp.
///
/// The order is the guarantee. A hot swap the file does not carry would vanish
/// on the next restart, leaving the tuning history claiming a change that is no
/// longer in force — and the file is the one place an operator can read what
/// their server is doing.
async fn tune_apply(
    State(st): State<AppState>,
    CanJudge(tenant): CanJudge,
    Path(run_id): Path<String>,
) -> UiResult<Response> {
    let Some(run) = tenant.core.store.eval_run(&run_id).await? else {
        return Err(crate::error::Error::NotFound.into());
    };
    // A recommendation that was already taken, a run that never was one, or
    // one a later sweep has since spoken over: all three arrive from a page
    // left open, and none is a reason to write anything. Asked of the store
    // rather than of this row, so what the button may take is exactly what the
    // page may offer.
    let open = tenant.core.store.open_recommendation().await?;
    if open.as_ref().is_none_or(|o| o.id != run.id) {
        return tune_fragment(
            &tenant,
            "that sweep is not an open recommendation — nothing was changed.",
        )
        .await;
    }

    let current = *tenant.core.ranking.read().expect("ranking lock");
    let params = applied_over(current, &run);
    if let Err(e) = crate::config::write_ranking(&st.config_path, &params) {
        // Said here rather than raised: a read-only config file is an ordinary
        // thing to find out about, and the operator is looking at the button
        // they just pressed. Nothing was swapped and nothing was stamped, so
        // the recommendation stays open and can be applied once the file can
        // be written.
        tracing::warn!(error = %e, path = %st.config_path.display(), "config.toml not written");
        return tune_fragment(
            &tenant,
            "config.toml could not be written, so nothing was applied. \
             The recommendation is still here.",
        )
        .await;
    }
    *tenant.core.ranking.write().expect("ranking lock") = params;
    // Every ranking change is a named generation, or the observations written
    // after it are evidence about settings that are not running. Logged and
    // carried past on failure: the file and the parameters are already
    // changed, and the journal missing a row is the smaller wrong.
    match tenant.core.store.live_generation().await {
        Ok(Some(live)) => {
            if let Err(e) = crate::store::generations::restate_generation(
                &tenant.core.store,
                &live,
                params.into(),
            )
            .await
            {
                tracing::warn!(error = %e, "applied settings were not journaled as a generation");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "could not read the live generation"),
    }
    // The stamp is what closes the recommendation, so its answer is the one
    // thing here that must not be dropped. `false` is the second press of the
    // same button arriving while the first was still in flight: same run, same
    // parameters, so the file and the running settings say what this press
    // would have written anyway — but only one press gets to report a change.
    // An error is worse than either, and raising it would have answered a 500
    // to a request that did change the file and the parameters: the operator
    // would have read "nothing happened" about a server that is now running
    // settings its history does not mention.
    match tenant.core.store.mark_eval_run_applied(&run_id).await {
        // The environment is layered over the file, so where one of these keys
        // is set the write is real and the restart undoes it. Said now, beside
        // the button, rather than discovered months later as a history claiming
        // settings the server stopped running at its last boot.
        Ok(true) => {
            let line = match crate::config::ranking_keys_in_env().as_slice() {
                [] => "applied — the next search runs with these settings.".to_string(),
                keys => format!(
                    "applied — the next search runs with these settings, but {} is set in the \
                     environment and will overrule the file at the next restart.",
                    keys.join(" and ")
                ),
            };
            tune_fragment(&tenant, &line).await
        }
        Ok(false) => {
            tune_fragment(
                &tenant,
                "that sweep had already been applied — nothing changed.",
            )
            .await
        }
        Err(e) => {
            tracing::error!(error = %e, run = %run_id, "applied run not stamped");
            tune_fragment(
                &tenant,
                "these settings are live and written to config.toml, but the run could not be \
                 recorded as applied — it may be offered again.",
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::web::test_support::{app_with_cookie, app_with_cookie_ungranted, body_of};
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

    /// The recommendation line has to name the knob that moved.
    ///
    /// It used to print four of the nine the sweep turns, chosen when four was
    /// all it turned. `sitting_prime`, `prime_lift`, `spread_max`, `rerank` and
    /// `review_min` joined the ladder afterwards, so an adopted sitting flip
    /// rendered as "recency 0.05 → 0.05, cap 3 → 3, pool ×3 → ×3, half-life
    /// 180d → 180d" — a change with no visible change, on the one line whose
    /// whole job is to say what changed.
    #[test]
    fn the_recommendation_line_names_every_knob_that_moved() {
        use crate::store::generations::GenerationParams;
        let base = GenerationParams::default();

        let flip = GenerationParams {
            sitting_prime: !base.sitting_prime,
            ..base
        };
        let line = super::moved_knobs(&base, &flip).join(", ");
        assert!(line.contains("sitting"), "{line}");
        assert_eq!(
            super::moved_knobs(&base, &flip).len(),
            1,
            "and names nothing that stood still: {line}"
        );

        // The other four latecomers, each on its own.
        let cases: Vec<(GenerationParams, &str)> = vec![
            (
                GenerationParams {
                    rerank: !base.rerank,
                    ..base
                },
                "rerank",
            ),
            (
                GenerationParams {
                    spread_max: base.spread_max + 1,
                    ..base
                },
                "spread",
            ),
            (
                GenerationParams {
                    prime_lift: base.prime_lift + 1,
                    ..base
                },
                "lift",
            ),
            (
                GenerationParams {
                    review_min: base.review_min + 0.1,
                    ..base
                },
                "review",
            ),
        ];
        for (candidate, name) in cases {
            let line = super::moved_knobs(&base, &candidate).join(", ");
            assert!(line.contains(name), "{name} is not named in {line:?}");
        }

        assert!(
            super::moved_knobs(&base, &base).is_empty(),
            "nothing moved, nothing named"
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

    async fn post(app: &axum::Router, uri: &str, cookie: &str) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
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

    /// An app whose store already holds one recommendation, plus the path to
    /// the configuration file that app would rewrite.
    async fn tune_app(
        recommended: bool,
    ) -> (
        axum::Router,
        String,
        crate::core::Core,
        String,
        std::path::PathBuf,
    ) {
        let core = crate::core::test_support::test_core().await;
        // Something held: the measures — and the tune block beside them —
        // render only over a base with anything in it.
        core.ingest("raw for tuning", "web", None).await.unwrap();
        let base = crate::store::eval_runs::RunParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        let best = if recommended {
            crate::store::eval_runs::RunParams {
                recency_weight: 0.1,
                per_source_cap: None,
                ..Default::default()
            }
        } else {
            base
        };
        let run = core
            .store
            .record_eval_run(&crate::store::eval_runs::NewEvalRun {
                judged_count: 50,
                pairs_used: 12,
                pairs_skipped: 0,
                base,
                base_recall: 0.70,
                base_mrr: 0.50,
                best,
                best_recall: 0.80,
                best_mrr: 0.60,
                diff: vec![crate::store::eval_runs::DiffRow {
                    query: "the image will not mount".into(),
                    base: Some(5),
                    new: Some(1),
                }],
                recommended,
            })
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie, state) = crate::web::test_support::app_with_state(core).await;
        let path = state.config_path.as_ref().clone();
        (app, cookie, handle, run, path)
    }

    /// The gate, from the outside: a signed-in user without the grant is
    /// refused at the one route that writes `config.toml`, and is shown no
    /// block whose button that refusal would answer.
    #[tokio::test]
    async fn an_ungranted_user_gets_neither_the_button_nor_the_door() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("raw for tuning", "web", None).await.unwrap();
        let run = core
            .store
            .record_eval_run(&crate::store::eval_runs::NewEvalRun {
                judged_count: 50,
                pairs_used: 12,
                pairs_skipped: 0,
                base: crate::store::eval_runs::RunParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                base_recall: 0.70,
                base_mrr: 0.50,
                best: crate::store::eval_runs::RunParams {
                    recency_weight: 0.1,
                    per_source_cap: None,
                    ..Default::default()
                },
                best_recall: 0.80,
                best_mrr: 0.60,
                diff: vec![],
                recommended: true,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie_ungranted(core).await;

        let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        let page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = body_of(page).await;
        assert!(
            !html.contains("/ui/insights/tune/"),
            "the page offers a button its own gate refuses: {html}"
        );
    }

    #[tokio::test]
    async fn an_open_recommendation_is_offered_with_the_pairs_that_moved() {
        let (app, cookie, _core, run, _) = tune_app(true).await;
        let body = insights_of(&app, &cookie).await;
        assert!(body.contains(&format!("/ui/insights/tune/{run}/apply")));
        assert!(body.contains("recency"), "the line must name what changes");
        assert!(body.contains("cap"), "both knobs are named");
        assert!(body.contains("MRR 0.50 → 0.60"), "{body}");
        assert!(
            body.contains("what changes"),
            "the diff is the part that decides it, not an extra"
        );
        assert!(
            body.contains("the image will not mount"),
            "the moved pair is named by its own query"
        );
        assert!(
            body.contains("replayed over 12 pairs"),
            "the sweep's figures are named as a replay: {body}"
        );
    }

    async fn insights_of(app: &axum::Router, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        body_of(res).await
    }

    #[tokio::test]
    async fn applying_writes_the_file_swaps_the_parameters_and_stamps_the_run() {
        // All three or none: a swap the file does not carry vanishes on
        // restart, and a stamp without either is a history of things that did
        // not happen.
        let (app, cookie, core, run, path) = tune_app(true).await;
        let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);

        let live = *core.ranking.read().unwrap();
        assert_eq!(live.recency_weight, 0.1);
        assert_eq!(live.per_source_cap, None);

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("recency_weight = 0.1"), "{written}");
        assert!(written.contains("per_source_cap = 0"), "{written}");
        assert!(
            written.contains("# a comment the apply path must not eat"),
            "the operator's file came back as a machine's: {written}"
        );

        assert!(
            core.store
                .eval_run(&run)
                .await
                .unwrap()
                .unwrap()
                .applied_at
                .is_some()
        );
        assert!(core.store.open_recommendation().await.unwrap().is_none());
    }

    /// A run from before a knob joined the ladder reads that knob back as the
    /// shipped default on both sides. Applying it moves what it moved, and
    /// leaves what an operator set by hand where they set it.
    #[tokio::test]
    async fn applying_an_older_run_leaves_the_knobs_it_never_measured_alone() {
        let (app, cookie, core, run, path) = tune_app(true).await;
        // The row as a sweep that knew two knobs wrote it.
        sqlx::query("UPDATE eval_runs SET base_params = ?, best_params = ? WHERE id = ?")
            .bind(r#"{"recency_weight":0.05,"per_source_cap":3}"#)
            .bind(r#"{"recency_weight":0.1,"per_source_cap":null}"#)
            .bind(&run)
            .execute(&core.store.pool)
            .await
            .unwrap();
        {
            let mut r = core.ranking.write().unwrap();
            r.review_min = 0.84;
            r.spread_max = 5;
        }

        let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
        assert_eq!(res.status(), StatusCode::OK);

        let live = *core.ranking.read().unwrap();
        assert_eq!(live.recency_weight, 0.1);
        assert_eq!(live.per_source_cap, None);
        assert_eq!(
            live.review_min, 0.84,
            "a knob the run never measured was reset to its default"
        );
        assert_eq!(live.spread_max, 5);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("review_min = 0.84"), "{written}");
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
        assert!(
            body.contains("Integrated 12 — 3 new, 8 known, 1 conflict waiting for you."),
            "{body}"
        );
        assert!(body.contains("Rehearsed 340 probes, 300 found."), "{body}");
        assert!(
            body.contains("Refused abcd1234 on the base&#39;s own probes."),
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
        // The section heading became the row's own word. See `QueueRow`.
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
        assert!(body.contains("autonomy is off"), "{body}");
        assert!(!body.contains("under watch"), "{body}");
    }

    #[tokio::test]
    async fn applying_journals_the_change_as_a_generation() {
        // Every ranking change is a named generation, or the observations
        // written after it are evidence about settings that are not running.
        let (app, cookie, core, run, _) = tune_app(true).await;
        let params = *core.ranking.read().unwrap();
        let before = core
            .store
            .record_generation(&crate::store::generations::NewGeneration {
                params: params.into(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                ..Default::default()
            })
            .await
            .unwrap();

        post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;

        let live = core.store.live_generation().await.unwrap().unwrap();
        assert_ne!(live.id, before);
        assert_eq!(live.parent_id.as_deref(), Some(before.as_str()));
        assert_eq!(
            crate::core::ranking::RankingParams::from(live.params),
            *core.ranking.read().unwrap(),
            "the generation says what is running"
        );
        assert!(
            live.predicted.is_none(),
            "a hand-applied change is not watched"
        );
    }

    #[tokio::test]
    async fn applying_answers_with_the_block_it_replaces() {
        // htmx swaps `#judge-tune` by id: a reply that is not that block would
        // leave the recommendation on screen after it was taken.
        let (app, cookie, _core, run, _) = tune_app(true).await;
        let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
        let body = body_of(res).await;
        assert!(body.contains(r#"id="judge-tune""#), "{body}");
        assert!(body.contains("applied"), "{body}");
        assert!(!body.contains("/apply"), "it is still offering itself");
    }

    #[tokio::test]
    async fn a_run_that_is_not_an_open_recommendation_changes_nothing() {
        // Both arrive from a page left open: one was never a recommendation,
        // the other has already been taken.
        for second_press in [false, true] {
            let (app, cookie, core, run, path) = tune_app(second_press).await;
            let before = std::fs::read_to_string(&path).unwrap();
            if second_press {
                assert_eq!(
                    post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie)
                        .await
                        .status(),
                    StatusCode::OK
                );
            }
            let live_before = *core.ranking.read().unwrap();

            let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "a stale press is an answer, not a 500"
            );
            assert_eq!(*core.ranking.read().unwrap(), live_before);
            if !second_press {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            }
        }
    }

    #[tokio::test]
    async fn a_run_that_does_not_exist_is_a_404() {
        let (app, cookie, _core, _, _) = tune_app(true).await;
        assert_eq!(
            post(&app, "/ui/insights/tune/no-such-run/apply", &cookie)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn an_unwritable_config_leaves_the_running_parameters_alone() {
        // The whole apply or none of it. The recommendation stays open, so it
        // can be taken once the file can be written.
        let (app, cookie, core, run, path) = tune_app(true).await;
        std::fs::remove_file(&path).unwrap();
        let before = *core.ranking.read().unwrap();

        let res = post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "the operator is told, not 500'd"
        );

        assert_eq!(*core.ranking.read().unwrap(), before, "swapped anyway");
        assert!(
            core.store
                .eval_run(&run)
                .await
                .unwrap()
                .unwrap()
                .applied_at
                .is_none(),
            "stamped a change that was never made"
        );
        assert!(core.store.open_recommendation().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_sweep_that_found_nothing_says_so_rather_than_going_quiet() {
        // Silence reads as "no sweep has ever run", which is a different fact
        // and the wrong one.
        let (app, cookie, _core, _, _) = tune_app(false).await;
        let body = insights_of(&app, &cookie).await;
        assert!(body.contains("no improvement found"), "{body}");
        assert!(!body.contains("/apply"), "nothing to apply was offered");
    }

    #[tokio::test]
    async fn before_any_sweep_the_block_says_nothing_at_all() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("raw for tuning", "web", None).await.unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let body = insights_of(&app, &cookie).await;
        assert!(!body.contains("no improvement found"));
        assert!(!body.contains("/apply"));
        assert!(!body.contains("tuning history"));
    }

    #[tokio::test]
    async fn an_applied_change_stands_in_the_history_with_its_numbers() {
        // The provenance rule, made structural: a number without the settings
        // that produced it cannot be compared against anything.
        let (app, cookie, _core, run, _) = tune_app(true).await;
        post(&app, &format!("/ui/insights/tune/{run}/apply"), &cookie).await;

        let body = insights_of(&app, &cookie).await;
        assert!(body.contains("tuning history"), "{body}");
        assert!(body.contains("MRR 0.50 → 0.60"), "{body}");
        assert!(body.contains("cap 3 → none"), "{body}");
    }
}
