//! The Unsure block on Insights: decisions the base is torn on, one button per answer.
//! Its JSON siblings are in `judge.rs` and share these calls.

use crate::core::Core;
use crate::core::judging;
use crate::error::Result;
use crate::store::decision_log::Logged;
use crate::tenants::Tenant;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use axum::Router;
use axum::extract::{Form, Path};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// The answered list is a way back, not a history.
const ANSWERED_CAP: i64 = 10;

pub struct UnsureRow {
    pub id: String,
    pub label: &'static str,
    /// (href, title)
    pub subjects: Vec<(String, String)>,
    /// (answer, whole number out of 100), largest first.
    pub weights: Vec<(String, u8)>,
    /// (pick, label); Leave is added by the template.
    pub buttons: Vec<(String, String)>,
}

pub struct Answered {
    pub id: String,
    pub label: &'static str,
    pub subjects: Vec<(String, String)>,
    pub said: String,
}

pub struct Unsure {
    pub rows: Vec<UnsureRow>,
    pub waiting_rows: i64,
    pub answered: Vec<Answered>,
    /// (question label, acts waiting on the daily cap).
    pub waiting_acts: Vec<(String, i64)>,
}

impl Unsure {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
            && self.waiting_rows == 0
            && self.answered.is_empty()
            && self.waiting_acts.is_empty()
    }
}

/// An open decision as both doors show it; subjects are (id, title).
pub(crate) struct Open {
    pub id: String,
    pub label: &'static str,
    pub subjects: Vec<(String, String)>,
    pub weights: Vec<(String, u8)>,
    pub buttons: Vec<(String, String)>,
}

/// The stored answer's probabilities as whole numbers, largest first.
fn weights(d: &Logged) -> Vec<(String, u8)> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&d.answer).unwrap_or_default();
    let mut w: Vec<(String, u8)> = map
        .into_iter()
        .map(|(k, v)| {
            let n = (v.as_f64().unwrap_or(0.0) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8;
            (k, n)
        })
        .collect();
    w.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    w
}

fn href(subjects: Vec<(String, String)>) -> Vec<(String, String)> {
    subjects
        .into_iter()
        .map(|(id, title)| (format!("/ui/artifacts/{id}"), title))
        .collect()
}

/// A decision whose question has no applier here has no label or buttons, so it is skipped.
pub(crate) async fn open_rows(core: &Core) -> Result<Vec<Open>> {
    let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
    let mut out = Vec::new();
    for d in core.store.open_asks(max).await? {
        let Some(a) = core.appliers.get(&d.question_id) else {
            continue;
        };
        out.push(Open {
            label: a.label(),
            subjects: a.subjects(core, &d).await?,
            weights: weights(&d),
            buttons: a.buttons(core, &d).await?,
            id: d.id,
        });
    }
    Ok(out)
}

pub(crate) async fn unsure(tenant: &Tenant) -> Result<Unsure> {
    let core = &tenant.core;
    let rows = open_rows(core)
        .await?
        .into_iter()
        .map(|o| UnsureRow {
            id: o.id,
            label: o.label,
            subjects: href(o.subjects),
            weights: o.weights,
            buttons: o.buttons,
        })
        .collect();
    let mut answered = Vec::new();
    for d in core.store.answered_without_action(ANSWERED_CAP).await? {
        let Some(a) = core.appliers.get(&d.question_id) else {
            continue;
        };
        let pick = d.answered_pick.clone().unwrap_or_default();
        let said = a
            .buttons(core, &d)
            .await?
            .into_iter()
            .find(|(p, _)| *p == pick)
            .map_or(pick, |(_, l)| l);
        answered.push(Answered {
            label: a.label(),
            subjects: href(a.subjects(core, &d).await?),
            said,
            id: d.id,
        });
    }
    let waiting_acts = core
        .store
        .waiting_counts()
        .await?
        .into_iter()
        .filter_map(|(q, n)| Some((core.appliers.get(&q)?.label().to_string(), n)))
        .collect();
    Ok(Unsure {
        rows,
        waiting_rows: core.store.waiting_ask_count().await?,
        answered,
        waiting_acts,
    })
}

/// One of the row's answers, or `leave`.
#[derive(serde::Deserialize)]
struct Pick {
    pick: String,
}

async fn answer_ui(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(p): Form<Pick>,
) -> UiResult<Response> {
    let pick = Some(p.pick.as_str()).filter(|p| *p != "leave");
    judging::answer(&tenant.core, &id, pick).await?;
    Ok(Redirect::to("/ui/insights").into_response())
}

