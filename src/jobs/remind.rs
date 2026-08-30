//! Push for what is due: the channels a user configured, and the unit that
//! sleeps until the next due moment and posts it.

use crate::core::Core;
use crate::error::Result;

/// The one Remind row per tenant.
pub const REMIND_TARGET: &str = "due";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Gotify { url: String, token: String },
    UnifiedPush { endpoint: String },
}

/// The channels in a user's `notify` JSON. A Gotify entry needs both its url
/// and its token; a UnifiedPush entry is its endpoint.
pub fn notify_targets(notify: &serde_json::Value) -> Vec<Target> {
    let mut out = vec![];
    if let (Some(url), Some(token)) = (notify["gotify"]["url"].as_str(), notify["gotify"]["token"].as_str())
        && !url.is_empty()
        && !token.is_empty()
    {
        out.push(Target::Gotify { url: url.into(), token: token.into() });
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
pub async fn push(http: &reqwest::Client, target: &Target, title: &str, message: &str) -> crate::error::Result<()> {
    let res = match target {
        Target::Gotify { url, token } => {
            http.post(url)
                .header("X-Gotify-Key", token)
                .json(&serde_json::json!({ "title": title, "message": message, "priority": 5 }))
                .send()
                .await
        }
        Target::UnifiedPush { endpoint } => http.post(endpoint).body(format!("{title}\n{message}")).send().await,
    };
    let res = res.map_err(|e| crate::error::Error::Inference { role: "push", detail: e.to_string() })?;
    if !res.status().is_success() {
        return Err(crate::error::Error::Inference { role: "push", detail: format!("HTTP {}", res.status()) });
    }
    Ok(())
}

/// Post what is owed, one message per moment, record it, and sleep until the
/// next. A failed post returns the error so the queue backs off; nothing is
/// marked notified that was not delivered.
pub async fn run(core: &Core) -> Result<()> {
    let targets = notify_targets(&core.store.control.notify(&core.store.subject).await?);
    if targets.is_empty() {
        return Ok(());
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))?;
    let now = core.clock.now();
    for row in core.store.due_unnotified(now).await? {
        let when = crate::web::due::when_words(
            row.moment.at.unwrap_or(now),
            now,
            crate::core::moments::zone(Some(&row.moment.tz)),
        );
        let message = format!("{}\n{}", row.opening, when);
        for t in &targets {
            push(&http, t, &row.title, &message).await?;
        }
        core.store.mark_notified(&[row.moment.id.clone()], now).await?;
    }
    core.store.rearm_remind().await
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
        let out = core.ingest_capture(Capture::new("Send the invoice", "ui")).await.unwrap();
        crate::jobs::test_support::drain(core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
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
            .set_notify(&core.store.subject, &serde_json::json!({"unifiedpush": {"endpoint": "http://127.0.0.1:9/x"}}))
            .await
            .unwrap();
        let now = crate::store::now();
        let a = due_at(&core, now + 3_000).await;
        assert_eq!(run_after_of(&core).await, Some(now + 3_000));
        due_at(&core, now + 1_000).await;
        assert_eq!(run_after_of(&core).await, Some(now + 1_000));
        core.store.mark_done(&a, now).await.unwrap();
        core.store.rearm_remind().await.unwrap();
        assert_eq!(run_after_of(&core).await, Some(now + 1_000));
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
        run(&core).await.unwrap();
        assert!(core.store.moment(&id).await.unwrap().unwrap().notified_at.is_some());
        run(&core).await.unwrap(); // nothing owed: no second post — `expect(1)` verifies on drop
        assert!(run_after_of(&core).await.is_none(), "nothing left to wait for");
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
            .set_notify(&core.store.subject, &serde_json::json!({"unifiedpush": {"endpoint": server.uri()}}))
            .await
            .unwrap();
        let now = crate::store::now();
        core.clock = Clock::Fixed(now);
        let id = due_at(&core, now - 10).await;
        assert!(run(&core).await.is_err(), "the queue's backoff handles it");
        assert!(core.store.moment(&id).await.unwrap().unwrap().notified_at.is_none());
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
            vec![Target::UnifiedPush { endpoint: "e".into() }]
        );
    }
}
