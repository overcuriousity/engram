//! The decisions a person makes about the base, as JSON.
//!
//! Pair review, gaps, the insights read and the undo actions — the surfaces
//! where somebody decides rather than reads. Every one of them shares the core
//! call its `/ui/ops` sibling uses, so the two doors cannot answer one press
//! differently.
//!
//! What does not cross, and is not an oversight: applying a tuning
//! recommendation, which is the one route that writes `config.toml`. Insights
//! says here what the sweep recommends; the press that adopts it stays on the
//! web, where the person who has `can_judge` already is.

use crate::error::Result;
use crate::tenants::Tenant;
use crate::web::page::Page;
use crate::web::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

/// Which side of a pair survives. Absent means the one the judge proposed,
/// which is what a confirmation on a proposed supersede sends.
#[derive(serde::Deserialize, Default)]
pub struct Keep {
    #[serde(default)]
    pub keep: Option<String>,
}

/// One side of a pair, as a client draws it.
#[derive(serde::Serialize)]
pub struct PairSide {
    pub id: String,
    pub label: String,
    /// Whether `label` is a name somebody gave — see `ui::RowLabel`.
    pub named: bool,
    /// Enough of it to decide by. Following a link out of the review leaves
    /// the queue and comes back to a card whose other half must be remembered,
    /// which is two readings with a navigation between them, not a comparison.
    pub excerpt: String,
}

/// One open pair, and only what somebody has actually established about it.
///
/// Three of these fields exist to stop a card claiming more than was found.
/// `unjudged` means the sweep filed this on a cosine score and nothing has
/// read it since, so "these two cover the same ground" is a finding nobody
/// made. `via_link` means no cosine was ever computed — the pair came from
/// repeated co-retrieval — so `percent` is not a measured similarity and must
/// not be shown as one. `mergeable` is whether the merge path would take a
/// synthesis at all; a button whose press can only return a validation error
/// is worse than no button.
#[derive(serde::Serialize)]
pub struct PairFacts {
    pub id: i64,
    pub percent: i64,
    pub via_link: bool,
    pub a: PairSide,
    pub b: PairSide,
    /// The judge's line, where one was written.
    pub finding: Option<String>,
    pub contradiction: bool,
    pub vacuous: bool,
    pub unjudged: bool,
    pub unmergeable: bool,
    pub mergeable: bool,
    pub synthesis_asked: bool,
    /// The artifact the judge's proposal amounts to keeping, where it made
    /// one. One field naming a side rather than two flags, which could be set
    /// to both.
    pub keeps: Option<String>,
}

/// One decision, however many pairs it takes to state it.
///
/// Clustered as the web clusters them: one artifact against three others is
/// one question, and it arrived as three cards reading 90%, 90% and 88% alike,
/// where answering one left the other two looking exactly like the one just
/// answered.
#[derive(serde::Serialize)]
pub struct PairCluster {
    pub members: usize,
    pub pairs: Vec<PairFacts>,
}

fn facts_of(p: crate::web::ops::PairRow) -> PairFacts {
    let keeps = match (p.keeps_a, p.keeps_b) {
        (true, _) => Some(p.a_id.clone()),
        (_, true) => Some(p.b_id.clone()),
        _ => None,
    };
    PairFacts {
        id: p.id,
        percent: p.percent,
        via_link: p.via_link,
        a: PairSide {
            id: p.a_id,
            label: p.a_title,
            named: p.a_named,
            excerpt: p.a_excerpt,
        },
        b: PairSide {
            id: p.b_id,
            label: p.b_title,
            named: p.b_named,
            excerpt: p.b_excerpt,
        },
        finding: p.detail,
        contradiction: p.contradiction,
        vacuous: p.vacuous,
        unjudged: p.unjudged,
        unmergeable: p.unmergeable,
        mergeable: p.mergeable,
        synthesis_asked: p.synthesis_asked,
        keeps,
    }
}

