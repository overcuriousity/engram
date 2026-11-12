//! Everything under `/ui/ops`: the buttons that take back what the base did to one artifact.
//! Each ends by redrawing the artifact pane through `artifact_changed`; `ReturnTo` says which page a press came from.

use crate::tenants::Tenant;
use crate::web::artifact::{ArtifactDetailFragment, ArtifactViewParams, build_artifact_detail};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// The same actions are on Insights and on an artifact's page; the page says where to land, Ops sends nothing.
#[derive(serde::Deserialize, Default)]
struct ReturnTo {
    to: Option<String>,
}

impl ReturnTo {
    /// Only a path inside this UI: a form field is user input, and following anything is an open redirect.
    fn path(&self) -> &str {
        match self.to.as_deref() {
            Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => p,
            _ => "/ui/insights",
        }
    }
}

/// Answers with the artifact re-rendered where it was, so nothing navigates.
/// Without htmx the redirect to `to` still applies.
async fn artifact_changed(
    tenant: &Tenant,
    headers: &axum::http::HeaderMap,
    aid: &str,
    terms: &str,
    back: &ReturnTo,
) -> UiResult<Response> {
    if headers.contains_key("hx-request") {
        // Redraws the pane at its opening length.
        let d = build_artifact_detail(&tenant.core, aid, terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(Redirect::to(back.path()).into_response())
}

/// One act: `unsupersede` alone leaves the journal claiming a supersession the sweep still reads.
pub(crate) async fn unsupersede_artifact(tenant: &Tenant, aid: &str) -> crate::error::Result<()> {
    tenant.core.unsupersede(aid).await?;
    tenant
        .core
        .store
        .undo_action_on(
            aid,
            crate::store::actions::Kind::Supersede,
            crate::store::actions::UndoneBy::Operator,
            "unsuperseded on Insights",
        )
        .await?;
    Ok(())
}

async fn unsupersede_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    unsupersede_artifact(&tenant, &aid).await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

async fn deprecate_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant.core.deprecate(&aid).await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

/// Stamps whichever of the three actions hid it; `undo_action_on` matches only open rows of that kind.
/// `Supersede` is included: a buried artifact can also be superseded, and an open row would let the sweep redo it.
pub(crate) async fn reactivate_artifact(tenant: &Tenant, aid: &str) -> crate::error::Result<()> {
    tenant.core.reactivate(aid).await?;
    for kind in [
        crate::store::actions::Kind::Discard,
        crate::store::actions::Kind::Reap,
        crate::store::actions::Kind::Supersede,
    ] {
        tenant
            .core
            .store
            .undo_action_on(
                aid,
                kind,
                crate::store::actions::UndoneBy::Operator,
                "reactivated on Insights",
            )
            .await?;
    }
    Ok(())
}

async fn reactivate_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    reactivate_artifact(&tenant, &aid).await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

pub(crate) async fn undo_merge(tenant: &Tenant, aid: &str) -> crate::error::Result<()> {
    use crate::store::actions::UndoneBy;
    // A later merge took this one's supersession since the page rendered; say so.
    if let crate::jobs::merge::Undone::HiddenBehind { later } =
        crate::jobs::merge::undo(&tenant.core, aid, crate::store::pairs::DecidedBy::Operator)
            .await?
    {
        return Err(crate::error::Error::Validation(format!(
            "this merge was itself merged into {later} since this page was drawn — \
             undo that one first, and this comes back with it"
        )));
    }
    tenant
        .core
        .store
        .undo_actions_under(aid, UndoneBy::Operator, "undone on Insights")
        .await?;
    Ok(())
}

async fn undo_merge_ui(tenant: Tenant, Path(aid): Path<String>) -> UiResult<Response> {
    undo_merge(&tenant, &aid).await?;
    Ok(Redirect::to("/ui/insights").into_response())
}

/// The path is the condensation's id, so the action is read for its `subject_id` first; no such action is a 404.
/// Answers with the artifact, which a caller redrawing a pane needs.
pub(crate) async fn undo_condensation(
    tenant: &Tenant,
    action_id: &str,
) -> crate::error::Result<String> {
    let artifact_id = tenant
        .core
        .store
        .action(action_id)
        .await?
        .ok_or(crate::error::Error::NotFound)?
        .subject_id;
    tenant
        .core
        .uncondense(action_id, crate::store::actions::UndoneBy::Operator)
        .await?;
    Ok(artifact_id)
}

async fn uncondense_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    let artifact_id = undo_condensation(&tenant, &aid).await?;
    artifact_changed(&tenant, &headers, &artifact_id, &p.terms, &back).await
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/ops/artifacts/{id}/unsupersede", post(unsupersede_ui))
        .route("/ui/ops/artifacts/{id}/deprecate", post(deprecate_ui))
        .route("/ui/ops/artifacts/{id}/reactivate", post(reactivate_ui))
        .route("/ui/ops/merges/{id}/undo", post(undo_merge_ui))
        .route("/ui/ops/condensations/{id}/undo", post(uncondense_ui))
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_ops_button_in_the_artifact_pane_returns_to_where_it_was_pressed() {
        let tpl = include_str!("templates/_artifact_detail.html");
        let mut checked = 0;
        for form in tpl.split("<form ").skip(1) {
            let head = &form[..form.find('>').expect("an opening form tag")];
            if !head.contains("/ui/ops/") {
                continue;
            }
            let body = &form[..form.find("</form>").expect("a closed form")];
            assert!(head.contains("hx-post="), "no hx-post: {head}");
            assert!(
                body.contains(r#"name="to""#),
                "nothing to return to: {head}"
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "the pane's ops buttons went missing: {checked}"
        );
    }

    use crate::web::test_support::{
        app_session_and_core, app_with_cookie, artifacts, body_of, form, row_on,
        session_with_an_artifact,
    };
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn reactivating_a_discarded_artifact_stamps_its_row_as_undone_by_the_operator() {
        use crate::store::actions::{Kind, UndoneBy};
        let (app, cookie, core, aid) = session_with_an_artifact().await;
        core.deprecate_with(&aid, Some(row_on(&aid, Kind::Discard)))
            .await
            .unwrap();
        assert!(
            core.store
                .open_action_on(&aid, Kind::Discard)
                .await
                .unwrap()
                .is_some()
        );

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{aid}/reactivate"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            res.status().is_redirection() || res.status().is_success(),
            "{}",
            res.status()
        );

        assert!(core.store.get_artifact(&aid).await.unwrap().in_results());
        assert!(
            core.store
                .open_action_on(&aid, Kind::Discard)
                .await
                .unwrap()
                .is_none()
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.undone_by, Some(UndoneBy::Operator));
    }

    #[tokio::test]
    async fn pressing_undo_on_a_merge_stamps_its_rows_as_taken_back_by_the_operator() {
        use crate::store::actions::{Kind, UndoneBy};
        let mut core = crate::core::test_support::test_core().await;
        // A duplicate verdict writes nothing on its own, so the writer is needed.
        core.pair_synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"merged":{"title":"Mounting","text":"Mount the filesystem, or attach the volume, before writing.",
                          "category":"procedure","caveats":[]}}"#
                    .into(),
            ]),
        ));
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store.ask_pair_synthesis(pair).await.unwrap();
        crate::jobs::dedupe::run(&core, &pair.to_string())
            .await
            .unwrap();
        let merged = core.store.merged_artifacts(10).await.unwrap()[0].id.clone();
        crate::jobs::embed::run(&core, &merged).await.unwrap();
        assert_eq!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .len(),
            2
        );
        let (app, cookie) = app_with_cookie(core.clone()).await;

        app.oneshot(form(&format!("/ui/ops/merges/{merged}/undo"), &cookie, ""))
            .await
            .unwrap();

        assert!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
        let rows = core.store.recent_actions(2).await.unwrap();
        assert!(rows.iter().all(|r| r.undone_by == Some(UndoneBy::Operator)));
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn unsuperseding_stamps_the_supersede_row() {
        use crate::store::actions::{Kind, UndoneBy};
        let (app, cookie, core, aid) = session_with_an_artifact().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(
                "The pool holds sixteen connections, and the timeout is 30s.",
                "ui",
            ))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let winner = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        core.supersede_with(
            &aid,
            &winner,
            Some(crate::store::actions::NewAction {
                survivor_id: Some(winner.clone()),
                ..row_on(&aid, Kind::Supersede)
            }),
        )
        .await
        .unwrap();

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{aid}/unsupersede"),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        assert!(
            core.store
                .get_artifact(&aid)
                .await
                .unwrap()
                .superseded_by
                .is_none()
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.kind, Kind::Supersede);
        assert_eq!(row.undone_by, Some(UndoneBy::Operator));
    }

    #[tokio::test]
    async fn ops_lists_a_superseded_artifact_and_can_undo_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the loser", "the keeper"]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        let res = app
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
        let html = body_of(res).await;
        assert!(
            html.contains("the loser") && html.contains("the keeper"),
            "the superseded artifact is not listed"
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{}/unsupersede", ids[0]),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "undo did not clear the flag"
        );
    }

    #[tokio::test]
    async fn restoring_a_superseded_artifact_stamps_the_supersession() {
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
        assert!(
            core.store
                .open_action_on(&ids[0], Kind::Supersede)
                .await
                .unwrap()
                .is_some()
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{}/reactivate", ids[0]),
                &cookie,
                "",
            ))
            .await
            .unwrap();

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
}
