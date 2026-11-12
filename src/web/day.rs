//! One day of the base: captures, due, references and sittings, read from tables with no model call.

use crate::core::moments::zone;
use crate::error::Error;
use crate::store::moments::Kind;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/day/today", get(today))
        .route("/ui/day/{date}", get(page))
}

#[derive(serde::Deserialize)]
pub(crate) struct TzQuery {
    #[serde(default)]
    tz: String,
}

pub(crate) struct Line {
    pub href: String,
    pub label: String,
    pub when: String,
    pub detail: String,
}

/// The row is a link only, so it needs to know whether the label is a name (see `ui::RowLabel`).
#[derive(serde::Serialize)]
pub(crate) struct Opened {
    pub id: String,
    pub label: String,
    pub named: bool,
}

pub(crate) struct Sitting {
    pub span: String,
    pub query: String,
    pub searches: usize,
    pub opened: Vec<Opened>,
}

#[derive(Template)]
#[template(path = "day.html")]
pub(crate) struct DayTemplate {
    pub prev: String,
    pub next: String,
    /// Short: the arrows are a step beside the long-form heading, not dates.
    pub prev_label: String,
    pub next_label: String,
    pub tz: String,
    pub heading: String,
    pub captured: Vec<Line>,
    pub was_due: Vec<Line>,
    pub refers: Vec<Line>,
    pub sittings: Vec<Sitting>,
    /// `false` on the embed-only floor: nothing is read for a date, so the page says why "Was due" is empty.
    pub synthesizes: bool,
}

impl DayTemplate {
    fn section(&self) -> &'static str {
        ""
    }
}

impl DayTemplate {
    pub fn is_empty(&self) -> bool {
        self.captured.is_empty()
            && self.was_due.is_empty()
            && self.refers.is_empty()
            && self.sittings.is_empty()
    }
}

/// `[from, to)` in Unix seconds, in the viewer's zone.
fn bounds(date: NaiveDate, tz: Tz) -> Option<(i64, i64)> {
    // Checked: chrono's `%Y` reads six-digit years, so a URL can name `NaiveDate::MAX`.
    let next = date.checked_add_signed(chrono::Duration::days(1))?;
    Some((day_start(date, tz)?, day_start(next, tz)?))
}

/// Or the day's first instant where there is no local midnight (zones that spring forward at 00:00).
fn day_start(date: NaiveDate, tz: Tz) -> Option<i64> {
    for hour in 0..4 {
        let local = date.and_hms_opt(hour, 0, 0)?;
        if let Some(d) = tz.from_local_datetime(&local).earliest() {
            return Some(d.timestamp());
        }
    }
    None
}

