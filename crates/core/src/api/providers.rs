//! OIDC identity providers, managed by the instance admin (instance-wide) or
//! an organization's owner (its own). An organization's provider takes its
//! secret as a value (stored encrypted: a server path would let its owner
//! read any file and send it to their provider) and passes the SSRF guard.

use axum::http::StatusCode;
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::error::HttpError;
use crate::db;
use crate::oidc::{SECRET_AAD, guard};

fn missing(field: &str) -> HttpError {
    HttpError::bad_request("invalid_provider", format!("{field} is required"))
}

/// Checks an issuer: a URL; for an organization's provider, `https` and public.
async fn check_issuer(
    state: &AppState,
    issuer: &str,
    organization: Option<i64>,
) -> Result<(), HttpError> {
    let url = openidconnect::url::Url::parse(issuer)
        .map_err(|_| HttpError::bad_request("invalid_provider", "the issuer isn't a URL"))?;
    if organization.is_some() && !state.settings.get().oidc_allow_internal_issuers {
        guard(&url)
            .await
            .map_err(|e| HttpError::bad_request("issuer_refused", e.to_string()))?;
    }
    Ok(())
}

/// The secret columns from a request: `(file, encrypted)`.
fn secret(
    state: &AppState,
    request: &api::ProviderRequest,
    organization: Option<i64>,
) -> Result<(Option<String>, Option<String>), HttpError> {
    match (&request.client_secret, &request.client_secret_file) {
        (Some(_), Some(_)) => Err(HttpError::bad_request(
            "invalid_provider",
            "give the secret or its file, not both",
        )),
        (None, Some(_)) if organization.is_some() => Err(HttpError::bad_request(
            "invalid_provider",
            "an organization's provider takes its secret as a value",
        )),
        (None, Some(file)) => Ok((Some(file.clone()), None)),
        (Some(value), None) => Ok((
            None,
            Some(state.cipher.encrypt(value.as_bytes(), SECRET_AAD)),
        )),
        (None, None) => Ok((None, None)),
    }
}

pub(super) async fn create(
    state: &AppState,
    organization: Option<i64>,
    request: &api::ProviderRequest,
) -> Result<api::ProviderInfo, HttpError> {
    let name = request
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| missing("name"))?;
    let issuer = request.issuer.as_deref().ok_or_else(|| missing("issuer"))?;
    let client_id = request
        .client_id
        .as_deref()
        .ok_or_else(|| missing("client_id"))?;
    check_issuer(state, issuer, organization).await?;
    let (file, encrypted) = secret(state, request, organization)?;
    if file.is_none() && encrypted.is_none() {
        return Err(missing("client_secret"));
    }
    let id = sqlx::query_scalar!(
        "INSERT INTO identity_provider (organization, name, issuer, client_id, client_secret_file, client_secret_encrypted, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING uuid",
        organization,
        name.trim(),
        issuer,
        client_id,
        file,
        encrypted,
        request.enabled.unwrap_or(true)
    )
    .fetch_one(&state.pool)
    .await
    .map_err(taken)?;
    find(state, organization, id).await
}

pub(super) async fn update(
    state: &AppState,
    organization: Option<i64>,
    id: Uuid,
    request: &api::ProviderRequest,
) -> Result<api::ProviderInfo, HttpError> {
    find(state, organization, id).await?;
    if let Some(issuer) = &request.issuer {
        check_issuer(state, issuer, organization).await?;
    }
    let (file, encrypted) = secret(state, request, organization)?;
    let replace = file.is_some() || encrypted.is_some();
    sqlx::query!(
        "UPDATE identity_provider SET name = coalesce($3, name), issuer = coalesce($4, issuer),
             client_id = coalesce($5, client_id), enabled = coalesce($6, enabled),
             client_secret_file = CASE WHEN $7 THEN $8 ELSE client_secret_file END,
             client_secret_encrypted = CASE WHEN $7 THEN $9 ELSE client_secret_encrypted END
         WHERE uuid = $1 AND organization IS NOT DISTINCT FROM $2",
        id,
        organization,
        request.name.as_deref().map(str::trim),
        request.issuer,
        request.client_id,
        request.enabled,
        replace,
        file,
        encrypted
    )
    .execute(&state.pool)
    .await
    .map_err(taken)?;
    find(state, organization, id).await
}

/// Deletes a provider, and the identities linked through it.
pub(super) async fn delete(
    state: &AppState,
    organization: Option<i64>,
    id: Uuid,
) -> Result<StatusCode, HttpError> {
    let deleted = sqlx::query!(
        "DELETE FROM identity_provider WHERE uuid = $1 AND organization IS NOT DISTINCT FROM $2",
        id,
        organization
    )
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(HttpError::not_found("identity provider"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn find(
    state: &AppState,
    organization: Option<i64>,
    id: Uuid,
) -> Result<api::ProviderInfo, HttpError> {
    db::admin::providers(&state.pool, organization)
        .await?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| HttpError::not_found("identity provider"))
}

fn taken(e: sqlx::Error) -> HttpError {
    if e.as_database_error().and_then(|d| d.constraint()) == Some("identity_provider_name") {
        HttpError::conflict("provider_name_taken", "a provider with this name exists")
    } else {
        e.into()
    }
}
