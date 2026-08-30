//! Turning an authenticated request into the data it is allowed to see.
//!
//! Runs after `Identity`, so an unauthenticated request fails in exactly the
//! place it failed before — with the same 401, which the redirect middleware in
//! `web/mod.rs` still rewrites into a login for a browser.

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

/// A tenant whose user may reach the judge — which is also the only door in the
/// tree that writes `config.toml`.
///
/// Named by every judge handler in place of `Tenant`, so a route added to that
/// router later without it does not compile against the pattern its neighbours
/// use, rather than silently opening.
pub struct CanJudge(pub Tenant);

impl FromRequestParts<AppState> for CanJudge {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let t = Tenant::from_request_parts(parts, state).await?;
        // The column, not the copy of it the registry is holding.
        //
        // `Tenant.user` is the row as it read at open time, and an open tenant
        // outlives a grant: `engram --revoke-judge` is a second process writing
        // the control database, with nothing to tell this one. Left on the
        // snapshot, a revoked user kept the judge — and with it the only route
        // in the tree that writes `config.toml` — until their core happened to
        // fall out of the LRU, which on an instance under its cap is never.
        //
        // One indexed read on the control database, and only on this door. The
        // hot paths keep the snapshot, which is what the cache is for.
        //
        // What this gate covers, and what it does not: the judging deck, which
        // labels anyone's searches out of context and is the only route in the
        // tree that writes `config.toml`. It is deliberately not on the verdict
        // bar or the rail's gap button in `workspace.rs` — those answer the
        // caller's own search, at the moment of it, the way the ask bar does,
        // and `event_is_mine` is what stands there instead. So `--revoke-judge`
        // means "no deck and no tuning", not "cannot label a pair": a revoked
        // user can still say yes, no or gap about a search they just ran, and
        // those rows do reach `feedback_stats`, `--export-eval` and the sweep.
        // Taking that away as well means taking the bar off their own results.
        let live = state
            .tenants
            .control()
            .user(&t.user.subject)
            .await?
            .ok_or(Error::Forbidden)?;
        if !live.can_judge {
            return Err(Error::Forbidden);
        }
        Ok(CanJudge(t))
    }
}

/// `Option<Tenant>` for the one page that renders without a session: the 404,
/// which has to say "not found" to a signed-out browser rather than bounce it
/// to a login it cannot come back from. Only "no credentials" becomes `None`;
/// a provisioning failure is still an error, because showing the signed-out
/// page to someone who is signed in would hide a real fault.
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
