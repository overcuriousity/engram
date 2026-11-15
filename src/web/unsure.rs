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
    /// (question label, sure decisions not yet carried out): held by the daily cap, by a switch
    /// that is off, or by a writer's pacing.
    pub waiting_acts: Vec<(String, i64)>,
    /// What became of a press that came too late; see `late`.
    pub notice: Option<&'static str>,
}

impl Unsure {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
            && self.waiting_rows == 0
            && self.answered.is_empty()
            && self.waiting_acts.is_empty()
            && self.notice.is_none()
    }

    /// With the sentence for `/ui/insights?unsure=<code>`, where a late press lands.
    pub(crate) fn saying(mut self, said: &Said) -> Self {
        self.notice = said.unsure.as_deref().and_then(notice);
        self
    }
}

/// The query a late press comes back to Insights with.
#[derive(serde::Deserialize, Default)]
pub(crate) struct Said {
    pub unsure: Option<String>,
}

/// (code in the address, what `judging` said, what the page says).
const LATE: [(&str, &str, &str); 3] = [
    (
        "answered",
        judging::ALREADY_ANSWERED,
        "That was already answered.",
    ),
    (
        "changed",
        judging::CHANGED_SINCE_ASKED,
        "That changed since it was asked. It will be asked again.",
    ),
    (
        "asked",
        judging::NOT_ASKED_AGAIN,
        "That is already being asked again.",
    ),
];

fn notice(code: &str) -> Option<&'static str> {
    LATE.iter().find(|l| l.0 == code).map(|l| l.2)
}

/// A press on a row that is no longer open is not a mistake to show an error page for: back to
/// Insights, which says what became of it. Anything else is the error it was.
fn late(e: crate::error::Error) -> UiResult<Response> {
    let code = match &e {
        crate::error::Error::Validation(m) => LATE.iter().find(|l| l.1 == m).map(|l| l.0),
        _ => None,
    };
    match code {
        Some(code) => Ok(Redirect::to(&format!("/ui/insights?unsure={code}")).into_response()),
        None => Err(e.into()),
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

/// The stored answer's probabilities as whole numbers, largest first. Only its numbers: a read
/// kept beside them (a link's `direction`) is an object, not a weight.
fn weights(d: &Logged) -> Vec<(String, u8)> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&d.answer).unwrap_or_default();
    let mut w: Vec<(String, u8)> = map
        .into_iter()
        .filter_map(|(k, v)| {
            let n = (v.as_f64()? * 100.0).round().clamp(0.0, 100.0) as u8;
            Some((k, n))
        })
        .collect();
    w.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    w
}

/// Every id an applier names is an artifact row it has just read (`PairApplier`, `LinkApplier`
/// and `ReapApplier` all answer with `get_artifact`'s own id and leave out what is gone), and the
/// artifact page opens any row, hidden or buried: so each subject links to its page.
fn href(subjects: Vec<(String, String)>) -> Vec<(String, String)> {
    subjects
        .into_iter()
        .map(|(id, title)| (format!("/ui/artifacts/{id}"), title))
        .collect()
}

/// The subjects and buttons of one row, or `None` when they cannot be read: that row is left
/// out and logged, and the rest of the page stands.
async fn drawn(
    core: &Core,
    a: &dyn judging::Applier,
    d: &Logged,
) -> Option<(Vec<(String, String)>, Vec<(String, String)>)> {
    let read = async {
        Ok::<_, crate::error::Error>((a.subjects(core, d).await?, a.buttons(core, d).await?))
    };
    match read.await {
        Ok(row) => Some(row),
        Err(e) => {
            tracing::warn!(decision = %d.id, error = %e, "could not draw an Unsure row; left out");
            None
        }
    }
}

/// A decision whose question has no applier here has no label or buttons, so it is skipped.
pub(crate) async fn open_rows(core: &Core) -> Result<Vec<Open>> {
    let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
    let mut out = Vec::new();
    for d in core.store.open_asks(max).await? {
        let Some(a) = core.appliers.get(&d.question_id) else {
            continue;
        };
        let Some((subjects, buttons)) = drawn(core, a.as_ref(), &d).await else {
            continue;
        };
        out.push(Open {
            label: a.label(),
            subjects,
            weights: weights(&d),
            buttons,
            id: d.id,
        });
    }
    Ok(out)
}

