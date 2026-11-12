//! Insights: what the base did on its own (each with its undo) and aggregate measures.
//! `/ui/ops` redirects here because it is bookmarked; the `POST /ui/ops/...` actions stay in `ui.rs`.

use crate::tenants::Tenant;
use askama::Template;
use axum::Router;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use crate::error::Result;
use crate::fmt::{ago, fmt_duration, fmt_elapsed, fmt_time};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui::{SourceRow, row_label, row_subtitle, source_rows, sweep_label, tally_sweep};
use crate::web::ui_error::UiResult;

/// Figures rendered to two places here, so the template holds no logic.
/// Not an `Option`: all three states render the same two rows.
struct Retrieval {
    recall_at_10: String,
    mrr: String,
    /// Rendered `|safe`: built here, its only variable part a count.
    note: String,
}

/// Takes an `Identity`: a redirect before the session check tells anonymous callers which paths exist.
async fn moved(_: Tenant) -> Response {
    Redirect::to("/ui/insights").into_response()
}

/// These tables answer "what happened to X", which is a search, so the cap is stated instead of paged.
const TABLE_CAP: i64 = 25;

/// Deeper: every UI search excludes deprecated rows, so this list is the only way back to them.
const DEPRECATED_CAP: i64 = 50;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/insights", get(page))
        .route("/ui/ops", get(moved))
}

#[derive(serde::Serialize)]
pub struct RetryingRow {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub due: String,
    pub last_error: String,
}

pub struct SupersededRow {
    pub id: String,
    pub title: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    /// Two artifacts can share a title (merges), so the rows need something that differs.
    pub subtitle: String,
    pub winner_id: String,
    pub winner_title: String,
    /// Whether `winner_title` is a given name; the winner can be a passage opening or `(deleted)`.
    pub winner_named: bool,
}

pub struct DeprecatedRow {
    pub id: String,
    pub title: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
}

pub struct GraveRow {
    pub id: String,
    pub title: String,
    /// Always true: the graveyard keeps the title with no provenance to say whether it was a name.
    pub named: bool,
    pub ago: String,
    pub reason: Option<String>,
}

/// One phrase of the last day: "412 links forgotten".
#[derive(serde::Serialize)]
pub(crate) struct SweepCount {
    n: i64,
    what: String,
}

#[derive(serde::Serialize)]
pub(crate) struct SweepRunRow {
    when: String,
    /// The stage in words; the identifier (e.g. `arm_dedupe`) is on the cell's `title` for grepping.
    stage: String,
    stage_id: String,
    /// Empty unless it failed, in which case it is why.
    error: String,
    took: String,
    /// Already worded; empty for a run that did nothing.
    counts: Vec<SweepCount>,
}

#[derive(Template)]
#[template(path = "insights.html")]
struct InsightsTemplate {
    held: crate::store::insights::Held,
    /// Bucketed in units of an open.
    used: Vec<crate::store::insights::Bucket>,
    /// recall@10 and MRR from judged searches; an em dash when not recording, since a zero reads as a score.
    retrieval: Retrieval,
    /// `None` before a generation exists.
    sleep: Option<SleepView>,
    /// `None` before a generation exists.
    evolve: Option<EvolveView>,
    job_counts: Vec<(String, i64)>,
    oldest_pending_secs: Option<i64>,
    artifact_count: i64,
    vector_count: u64,
    retrying: Vec<RetryingRow>,
    /// What the base did on its own and left an undo for; see [`SetAsideRow`].
    set_aside: Vec<SetAsideRow>,
    /// A cap was hit; said, since a list that stops silently reads as complete.
    set_aside_capped: bool,
    /// `None` when nothing is learned, which renders nothing.
    links: Option<crate::store::links::LinkCounts>,
    /// A count, not a table: unsatisfied pursuits are worked on by the base, written-up ones are `generated` rows.
    pursuit_enabled: bool,
    pursuit_recent: usize,
    pursuit_unsatisfied: usize,
    /// Summed over twenty-four hours: units on their own periods form no single cycle.
    last_day: Vec<SweepCount>,
    /// Said separately: a summary of what got done cannot report what did not.
    last_day_failures: usize,
    /// Newest first, so a week-long failure is visible.
    sweep_history: Vec<SweepRunRow>,
    /// Empty when the offer is off or has never offered; then no table is drawn.
    offer_rates: Vec<crate::store::pursuits::OfferRate>,
}

impl InsightsTemplate {
    fn section(&self) -> &'static str {
        "insights"
    }
}

