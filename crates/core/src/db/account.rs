//! Accounts, their identities, and sessions.

use std::net::IpAddr;

use sqlx::PgPool;
use sqlx::types::ipnetwork::IpNetwork;
use theca_common::api;
use uuid::Uuid;

use std::time::Duration;

use crate::auth::{LAST_SEEN_PRECISION, OpaqueToken};

/// A request's credential: a live session or app password (its `token` row).
/// What it reaches is decided in SQL: `organization_access(token)` and
/// `library_access(token)`.
#[derive(Debug, Clone, Copy)]
pub struct Credential {
    pub token: i64,
    /// The session or app password, as exposed.
    pub token_uuid: Uuid,
    pub account: i64,
    pub account_uuid: Uuid,
    /// A session (not an app password).
    pub session: bool,
    /// The only organization it acts on, if scoped (restricted session, app password).
    pub organization: Option<i64>,
    /// The only library it acts on, if scoped (app password).
    pub library: Option<i64>,
    /// An app password that only reads.
    pub read_only: bool,
}

impl Credential {
    /// A session that isn't restricted to an organization: may act on the account.
    pub fn is_full_session(&self) -> bool {
        self.session && self.organization.is_none()
    }
}

/// How long sessions live (instance settings).
#[derive(Debug, Clone, Copy)]
pub struct SessionLifetime {
    /// Without being used (sliding).
    pub idle: Duration,
    /// Since login, however used. `None`: no cap.
    pub max: Option<Duration>,
}

impl SessionLifetime {
    /// `(idle, max)` in seconds, for SQL.
    fn secs(&self) -> (f64, Option<f64>) {
        (self.idle.as_secs_f64(), self.max.map(|d| d.as_secs_f64()))
    }
}

/// The credential for a token presented by a client: not revoked, and for a
/// session, within its lifetimes. Slides `last_seen` (written at most every
/// [`LAST_SEEN_PRECISION`]).
pub async fn authenticate(
    pool: &PgPool,
    prefix: &str,
    hash: &[u8; 32],
    lifetime: SessionLifetime,
) -> sqlx::Result<Option<Credential>> {
    let (idle, max) = lifetime.secs();
    let row = sqlx::query!(
        r#"WITH t AS (
               SELECT pk, uuid, usage, organization, library, read_only, resource, last_seen FROM token
               WHERE prefix = $1 AND hash = $2 AND usage IN ('session', 'app_password')
                 AND revoked_at IS NULL
                 AND (usage <> 'session' OR (last_seen > now() - make_interval(secs => $3)
                      AND ($5::float8 IS NULL OR created_at > now() - make_interval(secs => $5))))
           ), touched AS (
               UPDATE token SET last_seen = now() FROM t
               WHERE token.pk = t.pk
                 AND (t.last_seen IS NULL OR t.last_seen < now() - make_interval(secs => $4))
           )
           SELECT t.pk AS "pk!", t.uuid AS "uuid!", t.usage = 'session' AS "session!",
                  t.organization, t.library, t.read_only AS "read_only!", a.pk AS account, a.uuid AS account_uuid
           FROM t JOIN account a ON a.uuid = t.resource AND a.suspended_at IS NULL"#,
        prefix,
        &hash[..],
        idle,
        LAST_SEEN_PRECISION.as_secs() as f64,
        max
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Credential {
        token: r.pk,
        token_uuid: r.uuid,
        account: r.account,
        account_uuid: r.account_uuid,
        session: r.session,
        organization: r.organization,
        library: r.library,
        read_only: r.read_only,
    }))
}

/// Self-registration: an unverified account with its password and its
/// personal organization. `None` if the address has an account.
pub async fn register(
    db: &mut sqlx::PgConnection,
    email: &str,
    password_hash: &str,
) -> sqlx::Result<Option<Uuid>> {
    let Some(account) = sqlx::query!(
        "INSERT INTO account (email) VALUES ($1) ON CONFLICT ((lower(email))) DO NOTHING RETURNING pk, uuid",
        email
    )
    .fetch_optional(&mut *db)
    .await?
    else {
        return Ok(None);
    };
    set_password(db, account.pk, password_hash).await?;
    super::organization::create(db, account.pk, email, true).await?;
    Ok(Some(account.uuid))
}

