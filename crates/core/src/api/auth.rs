//! Authentication of API requests.
//!
//! - [`authenticate`] (middleware): looks up the request's opaque token (the
//!   session cookie, or `Bearer` for the CLI and API clients) on every request:
//!   revocation takes effect at once. It also enforces CSRF protection for
//!   cookie-authenticated requests.
//! - [`CurrentAccount`] (extractor): the authenticated credential. Each request
//!   acts as the account's membership in the organization of the resource it
//!   touches, within the credential's scope (`library_access` in SQL).

use std::net::IpAddr;
use std::ops::Deref;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::error::HttpError;
use crate::auth::OpaqueToken;
use crate::db;
use crate::db::account::Credential;

pub const SESSION_COOKIE: &str = "theca_session";

/// The authenticated credential (a session or an app password).
#[derive(Debug, Clone, Copy)]
pub struct CurrentAccount(pub Credential);

impl Deref for CurrentAccount {
    type Target = Credential;

    fn deref(&self) -> &Credential {
        &self.0
    }
}

impl CurrentAccount {
    /// Account-level actions (password, identities, other sessions,
    /// invitations, deleting the account) need a full session: not an app
    /// password, and not a session restricted to an organization (otherwise an
    /// organization's SSO could escape its restriction).
    pub fn require_full_session(&self) -> Result<(), HttpError> {
        if self.is_full_session() {
            Ok(())
        } else {
            Err(HttpError::forbidden("this needs a full session")
                .into_code("full_session_required"))
        }
    }
}

impl FromRequestParts<AppState> for CurrentAccount {
    type Rejection = HttpError;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, HttpError> {
        parts
            .extensions
            .get::<Credential>()
            .copied()
            .map(Self)
            .ok_or_else(HttpError::unauthenticated)
    }
}

/// See the module documentation.
pub async fn authenticate(
    State(state): State<AppState>,
    ip: ClientIp,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let bearer = bearer(headers);
    let via_cookie = bearer.is_none();
    let Some(token) = bearer.or_else(|| cookie(headers, SESSION_COOKIE)) else {
        return next.run(request).await;
    };
    let credential = match OpaqueToken::parse(token) {
        Some((prefix, hash)) => match db::account::authenticate(
            &state.pool,
            prefix,
            &hash,
            state.settings.session_lifetime(),
        )
        .await
        {
            Ok(found) => found,
            Err(e) => return HttpError::from(e).into_response(),
        },
        None => None,
    };
    let Some(credential) = credential else {
        let prefix = token.split_once('.').map_or("", |(p, _)| p);
        tracing::warn!(anomaly = "invalid_token", prefix, ip = ?ip.0,
            "a revoked, expired or unknown token");
        return next.run(request).await;
    };

    // CSRF: a cookie-authenticated POST must be JSON (or carry our header).
    // Another site can only send a form (GET/POST, form content types); any
    // other method or content type needs a CORS preflight, which we never
    // allow. Presigned blob URLs need no cookies and carry raw bytes.
    if via_cookie
        && request.method() == Method::POST
        && !request.uri().path().starts_with("/api/v1/blobs/")
    {
        let json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        if !json && !request.headers().contains_key("x-theca-csrf") {
            return HttpError::forbidden(
                "cookie-authenticated requests must be JSON (or send X-Theca-CSRF)",
            )
            .into_code("csrf")
            .into_response();
        }
    }

    request.extensions_mut().insert(credential);
    next.run(request).await
}

/// `Set-Cookie` for a session. The server decides when a session expires (it
/// slides with `last_seen`), so the cookie lives as long as browsers allow
/// (400 days) instead of copying an expiry it couldn't keep up to date.
pub fn set_session_cookie(headers: &mut HeaderMap, token: &str) {
    const MAX_AGE: u64 = 400 * 24 * 3600;
    set_cookie(
        headers,
        &format!("{SESSION_COOKIE}={token}; Max-Age={MAX_AGE}"),
    );
}

/// `Set-Cookie` clearing the session cookie (logout).
pub fn clear_session_cookie(headers: &mut HeaderMap) {
    set_cookie(headers, &format!("{SESSION_COOKIE}=; Max-Age=0"));
}

/// Appends a `Set-Cookie` (`name=value; attributes`) with our flags.
pub(super) fn set_cookie(headers: &mut HeaderMap, cookie: &str) {
    let cookie = format!("{cookie}; Path=/; HttpOnly; Secure; SameSite=Lax");
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("cookie values are ASCII"),
    );
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// The client's IP address, when known (logged with anomalies on requests that
/// have no user, like presigned URLs). Never fails.
// ponytail: the direct peer; behind a reverse proxy that's the proxy. Trusting
// X-Forwarded-For needs a configured list of trusted proxies.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

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

/// The client's `User-Agent`, recorded with sessions.
pub struct UserAgent(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for UserAgent {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        ))
    }
}
