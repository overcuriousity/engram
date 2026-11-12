//! The due band: what is due, in the viewer's zone. Read-only over `moments` plus four button writes.

use crate::core::moments::{DEFAULT_HOUR, zone};
use crate::store::moments::DueRow;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
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
        .route("/ui/moments/{id}/not-a-reminder", post(not_a_reminder))
        .route("/ui/artifacts/{id}/is-a-reminder", post(is_a_reminder))
}

#[derive(serde::Deserialize)]
struct TzForm {
    #[serde(default)]
    tz: String,
    #[serde(default)]
    until: String,
    #[serde(default)]
    when: String,
    /// When the band last rendered; newer rows are fresh. `0` on a page's first render.
    #[serde(default)]
    since: i64,
    /// Rides on the fragment like `tz` and `since`: the band replaces itself on every poll.
    #[serde(default)]
    all: String,
    /// Insights draws one and the workspace does not; on the fragment because the band swaps itself.
    #[serde(default)]
    head: String,
}

pub(crate) struct DueView {
    pub id: String,
    pub artifact_id: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    pub title: String,
    pub when: String,
    /// The absolute time for the tooltip; `when` drops the date once it counts down.
    pub full: String,
    /// For the client's countdown between polls; `0` on an undated row.
    pub at: i64,
    /// Between 0 and 1 (see `heat`); read only by the stylesheet.
    pub heat: String,
    pub overdue: bool,
    pub undated: bool,
    pub recurring: bool,
    pub source: &'static str,
    /// Created since the band last rendered; drives one short animation.
    pub fresh: bool,
}

pub(crate) struct EventView {
    pub artifact_id: String,
    /// Whether `title` is a given name; see `ui::RowLabel`.
    pub named: bool,
    pub title: String,
    pub when: String,
    pub span: String,
}

/// What happened to the row and the route that takes it back.
/// `undo` is a whole path: "not a reminder" deletes the moment, so its undo is addressed to the artifact.
/// `None` where nothing can restore it (a merged artifact).
pub(crate) struct Just {
    pub verb: &'static str,
    pub undo: Option<String>,
}

impl Just {
    fn moment(id: &str, verb: &'static str, undo: &str) -> Self {
        Just {
            verb,
            undo: Some(format!("/ui/moments/{id}/{undo}")),
        }
    }
}

#[derive(Template)]
#[template(path = "_due.html")]
pub(crate) struct DueTemplate {
    pub rows: Vec<DueView>,
    pub events: Vec<EventView>,
    pub tz: String,
    pub just: Option<Just>,
    /// This render's stamp, sent back so the band can tell seen rows from new; the band is otherwise stateless.
    pub since: i64,
    /// `0` when the band shows them all.
    pub hidden: usize,
    /// Echoed into `hx-vals` so the next poll keeps the viewer's shape.
    pub all: bool,
    /// Drawn inside the swapped element, so emptying the band removes the heading too.
    pub head: bool,
    /// The fragment sets its own next interval each swap; see `refresh_in`.
    pub refresh_in: Option<i64>,
}

/// The rows past the cap are the least urgent; the fold opens on a click.
pub(crate) const BAND_ROWS: usize = 8;

/// The cap and the floor: nothing due is not nothing coming.
const POLL_CAP: i64 = 300;
/// Two seconds between "you pressed Capture" and the band holding the reminder.
const POLL_QUEUE: i64 = 2;

/// `queue_active`: a capture of this tenant still being read (see `foreground_work_in_flight`).
/// Never `None`: rows this browser did not create and events need the five-minute floor.
pub(crate) fn refresh_in(queue_active: bool, next_at: Option<i64>, now: i64) -> Option<i64> {
    if queue_active {
        return Some(POLL_QUEUE);
    }
    // Already past and still open: the cap is the whole answer.
    let ahead = match next_at {
        Some(at) => at.saturating_sub(now),
        None => POLL_CAP,
    };
    if ahead <= 0 {
        return Some(POLL_CAP);
    }
    Some(ahead.min(POLL_CAP))
}

