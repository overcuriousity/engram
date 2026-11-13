//! The settings page and the writes it makes.
//! `push_url` and `points_inward` live here because a destination is vetted once, at save time; `jobs::remind` relies on it.

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
    /// As the asker announced itself, or `—`; extension tokens share one name, so this tells them apart.
    pub minted_by: String,
    pub revoked: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    /// The token is never rendered; only whether one is stored.
    gotify_url: String,
    gotify_token_set: bool,
    up_endpoint: String,
    /// What registered the endpoint over the API; none for a typed row.
    up_device: Option<String>,
    /// An endpoint with no keys is pushed in plaintext; said, since the field cannot show it.
    up_legacy: bool,
    tokens: Vec<TokenRow>,
    /// `None` when capture is off, which renders nothing.
    feedback: Option<crate::store::feedback::Stats>,
    /// Set exactly when `feedback` is: one switch records both and one purge takes both.
    asks: Option<crate::store::asks::AskStats>,
    /// The email where the provider gave one, else the subject.
    account: String,
    /// Built here so "follow the browser" and the ten sit in one list.
    langs: Vec<LangRow>,
    /// Named beside the automatic row, since "follow this browser" says nothing on its own.
    browser_lang: &'static str,
    /// The capture language instructs the synthesizer; without one it means nothing.
    synthesizes: bool,
    /// The place only feeds the context offer.
    recommends: bool,
}

impl SettingsTemplate {
    fn section(&self) -> &'static str {
        "settings"
    }
}

pub struct LangRow {
    /// A tag, or `""` for automatic.
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "_token_created.html")]
struct TokenCreatedTemplate {
    token: String,
}

async fn token_rows(tenant: &Tenant) -> Result<Vec<TokenRow>> {
    Ok(tenant
        .core
        .store
        .control
        // This user's: `api_tokens` is one table for everybody.
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
            // Extension tokens share one name, so the user agent is what differs.
            minted_by: t.user_agent.clone().unwrap_or_else(|| "—".into()),
            revoked: t.revoked_at.is_some(),
        })
        .collect())
}

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
        up_device: notify["unifiedpush"]["device"]
            .as_str()
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        up_legacy: crate::jobs::remind::notify_targets(&notify)
            .iter()
            .any(|t| {
                matches!(
                    t,
                    crate::jobs::remind::Target::UnifiedPush { keys: None, .. }
                )
            }),
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
        synthesizes: tenant.core.synthesizes(),
        recommends: tenant.core.recommends(),
    })
    .into_response())
}

/// Takes judgements, situations (rows and point centroids) and observations with it; accepted settings stay.
/// Observations do not cascade (`event_id` is a bare column), so they are deleted by name.
pub const PURGE_NAME: &str = "recorded searches";

#[derive(serde::Deserialize)]
pub(crate) struct TypedName {
    #[serde(default)]
    pub name: String,
}

/// A 422 the dialog swaps in place.
pub(crate) fn name_mismatch() -> Response {
    (
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        axum::response::Html(crate::web::notice::inline_danger(
            "That is not the name; type it exactly.",
        )),
    )
        .into_response()
}

