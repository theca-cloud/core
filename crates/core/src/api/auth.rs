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
    /// For logs (anomalies): the only user ID ever exposed.
    pub uuid: Uuid,
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
        Ok(Self {
            pk,
            organization,
            uuid,
        })
    }
}

/// The client's IP address, when known (logged with anomalies on requests that
/// have no user, like presigned URLs). Never fails.
// ponytail: the direct peer; behind a reverse proxy that's the proxy. Trusting
// X-Forwarded-For needs a configured list of trusted proxies.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<std::net::IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0.ip()),
        ))
    }
}