pub(crate) struct GeneratedRow {
    id: String,
    title: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    subtitle: String,
    cues: Vec<String>,
    sources: Vec<SourceRow>,
}

pub(crate) struct MergedRow {
    id: String,
    title: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    /// See `SupersededRow::subtitle`.
    subtitle: String,
    /// In the order the lineage stores them.
    sources: Vec<SourceRow>,
    /// A source was deleted since, so the artifact claims less provenance than its text carries.
    orphaned: bool,
}

/// One thing the base did on its own, with the button that takes it back.
/// `kind` is the badge; `why` is said per row because it differs per row.
pub(crate) struct SetAsideRow {
    href: String,
    /// The artifact for every kind, `merged` included; a row is identified by `kind` and `subject_id` together.
    pub(crate) subject_id: String,
    /// The same as `subject_id` on every row, named apart so a client need not know that.
    pub(crate) artifact_id: String,
    pub(crate) title: String,
    /// See `ui::RowLabel::named`.
    pub(crate) named: bool,
    /// Empty where nothing tells two same-titled rows apart.
    pub(crate) subtitle: String,
    /// The one-word name for what put this row here, as a badge.
    pub(crate) kind: &'static str,
    /// Never a mechanism the reader must know: "written from 3 others", not "the dedupe pass".
    pub(crate) why: String,
    /// The sources of a merge, or the artifact a near-duplicate lost to.
    pub(crate) beside: Vec<crate::web::ui::SourceRow>,
    /// For the one thing that is not simply reversible.
    pub(crate) caveat: Option<String>,
    actions: Vec<SetAsideAction>,
}

pub(crate) struct SetAsideAction {
    action: String,
    label: &'static str,
    /// The long form for a pointer and a screen reader; the label carries the short one.
    hint: &'static str,
}

impl SetAsideAction {
    fn new(action: String, label: &'static str, hint: &'static str) -> Self {
        Self {
            action,
            label,
            hint,
        }
    }
}

