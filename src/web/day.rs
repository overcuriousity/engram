//! One day of the base: what was written as an entry, what was captured,
//! what was due, what refers to it, and the sittings — every section a read
//! over tables that exist, no model call, no prose generated.

use crate::core::ingest::{Capture, ORIGIN_JOURNAL};
use crate::core::moments::zone;
use crate::error::Error;
use crate::store::moments::Kind;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
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
pub(crate) struct TzQuery {
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

/// One artifact a sitting opened, as the row links to it. A pair of strings
/// could not say whether the label was a name or the opening of the text
/// standing in for one — see `ui::RowLabel` — and the row is a link and
/// nothing else, so it needs both.
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
    pub date: String,
    pub prev: String,
    pub next: String,
    /// What the two arrows say out loud. The heading beside them is
    /// "Sunday, 6 September 2026" and the arrows were `2026-09-05` and
    /// `2026-09-07` — three dates in one row, written two ways. Short, because
    /// they sit either side of the heading and are a step rather than a date.
    pub prev_label: String,
    pub next_label: String,
    pub tz: String,
    pub heading: String,
    pub entries: Vec<Line>,
    pub captured: Vec<Line>,
    pub was_due: Vec<Line>,
    pub refers: Vec<Line>,
    pub sittings: Vec<Sitting>,
}

impl DayTemplate {
    /// Which entry in the top row and the tab bar is the one you are inside.
    ///
    /// Read by `layout.html` to set `aria-current="page"`. The empty string is
    /// "none of them", which is a real answer for a page that hangs off no
    /// section.
    fn section(&self) -> &'static str {
        ""
    }
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

/// One day, as facts: ids, instants and labels, and nothing that belongs to a
/// rendering. No `href`, no "14:32" — the HTML page builds those from this,
/// and the JSON door serialises it as it stands, so the two are one
/// computation and cannot disagree about what happened on a day.
#[derive(serde::Serialize)]
pub(crate) struct Day {
    pub date: String,
    pub tz: String,
    /// The day's `[from, to)` in Unix seconds, in `tz`.
    pub from: i64,
    pub to: i64,
    pub entries: Vec<DayCorpus>,
    pub captured: Vec<DayCorpus>,
    pub was_due: Vec<DayMoment>,
    pub refers: Vec<DayMoment>,
    pub sittings: Vec<DaySitting>,
}

#[derive(serde::Serialize)]
pub(crate) struct DayCorpus {
    pub id: String,
    pub label: String,
    /// Whether `label` is a name somebody gave it — see `ui::RowLabel`.
    pub named: bool,
    pub at: i64,
    /// The whole text of an *entry*. A day is the one list that carries
    /// bodies, and rule 2 scopes that to entries: an entry is read on the day
    /// page, not behind it. Empty on a captured row, which is a link to a
    /// document that may be a book — `docs/api.md` rule 2.
    pub text: String,
}

#[derive(serde::Serialize)]
pub(crate) struct DayMoment {
    pub id: String,
    pub artifact_id: String,
    pub label: String,
    /// Whether `label` is a name somebody gave the artifact.
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

/// What happened on `date`, read in `tz`. `NotFound` for a date that is not
/// one, which is what both doors answer it with.
pub(crate) async fn facts(tenant: &Tenant, date: &str, tz: Tz) -> Result<Day, Error> {
    let Ok(day) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
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
        let journal = c.origin == ORIGIN_JOURNAL;
        let row = DayCorpus {
            id: c.id.clone(),
            named: c.title_hint.is_some(),
            label: crate::web::ui::corpus_label(c.title_hint.clone(), &c.raw_text, &c.origin),
            at: c.created_at,
            // Only an entry's. A captured row is a link on both doors — the
            // page draws no body under it and the app draws a `LinkRow` — and
            // a day on which a book was captured sent the whole book down the
            // wire and into the phone's cache.
            text: match journal {
                true => c.raw_text,
                false => String::new(),
            },
        };
        if journal {
            entries.push(row)
        } else {
            captured.push(row)
        }
    }

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
        entries,
        captured,
        was_due,
        refers,
        sittings,
    })
}

