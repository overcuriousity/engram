//! The settings page, and everything an operator changes from it.
//!
//! Split out of `web::ui`, which had grown into the module every page's
//! leftovers landed in. This is one screen and the writes it makes: the
//! account it names, the capture language, where reminders are pushed, the
//! API tokens, and the one button that forgets the search log.
//!
//! `push_url` and `points_inward` are here rather than beside the notifier
//! that spends them, because a destination is vetted once, at save time —
//! `jobs::remind` says so in its own comments and relies on it.

use crate::error::{Error, Result};
use crate::fmt::fmt_time;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub created: String,
    pub last_used: String,
    /// What asked for the token, as it announced itself, or `—` for one minted
    /// before this was recorded. The extension names every token it mints the
    /// same thing, so this is what tells two of those rows apart.
    pub minted_by: String,
    pub revoked: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    /// Where due reminders are pushed. The token is never rendered; only
    /// whether one is stored.
    gotify_url: String,
    gotify_token_set: bool,
    up_endpoint: String,
    tokens: Vec<TokenRow>,
    /// `None` when capture is switched off, which renders nothing at all: a
    /// section about a log nobody is keeping is noise.
    feedback: Option<crate::store::feedback::Stats>,
    /// The questions, counted beside the searches. Set exactly when `feedback`
    /// is: one switch records both, one purge takes both, and a page that named
    /// only the searches let an operator clear their query log without knowing
    /// the judged questions went with it.
    asks: Option<crate::store::asks::AskStats>,
    /// Who is looking at this base.
    ///
    /// Every user has held their own database and their own collection since
    /// #52, and no page said so. The email where the identity provider gave
    /// one, because that is the name a person recognises as theirs; the
    /// subject otherwise, because a stable identifier beats no answer.
    account: String,
    /// Every language a capture can be read in, with the one currently chosen
    /// marked. Built here rather than iterated in the template so the "follow
    /// the browser" row and the ten sit in one list in one order.
    langs: Vec<LangRow>,
    /// What the browser would choose, named beside the automatic row: "follow
    /// this browser" says nothing about what that would mean today.
    browser_lang: &'static str,
}

impl SettingsTemplate {
    /// Which entry in the top row and the tab bar is the one you are inside.
    ///
    /// Read by `layout.html` to set `aria-current="page"`. The empty string is
    /// "none of them", which is a real answer for a page that hangs off no
    /// section.
    fn section(&self) -> &'static str {
        "settings"
    }
}

/// One row of the language control.
pub struct LangRow {
    /// The stored value: a tag, or `""` for automatic.
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "_token_created.html")]
struct TokenCreatedTemplate {
    token: String,
}

/// The API tokens, formatted for a table.
async fn token_rows(tenant: &Tenant) -> Result<Vec<TokenRow>> {
    Ok(tenant
        .core
        .store
        .control
        // This user's, not the instance's: `api_tokens` is one table for
        // everybody now.
        .list_tokens(&tenant.user.subject)
        .await?
        .into_iter()
        .map(|t| TokenRow {
            id: t.id,
            name: t.name,
            created: fmt_time(t.created_at),
            last_used: t
                .last_used_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".into()),
            // What asked for it. Two tokens can carry one name — the extension
            // gives every token it mints the same one — and when neither has
            // been used yet, this is the only thing that differs.
            minted_by: t.user_agent.clone().unwrap_or_else(|| "—".into()),
            revoked: t.revoked_at.is_some(),
        })
        .collect())
}