/// Sets (or replaces) an account's password.
pub async fn set_password(
    db: &mut sqlx::PgConnection,
    account: i64,
    password_hash: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO identity (account, kind, details) VALUES ($1, 'password', jsonb_build_object('hash', $2::text))
         ON CONFLICT (account) WHERE kind = 'password' DO UPDATE SET details = excluded.details",
        account,
        password_hash
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Marks an account's email verified (its owner proved the inbox). Returns its pk.
pub async fn set_verified(db: &mut sqlx::PgConnection, account: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "UPDATE account SET verified = true WHERE uuid = $1 RETURNING pk",
        account
    )
    .fetch_optional(db)
    .await
}

/// Revokes all of an account's sessions (e.g. after a password reset).
pub async fn revoke_sessions(
    db: &mut sqlx::PgConnection,
    account: Uuid,
    reason: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = $2
         WHERE resource = $1 AND usage = 'session' AND revoked_at IS NULL",
        account,
        reason
    )
    .execute(db)
    .await?;
    Ok(())
}

/// An account by email: `(uuid, verified, has a password)`.
pub async fn by_email(pool: &PgPool, email: &str) -> sqlx::Result<Option<(Uuid, bool, bool)>> {
    let row = sqlx::query!(
        r#"SELECT a.uuid, a.verified,
                  EXISTS (SELECT 1 FROM identity WHERE account = a.pk AND kind = 'password') AS "password!"
           FROM account a WHERE lower(a.email) = lower($1)"#,
        email
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.uuid, r.verified, r.password)))
}

/// The account, password hash and verified flag for an email (case-insensitive).
pub async fn password_of(
    pool: &PgPool,
    email: &str,
) -> sqlx::Result<Option<(i64, Uuid, Option<String>, bool, bool)>> {
    let row = sqlx::query!(
        r#"SELECT a.pk, a.uuid, i.details->>'hash' AS "hash?", a.verified,
                  a.suspended_at IS NOT NULL AS "suspended!"
           FROM account a LEFT JOIN identity i ON i.account = a.pk AND i.kind = 'password'
           WHERE lower(a.email) = lower($1)"#,
        email
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.uuid, r.hash, r.verified, r.suspended)))
}

/// Opens a session: a refresh token (returned once), its row, and its uuid.
pub async fn create_session(
    pool: &PgPool,
    account: Uuid,
    organization: Option<i64>,
    user_agent: Option<&str>,
    ip: Option<IpAddr>,
) -> sqlx::Result<(OpaqueToken, Uuid)> {
    let token = OpaqueToken::generate();
    // ponytail: the raw user agent; a short "Firefox on macOS" once the web app shows it.
    let name: String = user_agent
        .unwrap_or("Unknown device")
        .chars()
        .take(200)
        .collect();
    let session = sqlx::query_scalar!(
        "INSERT INTO token (usage, resource, name, prefix, hash, user_agent, ip, organization, last_seen)
         VALUES ('session', $1, $7, $2, $3, $4, $5, $6, now()) RETURNING uuid",
        account,
        token.prefix,
        &token.hash[..],
        user_agent,
        ip.map(IpNetwork::from) as Option<IpNetwork>,
        organization,
        name
    )
    .fetch_one(pool)
    .await?;
    Ok((token, session))
}