/// The JSON door onto the same day. `GET /api/v1/days/{date}?tz=`.
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
    // `facts` answered, so the date parses; canonical, because `facts` made it so.
    let day = NaiveDate::parse_from_str(&d.date, "%Y-%m-%d").map_err(|_| Error::NotFound)?;
    let from = d.from;
    // The zone as the zone table spells it, never as the query string spelled
    // it: it goes back out on every `prev`/`next` href and in the entry form's
    // hidden field, and `due.rs::render` normalises for the same reason.
    let tz_name = d.tz;
    let date = d.date;

    let corpus_line = |c: DayCorpus| Line {
        href: format!("/ui/corpora/{}", c.id),
        id: c.id,
        label: c.label,
        when: hm(c.at, tz),
        detail: c.text,
    };
    let moment_line = |m: DayMoment| Line {
        href: format!("/ui/artifacts/{}", m.artifact_id),
        id: m.id,
        label: m.label,
        when: hm(m.at.unwrap_or(from), tz),
        detail: match (m.kind, m.done) {
            ("due", true) => "done".to_string(),
            ("due", false) => "still open".to_string(),
            _ => m.span.unwrap_or_default(),
        },
    };
    let entries: Vec<Line> = d.entries.into_iter().map(corpus_line).collect();
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
) -> UiResult<Response> {
    // The date is a date, exactly as `page` demands — and for both of the
    // reasons `page` has plus one of its own. Unchecked, `POST
    // /ui/day/garbage/entry` stored a capture carrying `metadata.day =
    // "garbage"` that no day page could ever show; and axum percent-decodes a
    // path parameter, so a segment holding a CR or an LF made `Redirect::to`
    // fail `HeaderValue::try_from` and answer 500 *after* the entry had been
    // written — which is the failure the comment just below says was fixed for
    // the zone, arriving through the other half of the same URL.
    let Ok(day) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return Err(Error::NotFound.into());
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
) -> UiResult<Response> {
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

    /// The same short line on two days is two entries.
    ///
    /// A diary repeats itself — that is most of what a diary is. Deduplicated
    /// on the text alone, the second day's writing hashed to the first day's
    /// corpus and stored nothing; `entry` discards `ingest_capture`'s outcome,
    /// so the press redirected as though it had worked, and the second day's
    /// page then said "Nothing on this day."
    ///
    /// Both directions are checked, because the fix moves what the `UNIQUE`
    /// column means: a repeat on the *same* day must still be one entry.
    #[tokio::test]
    async fn the_same_line_written_on_two_days_is_two_entries() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        for date in ["2026-08-28", "2026-09-03", "2026-09-03"] {
            app.clone()
                .oneshot(form(
                    &format!("/ui/day/{date}/entry"),
                    &cookie,
                    "text=Long+day.&tz=Europe/Berlin",
                ))
                .await
                .unwrap();
        }

        for date in ["2026-08-28", "2026-09-03"] {
            let html = body_of(
                app.clone()
                    .oneshot(get(&format!("/ui/day/{date}?tz=Europe/Berlin"), &cookie))
                    .await
                    .unwrap(),
            )
            .await;
            assert!(
                html.contains("Long day."),
                "{date} lost the entry written on it"
            );
        }
        assert_eq!(
            core.store.recent_captures(5).await.unwrap().len(),
            2,
            "the second writing on one day made a second entry"
        );
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

    /// The JSON door, for the same three things the page is held to: an entry
    /// belongs to the day it was written about, a lenient spelling names the
    /// same day, and a day that is not a date is a 404.
    #[tokio::test]
    async fn the_json_day_holds_to_what_the_page_holds_to() {
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
            assert_eq!(day["entries"][0]["text"], "Long day.", "asked as {spelled}");
            assert!(day["captured"].as_array().unwrap().is_empty());
        }
        // Written today about the 28th: today's day must not list it.
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let day = crate::web::test_support::json_of(
            app.clone()
                .oneshot(get(&format!("/api/v1/days/{today}?tz=UTC"), &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert!(day["entries"].as_array().unwrap().is_empty(), "{day}");

        let res = app
            .oneshot(get("/api/v1/days/yesterday", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let err = crate::web::test_support::json_of(res).await;
        assert_eq!(err["error"], "not found", "one error vocabulary");
    }

    /// Rule 2 of `docs/api.md` scopes the day's bodies to entries. One
    /// `DayCorpus` serves both lists, so a captured row carried `raw_text` —
    /// for a captured book, the book — down a link that draws it nowhere: the
    /// page has no body under a captured row and the app draws a link. It
    /// landed in the phone's cache all the same.
    #[tokio::test]
    async fn a_captured_row_on_a_day_carries_no_body_and_an_entry_still_does() {
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
        // The capture lands at the real now, not on the fixed day; the row is
        // moved into the day's bounds by hand, as the fixture above does it.
        let (from, _) = bounds(NaiveDate::from_ymd_opt(2026, 8, 30).unwrap(), tz).unwrap();
        sqlx::query("UPDATE corpora SET created_at = ? WHERE id = ?")
            .bind(from + 3_600)
            .bind(&out.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form(
                "/ui/day/2026-08-30/entry",
                &cookie,
                "text=Long+day.&tz=Europe/Berlin",
            ))
            .await
            .unwrap();

        let d = crate::web::test_support::json_of(
            app.oneshot(get("/api/v1/days/2026-08-30?tz=Europe/Berlin", &cookie))
                .await
                .unwrap(),
        )
        .await;

        assert_eq!(
            d["entries"][0]["text"], "Long day.",
            "an entry is read here"
        );
        let captured = &d["captured"][0];
        assert_eq!(captured["text"], "", "a captured row is a link, not a body");
        assert!(
            captured["label"].as_str().is_some_and(|l| !l.is_empty()),
            "and it still says what it is: {captured}"
        );
    }

    /// A sitting names what it opened, and nothing else: the row is a comma
    /// list of links. A passage there was listed under the heading of the
    /// section it was cut from, and a note under the word "untitled".
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

        // The other door onto the same day, over the same fixture: two doors
        // onto one fact that disagree are worse than one door.
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
        assert!(day["entries"].as_array().unwrap().is_empty());
        assert_eq!(day["was_due"][0]["kind"], "due");
        assert_eq!(day["was_due"][0]["done"], false);
        assert_eq!(day["was_due"][0]["artifact_id"], aid.as_str());
        assert_eq!(day["was_due"][0]["at"], day_at(9));
        assert_eq!(day["refers"][0]["span"], "12.9.");
        assert_eq!(day["sittings"][0]["query"], "qdrant payload filter");
        assert_eq!(day["sittings"][0]["opened"][0]["id"], aid.as_str());
        // Facts, and nothing a rendering made of them.
        let text = day.to_string();
        assert!(!text.contains("/ui/"), "an href crossed the API: {text}");
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
