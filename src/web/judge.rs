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
use crate::web::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};

/// Which side of a pair survives. Absent means the one the judge proposed,
/// which is what a confirmation on a proposed supersede sends.
#[derive(serde::Deserialize, Default)]
pub struct Keep {
    #[serde(default)]
    pub keep: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
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
