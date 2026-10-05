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
use crate::keys::DataCipher;

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
    cipher: &DataCipher,
    allow_internal: bool,
    provider: &Provider,
    redirect: &str,
    link: Option<Uuid>,
) -> Result<(String, Flow), OidcError> {
    let client = client(cipher, allow_internal, provider, redirect).await?;
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
    cipher: &DataCipher,
    allow_internal: bool,
    provider: &Provider,
    redirect: &str,
    flow: Flow,
    code: String,
) -> Result<Asserted, OidcError> {
    let client = client(cipher, allow_internal, provider, redirect).await?;
    let guarded = provider.organization.is_some() && !allow_internal;
    let http = move |request| http(request, guarded);
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
    cipher: &DataCipher,
    allow_internal: bool,
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
    let secret = secret(cipher, provider)?;
    let guarded = provider.organization.is_some() && !allow_internal;
    let http = move |request| http(request, guarded);
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

/// The AAD of a provider's encrypted client secret.
pub const SECRET_AAD: &[u8] = b"theca identity provider secret";

/// A provider's client secret: read from its file (instance-wide), or
/// decrypted (an organization's).
fn secret(cipher: &DataCipher, provider: &Provider) -> Result<String, OidcError> {
    match (
        &provider.client_secret_file,
        &provider.client_secret_encrypted,
    ) {
        (Some(path), _) => std::fs::read_to_string(path)
            .map(|s| s.trim().to_owned())
            .map_err(|e| OidcError::Secret(path.clone(), e)),
        (None, Some(sealed)) => cipher
            .decrypt(sealed, SECRET_AAD)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| {
                OidcError::Secret(
                    "<encrypted>".into(),
                    std::io::Error::other("doesn't decrypt"),
                )
            }),
        (None, None) => Err(OidcError::Secret(
            "<none>".into(),
            std::io::Error::other("no secret"),
        )),
    }
}

/// Why an address may not be fetched for an organization's provider.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("only https issuers")]
    NotHttps,
    #[error("{0} resolves to a non-public address ({1})")]
    Private(String, std::net::IpAddr),
    #[error("{0}: {1}")]
    Resolve(String, String),
}

/// SSRF guard for organization-configured providers: `https` only, and every
/// address the host resolves to must be public. Returns the address to
/// connect to, so the connection goes where the check looked (no DNS
/// rebinding in between).
pub async fn guard(
    url: &openidconnect::url::Url,
) -> Result<(String, std::net::SocketAddr), GuardError> {
    if url.scheme() != "https" {
        return Err(GuardError::NotHttps);
    }
    let host = url.host_str().ok_or(GuardError::NotHttps)?.to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| GuardError::Resolve(host.clone(), e.to_string()))?
        .collect();
    if let Some(bad) = addresses.iter().find(|a| !is_public(a.ip())) {
        return Err(GuardError::Private(host, bad.ip()));
    }
    let first = *addresses
        .first()
        .ok_or_else(|| GuardError::Resolve(host.clone(), "no address".into()))?;
    Ok((host, first))
}

/// A globally routable address (no loopback, private, link-local, CGNAT,
/// documentation, benchmarking, multicast, or cloud metadata ranges).
pub fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b)) // CGNAT
                || (a == 198 && (18..20).contains(&b)) // benchmarking
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link-local
                || first == 0x2001 && v6.segments()[1] == 0x0db8) // documentation
        }
    }
}

/// The HTTP client for providers: ours (reqwest 0.13, rustls with ring), never
/// following redirects (a provider can't bounce us to another host). For an
/// organization's provider (`guarded`), every request passes the SSRF guard
/// and connects to the address it checked.
async fn http(request: HttpRequest, guarded: bool) -> Result<HttpResponse, GuardedError> {
    // ponytail: a client per request (logins are rare); share one in AppState
    // if that shows up.
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10));
    if guarded {
        let url = openidconnect::url::Url::parse(&request.uri().to_string())
            .map_err(|_| GuardedError::Guard(GuardError::NotHttps))?;
        let (host, address) = guard(&url).await?;
        builder = builder.resolve(&host, address);
    }
    let client = builder.build()?;
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

/// An HTTP failure, or the SSRF guard refusing.
#[derive(Debug, thiserror::Error)]
pub enum GuardedError {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("refused: {0}")]
    Guard(#[from] GuardError),
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::is_public;

    #[test]
    fn only_public_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }
}
