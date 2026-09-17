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
async fn pairs(tenant: Tenant) -> Result<Json<Page<PairCluster>>> {
    let (rows, _more) = crate::web::ops::pair_rows(&tenant).await?;
    Ok(Json(Page::whole(
        crate::web::ops::group_pairs(rows)
            .into_iter()
            .map(|c| PairCluster {
                members: c.members,
                pairs: c.pairs.into_iter().map(facts_of).collect(),
            })
            .collect(),
    )))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/pairs", get(pairs))
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
    tenant.core.reactivate(&aid).await?;
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