async fn purge_feedback_ui(tenant: Tenant, Form(f): Form<TypedName>) -> UiResult<Response> {
    if f.name != PURGE_NAME {
        return Ok(name_mismatch());
    }
    // The index first, while the rows still say which points carry a set.
    let cleared = tenant.core.forget_situations().await;
    let n = tenant.core.store.purge_feedback().await?;
    tracing::info!(
        dropped = n,
        points_cleared = cleared,
        "captured searches, questions and situations deleted by the operator"
    );
    Ok((
        [("hx-redirect", "/ui/settings")],
        Redirect::to("/ui/settings"),
    )
        .into_response())
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

/// Only `http(s)`: `jobs::remind::run` POSTs here from the server on a timer.
pub(crate) fn push_url(field: &str, raw: &str) -> Result<()> {
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

/// Loopback, unspecified and link-local: the test button would otherwise port-scan the server.
/// Private ranges stay allowed (a LAN Gotify is ordinary); a hostname resolving to loopback is not caught.
fn points_inward(host: &url::Host<&str>) -> bool {
    use std::net::Ipv4Addr;
    let v4 = |a: Ipv4Addr| a.is_loopback() || a.is_link_local() || a.is_unspecified();
    match host {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        url::Host::Ipv4(a) => v4(*a),
        // `fe80::/10` written out: `is_unicast_link_local` is unstable. A v4-mapped address is the v4 question again.
        url::Host::Ipv6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            None => a.is_loopback() || a.is_unspecified() || a.segments()[0] & 0xffc0 == 0xfe80,
        },
    }
}

/// A blank token keeps the stored one; a blank url or endpoint switches the channel off.
#[derive(serde::Deserialize)]
struct LangForm {
    #[serde(default)]
    lang: String,
}

/// Changes the next capture only: stored corpora keep their language.
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

#[derive(serde::Serialize)]
pub(crate) struct NotifySetting {
    pub gotify_url: String,
    /// The token is never read back; only whether one is stored.
    pub gotify_token_set: bool,
    pub up_endpoint: String,
    /// Where an app registered the endpoint with keys.
    pub up_device: Option<String>,
    /// A pasted endpoint with no keys: plaintext.
    pub up_legacy: bool,
}

pub(crate) async fn notify_setting(tenant: &Tenant) -> Result<NotifySetting> {
    let notify = tenant
        .core
        .store
        .control
        .notify(&tenant.user.subject)
        .await?;
    Ok(NotifySetting {
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
        up_device: notify["unifiedpush"]["device"]
            .as_str()
            .filter(|d| !d.is_empty())
            .map(str::to_string),
        up_legacy: crate::jobs::remind::notify_targets(&notify)
            .iter()
            .any(|t| {
                matches!(
                    t,
                    crate::jobs::remind::Target::UnifiedPush { keys: None, .. }
                )
            }),
    })
}

/// An empty field switches the channel off; an empty token keeps the stored one; a registered endpoint keeps its keys while the URL is the same.
pub(crate) async fn set_notify(
    tenant: &Tenant,
    gotify_url: &str,
    gotify_token: &str,
    up_endpoint: &str,
) -> Result<()> {
    let control = &tenant.core.store.control;
    let stored = control.notify(&tenant.user.subject).await?;
    let mut notify = serde_json::json!({});
    let url = gotify_url.trim();
    if !url.is_empty() {
        push_url("gotify_url", url)?;
        let token = match gotify_token.trim() {
            "" => stored["gotify"]["token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            t => t.to_string(),
        };
        notify["gotify"] = serde_json::json!({ "url": url, "token": token });
    }
    let endpoint = up_endpoint.trim();
    if !endpoint.is_empty() {
        push_url("up_endpoint", endpoint)?;
        notify["unifiedpush"] = match stored["unifiedpush"]["endpoint"].as_str() == Some(endpoint) {
            true => stored["unifiedpush"].clone(),
            false => serde_json::json!({ "endpoint": endpoint }),
        };
    }
    control.set_notify(&tenant.user.subject, &notify).await?;
    tenant.core.store.rearm_remind().await?;
    Ok(())
}

async fn save_notify(tenant: Tenant, Form(f): Form<NotifyForm>) -> UiResult<Response> {
    set_notify(&tenant, &f.gotify_url, &f.gotify_token, &f.up_endpoint).await?;
    Ok(Redirect::to("/ui/settings").into_response())
}

/// `Ok(Err(why))`: not configured or not accepted, in the page's words.
pub(crate) async fn test_channel(
    tenant: &Tenant,
    channel: &str,
) -> Result<std::result::Result<(), String>> {
    let notify = tenant
        .core
        .store
        .control
        .notify(&tenant.user.subject)
        .await?;
    let target = crate::jobs::remind::notify_targets(&notify)
        .into_iter()
        .find(|t| match t {
            crate::jobs::remind::Target::Gotify { .. } => channel == "gotify",
            crate::jobs::remind::Target::UnifiedPush { .. } => channel == "unifiedpush",
        });
    let Some(target) = target else {
        return Ok(Err("That channel is not configured — save it first.".into()));
    };
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Internal(e.to_string()))?;
    let msg = crate::jobs::remind::Message::notice(
        "engram",
        "A test from Settings.",
        tenant.core.clock.now(),
    );
    let sender = crate::jobs::remind::Sender::load(&tenant.core).await?;
    Ok(
        match crate::jobs::remind::push(&http, &target, &msg, &sender).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, channel, "the test push could not be delivered");
                Err("Not delivered; see the server log.".into())
            }
        },
    )
}