async fn again_ui(tenant: Tenant, Path(id): Path<String>) -> UiResult<Response> {
    judging::ask_again(&tenant.core, &id).await?;
    Ok(Redirect::to("/ui/insights").into_response())
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/ops/unsure/{id}/answer", post(answer_ui))
        .route("/ui/ops/unsure/{id}/again", post(again_ui))
}

#[cfg(test)]
mod tests {
    use crate::core::judging::band::Band;
    use crate::core::judging::test_support::{CHOICE, TestApplier, by_state, core_with, seeded};
    use crate::store::decision_log::State;
    use crate::store::pairs::DecidedBy;
    use crate::web::test_support::{app_with_cookie, body_of, form, get_body, json_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn torn() -> serde_json::Value {
        serde_json::json!({"distinct": 0.41, "duplicate": 0.48, "replaced": 0.11})
    }

    async fn app() -> (axum::Router, String, crate::core::Core, Arc<TestApplier>) {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        (app, cookie, core, ap)
    }

    async fn open_one() -> (
        axum::Router,
        String,
        crate::core::Core,
        Arc<TestApplier>,
        String,
    ) {
        let (app, cookie, core, ap) = app().await;
        let d = seeded(&core, CHOICE, "a", torn(), Band::Ask).await;
        assert_eq!(d.state, State::Open);
        (app, cookie, core, ap, d.id)
    }

    async fn insights(app: &axum::Router, cookie: &str) -> String {
        get_body(app, cookie, "/ui/insights").await
    }

    async fn press(
        app: &axum::Router,
        cookie: &str,
        uri: &str,
        body: &str,
    ) -> axum::response::Response {
        app.clone().oneshot(form(uri, cookie, body)).await.unwrap()
    }

    fn json_post(uri: &str, cookie: Option<&str>, body: &str) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn json_get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        b.body(Body::empty()).unwrap()
    }

    fn redirected_home(res: &axum::response::Response) {
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/insights");
    }

    #[tokio::test]
    async fn insights_shows_nothing_unsure_when_nothing_is() {
        let (app, cookie, _, _) = app().await;
        let page = insights(&app, &cookie).await;
        assert!(!page.contains("Unsure"), "{page}");
    }