/// Revokes one of the account's sessions. Returns whether one was revoked.
pub async fn revoke_session(
    pool: &PgPool,
    account: Uuid,
    session: Uuid,
    reason: &str,
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = $3
         WHERE uuid = $2 AND resource = $1 AND usage = 'session' AND revoked_at IS NULL",
        account,
        session,
        reason
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The account's live sessions.
pub async fn sessions(
    pool: &PgPool,
    account: Uuid,
    current: Uuid,
    lifetime: SessionLifetime,
) -> sqlx::Result<Vec<api::Session>> {
    let (idle, max) = lifetime.secs();
    let rows = sqlx::query!(
        r#"SELECT t.uuid, t.name, t.user_agent, host(t.ip) AS "ip?", t.created_at,
                  t.last_seen, o.uuid AS "organization?"
           FROM token t LEFT JOIN organization o ON o.pk = t.organization
           WHERE t.resource = $1 AND t.usage = 'session' AND t.revoked_at IS NULL
             AND t.last_seen > now() - make_interval(secs => $2)
             AND ($3::float8 IS NULL OR t.created_at > now() - make_interval(secs => $3))
           ORDER BY t.last_seen DESC"#,
        account,
        idle,
        max
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| api::Session {
            id: r.uuid,
            name: r.name,
            user_agent: r.user_agent,
            ip: r.ip,
            created_at: r.created_at,
            last_seen: r.last_seen,
            restricted_to: r.organization,
            current: r.uuid == current,
        })
        .collect())
}

/// The account and its organizations (joined or invited).
pub async fn me(pool: &PgPool, account: i64) -> sqlx::Result<api::Me> {
    let a = sqlx::query!(
        r#"SELECT uuid, email, instance_admin, EXISTS (
               SELECT 1 FROM identity WHERE account = $1 AND kind = 'totp' AND details->>'confirmed' = 'true'
           ) AS "totp!" FROM account WHERE pk = $1"#,
        account
    )
        .fetch_one(pool)
        .await?;
    let organizations = sqlx::query_as!(
        api::Membership,
        r#"SELECT o.uuid AS organization, o.name, u.joined_at IS NOT NULL AS "joined!",
                  o.personal_of IS NOT NULL AS "personal!", o.suspended_at IS NOT NULL AS "suspended!",
                  o.owner IS NOT DISTINCT FROM u.pk AS "owner!"
           FROM "user" u JOIN organization o ON o.pk = u.organization
           WHERE u.account = $1 ORDER BY lower(o.name)"#,
        account
    )
    .fetch_all(pool)
    .await?;
    Ok(api::Me {
        account: a.uuid,
        email: a.email,
        totp: a.totp,
        instance_admin: a.instance_admin,
        organizations,
    })
}

/// What an app password is limited to, resolved through the creating
/// credential's access: `(organization, library)` pks.
pub enum Scope {
    Account,
    Organization(i64),
    Library(i64, i64),
}

/// Resolves a requested scope through what `who` reaches. `None`: not reachable.
pub async fn resolve_scope(
    pool: &PgPool,
    who: &Credential,
    organization: Option<Uuid>,
    library: Option<Uuid>,
) -> sqlx::Result<Option<Scope>> {
    if let Some(library) = library {
        let found = sqlx::query!(
            r#"SELECT a.library AS "library!", a.organization AS "organization!", o.uuid
               FROM library_access($1) a
               JOIN library l ON l.pk = a.library
               JOIN organization o ON o.pk = a.organization
               WHERE l.uuid = $2"#,
            who.token,
            library
        )
        .fetch_optional(pool)
        .await?;
        return Ok(found
            .filter(|f| organization.is_none_or(|o| o == f.uuid))
            .map(|f| Scope::Library(f.organization, f.library)));
    }
    if let Some(organization) = organization {
        let found = sqlx::query_scalar!(
            r#"SELECT a.organization AS "organization!" FROM organization_access($1) a
               JOIN organization o ON o.pk = a.organization WHERE o.uuid = $2"#,
            who.token,
            organization
        )
        .fetch_optional(pool)
        .await?;
        return Ok(found.map(Scope::Organization));
    }
    // A restricted session can only hand out its own organization.
    Ok(Some(
        who.organization.map_or(Scope::Account, Scope::Organization),
    ))
}

