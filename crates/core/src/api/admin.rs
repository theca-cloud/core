//! The instance admin's API: background jobs (more in step 13).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use theca_common::api;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::HttpError;
use crate::jobs::{self, Job};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/jobs", get(jobs))
        .route("/admin/jobs/{job}/run", post(run_job))
}

/// Instance admins, from a full session.
async fn require_admin(state: &AppState, account: &CurrentAccount) -> Result<(), HttpError> {
    account.require_full_session()?;
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
    require_admin(&state, &account).await?;
    let mut out = Vec::new();
    for job in Job::ALL {
        out.push(api::JobStatus {
            job: job.name().to_owned(),
            interval_secs: job.interval().as_secs(),
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
    require_admin(&state, &account).await?;
    let job = Job::from_name(&job).ok_or_else(|| HttpError::not_found("job"))?;
    let run = jobs::start(&state.pool, job)
        .await?
        .ok_or_else(|| HttpError::conflict("job_running", "this job is already running"))?;
    let started = jobs::runs(&state.pool, job, 1).await?.remove(0);
    tracing::info!(job = job.name(), account = %account.account_uuid, "job triggered by an admin");
    tokio::spawn(jobs::execute(state, job, run));
    Ok((StatusCode::ACCEPTED, Json(started)))
}