fn hm(at: i64, tz: Tz) -> String {
    tz.timestamp_opt(at, 0)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

async fn today(tenant: Tenant, Query(q): Query<TzQuery>) -> UiResult<Response> {
    let tz = zone(Some(&q.tz));
    let d = tz
        .timestamp_opt(tenant.core.clock.now(), 0)
        .single()
        .map(|d| d.date_naive())
        .unwrap_or_default();
    Ok(Redirect::to(&format!(
        "/ui/day/{}?tz={}",
        d.format("%Y-%m-%d"),
        tz.name()
    ))
    .into_response())
}

/// Facts only, no rendering: the HTML page and the JSON door share this computation.
#[derive(serde::Serialize)]
pub(crate) struct Day {
    pub date: String,
    pub tz: String,
    /// `[from, to)` in Unix seconds, in `tz`.
    pub from: i64,
    pub to: i64,
    pub captured: Vec<DayCorpus>,
    pub was_due: Vec<DayMoment>,
    pub refers: Vec<DayMoment>,
    pub sittings: Vec<DaySitting>,
}

#[derive(serde::Serialize)]
pub(crate) struct DayCorpus {
    pub id: String,
    pub label: String,
    /// Whether `label` is a given name; see `ui::RowLabel`.
    pub named: bool,
    pub at: i64,
}

#[derive(serde::Serialize)]
pub(crate) struct DayMoment {
    pub id: String,
    pub artifact_id: String,
    pub label: String,
    /// Whether `label` is a given name.
    pub named: bool,
    /// `due` or `event`.
    pub kind: &'static str,
    /// `None` for a reminder with no time of its own.
    pub at: Option<i64>,
    pub done: bool,
    pub span: Option<String>,
}

#[derive(serde::Serialize)]
pub(crate) struct DaySitting {
    pub opened_at: i64,
    pub closed_at: i64,
    pub query: String,
    pub searches: usize,
    pub opened: Vec<Opened>,
}

/// `NotFound` for a date that is not one, which both doors answer with.
pub(crate) async fn facts(tenant: &Tenant, date: &str, tz: Tz) -> Result<Day, Error> {
    let Ok(day) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return Err(Error::NotFound);
    };
    // Round-tripped: chrono reads `%Y-%m-%d` leniently, so the canonical spelling goes back out.
    let date = day.format("%Y-%m-%d").to_string();
    let Some((from, to)) = bounds(day, tz) else {
        return Err(Error::NotFound);
    };
    let store = &tenant.core.store;

    // A row is a link, never a body (`docs/api.md` rule 2).
    let captured = store
        .corpora_between(from, to)
        .await?
        .into_iter()
        .map(|c| DayCorpus {
            id: c.id.clone(),
            named: c.title_hint.is_some(),
            label: crate::web::ui::corpus_label(c.title_hint.clone(), &c.raw_text, &c.origin),
            at: c.created_at,
        })
        .collect();

    let mut was_due = vec![];
    let mut refers = vec![];
    for m in store.moments_between(from, to).await? {
        let due = matches!(m.moment.kind, Kind::Due);
        let row = DayMoment {
            id: m.moment.id.clone(),
            artifact_id: m.moment.artifact_id.clone(),
            label: m.title,
            named: m.named,
            kind: if due { "due" } else { "event" },
            at: m.moment.at,
            done: m.moment.done_at.is_some(),
            span: m.moment.span.clone(),
        };
        if due {
            was_due.push(row)
        } else {
            refers.push(row)
        }
    }

    let searches = store.events_between(from, to).await?;
    let mut sittings = vec![];
    for p in store.pursuits_between(from, to).await? {
        let end = p.closed_at.unwrap_or(p.opened_at);
        let n = searches
            .iter()
            .filter(|e| e.created_at >= p.opened_at && e.created_at <= end.max(p.opened_at + 1))
            .count()
            .max(p.queries.len());
        let mut opened = vec![];
        for aid in &p.sources {
            if let Ok(a) = store.get_artifact(aid).await {
                let label = crate::web::ui::row_label(&a);
                opened.push(Opened {
                    id: aid.clone(),
                    label: label.text,
                    named: label.named,
                });
            }
        }
        sittings.push(DaySitting {
            opened_at: p.opened_at,
            closed_at: end,
            query: p.queries.first().cloned().unwrap_or_default(),
            searches: n,
            opened,
        });
    }

    Ok(Day {
        date,
        tz: tz.name().to_string(),
        from,
        to,
        captured,
        was_due,
        refers,
        sittings,
    })
}

/// `GET /api/v1/days/{date}?tz=`.
pub(crate) async fn api_day(
    tenant: Tenant,
    Path(date): Path<String>,
    Query(q): Query<TzQuery>,
) -> crate::error::Result<axum::Json<Day>> {
    Ok(axum::Json(facts(&tenant, &date, zone(Some(&q.tz))).await?))
}

