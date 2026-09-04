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
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::tenant::CanJudge;
use crate::web::ui::{
    SourceRow, ago, fmt_duration, fmt_elapsed, fmt_time, row_subtitle, source_rows, sweep_label,
    tally_sweep, title_of,
};

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
    /// Decisions waiting on a person. Empty renders nothing at all. Grouped,
    /// because one artifact against three others is one decision and arrived
    /// as three — see `group_pairs`.
    ///
    /// It used to sit on Capture, "where the work arrives". Capture is a verb
    /// now and not a page, and this was never work *with* the base anyway —
    /// it is work on it, which is what this page is.
    pairs: Vec<crate::web::ui::PairCluster>,
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
    /// Whether the ask door is open. See `state::ask_enabled`.
    ///
    /// The nav has no use for it any more — Ask is a verb on the box, not a
    /// place to go — but `_gaps.html` still offers "ask again" beside a hole,
    /// and that link must not exist where there is nothing to answer with.
    ask_enabled: bool,
    /// The holes, grouped and named by the sweep. Empty when feedback is off.
    gaps: Vec<crate::web::ui::GapGroup>,
    /// Open gaps the sweep has not grouped yet.
    loose: Vec<crate::web::ui::GapMember>,
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
    more_deprecated: bool,
    /// `TABLE_CAP`, so the line that says how many rows are showing says the
    /// number the code actually truncated to. Written out twice in the
    /// template, it drifted from the constant the first time either moved.
    table_cap: i64,
    /// `DEPRECATED_CAP`, for the same reason and for the one table that does
    /// not share `TABLE_CAP`.
    deprecated_cap: i64,
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

async fn page(tenant: Tenant) -> Result<Response> {
    use sqlx::Row;

    let (pairs, more_pairs) = crate::web::ui::pair_rows(&tenant).await?;
    let pairs = crate::web::ui::group_pairs(pairs);

    // Read, never computed: the page shows what the sweep grouped and named,
    // and whatever has been judged since sits under itself until the next
    // pass. Nothing here embeds or calls a model.
    let (gaps, loose) = if tenant.core.learn.enabled {
        let (rows, loose) = tenant
            .core
            .store
            .gap_rows(tenant.core.embedder.model(), tenant.core.weak_below())
            .await?;
        (
            rows.into_iter()
                .map(|r| crate::web::ui::GapGroup {
                    label: r.label,
                    members: r
                        .members
                        .into_iter()
                        .map(crate::web::ui::gap_member)
                        .collect(),
                })
                .collect(),
            loose.into_iter().map(crate::web::ui::gap_member).collect(),
        )
    } else {
        (vec![], vec![])
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
        let winner_title = match tenant.core.store.get_artifact(&winner_id).await {
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
        generated.push(GeneratedRow {
            title: title_of(&c),
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
            title: title_of(&c),
            id: c.id,
        })
        .collect();
    let more_deprecated = deprecated.len() > DEPRECATED_CAP as usize;
    deprecated.truncate(DEPRECATED_CAP as usize);

    // Read-only candidates: nothing here has been changed, only listed.
    let stale = tenant
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

    // The column, read live rather than off the tenant snapshot, for the
    // reason `web::tenant::CanJudge` gives at length: an open tenant outlives
    // a grant, and the block and the gate on its button must agree.
    let tune = match tenant.core.store.control.user(&tenant.user.subject).await {
        Ok(Some(u)) if u.can_judge => Some(tune_view(&tenant, "").await?),
        _ => None,
    };

    Ok(HtmlTemplate(InsightsTemplate {
        tune,
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
        ask_enabled: crate::web::state::ask_enabled(&tenant),
        pairs,
        more_pairs,
        gaps,
        loose,
        retrying,
        parked,
        superseded,
        merged,
        more_merged,
        more_superseded,
        more_deprecated,
        table_cap: TABLE_CAP,
        deprecated_cap: DEPRECATED_CAP,
        deprecated,
        stale,
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
    format!(
        "recency {:.2} → {:.2}, cap {} → {} · replayed over {} pairs: \
         MRR {:.2} → {:.2}, recall@10 {:.2} → {:.2}",
        run.base_params.recency_weight,
        run.best_params.recency_weight,
        cap_str(run.base_params.per_source_cap),
        cap_str(run.best_params.per_source_cap),
        run.pairs_used,
        run.base_mrr,
        run.best_mrr,
        run.base_recall,
        run.best_recall,
    )
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

// ── Taking a recommendation live ────────────────────────────────────────────

/// The tuning block, redrawn, with a line about what just happened.
async fn tune_fragment(tenant: &Tenant, line: &str) -> Result<Response> {
    Ok(HtmlTemplate(TuneTemplate {
        tune: Some(tune_view(tenant, line).await?),
    })
    .into_response())
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
) -> Result<Response> {
    let Some(run) = tenant.core.store.eval_run(&run_id).await? else {
        return Err(crate::error::Error::NotFound);
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

    let params: crate::core::ranking::RankingParams = run.best_params.into();
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
        assert!(html.contains("Nothing judged yet"), "{html}");
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
        };
        let best = if recommended {
            crate::store::eval_runs::RunParams {
                recency_weight: 0.1,
                per_source_cap: None,
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
                },
                base_recall: 0.70,
                base_mrr: 0.50,
                best: crate::store::eval_runs::RunParams {
                    recency_weight: 0.1,
                    per_source_cap: None,
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
