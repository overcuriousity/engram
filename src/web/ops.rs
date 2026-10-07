//! The operator's hand on the corpus: taking back what the base did.
//!
//! The last screen out of `web::ui`. Everything under `/ui/ops`: the buttons
//! that act on one artifact — take a supersession back, deprecate and
//! reactivate, undo a merge, undo a condensation. There is no review queue any
//! more: the base settles every pair itself, and what a person can do is undo
//! what it settled or hide something by hand.
//!
//! Every one of these ends by redrawing the artifact pane, which is why this
//! module is a caller of `web::artifact` rather than a peer: `artifact_changed`
//! is the one place that decides between the fragment and a redirect, and
//! `ReturnTo` is what a form carries so a press knows which page it was made
//! from.

use crate::tenants::Tenant;
use crate::web::artifact::{ArtifactDetailFragment, ArtifactViewParams, build_artifact_detail};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// Where a lifecycle button should land afterwards.
///
/// The same actions are offered from two places: the journal on Insights,
/// where the list is the thing being worked through, and an artifact's own
/// page, where being thrown onto Insights for pressing "Hide from results"
/// loses the reader's place. The page that rendered the button says where it
/// leads; Ops sends nothing and keeps the default.
#[derive(serde::Deserialize, Default)]
struct ReturnTo {
    to: Option<String>,
}

impl ReturnTo {
    /// Only a path inside this UI. A form field is user input, and a redirect
    /// that will follow anything it is handed is an open redirect — worth
    /// nothing to the operator and a phishing hop to everyone else.
    fn path(&self) -> &str {
        match self.to.as_deref() {
            Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => p,
            _ => "/ui/insights",
        }
    }
}

/// What a lifecycle button answers with: the artifact it just changed.
///
/// These buttons say something about an artifact, not about the page it is
/// on — so the answer is that artifact, re-rendered where it already was, and
/// nothing navigates. `_artifact_detail.html` is rendered in two places and the
/// hidden `to` beside each button can only name one of them: it named the
/// standalone artifact page, so pressing a button on a search result took the
/// whole window there and the results the operator was working
/// through were gone. That is the reason `ReturnTo` exists, arrived at from the
/// other side.
///
/// The redirect is still what a browser without htmx gets, and `to` is still
/// what it follows. Nothing here is the only way any of these buttons work.
async fn artifact_changed(
    tenant: &Tenant,
    headers: &axum::http::HeaderMap,
    aid: &str,
    terms: &str,
    back: &ReturnTo,
) -> UiResult<Response> {
    if headers.contains_key("hx-request") {
        // As above: an action on the artifact redraws the pane at its opening
        // length rather than reconstructing a run nobody passed along.
        let d = build_artifact_detail(&tenant.core, aid, terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(Redirect::to(back.path()).into_response())
}

/// Put a superseded artifact back in results, and mark the action undone.
///
/// The two halves are one act: `unsupersede` alone leaves the journal claiming
/// a supersession that no longer holds, which is what the sweep reads.
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

/// Put a hidden or buried artifact back in results, and mark undone whichever
/// of the three actions hid it.
///
/// The two halves are one act, as in `unsupersede_artifact`: whichever of the
/// three hid it stamps, and the others stamp nothing, since `undo_action_on`
/// matches only open rows of that kind on this subject.
///
/// `Supersede` belongs here even though the button that undoes a plain
/// supersession is `unsupersede_ui`'s. A *buried* artifact can also be
/// superseded — `graveyard_list` filters on neither — and `Core::reactivate`
/// falls straight through to `unsupersede_locked` for it. Unstamped, the
/// supersession's journal row stayed open: it is what
/// `dedupe::taken_back_before` reads to hand a pair to a person instead of
/// ruling on it again, so the sweep simply re-applied the supersession the
/// operator had just undone.
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
    // A press on a page that has gone stale: between the render and the press,
    // a later merge subsumed this one and took its supersession. Saying so is
    // the whole of what is available from here — the alternative is a redirect
    // to a queue the row has left, which reads as "done".
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

/// Take a merge back: what it replaced returns, the merge is retired, and the
/// pairs behind it are dismissed so the sweep does not simply redo it.
/// Put the version a condensation retired back. One button per open
/// condensation on the artifact's page; the base's own undo is the same
/// method with `UndoneBy::Evidence`.
/// The path is the *condensation's* id and `artifact_changed` wants the
/// artifact's, so the action is read for its `subject_id` before it is undone
/// — after which the row still names the same artifact, but reading it first
/// keeps the failure ordinary: no such action is a `404` with nothing written.
/// Answers with the artifact the condensation was on, which is what a caller
/// redrawing a pane needs and what the path does not carry.
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
    /// Every button in the artifact pane that posts to `/ui/ops` must carry an
    /// `hx-post` and a `to`, or pressing it from a search result navigates the
    /// whole window away and the results being worked through are gone — the
    /// failure `ReturnTo` exists to prevent. "Restore the last version" was a
    /// bare `<form method="post">` for exactly as long as nothing checked, and
    /// `uncondense_ui` redirected to /ui/insights whatever page it was pressed
    /// from. Asserted over the template rather than on that one button, so the
    /// next button added is held to it too.
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
        // Put it back, reactivate, hide, and restore a condensed version.
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
        // Through the press: a duplicate verdict is a proposal now and writes
        // nothing on its own, so the writer is what this test needs.
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

    /// Restore has to stamp whichever row was hiding the artifact, and a
    /// supersession is one of them.
    ///
    /// A *buried* artifact can also be superseded — `graveyard_list` filters
    /// on neither — so such a row appears under Restore, and
    /// `Core::reactivate` falls straight through to `unsupersede_locked` for
    /// it. (A deprecated one no longer reaches here: `artifacts_by_status`
    /// leaves the superseded to `superseded_artifacts`, which is the row that
    /// can name the winner.) Unstamped, the supersession's journal row stayed
    /// open, which is what `dedupe::taken_back_before` reads to hand a pair to
    /// a person rather than ruling on it again: the sweep simply re-applied
    /// the supersession the operator had just undone.
    #[tokio::test]
    async fn restoring_a_superseded_artifact_stamps_the_supersession() {
        use crate::store::actions::{Job, Kind, NewAction};
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
        // Hidden behind the other, journaled the way a corpus job journals it.
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
