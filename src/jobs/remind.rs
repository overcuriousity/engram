//! Push for what is due: the channels a user configured, and the unit that
//! sleeps until the next due moment and posts it.

use crate::core::Core;
use crate::error::Result;
use crate::jobs::webpush::{Kind, MAX_PLAINTEXT, Payload, PayloadMoment, Vapid, WebPushKeys};

pub const REMIND_TARGET: &str = "due";

/// Longest lead first, the last rung the moment itself. The first is the band's
/// `time.horizon_hours` (48), so phone and screen agree.
pub const LEADS: &[i64] = &[48 * 3_600, 12 * 3_600, 3 * 3_600, 30 * 60, 0];

/// A snoozed row climbs only the last rung: `eff_at` becomes `snoozed_until`, so every longer lead
/// would already be owed the moment it is snoozed.
fn ladder(snoozed: bool) -> &'static [i64] {
    if snoozed {
        &LEADS[LEADS.len() - 1..]
    } else {
        LEADS
    }
}

/// A rung that fell before the moment was created says nothing about it; otherwise most reminders
/// would push beside `confirm_created`.
fn reachable(lead: i64, eff_at: i64, created_at: i64) -> bool {
    lead == 0 || eff_at - lead >= created_at
}

/// The nearest passed rung the last push did not cover, so a reminder set ten minutes out pushes
/// once. `created_at` is the moment's own (`reachable`).
pub fn owed_lead(
    eff_at: i64,
    snoozed: bool,
    notified_at: Option<i64>,
    created_at: i64,
    now: i64,
) -> Option<i64> {
    ladder(snoozed)
        .iter()
        .copied()
        .filter(|lead| reachable(*lead, eff_at, created_at))
        .filter(|lead| eff_at - lead <= now)
        .rfind(|lead| notified_at.is_none_or(|n| n < eff_at - lead))
}

/// The earliest rung the last push did not cover, past or future.
pub fn next_lead_at(
    eff_at: i64,
    snoozed: bool,
    notified_at: Option<i64>,
    created_at: i64,
) -> Option<i64> {
    ladder(snoozed)
        .iter()
        .filter(|lead| reachable(**lead, eff_at, created_at))
        .map(|lead| eff_at - lead)
        .find(|at| notified_at.is_none_or(|n| n < *at))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Gotify {
        url: String,
        token: String,
    },
    /// `keys` make the push RFC 8291 ciphertext the push service cannot read. `None` is an endpoint
    /// typed in by hand, which keeps receiving plaintext.
    UnifiedPush {
        endpoint: String,
        keys: Option<WebPushKeys>,
    },
}

/// A Gotify entry needs url and token; a UnifiedPush entry is its endpoint, with keys where both
/// were registered.
pub fn notify_targets(notify: &serde_json::Value) -> Vec<Target> {
    let mut out = vec![];
    if let (Some(url), Some(token)) = (
        notify["gotify"]["url"].as_str(),
        notify["gotify"]["token"].as_str(),
    ) && !url.is_empty()
        && !token.is_empty()
    {
        out.push(Target::Gotify {
            url: url.into(),
            token: token.into(),
        });
    }
    if let Some(e) = notify["unifiedpush"]["endpoint"].as_str()
        && !e.is_empty()
    {
        let keys = match (
            notify["unifiedpush"]["p256dh"].as_str(),
            notify["unifiedpush"]["auth"].as_str(),
        ) {
            (Some(p256dh), Some(auth)) if !p256dh.is_empty() && !auth.is_empty() => {
                Some(WebPushKeys {
                    p256dh: p256dh.into(),
                    auth: auth.into(),
                })
            }
            _ => None,
        };
        out.push(Target::UnifiedPush {
            endpoint: e.into(),
            keys,
        });
    }
    out
}

/// Composed once per wake, so no channel disagrees with another about what was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
    pub payload: Payload,
}

impl Message {
    /// Not on the ladder: a confirmation or a test. The payload is cut to one record, or
    /// `webpush::request` refuses it and a confirmation is never retried.
    pub fn notice(title: &str, body: &str, now: i64) -> Message {
        let mut cap = title.chars().count().max(body.chars().count());
        let mut payload = notice_payload(title, body, now, usize::MAX);
        // JSON escaping makes the arithmetic inexact, so measure; three quarters per pass reaches
        // zero.
        while cap > 0 && payload.to_bytes().len() > MAX_PLAINTEXT {
            cap = cap * 3 / 4;
            payload = notice_payload(title, body, now, cap);
        }
        Message {
            title: title.into(),
            body: body.into(),
            payload,
        }
    }

