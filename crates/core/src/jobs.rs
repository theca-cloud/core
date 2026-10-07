//! Background jobs: one runner for every job, looping "wait, run", each run
//! recorded in `job_run`. See docs/decisions.md, Background jobs.
//!
//! - The wait comes from the last finished run (interrupted runs don't count),
//!   so restarts neither starve a job nor delay the resumption of a run a
//!   crash cut off.
//! - A run is its own task (a panic kills only it) under a timeout. Every job
//!   is safe to interrupt anywhere: the next run resumes.
//! - No locks: one server process per database.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use sqlx::PgPool;
use tokio::sync::watch;
use uuid::Uuid;

use crate::api::AppState;
use crate::settings::Settings;
use crate::storage::{BackendError, chunk_of};

/// The jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Deletes chunks no version references (objects, then rows).
    ChunkGc,
    /// Deletes trashed nodes and libraries past the retention.
    TrashPurge,
    /// Deletes revoked and expired tokens, and old job history.
    Cleanup,
    /// Deletes objects without a chunk row.
    OrphanCheck,
    /// Sends the queued emails.
    EmailSend,
}

const HOUR: Duration = Duration::from_secs(3600);

/// Rows (chunks, nodes, keys) handled per batch.
const BATCH: i64 = 1000;

impl Job {
    pub const ALL: [Job; 5] = [
        Job::ChunkGc,
        Job::TrashPurge,
        Job::Cleanup,
        Job::OrphanCheck,
        Job::EmailSend,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Job::ChunkGc => "chunk_gc",
            Job::TrashPurge => "trash_purge",
            Job::Cleanup => "cleanup",
            Job::OrphanCheck => "orphan_check",
            Job::EmailSend => "email_send",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|j| j.name() == name)
    }

    /// Its interval (settings `job_<name>_interval_secs`).
    pub fn interval(self, settings: &Settings) -> Duration {
        settings
            .job_intervals
            .get(self.name())
            .copied()
            .unwrap_or(HOUR)
    }

    fn timeout(self) -> Duration {
        match self {
            Job::ChunkGc | Job::TrashPurge | Job::Cleanup => HOUR,
            Job::OrphanCheck => 6 * HOUR,
            Job::EmailSend => Duration::from_secs(600),
        }
    }

    /// Runs the job once. Returns how many items it handled.
    pub async fn run(self, state: &AppState) -> Result<i64, JobError> {
        match self {
            Job::ChunkGc => chunk_gc(state).await,
            Job::TrashPurge => trash_purge(state).await,
            Job::Cleanup => cleanup(state).await,
            Job::OrphanCheck => orphan_check(state).await,
            Job::EmailSend => crate::email::send_due(state).await,
        }
    }
}

/// Why a run failed (recorded in `job_run.error`).
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Backend(#[from] BackendError),
    #[error("{0}")]
    Other(String),
}

