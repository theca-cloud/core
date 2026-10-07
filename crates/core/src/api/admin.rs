//! The instance admin's API (the SaaS app's too): settings, organizations,
//! accounts, storages, identity providers, background jobs.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use theca_common::api;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::jobs::{self, Job};
use crate::settings::{DEFINITIONS, Settings};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/jobs", get(jobs))
        .route("/admin/jobs/{job}/run", post(run_job))
        .route("/admin/settings", get(settings).patch(change_settings))
        .route(
            "/admin/organizations",
            get(organizations).post(create_organization),
        )
        .route(
            "/admin/organizations/{id}",
            patch(update_organization).delete(delete_organization),
        )
        .route("/admin/accounts", get(accounts))
        .route(
            "/admin/accounts/{id}",
            patch(update_account).delete(delete_account),
        )
        .route("/admin/storages", get(storages).post(add_storage))
        .route("/admin/storages/{id}/default", post(default_storage))
        .route(
            "/admin/identity-providers",
            get(providers).post(create_provider),
        )
        .route(
            "/admin/identity-providers/{id}",
            patch(update_provider).delete(delete_provider),
        )
}

/// Instance admins: from a full session, or an account-wide app password
/// (the SaaS app drives the instance this way); changes need one that isn't
/// read-only.
pub(super) async fn require_admin(
    state: &AppState,
    account: &CurrentAccount,
    write: bool,
) -> Result<(), HttpError> {
    let account_wide = account.organization.is_none() && account.library.is_none();
    if !(account.is_full_session()
        || (!account.session && account_wide && !(write && account.read_only)))
    {
        return Err(
            HttpError::forbidden("a full session or an account-wide app password")
                .into_code("admin_required"),
        );
    }
    let admin = sqlx::query_scalar!(
        "SELECT instance_admin FROM account WHERE pk = $1",
        account.account
    )
    .fetch_one(&state.pool)
    .await?;
    if admin {
        Ok(())
    } else {
        Err(HttpError::forbidden("instance admins only").into_code("admin_required"))
    }
}

/// Every background job with its last 10 runs.
#[utoipa::path(get, path = "/api/v1/admin/jobs", responses(
    (status = 200, body = Vec<api::JobStatus>),
    (status = 403, description = "`admin_required`, `full_session_required`", body = api::ApiError),
))]
pub async fn jobs(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::JobStatus>>, HttpError> {
    require_admin(&state, &account, false).await?;
    let mut out = Vec::new();
    for job in Job::ALL {
        out.push(api::JobStatus {
            job: job.name().to_owned(),
            interval_secs: job.interval(&state.settings.get()).as_secs(),
            runs: jobs::runs(&state.pool, job, 10).await?,
        });
    }
    Ok(Json(out))
}

/// Run a job now, in the background. Returns its run (`running`).
#[utoipa::path(post, path = "/api/v1/admin/jobs/{job}/run", params(("job" = String, Path)), responses(
    (status = 202, body = api::JobRun),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`job_running`", body = api::ApiError),
))]
pub async fn run_job(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(job): Path<String>,
) -> Result<(StatusCode, Json<api::JobRun>), HttpError> {
    require_admin(&state, &account, true).await?;
    let job = Job::from_name(&job).ok_or_else(|| HttpError::not_found("job"))?;
    let run = jobs::start(&state.pool, job)
        .await?
        .ok_or_else(|| HttpError::conflict("job_running", "this job is already running"))?;
    let started = jobs::runs(&state.pool, job, 1).await?.remove(0);
    tracing::info!(job = job.name(), account = %account.account_uuid, "job triggered by an admin");
    tokio::spawn(jobs::execute(state, job, run));
    Ok((StatusCode::ACCEPTED, Json(started)))
}