    /// The payload takes the first `BODY_LINES` rows by id, the rest as a count, cut further until
    /// one record holds it: an over-long payload is refused on every retry, and the reminder never
    /// rings.
    pub fn due(rows: &[crate::store::moments::DueRow], now: i64) -> Message {
        let (title, body) = compose(rows, now);
        let mut take = rows.len().min(BODY_LINES);
        let mut payload = due_payload(rows, now, take, usize::MAX);
        while take > 1 && payload.to_bytes().len() > MAX_PLAINTEXT {
            take -= 1;
            payload = due_payload(rows, now, take, usize::MAX);
        }
        // JSON escaping makes the arithmetic inexact, so measure; three quarters per pass reaches
        // zero.
        let mut cap = rows.first().map_or(0, |row| row.title.chars().count());
        while cap > 0 && payload.to_bytes().len() > MAX_PLAINTEXT {
            cap = cap * 3 / 4;
            payload = due_payload(rows, now, take, cap);
        }
        Message {
            title,
            body,
            payload,
        }
    }
}

fn notice_payload(title: &str, body: &str, now: i64, cap: usize) -> Payload {
    Payload::new(Kind::Notice {
        at: now,
        title: title.chars().take(cap).collect(),
        body: body.chars().take(cap).collect(),
    })
}

fn due_payload(
    rows: &[crate::store::moments::DueRow],
    now: i64,
    take: usize,
    cap: usize,
) -> Payload {
    let moments = rows
        .iter()
        .take(take)
        .map(|row| PayloadMoment {
            id: row.moment.id.clone(),
            title: row.title.chars().take(cap).collect(),
            at: row.moment.snoozed_until.or(row.moment.at).unwrap_or(now),
        })
        .collect();
    Payload::new(Kind::Due {
        at: now,
        moments,
        more: rows.len().saturating_sub(take),
    })
}

/// The instance's VAPID identity and contact, loaded once per wake rather than per target.
pub struct Sender {
    pub vapid: Vapid,
    /// RFC 8292's `sub`, which a sender SHOULD carry: `mailto:` the user's address where one is
    /// known.
    pub sub: Option<String>,
}

impl Sender {
    pub async fn load(core: &Core) -> Result<Sender> {
        let vapid = Vapid::from_keys(&core.store.control.vapid().await?)?;
        let sub = core
            .store
            .control
            .user(&core.store.subject)
            .await?
            .and_then(|u| u.email)
            .map(|e| format!("mailto:{e}"));
        Ok(Sender { vapid, sub })
    }
}

/// Gotify: JSON body, token in a header. Keyed UnifiedPush: one `aes128gcm` record under VAPID
/// (`jobs::webpush`). Keyless: plaintext.
pub async fn push(
    http: &reqwest::Client,
    target: &Target,
    msg: &Message,
    sender: &Sender,
) -> crate::error::Result<()> {
    let (title, message) = (&msg.title, &msg.body);
    let res = match target {
        Target::Gotify { url, token } => {
            http.post(url)
                .header("X-Gotify-Key", token)
                .json(&serde_json::json!({ "title": title, "message": message, "priority": 5 }))
                .send()
                .await
        }
        Target::UnifiedPush {
            endpoint,
            keys: Some(keys),
        } => {
            let req = crate::jobs::webpush::request(
                endpoint,
                keys,
                &sender.vapid,
                sender.sub.as_deref(),
                &msg.payload.to_bytes(),
            )?;
            // Executed rather than rebuilt, so the client's redirect policy and timeout apply here
            // too.
            let req = reqwest::Request::try_from(req)
                .map_err(|e| crate::error::Error::Internal(format!("web push request: {e}")))?;
            http.execute(req).await
        }
        Target::UnifiedPush {
            endpoint,
            keys: None,
        } => {
            http.post(endpoint)
                .body(format!("{title}\n{message}"))
                .send()
                .await
        }
    };
    let res = res.map_err(|e| crate::error::Error::Inference {
        role: "push",
        detail: e.to_string(),
    })?;
    if !res.status().is_success() {
        return Err(crate::error::Error::Inference {
            role: "push",
            detail: format!("HTTP {}", res.status()),
        });
    }
    Ok(())
}