/// What is true about this installation, as opposed to what is in it.
///
/// Split off Housekeeping, which had grown to hold six tables about the corpus
/// plus the extension, the tokens and the feedback purge — so revoking a token
/// meant scrolling past every merge and every hidden artifact first. Reached
/// from the same quiet line under Capture, and no more advertised than
/// Housekeeping is: neither belongs in a top row that is three destinations
/// wide on purpose.
async fn settings(tenant: Tenant, headers: axum::http::HeaderMap) -> UiResult<Response> {
    let chosen = tenant.core.store.control.lang(&tenant.user.subject).await?;
    let browser = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map(crate::infer::lang::Lang::from_accept_language)
        .unwrap_or_default();
    let mut langs = vec![LangRow {
        value: "",
        label: "Automatic — follow this browser",
        selected: chosen.is_none(),
    }];
    langs.extend(crate::infer::lang::Lang::ALL.iter().map(|l| LangRow {
        value: l.tag(),
        label: l.endonym(),
        selected: chosen == Some(*l),
    }));
    let notify = tenant
        .core
        .store
        .control
        .notify(&tenant.user.subject)
        .await?;
    Ok(HtmlTemplate(SettingsTemplate {
        gotify_url: notify["gotify"]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        gotify_token_set: notify["gotify"]["token"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        up_endpoint: notify["unifiedpush"]["endpoint"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        tokens: token_rows(&tenant).await?,
        feedback: match tenant.core.learn.enabled {
            true => Some(
                tenant
                    .core
                    .store
                    .feedback_stats(tenant.core.weak_below())
                    .await?,
            ),
            false => None,
        },
        asks: match tenant.core.learn.enabled {
            true => Some(tenant.core.store.ask_stats().await?),
            false => None,
        },
        account: tenant
            .user
            .email
            .clone()
            .unwrap_or_else(|| tenant.user.subject.clone()),
        langs,
        browser_lang: browser.endonym(),
    })
    .into_response())
}

/// Forget every captured search, every recorded question, and every situation.
///
/// Judgements go with them: a verdict is a statement about a query, and one
/// whose query no longer exists records nothing. The situations a page view
/// was made in go too, in both places they live — the rows in SQLite and the
/// centroids on the points — because a profile is the situations that formed
/// it, and a button that says "forget" may not leave the average behind. Accepted settings and their
/// history stay, because they describe how the application is configured now.
///
/// Both tables, because one switch records both and `expire_feedback` ages both
/// under one window — but the questions are the harder loss, being the only
/// source `--export-eval` has for `questions.json`, so the button and its
/// confirmation name them rather than leaving them to the word "searches".
async fn purge_feedback_ui(tenant: Tenant) -> UiResult<Response> {
    // The index first, while the rows still say which points carry a set.
    let cleared = tenant.core.forget_situations().await;
    let n = tenant.core.store.purge_feedback().await?;
    tracing::info!(
        dropped = n,
        points_cleared = cleared,
        "captured searches, questions and situations deleted by the operator"
    );
    // Back to the page the button is on. The route keeps its /ui/ops prefix —
    // the two pages split, the endpoints did not.
    Ok(Redirect::to("/ui/settings").into_response())
}

#[derive(serde::Deserialize)]
struct MintForm {
    name: String,
}

#[derive(serde::Deserialize)]
struct NotifyForm {
    #[serde(default)]
    gotify_url: String,
    #[serde(default)]
    gotify_token: String,
    #[serde(default)]
    up_endpoint: String,
}

/// A push destination, checked before it is stored.
///
/// Both fields were taken on `trim()` alone, while the URL capture path
/// (`api.rs`) has always refused anything that is not `http(s)` in as many
/// words. That mattered more here, not less: nothing fetches a captured URL on
/// a schedule, and `jobs::remind::run` POSTs to whatever is saved here on a
/// timer, from the server, for as long as it stands.
fn push_url(field: &str, raw: &str) -> Result<()> {
    let u = url::Url::parse(raw).map_err(|e| Error::Validation(format!("{field}: {e}")))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(Error::Validation(format!(
            "{field}: `{}` is not a scheme a push is sent over",
            u.scheme()
        )));
    }
    let Some(host) = u.host() else {
        return Err(Error::Validation(format!(
            "{field}: that URL names no host"
        )));
    };
    if points_inward(&host) {
        return Err(Error::Validation(format!(
            "{field}: a push goes out to a service, and that address is the server's own machine. \
             A push server on the network — `http://192.168.1.5:8080`, a hostname, a public URL — \
             is what this field is for."
        )));
    }
    Ok(())
}

/// Does this host name the server itself, or the link-local range?
///
/// The reason the field is validated at all. Whoever fills this form is telling
/// the server to make an HTTP request from *its* network position, and it then
/// makes that request twice over: once immediately, for the "Test Gotify"
/// button, and on a timer for as long as the setting stands. Pointed at
/// `http://127.0.0.1:9200/`, the button's two-way "Sent." / "Could not send"
/// answer says whether something is listening on that port of the server's own
/// loopback — a port scan of a machine the person at the form may have no other
/// access to, one entry at a time — and the timer turns an unauthenticated
/// internal endpoint into a POST it will keep receiving.
///
/// Loopback, unspecified and link-local only. The private ranges are
/// deliberately left alone: engram is self-hosted, a Gotify at `192.168.1.5` is
/// an ordinary setup, and refusing it would break the common case to narrow an
/// attack that the loopback rule has already taken the sharp edge off. A
/// hostname that *resolves* to loopback still gets through — closing that means
/// resolving at save time and again at send time, which is a different piece of
/// work.
fn points_inward(host: &url::Host<&str>) -> bool {
    use std::net::Ipv4Addr;
    let v4 = |a: Ipv4Addr| a.is_loopback() || a.is_link_local() || a.is_unspecified();
    match host {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        url::Host::Ipv4(a) => v4(*a),
        // `fe80::/10` written out, because `is_unicast_link_local` is unstable.
        // A v4-mapped address is the v4 question again and not a second one.
        url::Host::Ipv6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            None => a.is_loopback() || a.is_unspecified() || a.segments()[0] & 0xffc0 == 0xfe80,
        },
    }
}