/// Every setting: its stored value (or none: the default), default, description.
#[utoipa::path(get, path = "/api/v1/admin/settings", responses((status = 200, body = Vec<api::SettingInfo>)))]
pub async fn settings(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::SettingInfo>>, HttpError> {
    require_admin(&state, &account, false).await?;
    setting_list(&state).await
}

async fn setting_list(state: &AppState) -> Result<Json<Vec<api::SettingInfo>>, HttpError> {
    let stored = Settings::stored(&state.pool)
        .await
        .map_err(HttpError::internal)?;
    Ok(Json(
        DEFINITIONS
            .iter()
            .map(|d| api::SettingInfo {
                key: d.key.to_owned(),
                value: stored.get(d.key).cloned(),
                default: d.default.to_owned(),
                description: d.description.to_owned(),
            })
            .collect(),
    ))
}

/// Change settings (`null`: back to the default), all or none: the result is
/// validated as a whole, then applied at once (no restart).
#[utoipa::path(patch, path = "/api/v1/admin/settings", request_body = BTreeMap<String, Option<String>>, responses(
    (status = 200, body = Vec<api::SettingInfo>),
    (status = 400, description = "`unknown_setting`, `invalid_setting`", body = api::ApiError),
))]
pub async fn change_settings(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(changes): ApiJson<BTreeMap<String, Option<String>>>,
) -> Result<Json<Vec<api::SettingInfo>>, HttpError> {
    require_admin(&state, &account, true).await?;
    let mut values = Settings::stored(&state.pool)
        .await
        .map_err(HttpError::internal)?;
    for (key, value) in &changes {
        if !DEFINITIONS.iter().any(|d| d.key == key) {
            return Err(HttpError::bad_request(
                "unknown_setting",
                format!("no setting {key:?}"),
            ));
        }
        match value {
            Some(value) => values.insert(key.clone(), value.clone()),
            None => values.remove(key),
        };
    }
    let (parsed, _) = Settings::parse(values)
        .map_err(|e| HttpError::bad_request("invalid_setting", e.to_string()))?;
    let mut tx = state.pool.begin().await?;
    for (key, value) in &changes {
        match value {
            Some(value) => sqlx::query!(
                "INSERT INTO setting (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = $2",
                key,
                value
            )
            .execute(&mut *tx)
            .await?,
            None => sqlx::query!("DELETE FROM setting WHERE key = $1", key).execute(&mut *tx).await?,
        };
    }
    tx.commit().await?;
    state.settings.set(parsed);
    tracing::info!(account = %account.account_uuid, keys = ?changes.keys().collect::<Vec<_>>(), "settings changed");
    setting_list(&state).await
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct Search {
    /// Part of a name or email.
    q: Option<String>,
}

/// Organizations (at most 200), with their owner, members, quota, usage.
#[utoipa::path(get, path = "/api/v1/admin/organizations", params(Search), responses((status = 200, body = Vec<api::AdminOrganization>)))]
pub async fn organizations(
    State(state): State<AppState>,
    account: CurrentAccount,
    Query(search): Query<Search>,
) -> Result<Json<Vec<api::AdminOrganization>>, HttpError> {
    require_admin(&state, &account, false).await?;
    Ok(Json(
        db::admin::organizations(&state.pool, search.q.as_deref()).await?,
    ))
}

/// Create an organization owned by an existing account.
#[utoipa::path(post, path = "/api/v1/admin/organizations", request_body = api::CreateOrganization, responses(
    (status = 201, body = api::AdminOrganization),
    (status = 404, description = "No account with that email", body = api::ApiError),
))]
pub async fn create_organization(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(request): ApiJson<api::CreateOrganization>,
) -> Result<(StatusCode, Json<api::AdminOrganization>), HttpError> {
    require_admin(&state, &account, true).await?;
    let name = super::organizations::name(&request.name)?;
    let (owner, ..) = db::account::by_email(&state.pool, request.owner.trim())
        .await?
        .ok_or_else(|| HttpError::not_found("account"))?;
    let owner_pk = sqlx::query_scalar!("SELECT pk FROM account WHERE uuid = $1", owner)
        .fetch_one(&state.pool)
        .await?;
    let mut tx = state.pool.begin().await?;
    let pk = db::organization::create(&mut tx, owner_pk, name, false).await?;
    tx.commit().await?;
    let uuid = sqlx::query_scalar!("SELECT uuid FROM organization WHERE pk = $1", pk)
        .fetch_one(&state.pool)
        .await?;
    Ok((StatusCode::CREATED, Json(organization(&state, uuid).await?)))
}