/// A lock screen shows a handful of lines; the rest is a count, and the band has them all.
pub const BODY_LINES: usize = 8;

/// Several rows collapse into one message: twenty buzzes at once are twenty reasons to turn the
/// channel off.
pub fn compose(rows: &[crate::store::moments::DueRow], now: i64) -> (String, String) {
    // `snoozed_until` first, everywhere: the ladder is keyed on the effective time.
    let eff = |m: &crate::store::moments::Moment| m.snoozed_until.or(m.at).unwrap_or(now);
    if let [row] = rows {
        let when = crate::web::due::when_words(
            eff(&row.moment),
            now,
            crate::core::moments::zone(Some(&row.moment.tz)),
        );
        // The opening only where the title is a name; otherwise the label already is the opening.
        let body = match row.named {
            true => format!("{}\n{}", row.opening, when),
            false => when,
        };
        return (row.title.clone(), body);
    }
    let mut body: Vec<String> = rows
        .iter()
        .take(BODY_LINES)
        .map(|row| {
            let when = crate::web::due::due_words(
                eff(&row.moment),
                now,
                crate::core::moments::zone(Some(&row.moment.tz)),
            );
            format!("{} — {when}", row.title)
        })
        .collect();
    if let Some(rest) = rows.len().checked_sub(BODY_LINES).filter(|n| *n > 0) {
        body.push(format!("…and {rest} more"));
    }
    (format!("{} reminders", rows.len()), body.join("\n"))
}

/// `Ok` once any channel took it. Neither caller reads or writes `notified_at` here, so the ladder
/// and a confirmation never step on each other.
async fn deliver(
    http: &reqwest::Client,
    sender: &Sender,
    targets: &[Target],
    msg: &Message,
) -> Result<()> {
    let mut delivered = false;
    let mut failure = None;
    for t in targets {
        match push(http, t, msg, sender).await {
            Ok(()) => delivered = true,
            Err(e) => {
                tracing::warn!(error = %e, "a push channel refused");
                failure = Some(e);
            }
        }
    }
    if !delivered {
        return Err(failure.unwrap_or_else(|| {
            crate::error::Error::Internal("no target was delivered to".into())
        }));
    }
    Ok(())
}

/// `Policy::none()`: `points_inward` vets the saved URL once, and a followed redirect to loopback
/// would turn the Settings test button into a port scanner.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))
}

/// Outside the due-time ladder, no row read or marked; silent when no channel is configured.
pub async fn notify_now(core: &Core, title: &str, message: &str) -> Result<()> {
    let targets = notify_targets(&core.store.control.notify(&core.store.subject).await?);
    if targets.is_empty() {
        return Ok(());
    }
    let msg = Message::notice(title, message, core.clock.now());
    deliver(&http_client()?, &Sender::load(core).await?, &targets, &msg).await
}

