//! Turning an authenticated request into the data it may see; runs after `Identity`.

use crate::auth::Identity;
use crate::error::Error;
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

impl FromRequestParts<AppState> for Tenant {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let id = Identity::from_request_parts(parts, state).await?;
        state
            .tenants
            .get_or_provision(&id.subject, id.email.as_deref())
            .await
    }
}

/// For the 404 page, which renders signed out; only missing credentials become `None`.
impl axum::extract::OptionalFromRequestParts<AppState> for Tenant {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        match <Tenant as FromRequestParts<AppState>>::from_request_parts(parts, state).await {
            Ok(t) => Ok(Some(t)),
            Err(Error::Unauthorized) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