/// Marks the runs a crash left `running` as interrupted (at startup).
pub async fn mark_interrupted(pool: &PgPool) -> sqlx::Result<u64> {
    let done = sqlx::query!(
        "UPDATE job_run SET status = 'interrupted', finished_at = now() WHERE status = 'running'"
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Starts every job's loop. They stop when `shutdown` becomes true.
pub fn spawn_all(state: AppState, shutdown: watch::Receiver<bool>) -> tokio::task::JoinSet<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for job in Job::ALL {
        tasks.spawn(run_forever(state.clone(), job, shutdown.clone()));
    }
    tasks
}

async fn run_forever(state: AppState, job: Job, mut shutdown: watch::Receiver<bool>) {
    let mut settings = state.settings.subscribe();
    loop {
        let interval = job.interval(&state.settings.get());
        let wait = match next_wait(&state.pool, job, interval).await {
            Ok(wait) => wait,
            Err(e) => {
                tracing::warn!(job = job.name(), error = %e, "cannot read the job history; retrying");
                Duration::from_secs(60)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {}
            // The interval may have changed: compute the wait again.
            _ = settings.changed() => continue,
        }
        if *shutdown.borrow() {
            return;
        }
        match start(&state.pool, job).await {
            Ok(Some(run)) => execute(state.clone(), job, run).await,
            Ok(None) => {} // already running (triggered by an admin)
            Err(e) => tracing::warn!(job = job.name(), error = %e, "cannot start the job"),
        }
        // Wait at least a second between runs, even if the history can't say.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// How long until the job is due: an interval after the last finished run
/// (success or failure; interrupted runs don't count), at once if none.
pub async fn next_wait(pool: &PgPool, job: Job, interval: Duration) -> sqlx::Result<Duration> {
    let secs = sqlx::query_scalar!(
        r#"SELECT greatest(0, extract(epoch FROM max(started_at) + make_interval(secs => $2) - now()))::float8
           AS "secs"
           FROM job_run WHERE job = $1 AND status IN ('success', 'failure')"#,
        job.name(),
        interval.as_secs_f64()
    )
    .fetch_one(pool)
    .await?;
    Ok(Duration::from_secs_f64(secs.unwrap_or(0.0)))
}

/// A run being recorded: `(pk, uuid)`.
pub type Run = (i64, Uuid);

/// Runs a job now, in the background (unless it's running already).
pub async fn trigger(state: &AppState, job: Job) {
    match start(&state.pool, job).await {
        Ok(Some(run)) => {
            tokio::spawn(execute(state.clone(), job, run));
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(job = job.name(), error = %e, "cannot start the job"),
    }
}

/// Records a new run, unless the job is already running.
pub async fn start(pool: &PgPool, job: Job) -> sqlx::Result<Option<Run>> {
    let row = sqlx::query!(
        "INSERT INTO job_run (job) SELECT $1
         WHERE NOT EXISTS (SELECT 1 FROM job_run WHERE job = $1 AND status = 'running')
         RETURNING pk, uuid",
        job.name()
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.uuid)))
}

/// Runs a started job in its own task, under its timeout, and records the outcome.
pub async fn execute(state: AppState, job: Job, run: Run) {
    let pool = state.pool.clone();
    supervise(&pool, job.name(), run, job.timeout(), async move {
        job.run(&state).await
    })
    .await;
}

/// Runs `work` as its own task (a panic kills only it) under `timeout`, and
/// records the outcome in the run's row.
pub async fn supervise<F>(pool: &PgPool, job: &str, (pk, _): Run, timeout: Duration, work: F)
where
    F: Future<Output = Result<i64, JobError>> + Send + 'static,
{
    let started = std::time::Instant::now();
    let task = tokio::spawn(tokio::time::timeout(timeout, work));
    let (items, error) = match task.await {
        Ok(Ok(Ok(items))) => (items, None),
        Ok(Ok(Err(e))) => (0, Some(e.to_string())),
        Ok(Err(_)) => (0, Some(format!("timed out after {timeout:?}"))),
        Err(e) if e.is_panic() => (
            0,
            Some(format!("panicked: {}", panic_message(e.into_panic()))),
        ),
        Err(e) => (0, Some(format!("cancelled: {e}"))),
    };
    let status = if error.is_none() {
        "success"
    } else {
        "failure"
    };
    metrics::counter!("theca_job_runs_total", "job" => job.to_owned(), "status" => status)
        .increment(1);
    metrics::histogram!("theca_job_duration_seconds", "job" => job.to_owned())
        .record(started.elapsed().as_secs_f64());
    match &error {
        None => tracing::info!(job, items, "job done"),
        Some(error) => tracing::error!(job, error, "job failed"),
    }
    let items = i32::try_from(items).unwrap_or(i32::MAX);
    if let Err(e) = sqlx::query!(
        "UPDATE job_run SET finished_at = now(), items = $2,
             status = CASE WHEN $3::text IS NULL THEN 'success' ELSE 'failure' END, error = $3
         WHERE pk = $1",
        pk,
        items,
        error
    )
    .execute(pool)
    .await
    {
        tracing::error!(job, error = %e, "cannot record the job's outcome");
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Chunks no version references, untouched for the grace period, whose upload
/// URL has expired, plus tombstoned ones (a cancel, or an interrupted run).
/// Three steps (docs/decisions.md, Garbage collection): mark `deleting_at`,
/// delete the object, delete the row.
///
/// Marking flips the generated `ready`, which `chunk_in_version`'s composite FK
/// references: the database refuses to mark a chunk a commit just referenced,
/// and refuses a commit referencing a marked chunk. A concurrent commit either
/// keeps its chunk or gets `chunk_missing`; it never points to a deleted object.
async fn chunk_gc(state: &AppState) -> Result<i64, JobError> {
    let grace = state.settings.get().gc_grace_period.as_secs_f64();
    let mut deleted = 0;
    let mut skipped: HashSet<i64> = HashSet::new();
    loop {
        let skip: Vec<i64> = skipped.iter().copied().collect();
        let candidates: Vec<i64> = sqlx::query_scalar!(
            "SELECT pk FROM chunk c
             WHERE (c.url_expires_at IS NULL OR c.url_expires_at < now())
               AND (c.deleting_at IS NOT NULL
                    OR (c.touched_at < now() - make_interval(secs => $1)
                        AND NOT EXISTS (SELECT 1 FROM chunk_in_version WHERE chunk = c.pk)))
               AND NOT (c.pk = ANY($2))
             ORDER BY c.pk LIMIT $3",
            grace,
            &skip,
            BATCH
        )
        .fetch_all(&state.pool)
        .await?;
        if candidates.is_empty() {
            return Ok(deleted);
        }
        // 1. Mark, in one statement; if a commit referenced one meanwhile, the
        // FK refuses the whole batch: mark one by one, skipping those.
        if let Err(e) = mark(&state.pool, &candidates).await {
            if !is_constraint(&e, "chunk_in_version_chunk") {
                return Err(e.into());
            }
            for &chunk in &candidates {
                if let Err(e) = mark(&state.pool, &[chunk]).await
                    && !is_constraint(&e, "chunk_in_version_chunk")
                {
                    return Err(e.into());
                }
            }
        }
        let marked = sqlx::query!(
            "SELECT pk, uuid, storage FROM chunk WHERE pk = ANY($1) AND deleting_at IS NOT NULL",
            &candidates
        )
        .fetch_all(&state.pool)
        .await?;
        skipped.extend(
            candidates
                .iter()
                .filter(|c| !marked.iter().any(|m| m.pk == **c)),
        );
        for chunk in marked {
            // 2. The object (idempotent), 3. the row.
            state
                .backends
                .get(chunk.storage)
                .await?
                .delete(chunk.uuid)
                .await?;
            deleted += sqlx::query!(
                "DELETE FROM chunk WHERE pk = $1 AND deleting_at IS NOT NULL",
                chunk.pk
            )
            .execute(&state.pool)
            .await?
            .rows_affected() as i64;
        }
    }
}

async fn mark(pool: &PgPool, chunks: &[i64]) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE chunk c SET deleting_at = now()
         WHERE pk = ANY($1) AND deleting_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM chunk_in_version WHERE chunk = c.pk)",
        chunks
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn is_constraint(e: &sqlx::Error, name: &str) -> bool {
    e.as_database_error().and_then(|d| d.constraint()) == Some(name)
}

/// Trashed nodes and libraries past the retention go for good (subtree,
/// versions, chunk references), one transaction each; their chunks follow with
/// the GC.
async fn trash_purge(state: &AppState) -> Result<i64, JobError> {
    let retention = state.settings.get().trash_retention.as_secs_f64();
    let mut purged = 0;
    loop {
        let nodes: Vec<i64> = sqlx::query_scalar!(
            "SELECT pk FROM node WHERE deleted_at <= now() - make_interval(secs => $1)
             ORDER BY deleted_at LIMIT $2",
            retention,
            BATCH
        )
        .fetch_all(&state.pool)
        .await?;
        if nodes.is_empty() {
            break;
        }
        for node in nodes {
            // A node inside an already purged folder went with it.
            let mut tx = state.pool.begin().await?;
            crate::db::node::purge(&mut tx, node).await?;
            tx.commit().await?;
            purged += 1;
        }
    }
    // Trashed libraries, whole.
    loop {
        let libraries: Vec<i64> = sqlx::query_scalar!(
            "SELECT pk FROM library WHERE deleted_at <= now() - make_interval(secs => $1)
             ORDER BY deleted_at LIMIT $2",
            retention,
            BATCH
        )
        .fetch_all(&state.pool)
        .await?;
        if libraries.is_empty() {
            return Ok(purged);
        }
        for library in libraries {
            let mut tx = state.pool.begin().await?;
            crate::db::library::purge(&mut tx, library).await?;
            tx.commit().await?;
            purged += 1;
        }
    }
}

/// Revoked tokens and expired sessions, job history and emails older than 30
/// days, expired invitations, never verified accounts.
async fn cleanup(state: &AppState) -> Result<i64, JobError> {
    let lifetime = state.settings.get().session_lifetime();
    let tokens = sqlx::query!(
        "DELETE FROM token
         WHERE revoked_at IS NOT NULL
            OR (usage = 'session' AND (last_seen < now() - make_interval(secs => $1)
                OR ($2::float8 IS NOT NULL AND created_at < now() - make_interval(secs => $2))))",
        lifetime.idle.as_secs_f64(),
        lifetime.max.map(|d| d.as_secs_f64())
    )
    .execute(&state.pool)
    .await?
    .rows_affected();
    let runs = sqlx::query!("DELETE FROM job_run WHERE finished_at < now() - interval '30 days'")
        .execute(&state.pool)
        .await?
        .rows_affected();
    // Sent or given-up emails hold addresses: kept 30 days for debugging.
    let emails = sqlx::query!(
        "DELETE FROM email WHERE sent_at < now() - interval '30 days' OR given_up_at < now() - interval '30 days'"
    )
    .execute(&state.pool)
    .await?
    .rows_affected();
    // Expired invitations: the pending user, and its account if it has no
    // identity and no other membership.
    let expired = sqlx::query!(
        r#"DELETE FROM "user" WHERE joined_at IS NULL
             AND invited_at < now() - make_interval(secs => $1) RETURNING account"#,
        crate::email::INVITATION_LIFETIME.as_secs_f64()
    )
    .fetch_all(&state.pool)
    .await?;
    for row in &expired {
        crate::db::organization::delete_if_phantom(&mut *state.pool.acquire().await?, row.account)
            .await?;
    }
    // Deleted organizations whose last chunk the GC has removed.
    let deleted = sqlx::query!(
        "DELETE FROM organization o WHERE o.deleted_at IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM chunk c WHERE c.organization = o.pk)"
    )
    .execute(&state.pool)
    .await?
    .rows_affected();
    // Self-registered accounts never verified: they could never log in, and
    // their address is freed (someone else may have registered it).
    let unverified = sqlx::query_scalar!(
        "SELECT a.pk FROM account a
         WHERE NOT a.verified AND a.created_at < now() - make_interval(secs => $1)
           AND EXISTS (SELECT 1 FROM identity WHERE account = a.pk AND kind = 'password')
           AND NOT EXISTS (SELECT 1 FROM identity WHERE account = a.pk AND kind = 'oidc')",
        crate::email::VERIFICATION_LIFETIME.as_secs_f64()
    )
    .fetch_all(&state.pool)
    .await?;
    for &account in &unverified {
        delete_unverified(&state.pool, account).await?;
    }
    Ok((tokens + runs + emails + deleted) as i64 + expired.len() as i64 + unverified.len() as i64)
}

/// Deletes a never verified account: its memberships, its personal
/// organization (empty: it never logged in), its tokens, itself.
async fn delete_unverified(pool: &PgPool, account: i64) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!(r#"DELETE FROM "user" WHERE account = $1"#, account)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM organization WHERE personal_of = $1", account)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM token WHERE resource = (SELECT uuid FROM account WHERE pk = $1)",
        account
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM account WHERE pk = $1", account)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Objects without a chunk row, older than any upload URL could be (rows exist
/// before their objects, so a young one is an upload in flight), are deleted
/// and logged as anomalies. Keys that aren't chunks are left alone.
async fn orphan_check(state: &AppState) -> Result<i64, JobError> {
    // An hour of margin past the longest URL (clock skew, slow uploads).
    let cutoff = unix_now() - state.settings.get().presigned_url_lifetime.as_secs() as i64 - 3600;
    let storages: Vec<i64> = sqlx::query_scalar!("SELECT pk FROM storage ORDER BY pk")
        .fetch_all(&state.pool)
        .await?;
    let mut deleted = 0;
    for storage in storages {
        let backend = state.backends.get(storage).await?;
        let mut objects = backend.list().chunks(BATCH as usize);
        while let Some(page) = objects.next().await {
            let mut old = Vec::new();
            for object in page {
                let (key, modified) = object?;
                match chunk_of(&key) {
                    Some(chunk) if modified < cutoff => old.push(chunk),
                    Some(_) => {}
                    None => {
                        tracing::warn!(storage, key, "an object that isn't a chunk: left alone")
                    }
                }
            }
            let known: Vec<Uuid> =
                sqlx::query_scalar!("SELECT uuid FROM chunk WHERE uuid = ANY($1)", &old)
                    .fetch_all(&state.pool)
                    .await?;
            for chunk in old.into_iter().filter(|c| !known.contains(c)) {
                tracing::warn!(anomaly = "orphan_object", storage, %chunk,
                    "an object without a chunk row (stored, never billed): deleted");
                backend.delete(chunk).await?;
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The jobs whose last 3 finished runs all failed.
pub async fn failing(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT job AS "job!" FROM (
               SELECT job, status, row_number() OVER (PARTITION BY job ORDER BY started_at DESC) AS n
               FROM job_run WHERE status IN ('success', 'failure')
           ) last WHERE n <= 3
           GROUP BY job HAVING count(*) = 3 AND bool_and(status = 'failure')
           ORDER BY job"#
    )
    .fetch_all(pool)
    .await
}

/// A job's last runs, newest first.
pub async fn runs(
    pool: &PgPool,
    job: Job,
    limit: i64,
) -> sqlx::Result<Vec<theca_common::api::JobRun>> {
    sqlx::query_as!(
        theca_common::api::JobRun,
        r#"SELECT uuid AS id, started_at, finished_at,
                  status, items, error
           FROM job_run WHERE job = $1 ORDER BY started_at DESC LIMIT $2"#,
        job.name(),
        limit
    )
    .fetch_all(pool)
    .await
}