    #[tokio::test]
    async fn a_torn_decision_is_a_row_with_its_weights_and_a_button_per_answer() {
        let (app, cookie, _, _, id) = open_one().await;
        let page = insights(&app, &cookie).await;
        assert!(page.contains("Unsure · 1"), "{page}");
        assert!(page.contains("Same thing?"), "{page}");
        assert!(
            page.contains(r#"href="/ui/artifacts/a">Title a</a>"#),
            "{page}"
        );
        assert!(
            page.contains(r#"href="/ui/artifacts/a-other">Title a-other</a>"#),
            "{page}"
        );
        assert!(page.contains("↔"), "{page}");
        assert!(page.contains("duplicate 48 · distinct 41"), "{page}");
        let action = format!(r#"action="/ui/ops/unsure/{id}/answer""#);
        assert_eq!(
            page.matches(&action).count(),
            3,
            "two answers and Leave: {page}"
        );
        for b in [">Same<", ">Different<", ">Leave<"] {
            assert!(page.contains(b), "no {b}: {page}");
        }
        let block = &page[page.find("Unsure · 1").unwrap()..];
        let block = &block[..block.find("</ul>").unwrap()];
        assert!(!block.contains('%'), "{block}");
    }

    #[tokio::test]
    async fn pressing_an_answer_does_it_and_the_row_is_gone() {
        let (app, cookie, _, ap, id) = open_one().await;
        let res = press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=same",
        )
        .await;
        redirected_home(&res);
        assert_eq!(
            ap.applied(),
            vec![("same".to_string(), DecidedBy::Operator)]
        );
        let page = insights(&app, &cookie).await;
        assert!(!page.contains("Unsure"), "{page}");
    }

    #[tokio::test]
    async fn pressing_leave_closes_the_row_and_does_nothing() {
        let (app, cookie, core, ap, id) = open_one().await;
        let res = press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=leave",
        )
        .await;
        redirected_home(&res);
        assert!(ap.applied().is_empty());
        let d = core.store.logged(&id).await.unwrap().unwrap();
        assert_eq!(d.state, State::Done);
        let page = insights(&app, &cookie).await;
        assert!(!page.contains("Unsure"), "{page}");
    }

    #[tokio::test]
    async fn an_answer_that_journals_nothing_can_be_asked_again() {
        let (app, cookie, _, ap, id) = open_one().await;
        ap.set_journals(false);
        press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=different",
        )
        .await;
        let page = insights(&app, &cookie).await;
        assert!(page.contains("answered (1)"), "{page}");
        assert!(
            page.contains(&format!(r#"action="/ui/ops/unsure/{id}/again""#)),
            "{page}"
        );
        assert!(page.contains(">Ask again<"), "{page}");
        assert!(page.contains("Different"), "what was said: {page}");

        let res = press(&app, &cookie, &format!("/ui/ops/unsure/{id}/again"), "").await;
        redirected_home(&res);
        assert_eq!(ap.reopened(), vec![id]);
        let page = insights(&app, &cookie).await;
        assert!(!page.contains("answered ("), "{page}");
    }

    #[tokio::test]
    async fn a_row_already_answered_says_so_in_one_sentence() {
        let (app, cookie, _, ap, id) = open_one().await;
        let uri = format!("/ui/ops/unsure/{id}/answer");
        press(&app, &cookie, &uri, "pick=same").await;
        let res = press(&app, &cookie, &uri, "pick=different").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_of(res).await;
        assert!(body.contains("this was already answered"), "{body}");
        assert_eq!(ap.applied().len(), 1, "the second press did something");
    }

    #[tokio::test]
    async fn the_block_counts_what_waits_for_a_seat_and_for_the_daily_pace() {
        let (mut core, _, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.decide_cfg.ask_open_max = 1;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        seeded(&core, CHOICE, "a", torn(), Band::Ask).await;
        let second = seeded(&core, CHOICE, "b", torn(), Band::Ask).await;
        assert_eq!(second.state, State::Waiting, "no seat for it");
        for s in ["c", "d"] {
            seeded(
                &core,
                CHOICE,
                s,
                serde_json::json!({"same": 0.95, "different": 0.05}),
                Band::Act,
            )
            .await;
        }
        let page = insights(&app, &cookie).await;
        assert!(page.contains("Unsure · 1"), "{page}");
        assert!(page.contains("1 more waiting"), "{page}");
        assert!(page.contains("2 waiting on the daily pace"), "{page}");
    }

    #[tokio::test]
    async fn the_json_door_lists_and_answers_the_same_rows() {
        let (app, cookie, _, ap, id) = open_one().await;
        let res = app
            .clone()
            .oneshot(json_get("/api/v1/unsure", Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_of(res).await;
        let row = &body["items"][0];
        assert_eq!(row["id"], id.as_str(), "{body}");
        assert_eq!(row["label"], "Same thing?");
        assert_eq!(
            row["subjects"][0],
            serde_json::json!({"id": "a", "title": "Title a"})
        );
        assert_eq!(row["subjects"][1]["id"], "a-other");
        assert_eq!(
            row["weights"],
            serde_json::json!({"duplicate": 48, "distinct": 41, "replaced": 11})
        );
        assert_eq!(row["answers"], serde_json::json!(["same", "different"]));
        assert_eq!(body["waiting"], 0);
        assert!(body["next"].is_null());
        assert!(!body.to_string().contains("/ui/"), "{body}");

        ap.set_journals(false);
        let res = app
            .clone()
            .oneshot(json_post(
                &format!("/api/v1/unsure/{id}/answer"),
                Some(&cookie),
                r#"{"pick": "same"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app
            .clone()
            .oneshot(json_post(
                &format!("/api/v1/unsure/{id}/answer"),
                Some(&cookie),
                r#"{"pick": null}"#,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "a second answer");
        let body = json_of(
            app.clone()
                .oneshot(json_get("/api/v1/unsure", Some(&cookie)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["items"], serde_json::json!([]), "{body}");
        let res = app
            .clone()
            .oneshot(json_post(
                &format!("/api/v1/unsure/{id}/again"),
                Some(&cookie),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(ap.reopened(), vec![id.clone()]);

        for req in [
            json_get("/api/v1/unsure", None),
            json_post(
                &format!("/api/v1/unsure/{id}/answer"),
                None,
                r#"{"pick": null}"#,
            ),
            json_post(&format!("/api/v1/unsure/{id}/again"), None, ""),
        ] {
            let uri = req.uri().to_string();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test]
    async fn status_counts_the_open_rows() {
        let (app, cookie, _, _, _) = open_one().await;
        let body = json_of(
            app.oneshot(json_get("/api/v1/status", Some(&cookie)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["unsure"], 1, "{body}");
    }
}
