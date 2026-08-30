//! The band under the recommendation: what is due, in the viewer's zone.
//! Read-only over `moments`, plus the four writes a person makes with a
//! button. No model call anywhere on this page.

use crate::core::moments::{zone, DEFAULT_HOUR};
use crate::error::Result;
use crate::store::moments::{DueRow, Kind, NewMoment, Source};
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use askama::Template;
use axum::extract::{Form, Path};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use chrono::{Datelike, TimeZone, Weekday};
use chrono_tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/due", post(fragment))
        .route("/ui/moments/{id}/done", post(done))
        .route("/ui/moments/{id}/undone", post(undone))
        .route("/ui/moments/{id}/snooze", post(snooze))
        .route("/ui/moments/{id}/unsnooze", post(unsnooze))
        .route("/ui/moments/{id}/date", post(set_date))
}

#[derive(serde::Deserialize)]
struct TzForm {
    #[serde(default)]
    tz: String,
    #[serde(default)]
    until: String,
    #[serde(default)]
    when: String,
}

pub(crate) struct DueView {
    pub id: String,
    pub artifact_id: String,
    pub title: String,
    pub when: String,
    pub overdue: bool,
    pub undated: bool,
    pub recurring: bool,
    pub source: &'static str,
}

pub(crate) struct EventView {
    pub artifact_id: String,
    pub title: String,
    pub when: String,
    pub span: String,
}

#[derive(Template)]
#[template(path = "_due.html")]
pub(crate) struct DueTemplate {
    pub rows: Vec<DueView>,
    pub events: Vec<EventView>,
    pub tz: String,
    pub just: Option<String>,
    /// Seconds until this fragment should ask again, or `None` for "never".
    ///
    /// The fragment carries its own trigger, so the swap that reports the last
    /// pending thing landing is also the swap that stops the polling — the
    /// contract `_queue.html` already keeps, and the reason an idle page open
    /// in a background tab costs nothing at all.
    pub refresh_in: Option<i64>,
}

/// The cap. Further out than this and there is nothing to watch for yet: the
/// band re-reads on the five and whatever is coming is still minutes away.
const POLL_CAP: i64 = 300;
/// While a capture is still being read, its reminder does not exist yet. Two
/// seconds is the gap between "you pressed Capture" and "the band holds it".
const POLL_QUEUE: i64 = 2;

/// `queue_active` — anything of this tenant's still waiting to be read.
/// `next_at` — the next second at which the band's contents change, if any.
pub(crate) fn refresh_in(queue_active: bool, next_at: Option<i64>, now: i64) -> Option<i64> {
    if queue_active {
        return Some(POLL_QUEUE);
    }
    let ahead = next_at?.saturating_sub(now);
    // Already past and still open: the row is on screen and nothing further is
    // coming, so there is nothing to poll for.
    if ahead <= 0 {
        return None;
    }
    Some(ahead.min(POLL_CAP))
}

/// *today 14:00* / *tomorrow 09:00* / *Fri 4 Sep 09:00* / *overdue since Thu 27 Aug 12:00*.
pub(crate) fn when_words(at: i64, now: i64, tz: Tz) -> String {
    let Some(d) = tz.timestamp_opt(at, 0).single() else { return String::new() };
    let Some(n) = tz.timestamp_opt(now, 0).single() else { return String::new() };
    let days = (d.date_naive() - n.date_naive()).num_days();
    let hm = d.format("%H:%M");
    if at < now {
        return format!("overdue since {} {}", d.format("%a %-d %b"), hm);
    }
    match days {
        0 => format!("today {hm}"),
        1 => format!("tomorrow {hm}"),
        _ => format!("{} {hm}", d.format("%a %-d %b")),
    }
}