/// Save the channels. A blank token keeps the stored one while the url stays;
/// a blank url or endpoint switches that channel off. Saving re-arms the
/// Remind unit, because a channel just configured is what makes it worth arming.
#[derive(serde::Deserialize)]
struct LangForm {
    #[serde(default)]
    lang: String,
}

/// Choose the language captures are read in, or clear it back to automatic.
///
/// It changes nothing already stored: a corpus carries the language it was
/// captured in, and re-reading old documents under a new setting would rewrite
/// artifacts nobody asked to have rewritten. What it changes is the next
/// capture.
async fn save_lang(tenant: Tenant, Form(f): Form<LangForm>) -> UiResult<Response> {
    let chosen = match f.lang.trim() {
        "" => None,
        tag => Some(crate::infer::lang::Lang::parse(tag).ok_or_else(|| {
            crate::error::Error::Validation(format!("lang: `{tag}` is not one of the ten"))
        })?),
    };
    tenant
        .core
        .store
        .control
        .set_lang(&tenant.user.subject, chosen)
        .await?;
    Ok(Redirect::to("/ui/settings").into_response())
}

async fn save_notify(tenant: Tenant, Form(f): Form<NotifyForm>) -> UiResult<Response> {
    let control = &tenant.core.store.control;
    let stored = control.notify(&tenant.user.subject).await?;
    let mut notify = serde_json::json!({});
    let url = f.gotify_url.trim();
    if !url.is_empty() {
        push_url("gotify_url", url)?;
        let token = match f.gotify_token.trim() {
            "" => stored["gotify"]["token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            t => t.to_string(),
        };
        notify["gotify"] = serde_json::json!({ "url": url, "token": token });
    }
    let endpoint = f.up_endpoint.trim();
    if !endpoint.is_empty() {
        push_url("up_endpoint", endpoint)?;
        notify["unifiedpush"] = serde_json::json!({ "endpoint": endpoint });
    }
    control.set_notify(&tenant.user.subject, &notify).await?;
    tenant.core.store.rearm_remind().await?;
    Ok(Redirect::to("/ui/settings").into_response())
}

#[derive(serde::Deserialize)]
struct NotifyTestForm {
    channel: String,
}

/// One test message down the named channel, answered as a fragment.
async fn test_notify(tenant: Tenant, Form(f): Form<NotifyTestForm>) -> UiResult<Response> {
    let notify = tenant
        .core
        .store
        .control
        .notify(&tenant.user.subject)
        .await?;
    let target = crate::jobs::remind::notify_targets(&notify)
        .into_iter()
        .find(|t| match t {
            crate::jobs::remind::Target::Gotify { .. } => f.channel == "gotify",
            crate::jobs::remind::Target::UnifiedPush { .. } => f.channel == "unifiedpush",
        });
    let Some(target) = target else {
        return Ok(axum::response::Html(
            "<p class=\"muted\">That channel is not configured — save it first.</p>",
        )
        .into_response());
    };
    // `Policy::none()`, for the reason `jobs::remind::http_client` gives at
    // length: this button's answer is two-valued and server-side, so a
    // followed redirect turns it into a loopback port oracle.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(match crate::jobs::remind::push(&http, &target, "engram", "A test from Settings.").await {
        Ok(()) => axum::response::Html("<p class=\"muted\">Sent.</p>".to_string()),
        // The transport detail goes to the server log, never the page: this
        // is a server-side POST to whatever URL the user saved, and in a
        // multi-tenant registry the difference between "connection refused",
        // a timeout and an HTTP status is a port-scan of the server's own
        // network, read back through the button.
        Err(e) => {
            tracing::warn!(error = %e, channel = %f.channel, "the test push could not be delivered");
            axum::response::Html(
                "<p class=\"muted\">Could not send — the endpoint did not take it. \
                 The server log has the transport detail.</p>"
                    .to_string(),
            )
        }
    }
    .into_response())
}

