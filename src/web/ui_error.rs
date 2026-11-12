//! The page half of [`crate::error::Error`]: `/ui` handlers return [`UiResult`], `/api/v1` handlers JSON.
//! `?` converts, so a handler body is unchanged.

use crate::error::Error;
use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub type UiResult<T> = std::result::Result<T, UiError>;

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
    /// Matched on the variant, not parsed back out of the formatted string.
    fn page(&self) -> (String, String) {
        match &self.0 {
            Error::NotFound => (
                "Nothing at this address".into(),
                "This page does not exist.".into(),
            ),
            Error::Validation(what) => ("That request did not make sense".into(), what.clone()),
            Error::Unauthorized => (
                "You are not signed in".into(),
                "Sign in, then try again.".into(),
            ),
            Error::Forbidden => (
                "Not yours to open".into(),
                "This account may not do that.".into(),
            ),
            Error::Inference { role, .. } => (
                "The model endpoint is not answering".into(),
                format!("The {role} role could not be reached; nothing was lost, so try again."),
            ),
            Error::InferenceRejected { role, detail } => (
                "The model endpoint refused that".into(),
                format!("The {role} role answered: {detail}"),
            ),
            Error::InferenceBusy { role, .. } => (
                "The model endpoint is busy".into(),
                format!("The {role} role is rate limited or loading; try again shortly."),
            ),
            Error::Vector(_) => (
                "The vector store is not answering".into(),
                "Check that Qdrant is running at the configured address.".into(),
            ),
            // These carry schema and prompt fragments; `client_message` withholds them and so does the page.
            Error::Store(_) | Error::MalformedLlmOutput(_) | Error::Internal(_) => (
                "Something went wrong here".into(),
                "The detail is in the server log.".into(),
            ),
        }
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        // The same accounting as `Error::into_response`, which this replaces on these routes.
        if status.is_server_error() {
            tracing::error!(error = %self.0, "page failed");
        } else {
            tracing::debug!(error = %self.0, "page rejected");
        }

        // A handler 404 and a router-fallback 404 are the same page.
        if status == StatusCode::NOT_FOUND {
            return crate::web::ui::not_found_page(status);
        }

        let (heading, detail) = self.page();
        match (ErrorTemplate { heading, detail }).render() {
            Ok(html) => (status, Html(html)).into_response(),
            // Rendering the error page failed: plain text, true status.
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