/// Shared with the JSON door, so both list the same undoable rows.
pub(crate) async fn set_aside_rows(tenant: &Tenant) -> Result<(Vec<SetAsideRow>, bool)> {
    let mut superseded = Vec::new();
    // One past the cap, so the page can say it is capped.
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
    // One lineage call per page: `roots_of` takes the batch.
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

    // One past the cap, as above.
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

    // One past the cap, as above.
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

    // Newest kind of work first: written, then hidden, then buried.
    let set_aside_capped = more_merged || more_superseded || more_deprecated || more_reaped;
    let mut set_aside: Vec<SetAsideRow> = Vec::new();
    for m in merged {
        let n = m.sources.len();
        set_aside.push(SetAsideRow {
            href: format!("/ui/artifacts/{}", m.id),
            subject_id: m.id.clone(),
            artifact_id: m.id.clone(),
            named: m.named,
            title: m.title,
            subtitle: m.subtitle,
            kind: "merged",
            why: format!(
                "written from {n} artifact{}, which are still stored — undoing brings them back and retires this",
                if n == 1 { "" } else { "s" }
            ),
            beside: m.sources,
            // Not data loss: a claim of provenance the artifact can no longer support.
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
            artifact_id: g.id.clone(),
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
            artifact_id: s.id.clone(),
            named: s.named,
            title: s.title,
            subtitle: s.subtitle,
            kind: "hidden",
            why: "near-identical to the one beside it, so it is kept out of results — still stored, still readable".to_string(),
            beside: vec![crate::web::ui::SourceRow {
                id: s.winner_id,
                title: s.winner_title,
                // Carried from `row_label`, which may have answered a passage opening or `(deleted)`.
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
            artifact_id: d.id.clone(),
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
            artifact_id: g.id.clone(),
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
    let artifacts: i64 = sqlx::query_scalar!("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?;
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

/// Disclosure, not control: nothing in it asks for a decision.
#[derive(serde::Serialize)]
pub(crate) struct Report {
    pub sleep: Option<SleepView>,
    pub evolve: Option<EvolveView>,
    /// Null while `[learn]` is off.
    pub pursuits: Option<(usize, usize)>,
}

pub(crate) async fn report(tenant: &Tenant) -> Result<Report> {
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
    })
}

async fn page(tenant: Tenant) -> UiResult<Response> {
    let artifact_count: i64 = sqlx::query_scalar!("SELECT COUNT(*) AS n FROM artifacts")
        .fetch_one(&tenant.core.store.pool)
        .await?;

    // Work waiting to retry on its own; nothing here needs a person.
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
    // Counted off the gap list, not the state: a later capture answering one leaves `unsatisfied` alone.
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
        // Only where searches are recorded; otherwise there is nothing to say, not 0.00.
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
        retrying,
        set_aside,
        set_aside_capped,
        job_counts: tenant.core.store.job_counts().await?,
        oldest_pending_secs: tenant.core.store.oldest_pending_age().await?,
        artifact_count,
        // Qdrant briefly unreachable must not blank the page.
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
        // The last month: a weekly pattern needs weeks. A failure must not blank the page.
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

#[derive(serde::Serialize)]
pub(crate) struct SleepView {
    runs: Vec<String>,
    /// For the empty state.
    idle_mins: i64,
    unrehearsed_count: i64,
    unrehearsed: Vec<(String, String)>,
}

/// Every number with an undo has a place on this page.
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
        "Integrated {} — {} new, {} known. Rehearsed {} probe{}, {} found.",
        r.integrated,
        r.novel,
        r.known,
        r.rehearsed,
        if r.rehearsed == 1 { "" } else { "s" },
        r.found
    ));
    // Without the id: the generation is spelled out under Ranking.
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
        // "Saw", not "Filed": retrieval competition is about ranking, not meaning.
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

#[derive(serde::Serialize)]
pub(crate) struct EvolveView {
    /// Said before anything else.
    suspended: Option<String>,
    /// Named in every mode, the default included.
    mode: String,
    live: String,
    /// As one line of `name value` pairs, folded on the page.
    params: String,
    standing: String,
    /// Said as a comparison, not a score.
    rehearsed: String,
    /// Newest first, each with how it came to be and how it ended.
    history: Vec<String>,
    /// Newest first, evidence undos told apart from operator undos.
    actions: Vec<String>,
    /// `None` where the rules have not run yet.
    rules: Option<String>,
}

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

/// As the pass wrote it to `meta`.
async fn rules_str(core: &crate::core::Core) -> Result<Option<String>> {
    let Some(raw) = core.store.meta_get(crate::jobs::retract::LAST_RUN).await? else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    Ok(Some(format!(
        "Last quiet period, {}: {} reconsidered, {} taken back, {} restored.",
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

/// Ids are ULIDs, so two minted together share a head; the tail tells them apart.
pub(crate) fn short(id: &str) -> &str {
    &id[id.len().saturating_sub(8)..]
}

pub(crate) async fn evolve_view(core: &crate::core::Core) -> Result<Option<EvolveView>> {
    let Some(live) = core.store.live_generation().await? else {
        return Ok(None);
    };
    let suspended = match crate::eval::anchor::agreement(core).await? {
        Some(a) if !crate::eval::anchor::trustworthy(&a) => Some(format!(
            "Suspended: its evidence agreed with your verdicts {} time{} and disagreed {}, no better than chance.",
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
                    "adopted and held: {} positive of {} observations, against {} of {} before.",
                    new.positives, new.observations, old.positives, old.observations
                )
            } else {
                format!(
                    "under watch: promised MRR {:+.3}; {} positive of {} observations, against {} of {} before.",
                    predicted, new.positives, new.observations, old.positives, old.observations
                )
            }
        }
        _ => "set by hand or at boot.".to_string(),
    };
    // Said in every mode, so "ranking" can be told from "off".
    let mode = format!(
        "Autonomy is {}: {}",
        core.evolve.autonomous.as_str(),
        match core.evolve.autonomous.moves_ranking() {
            true => "the base may propose and adopt a ranking.",
            false => "the config file is in force.",
        }
    );
    // Only "full" moves the review threshold and acts on the corpus; taking a generation back undoes neither.
    let mode = match (
        core.evolve.autonomous.moves_ranking(),
        core.evolve.autonomous.acts_on_corpus(),
    ) {
        (true, true) => format!(
            "{mode} It may also merge, hide, shorten and move the review threshold; \
             taking a generation back does not undo those."
        ),
        (true, false) => format!("{mode} It changes nothing in the corpus."),
        _ => mode,
    };
    let rehearsed = crate::eval::rehearsed::rehearsed_live(core, &live.id).await?;
    let rehearsed = if rehearsed.probes == 0 {
        "No probe has been rehearsed under this generation yet.".to_string()
    } else {
        format!(
            "Own probes: {} of {} found, MRR {:.3}.",
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
        // Only the measures are gated; due reminders and sweeps show regardless.
        assert!(
            html.contains("What the machine is doing"),
            "what the machine is doing is true of an empty base too"
        );
    }

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
            html.contains(r#""head": "1""#),
            "and it is asked for with its heading: {html}"
        );
    }

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

    #[tokio::test]
    async fn an_unjudged_base_says_so_rather_than_reporting_zero() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
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

    #[tokio::test]
    async fn insights_asks_nothing_of_anyone() {
        use crate::store::actions::{Job, Kind, NewAction};
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;

        let ids =
            crate::web::test_support::artifacts(&core, &["the timeout is 30", "the timeout is 90"])
                .await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pid = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
            .id;
        core.store
            .set_pair_state(
                pid,
                crate::store::pairs::PairState::Contradiction,
                Some("30 against 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();

        let first = core
            .store
            .insert_corpus("mount the volume first", "web", None)
            .await
            .unwrap();
        let second = core
            .store
            .insert_corpus("mount the volume first, then write", "web", None)
            .await
            .unwrap();
        core.store
            .set_near_dupe(&second.id, Some(&first.id), Some(0.95))
            .await
            .unwrap();

        core.ingest("an old note nobody reads", "web", None)
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        for id in core.store.list_all_artifact_ids().await.unwrap() {
            core.vectors
                .set_last_verified_at(&id, 1, false)
                .await
                .unwrap();
        }
        assert!(
            !core.stale_candidates(10).await.unwrap().is_empty(),
            "the fixture has no unverified artifact to leave off the page"
        );

        let ask = core
            .store
            .record_ask(crate::store::asks::NewAsk {
                question: "how do ticks work".into(),
                filters: "{}".into(),
                query_vec: vec![1.0, 0.0],
                embed_model: core.embedder.model().into(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                ..Default::default()
            })
            .await
            .unwrap();
        core.store
            .judge_ask(&ask, crate::store::asks::AskVerdict::NothingHere)
            .await
            .unwrap();

        let roots = crate::web::test_support::artifacts(&core, &["left half", "right half"]).await;
        let merged = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "left half and right half".into(),
                    title: Some("Both halves".into()),
                    ..Default::default()
                },
                &roots,
            )
            .await
            .unwrap();
        core.store
            .record_action(&NewAction {
                job: Job::Dedupe,
                kind: Kind::Merge,
                subject_id: roots[0].clone(),
                survivor_id: Some(merged.id.clone()),
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();

        let html = insights(core).await;
        assert!(!html.contains("Needs you"), "{html}");
        assert!(!html.contains("Knowledge gaps"), "{html}");
        assert!(!html.contains("Still accurate"), "{html}");
        assert!(!html.contains("Keep both"), "{html}");
        assert!(!html.contains("/ui/ops/pairs/"), "{html}");
        assert!(!html.contains("/resolve"), "{html}");
        assert!(!html.contains("/ui/gaps/"), "{html}");
        assert!(html.contains("What the base did"), "{html}");
        assert!(html.contains("Undo"), "the merge keeps its undo: {html}");
        assert!(
            html.contains(&format!("/ui/ops/merges/{}/undo", merged.id)),
            "and the undo is the merge's own: {html}"
        );
    }

    #[tokio::test]
    async fn the_decision_routes_are_gone() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        for (method, uri) in [
            ("POST", "/api/v1/pairs/1/supersede"),
            ("POST", "/api/v1/pairs/1/synthesize"),
            ("POST", "/api/v1/pairs/1/dismiss"),
            ("POST", "/api/v1/pairs/1/discard"),
            ("GET", "/api/v1/pairs"),
            ("GET", "/api/v1/gaps"),
            ("POST", "/api/v1/gaps/forget"),
            ("POST", "/api/v1/gaps/ask/x/dismiss"),
            ("POST", "/api/v1/artifacts/x/verify"),
            ("POST", "/api/v1/artifacts/x/reviewed"),
            ("POST", "/api/v1/corpora/x/resolve"),
            ("POST", "/ui/ops/pairs/1/supersede"),
            ("POST", "/ui/ops/pairs/1/synthesize"),
            ("POST", "/ui/ops/pairs/1/dismiss"),
            ("POST", "/ui/ops/pairs/1/discard"),
            ("POST", "/ui/ops/corpora/x/resolve"),
            ("POST", "/ui/ops/artifacts/x/verify"),
            ("POST", "/ui/artifacts/x/reviewed"),
            ("POST", "/ui/gaps/forget"),
            ("POST", "/ui/gaps/ask/x/dismiss"),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("cookie", &cookie)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    res.status(),
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {uri} answered {}",
                res.status()
            );
        }
    }

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
            body.contains("3 reconsidered, 1 taken back, 2 restored"),
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
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - 400 * 86_400,
            &id
        )
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
        assert!(body.contains(">buried<"), "{body}");
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
