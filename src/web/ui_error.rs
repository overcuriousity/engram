//! The page half of [`crate::error::Error`].
//!
//! One error type serves two doors. `/api/v1` answers a failure with
//! `{"error": …}` as `application/json`, which is what a client asked for; the
//! `/ui` routes returned the same body, and a browser navigating to
//! `/ui/artifacts/<an id that is not there>` was shown that literal line of
//! JSON — no nav, no session, no way back — while an unrouted `/ui/…` path got
//! the app's own `not_found.html`. Two 404s from one surface, one of them a
//! dead end.
//!
//! The split is made by the route rather than by sniffing the request: a `/ui`
//! handler returns [`UiResult`], an `/api/v1` handler returns
//! [`crate::error::Result`], and each says in its own signature what it
//! serves. `?` converts, so a handler body is unchanged.

use crate::error::Error;
use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub type UiResult<T> = std::result::Result<T, UiError>;

/// An [`Error`] on its way to a person rather than to a program.
#[derive(Debug)]
pub struct UiError(pub Error);

impl From<Error> for UiError {
    fn from(e: Error) -> Self {
        UiError(e)
    }
}

impl From<sqlx::Error> for UiError {
    fn from(e: sqlx::Error) -> Self {
        UiError(e.into())
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    heading: String,
    detail: String,
}

impl ErrorTemplate {
    fn section(&self) -> &'static str {
        ""
    }
}

impl UiError {
    /// What to put on the page, per variant.
    ///
    /// Matched on the variant rather than read out of the formatted string:
    /// the wire message is `"validation: …"`, `"vector store: …"`, and turning
    /// those back into a sentence by looking for the colon would be guessing
    /// at text this module already has the structure of.
    fn page(&self) -> (String, String) {
        match &self.0 {
            Error::NotFound => (
                "Nothing at this address".into(),
                "The page you asked for is not one this base has.".into(),
            ),
            Error::Validation(what) => ("That request did not make sense".into(), what.clone()),
            Error::Unauthorized => (
                "You are not signed in".into(),
                "Sign in and try that again.".into(),
            ),
            Error::Forbidden => (
                "Not yours to open".into(),
                "This account is not allowed to do that.".into(),
            ),
            Error::Inference { role, .. } => (
                "The model endpoint is not answering".into(),
                format!(
                    "The {role} role could not be reached. Nothing was lost — what is \
                     stored is stored, and this can be tried again."
                ),
            ),
            Error::InferenceRejected { role, detail } => (
                "The model endpoint refused that".into(),
                format!("The {role} role answered: {detail}"),
            ),
            Error::InferenceBusy { role, .. } => (
                "The model endpoint is busy".into(),
                format!(
                    "The {role} role asked us to come back — it is rate limited, or still \
                     loading. Nothing was lost, and the next try is likely to land."
                ),
            ),
            Error::Vector(_) => (
                "The vector store is not answering".into(),
                "Search and capture need it; what is already stored is untouched. \
                 Check that Qdrant is running at the address in the config."
                    .into(),
            ),
            // The detail on these carries schema fragments and prompt text —
            // `client_message` already refuses to send it, and a page is no
            // more entitled to it than the API is.
            Error::Store(_) | Error::MalformedLlmOutput(_) | Error::Internal(_) => (
                "Something went wrong here".into(),
                "The detail is in this instance's log.".into(),
            ),
        }
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        // The same accounting `Error::into_response` does, because this
        // replaces it on these routes rather than running after it.
        if status.is_server_error() {
            tracing::error!(error = %self.0, "page failed");
        } else {
            tracing::debug!(error = %self.0, "page rejected");
        }

        // A 404 from a handler and a 404 from the router fallback are the same
        // wrong turn, so they are the same page.
        if status == StatusCode::NOT_FOUND {
            return crate::web::ui::not_found_page(status);
        }

        let (heading, detail) = self.page();
        match (ErrorTemplate { heading, detail }).render() {
            Ok(html) => (status, Html(html)).into_response(),
            // Rendering the error page is the one failure with nowhere left to
            // go. Plain text, and the status is still the truth.
            Err(e) => {
                tracing::error!(error = %e, "the error page itself would not render");
                (status, "engram could not render this error.").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page marks the sentence that says what went wrong, which is what
    /// a failed htmx swap shows in place of the fragment. Unmarked, the
    /// driver had nothing to read out of a page and showed the status code.
    #[tokio::test]
    async fn the_error_page_marks_its_reason_for_a_failed_swap_to_show() {
        let res = UiError(Error::Validation("chunk text is empty".into())).into_response();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("data-error-detail>chunk text is empty</p>"),
            "{html}"
        );
    }
}