/// Creates an app password for the account.
pub async fn create_app_password(
    pool: &PgPool,
    who: &Credential,
    name: &str,
    scope: Scope,
    read_only: bool,
) -> sqlx::Result<(OpaqueToken, api::AppPassword)> {
    let token = OpaqueToken::generate();
    let (organization, library) = match scope {
        Scope::Account => (None, None),
        Scope::Organization(o) => (Some(o), None),
        Scope::Library(o, l) => (Some(o), Some(l)),
    };
    let uuid = sqlx::query_scalar!(
        "INSERT INTO token (usage, resource, name, prefix, hash, organization, library, read_only)
         VALUES ('app_password', $1, $2, $3, $4, $5, $6, $7) RETURNING uuid",
        who.account_uuid,
        name,
        token.prefix,
        &token.hash[..],
        organization,
        library,
        read_only
    )
    .fetch_one(pool)
    .await?;
    let created = app_passwords(pool, who)
        .await?
        .into_iter()
        .find(|p| p.id == uuid)
        .expect("just created");
    Ok((token, created))
}

/// The account's app passwords (a restricted session: only those limited to
/// its organization).
pub async fn app_passwords(pool: &PgPool, who: &Credential) -> sqlx::Result<Vec<api::AppPassword>> {
    sqlx::query_as!(
        api::AppPassword,
        r#"SELECT t.uuid AS id, t.name, o.uuid AS "organization?", l.uuid AS "library?", t.read_only,
                  t.created_at, t.last_seen
           FROM token t
           LEFT JOIN organization o ON o.pk = t.organization
           LEFT JOIN library l ON l.pk = t.library
           WHERE t.resource = $1 AND t.usage = 'app_password' AND t.revoked_at IS NULL
             AND ($2::bigint IS NULL OR t.organization = $2)
           ORDER BY t.created_at"#,
        who.account_uuid,
        who.organization
    )
    .fetch_all(pool)
    .await
}

/// Revokes one of the account's app passwords (same visibility as
/// [`app_passwords`]). Returns whether one was revoked.
pub async fn revoke_app_password(pool: &PgPool, who: &Credential, id: Uuid) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = 'revoked'
         WHERE uuid = $2 AND resource = $1 AND usage = 'app_password' AND revoked_at IS NULL
           AND ($3::bigint IS NULL OR organization = $3)",
        who.account_uuid,
        id,
        who.organization
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// An account's TOTP identity.
pub struct TotpIdentity {
    pub pk: i64,
    /// Encrypted (see [`crate::keys::DataCipher`], the account uuid as associated data).
    pub secret: String,
    pub confirmed: bool,
}

/// The account's TOTP identity, if any.
pub async fn totp_of(pool: &PgPool, account: i64) -> sqlx::Result<Option<TotpIdentity>> {
    let row = sqlx::query!(
        r#"SELECT pk, details->>'secret' AS "secret!", (details->>'confirmed')::boolean AS "confirmed!"
           FROM identity WHERE account = $1 AND kind = 'totp'"#,
        account
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| TotpIdentity {
        pk: r.pk,
        secret: r.secret,
        confirmed: r.confirmed,
    }))
}

