//! The requesting user.
//!
//! TEMPORARY (until authentication, step 06): the user is given by the
//! `X-Theca-User` header (their uuid). Handlers only use [`CurrentUser`], so
//! authentication only replaces this extractor's internals.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

use super::AppState;
use super::error::HttpError;

/// A user who has joined their organization.
#[derive(Debug, Clone, Copy)]
pub struct CurrentUser {
    pub pk: i64,
    pub organization: i64,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = HttpError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, HttpError> {
        let uuid: Uuid = parts
            .headers
            .get("x-theca-user")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(HttpError::unauthenticated)?;
        let (pk, organization) = crate::db::user::joined(&state.pool, uuid)
            .await?
            .ok_or_else(HttpError::unauthenticated)?;
        Ok(Self { pk, organization })
    }
}