async fn page(
    tenant: Tenant,
    Path(date): Path<String>,
    Query(q): Query<TzQuery>,
) -> UiResult<Response> {
    let tz = zone(Some(&q.tz));
    let d = facts(&tenant, &date, tz).await?;
    let day = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").map_err(|_| Error::NotFound)?;
    let from = d.from;
    // As the zone table spells it: it goes back out on every `prev`/`next` href.
    let tz_name = d.tz;

    let corpus_line = |c: DayCorpus| Line {
        href: format!("/ui/corpora/{}", c.id),
        label: c.label,
        when: hm(c.at, tz),
        detail: String::new(),
    };
    let moment_line = |m: DayMoment| Line {
        href: format!("/ui/artifacts/{}", m.artifact_id),
        label: m.label,
        when: hm(m.at.unwrap_or(from), tz),
        detail: match (m.kind, m.done) {
            ("due", true) => "done".to_string(),
            ("due", false) => "still open".to_string(),
            _ => m.span.unwrap_or_default(),
        },
    };
    let captured: Vec<Line> = d.captured.into_iter().map(corpus_line).collect();
    let was_due: Vec<Line> = d.was_due.into_iter().map(moment_line).collect();
    let refers: Vec<Line> = d.refers.into_iter().map(moment_line).collect();
    let sittings: Vec<Sitting> = d
        .sittings
        .into_iter()
        .map(|s| Sitting {
            span: format!("{}–{}", hm(s.opened_at, tz), hm(s.closed_at, tz)),
            query: s.query,
            searches: s.searches,
            opened: s.opened,
        })
        .collect();

    let t = DayTemplate {
        prev: day
            .checked_sub_signed(chrono::Duration::days(1))
            .unwrap_or(day)
            .format("%Y-%m-%d")
            .to_string(),
        next: day
            .checked_add_signed(chrono::Duration::days(1))
            .unwrap_or(day)
            .format("%Y-%m-%d")
            .to_string(),
        prev_label: day
            .checked_sub_signed(chrono::Duration::days(1))
            .unwrap_or(day)
            .format("%a %-d %b")
            .to_string(),
        next_label: day
            .checked_add_signed(chrono::Duration::days(1))
            .unwrap_or(day)
            .format("%a %-d %b")
            .to_string(),
        heading: day.format("%A, %-d %B %Y").to_string(),
        tz: tz_name,
        captured,
        was_due,
        refers,
        sittings,
        synthesizes: tenant.core.synthesizes(),
    };
    Ok(HtmlTemplate(t).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::Clock;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::store::moments::{NewMoment, Source};
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn an_empty_day_says_so() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Nothing on this day"));
        assert!(html.contains("/ui/day/2026-08-29") && html.contains("/ui/day/2026-08-31"));
    }

    #[tokio::test]
    async fn a_day_with_no_synthesizer_says_what_it_cannot_show() {
        let core = crate::core::test_support::test_core_without_synthesizer().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Reminders off"), "{html}");
    }

    #[tokio::test]
    async fn a_day_with_a_synthesizer_does_not_say_it() {
        let (app, cookie) = app_with_cookie(test_core().await).await;
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(!html.contains("not configured on this base"));
    }

    #[test]
    fn a_day_with_no_local_midnight_still_has_bounds() {
        // Havana skipped 00:00 on 2026-03-08, so `.earliest()` is None that day.
        let tz: Tz = "America/Havana".parse().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let (from, to) = bounds(day, tz).expect("the day starts at 01:00, not never");
        assert!(from < to);
        assert_eq!(hm(from, tz), "01:00");
    }

    #[test]
    fn the_edge_of_representable_time_is_a_404_not_a_panic() {
        // The +1 for the upper bound of `NaiveDate::MAX` must answer `None`, not panic.
        assert!(bounds(NaiveDate::MAX, "UTC".parse().unwrap()).is_none());
    }

    #[tokio::test]
    async fn today_redirects_to_the_date() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app.oneshot(get("/ui/day/today", &cookie)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(
            res.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with("/ui/day/20")
        );
    }

    #[tokio::test]
    async fn a_zone_the_table_cannot_spell_never_reaches_a_location_header() {
        // A `tz` no header may carry must not reach `Redirect::to`, or the redirect is a 500.
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        for tz in ["%C3%9C", "Europe%2FBerlin%0D%0AX:+1", "Not/AZone"] {
            let res = app
                .clone()
                .oneshot(get(&format!("/ui/day/today?tz={tz}"), &cookie))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::SEE_OTHER,
                "tz={tz} did not redirect"
            );
            let loc = res.headers()["location"].to_str().unwrap();
            assert!(
                loc.ends_with("?tz=UTC"),
                "an unreadable zone is UTC, not echoed back: {loc}"
            );
        }
    }

    #[tokio::test]
    async fn the_json_day_holds_to_what_the_page_holds_to() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        for spelled in ["2026-08-28", "2026-8-28"] {
            let day = crate::web::test_support::json_of(
                app.clone()
                    .oneshot(get(
                        &format!("/api/v1/days/{spelled}?tz=Europe/Berlin"),
                        &cookie,
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(day["date"], "2026-08-28", "asked as {spelled}");
            assert!(day["captured"].as_array().unwrap().is_empty());
        }

        let res = app
            .oneshot(get("/api/v1/days/yesterday", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let err = crate::web::test_support::json_of(res).await;
        assert_eq!(err["error"], "not found", "one error vocabulary");
    }

    #[tokio::test]
    async fn a_captured_row_on_a_day_carries_no_body() {
        let mut core = test_core().await;
        let tz = chrono_tz::Tz::Europe__Berlin;
        let day = tz
            .with_ymd_and_hms(2026, 8, 30, 0, 0, 0)
            .unwrap()
            .timestamp();
        core.clock = Clock::Fixed(day + 10 * 3_600);
        let out = core
            .ingest_capture(Capture::new(
                "a whole book, as far as this is concerned",
                "web",
            ))
            .await
            .unwrap();
        let (from, _) = bounds(NaiveDate::from_ymd_opt(2026, 8, 30).unwrap(), tz).unwrap();
        sqlx::query!(
            "UPDATE corpora SET created_at = ? WHERE id = ?",
            from + 3_600,
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let d = crate::web::test_support::json_of(
            app.oneshot(get("/api/v1/days/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;

        let captured = &d["captured"][0];
        assert!(
            captured.get("text").is_none(),
            "a captured row is a link, not a body"
        );
        assert!(
            captured["label"].as_str().is_some_and(|l| !l.is_empty()),
            "and it still says what it is: {captured}"
        );
    }

    #[tokio::test]
    async fn a_sitting_names_a_passage_it_opened_by_how_its_text_opens() {
        let mut core = test_core().await;
        let tz = chrono_tz::Tz::Europe__Berlin;
        let day = tz
            .with_ymd_and_hms(2026, 8, 30, 0, 0, 0)
            .unwrap()
            .timestamp();
        core.clock = Clock::Fixed(day + 10 * 3_600);
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
        core.store
            .insert_pursuit(
                day + 14 * 3_600,
                &["qdrant payload filter".into()],
                std::slice::from_ref(&p[0].id),
                None,
            )
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(!html.contains("Kapitel 3"), "{html}");
        assert!(html.contains("Der Vorgang setzt voraus"), "{html}");
        assert!(
            html.contains("name-opening"),
            "the opening was set as a name: {html}"
        );
    }

    #[tokio::test]
    async fn the_day_shows_captures_what_was_due_what_refers_to_it_and_sittings() {
        let mut core = test_core().await;
        let tz = chrono_tz::Tz::Europe__Berlin;
        let day = tz
            .with_ymd_and_hms(2026, 8, 30, 0, 0, 0)
            .unwrap()
            .timestamp();
        core.clock = Clock::Fixed(day + 10 * 3_600);
        let day_at = |hour: i64| day + hour * 3_600;
        let out = core
            .ingest_capture(Capture::new("Zahnarzt 12.9.", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.in_results())
            .expect("a live artifact")
            .id;
        for (kind, at, source, span) in [
            (Kind::Due, day + 9 * 3_600, Source::Set, None),
            (
                Kind::Event,
                day + 14 * 3_600,
                Source::Extracted,
                Some("12.9.".to_string()),
            ),
        ] {
            core.store
                .insert_moment(&NewMoment {
                    artifact_id: aid.clone(),
                    kind,
                    at: Some(at),
                    tz: "Europe/Berlin".into(),
                    rule: None,
                    source,
                    span,
                    series_id: None,
                })
                .await
                .unwrap();
        }
        core.store
            .insert_pursuit(
                day + 14 * 3_600,
                &["qdrant payload filter".into()],
                std::slice::from_ref(&aid),
                None,
            )
            .await
            .unwrap();
        let (from, _) = bounds(NaiveDate::from_ymd_opt(2026, 8, 30).unwrap(), tz).unwrap();
        sqlx::query!(
            "UPDATE corpora SET created_at = ? WHERE id = ?",
            from + 3_600,
            &out.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .clone()
            .oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
            .await
            .unwrap();
        let status = res.status();
        let html = body_of(res).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for s in [
            "Captured",
            "Was due",
            "Refers to this day",
            "Sittings",
            "qdrant payload filter",
            "12.9.",
        ] {
            assert!(html.contains(s), "{s}");
        }

        let res = app
            .oneshot(get("/api/v1/days/2026-08-30?tz=Europe/Berlin", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("etag"), "a read revalidates");
        let day = crate::web::test_support::json_of(res).await;
        assert_eq!(day["date"], "2026-08-30");
        assert_eq!(day["tz"], "Europe/Berlin");
        assert_eq!(day["from"], from);
        assert_eq!(day["captured"][0]["id"], out.id.as_str());
        assert_eq!(day["captured"][0]["at"], from + 3_600);
        assert_eq!(day["was_due"][0]["kind"], "due");
        assert_eq!(day["was_due"][0]["done"], false);
        assert_eq!(day["was_due"][0]["artifact_id"], aid.as_str());
        assert_eq!(day["was_due"][0]["at"], day_at(9));
        assert_eq!(day["refers"][0]["span"], "12.9.");
        assert_eq!(day["sittings"][0]["query"], "qdrant payload filter");
        assert_eq!(day["sittings"][0]["opened"][0]["id"], aid.as_str());
        let text = day.to_string();
        assert!(!text.contains("/ui/"), "an href crossed the API: {text}");
    }
}