/// Stores a new, unconfirmed TOTP secret, replacing an unconfirmed one.
/// `false` if TOTP is already on.
pub async fn start_totp(pool: &PgPool, account: i64, secret: &str) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "INSERT INTO identity (account, kind, details)
         VALUES ($1, 'totp', jsonb_build_object('secret', $2::text, 'confirmed', false, 'last_step', 0))
         ON CONFLICT (account) WHERE kind = 'totp'
         DO UPDATE SET details = excluded.details WHERE identity.details->>'confirmed' = 'false'",
        account,
        secret
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Records a code's time step as used. `false` if it (or a later one) already
/// was: a code is accepted once.
pub async fn use_totp_step(pool: &PgPool, identity: i64, step: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE identity SET details = jsonb_set(details, '{last_step}', to_jsonb($2::bigint))
         WHERE pk = $1 AND (details->>'last_step')::bigint < $2",
        identity,
        step
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Uses one of the account's recovery codes. `false` if it isn't a live one.
pub async fn use_recovery_code(
    pool: &PgPool,
    account: Uuid,
    prefix: &str,
    hash: &[u8; 32],
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = 'used'
         WHERE prefix = $2 AND hash = $3 AND resource = $1 AND usage = 'recovery_code'
           AND revoked_at IS NULL",
        account,
        prefix,
        &hash[..]
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Confirms TOTP (if `identity` given) and replaces the account's recovery
/// codes with new ones, given as `(prefix, hash)`. One transaction.
pub async fn set_recovery_codes(
    pool: &PgPool,
    account: Uuid,
    confirm: Option<i64>,
    codes: &[(String, [u8; 32])],
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    if let Some(identity) = confirm {
        sqlx::query!(
            "UPDATE identity SET details = jsonb_set(details, '{confirmed}', 'true') WHERE pk = $1",
            identity
        )
        .execute(&mut *tx)
        .await?;
    }
    revoke_recovery_codes(&mut tx, account, "replaced").await?;
    let prefixes: Vec<&str> = codes.iter().map(|(p, _)| p.as_str()).collect();
    let hashes: Vec<&[u8]> = codes.iter().map(|(_, h)| &h[..]).collect();
    sqlx::query!(
        "INSERT INTO token (usage, resource, name, prefix, hash)
         SELECT 'recovery_code', $1, 'recovery code', p, h FROM unnest($2::text[], $3::bytea[]) AS c(p, h)",
        account,
        &prefixes as &[&str],
        &hashes as &[&[u8]]
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Turns TOTP off: deletes the identity and revokes the recovery codes.
pub async fn disable_totp(pool: &PgPool, account: i64, account_uuid: Uuid) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "DELETE FROM identity WHERE account = $1 AND kind = 'totp'",
        account
    )
    .execute(&mut *tx)
    .await?;
    revoke_recovery_codes(&mut tx, account_uuid, "totp_disabled").await?;
    tx.commit().await
}

async fn revoke_recovery_codes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: Uuid,
    reason: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = $2
         WHERE resource = $1 AND usage = 'recovery_code' AND revoked_at IS NULL",
        account,
        reason
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A one-off token (email verification, password reset, invitation) for
/// `resource`, created at send time. Revokes the previous unused ones of the
/// same usage and resource: only the latest link works.
pub async fn create_one_off(
    db: &mut sqlx::PgConnection,
    usage: &str,
    resource: Uuid,
) -> sqlx::Result<OpaqueToken> {
    sqlx::query!(
        "UPDATE token SET revoked_at = now(), revocation_reason = 'replaced'
         WHERE usage = $1 AND resource = $2 AND revoked_at IS NULL",
        usage,
        resource
    )
    .execute(&mut *db)
    .await?;
    let token = OpaqueToken::generate();
    sqlx::query!(
        "INSERT INTO token (usage, resource, name, prefix, hash) VALUES ($1, $2, $1, $3, $4)",
        usage,
        resource,
        token.prefix,
        &token.hash[..]
    )
    .execute(&mut *db)
    .await?;
    Ok(token)
}

/// Uses a one-off token: valid (right usage, not used or replaced, younger
/// than `lifetime`) once. Returns its resource.
pub async fn consume_one_off(
    db: &mut sqlx::PgConnection,
    usage: &str,
    token: &str,
    lifetime: Duration,
) -> sqlx::Result<Option<Uuid>> {
    let Some((prefix, hash)) = OpaqueToken::parse(token) else {
        return Ok(None);
    };
    sqlx::query_scalar!(
        "UPDATE token SET revoked_at = now(), revocation_reason = 'used'
         WHERE prefix = $1 AND hash = $2 AND usage = $3 AND revoked_at IS NULL
           AND created_at > now() - make_interval(secs => $4)
         RETURNING resource",
        prefix,
        &hash[..],
        usage,
        lifetime.as_secs_f64()
    )
    .fetch_optional(db)
    .await
}