/// What `/status` counts as unsure: the open rows of the questions `open_rows` can list. One
/// count: `/status` is polled, and drawing every row to count them reads each row's subjects.
pub(crate) async fn open_count(core: &Core) -> Result<i64> {
    core.store.open_ask_count_of(&core.appliers.ids()).await
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
        let Some((subjects, buttons)) = drawn(core, a.as_ref(), &d).await else {
            continue;
        };
        let pick = d.answered_pick.clone().unwrap_or_default();
        let said = buttons
            .into_iter()
            .find(|(p, _)| *p == pick)
            .map_or(pick, |(_, l)| l);
        answered.push(Answered {
            label: a.label(),
            subjects: href(subjects),
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
        notice: None,
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
    match judging::answer(&tenant.core, &id, pick).await {
        Ok(_) => Ok(Redirect::to("/ui/insights").into_response()),
        Err(e) => late(e),
    }
}

async fn again_ui(tenant: Tenant, Path(id): Path<String>) -> UiResult<Response> {
    match judging::ask_again(&tenant.core, &id).await {
        Ok(()) => Ok(Redirect::to("/ui/insights").into_response()),
        Err(e) => late(e),
    }
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
    async fn a_second_press_on_an_answered_row_goes_back_to_insights_which_says_so() {
        let (app, cookie, _, ap, id) = open_one().await;
        let uri = format!("/ui/ops/unsure/{id}/answer");
        press(&app, &cookie, &uri, "pick=same").await;
        for pick in ["pick=different", "pick=leave"] {
            let res = press(&app, &cookie, &uri, pick).await;
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{pick}");
            assert_eq!(
                res.headers()["location"],
                "/ui/insights?unsure=answered",
                "{pick}"
            );
        }
        assert_eq!(ap.applied().len(), 1, "the second press did something");
        let page = get_body(&app, &cookie, "/ui/insights?unsure=answered").await;
        assert!(page.contains("That was already answered."), "{page}");
        // Only the codes a late press comes back with are said.
        let page = get_body(&app, &cookie, "/ui/insights?unsure=nope").await;
        assert!(!page.contains("Unsure"), "{page}");
    }

    #[tokio::test]
    async fn a_press_on_a_row_that_changed_and_a_second_ask_again_go_back_with_their_notice() {
        let (app, cookie, core, ap, id) = open_one().await;
        ap.set_changed(true);
        let res = press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=same",
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/insights?unsure=changed");
        assert!(ap.applied().is_empty());
        assert_eq!(ap.reopened(), vec![id]);

        ap.set_changed(false);
        ap.set_journals(false);
        let other = seeded(&core, CHOICE, "b", torn(), Band::Ask).await.id;
        press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{other}/answer"),
            "pick=same",
        )
        .await;
        let again = format!("/ui/ops/unsure/{other}/again");
        redirected_home(&press(&app, &cookie, &again, "").await);
        let res = press(&app, &cookie, &again, "").await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/insights?unsure=asked");
        assert_eq!(ap.reopened().len(), 2, "the second press did something");
    }

    #[tokio::test]
    async fn an_unknown_row_is_still_a_404_and_an_unknown_answer_still_a_400() {
        let (app, cookie, _, _, id) = open_one().await;
        let res = press(&app, &cookie, "/ui/ops/unsure/nope/answer", "pick=same").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=sideways",
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_of(res).await;
        assert!(body.contains("that is not one of the answers"), "{body}");
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
        assert!(page.contains("2 sure, not yet carried out"), "{page}");
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

    async fn status_and_list(app: &axum::Router, cookie: &str) -> (serde_json::Value, usize) {
        let status = json_of(
            app.clone()
                .oneshot(json_get("/api/v1/status", Some(cookie)))
                .await
                .unwrap(),
        )
        .await;
        let list = json_of(
            app.clone()
                .oneshot(json_get("/api/v1/unsure", Some(cookie)))
                .await
                .unwrap(),
        )
        .await;
        let listed = list["items"].as_array().unwrap().len();
        (status["unsure"].clone(), listed)
    }

    #[tokio::test]
    async fn status_counts_what_is_listed_not_a_row_nothing_here_can_show() {
        let (app, cookie, core, _, _) = open_one().await;
        // Open, of a question no applier answers: it has no label and no buttons.
        let id = core
            .store
            .log_decision(&crate::store::decision_log::NewLogged {
                question_id: "gone_question".into(),
                question_version: 1,
                model: "m".into(),
                subject_kind: "pair",
                subject_a: "x".into(),
                subject_b: None,
                rev_a: None,
                rev_b: None,
                answer: "{}".into(),
                top: 0.7,
                pick: "yes".into(),
                swapped_agree: None,
                band: Band::Ask,
                reason: None,
                state: State::Waiting,
            })
            .await
            .unwrap();
        assert!(core.store.seat_ask(&id, 20).await.unwrap());
        assert_eq!(core.store.open_ask_count().await.unwrap(), 2);
        let (count, listed) = status_and_list(&app, &cookie).await;
        assert_eq!(listed, 1);
        assert_eq!(count, 1, "the count and the list disagree");
    }

    #[tokio::test]
    async fn a_row_whose_subjects_cannot_be_read_is_left_out_and_the_page_stands() {
        let (app, cookie, core, ap, _) = open_one().await;
        seeded(&core, CHOICE, "b", torn(), Band::Ask).await;
        ap.set_unreadable("a");
        let page = insights(&app, &cookie).await;
        assert!(page.contains("Unsure · 1"), "{page}");
        assert!(page.contains("Title b"), "{page}");
        assert!(!page.contains("Title a<"), "{page}");
        let (count, listed) = status_and_list(&app, &cookie).await;
        assert_eq!(listed, 1);
        // Counted by its question without drawing it: the row is open, and will show again.
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn an_answer_that_cannot_be_read_is_left_out_of_the_answered_list() {
        let (app, cookie, _, ap, id) = open_one().await;
        ap.set_journals(false);
        press(
            &app,
            &cookie,
            &format!("/ui/ops/unsure/{id}/answer"),
            "pick=same",
        )
        .await;
        ap.set_unreadable("a");
        let page = insights(&app, &cookie).await;
        assert!(!page.contains("answered ("), "{page}");
    }

    #[tokio::test]
    async fn a_read_kept_beside_the_weights_is_not_shown_as_a_weight() {
        let (app, cookie, core, _) = app().await;
        let mut answer = torn();
        answer["direction"] = serde_json::json!({
            "pick": "a_first", "top": 0.9, "agree": true, "exchanged": "b_first"
        });
        let d = seeded(&core, CHOICE, "a", answer, Band::Ask).await;
        assert_eq!(d.state, State::Open);
        let page = insights(&app, &cookie).await;
        // The span ends after the last weight: no "direction 0" follows.
        assert!(
            page.contains("duplicate 48 · distinct 41 · replaced 11</span>"),
            "{page}"
        );
        let body = json_of(
            app.clone()
                .oneshot(json_get("/api/v1/unsure", Some(&cookie)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            body["items"][0]["weights"],
            serde_json::json!({"duplicate": 48, "distinct": 41, "replaced": 11}),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_heading_gives_no_count_when_no_row_is_open() {
        let (app, cookie, core, _) = app().await;
        seeded(
            &core,
            CHOICE,
            "c",
            serde_json::json!({"same": 0.95, "different": 0.05}),
            Band::Act,
        )
        .await;
        let page = insights(&app, &cookie).await;
        assert!(page.contains("1 sure, not yet carried out"), "{page}");
        assert!(page.contains(">Unsure</h2>"), "{page}");
        assert!(!page.contains("Unsure · 0"), "{page}");
    }

    #[tokio::test]
    async fn every_subject_a_built_in_question_names_is_an_artifact_that_opens() {
        let core = crate::core::test_support::test_core().await;
        let a = crate::core::test_support::seed_artifact(&core, "first").await;
        let b = crate::core::test_support::seed_artifact(&core, "second").await;
        core.store.record_pair(&a, &b, 0.9).await.unwrap();
        let pair = core
            .store
            .pair_between(&a, &b)
            .await
            .unwrap()
            .unwrap()
            .id
            .to_string();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        for (question, kind, first, second, named) in [
            ("pair_relation", "pair", pair, None, 2),
            ("link_kind", "link", a.clone(), Some(b.clone()), 2),
            ("reap_worth", "artifact", a.clone(), None, 1),
            // What is gone is named by nothing, so no link leads nowhere.
            ("pair_relation", "pair", "999999".to_string(), None, 0),
            (
                "link_kind",
                "link",
                "gone".to_string(),
                Some("too".to_string()),
                0,
            ),
            ("reap_worth", "artifact", "gone".to_string(), None, 0),
        ] {
            let id = core
                .store
                .log_decision(&crate::store::decision_log::NewLogged {
                    question_id: question.into(),
                    question_version: 1,
                    model: "m".into(),
                    subject_kind: kind,
                    subject_a: first,
                    subject_b: second,
                    rev_a: None,
                    rev_b: None,
                    answer: "{}".into(),
                    top: 0.7,
                    pick: String::new(),
                    swapped_agree: None,
                    band: Band::Ask,
                    reason: None,
                    state: State::Open,
                })
                .await
                .unwrap();
            let d = core.store.logged(&id).await.unwrap().unwrap();
            let applier = core.appliers.get(question).unwrap();
            let subjects = applier.subjects(&core, &d).await.unwrap();
            assert_eq!(subjects.len(), named, "{question}: {subjects:?}");
            for (href, _) in super::href(subjects) {
                // `get_body` asserts the page answers 200.
                get_body(&app, &cookie, &href).await;
            }
        }
    }
}