/// The open pairs, clustered. Bounded by the same `PAIR_LIMIT` the page is
/// bounded by, so `next` is always null: this is a queue to work through, not
/// a corpus to page.
///
/// `more` is how many are waiting beyond the ones listed, which is the number
/// the page says out loud ("2 more waiting"). A cap that goes unreported
/// reads as the whole queue, and there is no `next` to go and find the rest
/// with — so the count sits beside `items`, as `capped` does on
/// `/insights/set-aside`.
async fn pairs(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    let (rows, more) = crate::web::ops::pair_rows(&tenant).await?;
    let items: Vec<PairCluster> = crate::web::ops::group_pairs(rows)
        .into_iter()
        .map(|c| PairCluster {
            members: c.members,
            pairs: c.pairs.into_iter().map(facts_of).collect(),
        })
        .collect();
    Ok(Json(serde_json::json!({
        "items": items,
        "next": serde_json::Value::Null,
        "more": more,
    })))
}

// ── Gaps ─────────────────────────────────────────────────────────────────────

/// One question the base could not answer.
#[derive(serde::Serialize)]
pub struct GapMember {
    /// Which vocabulary the id belongs to; what dismissing one names.
    pub kind: String,
    pub id: String,
    pub text: String,
}

/// Questions the sweep found to be about one subject, under the name it gave
/// them. A lone question is a cluster of one and reads the same: which of the
/// two it is matters to nobody deciding what to do about it.
#[derive(serde::Serialize)]
pub struct GapClusterFacts {
    pub label: String,
    /// `model` or `terms` — whether a model named this group or its shared
    /// wording did. Said because a name a model gave is a reading, and a name
    /// taken from the words is not.
    pub labelled_by: String,
    pub members: Vec<GapMember>,
}

async fn gaps(tenant: Tenant) -> Result<Json<Page<GapClusterFacts>>> {
    if !tenant.core.learn.enabled {
        // Not an error: gaps are recorded only where searches are, and an
        // installation that records none has nothing here rather than a
        // failure to report.
        return Ok(Json(Page::whole(Vec::new())));
    }
    let (rows, loose) = tenant
        .core
        .store
        .gap_rows(tenant.core.embedder.model(), tenant.core.weak_below())
        .await?;
    let member = |g: crate::store::gaps::Gap| GapMember {
        kind: g.kind.as_str().to_string(),
        id: g.id,
        text: g.text,
    };
    let clustered = rows.into_iter().map(|r| GapClusterFacts {
        label: r.label,
        labelled_by: r.labelled_by,
        members: r.members.into_iter().map(member).collect(),
    });
    let alone = loose.into_iter().map(|g| GapClusterFacts {
        label: g.text.clone(),
        labelled_by: "terms".into(),
        members: vec![member(g)],
    });
    Ok(Json(Page::whole(clustered.chain(alone).collect())))
}

