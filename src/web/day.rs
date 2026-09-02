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
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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
    // Checked: chrono's `%Y` reads signed six-digit years, so a URL can name
    // `NaiveDate::MAX`, and a plain `+ days(1)` on it panics the connection
    // away instead of answering the 404 the caller makes of `None`.
    let next = date.checked_add_signed(chrono::Duration::days(1))?;
    Some((day_start(date, tz)?, day_start(next, tz)?))
}

/// Local midnight — or, where there is no local midnight, the first instant of
/// the day there is. A zone whose clocks go forward at 00:00 (Havana, and
/// Santiago and São Paulo historically) has one day a year with no 00:00 at
/// all, and that day's page is reachable from every "today" link.
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

async fn today(tenant: Tenant, Query(q): Query<TzQuery>) -> Result<Response> {
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

async fn page(
    tenant: Tenant,
    Path(date): Path<String>,
    Query(q): Query<TzQuery>,
) -> Result<Response> {
    let Ok(day) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return Err(Error::NotFound);
    };
    // Round-tripped through the parse, the way `entry` does it and for the
    // same reason: chrono reads `%Y-%m-%d` leniently, so `/ui/day/2026-8-30`
    // parses. Left as it was spelled, that string is what `corpora_by_day`
    // matches on and what the `metadata["day"]` skip below compares against —
    // neither of which any entry ever wrote — and the page answered "nothing
    // on this day" for a day that has entries, over a form that then posted
    // the non-canonical segment back.
    let date = day.format("%Y-%m-%d").to_string();
    let tz = zone(Some(&q.tz));
    // The zone as the zone table spells it, never as the query string spelled
    // it: it goes back out on every `prev`/`next` href and in the entry form's
    // hidden field, and `due.rs::render` normalises for the same reason.
    let tz_name = tz.name().to_string();
    let Some((from, to)) = bounds(day, tz) else {
        return Err(Error::NotFound);
    };
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
        heading: day.format("%A, %-d %B %Y").to_string(),
        date,
        tz: tz_name,
        entries,
        captured,
        was_due,
        refers,
        sittings,
    };
    Ok(HtmlTemplate(t).into_response())
}

async fn entry(
    tenant: Tenant,
    Path(date): Path<String>,
    headers: HeaderMap,
    Form(f): Form<EntryForm>,
) -> Result<Response> {
    // The date is a date, exactly as `page` demands — and for both of the
    // reasons `page` has plus one of its own. Unchecked, `POST
    // /ui/day/garbage/entry` stored a capture carrying `metadata.day =
    // "garbage"` that no day page could ever show; and axum percent-decodes a
    // path parameter, so a segment holding a CR or an LF made `Redirect::to`
    // fail `HeaderValue::try_from` and answer 500 *after* the entry had been
    // written — which is the failure the comment just below says was fixed for
    // the zone, arriving through the other half of the same URL.
    let Ok(day) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return Err(Error::NotFound);
    };
    // Round-tripped through the parse, so what goes into the header and into
    // `metadata.day` is the canonical spelling and not whatever spelled it.
    let date = day.format("%Y-%m-%d").to_string();
    // Through the zone table before it reaches a `Location` header. A raw form
    // value is not header-safe — `tz=Ü`, or anything carrying a control
    // character, made `Redirect::to` build a header axum then refused to send,
    // and the day page answered 500 instead of redirecting.
    let tz_name = zone(Some(&f.tz)).name().to_string();
    let back = format!("/ui/day/{date}?tz={tz_name}");
    if f.text.trim().is_empty() {
        return Ok(Redirect::to(&back).into_response());
    }
    // The journal is a door like any other, and a diary is the text most
    // likely to be written in the writer's own language: without the stamp a
    // German entry was synthesized against the English system prompt.
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    let mut c = Capture::new(&f.text, ORIGIN_JOURNAL)
        .from_channel(crate::core::ingest::ORIGIN_WEB)
        .with_lang(lang)
        .with_tz(Some(tz_name));
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
        .and_then(same_origin_path)
        .unwrap_or_else(|| "/ui/day/today".to_string());
    Ok(Redirect::to(&back).into_response())
}