/// *today 14:00* / *tomorrow 09:00* / *Fri 4 Sep 09:00* / *overdue since Thu 27 Aug 12:00*.
pub(crate) fn when_words(at: i64, now: i64, tz: Tz) -> String {
    let Some(d) = tz.timestamp_opt(at, 0).single() else {
        return String::new();
    };
    let Some(n) = tz.timestamp_opt(now, 0).single() else {
        return String::new();
    };
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

/// Further out than this a row is neutral.
pub(crate) const HEAT_HOURS: i64 = 6;

/// 0 at the window's far edge, 1 from the moment it is due; the only urgency input for the stylesheet.
pub(crate) fn heat(at: i64, now: i64) -> f32 {
    let ahead = at - now;
    let window = HEAT_HOURS * 3_600;
    if ahead <= 0 {
        return 1.0;
    }
    if ahead >= window {
        return 0.0;
    }
    1.0 - ahead as f32 / window as f32
}

/// *45s*, *12m*, *3h 05m*, *2d 4h*, *9d*; minutes zero-padded so a countdown does not shift.
pub(crate) fn span_words(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h {:02}m", m % 60);
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d {}h", h % 24);
    }
    format!("{d}d")
}

/// A countdown inside the heat window, a wall-clock time outside it, and how late once past.
/// `DueView::full` keeps the absolute time.
pub(crate) fn due_words(at: i64, now: i64, tz: Tz) -> String {
    let ahead = at - now;
    if ahead <= 0 {
        return format!("{} overdue", span_words(-ahead));
    }
    if ahead < HEAT_HOURS * 3_600 {
        return format!("in {}", span_words(ahead));
    }
    when_words(at, now, tz)
}

async fn render(
    tenant: &Tenant,
    tz_name: &str,
    just: Option<Just>,
    since: i64,
    all: bool,
    head: bool,
) -> UiResult<Response> {
    let tz = zone(Some(tz_name));
    // As the zone table spells it: echoed into `hx-vals` JSON, where a quote would break every button.
    let tz_name = tz.name();
    let now = tenant.core.clock.now();
    let horizon = now + tenant.core.time.horizon_hours as i64 * 3_600;
    let open = tenant.core.store.open_due(now, horizon).await?;
    // Capped at the read, so the kept rows are those the read put first.
    let hidden = if all {
        0
    } else {
        open.len().saturating_sub(BAND_ROWS)
    };
    let rows = open
        .into_iter()
        .take(if all { usize::MAX } else { BAND_ROWS })
        .map(|r: DueRow| {
            // The effective instant, `COALESCE(snoozed_until, at)`, as `open_due`, the ladder and the pane use.
            let eff = r.moment.snoozed_until.or(r.moment.at);
            DueView {
                id: r.moment.id.clone(),
                artifact_id: r.moment.artifact_id.clone(),
                named: r.named,
                title: r.title,
                when: eff
                    .map(|a| due_words(a, now, tz))
                    .unwrap_or_else(|| "when?".into()),
                full: eff.map(|a| when_words(a, now, tz)).unwrap_or_default(),
                at: eff.unwrap_or(0),
                heat: eff.map_or_else(String::new, |a| format!("{:.3}", heat(a, now))),
                overdue: eff.is_some_and(|a| a < now),
                undated: eff.is_none(),
                recurring: r.moment.rule.is_some(),
                source: r.moment.source.as_str(),
                fresh: r.moment.created_at > since,
            }
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
            named: r.named,
            title: r.title,
            when: r
                .moment
                .at
                .map(|a| when_words(a, now, tz))
                .unwrap_or_default(),
            span: r.moment.span.unwrap_or_default(),
        })
        .collect();
    // The narrowed probe the idle line asks: only a reading stage writes a reminder.
    let queue_active = crate::web::ui::reading_a_capture(&tenant.core.store).await;
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
        since: now,
        hidden,
        all,
        head,
        refresh_in,
    })
    .into_response())
}

async fn fragment(tenant: Tenant, Form(f): Form<TzForm>) -> UiResult<Response> {
    render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await
}

async fn done(tenant: Tenant, Path(id): Path<String>, Form(f): Form<TzForm>) -> UiResult<Response> {
    // Only where something was finished; a press that changed nothing gets no "Done".
    let just = tenant
        .core
        .complete_moment(&id)
        .await?
        .then(|| Just::moment(&id, "Done", "undone"));
    render(&tenant, &f.tz, just, f.since, f.all == "1", f.head == "1").await
}

async fn undone(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    tenant.core.uncomplete_moment(&id).await?;
    render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await
}

