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
    /// The endpoint understood the request and refused it; retrying sends the same answer back.
    #[error("inference[{role}] rejected: {detail}")]
    InferenceRejected { role: &'static str, detail: String },
    /// The endpoint is up but will not do it now (rate limit, model loading); waiting fixes it.
    #[error("inference[{role}] busy: {detail}")]
    InferenceBusy { role: &'static str, detail: String },
    #[error("vector store: {0}")]
    Vector(String),
    #[error("store: {0}")]
    Store(String),
    #[error("malformed llm output: {0}")]
    MalformedLlmOutput(String),
    /// This server broke; apart from `Validation` so a crash gets a 5xx, not a 400.
    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    /// Whether a worker should spend another attempt on this.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Error::Inference { .. }
                | Error::InferenceBusy { .. }
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
            // 503 means "ask again": the client tells a blip from an outage without the body.
            Error::InferenceBusy { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Error::Store(_) | Error::MalformedLlmOutput(_) | Error::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// What the client may see: store and LLM-parse failures carry schema and prompt fragments.
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
    fn every_variant_maps_to_a_status_and_one_sentence() {
        let all = [
            Error::NotFound,
            Error::Validation("q: empty".into()),
            Error::Unauthorized,
            Error::Forbidden,
            Error::Inference {
                role: "embed",
                detail: "timeout".into(),
            },
            Error::InferenceRejected {
                role: "embed",
                detail: "too large".into(),
            },
            Error::InferenceBusy {
                role: "embed",
                detail: "429".into(),
            },
            Error::Vector("down".into()),
            Error::Store("schema".into()),
            Error::MalformedLlmOutput("json".into()),
            Error::Internal("bug".into()),
        ];
        for e in all {
            let status = e.status();
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{e:?}"
            );
            let m = e.client_message();
            assert!(!m.is_empty(), "{e:?}");
            assert!(m.trim_end_matches('.').matches(". ").count() == 0, "{m}");
            if status.is_server_error()
                && !matches!(
                    e,
                    Error::Inference { .. }
                        | Error::InferenceRejected { .. }
                        | Error::InferenceBusy { .. }
                        | Error::Vector(_)
                )
            {
                assert_eq!(m, "internal error", "{e:?} leaks its detail");
            }
        }
    }

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
        // The endpoint said no: another attempt gets the same answer.
        assert!(
            !Error::InferenceRejected {
                role: "vision",
                detail: "HTTP 400: model does not accept images".into()
            }
            .retryable()
        );

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
    fn a_busy_endpoint_is_not_a_bad_gateway() {
        let busy = Error::InferenceBusy {
            role: "embed",
            detail: "HTTP 429 Too Many Requests".into(),
        };
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(busy.retryable(), "waiting is exactly what fixes this one");
        assert!(
            busy.client_message().contains("429"),
            "the caller cannot see what the endpoint said: {}",
            busy.client_message()
        );
    }

    #[test]
    fn internal_errors_do_not_leak_detail_to_clients() {
        let body = Error::Store("no such table: sources".into()).client_message();
        assert!(!body.contains("sources"), "internal detail leaked: {body}");
    }
}
