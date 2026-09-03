//! Push for what is due: the channels a user configured, and the unit that
//! sleeps until the next due moment and posts it.

use crate::core::Core;
use crate::error::Result;

/// The one Remind row per tenant.
pub const REMIND_TARGET: &str = "due";

/// How long before a due moment each push goes out, longest lead first, the
/// last rung being the moment itself.
///
/// The first rung is the hour a moment enters the band the front page draws
/// (`time.horizon_hours`, 48 by default), so what the phone says and what the
/// screen shows appear together. The rest close in, because a reminder two
/// days out is news and a reminder half an hour out is a nudge, and the two
/// want different spacing.
pub const LEADS: &[i64] = &[48 * 3_600, 12 * 3_600, 3 * 3_600, 30 * 60, 0];

/// The ladder a row actually climbs. A snoozed row climbs one rung, the last
/// one — the moment itself, which for a snoozed row is the second the snooze
/// ends.
///
/// Because a snooze re-keys the ladder: `eff_at` becomes `snoozed_until`, and
/// the leads are measured back from it. Snoozing for an hour therefore put
/// every rung above 30m *behind* the new time, so `owed_lead` found one owed
/// at once and the operator got a push for the reminder they had just put
/// aside — while the band, reading `snoozed_until` directly, correctly hid the
/// row. A snooze is a time the operator named; the only thing to say at it is
/// the reminder, once.
fn ladder(snoozed: bool) -> &'static [i64] {
    if snoozed {
        &LEADS[LEADS.len() - 1..]
    } else {
        LEADS
    }
}

/// Rungs a row was created too late to stand on are not rungs it owes.
///
/// The first rung is 48 hours, and most reminders are set inside that: "erinnere
/// mich morgen um 9" is armed at `eff_at - 48h`, which is yesterday, so the
/// ladder found a rung owed the second the row was written and pushed — beside
/// `confirm_created`, which had just pushed to say the reminder was set. Two
/// buzzes seconds apart for every reminder under two days out, which is to say
/// for the common one.
///
/// A rung that fell before the moment existed says nothing about it: nobody was
/// waiting to be told two days ahead about a thing they decided on this
/// morning. The rungs that come *after* it was created are the ladder it
/// actually has, and the last of them is the moment itself, which always
/// remains.
fn reachable(lead: i64, eff_at: i64, created_at: i64) -> bool {
    lead == 0 || eff_at - lead >= created_at
}

/// The rung a moment owes at `now`: the nearest one already reached that the
/// last push did not cover, or `None` when nothing is owed.
///
/// The *nearest* rung and not every passed one, which is what keeps a
/// reminder set ten minutes out from firing four pushes at once: the rungs
/// above it are behind us, one push covers them all, and `notified_at` moving
/// to now retires them.
///
/// `created_at` is the moment's own — see `reachable`.
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

/// The next second at which this moment owes a push: the earliest rung the
/// last push did not cover, at any time, past or future.
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
    Gotify { url: String, token: String },
    UnifiedPush { endpoint: String },
}

/// The channels in a user's `notify` JSON. A Gotify entry needs both its url
/// and its token; a UnifiedPush entry is its endpoint.
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
        out.push(Target::UnifiedPush { endpoint: e.into() });
    }
    out
}