/// `hour`, `tomorrow` 09:00, `monday` 09:00, in the viewer's zone; anything else is `None` and hides nothing.
fn snooze_until(word: &str, now: i64, tz: Tz) -> Option<i64> {
    if word == "hour" {
        return Some(now + 3_600);
    }
    if word != "tomorrow" && word != "monday" {
        return None;
    }
    let today = tz.timestamp_opt(now, 0).single()?.date_naive();
    let mut d = today + chrono::Duration::days(1);
    if word == "monday" {
        while d.weekday() != Weekday::Mon {
            d += chrono::Duration::days(1);
        }
    }
    let at = d.and_hms_opt(DEFAULT_HOUR, 0, 0)?;
    local(at, tz)
}

/// Resolves ambiguous and skipped local times; see `core::moments::resolve_local`.
fn local(at: chrono::NaiveDateTime, tz: Tz) -> Option<i64> {
    crate::core::moments::resolve_local(at, tz)
}

async fn snooze(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    let mut just = None;
    if let Some(until) = snooze_until(&f.until, tenant.core.clock.now(), zone(Some(&f.tz)))
        && tenant.core.store.snooze(&id, until).await?
    {
        tenant.core.store.rearm_remind().await?;
        just = Some(Just::moment(&id, "Snoozed", "unsnooze"));
    }
    render(&tenant, &f.tz, just, f.since, f.all == "1", f.head == "1").await
}

async fn unsnooze(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    tenant.core.store.unsnooze(&id).await?;
    tenant.core.store.rearm_remind().await?;
    render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await
}

/// Updated in place, not completed and recreated: `occurrences_of_rule` counts rows.
async fn set_date(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    let tz = zone(Some(&f.tz));
    let at = chrono::NaiveDateTime::parse_from_str(&f.when, "%Y-%m-%dT%H:%M")
        .ok()
        .and_then(|dt| local(dt, tz));
    if let (Some(at), Some(_)) = (at, tenant.core.store.moment(&id).await?) {
        tenant.core.store.move_moment(&id, at, tz.name()).await?;
        tenant.core.store.rearm_remind().await?;
    }
    render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await
}

/// Says the reading was wrong; the undo hands the note back to the stage rather than re-inserting a row.
async fn not_a_reminder(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    let Some(m) = tenant.core.store.moment(&id).await? else {
        return render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await;
    };
    // The banner follows what happened: a `set` row is never removed.
    // The undo only where `Core::can_be_a_reminder` says it can be honoured.
    let takes_it_back = tenant
        .core
        .can_be_a_reminder(&m.artifact_id)
        .await
        .unwrap_or(false);
    let just = tenant
        .core
        .set_reminder(&m.artifact_id, false)
        .await?
        .then(|| Just {
            verb: "Not a reminder",
            undo: takes_it_back.then(|| format!("/ui/artifacts/{}/is-a-reminder", m.artifact_id)),
        });
    tenant
        .core
        .store
        .undo_action_on(
            &id,
            crate::store::actions::Kind::Moment,
            crate::store::actions::UndoneBy::Operator,
            "not a reminder",
        )
        .await?;
    render(&tenant, &f.tz, just, f.since, f.all == "1", f.head == "1").await
}