async fn organization(state: &AppState, id: Uuid) -> Result<api::AdminOrganization, HttpError> {
    db::admin::organizations(&state.pool, None)
        .await?
        .into_iter()
        .find(|o| o.id == id)
        .ok_or_else(|| HttpError::not_found("organization"))
}

/// Rename, set the quota, suspend or lift, set the default storage.
#[utoipa::path(patch, path = "/api/v1/admin/organizations/{id}", params(("id" = Uuid, Path)), request_body = api::UpdateOrganization, responses(
    (status = 200, body = api::AdminOrganization),
    (status = 404, body = api::ApiError),
))]
pub async fn update_organization(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(update): ApiJson<api::UpdateOrganization>,
) -> Result<Json<api::AdminOrganization>, HttpError> {
    require_admin(&state, &account, true).await?;
    let pk = db::admin::organization_pk(&state.pool, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))?;
    let mut tx = state.pool.begin().await?;
    if !db::admin::update_organization(&mut tx, pk, &update).await? {
        return Err(HttpError::not_found("storage"));
    }
    tx.commit().await?;
    Ok(Json(organization(&state, id).await?))
}

/// Accounts (at most 200).
#[utoipa::path(get, path = "/api/v1/admin/accounts", params(Search), responses((status = 200, body = Vec<api::AdminAccount>)))]
pub async fn accounts(
    State(state): State<AppState>,
    account: CurrentAccount,
    Query(search): Query<Search>,
) -> Result<Json<Vec<api::AdminAccount>>, HttpError> {
    require_admin(&state, &account, false).await?;
    Ok(Json(
        db::admin::accounts(&state.pool, search.q.as_deref()).await?,
    ))
}

/// Make or unmake an instance admin, suspend or lift (never leaving the
/// instance without an active admin).
#[utoipa::path(patch, path = "/api/v1/admin/accounts/{id}", params(("id" = Uuid, Path)), request_body = api::UpdateAccount, responses(
    (status = 200, body = api::AdminAccount),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`last_admin`", body = api::ApiError),
))]
pub async fn update_account(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(update): ApiJson<api::UpdateAccount>,
) -> Result<Json<api::AdminAccount>, HttpError> {
    require_admin(&state, &account, true).await?;
    db::admin::update_account(&state.pool, id, &update)
        .await
        .map_err(|e| match e {
            db::admin::AccountError::NotFound => HttpError::not_found("account"),
            db::admin::AccountError::LastAdmin => {
                HttpError::conflict("last_admin", "the instance needs at least one active admin")
            }
            db::admin::AccountError::Db(e) => e.into(),
        })?;
    let found = db::admin::accounts(&state.pool, None)
        .await?
        .into_iter()
        .find(|a| a.id == id);
    found
        .map(Json)
        .ok_or_else(|| HttpError::not_found("account"))
}

