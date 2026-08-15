use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("validation: {0}")]
    Validation(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("inference[{role}]: {detail}")]
    Inference { role: &'static str, detail: String },
    /// The endpoint understood the request and refused it — a model that does
    /// not take images, a body over its limit, a name it does not serve. Kept
    /// apart from `Inference` because the two ask opposite things of a worker:
    /// one is a wait, the other is the same answer for as long as the same
    /// request is sent.
    #[error("inference[{role}] rejected: {detail}")]
    InferenceRejected { role: &'static str, detail: String },
    #[error("vector store: {0}")]
    Vector(String),
    #[error("store: {0}")]
    Store(String),
    #[error("malformed llm output: {0}")]
    MalformedLlmOutput(String),
    /// This server broke, and no amount of fixing the request will help. Kept
    /// apart from `Validation` so that a panic in a parser fed a hostile page
    /// is not reported to the caller as "your input was malformed": the two
    /// need different status codes, and a 400 hides a crash from every log
    /// that watches for 5xx.
    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    /// Whether a worker should spend another attempt on this. Classified once,
    /// here, rather than by string-matching at the call site later.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Error::Inference { .. }
                | Error::Vector(_)
                | Error::Store(_)
                | Error::MalformedLlmOutput(_)
        )
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Validation(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden => StatusCode::FORBIDDEN,
            Error::Inference { .. } | Error::InferenceRejected { .. } | Error::Vector(_) => {
                StatusCode::BAD_GATEWAY
            }
            Error::Store(_) | Error::MalformedLlmOutput(_) | Error::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// What the client is allowed to see. Store and LLM-parse failures carry
    /// schema and prompt fragments, so they are replaced with a generic string
    /// and the detail goes to the log instead.
    pub fn client_message(&self) -> String {
        match self {
            Error::Store(_) => "internal error".into(),
            Error::MalformedLlmOutput(_) => "internal error".into(),
            Error::Internal(_) => "internal error".into(),
            other => other.to_string(),
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Error::NotFound,
            other => Error::Store(other.to_string()),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if self.status().is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }
        (
            self.status(),
            Json(json!({ "error": self.client_message() })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn retryable_only_for_transient_failures() {
        assert!(
            Error::Inference {
                role: "embed",
                detail: "timeout".into()
            }
            .retryable()
        );
        assert!(Error::Vector("connection refused".into()).retryable());
        assert!(Error::Store("database is locked".into()).retryable());
        assert!(Error::MalformedLlmOutput("expected `{`".into()).retryable());
        // The endpoint answered and said no: another attempt sends the same
        // request and gets the same answer.
        assert!(
            !Error::InferenceRejected {
                role: "vision",
                detail: "HTTP 400: model does not accept images".into()
            }
            .retryable()
        );

        // Retrying these burns inference calls and never succeeds.
        assert!(!Error::Validation("empty text".into()).retryable());
        assert!(!Error::NotFound.retryable());
        assert!(!Error::Unauthorized.retryable());
        assert!(!Error::Forbidden.retryable());
    }

    #[test]
    fn maps_to_expected_status_codes() {
        assert_eq!(Error::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            Error::Validation("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(Error::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Error::Forbidden.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            Error::Inference {
                role: "chunk",
                detail: "x".into()
            }
            .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(Error::Vector("x".into()).status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            Error::Store("x".into()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn internal_errors_do_not_leak_detail_to_clients() {
        let body = Error::Store("no such table: sources".into()).client_message();
        assert!(!body.contains("sources"), "internal detail leaked: {body}");
    }
}