async fn render(tenant: &Tenant, tz_name: &str, just: Option<String>) -> Result<Response> {
    let tz = zone(Some(tz_name));
    let now = tenant.core.clock.now();
    let horizon = now + tenant.core.time.horizon_hours as i64 * 3_600;
    let rows = tenant
        .core
        .store
        .open_due(now, horizon)
        .await?
        .into_iter()
        .map(|r: DueRow| DueView {
            id: r.moment.id.clone(),
            artifact_id: r.moment.artifact_id.clone(),
            title: r.title,
            when: r.moment.at.map(|a| when_words(a, now, tz)).unwrap_or_else(|| "when?".into()),
            overdue: r.moment.at.is_some_and(|a| a < now),
            undated: r.moment.at.is_none(),
            recurring: r.moment.rule.is_some(),
            source: r.moment.source.as_str(),
        })
        .collect::<Vec<_>>();
    let coming = now + tenant.core.time.coming_up_days as i64 * 86_400;
    let events = tenant
        .core
        .store
        .event_moments_between(now, coming)
        .await?
        .into_iter()
        .map(|r| EventView {
            artifact_id: r.moment.artifact_id,
            title: r.title,
            when: r.moment.at.map(|a| when_words(a, now, tz)).unwrap_or_default(),
            span: r.moment.span.unwrap_or_default(),
        })
        .collect();
    // What the band is waiting for: a capture still being read, or the next
    // change to what is due — whichever is sooner.
    let queue_active = tenant.core.store.oldest_pending_age().await.unwrap_or(None).is_some();
    let next_at = tenant
        .core
        .store
        .next_due_change(now, tenant.core.time.horizon_hours as i64 * 3_600)
        .await
        .unwrap_or(None);
    let refresh_in = refresh_in(queue_active, next_at, now);
    Ok(HtmlTemplate(DueTemplate {
        rows,
        events,
        tz: tz_name.to_string(),
        just,
        refresh_in,
    })
    .into_response())
}

async fn fragment(tenant: Tenant, Form(f): Form<TzForm>) -> Result<Response> {
    render(&tenant, &f.tz, None).await
}

async fn done(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    tenant.core.complete_moment(&id).await?;
    render(&tenant, &f.tz, Some(id)).await
}

async fn undone(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    tenant.core.store.undo_done(&id).await?;
    // The row comes back, so the note comes back with it. Unconditional: a
    // corpus that was never retired is already NULL here.
    if let Some(cid) = tenant.core.store.corpus_of_moment(&id).await? {
        tenant.core.store.unretire_corpus(&cid).await?;
    }
    tenant.core.store.rearm_remind().await?;
    render(&tenant, &f.tz, None).await
}

/// `hour` = now + 1h; `tomorrow` = 09:00 tomorrow; `monday` = 09:00 next
/// Monday — in the viewer's zone.
fn snooze_until(word: &str, now: i64, tz: Tz) -> Option<i64> {
    if word == "hour" {
        return Some(now + 3_600);
    }
    let today = tz.timestamp_opt(now, 0).single()?.date_naive();
    let mut d = today + chrono::Duration::days(1);
    if word == "monday" {
        while d.weekday() != Weekday::Mon {
            d += chrono::Duration::days(1);
        }
    }
    tz.from_local_datetime(&d.and_hms_opt(DEFAULT_HOUR, 0, 0)?).single().map(|x| x.timestamp())
}

async fn snooze(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    if let Some(until) = snooze_until(&f.until, tenant.core.clock.now(), zone(Some(&f.tz))) {
        tenant.core.store.snooze(&id, until).await?;
        tenant.core.store.rearm_remind().await?;
    }
    render(&tenant, &f.tz, Some(id)).await
}

async fn unsnooze(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    tenant.core.store.unsnooze(&id).await?;
    tenant.core.store.rearm_remind().await?;
    render(&tenant, &f.tz, None).await
}