#[derive(serde::Deserialize)]
struct NotifyTestForm {
    channel: String,
}

async fn test_notify(tenant: Tenant, Form(f): Form<NotifyTestForm>) -> UiResult<Response> {
    Ok(
        axum::response::Html(match test_channel(&tenant, &f.channel).await? {
            Ok(()) => "<p class=\"muted\">Sent.</p>".to_string(),
            Err(why) => format!("<p class=\"muted\">{why}</p>"),
        })
        .into_response(),
    )
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

#[derive(Template)]
#[template(path = "_token_row.html")]
struct TokenRowTemplate {
    t: TokenRow,
}

async fn token_row(tenant: &Tenant, id: &str) -> Result<TokenRow> {
    token_rows(tenant)
        .await?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or(crate::error::Error::NotFound)
}

fn render_row(t: TokenRow) -> Result<String> {
    TokenRowTemplate { t }
        .render()
        .map_err(|e| crate::error::Error::Internal(e.to_string()))
}

/// At once, with an undo notice; an unknown id and someone else's answer alike.
async fn revoke_token_ui(tenant: Tenant, Path(tid): Path<String>) -> UiResult<Response> {
    crate::auth::tokens::revoke(&tenant.core.store.control, &tid, &tenant.user.subject).await?;
    let row = token_row(&tenant, &tid).await?;
    let notice = crate::web::notice::undo(
        &format!("Revoked '{}'", row.name),
        &format!("/ui/ops/tokens/{tid}/restore"),
    );
    Ok(axum::response::Html(format!("{}{notice}", render_row(row)?)).into_response())
}

async fn restore_token_ui(tenant: Tenant, Path(tid): Path<String>) -> UiResult<Response> {
    crate::auth::tokens::restore(&tenant.core.store.control, &tid, &tenant.user.subject).await?;
    Ok((
        [("hx-refresh", "true")],
        axum::response::Html(String::new()),
    )
        .into_response())
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/settings", get(settings))
        .route("/ui/settings/lang", post(save_lang))
        .route("/ui/settings/notify", post(save_notify))
        .route("/ui/settings/notify/test", post(test_notify))
        // Under `/ui/ops/...`: these form actions predate this page and may be open in a tab.
        .route("/ui/ops/tokens", post(mint_token))
        .route("/ui/ops/tokens/{id}/revoke", post(revoke_token_ui))
        .route("/ui/ops/tokens/{id}/restore", post(restore_token_ui))
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
            up_device: None,
            up_legacy: false,
            tokens,
            feedback: None,
            asks: None,
            langs: vec![LangRow {
                value: "",
                label: "Automatic — follow this browser",
                selected: true,
            }],
            browser_lang: "English",
            synthesizes: true,
            recommends: true,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn revoking_a_token_can_be_undone() {
        let (app, cookie, core) = app_session_and_core().await;
        let (row, token) = crate::auth::tokens::mint(&core.store.control, "laptop", "user-1", None)
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/tokens/{}/revoke", row.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains("Revoked &#39;laptop&#39; ·"), "{body}");
        assert!(body.contains("revoked"), "{body}");
        assert!(
            crate::auth::tokens::verify(&core.store.control, &token)
                .await
                .is_err()
        );
        let res = app
            .oneshot(form(
                &format!("/ui/ops/tokens/{}/restore", row.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            crate::auth::tokens::verify(&core.store.control, &token)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn purging_needs_the_exact_typed_name() {
        let (app, cookie, _core) = app_session_and_core().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/ops/feedback/purge",
                &cookie,
                "name=recorded+search",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body_of(res).await.contains("type it exactly"));
        let res = app
            .oneshot(form(
                "/ui/ops/feedback/purge",
                &cookie,
                "name=recorded+searches",
            ))
            .await
            .unwrap();
        assert!(res.status().is_redirection(), "{}", res.status());
        assert_eq!(res.headers()["hx-redirect"], "/ui/settings");
    }

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
        // A LAN push server is the ordinary case; private ranges are not refused.
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

    #[tokio::test]
    async fn an_unconfigured_subsystem_shows_no_entry_point() {
        let (app, cookie) =
            app_with_cookie(crate::core::test_support::test_core_without_synthesizer().await).await;
        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(!settings.contains("/ui/settings/lang"), "{settings}");

        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = false;
        let (app, cookie) = app_with_cookie(core).await;
        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(!settings.contains("place-switch"), "{settings}");
        assert!(settings.contains("/ui/settings/lang"), "{settings}");
    }

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
    async fn settings_links_the_page_that_pairs_a_phone() {
        let (app, cookie) = app_with_cookie(crate::core::test_support::test_core().await).await;
        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(
            settings.contains(r#"href="/ui/app""#),
            "the app is paired from here: {settings}"
        );
    }

    #[tokio::test]
    async fn two_tokens_with_one_name_are_still_tellable_apart() {
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
        // The ownership check runs first, and a gone row belongs to nobody: that is the guard pinned here.
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
        assert!(
            html.contains("kept unless you type a new one"),
            "a stored token is shown as stored"
        );
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
    async fn the_page_names_the_device_that_registered_and_marks_a_keyless_row() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {
                    "endpoint": "https://push.example/up/1",
                    "p256dh": "p", "auth": "a",
                    "device": "engram-android/0.1 (Pixel 8)",
                    "registered_at": 1,
                }}),
            )
            .await
            .unwrap();
        let html = get_body(&app, &cookie, "/ui/settings").await;
        assert!(html.contains("engram-android/0.1 (Pixel 8)"), "{html}");
        assert!(html.contains("encrypted"), "{html}");
        assert!(!html.contains("plaintext"), "{html}");

        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {"endpoint": "https://push.example/up/1"}}),
            )
            .await
            .unwrap();
        let html = get_body(&app, &cookie, "/ui/settings").await;
        assert!(html.contains("plaintext"), "{html}");
        assert!(!html.contains("encrypted"), "{html}");
    }

    #[tokio::test]
    async fn saving_the_form_with_the_registered_endpoint_untouched_keeps_its_keys() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_with_cookie(core.clone()).await;
        core.store
            .control
            .set_notify(
                &core.store.subject,
                &serde_json::json!({"unifiedpush": {
                    "endpoint": "https://push.example/up/1",
                    "p256dh": "p", "auth": "a", "device": "d", "registered_at": 1,
                }}),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(form(
                "/ui/settings/notify",
                &cookie,
                "gotify_url=&gotify_token=&up_endpoint=https%3A%2F%2Fpush.example%2Fup%2F1",
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert_eq!(notify["unifiedpush"]["p256dh"], "p");
        assert_eq!(notify["unifiedpush"]["device"], "d");

        app.clone()
            .oneshot(form(
                "/ui/settings/notify",
                &cookie,
                "gotify_url=&gotify_token=&up_endpoint=https%3A%2F%2Fpush.example%2Fup%2F2",
            ))
            .await
            .unwrap();
        let notify = core
            .store
            .control
            .notify(&core.store.subject)
            .await
            .unwrap();
        assert_eq!(
            notify["unifiedpush"]["endpoint"],
            "https://push.example/up/2"
        );
        assert!(
            notify["unifiedpush"].get("p256dh").is_none(),
            "a typed endpoint is a person's, and the keys were for the other one"
        );
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
        let html = settings_fixture(vec![]);
        assert!(!html.contains("Minted by"), "{html}");
        assert!(html.contains("No API tokens yet."), "{html}");
    }
}
