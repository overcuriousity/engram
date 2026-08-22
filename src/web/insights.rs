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

use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use crate::auth::Identity;
use crate::error::Result;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{
    SourceRow, fmt_duration, fmt_elapsed, fmt_time, row_subtitle, source_rows, sweep_label,
    tally_sweep, title_of,
};

/// The old door. It takes an `Identity` like every other `/ui` route: a
/// redirect that answers before the session is checked is a route that tells
/// an anonymous caller which paths exist.
async fn moved(_id: Identity) -> Response {
    Redirect::to("/ui/insights").into_response()
}

/// Rows of one housekeeping table before it says there are more.
///
/// These tables are read to answer "what happened to X", and the answer to
/// that is a search for X rather than a scroll — so the cap is stated and the
/// rest arrive as these are cleared, instead of growing a pager nobody would
/// page through.
const TABLE_CAP: i64 = 25;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/insights", get(page))
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
    /// When it was written and how it opens. Two artifacts can carry the same
    /// title — a merge of two documents that named a section identically
    /// produces exactly that — and a table of them is unreadable without
    /// something that differs between the rows.
    pub subtitle: String,
    pub winner_id: String,
    pub winner_title: String,
}

/// An artifact flagged stale with no specific replacement.
pub struct DeprecatedRow {
    pub id: String,
    pub title: String,
}

/// An active artifact nobody has confirmed or retrieved in a while.
pub struct StaleRow {
    pub id: String,
    pub title: String,
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
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
    /// Whether the ask door is open. See `state::ask_enabled`.
    ask_enabled: bool,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    artifact_count: i64,
    vector_count: u64,
    retrying: Vec<RetryingRow>,
    parked: Vec<ParkedRow>,
    superseded: Vec<SupersededRow>,
    /// Artifacts the dedupe pass wrote out of several others, with what they
    /// were written from and an undo.
    merged: Vec<MergedRow>,
    /// The list is capped; there are rows this page is not showing. Said out
    /// loud, because a table that stops without saying so reads as a table of
    /// everything there is.
    more_merged: bool,
    more_superseded: bool,
    /// `TABLE_CAP`, so the line that says how many rows are showing says the
    /// number the code actually truncated to. Written out twice in the
    /// template, it drifted from the constant the first time either moved.
    table_cap: i64,
    deprecated: Vec<DeprecatedRow>,
    stale: Vec<StaleRow>,
    /// `None` when nothing is being learned, which renders nothing at all: a
    /// count of links on a base that records no searches is a line about a
    /// feature that is switched off.
    links: Option<crate::store::links::LinkCounts>,
    /// Artifacts written from pursuits, newest first, each one click from
    /// deprecated.
    generated: Vec<GeneratedRow>,
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

/// One generated artifact on Ops.
pub(crate) struct GeneratedRow {
    id: String,
    title: String,
    subtitle: String,
    cues: Vec<String>,
    sources: Vec<SourceRow>,
}

pub(crate) struct MergedRow {
    id: String,
    title: String,
    /// See `SupersededRow::subtitle`: what tells two rows with one title apart.
    subtitle: String,
    /// What it was written from, in the order the lineage stores them.
    sources: Vec<SourceRow>,
    /// True when a source has been deleted since, so the artifact claims less
    /// provenance than its text carries.
    orphaned: bool,
}

async fn page(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use sqlx::Row;

    let artifact_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&st.core.store.pool)
        .await?
        .get("n");