/// Moments are marked once any channel took the message: a retry would duplicate on the channel
/// that succeeded, which is worse than a miss on the other.
pub async fn run(core: &Core) -> Result<()> {
    let targets = notify_targets(&core.store.control.notify(&core.store.subject).await?);
    if targets.is_empty() {
        return Ok(());
    }
    let now = core.clock.now();
    let owed = core.store.due_owed(now).await?;
    if owed.is_empty() {
        return Ok(());
    }
    let msg = Message::due(&owed, now);
    deliver(&http_client()?, &Sender::load(core).await?, &targets, &msg).await?;
    let ids: Vec<String> = owed.iter().map(|r| r.moment.id.clone()).collect();
    // The message is out, so the mark is the fragile step: a `?` retry would deliver the whole
    // batch again. Retried until it sticks; the cause is a brief writer lock.
    let mut failed = None;
    for attempt in 0..5u32 {
        match core.store.mark_notified(&ids, now).await {
            Ok(()) => {
                failed = None;
                break;
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "could not record a push that went out");
                failed = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(50 << attempt)).await;
            }
        }
    }
    if let Some(e) = failed {
        // Returned, so the unit backs off rather than re-arming at a past instant in a tight loop
        // of duplicates. Once exhausted, `run_claimed` closes it and `arm_missing_periodic` brings
        // it back.
        tracing::error!(error = %e, n = ids.len(), "a push went out that could not be recorded");
        return Err(e);
    }
    // Not re-armed here: `arm_at` refuses a `running` row; `jobs::run_claimed` re-arms after
    // closing it.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::Clock;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::store::jobs::Stage;
    use crate::store::moments::{Kind, NewMoment, Source};

    async fn due_at(core: &Core, at: i64) -> String {
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
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(at),
                tz: "UTC".into(),
                rule: None,
                source: Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        core.store.rearm_remind().await.unwrap();
        id
    }

    #[test]
    fn a_push_over_a_nameless_reminder_does_not_say_its_text_twice() {
        let now = 1_787_320_320;
        let bare = crate::store::moments::DueRow {
            named: false,
            title: "Der Vorgang setzt voraus, dass das Journal".into(),
            opening: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
            ..row(Some(now + 3_600), None)
        };
        let (title, body) = compose(std::slice::from_ref(&bare), now);
        assert!(title.starts_with("Der Vorgang setzt voraus"), "{title}");
        assert!(
            !body.contains("Der Vorgang setzt voraus"),
            "the text stood in the title and again in the body: {body}"
        );
        assert!(!body.trim().is_empty(), "the body still says when: {body}");

        // A name is not the text, so the opening under it says something new.
        let (title, body) = compose(&[row(Some(now + 3_600), None)], now);
        assert_eq!(title, "Send the invoice");
        assert!(body.contains("Send the invoice"), "{body}");
    }

    #[test]
    fn a_due_payload_drops_moments_until_one_record_holds_it() {
        let now = 1_787_320_320;
        let long = "ü".repeat(600);
        let rows: Vec<_> = (0..BODY_LINES)
            .map(|i| crate::store::moments::DueRow {
                title: long.clone(),
                ..row(Some(now + i as i64), None)
            })
            .collect();
        let msg = Message::due(&rows, now);
        assert!(
            msg.payload.to_bytes().len() <= MAX_PLAINTEXT,
            "{} bytes",
            msg.payload.to_bytes().len()
        );
        let crate::jobs::webpush::Kind::Due { moments, more, .. } = &msg.payload.kind else {
            panic!("a due payload")
        };
        assert!(!moments.is_empty(), "something still rings");
        assert!(moments.len() < rows.len(), "and something was dropped");
        assert_eq!(
            moments.len() + more,
            rows.len(),
            "what was dropped is counted, and the band has it"
        );
    }

    #[test]
    fn a_notice_past_one_record_is_shortened_and_still_sent() {
        let now = 1_787_320_320;
        let title = format!("Reminder set: {}", "ü".repeat(MAX_PLAINTEXT));
        let msg = Message::notice(&title, "in an hour", now);
        assert!(
            msg.payload.to_bytes().len() <= MAX_PLAINTEXT,
            "{} bytes",
            msg.payload.to_bytes().len()
        );
        let crate::jobs::webpush::Kind::Notice {
            title: t, body: b, ..
        } = &msg.payload.kind
        else {
            panic!("a notice payload")
        };
        assert!(!t.is_empty(), "shortened, not emptied");
        assert_eq!(b, "in an hour", "the short half is left alone");
        assert_eq!(
            msg.title, title,
            "the prose channels have no such limit and still say everything"
        );
    }

    #[test]
    fn a_notice_that_fits_is_left_whole() {
        let msg = Message::notice("Reminder set: milk", "tomorrow at nine", 1_787_320_320);
        let crate::jobs::webpush::Kind::Notice {
            title: t, body: b, ..
        } = &msg.payload.kind
        else {
            panic!("a notice payload")
        };
        assert_eq!(t, "Reminder set: milk");
        assert_eq!(b, "tomorrow at nine");
    }

    #[test]
    fn a_single_title_past_one_record_is_shortened_and_still_sent() {
        let now = 1_787_320_320;
        let rows = vec![crate::store::moments::DueRow {
            title: "ü".repeat(MAX_PLAINTEXT),
            ..row(Some(now), None)
        }];
        let msg = Message::due(&rows, now);
        assert!(msg.payload.to_bytes().len() <= MAX_PLAINTEXT);
        let crate::jobs::webpush::Kind::Due { moments, more, .. } = &msg.payload.kind else {
            panic!("a due payload")
        };
        assert_eq!(moments.len(), 1);
        assert_eq!(*more, 0);
        assert!(!moments[0].title.is_empty(), "shortened, not emptied");
        assert_eq!(
            moments[0].id, rows[0].moment.id,
            "still the moment it is about"
        );
    }

    fn row(at: Option<i64>, snoozed_until: Option<i64>) -> crate::store::moments::DueRow {
        crate::store::moments::DueRow {
            named: true,
            moment: crate::store::moments::Moment {
                id: "m1".into(),
                artifact_id: "a1".into(),
                kind: Kind::Due,
                at,
                tz: "UTC".into(),
                rule: None,
                source: Source::Set,
                span: None,
                done_at: None,
                snoozed_until,
                notified_at: None,
                moved_from: None,
                moved_at: None,
                series_id: None,
                origin_corpus_id: None,
                until: None,
                created_at: 0,
            },
            title: "Send the invoice".into(),
            opening: "Send the invoice".into(),
        }
    }

    #[test]
    fn a_snoozed_reminder_is_described_by_the_time_it_was_put_aside_until() {
        let due = 1_600_000_000;
        let snoozed = due + 6 * 3_600;
        let (_, body) = compose(&[row(Some(due), Some(snoozed))], snoozed);
        let (_, unsnoozed) = compose(&[row(Some(snoozed), None)], snoozed);
        assert_eq!(
            body, unsnoozed,
            "a row put aside until now reads exactly like one due now: {body}"
        );
        assert!(
            !body.contains("ago"),
            "and never counts from the instant it was moved off: {body}"
        );
        let (_, many) = compose(
            &[row(Some(due), Some(snoozed)), row(Some(due), Some(snoozed))],
            snoozed,
        );
        let (_, many_unsnoozed) = compose(
            &[row(Some(snoozed), None), row(Some(snoozed), None)],
            snoozed,
        );
        assert_eq!(many, many_unsnoozed);
    }

    #[tokio::test]
    async fn a_push_endpoint_that_redirects_is_not_followed() {
        let inward = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&inward)
            .await;

        let allowed = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(302).insert_header("location", inward.uri()),
            )
            .expect(1)
            .mount(&allowed)
            .await;

        let http = http_client().unwrap();
        let err = push(
            &http,
            &Target::UnifiedPush {
                endpoint: allowed.uri(),
                keys: None,
            },
            &Message::notice("engram", "A test from Settings.", 0),
            &Sender::load(&test_core().await).await.unwrap(),
        )
        .await
        .expect_err("a redirect is a misconfigured endpoint, not a hop to take");
        assert!(
            err.to_string().contains("302"),
            "and it is reported as the status it is: {err}"
        );
        // Explicit: `expect(0)` above is checked on drop.
        drop(inward);
    }

    async fn run_after_of(core: &Core) -> Option<i64> {
        sqlx::query_scalar!(
            "SELECT run_after FROM jobs WHERE stage = ? AND target_id = ? AND state = 'pending'",
            Stage::Remind.as_str(),
            REMIND_TARGET
        )
        .fetch_optional(&core.store.control.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn restoring_a_superseded_artifact_arms_its_reminder_again() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({
                    "gotify": {"url": format!("{}/message", server.uri()), "token": "tok"},
                }),
            )
            .await
            .unwrap();

        let id = due_at(&core, crate::store::now() + 3_600).await;
        let aid = core.store.moment(&id).await.unwrap().unwrap().artifact_id;

        let out = core
            .ingest_capture(Capture::new("Something else entirely", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let other = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let armed = run_after_of(&core).await;
        assert!(armed.is_some(), "armed to begin with");

        core.store
            .set_superseded_by(&aid, Some(&other))
            .await
            .unwrap();
        core.store.rearm_remind().await.unwrap();
        assert!(
            run_after_of(&core).await.is_none(),
            "nothing owed, disarmed"
        );

        core.unsupersede(&aid).await.unwrap();
        assert_eq!(
            run_after_of(&core).await,
            armed,
            "the restore brought the arming back with it"
        );
    }

    #[tokio::test]
    async fn one_channel_refusing_does_not_push_the_other_one_twice() {
        let good = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&good)
            .await;
        let bad = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&bad)
            .await;
        let mut core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({
                    "gotify": {"url": format!("{}/message", good.uri()), "token": "tok"},
                    "unifiedpush": {"endpoint": format!("{}/up", bad.uri())},
                }),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;

        run(&core).await.unwrap();
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_some(),
            "delivered somewhere"
        );
        run(&core).await.unwrap(); // the retry: `expect(1)` on the good server verifies on drop
    }

    #[tokio::test]
    async fn everything_owed_at_one_wake_is_one_push() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("2 reminders"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let a = due_at(&core, now - 10).await;
        let b = due_at(&core, now - 5).await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        for id in [&a, &b] {
            assert!(
                core.store
                    .moment(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .notified_at
                    .is_some(),
                "both are said"
            );
        }
    }

    #[tokio::test]
    async fn a_wake_with_more_than_the_body_holds_says_how_many_it_left_out() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("and 2 more"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let mut ids = vec![];
        for i in 0..(BODY_LINES + 2) {
            ids.push(due_at(&core, now - 100 + i as i64).await);
        }
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        for id in &ids {
            let m = core.store.moment(id).await.unwrap().unwrap();
            assert!(
                m.notified_at.is_some(),
                "left out of the body, not off the ladder"
            );
        }
    }

    #[tokio::test]
    async fn nothing_delivered_anywhere_is_still_an_error() {
        let bad = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&bad)
            .await;
        let mut core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": format!("{}/up", bad.uri())}}),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;
        assert!(run(&core).await.is_err(), "the queue backs off");
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_none()
        );
    }

    fn a_receiver() -> (p256::SecretKey, [u8; 16], serde_json::Value) {
        use base64::Engine;
        use p256::elliptic_curve::{Generate, sec1::ToSec1Point};
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let secret = p256::SecretKey::generate();
        let auth = [9u8; 16];
        let keys = serde_json::json!({
            "p256dh": b64.encode(secret.public_key().to_sec1_point(false).as_bytes()),
            "auth": b64.encode(auth),
        });
        (secret, auth, keys)
    }

    #[tokio::test]
    async fn a_keyed_target_receives_an_encrypted_due_payload_it_can_decrypt() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let (secret, auth, mut keys) = a_receiver();
        keys["endpoint"] = format!("{}/up", server.uri()).into();
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({ "unifiedpush": keys }),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let [req] = &reqs[..] else {
            panic!("one push: {}", reqs.len())
        };
        assert_eq!(req.headers["content-encoding"], "aes128gcm");
        assert_eq!(req.headers["content-type"], "application/octet-stream");
        assert!(req.headers.contains_key("ttl"));
        let vapid = core.store.control.vapid().await.unwrap();
        let authz = req.headers["authorization"].to_str().unwrap();
        assert!(authz.starts_with("vapid t="), "{authz}");
        assert!(authz.ends_with(&format!(", k={}", vapid.public)), "{authz}");
        assert!(
            !req.body.windows(7).any(|w| w == b"invoice"),
            "the push service reads nothing"
        );
        let plain = crate::jobs::webpush::decrypt(&req.body, &secret, &auth).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(payload["v"], 1);
        assert_eq!(payload["kind"], "due");
        assert_eq!(payload["at"], now);
        assert_eq!(payload["moments"][0]["id"], id);
        assert!(
            payload["moments"][0]["title"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "the row's label travels: {payload}"
        );
        assert_eq!(payload["moments"][0]["at"], now - 10);
        assert_eq!(payload["more"], 0);
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_keyless_legacy_target_still_gets_plaintext() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::body_string_contains("Send the invoice"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        due_at(&core, now - 10).await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": format!("{}/up", server.uri())}}),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(
            !reqs[0].headers.contains_key("content-encoding"),
            "a row registered for plaintext is not handed ciphertext"
        );
        assert!(!reqs[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn a_wake_past_one_record_is_trimmed_and_every_channel_takes_it() {
        let up = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(201))
            .expect(1)
            .mount(&up)
            .await;
        let gotify = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&gotify)
            .await;
        let (_, _, mut keys) = a_receiver();
        keys["endpoint"] = format!("{}/up", up.uri()).into();
        let mut core = test_core().await;
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let long = "x".repeat(crate::jobs::webpush::MAX_PLAINTEXT);
        let rows: Vec<_> = (0..2)
            .map(|i| crate::store::moments::DueRow {
                title: long.clone(),
                ..row(Some(now - 10 - i), None)
            })
            .collect();
        let msg = Message::due(&rows, now);
        assert!(
            msg.payload.to_bytes().len() <= MAX_PLAINTEXT,
            "trimmed to one record, not refused"
        );
        assert!(
            msg.body.contains(&long),
            "the prose channels have no such limit and still say everything"
        );
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({
                    "gotify": {"url": format!("{}/message", gotify.uri()), "token": "t"},
                    "unifiedpush": keys,
                }),
            )
            .await
            .unwrap();
        let targets = notify_targets(
            &core
                .store
                .control
                .notify(&core.store.subject)
                .await
                .unwrap(),
        );
        let sender = Sender::load(&core).await.unwrap();
        deliver(&http_client().unwrap(), &sender, &targets, &msg)
            .await
            .unwrap();
        drop(up);
    }

    #[tokio::test]
    async fn nothing_is_armed_for_a_user_with_no_channel() {
        let core = test_core().await;
        due_at(&core, crate::store::now() + 60).await;
        assert!(run_after_of(&core).await.is_none());
    }

    #[tokio::test]
    async fn the_unit_sleeps_until_the_earliest_owed_moment_and_follows_it() {
        let core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": "http://127.0.0.1:9/x"}}),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        let a = due_at(&core, now + 30 * 86_400).await;
        assert_eq!(
            run_after_of(&core).await,
            Some(now + 30 * 86_400 - LEADS[0])
        );
        due_at(&core, now + 10 * 86_400).await;
        assert_eq!(
            run_after_of(&core).await,
            Some(now + 10 * 86_400 - LEADS[0])
        );
        core.store.mark_done(&a, now).await.unwrap();
        core.store.rearm_remind().await.unwrap();
        assert_eq!(
            run_after_of(&core).await,
            Some(now + 10 * 86_400 - LEADS[0])
        );
    }

    #[tokio::test]
    async fn a_due_moment_is_pushed_once_and_the_unit_rearms_or_stops() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/message"))
            .and(wiremock::matchers::header("X-Gotify-Key", "tok"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"gotify": {"url": format!("{}/message", server.uri()), "token": "tok"}}),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;
        // The re-arm lives in `run_claimed`, after the row is closed.
        let job = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(job.stage, Stage::Remind);
        crate::jobs::run_claimed(&core, job).await.unwrap();
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_some()
        );
        run(&core).await.unwrap(); // nothing owed: no second post — `expect(1)` verifies on drop
        assert!(
            run_after_of(&core).await.is_none(),
            "nothing left to wait for"
        );
    }

    #[tokio::test]
    async fn pushing_the_first_reminder_leaves_the_unit_waiting_for_the_second() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        // `due_at` drains the queue, which would claim a unit armed at a past second; ten days out
        // stands on no rung yet.
        let later = due_at(&core, now + 10 * 86_400).await;
        let owed = due_at(&core, now - 10).await;
        assert_eq!(
            run_after_of(&core).await,
            Some(now - 10),
            "the moment itself, already behind us"
        );

        let job = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(job.stage, Stage::Remind);
        crate::jobs::run_claimed(&core, job).await.unwrap();

        assert!(
            core.store
                .moment(&owed)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_some()
        );
        assert!(
            core.store
                .moment(&later)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_none()
        );
        assert_eq!(
            run_after_of(&core).await,
            Some(now + 10 * 86_400 - LEADS[0]),
            "the unit slept through the second reminder"
        );
    }

    #[tokio::test]
    async fn a_failed_push_leaves_the_moment_owed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let mut core = test_core().await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}),
            )
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;
        assert!(run(&core).await.is_err(), "the queue's backoff handles it");
        assert!(
            core.store
                .moment(&id)
                .await
                .unwrap()
                .unwrap()
                .notified_at
                .is_none()
        );
    }

    #[test]
    fn targets_are_read_from_the_namespaced_json() {
        assert!(notify_targets(&serde_json::json!({})).is_empty());
        assert_eq!(
            notify_targets(&serde_json::json!({"gotify": {"url": "u", "token": ""}})),
            vec![],
            "a token is required"
        );
        assert_eq!(
            notify_targets(&serde_json::json!({"unifiedpush": {"endpoint": "e"}})),
            vec![Target::UnifiedPush {
                endpoint: "e".into(),
                keys: None,
            }]
        );
        assert_eq!(
            notify_targets(
                &serde_json::json!({"unifiedpush": {"endpoint": "e", "p256dh": "p", "auth": "a"}})
            ),
            vec![Target::UnifiedPush {
                endpoint: "e".into(),
                keys: Some(WebPushKeys {
                    p256dh: "p".into(),
                    auth: "a".into(),
                }),
            }]
        );
        assert_eq!(
            notify_targets(&serde_json::json!({"unifiedpush": {"endpoint": "e", "p256dh": "p"}})),
            vec![Target::UnifiedPush {
                endpoint: "e".into(),
                keys: None,
            }],
            "half a keypair is no keypair"
        );
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;

    const LONG_AGO: i64 = 0;

    #[test]
    fn a_far_out_moment_is_owed_its_first_rung_when_it_enters_the_band() {
        let due = 1_000_000;
        assert_eq!(
            owed_lead(due, false, None, LONG_AGO, due - LEADS[0] - 1),
            None,
            "still outside the band"
        );
        assert_eq!(
            owed_lead(due, false, None, LONG_AGO, due - LEADS[0]),
            Some(LEADS[0])
        );
    }

    #[test]
    fn a_rung_already_pushed_is_not_owed_again_but_the_next_one_is() {
        let due = 1_000_000;
        let sent = due - LEADS[0];
        assert_eq!(owed_lead(due, false, Some(sent), LONG_AGO, sent + 1), None);
        assert_eq!(
            owed_lead(due, false, Some(sent), LONG_AGO, due - LEADS[1]),
            Some(LEADS[1])
        );
    }

    #[test]
    fn a_moment_set_inside_the_ladder_owes_one_rung_not_every_passed_one() {
        let due = 1_000_000;
        let now = due - 600; // ten minutes out: every rung above 30m is behind us
        assert_eq!(
            owed_lead(due, false, None, LONG_AGO, now),
            Some(30 * 60),
            "the nearest passed rung, once"
        );
        assert_eq!(owed_lead(due, false, Some(now), LONG_AGO, due), Some(0));
    }

    #[test]
    fn a_reminder_created_inside_the_band_owes_nothing_until_its_own_time() {
        let due = 1_000_000;
        let made = due - 20 * 3_600; // set twenty hours out: inside the 48h rung
        assert_eq!(
            owed_lead(due, false, None, made, made),
            None,
            "nothing is owed at the second it is written"
        );
        assert_eq!(
            owed_lead(due, false, None, made, due - 13 * 3_600),
            None,
            "and not before the first rung it was actually created above"
        );
        assert_eq!(
            owed_lead(due, false, None, made, due - 12 * 3_600),
            Some(LEADS[1]),
            "the twelve-hour rung is inside its life and is owed"
        );
        let soon = due - 600;
        assert_eq!(owed_lead(due, false, None, soon, soon), None);
        assert_eq!(owed_lead(due, false, None, soon, due), Some(0));
        assert_eq!(next_lead_at(due, false, None, soon), Some(due));
    }

    #[test]
    fn nothing_is_owed_once_the_moment_itself_has_been_pushed() {
        let due = 1_000_000;
        assert_eq!(
            owed_lead(due, false, Some(due), LONG_AGO, due + 10_000),
            None
        );
    }

    #[test]
    fn the_next_boundary_is_the_earliest_rung_not_yet_covered() {
        let due = 1_000_000;
        assert_eq!(
            next_lead_at(due, false, None, LONG_AGO),
            Some(due - LEADS[0])
        );
        assert_eq!(
            next_lead_at(due, false, Some(due - LEADS[0]), LONG_AGO),
            Some(due - LEADS[1])
        );
        assert_eq!(next_lead_at(due, false, Some(due), LONG_AGO), None);
    }
}
