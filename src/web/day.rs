//! One day of the base: what was written as an entry, what was captured,
//! what was due, what refers to it, and the sittings — every section a read
//! over tables that exist, no model call, no prose generated.

use crate::core::ingest::{Capture, ORIGIN_JOURNAL};
use crate::core::moments::zone;
use crate::error::{Error, Result};
use crate::store::moments::Kind;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use askama::Template;
use axum::extract::{Form, Path, Query};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/day/today", get(today))
        .route("/ui/day/{date}", get(page))
        .route("/ui/day/{date}/entry", post(entry))
        .route("/ui/corpora/{id}/entry", post(set_entry))
}

#[derive(serde::Deserialize)]
struct TzQuery {
    #[serde(default)]
    tz: String,
}

#[derive(serde::Deserialize)]
struct EntryForm {
    text: String,
    #[serde(default)]
    tz: String,
}

#[derive(serde::Deserialize)]
struct OnForm {
    on: String,
}

pub(crate) struct Line {
    pub id: String,
    pub href: String,
    pub label: String,
    pub when: String,
    pub detail: String,
}

pub(crate) struct Sitting {
    pub span: String,
    pub query: String,
    pub searches: usize,
    pub opened: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "day.html")]
pub(crate) struct DayTemplate {
    pub date: String,
    pub prev: String,
    pub next: String,
    pub tz: String,
    pub heading: String,
    pub entries: Vec<Line>,
    pub captured: Vec<Line>,
    pub was_due: Vec<Line>,
    pub refers: Vec<Line>,
    pub sittings: Vec<Sitting>,
    pub judge_pending: Option<i64>,
}

impl DayTemplate {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.captured.is_empty()
            && self.was_due.is_empty()
            && self.refers.is_empty()
            && self.sittings.is_empty()
    }
}

/// The day's `[from, to)` in Unix seconds, in the viewer's zone.
fn bounds(date: NaiveDate, tz: Tz) -> Option<(i64, i64)> {
    let from = tz.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()?.timestamp();
    let to = tz.from_local_datetime(&(date + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)?).earliest()?.timestamp();
    Some((from, to))
}

fn hm(at: i64, tz: Tz) -> String {
    tz.timestamp_opt(at, 0).single().map(|d| d.format("%H:%M").to_string()).unwrap_or_default()
}

async fn today(tenant: Tenant, Query(q): Query<TzQuery>) -> Result<Response> {
    let tz = zone(Some(&q.tz));
    let d = tz.timestamp_opt(tenant.core.clock.now(), 0).single().map(|d| d.date_naive()).unwrap_or_default();
    Ok(Redirect::to(&format!("/ui/day/{}?tz={}", d.format("%Y-%m-%d"), q.tz)).into_response())
}

async fn page(tenant: Tenant, Path(date): Path<String>, Query(q): Query<TzQuery>) -> Result<Response> {
    let Ok(day) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else { return Err(Error::NotFound) };
    let tz = zone(Some(&q.tz));
    let Some((from, to)) = bounds(day, tz) else { return Err(Error::NotFound) };
    let store = &tenant.core.store;

    // Every corpus created on the day, plus any entry that names the day.
    let mut corpora = store.corpora_between(from, to).await?;
    for c in store.corpora_by_day(&date).await? {
        if !corpora.iter().any(|x| x.id == c.id) {
            corpora.push(c);
        }
    }
    let mut entries = vec![];
    let mut captured = vec![];
    for c in corpora {
        if c.metadata["day"].as_str().is_some_and(|d| d != date) {
            continue;
        }
        let line = Line {
            id: c.id.clone(),
            href: format!("/ui/corpora/{}", c.id),
            label: crate::web::ui::corpus_label(c.title_hint.clone(), &c.raw_text, &c.origin),
            when: hm(c.created_at, tz),
            detail: c.raw_text.clone(),
        };
        if c.origin == ORIGIN_JOURNAL {
            entries.push(line)
        } else {
            captured.push(line)
        }
    }

    let mut was_due = vec![];
    let mut refers = vec![];
    for m in store.moments_between(from, to).await? {
        let detail = match m.moment.kind {
            Kind::Due if m.moment.done_at.is_some() => "done".to_string(),
            Kind::Due => "still open".to_string(),
            Kind::Event => m.moment.span.clone().unwrap_or_default(),
        };
        let line = Line {
            id: m.moment.id.clone(),
            href: format!("/ui/artifacts/{}", m.moment.artifact_id),
            label: m.title,
            when: hm(m.moment.at.unwrap_or(from), tz),
            detail,
        };
        match m.moment.kind {
            Kind::Due => was_due.push(line),
            Kind::Event => refers.push(line),
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
                opened.push((aid.clone(), a.title.unwrap_or_else(|| "untitled".into())));
            }
        }
        sittings.push(Sitting {
            span: format!("{}–{}", hm(p.opened_at, tz), hm(end, tz)),
            query: p.queries.first().cloned().unwrap_or_default(),
            searches: n,
            opened,
        });
    }

    let t = DayTemplate {
        prev: (day - chrono::Duration::days(1)).format("%Y-%m-%d").to_string(),
        next: (day + chrono::Duration::days(1)).format("%Y-%m-%d").to_string(),
        heading: day.format("%A, %-d %B %Y").to_string(),
        date,
        tz: q.tz,
        entries,
        captured,
        was_due,
        refers,
        sittings,
        judge_pending: crate::web::state::judge_pending(&tenant).await,
    };
    Ok(HtmlTemplate(t).into_response())
}