async fn is_a_reminder(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<TzForm>,
) -> UiResult<Response> {
    // The window changed under an open page; say so rather than drop it.
    if !tenant.core.set_reminder(&id, true).await? {
        tracing::info!(
            artifact_id = id,
            "nothing to hand back to the judged read; the reminder was not restored"
        );
    }
    render(&tenant, &f.tz, None, f.since, f.all == "1", f.head == "1").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::store::moments::{Kind, NewMoment, Source};
    use crate::web::test_support::{app_with_cookie, body_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn artifact_with_due(core: &Core, at: Option<i64>) -> String {
        let out = core
            .ingest_capture(Capture::new("Send the invoice", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at,
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Cue,
                span: None,
                series_id: None,
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
    async fn a_reminder_on_a_passage_is_listed_by_how_its_text_opens() {
        let core = test_core().await;
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
            .insert_moment(&NewMoment {
                artifact_id: p[0].id.clone(),
                kind: Kind::Due,
                at: Some(crate::store::now() - 3_600),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Cue,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
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
    async fn nothing_due_renders_nothing_at_all() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains(r#"id="due""#));
        assert!(
            !html.contains("due-filled"),
            "a card is drawn around something or not at all"
        );
    }

    #[tokio::test]
    async fn the_heading_leaves_with_the_last_row_it_was_a_heading_for() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.clone()
                .oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin&head=1"))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("<h2>Due</h2>"), "{html}");
        assert!(
            html.contains(r#""head": "1""#),
            "and the next poll asks for it again: {html}"
        );

        let after = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/moments/{id}/done"),
                    &cookie,
                    "tz=Europe/Berlin&head=1",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            after.contains("undone"),
            "the undo is the one thing this render still carries: {after}"
        );
        assert!(
            after.contains("<h2>Due</h2>"),
            "an undo is still something, and a heading over it is honest: {after}"
        );

        let empty = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin&head=1"))
                .await
                .unwrap(),
        )
        .await;
        assert!(!empty.contains("due-filled"), "{empty}");
        assert!(
            !empty.contains("<h2>Due</h2>"),
            "nothing is due, so nothing says so: {empty}"
        );
    }

    #[tokio::test]
    async fn the_workspace_column_draws_no_heading() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("due-filled"), "{html}");
        assert!(!html.contains("<h2>Due</h2>"), "{html}");
    }

    #[tokio::test]
    async fn overdue_then_due_then_undated_with_their_buttons() {
        let core = test_core().await;
        let now = crate::store::now();
        let late = artifact_with_due(&core, Some(now - 3_600)).await;
        let soon = artifact_with_due(&core, Some(now + 3_600)).await;
        let none = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        let (a, b, c) = (
            html.find(&late).unwrap(),
            html.find(&soon).unwrap(),
            html.find(&none).unwrap(),
        );
        assert!(a < b && b < c);
        assert!(html.contains("overdue"));
        assert!(html.contains(&format!("/ui/moments/{late}/done")));
        assert!(
            html.contains(&format!("/ui/moments/{none}/date")),
            "an undated reminder asks for its date"
        );
        assert!(html.contains("due-filled"));
    }

    #[tokio::test]
    async fn done_strikes_the_row_and_undo_restores_it() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 60)).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let html = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/moments/{id}/done"),
                    &cookie,
                    "tz=Europe/Berlin",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(&format!("/ui/moments/{id}/undone")),
            "an undo is offered"
        );
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .done_at
                .is_some()
        );
        let res = app
            .oneshot(form(
                &format!("/ui/moments/{id}/undone"),
                &cookie,
                "tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .done_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn done_retires_a_note_that_was_read_as_a_reminder() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 60)).await;
        let cid = core.store.corpus_of_moment(&id).await.unwrap().unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        app.clone()
            .oneshot(form(
                &format!("/ui/moments/{id}/done"),
                &cookie,
                "tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        assert!(
            core.store.is_retired(&cid).await.unwrap(),
            "the last read reminder closed, so the note retires"
        );

        app.oneshot(form(
            &format!("/ui/moments/{id}/undone"),
            &cookie,
            "tz=Europe/Berlin",
        ))
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
        let out = core
            .ingest_capture(Capture::new("Pay rent", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = chrono_tz::Tz::Europe__Berlin
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
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
                series_id: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/done"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
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
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
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
                series_id: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/done"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
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
        app.oneshot(form(
            &format!("/ui/moments/{id}/snooze"),
            &cookie,
            "until=tomorrow&tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let until = core
            .store
            .moment(&id)
            .await
            .unwrap()
            .unwrap()
            .snoozed_until
            .unwrap();
        let local = chrono_tz::Tz::Europe__Berlin
            .timestamp_opt(until, 0)
            .unwrap();
        assert_eq!(local.format("%H:%M").to_string(), "09:00");
        assert!(until > crate::store::now());
    }

    #[tokio::test]
    async fn a_row_the_stage_read_can_be_told_it_is_not_a_reminder_and_taken_back() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;
        core.store
            .record_action(&crate::store::actions::NewAction {
                job: crate::store::actions::Job::Judgement,
                kind: crate::store::actions::Kind::Moment,
                subject_id: id.clone(),
                survivor_id: None,
                detail: Some("due".into()),
                evidence: serde_json::json!({ "artifact": aid }),
                pair_score: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;

        let band = body_of(
            app.clone()
                .oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            band.contains("Not a reminder"),
            "the band offers it on a row it read: {band}"
        );

        let html = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/moments/{id}/not-a-reminder"),
                    &cookie,
                    "tz=Europe/Berlin",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Not a reminder"), "{html}");
        assert!(
            html.contains(&format!("/ui/artifacts/{aid}/is-a-reminder")),
            "the undo is on the artifact: {html}"
        );
        assert!(
            core.store.moment(&id).await.unwrap().is_none(),
            "the row is withdrawn, not completed"
        );
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
        assert!(
            core.store
                .open_action_on(&id, crate::store::actions::Kind::Moment)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            core.store.recent_actions(1).await.unwrap()[0].undone_by,
            Some(crate::store::actions::UndoneBy::Operator)
        );

        app.oneshot(form(
            &format!("/ui/artifacts/{aid}/is-a-reminder"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        assert!(
            !crate::core::moments::intent_refused(
                &core
                    .store
                    .get_corpus(
                        &core
                            .store
                            .get_artifact(&aid)
                            .await
                            .unwrap()
                            .corpus_id
                            .unwrap()
                    )
                    .await
                    .unwrap()
                    .metadata,
                crate::core::moments::Intent::Remind
            ),
            "the undo withdraws the refusal"
        );
    }

    #[tokio::test]
    async fn a_reminder_carried_onto_a_merge_can_still_be_told_it_is_not_one() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;
        let merge = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "the invoice, and everything else about it".into(),
                    title: Some("Invoices".into()),
                    ..Default::default()
                },
                std::slice::from_ref(&aid),
            )
            .await
            .unwrap();
        assert_eq!(core.store.carry_moments(&aid, &merge.id).await.unwrap(), 1);
        assert!(
            core.store
                .get_artifact(&merge.id)
                .await
                .unwrap()
                .corpus_id
                .is_none(),
            "a merge belongs to no corpus, which is the whole of this case"
        );

        let (app, cookie) = app_with_cookie(core.clone()).await;
        let html = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/moments/{id}/not-a-reminder"),
                    &cookie,
                    "tz=Europe/Berlin",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Not a reminder"), "the band says so: {html}");
        assert!(
            !html.contains("is-a-reminder"),
            "and offers no undo it could not honour: {html}"
        );
        assert!(
            core.store.moment(&id).await.unwrap().is_none(),
            "the row is gone, not merely reported gone"
        );
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_reminder_somebody_set_themselves_is_not_offered_an_un_reading() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;
        core.store.delete_read_due(&aid).await.unwrap();
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(crate::store::now() + 3_600),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let band = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(!band.contains("Not a reminder"), "{band}");
    }

    #[tokio::test]
    async fn a_snooze_says_snoozed_and_its_undo_unsnoozes() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() - 60)).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let html = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/moments/{id}/snooze"),
                    &cookie,
                    "until=hour&tz=Europe/Berlin",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("Snoozed"), "{html}");
        assert!(
            html.contains(&format!("/ui/moments/{id}/unsnooze")),
            "the undo undoes this: {html}"
        );
        assert!(!html.contains(&format!("/ui/moments/{id}/undone")));

        app.oneshot(form(
            &format!("/ui/moments/{id}/unsnooze"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .snoozed_until
                .is_none(),
            "and it comes back"
        );
    }

    #[tokio::test]
    async fn the_zone_is_echoed_as_the_zone_table_spells_it() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form(
                "/ui/due",
                &cookie,
                "tz=Europe%2FBerlin%22%2C%22x%22%3A%22",
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(
            html.contains(r#"{"tz": "UTC""#),
            "an unreadable zone is UTC, not echoed back: {html}"
        );
    }

    #[tokio::test]
    async fn setting_a_date_moves_the_row_it_was_given() {
        let core = test_core().await;
        let id = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/date"),
            &cookie,
            "when=2027-01-05T10:30&tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let moved = core.store.moment(&id).await.unwrap().unwrap();
        assert!(moved.done_at.is_none(), "a move does not close anything");
        assert_eq!(
            moved.source,
            Source::Cue,
            "the reading that made it still stands"
        );
        assert!(
            moved.moved_at.is_some(),
            "and the row is marked as one somebody moved"
        );
        let rows = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.id, id);
        let local = chrono_tz::Tz::Europe__Berlin
            .timestamp_opt(rows[0].moment.at.unwrap(), 0)
            .unwrap();
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2027-01-05 10:30"
        );
    }

    #[tokio::test]
    async fn a_recurring_done_arms_the_next_occurrence() {
        let core = test_core().await;
        let out = core
            .ingest_capture(Capture::new("Pay rent", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = chrono_tz::Tz::Europe__Berlin
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
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
                series_id: None,
            })
            .await
            .unwrap();
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/done"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1);
        let local = chrono_tz::Tz::Europe__Berlin
            .timestamp_opt(open[0].moment.at.unwrap(), 0)
            .unwrap();
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2026-10-01 09:00"
        );
        assert_eq!(
            open[0].moment.rule.as_deref(),
            Some("FREQ=MONTHLY;BYMONTHDAY=1")
        );
    }

    async fn artifact_with_rule(core: &Core, rule: &str) -> String {
        let out = core
            .ingest_capture(Capture::new("Pay rent", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let at = chrono_tz::Tz::Europe__Berlin
            .with_ymd_and_hms(2026, 9, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn undoing_a_recurring_done_takes_the_next_occurrence_back_with_it() {
        let core = test_core().await;
        let id = artifact_with_rule(&core, "FREQ=MONTHLY;BYMONTHDAY=1").await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.clone()
            .oneshot(form(
                &format!("/ui/moments/{id}/done"),
                &cookie,
                "tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        assert_eq!(
            core.store
                .occurrences_of_rule(&aid, "FREQ=MONTHLY;BYMONTHDAY=1")
                .await
                .unwrap(),
            2
        );

        app.oneshot(form(
            &format!("/ui/moments/{id}/undone"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(
            open.len(),
            1,
            "one open row, not the reopened one plus its successor"
        );
        assert_eq!(open[0].moment.id, id, "and it is the row that was undone");
        assert_eq!(
            core.store
                .occurrences_of_rule(&aid, "FREQ=MONTHLY;BYMONTHDAY=1")
                .await
                .unwrap(),
            1,
            "the occurrence is given back to the count"
        );
    }

    #[tokio::test]
    async fn undoing_does_not_discard_a_successor_that_has_a_history_of_its_own() {
        let core = test_core().await;
        let id = artifact_with_rule(&core, "FREQ=MONTHLY;BYMONTHDAY=1").await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.clone()
            .oneshot(form(
                &format!("/ui/moments/{id}/done"),
                &cookie,
                "tz=Europe/Berlin",
            ))
            .await
            .unwrap();
        let next = core.store.open_due(0, i64::MAX).await.unwrap()[0]
            .moment
            .id
            .clone();
        core.store
            .snooze(&next, crate::store::now() + 86_400)
            .await
            .unwrap();

        app.oneshot(form(
            &format!("/ui/moments/{id}/undone"),
            &cookie,
            "tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        assert!(
            core.store.moment(&next).await.unwrap().is_some(),
            "the snoozed occurrence stays"
        );
    }

    #[tokio::test]
    async fn moving_a_recurring_reminder_does_not_spend_an_occurrence() {
        let core = test_core().await;
        let id = artifact_with_rule(&core, "FREQ=DAILY;COUNT=3").await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/date"),
            &cookie,
            "when=2026-09-02T10:30&tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        assert_eq!(
            core.store
                .occurrences_of_rule(&aid, "FREQ=DAILY;COUNT=3")
                .await
                .unwrap(),
            1,
            "moving is not a firing"
        );
        assert!(
            !core
                .store
                .rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=3", None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_date_in_the_hour_the_clocks_go_back_is_still_set() {
        // 02:30 on 25 October 2026 happens twice in Berlin; the earlier instant is taken.
        let core = test_core().await;
        let id = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/date"),
            &cookie,
            "when=2026-10-25T02:30&tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let at = core
            .store
            .moment(&id)
            .await
            .unwrap()
            .unwrap()
            .at
            .expect("the date is set");
        let local = chrono_tz::Tz::Europe__Berlin
            .timestamp_opt(at, 0)
            .earliest()
            .unwrap();
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2026-10-25 02:30"
        );
    }

    #[tokio::test]
    async fn a_date_in_the_hour_the_clocks_skip_is_still_set() {
        // 02:30 on 28 March 2027 never happens in Berlin; the zone's next instant is taken.
        let core = test_core().await;
        let id = artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        app.oneshot(form(
            &format!("/ui/moments/{id}/date"),
            &cookie,
            "when=2027-03-28T02:30&tz=Europe/Berlin",
        ))
        .await
        .unwrap();
        let at = core
            .store
            .moment(&id)
            .await
            .unwrap()
            .unwrap()
            .at
            .expect("the date is set");
        let local = chrono_tz::Tz::Europe__Berlin
            .timestamp_opt(at, 0)
            .earliest()
            .unwrap();
        assert_eq!(
            local.format("%Y-%m-%d %H:%M").to_string(),
            "2027-03-28 03:00"
        );
    }

    #[tokio::test]
    async fn a_snooze_word_nobody_recognises_hides_nothing() {
        let core = test_core().await;
        let id = artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        for body in ["tz=Europe/Berlin", "until=tomorow&tz=Europe/Berlin"] {
            let html = body_of(
                app.clone()
                    .oneshot(form(&format!("/ui/moments/{id}/snooze"), &cookie, body))
                    .await
                    .unwrap(),
            )
            .await;
            assert!(!html.contains("Snoozed"), "{body} is not a snooze: {html}");
            assert!(
                core.store
                    .moment(&id)
                    .await
                    .unwrap()
                    .unwrap()
                    .snoozed_until
                    .is_none(),
                "{body} left the row on the band"
            );
        }
    }

    fn rows_in(html: &str) -> usize {
        html.matches("class=\"due-row").count()
    }

    #[tokio::test]
    async fn a_band_longer_than_it_can_be_read_folds_the_rest_into_a_count() {
        let core = test_core().await;
        let now = crate::store::now();
        for i in 0..(BAND_ROWS + 4) {
            artifact_with_due(&core, Some(now + 3_600 + i as i64)).await;
        }
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            rows_in(&html),
            BAND_ROWS,
            "the band stops at its cap: {html}"
        );
        assert!(
            html.contains("4 more"),
            "and says what it is holding back: {html}"
        );
        assert!(html.contains("show all"));
    }

    #[tokio::test]
    async fn the_fold_opens_and_stays_open_across_the_bands_own_poll() {
        let core = test_core().await;
        let now = crate::store::now();
        for i in 0..(BAND_ROWS + 4) {
            artifact_with_due(&core, Some(now + 3_600 + i as i64)).await;
        }
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin&all=1"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            rows_in(&html),
            BAND_ROWS + 4,
            "everything, once asked: {html}"
        );
        assert!(html.contains("Show less"));
        assert!(
            html.contains(r#""all": "1""#),
            "the poll asks the same question again: {html}"
        );
    }

    #[tokio::test]
    async fn a_band_inside_the_cap_says_nothing_about_folding() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            !html.contains("show all"),
            "nothing is being held back: {html}"
        );
        assert!(!html.contains("Show less"));
    }

    #[tokio::test]
    async fn a_row_shows_one_verb_and_keeps_the_rest_behind_a_disclosure() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains("due-later"),
            "snooze and move are behind a disclosure: {html}"
        );
        assert!(html.contains("<summary>later</summary>"));
        assert!(html.contains(">Done<"), "and Done is the one visible verb");
    }

    #[tokio::test]
    async fn an_undated_row_opens_its_date_field_rather_than_hiding_it() {
        let core = test_core().await;
        artifact_with_due(&core, None).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            !html.contains("due-later"),
            "asking for the date is the whole point of the row"
        );
        assert!(html.contains("Set date"));
    }

    #[tokio::test]
    async fn a_row_the_viewer_has_already_seen_does_not_announce_itself_again() {
        let core = test_core().await;
        artifact_with_due(&core, Some(crate::store::now() + 3_600)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let first = body_of(
            app.clone()
                .oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin&since=0"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            first.contains("due-new"),
            "a row nobody has seen announces itself: {first}"
        );
        let later = crate::store::now() + 60;
        let again = body_of(
            app.oneshot(form(
                "/ui/due",
                &cookie,
                &format!("tz=Europe/Berlin&since={later}"),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(
            !again.contains("due-new"),
            "and does not go on announcing it: {again}"
        );
    }

    #[test]
    fn the_cadence_is_the_soonest_thing_worth_asking_about() {
        assert_eq!(
            refresh_in(true, None, 1_000),
            Some(2),
            "a capture is still being read"
        );
        assert_eq!(
            refresh_in(false, None, 1_000),
            Some(300),
            "nothing pending is not nothing coming: the band also shows events, \
             and rows arrive from doors this page did not open"
        );
        assert_eq!(
            refresh_in(false, Some(1_090), 1_000),
            Some(90),
            "polled at the second it lands"
        );
        assert_eq!(
            refresh_in(false, Some(20_000), 1_000),
            Some(300),
            "and no later than the cap"
        );
        assert_eq!(
            refresh_in(false, Some(900), 1_000),
            Some(300),
            "already past and on screen, so the cap is the whole answer"
        );
    }

    #[tokio::test]
    async fn an_idle_band_polls_no_faster_than_the_cap() {
        let core = test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(r#"data-refresh-in="300""#),
            "an idle page asks once every five minutes, and no more often: {html}"
        );
    }

    #[tokio::test]
    async fn a_reminder_landing_soon_is_polled_for_at_its_second() {
        // A fixed clock, or the assertion is off by one when the second turns.
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = crate::core::context::Clock::Fixed(now);
        artifact_with_due(&core, Some(now + 90)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(r#"data-refresh-in="90""#),
            "polled when it lands, not on a fixed tick: {html}"
        );
    }

    #[tokio::test]
    async fn a_reminder_further_out_is_polled_for_at_the_cap() {
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = crate::core::context::Clock::Fixed(now);
        artifact_with_due(&core, Some(now + 30 * 86_400)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(r#"data-refresh-in="300""#),
            "five-minute cap: {html}"
        );
    }

    #[test]
    fn when_words_read_like_a_person_would_say_them() {
        let tz = chrono_tz::Tz::Europe__Berlin;
        let now = tz
            .with_ymd_and_hms(2026, 8, 30, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(when_words(now + 2 * 3_600, now, tz), "today 14:00");
        assert_eq!(when_words(now + 21 * 3_600, now, tz), "tomorrow 09:00");
        assert_eq!(when_words(now + 5 * 86_400, now, tz), "Fri 4 Sep 12:00");
        assert_eq!(
            when_words(now - 3 * 86_400, now, tz),
            "overdue since Thu 27 Aug 12:00"
        );
    }

    #[test]
    fn a_row_counts_down_inside_the_window_and_names_the_day_outside_it() {
        let tz = chrono_tz::Tz::Europe__Berlin;
        let now = tz
            .with_ymd_and_hms(2026, 8, 30, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(due_words(now + 45, now, tz), "in 45s");
        assert_eq!(due_words(now + 12 * 60, now, tz), "in 12m");
        assert_eq!(due_words(now + 3 * 3_600 + 300, now, tz), "in 3h 05m");
        assert_eq!(due_words(now + HEAT_HOURS * 3_600, now, tz), "today 18:00");
        assert_eq!(due_words(now + 5 * 86_400, now, tz), "Fri 4 Sep 12:00");
        assert_eq!(due_words(now - 90 * 60, now, tz), "1h 30m overdue");
        assert_eq!(due_words(now - 3 * 86_400, now, tz), "3d 0h overdue");
        assert_eq!(due_words(now - 30 * 86_400, now, tz), "30d overdue");
    }

    #[test]
    fn heat_is_nothing_at_the_window_and_everything_once_due() {
        let now = 1_000_000;
        assert_eq!(heat(now + HEAT_HOURS * 3_600, now), 0.0);
        assert_eq!(heat(now + 30 * 86_400, now), 0.0);
        assert_eq!(heat(now, now), 1.0);
        assert_eq!(heat(now - 86_400, now), 1.0);
        let half = heat(now + HEAT_HOURS * 1_800, now);
        assert!(
            (half - 0.5).abs() < 0.001,
            "halfway through the window: {half}"
        );
    }

    #[tokio::test]
    async fn a_dated_row_carries_its_heat_and_its_instant() {
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = crate::core::context::Clock::Fixed(now);
        let at = now + 3 * 3_600;
        artifact_with_due(&core, Some(at)).await;
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(&format!(r#"data-due-at="{at}""#)),
            "the instant: {html}"
        );
        assert!(
            html.contains(r#"data-var-heat="0.500""#),
            "halfway up the ramp: {html}"
        );
        assert!(html.contains("in 3h 00m"), "and it counts down: {html}");
    }

    #[tokio::test]
    async fn an_elapsed_snooze_is_the_rows_time_on_the_band_too() {
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = crate::core::context::Clock::Fixed(now);
        let at = now - 6 * 3_600;
        let until = now - 300;
        let id = artifact_with_due(&core, Some(at)).await;
        assert!(core.store.snooze(&id, until).await.unwrap());
        let (app, cookie) = app_with_cookie(core).await;
        let html = body_of(
            app.oneshot(form("/ui/due", &cookie, "tz=Europe/Berlin"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            html.contains(&format!(r#"data-due-at="{until}""#)),
            "the client counts from the snooze, not from the original: {html}"
        );
        assert!(
            !html.contains(&format!(r#"data-due-at="{at}""#)),
            "and never from the original: {html}"
        );
        assert!(
            html.contains("5m overdue"),
            "five minutes, not six hours: {html}"
        );
    }
}