async fn dismiss_gap(
    tenant: Tenant,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode> {
    crate::web::ui::dismiss_gap(&tenant, &kind, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct Members {
    #[serde(default)]
    pub members: Vec<GapRef>,
}

#[derive(serde::Deserialize)]
pub struct GapRef {
    pub kind: String,
    pub id: String,
}

/// Forget a cluster: every question in it at once, which is how the page
/// offers it. A caller sends the members it was shown.
async fn forget_gaps(tenant: Tenant, Json(m): Json<Members>) -> Result<StatusCode> {
    let members: Vec<(String, String)> = m.members.into_iter().map(|g| (g.kind, g.id)).collect();
    crate::web::ui::forget_gaps(&tenant, &members).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── What the base has been doing ─────────────────────────────────────────────

/// One thing the base did on its own, or is waiting to be told about.
///
/// `kind` is the whole of what says which answers this row admits — each of
/// them is a route of its own, and a client knows the six. A `kind` this
/// client has never heard of draws no buttons rather than guessing, which is
/// what lets the server grow a seventh without breaking an older app.
///
/// `subject_id` is what those routes name, and which thing it is depends on
/// the kind: a corpus for `parked`, the artifact for the rest.
#[derive(serde::Serialize)]
pub struct SetAsideFacts {
    pub kind: &'static str,
    pub subject_id: String,
    /// The artifact to open. `None` for a parked capture, which is a corpus.
    pub artifact_id: Option<String>,
    pub label: String,
    pub named: bool,
    /// What tells two rows with one label apart. Empty where nothing does.
    pub subtitle: String,
    /// Why it is here, in a sentence. Never a mechanism the reader has to
    /// already know.
    pub why: String,
    /// What the base put beside it — the sources a merge came from, the
    /// artifact a near-duplicate lost to.
    pub beside: Vec<Beside>,
    /// The one thing about this row that is not simply reversible.
    pub caveat: Option<String>,
}

#[derive(serde::Serialize)]
pub struct Beside {
    pub id: String,
    pub corpus_id: String,
    pub label: String,
    pub named: bool,
}

/// `GET /insights/set-aside` — the undo list, and whether any cap bit.
async fn set_aside(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    let (rows, capped) = crate::web::insights::set_aside_rows(&tenant).await?;
    let items: Vec<SetAsideFacts> = rows
        .into_iter()
        .map(|r| SetAsideFacts {
            kind: r.kind,
            subject_id: r.subject_id,
            artifact_id: r.artifact_id,
            label: r.title,
            named: r.named,
            subtitle: r.subtitle,
            why: r.why,
            beside: r
                .beside
                .into_iter()
                .map(|b| Beside {
                    id: b.id,
                    corpus_id: b.corpus_id,
                    label: b.title,
                    named: b.named,
                })
                .collect(),
            caveat: r.caveat,
        })
        .collect();
    Ok(Json(serde_json::json!({
        "items": items,
        "next": serde_json::Value::Null,
        "capped": capped,
    })))
}

/// What the base is like, read-only.
///
/// Read-only by the programme: Insights says what the tuning sweep
/// recommends, and the press that adopts it stays on the web, because it
/// writes `config.toml` and the person who may do that is at a keyboard.
/// Nothing about tuning is in this answer at all.
async fn insights(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    let held = tenant.core.store.held().await?;
    let used = tenant
        .core
        .store
        .used(tenant.core.activation.half_life_days, crate::store::now())
        .await?;
    // Only where searches are recorded. On an installation that records none
    // the honest answer is that there is nothing to say — not 0.00, which
    // would read as a score.
    let retrieval = match tenant.core.learn.enabled {
        true => {
            let f = tenant
                .core
                .store
                .feedback_stats(tenant.core.weak_below())
                .await?;
            serde_json::json!({
                "judged": f.judged,
                "recall_at_10": (f.judged > 0).then_some(f.recall_at_10),
                "mrr": (f.judged > 0).then_some(f.mrr),
            })
        }
        false => serde_json::Value::Null,
    };
    Ok(Json(serde_json::json!({
        "held": {
            "corpora": held.corpora,
            "artifacts": held.artifacts,
            "segments": held.segments,
            "synthesized": held.synthesized,
        },
        "used": used
            .iter()
            .map(|b| serde_json::json!({ "label": b.label, "count": b.count }))
            .collect::<Vec<_>>(),
        "retrieval": retrieval,
    })))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/pairs", get(pairs))
        .route("/gaps", get(gaps))
        .route("/gaps/forget", post(forget_gaps))
        .route("/gaps/{kind}/{id}/dismiss", post(dismiss_gap))
        .route("/insights", get(insights))
        .route("/insights/set-aside", get(set_aside))
        .route("/pairs/{id}/dismiss", post(dismiss))
        .route("/pairs/{id}/discard", post(discard))
        .route("/pairs/{id}/supersede", post(supersede))
        .route("/pairs/{id}/synthesize", post(synthesize))
        .route("/artifacts/{id}/verify", post(verify))
        .route("/artifacts/{id}/deprecate", post(deprecate))
        .route("/artifacts/{id}/reactivate", post(reactivate))
        .route("/artifacts/{id}/unsupersede", post(unsupersede))
        .route("/merges/{id}/undo", post(undo_merge))
        .route("/condensations/{id}/undo", post(undo_condensation))
}

// ── The five answers to a pair ───────────────────────────────────────────────

async fn dismiss(tenant: Tenant, Path(pid): Path<i64>) -> Result<StatusCode> {
    crate::web::ops::dismiss_pair(&tenant, pid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn discard(tenant: Tenant, Path(pid): Path<i64>) -> Result<StatusCode> {
    crate::web::ops::discard_pair(&tenant, pid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body is optional: `{}` and no body at all both mean "whichever the
/// judge proposed", and a client that has nothing to say should not have to
/// send an empty object to say it.
async fn supersede(
    tenant: Tenant,
    Path(pid): Path<i64>,
    body: Option<Json<Keep>>,
) -> Result<StatusCode> {
    let keep = body.and_then(|Json(k)| k.keep);
    crate::web::ops::supersede_pair(&tenant, pid, keep.as_deref()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn synthesize(tenant: Tenant, Path(pid): Path<i64>) -> Result<StatusCode> {
    crate::web::ops::synthesize_pair(&tenant, pid).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Taking things back ───────────────────────────────────────────────────────

async fn verify(tenant: Tenant, Path(aid): Path<String>) -> Result<StatusCode> {
    tenant.core.verify(&aid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn deprecate(tenant: Tenant, Path(aid): Path<String>) -> Result<StatusCode> {
    tenant.core.deprecate(&aid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reactivate(tenant: Tenant, Path(aid): Path<String>) -> Result<StatusCode> {
    crate::web::ops::reactivate_artifact(&tenant, &aid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unsupersede(tenant: Tenant, Path(aid): Path<String>) -> Result<StatusCode> {
    crate::web::ops::unsupersede_artifact(&tenant, &aid).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn undo_merge(tenant: Tenant, Path(aid): Path<String>) -> Result<StatusCode> {
    crate::web::ops::undo_merge(&tenant, &aid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The path is the condensation's id, not the artifact's. The artifact it was
/// on comes back from the call and is answered, because a client that just put
/// a version back wants to re-read that artifact and the path never named it.
async fn undo_condensation(
    tenant: Tenant,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let artifact_id = crate::web::ops::undo_condensation(&tenant, &id).await?;
    Ok(Json(serde_json::json!({ "artifact_id": artifact_id })))
}

#[cfg(test)]
mod tests {
    use crate::store::artifacts::ArtifactStatus;
    use crate::store::pairs::PairState;
    use crate::web::test_support::{app_session_and_core, artifacts, json_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Two artifacts and one pending pair between them.
    async fn a_pair(core: &crate::core::Core) -> (Vec<String>, i64) {
        let ids = artifacts(core, &["the timeout is 30 seconds", "the timeout is 90"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pid = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0)
            .id;
        (ids, pid)
    }

    fn post(uri: &str, cookie: &str, body: Option<&str>) -> Request<Body> {
        let b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", cookie);
        match body {
            Some(j) => b
                .header("content-type", "application/json")
                .body(Body::from(j.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    /// The queue is capped at `PAIR_LIMIT` and there is no `next` to find the
    /// rest with, so the number still waiting has to cross. Dropped, a phone
    /// worked five pairs and was told nothing about the sixth; the page beside
    /// it says "2 more waiting".
    #[tokio::test]
    async fn the_pair_queue_says_how_many_are_waiting_beyond_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(
            &core,
            &[
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n",
            ],
        )
        .await;
        for w in ids.chunks(2) {
            core.store.record_pair(&w[0], &w[1], 0.9).await.unwrap();
        }

        let body = json_of(app.oneshot(get("/api/v1/pairs", &cookie)).await.unwrap()).await;

        assert_eq!(
            body["items"].as_array().unwrap().len(),
            crate::web::ops::PAIR_LIMIT,
            "seven pairs, five offered: {body}"
        );
        assert_eq!(body["more"], 2, "and the other two said out loud: {body}");
        assert!(body["next"].is_null(), "a queue is not paged");
    }

    /// A pending pair was filed on a cosine score and nothing has read it. The
    /// card must be able to tell that apart from a finding, so the flag
    /// crosses and the prose does not.
    #[tokio::test]
    async fn an_unread_pair_says_so_and_carries_no_finding() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;

        let res = app.oneshot(get("/api/v1/pairs", &cookie)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("etag"), "a read revalidates");
        let body = json_of(res).await;

        assert!(body["next"].is_null(), "a queue is not paged: {body}");
        let cluster = &body["items"][0];
        assert_eq!(cluster["members"], 2);
        let p = &cluster["pairs"][0];
        assert_eq!(p["id"], pid);
        assert_eq!(p["unjudged"], true);
        assert!(p["finding"].is_null(), "a claim nobody made: {p}");
        assert!(p["keeps"].is_null(), "nothing proposed a side");
        assert_eq!(p["percent"], 90);
        assert_eq!(p["via_link"], false);
        assert_eq!(p["a"]["id"], ids[0].as_str());
        assert_eq!(p["a"]["named"], true);
        assert!(p["a"]["excerpt"].as_str().is_some_and(|e| !e.is_empty()));
        // Both sides are captured artifacts, so the merge path would take a
        // synthesis over them and the button is offered.
        assert_eq!(p["mergeable"], true, "{p}");
    }

    /// A passage is its own root, and `insert_merged_artifact` refuses a
    /// lineage naming stored source text. The card must not offer a press that
    /// can only come back a validation error.
    #[tokio::test]
    async fn a_pair_of_passages_is_not_offered_a_synthesis() {
        let (app, cookie, core) = app_session_and_core().await;
        let src = core
            .store
            .insert_corpus("one\ntwo", "web", None)
            .await
            .unwrap();
        let made = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "the timeout is 30 seconds".into(),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "the timeout is 90".into(),
                        ..Default::default()
                    },
                ],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        core.store
            .record_pair(&made[0].id, &made[1].id, 0.9)
            .await
            .unwrap();

        let body = json_of(app.oneshot(get("/api/v1/pairs", &cookie)).await.unwrap()).await;

        let p = &body["items"][0]["pairs"][0];
        assert_eq!(p["mergeable"], false, "Write one would be refused: {p}");
        // And a passage is named by how its text opens, which is not a name.
        assert_eq!(p["a"]["named"], false, "{p}");
    }

    /// One artifact against two others is one question. Three cards reading
    /// 90%, 90% and 88% alike is the failure the clustering exists to prevent.
    #[tokio::test]
    async fn pairs_naming_one_artifact_arrive_as_one_cluster() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["hub", "spoke one", "spoke two"]).await;
        for other in &ids[1..] {
            core.store.record_pair(&ids[0], other, 0.9).await.unwrap();
        }

        let body = json_of(app.oneshot(get("/api/v1/pairs", &cookie)).await.unwrap()).await;

        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "two questions about one artifact: {body}");
        assert_eq!(items[0]["members"], 3);
        assert_eq!(items[0]["pairs"].as_array().unwrap().len(), 2);
    }

    /// The judge's proposal is one field naming a side, not two flags that
    /// could both be set.
    #[tokio::test]
    async fn a_judged_pair_carries_its_finding_and_the_side_it_would_keep() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;
        core.store
            .set_pair_superseded(
                pid,
                &ids[1],
                Some("30 seconds vs 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();

        let body = json_of(app.oneshot(get("/api/v1/pairs", &cookie)).await.unwrap()).await;

        let p = &body["items"][0]["pairs"][0];
        assert_eq!(p["finding"], "30 seconds vs 90");
        assert_eq!(p["unjudged"], false);
        assert_eq!(
            p["keeps"],
            ids[0].as_str(),
            "keeping a is superseding b: {p}"
        );
    }

    // ── Gaps ────────────────────────────────────────────────────────────────

    /// An abstained ask, judged as having no answer here: the plainest gap
    /// there is, and the one the capture page lists.
    async fn a_gap(core: &crate::core::Core, q: &str) -> String {
        let id = core
            .store
            .record_ask(crate::store::asks::NewAsk {
                question: q.into(),
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
            .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_gap_is_listed_and_can_be_covered_by_saying_it_needs_no_answer() {
        // Gaps are only recorded where searches are, so the core is built
        // with that on before the router is wrapped around it.
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let id = a_gap(&handle, "how do ticks work").await;

        let body = json_of(
            app.clone()
                .oneshot(get("/api/v1/gaps", &cookie))
                .await
                .unwrap(),
        )
        .await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["members"][0]["text"], "how do ticks work");
        let kind = items[0]["members"][0]["kind"].as_str().unwrap().to_string();

        let res = app
            .clone()
            .oneshot(post(
                &format!("/api/v1/gaps/{kind}/{id}/dismiss"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let after = json_of(app.oneshot(get("/api/v1/gaps", &cookie)).await.unwrap()).await;
        assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
    }

    /// A kind this base does not know deletes nothing, so it must not report
    /// success.
    #[tokio::test]
    async fn a_gap_kind_this_base_does_not_know_is_refused() {
        let (app, cookie, _core) = app_session_and_core().await;
        let res = app
            .oneshot(post("/api/v1/gaps/invented/x/dismiss", &cookie, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// Gaps are recorded only where searches are. Nothing to say is an empty
    /// list, not a failure.
    #[tokio::test]
    async fn a_base_that_records_nothing_has_no_gaps_rather_than_an_error() {
        let (app, cookie, _core) = app_session_and_core().await;
        let res = app.oneshot(get("/api/v1/gaps", &cookie)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(json_of(res).await["items"].as_array().unwrap().is_empty());
    }

    // ── What the base has been doing ────────────────────────────────────────

    #[tokio::test]
    async fn insights_says_what_is_held_and_nothing_about_tuning() {
        let (app, cookie, core) = app_session_and_core().await;
        artifacts(&core, &["one", "two"]).await;

        let res = app.oneshot(get("/api/v1/insights", &cookie)).await.unwrap();
        assert!(res.headers().contains_key("etag"));
        let body = json_of(res).await;

        assert_eq!(body["held"]["artifacts"], 2);
        assert_eq!(body["held"]["corpora"], 1);
        assert!(body["used"].is_array());
        // Recording is off in the fixture, so there is nothing to say about
        // retrieval — and nothing to say is null, never 0.00, which would read
        // as a score.
        assert!(body["retrieval"].is_null(), "{body}");
        assert!(
            body.get("tune").is_none(),
            "the one route that writes config.toml has no business here: {body}"
        );
    }

    /// The undo list. A superseded artifact is the plainest row on it: the
    /// base hid something, and the row is how it comes back.
    #[tokio::test]
    async fn the_set_aside_list_names_what_a_row_is_about_and_what_answers_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;
        app.clone()
            .oneshot(post(
                &format!("/api/v1/pairs/{pid}/supersede"),
                &cookie,
                Some(&format!(r#"{{"keep":"{}"}}"#, ids[0])),
            ))
            .await
            .unwrap();

        let res = app
            .oneshot(get("/api/v1/insights/set-aside", &cookie))
            .await
            .unwrap();
        assert!(res.headers().contains_key("etag"));
        let body = json_of(res).await;

        let hidden = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["subject_id"] == ids[1].as_str())
            .unwrap_or_else(|| panic!("the artifact that was hidden is not listed: {body}"));
        assert_eq!(hidden["kind"], "hidden");
        assert_eq!(hidden["artifact_id"], ids[1].as_str());
        assert!(hidden["why"].as_str().is_some_and(|w| !w.is_empty()));
        // What it lost to, so the row can say what took its place.
        assert_eq!(hidden["beside"][0]["id"], ids[0].as_str());
        // Facts, not a rendering: no hrefs and no button labels.
        let text = body.to_string();
        assert!(!text.contains("/ui/"), "a link crossed the API: {text}");
        assert!(!text.contains("Restore"), "a button label crossed: {text}");
    }

    #[tokio::test]
    async fn a_pair_is_dismissed_over_the_api_and_leaves_the_queue() {
        let (app, cookie, core) = app_session_and_core().await;
        let (_ids, pid) = a_pair(&core).await;

        let res = app
            .oneshot(post(&format!("/api/v1/pairs/{pid}/dismiss"), &cookie, None))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            core.store.get_pair(pid).await.unwrap().state,
            PairState::Dismissed
        );
    }

    /// The answer that hides one artifact behind another. Asserted on what it
    /// did to the artifacts, not on the pair row: the hiding is the point.
    #[tokio::test]
    async fn keeping_one_side_hides_the_other_behind_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;

        let res = app
            .oneshot(post(
                &format!("/api/v1/pairs/{pid}/supersede"),
                &cookie,
                Some(&format!(r#"{{"keep":"{}"}}"#, ids[0])),
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let kept = core.store.get_artifact(&ids[0]).await.unwrap();
        let hidden = core.store.get_artifact(&ids[1]).await.unwrap();
        assert!(kept.in_results(), "the side that was kept left results");
        assert_eq!(hidden.superseded_by.as_deref(), Some(ids[0].as_str()));
    }

    /// A body is user input wherever it arrives from. Superseding an arbitrary
    /// id because it was posted would hide an artifact that has nothing to do
    /// with the pair that was answered.
    #[tokio::test]
    async fn keeping_something_that_is_not_in_the_pair_is_refused() {
        let (app, cookie, core) = app_session_and_core().await;
        let (_ids, pid) = a_pair(&core).await;
        let stranger = artifacts(&core, &["unrelated"]).await;

        let res = app
            .oneshot(post(
                &format!("/api/v1/pairs/{pid}/supersede"),
                &cookie,
                Some(&format!(r#"{{"keep":"{}"}}"#, stranger[0])),
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            core.store
                .get_artifact(&stranger[0])
                .await
                .unwrap()
                .in_results()
        );
    }

    #[tokio::test]
    async fn both_sides_can_be_discarded() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;

        let res = app
            .oneshot(post(&format!("/api/v1/pairs/{pid}/discard"), &cookie, None))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        for id in &ids {
            assert!(
                !core.store.get_artifact(id).await.unwrap().in_results(),
                "{id} is still in results after discarding both"
            );
        }
    }

    /// The whole point of the undo half: what a decision did can be taken back
    /// from the phone, not only from the desk it was not made at.
    #[tokio::test]
    async fn a_supersede_is_taken_back_again() {
        let (app, cookie, core) = app_session_and_core().await;
        let (ids, pid) = a_pair(&core).await;
        app.clone()
            .oneshot(post(
                &format!("/api/v1/pairs/{pid}/supersede"),
                &cookie,
                Some(&format!(r#"{{"keep":"{}"}}"#, ids[0])),
            ))
            .await
            .unwrap();

        let res = app
            .oneshot(post(
                &format!("/api/v1/artifacts/{}/unsupersede", ids[1]),
                &cookie,
                None,
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(core.store.get_artifact(&ids[1]).await.unwrap().in_results());
    }

    /// `reactivate` is the only answer the app draws for a `hidden` row, and
    /// a supersession is one of the two things that word covers
    /// (`insights::set_aside_rows`). Taking it back has to close the journal
    /// row as well as clear the payload: `dedupe::taken_back_before` reads
    /// that row to hand the pair to a person rather than ruling on it again,
    /// and unstamped, the next consolidation tick re-applied the supersession
    /// the press had just undone. The `/ui/ops` twin is
    /// `ops::tests::restoring_a_superseded_artifact_stamps_the_supersession`.
    #[tokio::test]
    async fn reactivating_a_superseded_artifact_stamps_the_supersession() {
        use crate::store::actions::{Job, Kind, NewAction};
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
        // Hidden behind the other by the sweep, journaled as the sweep journals it.
        core.supersede_with(
            &ids[0],
            &ids[1],
            Some(NewAction {
                job: Job::Dedupe,
                kind: Kind::Supersede,
                subject_id: ids[0].clone(),
                survivor_id: Some(ids[1].clone()),
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            }),
        )
        .await
        .unwrap();

        let res = app
            .oneshot(post(
                &format!("/api/v1/artifacts/{}/reactivate", ids[0]),
                &cookie,
                None,
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(
            core.store.get_artifact(&ids[0]).await.unwrap().in_results(),
            "the restore itself did not happen"
        );
        assert!(
            core.store
                .open_action_on(&ids[0], Kind::Supersede)
                .await
                .unwrap()
                .is_none(),
            "the supersession row is still open, so the sweep will re-apply it"
        );
        assert!(
            core.store
                .action_was_undone(&ids[0], Kind::Supersede)
                .await
                .unwrap(),
            "and nothing records that a person took it back"
        );
    }

    /// Hidden twice, returned once — the state a row filed before the two
    /// guards existed can still be in.
    ///
    /// `reactivate` routes a superseded artifact through `unsupersede`, which
    /// returns before the status write, so this reads like a press that clears
    /// one hiding and leaves the other. It is not: `set_superseded_by` writes
    /// `status` in the same UPDATE as `superseded_by`, so clearing the winner
    /// *is* the status write and the row comes back active whatever it read
    /// before. Asserted because nothing else says so, and the next reader of
    /// that early return will draw the same wrong conclusion.
    #[tokio::test]
    async fn reactivating_an_artifact_hidden_both_ways_takes_one_press() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
        // Straight to the store, because both doors refuse to make this state
        // now: `supersede` refuses a deprecated loser and `deprecate` refuses a
        // superseded one. Rows filed before those guards existed are still in
        // live bases, and this is the shape they have.
        core.deprecate(&ids[0]).await.unwrap();
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        let res = app
            .oneshot(post(
                &format!("/api/v1/artifacts/{}/reactivate", ids[0]),
                &cookie,
                None,
            ))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let back = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(back.superseded_by.is_none(), "still hidden behind a winner");
        assert_eq!(
            back.status,
            ArtifactStatus::Active,
            "the deprecation outlived a press that said it returned this to results"
        );
        assert!(back.in_results());
    }

    #[tokio::test]
    async fn deprecating_and_reactivating_an_artifact_round_trips() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["a note"]).await;

        for (path, expect_in_results) in [("deprecate", false), ("reactivate", true)] {
            let res = app
                .clone()
                .oneshot(post(
                    &format!("/api/v1/artifacts/{}/{path}", ids[0]),
                    &cookie,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NO_CONTENT, "{path}");
            assert_eq!(
                core.store.get_artifact(&ids[0]).await.unwrap().in_results(),
                expect_in_results,
                "after {path}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_pair_and_an_unknown_artifact_are_404s_in_the_one_error_shape() {
        let (app, cookie, _core) = app_session_and_core().await;

        for uri in [
            "/api/v1/pairs/999999/dismiss",
            "/api/v1/artifacts/no-such-thing/unsupersede",
            "/api/v1/condensations/no-such-action/undo",
        ] {
            let res = app.clone().oneshot(post(uri, &cookie, None)).await.unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(json_of(res).await["error"], "not found", "{uri}");
        }
    }
}