async fn mint_token(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Form(f): Form<MintForm>,
) -> UiResult<Response> {
    let name = if f.name.trim().is_empty() {
        "unnamed"
    } else {
        f.name.trim()
    };
    let (_, plaintext) = crate::auth::tokens::mint(
        &tenant.core.store.control,
        name,
        &tenant.user.subject,
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
    )
    .await?;
    // Shown once, here, and never stored in plaintext anywhere.
    Ok(HtmlTemplate(TokenCreatedTemplate { token: plaintext }).into_response())
}

async fn revoke_token_ui(tenant: Tenant, Path(tid): Path<String>) -> UiResult<Response> {
    // Scoped to the caller. An id-only revoke is a button that kills anyone
    // else's extension pairing, and an unknown id and someone else's id have to
    // answer alike or the 404 becomes an oracle.
    crate::auth::tokens::revoke(&tenant.core.store.control, &tid, &tenant.user.subject).await?;
    Ok(Redirect::to("/ui/settings").into_response())
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/settings", get(settings))
        .route("/ui/settings/lang", post(save_lang))
        .route("/ui/settings/notify", post(save_notify))
        .route("/ui/settings/notify/test", post(test_notify))
        // `/ui/ops/...` rather than `/ui/settings/...`: the tokens and the
        // purge were on Housekeeping before they were on this page, and a
        // form action is a URL somebody may have open in a tab.
        .route("/ui/ops/tokens", post(mint_token))
        .route("/ui/ops/tokens/{id}/revoke", post(revoke_token_ui))
        .route("/ui/ops/feedback/purge", post(purge_feedback_ui))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{
        app_session_and_core, app_with_cookie, app_with_session, body_of, form, get_body,
        searched_app,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn settings_fixture(tokens: Vec<TokenRow>) -> String {
        askama::Template::render(&SettingsTemplate {
            account: crate::store::TEST_SUBJECT.into(),
            gotify_url: String::new(),
            gotify_token_set: false,
            up_endpoint: String::new(),
            tokens,
            feedback: None,
            asks: None,
            langs: vec![LangRow {
                value: "",
                label: "Automatic — follow this browser",
                selected: true,
            }],
            browser_lang: "English",
        })
        .unwrap()
    }

    /// The server POSTs to whatever is stored here — once for the test button,
    /// and on a timer thereafter — from its own network position. The two-way
    /// answer over loopback is a port scan of the machine one entry at a time.
    #[test]
    fn a_push_destination_may_not_be_the_server_talking_to_itself() {
        for inward in [
            "http://127.0.0.1:9200/message",
            "http://127.5.6.7/x",
            "https://localhost/message",
            "http://LocalHost.:8080/",
            "http://sub.localhost/x",
            "http://[::1]:9200/",
            "http://[::ffff:127.0.0.1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://0.0.0.0:8080/",
        ] {
            assert!(
                push_url("gotify_url", inward).is_err(),
                "{inward} points at the server itself"
            );
        }
        // A self-hosted push server on the network is the ordinary case and
        // stays allowed — the private ranges are deliberately not refused.
        for outward in [
            "http://192.168.1.5:8080/message",
            "http://10.0.0.9/message",
            "http://gotify.lan:8080/message",
            "https://push.example.com/message",
        ] {
            assert!(push_url("gotify_url", outward).is_ok(), "{outward}");
        }
        assert!(push_url("gotify_url", "ftp://example.com/x").is_err());
        assert!(push_url("gotify_url", "not a url").is_err());
    }

    /// After #52 every user has their own base, and nothing anywhere said
    /// which account was looking at one. Settings is where account things
    /// live; it was also the one page a phone could not reach.
    #[tokio::test]
    async fn settings_is_reachable_and_names_the_account() {
        let (app, cookie) = app_with_cookie(crate::core::test_support::test_core().await).await;

        let html = get_body(&app, &cookie, "/ui").await;
        assert_eq!(
            html.matches(r#"href="/ui/settings""#).count(),
            2,
            "the top row and the tabbar, so a phone can reach it too: {html}"
        );

        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(
            settings.contains("Signed in as"),
            "and the page that holds account things says which account"
        );
        assert!(
            settings.contains(crate::store::TEST_SUBJECT),
            "named, not merely alluded to"
        );
    }

    #[tokio::test]
    async fn two_tokens_with_one_name_are_still_tellable_apart() {
        // The extension mints every token under the same name, so two rows
        // called "browser extension" and neither used yet were the same row
        // twice — and one of them was the one currently working.
        let (app, cookie, core) = app_session_and_core().await;
        crate::auth::tokens::mint(
            &core.store.control,
            "browser extension",
            "user-1",
            Some("Firefox/141.0"),
        )
        .await
        .unwrap();
        crate::auth::tokens::mint(
            &core.store.control,
            "browser extension",
            "user-1",
            Some("Chrome/152.0"),
        )
        .await
        .unwrap();

        let page = get_body(&app, &cookie, "/ui/settings").await;
        assert!(page.contains("Firefox"), "{page}");
        assert!(page.contains("Chrome"), "{page}");
    }

    #[tokio::test]
    async fn minting_a_token_shows_the_plaintext_exactly_once() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .clone()
            .oneshot(form("/ui/ops/tokens", &cookie, "name=claude-code"))
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("engram_"),
            "the token must be shown once: {html}"
        );

        // It is not recoverable from any later page. Settings, not Housekeeping:
        // that is the page the token table moved to, and asserting against a
        // page that renders no tokens at all asserts nothing.
        let page = body_of(
            app.oneshot(
                Request::builder()
                    .uri("/ui/settings")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(
            page.contains("claude-code"),
            "the minted token's row must be on the page this asserts against: {page}"
        );
        assert!(
            !page.contains("engram_"),
            "a stored token leaked into the settings page"
        );
    }

    #[tokio::test]
    async fn a_purged_search_is_not_the_bar_owner_s_any_more() {
        // Where the bar's writing guards do *not* come into it: the ownership
        // check runs first and a row that is gone belongs to nobody, so all
        // four answers stop there. Worth pinning, because the store guards
        // below it read as the thing standing between a stale tab and a purged
        // event, and they are not — this is.
        let (app, cookie, handle, a, event) = searched_app().await;
        handle.store.purge_feedback().await.unwrap();

        for body in [
            format!("verdict=hit&artifact_id={a}"),
            format!("verdict=no&artifact_id={a}"),
            format!("verdict=skip&artifact_id={a}"),
            format!("verdict=none&artifact_id={a}"),
        ] {
            let res = app
                .clone()
                .oneshot(form(&format!("/ui/search/{event}/verdict"), &cookie, &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{body}");
        }
    }

    #[tokio::test]
    async fn notification_channels_are_saved_masked_and_a_blank_token_keeps_the_stored_one() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/settings/notify",
                &cookie,
                "gotify_url=https%3A%2F%2Fg%2Fmessage&gotify_token=abc&up_endpoint=",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let html = get_body(&app, &cookie, "/ui/settings").await;
        assert!(html.contains("https://g/message"));
        assert!(html.contains("••••"), "a stored token is shown as stored");
        assert!(!html.contains("abc"), "and never rendered");
        app.clone()
            .oneshot(form(
                "/ui/settings/notify",
                &cookie,
                "gotify_url=https%3A%2F%2Fg%2Fmessage&gotify_token=&up_endpoint=",
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert_eq!(notify["gotify"]["token"], "abc");
        app.clone()
            .oneshot(form(
                "/ui/settings/notify",
                &cookie,
                "gotify_url=&gotify_token=&up_endpoint=https%3A%2F%2Fu%2Fx",
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert!(
            notify.get("gotify").is_none(),
            "a blank url switches the channel off"
        );
        assert_eq!(notify["unifiedpush"]["endpoint"], "https://u/x");
    }

    #[tokio::test]
    async fn a_test_on_an_unconfigured_channel_says_so() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core).await;
        let res = app
            .oneshot(form("/ui/settings/notify/test", &cookie, "channel=gotify"))
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(html.contains("not configured"), "{html}");
    }

    #[test]
    fn a_token_table_with_no_tokens_says_so_instead_of_showing_its_headings() {
        // Five column headings over nothing is a table pretending to have
        // rows — the same thing `_decide.html` names at its top: the old Ops
        // page answered five headings with "None." and made an empty base look
        // like a backlog.
        let html = settings_fixture(vec![]);
        assert!(!html.contains("Minted by"), "{html}");
        assert!(html.contains("No tokens yet"), "{html}");
    }
}
