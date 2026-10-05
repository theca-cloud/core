//! OIDC logins: the authorization code flow with PKCE, against a provider's
//! discovery document. The flow's state lives in an encrypted cookie between
//! the start and the callback, not in the database.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, HttpResponse, IssuerUrl,
    Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::identity::Provider;

/// How long a user may take at the provider.
pub const FLOW_LIFETIME: Duration = Duration::from_secs(600);

/// Why an OIDC login failed (logged; the user sees a generic error).
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("cannot read the client secret file {0}: {1}")]
    Secret(String, std::io::Error),
    #[error("invalid URL: {0}")]
    Url(String),
    #[error("discovery failed: {0}")]
    Discovery(String),
    #[error("code exchange failed: {0}")]
    Exchange(String),
    #[error("invalid ID token: {0}")]
    IdToken(String),
}

/// What happens between the start and the callback, kept in an encrypted cookie.
#[derive(Debug, Serialize, Deserialize)]
pub struct Flow {
    pub provider: Uuid,
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    /// Linking this identity to the account that started the flow.
    pub link: Option<Uuid>,
    pub expires: u64,
}

impl Flow {
    pub fn is_expired(&self) -> bool {
        now() > self.expires
    }
}

/// What the provider asserts about the user.
#[derive(Debug)]
pub struct Asserted {
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

/// The provider's authorization URL, and the flow to keep until the callback.
pub async fn start(
    provider: &Provider,
    redirect: &str,
    link: Option<Uuid>,
) -> Result<(String, Flow), OidcError> {
    let client = client(provider, redirect).await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    let flow = Flow {
        provider: provider.uuid,
        state: state.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: verifier.secret().clone(),
        link,
        expires: now() + FLOW_LIFETIME.as_secs(),
    };
    Ok((url.to_string(), flow))
}

/// Exchanges the callback's code and verifies the ID token (signature,
/// issuer, audience, expiry, nonce).
pub async fn finish(
    provider: &Provider,
    redirect: &str,
    flow: Flow,
    code: String,
) -> Result<Asserted, OidcError> {
    let client = client(provider, redirect).await?;
    let response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| OidcError::Exchange(e.to_string()))?
        .set_pkce_verifier(PkceCodeVerifier::new(flow.pkce_verifier))
        .request_async(&http)
        .await
        .map_err(|e| OidcError::Exchange(e.to_string()))?;
    let id_token = response
        .id_token()
        .ok_or_else(|| OidcError::IdToken("no ID token".into()))?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(flow.nonce))
        .map_err(|e| OidcError::IdToken(e.to_string()))?;
    Ok(Asserted {
        subject: claims.subject().to_string(),
        email: claims.email().map(|e| e.to_string()),
        email_verified: claims.email_verified().unwrap_or(false),
    })
}

/// A client for the provider, from its discovery document.
// ponytail: discovery (and the JWKS) fetched on every start and callback; cache
// per provider if logins get frequent.
async fn client(
    provider: &Provider,
    redirect: &str,
) -> Result<
    CoreClient<
        openidconnect::EndpointSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointNotSet,
        openidconnect::EndpointMaybeSet,
        openidconnect::EndpointMaybeSet,
    >,
    OidcError,
> {
    let secret = std::fs::read_to_string(&provider.client_secret_file)
        .map_err(|e| OidcError::Secret(provider.client_secret_file.clone(), e))?;
    let issuer =
        IssuerUrl::new(provider.issuer.clone()).map_err(|e| OidcError::Url(e.to_string()))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, &http)
        .await
        .map_err(|e| OidcError::Discovery(e.to_string()))?;
    let redirect =
        RedirectUrl::new(redirect.to_owned()).map_err(|e| OidcError::Url(e.to_string()))?;
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        Some(ClientSecret::new(secret.trim().to_owned())),
    )
    .set_redirect_uri(redirect))
}

/// The HTTP client for providers: ours (reqwest 0.13, rustls with ring), never
/// following redirects (a provider can't bounce us to another host).
async fn http(request: HttpRequest) -> Result<HttpResponse, reqwest::Error> {
    // ponytail: a client per request (logins are rare); share one in AppState
    // if that shows up.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let (parts, body) = request.into_parts();
    let response = client
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .body(body)
        .send()
        .await?;
    let mut builder = http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let body = response.bytes().await?.to_vec();
    Ok(builder.body(body).expect("a valid response"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