/// The shared storages, with their usage.
#[utoipa::path(get, path = "/api/v1/admin/storages", responses((status = 200, body = Vec<api::StorageInfo>)))]
pub async fn storages(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::StorageInfo>>, HttpError> {
    require_admin(&state, &account, false).await?;
    Ok(Json(db::admin::storages(&state.pool).await?))
}

/// Add a shared storage, tested first (write, read back, delete).
#[utoipa::path(post, path = "/api/v1/admin/storages", request_body = api::NewStorage, responses(
    (status = 201, body = api::StorageInfo),
    (status = 400, description = "`storage_unusable`", body = api::ApiError),
    (status = 409, description = "`storage_name_taken`", body = api::ApiError),
))]
pub async fn add_storage(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(request): ApiJson<api::NewStorage>,
) -> Result<(StatusCode, Json<api::StorageInfo>), HttpError> {
    require_admin(&state, &account, true).await?;
    let (implementation, details) = crate::storage::details(&request.backend);
    crate::storage::probe(implementation, &details)
        .await
        .map_err(|e| HttpError::bad_request("storage_unusable", e))?;
    let id = db::admin::add_storage(&state.pool, request.name.trim(), implementation, &details)
        .await
        .map_err(|e| {
            if e.as_database_error().and_then(|d| d.constraint()) == Some("storage_name") {
                HttpError::conflict("storage_name_taken", "a storage with this name exists")
            } else {
                e.into()
            }
        })?;
    let storage = db::admin::storages(&state.pool)
        .await?
        .into_iter()
        .find(|s| s.id == id);
    Ok((StatusCode::CREATED, Json(storage.expect("just added"))))
}

/// Make a shared storage the instance default (new libraries go there).
#[utoipa::path(post, path = "/api/v1/admin/storages/{id}/default", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn default_storage(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &account, true).await?;
    if db::admin::set_default_storage(&state.pool, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(HttpError::not_found("storage"))
    }
}

/// The instance-wide identity providers.
#[utoipa::path(get, path = "/api/v1/admin/identity-providers", responses((status = 200, body = Vec<api::ProviderInfo>)))]
pub async fn providers(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::ProviderInfo>>, HttpError> {
    require_admin(&state, &account, false).await?;
    Ok(Json(db::admin::providers(&state.pool, None).await?))
}

/// Add an instance-wide identity provider (full sessions for its logins).
#[utoipa::path(post, path = "/api/v1/admin/identity-providers", request_body = api::ProviderRequest, responses(
    (status = 201, body = api::ProviderInfo),
    (status = 400, description = "`invalid_provider`", body = api::ApiError),
))]
pub async fn create_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(request): ApiJson<api::ProviderRequest>,
) -> Result<(StatusCode, Json<api::ProviderInfo>), HttpError> {
    require_admin(&state, &account, true).await?;
    Ok((
        StatusCode::CREATED,
        Json(super::providers::create(&state, None, &request).await?),
    ))
}

/// Change an instance-wide identity provider.
#[utoipa::path(patch, path = "/api/v1/admin/identity-providers/{id}", params(("id" = Uuid, Path)), request_body = api::ProviderRequest, responses(
    (status = 200, body = api::ProviderInfo),
    (status = 404, body = api::ApiError),
))]
pub async fn update_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::ProviderRequest>,
) -> Result<Json<api::ProviderInfo>, HttpError> {
    require_admin(&state, &account, true).await?;
    Ok(Json(
        super::providers::update(&state, None, id, &request).await?,
    ))
}

/// Delete an instance-wide identity provider, and the identities linked through it.
#[utoipa::path(delete, path = "/api/v1/admin/identity-providers/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn delete_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &account, true).await?;
    super::providers::delete(&state, None, id).await
}

/// Delete an organization (personal ones included): its data at once, its
/// stored chunks with the garbage collection, started now.
#[utoipa::path(delete, path = "/api/v1/admin/organizations/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn delete_organization(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &account, true).await?;
    let pk = db::admin::organization_pk(&state.pool, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))?;
    super::organizations::erase(&state, pk).await
}

/// Delete an account (GDPR erasure). Refused while it owns a company
/// organization (transfer it with the owner's API, or delete it first).
#[utoipa::path(delete, path = "/api/v1/admin/accounts/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`owns_organizations`, `last_admin`", body = api::ApiError),
))]
pub async fn delete_account(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &account, true).await?;
    let pk = sqlx::query_scalar!("SELECT pk FROM account WHERE uuid = $1", id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| HttpError::not_found("account"))?;
    super::session::erase_account(&state, pk).await?;
    tracing::info!(account = %id, by = %account.account_uuid, "account deleted by an admin");
    Ok(StatusCode::NO_CONTENT)
}