    // Not a queue of chores: work that hit something and is waiting to try
    // again on its own. Nothing here needs a person.
    let retrying: Vec<RetryingRow> = st
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
    for c in st.core.store.parked_corpora(50).await? {
        let other_id = c.near_dupe_of.clone().unwrap_or_default();
        let other_title = match st.core.store.get_corpus(&other_id).await {
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
    for c in st.core.store.superseded_artifacts(TABLE_CAP + 1).await? {
        let winner_id = c.superseded_by.clone().unwrap_or_default();
        let winner_title = match st.core.store.get_artifact(&winner_id).await {
            Ok(w) => title_of(&w),
            Err(_) => "(deleted)".to_string(),
        };
        superseded.push(SupersededRow {
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            id: c.id,
            winner_id,
            winner_title,
        });
    }

    let mut merged = Vec::new();
    let merged_chunks = st.core.store.merged_artifacts(TABLE_CAP + 1).await?;
    // One lineage call per page, not one per row: `roots_of` takes the batch.
    let merged_ids: Vec<String> = merged_chunks.iter().map(|c| c.id.clone()).collect();
    let roots = st
        .core
        .store
        .roots_of(&merged_ids)
        .await
        .unwrap_or_default();
    for c in merged_chunks {
        let sources = source_rows(
            &st.core.store,
            &c.id,
            roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        merged.push(MergedRow {
            orphaned: c.flags.iter().any(|f| f == "orphaned_source"),
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            id: c.id,
            sources,
        });
    }

    let more_merged = merged.len() > TABLE_CAP as usize;
    merged.truncate(TABLE_CAP as usize);

    let mut generated = Vec::new();
    let gen_chunks = st.core.store.synthesized_artifacts(TABLE_CAP).await?;
    let gen_ids: Vec<String> = gen_chunks.iter().map(|c| c.id.clone()).collect();
    let gen_roots = st.core.store.roots_of(&gen_ids).await.unwrap_or_default();
    for c in gen_chunks {
        let sources = source_rows(
            &st.core.store,
            &c.id,
            gen_roots.get(&c.id).map(Vec::as_slice).unwrap_or_default(),
        )
        .await;
        generated.push(GeneratedRow {
            title: title_of(&c),
            subtitle: row_subtitle(&c),
            cues: c.cues.clone(),
            id: c.id,
            sources,
        });
    }
    let pursuit_enabled = st.core.learn.enabled;
    let recent = match pursuit_enabled {
        true => st.core.store.recent_pursuits(50).await?,
        false => Vec::new(),
    };
    let pursuit_recent = recent.len();
    // The ones the sentence below can honestly point at. `unsatisfied` is how a
    // run of searches *ended*, and a capture that answers one afterwards leaves
    // that word alone deliberately — coverage never rewrites what happened — so
    // counting the state sent the operator to a gap list that had already
    // dropped half of them.
    let on_the_gap_list = match pursuit_enabled {
        true => st
            .core
            .store
            .open_pursuit_gap_ids(st.core.embedder.model())
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
    let day = st
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
    let sweep_history: Vec<SweepRunRow> = st
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

    let deprecated = st
        .core
        .store
        .artifacts_by_status(crate::store::artifacts::ArtifactStatus::Deprecated, 50)
        .await?
        .into_iter()
        .map(|c| DeprecatedRow {
            title: title_of(&c),
            id: c.id,
        })
        .collect();

    // Read-only candidates: nothing here has been changed, only listed.
    let stale = st
        .core
        .stale_candidates(50)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "no stale candidates for ops");
            vec![]
        })
        .into_iter()
        .map(|r| StaleRow {
            title: r.title.unwrap_or_else(|| markdown::snippet(&r.text, 60)),
            id: r.artifact_id,
            last_verified: r
                .last_verified_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".to_string()),
        })
        .collect();

    Ok(HtmlTemplate(InsightsTemplate {
        judge_pending: crate::web::state::judge_pending(&st).await,
        ask_enabled: crate::web::state::ask_enabled(&st),
        retrying,
        parked,
        superseded,
        merged,
        more_merged,
        more_superseded,
        table_cap: TABLE_CAP,
        deprecated,
        stale,
        job_counts: st.core.store.job_counts().await?,
        oldest_pending_secs: st.core.store.oldest_pending_age().await?,
        artifact_count,
        // Qdrant being briefly unreachable must not blank the ops page, which
        // is exactly where you look when something is wrong.
        vector_count: st.core.vectors.count().await.unwrap_or(0),
        links: match st.core.associating() {
            true => Some(st.core.store.link_counts().await?),
            false => None,
        },
        generated,
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
        offer_rates: match st.core.recommends() {
            true => st
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