/// The path and query of a `Referer`, and never its origin.
///
/// This is a 303 out of an authenticated route, so `https://evil.example/ui/`
/// must not become a `Location`. What it must also do is *work*: the header a
/// browser actually sends is an absolute URI, always, so a filter demanding
/// `starts_with('/')` rejected every real referer and every press fell through
/// to today — and to UTC today, since the fallback carries no `?tz`. Pressing
/// "make it an entry" on `/ui/day/2026-08-15?tz=Europe/Berlin` moved the
/// reader off the day they were reading.
///
/// Keeping only path and query answers both: whatever origin wrote the header,
/// what comes back is a path on this server, so there is no origin left to
/// redirect to. A relative referer is taken as it stands, minus the
/// protocol-relative `//host`, which is a URL and not a path.
fn same_origin_path(referer: &str) -> Option<String> {
    if let Some(rest) = referer.strip_prefix('/') {
        return (!rest.starts_with('/')).then(|| referer.to_string());
    }
    let u = url::Url::parse(referer).ok()?;
    Some(match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    })
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
        Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
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
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Nothing on this day"));
        assert!(html.contains(r#"action="/ui/day/2026-08-30/entry""#));
        assert!(html.contains("/ui/day/2026-08-29") && html.contains("/ui/day/2026-08-31"));
    }

    #[test]
    fn a_day_with_no_local_midnight_still_has_bounds() {
        // Havana moved its clocks forward at 00:00 on 2026-03-08: there is no
        // 00:00 that day, and `.earliest()` on it is None. The day page is
        // reachable from every "today" link and must not 404 for it.
        let tz: Tz = "America/Havana".parse().unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap();
        let (from, to) = bounds(day, tz).expect("the day starts at 01:00, not never");
        assert!(from < to);
        assert_eq!(hm(from, tz), "01:00");
    }

    #[test]
    fn the_edge_of_representable_time_is_a_404_not_a_panic() {
        // chrono's `%Y` reads signed six-digit years, so a URL can spell the
        // last representable day; the +1 for the day's upper bound must
        // answer `None` — the page's 404 — not abort the connection.
        assert!(bounds(NaiveDate::MAX, "UTC".parse().unwrap()).is_none());
    }

    #[tokio::test]
    async fn the_entry_toggle_does_not_redirect_off_site() {
        let core = test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Long day.", "ui"))
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let mut req = form(&format!("/ui/corpora/{}/entry", out.id), &cookie, "on=1");
        req.headers_mut()
            .insert("referer", "https://evil.example/ui/".parse().unwrap());
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let to = res.headers()["location"].to_str().unwrap().to_string();
        assert!(
            to.starts_with('/') && !to.starts_with("//"),
            "an absolute URL keeps its path and loses its origin: {to}"
        );
        assert!(!to.contains("evil.example"), "{to}");
    }

    /// The half the reject-path test hid: the header a browser actually sends
    /// is absolute, so a filter that only accepted a leading `/` rejected
    /// every real referer and sent the reader to *UTC* today.
    #[tokio::test]
    async fn the_entry_toggle_comes_back_to_the_day_it_was_pressed_on() {
        let core = test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Long day.", "ui"))
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let mut req = form(&format!("/ui/corpora/{}/entry", out.id), &cookie, "on=1");
        req.headers_mut().insert(
            "referer",
            "http://localhost:7777/ui/day/2026-08-15?tz=Europe/Berlin"
                .parse()
                .unwrap(),
        );
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers()["location"],
            "/ui/day/2026-08-15?tz=Europe/Berlin"
        );
    }

    #[test]
    fn a_referer_keeps_its_path_and_never_its_origin() {
        assert_eq!(
            same_origin_path("/ui/day/2026-08-15?tz=UTC").as_deref(),
            Some("/ui/day/2026-08-15?tz=UTC")
        );
        assert_eq!(
            same_origin_path("https://evil.example/ui/day/today?tz=UTC").as_deref(),
            Some("/ui/day/today?tz=UTC")
        );
        // Protocol-relative is a URL wearing a path's clothes.
        assert_eq!(same_origin_path("//evil.example/ui/"), None);
        assert_eq!(same_origin_path("not a url at all"), None);
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
        // The raw query value went straight into `Redirect::to`. A `tz` with a
        // character no header may carry built a `Location` axum then refused
        // to send, and the day page answered 500 instead of redirecting — on
        // the `today` link, which is how the page is reached at all.
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
        // And on the entry form, which redirects back to the page it posted from.
        let (app, cookie) = app_with_cookie(test_core().await).await;
        let res = app
            .oneshot(form(
                "/ui/day/2026-08-28/entry",
                &cookie,
                "text=Long+day.&tz=%C3%9C",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/day/2026-08-28?tz=UTC");
    }

    #[tokio::test]
    async fn a_day_that_is_not_a_date_stores_nothing_and_answers_404() {
        // `page` refused it and `entry` did not. Unchecked, the capture landed
        // with `metadata.day = "garbage"`, where no day page could ever show
        // it — and a segment carrying a control character made `Redirect::to`
        // fail on the way out, so the answer was a 500 *after* the write.
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        for date in ["garbage", "2026-13-40", "2026-08-30%0d%0aX", "today"] {
            let res = app
                .clone()
                .oneshot(form(
                    &format!("/ui/day/{date}/entry"),
                    &cookie,
                    "text=Long+day.&tz=Europe/Berlin",
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{date}");
        }
        assert!(
            core.store.recent_captures(5).await.unwrap().is_empty(),
            "and nothing was stored"
        );
    }

    #[tokio::test]
    async fn an_entry_written_on_the_day_page_belongs_to_that_day_whenever_it_was_written() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.clone()
            .oneshot(form(
                "/ui/day/2026-08-28/entry",
                &cookie,
                "text=Long+day.&tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        let html = body_of(
            app.oneshot(get("/ui/day/2026-08-28?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Long day."));
        assert!(html.contains("Entries"));
        let c = core.store.recent_captures(1).await.unwrap();
        assert_eq!(c[0].2, "journal");
    }

    #[tokio::test]
    async fn a_leniently_spelled_day_still_shows_the_day_it_names() {
        // chrono reads `%Y-%m-%d` leniently, so `2026-8-28` parses and the page
        // answered 200 — but on the string as it was spelled, which is what
        // `corpora_by_day` matches and what the `metadata["day"]` skip
        // compares against. Neither ever matched, so a day holding entries
        // reported nothing on it, over a form that posted the same spelling
        // back. `entry` canonicalised; `page` did not.
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form(
                "/ui/day/2026-08-28/entry",
                &cookie,
                "text=Long+day.&tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        let html = body_of(
            app.oneshot(get("/ui/day/2026-8-28?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Long day."), "the day's own entry: {html}");
        assert!(
            html.contains(r#"action="/ui/day/2026-08-28/entry""#),
            "and the form posts the canonical day"
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
        let out = core
            .ingest_capture(Capture::new("Zahnarzt 12.9.", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        // The live one, and not simply the first: `drain` promotes this
        // capture, so `artifacts_for_corpus` opens with the superseded
        // verbatim passage. Hanging the day's moments off that row was the
        // test asserting the very thing `moments_between`'s missing
        // `status = 'active'` used to let through.
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
        let res = app
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
    }

    /// The journal is the door most likely to be written in the writer's own
    /// language, and for a while it was the one door that stamped none: a
    /// German diary entry was synthesized against the English system prompt
    /// however Settings was set.
    #[tokio::test]
    async fn a_journal_entry_is_stamped_with_the_language_it_will_be_read_in() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ui/day/2026-08-30/entry")
                    .header("cookie", &cookie)
                    .header("accept-language", "de-DE,de;q=0.9,en;q=0.8")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "text=Heute+war+ein+langer+Tag.&tz=Europe/Berlin",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let stored = &core.store.list_corpora(10, 0).await.unwrap()[0];
        assert_eq!(
            crate::infer::lang::of_corpus(&stored.metadata),
            crate::infer::lang::Lang::De
        );
    }

    #[tokio::test]
    async fn not_an_entry_restores_the_origin() {
        let core = test_core().await;
        let out = core
            .ingest_capture(Capture::new("Heute war ein langer Tag.", "ui"))
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let res = app
            .oneshot(form(
                &format!("/ui/corpora/{}/entry", out.id),
                &cookie,
                "on=0",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(core.store.get_corpus(&out.id).await.unwrap().origin, "ui");
    }
}
