//! What the base did on its own, and the undo for each, as JSON.
//! Every undo shares the core call of its `/ui/ops` sibling.

use crate::error::Result;
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

/// `kind` names which undo route applies; an unknown `kind` draws no buttons.
/// `subject_id` is what those routes name: the artifact, for every kind.
#[derive(serde::Serialize)]
pub struct SetAsideFacts {
    pub kind: &'static str,
    pub subject_id: String,
    pub artifact_id: String,
    pub label: String,
    pub named: bool,
    /// Empty where nothing tells two rows apart.
    pub subtitle: String,
    /// Never a mechanism the reader has to already know.
    pub why: String,
    /// The sources of a merge, or the artifact a near-duplicate lost to.
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

/// Read-only: the base tunes itself and Insights reports it.
async fn insights(tenant: Tenant) -> Result<Json<serde_json::Value>> {
    let held = tenant.core.store.held().await?;
    let used = tenant
        .core
        .store
        .used(tenant.core.activation.half_life_days, crate::store::now())
        .await?;
    // Only where searches are recorded; otherwise null, since 0.00 reads as a score.
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
        .route("/insights", get(insights))
        .route("/insights/set-aside", get(set_aside))
        .route("/artifacts/{id}/deprecate", post(deprecate))
        .route("/artifacts/{id}/reactivate", post(reactivate))
        .route("/artifacts/{id}/unsupersede", post(unsupersede))
        .route("/merges/{id}/undo", post(undo_merge))
        .route("/condensations/{id}/undo", post(undo_condensation))
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

/// The path is the condensation's id; the artifact is answered so the client can re-read it.
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
    use crate::web::test_support::{app_session_and_core, artifacts, json_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

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
        // Nothing to say is null, never 0.00, which would read as a score.
        assert!(body["retrieval"].is_null(), "{body}");
        assert!(
            body.get("tune").is_none(),
            "the one route that writes config.toml has no business here: {body}"
        );
    }

    #[tokio::test]
    async fn the_set_aside_list_names_what_a_row_is_about_and_what_answers_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the timeout is 30 seconds", "the timeout is 90"]).await;
        core.supersede(&ids[1], &ids[0]).await.unwrap();

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
        assert_eq!(hidden["beside"][0]["id"], ids[0].as_str());
        let text = body.to_string();
        assert!(!text.contains("/ui/"), "a link crossed the API: {text}");
        assert!(!text.contains("Restore"), "a button label crossed: {text}");
    }

    #[tokio::test]
    async fn a_supersede_is_taken_back_again() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the timeout is 30 seconds", "the timeout is 90"]).await;
        core.supersede(&ids[1], &ids[0]).await.unwrap();

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
    async fn reactivating_a_superseded_artifact_stamps_the_supersession() {
        use crate::store::actions::{Job, Kind, NewAction};
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
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

    #[tokio::test]
    async fn reactivating_an_artifact_hidden_both_ways_takes_one_press() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
        // Straight to the store: both doors now refuse this state, but older bases hold it.
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
    async fn an_unknown_artifact_and_an_unknown_action_are_404s_in_the_one_error_shape() {
        let (app, cookie, _core) = app_session_and_core().await;

        for uri in [
            "/api/v1/artifacts/no-such-thing/unsupersede",
            "/api/v1/condensations/no-such-action/undo",
        ] {
            let res = app.clone().oneshot(post(uri, &cookie, None)).await.unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(json_of(res).await["error"], "not found", "{uri}");
        }
    }
}
