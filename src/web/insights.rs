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
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui::{
    SourceRow, fmt_duration, fmt_elapsed, fmt_time, row_subtitle, source_rows, sweep_label,
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
    /// Waiting judgements for the nav. See `state::judge_pending`.
    judge_pending: Option<i64>,
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
    /// Whether the `/ui/due` band, loaded separately, has anything to show —
    /// read with the same window it renders with, so the heading never sits
    /// over a band that comes back empty. A heading over no rows is a claim
    /// that something is being measured when nothing is, the same reasoning
    /// `offer_rates` follows above.
    has_due: bool,
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
            .gap_rows(tenant.core.embedder.model(), tenant.core.weak_below)
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

    Ok(HtmlTemplate(InsightsTemplate {
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
                    .feedback_stats(tenant.core.weak_below)
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
        judge_pending: crate::web::state::judge_pending(&tenant).await,
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
        has_due: {
            let now = tenant.core.clock.now();
            let horizon = now + tenant.core.time.horizon_hours as i64 * 3_600;
            !tenant.core.store.open_due(now, horizon).await?.is_empty()
        },
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use crate::web::test_support::{app_with_cookie, body_of};
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

    /// A heading over a band that loads in empty is a claim that something
    /// is being measured when nothing is — the same reasoning `offer_rates`
    /// already follows on this page.
    #[tokio::test]
    async fn a_base_with_nothing_due_carries_no_due_heading() {
        let core = crate::core::test_support::test_core().await;
        core.ingest("just a note, nothing to remind about", "web", None).await.unwrap();
        let html = insights(core).await;
        assert!(!html.contains("<h2>Due</h2>"), "{html}");
    }

    /// The reported gap: a reminder existed and nothing on the page that
    /// measures the base ever said so.
    #[tokio::test]
    async fn a_reminder_inside_the_horizon_gets_its_own_heading_and_band() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_capture(
            crate::core::ingest::Capture::new("Remind me tomorrow to send the invoice", "ui")
                .with_intent(Some(crate::core::moments::Intent::Remind)),
        )
        .await
        .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let html = insights(core).await;
        assert!(html.contains("<h2>Due</h2>"), "{html}");
        assert!(html.contains(r#"id="due""#), "the same band the workspace column shows: {html}");
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
}