/// One POST per channel, no library. Gotify takes a JSON body and the token
/// in a header; UnifiedPush takes the message as the body.
pub async fn push(
    http: &reqwest::Client,
    target: &Target,
    title: &str,
    message: &str,
) -> crate::error::Result<()> {
    let res = match target {
        Target::Gotify { url, token } => {
            http.post(url)
                .header("X-Gotify-Key", token)
                .json(&serde_json::json!({ "title": title, "message": message, "priority": 5 }))
                .send()
                .await
        }
        Target::UnifiedPush { endpoint } => {
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

/// How many reminders one collapsed push spells out before it starts
/// counting. A body is read on a lock screen, and past a handful of lines
/// nobody reads it — the rest is a number, and the band has them all.
pub const BODY_LINES: usize = 8;

/// The one message a wake sends: what the notification is titled, and what it
/// says.
///
/// One row keeps the shape it has always had — its own title, its opening,
/// when it is due. Several rows collapse into one message, because a rung of
/// the ladder is a buzz in a pocket and twenty of them at once is twenty
/// reasons to turn the channel off.
pub fn compose(rows: &[crate::store::moments::DueRow], now: i64) -> (String, String) {
    if let [row] = rows {
        let when = crate::web::due::when_words(
            row.moment.at.unwrap_or(now),
            now,
            crate::core::moments::zone(Some(&row.moment.tz)),
        );
        return (row.title.clone(), format!("{}\n{}", row.opening, when));
    }
    let mut body: Vec<String> = rows
        .iter()
        .take(BODY_LINES)
        .map(|row| {
            let when = crate::web::due::due_words(
                row.moment.at.unwrap_or(now),
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

/// One title and message, to every configured channel. `Ok` once any channel
/// has taken it; the error is returned only when every channel refused.
/// Shared by the due-time ladder and by an immediate, ad-hoc confirmation
/// that is not on the ladder at all — neither reads or writes `notified_at`,
/// so the two can never step on each other's delivery state.
async fn deliver(
    http: &reqwest::Client,
    targets: &[Target],
    title: &str,
    message: &str,
) -> Result<()> {
    let mut delivered = false;
    let mut failure = None;
    for t in targets {
        match push(http, t, title, message).await {
            Ok(()) => delivered = true,
            Err(e) => {
                tracing::warn!(error = %e, "a push channel refused");
                failure = Some(e);
            }
        }
    }
    if !delivered {
        return Err(failure.expect("targets is non-empty, so a wake with no delivery has an error"));
    }
    Ok(())
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))
}

/// A push right now, to whatever channels the user has configured, entirely
/// outside the due-time ladder — no row is read or marked. For a confirmation
/// that something just happened rather than that something is owed: silent
/// when nobody has configured a channel, same as the ladder itself.
pub async fn notify_now(core: &Core, title: &str, message: &str) -> Result<()> {
    let targets = notify_targets(&core.store.control.notify(&core.store.subject).await?);
    if targets.is_empty() {
        return Ok(());
    }
    deliver(&http_client()?, &targets, title, message).await
}

/// Post what this wake owes as one message, record every moment it covered,
/// and let the queue re-arm for the next rung.
///
/// The moments are marked once any channel has taken the message, and the
/// error is returned only when every channel refused. Retrying a wake that
/// reached one of two channels would deliver it twice there, and a duplicate
/// push is worse than a missing one on the flakier channel — the rows are
/// still on the band either way.
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
    let (title, message) = compose(&owed, now);
    deliver(&http_client()?, &targets, &title, &message).await?;
    let ids: Vec<String> = owed.iter().map(|r| r.moment.id.clone()).collect();
    // The message is already out, so from here the mark is the fragile step
    // and `?` is the wrong thing to do with it. `Error::Store` is retryable —
    // `SQLITE_BUSY` is treated as routine everywhere else — and a retry
    // re-reads the same `due_owed` set and delivers the whole batch a second
    // time. So the mark is attempted until it sticks; the realistic cause is a
    // writer holding the file for a few milliseconds.
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
        // Still not returned. A job that fails here is a job that retries, and
        // its retry says the same thing to the same people about the same
        // rows. The rows stay owed and the next wake will say it again, which
        // is the same duplicate arriving later and with a record of why.
        tracing::error!(error = %e, n = ids.len(), "a push went out that could not be recorded");
    }
    // The re-arm is NOT here. `arm_at` only moves a row in `pending`, `done`
    // or `failed`, and while this runs the row is `running` — so an arming
    // from inside the run is a no-op, `complete_job` then closes the row with
    // `run_after` at the instant that just passed, and the unit never wakes
    // for the next rung. `jobs::run_claimed` re-arms after the row is closed,
    // the same order `embed::rearm_if_more` follows and for the same reason.
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

    /// The pending Remind row's wake time, or none when nothing is armed.
    async fn run_after_of(core: &Core) -> Option<i64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT run_after FROM jobs WHERE stage = ? AND target_id = ? AND state = 'pending'",
        )
        .bind(Stage::Remind.as_str())
        .bind(REMIND_TARGET)
        .fetch_optional(&core.store.control.pool)
        .await
        .unwrap()
    }

    /// Restoring an artifact from Ops has to re-arm the unit that its
    /// disappearance disarmed.
    ///
    /// `uncovered` filters `a.status = 'active'`, so an artifact dedupe hides
    /// takes its open reminder out of the unit's sight; where it was the only
    /// one owed, the unit disarms itself. `unsupersede` cleared the row and
    /// re-embedded the artifact and armed nothing, so that reminder was never
    /// pushed at any rung again. Every other moment-moving write already called
    /// `rearm_remind`; this path was the hole.
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

        // Dedupe hides it in favour of another artifact, and the unit has
        // nothing left to owe.
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
        // The retry would re-post to the channel that already took it. A row
        // delivered somewhere is a row that has been delivered.
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
        // Twenty reminders due this afternoon is twenty rows on the band and
        // one buzz in the pocket. A rung is a message, not a moment.
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
        // Far enough out that the moments are still climbing the ladder: what
        // the unit waits for is the first rung of the nearer one.
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
        // Through the queue, as a worker runs it: the re-arm lives in
        // `run_claimed`, after the row is closed, because `arm_at` refuses a
        // row that is still `running`.
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

    /// The second reminder, and the seam it fell through.
    ///
    /// The re-arm used to be the last line of `run`, where the unit's own row
    /// is still `running` — and `arm_at` only moves a row that is `pending`,
    /// `done` or `failed`. So the upsert did nothing, `complete_job` closed
    /// the row with `run_after` at the instant that had just passed, and the
    /// unit never woke again: two moments due at different times meant one
    /// push and then silence, until some unrelated write happened to re-arm
    /// it. Driven through `run_claimed` on purpose — calling `run` directly is
    /// what hid this, because no running row exists there.
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
        // The later one first: `due_at` drains the queue, and a unit already
        // armed at a past second would be claimed by that drain. Ten days out,
        // so it is not standing on a rung of its own yet.
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
                endpoint: "e".into()
            }]
        );
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;

    /// A moment that has existed longer than the ladder is deep: every rung is
    /// one it could have stood on. The `created_at` most of these tests want.
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
        // And then the moment itself, which is the only rung still ahead.
        assert_eq!(owed_lead(due, false, Some(now), LONG_AGO, due), Some(0));
    }

    /// The rungs a reminder was created too late to stand on.
    ///
    /// "erinnere mich morgen um 9", set this afternoon, is inside the 48-hour
    /// first rung before it exists. The ladder read that rung as passed and
    /// unpushed and owed it at once — while `confirm_created` was pushing
    /// "reminder set" for the same act. Two buzzes seconds apart, for every
    /// reminder under two days out.
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
        // A reminder set ten minutes out clears no rung but the last, and that
        // one is what it is for.
        let soon = due - 600;
        assert_eq!(owed_lead(due, false, None, soon, soon), None);
        assert_eq!(owed_lead(due, false, None, soon, due), Some(0));
        // And the unit sleeps until that, rather than waking to owe nothing.
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