async fn entry(tenant: Tenant, Path(date): Path<String>, Form(f): Form<EntryForm>) -> Result<Response> {
    let back = format!("/ui/day/{date}?tz={}", f.tz);
    if f.text.trim().is_empty() {
        return Ok(Redirect::to(&back).into_response());
    }
    let mut c = Capture::new(&f.text, ORIGIN_JOURNAL).with_tz(Some(f.tz.clone()));
    c.metadata["day"] = serde_json::Value::String(date.clone());
    tenant.core.ingest_capture(c).await?;
    Ok(Redirect::to(&back).into_response())
}

async fn set_entry(
    tenant: Tenant,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(f): Form<OnForm>,
) -> Result<Response> {
    tenant.core.set_entry(&id, f.on == "1").await?;
    let back = headers
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .filter(|r| r.starts_with('/') || r.contains("/ui/"))
        .unwrap_or("/ui/day/today")
        .to_string();
    Ok(Redirect::to(&back).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::Clock;
    use crate::core::test_support::test_core;
    use crate::store::moments::{NewMoment, Source};
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder().uri(uri).header("cookie", cookie).body(Body::empty()).unwrap()
    }

    fn form(uri: &str, cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn an_empty_day_says_so_and_still_offers_the_box() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie)).await.unwrap()).await;
        assert!(html.contains("Nothing on this day"));
        assert!(html.contains(r#"action="/ui/day/2026-08-30/entry""#));
        assert!(html.contains("/ui/day/2026-08-29") && html.contains("/ui/day/2026-08-31"));
    }

    #[tokio::test]
    async fn today_redirects_to_the_date() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app.oneshot(get("/ui/day/today", &cookie)).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(res.headers()["location"].to_str().unwrap().starts_with("/ui/day/20"));
    }

    #[tokio::test]
    async fn an_entry_written_on_the_day_page_belongs_to_that_day_whenever_it_was_written() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.clone()
            .oneshot(form("/ui/day/2026-08-28/entry", &cookie, "text=Long+day.&tz=Europe/Berlin"))
            .await
            .unwrap();
        let html = body_of(app.oneshot(get("/ui/day/2026-08-28?tz=Europe/Berlin", &cookie)).await.unwrap()).await;
        assert!(html.contains("Long day."));
        assert!(html.contains("Entries"));
        let c = core.store.recent_captures(1).await.unwrap();
        assert_eq!(c[0].2, "journal");
    }

    #[tokio::test]
    async fn the_day_shows_captures_what_was_due_what_refers_to_it_and_sittings() {
        let mut core = test_core().await;
        let tz = chrono_tz::Tz::Europe__Berlin;
        let day = tz.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap().timestamp();
        core.clock = Clock::Fixed(day + 10 * 3_600);
        let out = core.ingest_capture(Capture::new("Zahnarzt 12.9.", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        for (kind, at, source, span) in [
            (Kind::Due, day + 9 * 3_600, Source::Set, None),
            (Kind::Event, day + 14 * 3_600, Source::Extracted, Some("12.9.".to_string())),
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
                })
                .await
                .unwrap();
        }
        core.store
            .insert_pursuit(day + 14 * 3_600, &["qdrant payload filter".into()], &[aid.clone()], None)
            .await
            .unwrap();
        // The capture itself landed at the real now, not on the fixed day;
        // what is pinned for "Captured" is the section over a corpus created
        // inside the day's bounds, so one is written there by hand.
        let (from, _) = bounds(NaiveDate::from_ymd_opt(2026, 8, 30).unwrap(), tz).unwrap();
        sqlx::query("UPDATE corpora SET created_at = ? WHERE id = ?")
            .bind(from + 3_600)
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let res = app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie)).await.unwrap();
        let status = res.status();
        let html = body_of(res).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        for s in ["Captured", "Was due", "Refers to this day", "Sittings", "qdrant payload filter", "12.9."] {
            assert!(html.contains(s), "{s}");
        }
    }

    #[tokio::test]
    async fn not_an_entry_restores_the_origin() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Heute war ein langer Tag.", "ui")).await.unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let res = app.oneshot(form(&format!("/ui/corpora/{}/entry", out.id), &cookie, "on=0")).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "ui");
    }
}
