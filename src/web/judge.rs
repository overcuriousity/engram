//! What the base did on its own, and the undo for each, as JSON.
//!
//! The insights read, the journal of what the base merged, wrote, hid and
//! buried, and the routes that take one of those back. Nothing here asks a
//! person to decide anything: the pair review, the gap list and the
//! still-accurate button that used to live here went when the base began
//! settling all three itself. Every undo shares the core call its `/ui/ops`
//! sibling uses, so the two doors cannot answer one press differently.

use crate::error::Result;
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

// ── What the base has been doing ─────────────────────────────────────────────

/// One thing the base did on its own.
///
/// `kind` is the whole of what says which undo this row admits — each of
/// them is a route of its own, and a client knows the four. A `kind` this
/// client has never heard of draws no buttons rather than guessing, which is
/// what lets the server grow another without breaking an older app.
///
/// `subject_id` is what those routes name: the artifact, for every kind.
#[derive(serde::Serialize)]
pub struct SetAsideFacts {
    pub kind: &'static str,
    pub subject_id: String,
    /// The artifact to open.
    pub artifact_id: String,
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

/// `GET /insights/set-aside` — the journal of what the base did on its own,
/// and whether any cap bit.
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
/// Read-only by the programme: the base tunes itself, and Insights says what
/// it did. There is nothing on this page to press.
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
        .route("/insights", get(insights))
        .route("/insights/set-aside", get(set_aside))
        .route("/artifacts/{id}/deprecate", post(deprecate))
        .route("/artifacts/{id}/reactivate", post(reactivate))
        .route("/artifacts/{id}/unsupersede", post(unsupersede))
        .route("/merges/{id}/undo", post(undo_merge))
        .route("/condensations/{id}/undo", post(undo_condensation))
}

// ── Taking things back ───────────────────────────────────────────────────────

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

    /// The journal. A superseded artifact is the plainest row on it: the
    /// base hid something, and the row is how it comes back.
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
        // What it lost to, so the row can say what took its place.
        assert_eq!(hidden["beside"][0]["id"], ids[0].as_str());
        // Facts, not a rendering: no hrefs and no button labels.
        let text = body.to_string();
        assert!(!text.contains("/ui/"), "a link crossed the API: {text}");
        assert!(!text.contains("Restore"), "a button label crossed: {text}");
    }

    /// The whole point of the undo half: what a decision did can be taken back
    /// from the phone, not only from the desk it was not made at.
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
