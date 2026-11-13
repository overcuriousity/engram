//! Theme and text size, stored per user and rendered on `<html>` before the first paint.

use axum::Form;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::post;

use crate::auth::Identity;
use crate::web::state::AppState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Density {
    #[default]
    Default,
    Compact,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Prefs {
    pub theme: Theme,
    pub density: Density,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
    /// The toggle names where it goes, not where it is.
    pub fn next_label(self) -> &'static str {
        match self.next() {
            Self::System => "Use the system theme",
            Self::Light => "Switch to light theme",
            Self::Dark => "Switch to dark theme",
        }
    }
}

impl Density {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Compact => "compact",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}

tokio::task_local! {
    static CURRENT: Option<Prefs>;
}

/// The signed-in viewer's prefs while a page renders; `None` when signed out.
pub fn current() -> Option<Prefs> {
    CURRENT.try_with(|p| *p).ok().flatten()
}

/// What `<html>` carries: the stored choice, or `data-prefs="local"` for `theme.js` to fill.
pub fn html_attrs() -> askama::filters::Safe<String> {
    let Some(p) = current() else {
        return askama::filters::Safe(" data-prefs=\"local\"".into());
    };
    let mut a = String::from(" data-prefs=\"user\"");
    if p.theme != Theme::System {
        a.push_str(&format!(" data-theme=\"{}\"", p.theme.as_str()));
    }
    if p.density == Density::Compact {
        a.push_str(" data-density=\"compact\"");
    }
    askama::filters::Safe(a)
}

pub fn theme() -> Theme {
    current().unwrap_or_default().theme
}

pub fn density() -> Density {
    current().unwrap_or_default().density
}

pub async fn render_prefs(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let page = req.method() == axum::http::Method::GET
        && ![
            "/assets/", "/api/", "/mcp", "/healthz", "/_oc/", "/sw.js", "/ui/live",
        ]
        .iter()
        .any(|p| path.starts_with(p));
    if !page {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let who = <Identity as axum::extract::FromRequestParts<AppState>>::from_request_parts(
        &mut parts, &st,
    )
    .await
    .ok();
    let prefs = match who {
        Some(w) => Some(
            st.tenants
                .control()
                .prefs(&w.subject)
                .await
                .unwrap_or_default(),
        ),
        None => None,
    };
    CURRENT
        .scope(prefs, next.run(Request::from_parts(parts, body)))
        .await
}

#[derive(serde::Deserialize)]
struct PrefsForm {
    theme: Option<String>,
    density: Option<String>,
}

async fn save(
    State(st): State<AppState>,
    who: Identity,
    Form(f): Form<PrefsForm>,
) -> crate::error::Result<StatusCode> {
    let control = st.tenants.control();
    let mut p = control.prefs(&who.subject).await?;
    if let Some(t) = f.theme.as_deref() {
        p.theme = Theme::parse(t).ok_or_else(|| crate::error::Error::Validation("theme".into()))?;
    }
    if let Some(d) = f.density.as_deref() {
        p.density =
            Density::parse(d).ok_or_else(|| crate::error::Error::Validation("density".into()))?;
    }
    control.set_prefs(&who.subject, p).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/_oc/prefs", post(save))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test_support::{app_session_and_core, get_body};
    use tower::ServiceExt;

    async fn page_with(p: Prefs) -> String {
        let (app, cookie, core) = app_session_and_core().await;
        core.store.control.provision("user-1", None).await.unwrap();
        core.store.control.set_prefs("user-1", p).await.unwrap();
        get_body(&app, &cookie, "/ui/settings").await
    }

    fn html_tag(page: &str) -> &str {
        let i = page.find("<html").unwrap();
        &page[i..i + page[i..].find('>').unwrap()]
    }

    #[tokio::test]
    async fn a_stored_dark_theme_is_rendered_on_the_html_element() {
        let page = page_with(Prefs {
            theme: Theme::Dark,
            density: Density::Default,
        })
        .await;
        let tag = html_tag(&page);
        assert!(tag.contains("data-theme=\"dark\""), "{tag}");
        assert!(tag.contains("data-prefs=\"user\""), "{tag}");
        assert!(!tag.contains("data-density"), "{tag}");
    }

    #[tokio::test]
    async fn compact_density_is_rendered_on_the_html_element() {
        let page = page_with(Prefs {
            theme: Theme::System,
            density: Density::Compact,
        })
        .await;
        let tag = html_tag(&page);
        assert!(tag.contains("data-density=\"compact\""), "{tag}");
        assert!(!tag.contains("data-theme"), "{tag}");
        assert!(
            page.contains("aria-pressed=\"true\""),
            "the toggle says it is on"
        );
    }

    #[tokio::test]
    async fn the_theme_tooltip_names_the_next_state() {
        let page = page_with(Prefs {
            theme: Theme::Light,
            density: Density::Default,
        })
        .await;
        assert!(
            page.contains("data-oc-tip=\"Switch to dark theme\""),
            "{page}"
        );
        assert_eq!(Theme::Dark.next_label(), "Use the system theme");
        assert_eq!(Theme::System.next_label(), "Switch to light theme");
    }

    #[tokio::test]
    async fn signed_out_pages_defer_to_local_storage() {
        let (app, _cookie, _core) = app_session_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::get("/auth/login")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let page = crate::web::test_support::body_of(res).await;
        assert!(html_tag(&page).contains("data-prefs=\"local\""), "{page}");
    }

    #[tokio::test]
    async fn a_choice_is_saved_for_the_signed_in_user() {
        let (app, cookie, core) = app_session_and_core().await;
        core.store.control.provision("user-1", None).await.unwrap();
        let res = app
            .oneshot(crate::web::test_support::form(
                "/_oc/prefs",
                &cookie,
                "theme=dark",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            core.store.control.prefs("user-1").await.unwrap().theme,
            Theme::Dark
        );
    }
}