async fn set_date(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> Result<Response> {
    let tz = zone(Some(&f.tz));
    let at = chrono::NaiveDateTime::parse_from_str(&f.when, "%Y-%m-%dT%H:%M")
        .ok()
        .and_then(|dt| tz.from_local_datetime(&dt).single())
        .map(|d| d.timestamp());
    if let (Some(at), Some(m)) = (at, tenant.core.store.moment(&id).await?) {
        tenant.core.store.mark_done(&id, tenant.core.clock.now()).await?;
        tenant
            .core
            .store
            .insert_moment(&NewMoment {
                artifact_id: m.artifact_id,
                kind: Kind::Due,
                at: Some(at),
                tz: f.tz.clone(),
                rule: m.rule,
                source: Source::Set,
                span: None,
            })
            .await?;
        tenant.core.store.rearm_remind().await?;
    }
    render(&tenant, &f.tz, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::core::Core;
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn artifact_with_due(core: &Core, at: Option<i64>) -> String {
        let out = core.ingest_capture(Capture::new("Send the invoice", "ui")).await.unwrap();
        crate::jobs::test_support::drain(core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at,
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Cue,
                span: None,
            })
            .await
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
    async fn nothing_due_renders_nothing_at_all() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains(r#"id="due""#));
        assert!(!html.contains("due-filled"), "a card is drawn around something or not at all");
    }

    #[tokio::test]
    async fn overdue_then_due_then_undated_with_their_buttons() {
        let core = test_core().await;
        let now = crate::store::now();
        let late = artifact_with_due(&core, Some(now - 3_600)).await;
        let soon = artifact_with_due(&core, Some(now + 3_600)).await;
        let none = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        let (a, b, c) = (html.find(&late).unwrap(), html.find(&soon).unwrap(), html.find(&none).unwrap());
        assert!(a < b && b < c);
        assert!(html.contains("overdue"));
        assert!(html.contains(&format!("/ui/moments/{late}/done")));
        assert!(html.contains(&format!("/ui/moments/{none}/date")), "an undated reminder asks for its date");
        assert!(html.contains("due-filled"));
    }

    #[tokio::test]
    async fn done_strikes_the_row_and_undo_restores_it() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 60)).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let html = body_of(
            app.clone().oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap(),
        )
        .await;
        assert!(html.contains(&format!("/ui/moments/{id}/undone")), "an undo is offered");
        assert!(core.store.moment(&id).await.unwrap().unwrap().done_at.is_some());
        let res = app.oneshot(form(&format!("/ui/moments/{id}/undone"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(core.store.moment(&id).await.unwrap().unwrap().done_at.is_none());
    }

    #[tokio::test]
    async fn done_retires_a_note_that_was_read_as_a_reminder() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 60)).await;
        let cid = core.store.corpus_of_moment(&id).await.unwrap().unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        app.clone()
            .oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin"))
            .await
            .unwrap();
        assert!(
            core.store.is_retired(&cid).await.unwrap(),
            "the last read reminder closed, so the note retires"
        );

        app.oneshot(form(&format!("/ui/moments/{id}/undone"), &cookie, "tz=Europe/Berlin"))
            .await
            .unwrap();
        assert!(
            !core.store.is_retired(&cid).await.unwrap(),
            "undo restores the row and the note together"
        );
    }

    #[tokio::test]
    async fn a_recurring_done_retires_nothing_because_the_next_one_is_open() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Pay rent", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let at = chrono_tz::Tz::Europe__Berlin.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).unwrap().timestamp();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: Some("FREQ=MONTHLY;BYMONTHDAY=1".into()),
                source: Source::Cue,
                span: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(
            !core.store.is_retired(&out.id).await.unwrap(),
            "an occurrence closed, the reminder did not"
        );
    }

    #[tokio::test]
    async fn a_hand_set_date_on_an_ordinary_note_does_not_retire_it() {
        let core = test_core().await;
        let out = core
            .ingest_capture(Capture::new("An article about vector indexes", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(crate::store::now() + 60),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Set,
                span: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        assert!(
            !core.store.is_retired(&out.id).await.unwrap(),
            "a document with a date on it stays a document"
        );
    }

    #[tokio::test]
    async fn snooze_until_tomorrow_is_nine_in_the_viewers_zone() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() - 60)).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/snooze"), &cookie, "until=tomorrow&tz=Europe/Berlin"))
            .await
            .unwrap();
        let until = core.store.moment(&id).await.unwrap().unwrap().snoozed_until.unwrap();
        let local = chrono_tz::Tz::Europe__Berlin.timestamp_opt(until, 0).unwrap();
        assert_eq!(local.format("%H:%M").to_string(), "09:00");
        assert!(until > crate::store::now());
    }

    #[tokio::test]
    async fn setting_a_date_writes_a_new_set_row_and_closes_the_old() {
        let core = test_core().await;
        let id = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/date"), &cookie, "when=2027-01-05T10:30&tz=Europe/Berlin"))
            .await
            .unwrap();
        let old = core.store.moment(&id).await.unwrap().unwrap();
        assert!(old.done_at.is_some());
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.source, Source::Set);
        let local = chrono_tz::Tz::Europe__Berlin.timestamp_opt(rows[0].moment.at.unwrap(), 0).unwrap();
        assert_eq!(local.format("%Y-%m-%d %H:%M").to_string(), "2027-01-05 10:30");
    }

    #[tokio::test]
    async fn a_recurring_done_arms_the_next_occurrence() {
        let core = test_core().await;
        let out = core.ingest_capture(Capture::new("Pay rent", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let at = chrono_tz::Tz::Europe__Berlin.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).unwrap().timestamp();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: Some("FREQ=MONTHLY;BYMONTHDAY=1".into()),
                source: Source::Set,
                span: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(&format!("/ui/moments/{id}/done"), &cookie, "tz=Europe/Berlin")).await.unwrap();
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1);
        let local = chrono_tz::Tz::Europe__Berlin.timestamp_opt(open[0].moment.at.unwrap(), 0).unwrap();
        assert_eq!(local.format("%Y-%m-%d %H:%M").to_string(), "2026-10-01 09:00");
        assert_eq!(open[0].moment.rule.as_deref(), Some("FREQ=MONTHLY;BYMONTHDAY=1"));
    }

    #[test]
    fn the_cadence_is_the_soonest_thing_worth_asking_about() {
        assert_eq!(refresh_in(true, None, 1_000), Some(2), "a capture is still being read");
        assert_eq!(refresh_in(false, None, 1_000), None, "nothing pending, nothing asked");
        assert_eq!(refresh_in(false, Some(1_090), 1_000), Some(90), "polled at the second it lands");
        assert_eq!(refresh_in(false, Some(20_000), 1_000), Some(300), "and no later than the cap");
        assert_eq!(refresh_in(false, Some(900), 1_000), None, "already past and on screen");
    }

    #[tokio::test]
    async fn an_idle_band_with_nothing_pending_polls_not_at_all() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(!html.contains("every "), "an idle page in a background tab makes no requests: {html}");
    }

    #[tokio::test]
    async fn a_reminder_landing_soon_is_polled_for_at_its_second() {
        let core = test_core().await;
        // Inside the horizon already, so what the band is waiting for is the
        // turn from coming to overdue.
        artifact_with_due(&core, Some(crate::store::now() + 90)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains("every 90s"), "polled when it lands, not on a fixed tick: {html}");
    }

    #[tokio::test]
    async fn a_reminder_further_out_is_polled_for_at_the_cap() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 30 * 86_400)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin")).await.unwrap()).await;
        assert!(html.contains("every 300s"), "five-minute cap: {html}");
    }

    #[test]
    fn when_words_read_like_a_person_would_say_them() {
        let tz = chrono_tz::Tz::Europe__Berlin;
        let now = tz.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap().timestamp();
        assert_eq!(when_words(now + 2 * 3_600, now, tz), "today 14:00");
        assert_eq!(when_words(now + 21 * 3_600, now, tz), "tomorrow 09:00");
        assert_eq!(when_words(now + 5 * 86_400, now, tz), "Fri 4 Sep 12:00");
        assert_eq!(when_words(now - 3 * 86_400, now, tz), "overdue since Thu 27 Aug 12:00");
    }
}
