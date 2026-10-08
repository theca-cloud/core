//! Instance settings: a string-to-string table in the DB, typed here.
//!
//! Each setting has a key, a default and a description ([`DEFINITIONS`]), and
//! a parser. Missing key → default; unparsable value → startup error (or a
//! refused change); unknown key → warning. The server holds them in memory
//! ([`LiveSettings`]): a change through the admin API applies at once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::Error;

/// The instance settings, held in memory by the server.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Shown in emails and the web app.
    pub instance_name: String,
    /// How long an unreferenced, untouched chunk is kept before GC deletes it.
    pub gc_grace_period: Duration,
    /// How long presigned upload and download URLs stay valid.
    pub presigned_url_lifetime: Duration,
    /// Most chunks a user may have "being uploaded" at once: declared, with an
    /// upload URL still valid, not uploaded yet. Clients upload in batches.
    pub max_pending_uploads: i64,
    /// Sessions expire after this long without being used (sliding).
    pub session_idle_lifetime: Duration,
    /// Sessions expire this long after login, however used. `None`: no cap.
    pub session_max_lifetime: Option<Duration>,
    /// An OIDC login with an unknown identity is linked to the account with
    /// the same email, if the provider says the email is verified.
    pub oidc_link_by_email: bool,
    /// Organizations' providers may use `http` and internal addresses (the SSRF
    /// guard off): only when every organization on the instance is trusted.
    pub oidc_allow_internal_issuers: bool,
    /// How long trashed files stay restorable before the purge (`0`: purged at
    /// its next run).
    pub trash_retention: Duration,
    /// Anyone may create an account (`registration = open`; needs SMTP).
    pub registration_open: bool,
    /// The zxcvbn score (0 to 4) a new password needs; 0: 8 characters only.
    pub password_strength: u8,
    /// Every account may create company organizations
    /// (`organization_creation = anyone`), not only instance admins.
    pub organization_creation_open: bool,
    /// Where emails go out. `None` (no `smtp_host`): emails stay queued.
    pub smtp: Option<Smtp>,
    /// Each background job's interval, by job name.
    pub job_intervals: HashMap<String, Duration>,
    /// Each rate limit: `(count, window)`, by limit name.
    pub rate_limits: HashMap<String, (u32, Duration)>,
}

/// A setting: its key, its default, and what it does.
pub struct Definition {
    pub key: &'static str,
    pub default: &'static str,
    pub description: &'static str,
}

const fn def(key: &'static str, default: &'static str, description: &'static str) -> Definition {
    Definition {
        key,
        default,
        description,
    }
}

/// Every setting (CONFIGURATION.md says more).
pub const DEFINITIONS: &[Definition] = &[
    def("instance_name", "Theca", "Shown in emails and the web app."),
    def(
        "registration",
        "closed",
        "open: anyone can create an account (needs SMTP); closed: invitations only.",
    ),
    def(
        "organization_creation",
        "anyone",
        "anyone: every account can create company organizations; admins: only instance admins.",
    ),
    def(
        "password_strength",
        "3",
        "The zxcvbn score (0 to 4) a new password needs; 0: 8 characters only.",
    ),
    def(
        "gc_grace_period_secs",
        "86400",
        "How long an unreferenced, untouched chunk is kept before deletion.",
    ),
    def(
        "presigned_url_lifetime_secs",
        "900",
        "How long presigned upload and download URLs stay valid.",
    ),
    def(
        "max_pending_uploads",
        "15",
        "Most chunks an account may be uploading at once.",
    ),
    def(
        "session_idle_lifetime_secs",
        "604800",
        "Sessions expire after this long without use (at least 600).",
    ),
    def(
        "session_max_lifetime_secs",
        "0",
        "Sessions expire this long after login (0: never; else at least 600).",
    ),
    def(
        "oidc_link_by_email",
        "false",
        "Link an unknown OIDC identity to the verified account with its email.",
    ),
    def(
        "oidc_allow_internal_issuers",
        "false",
        "Organizations' providers may use http and internal addresses (trusted organizations only).",
    ),
    def(
        "trash_retention_secs",
        "2592000",
        "How long trashed files stay restorable (0: purged at the next run).",
    ),
    def(
        "smtp_host",
        "",
        "The SMTP server (unset: emails stay queued).",
    ),
    def("smtp_port", "587", "Its port."),
    def("smtp_tls", "starttls", "starttls, tls or none."),
    def("smtp_username", "", "Login, if needed."),
    def(
        "smtp_password_file",
        "",
        "Path to a file holding the SMTP password.",
    ),
    def(
        "smtp_from",
        "",
        "The sender, e.g. Theca <noreply@example.com> (required with smtp_host).",
    ),
    def(
        "rate_limit_login_ip",
        "30/300",
        "Logins from one address: count/seconds.",
    ),
    def(
        "rate_limit_login_account",
        "10/300",
        "Failed logins on one account: count/seconds.",
    ),
    def(
        "rate_limit_totp",
        "10/300",
        "Failed TOTP or recovery codes on one account: count/seconds.",
    ),
    def(
        "rate_limit_email_address",
        "5/3600",
        "Emails asked for one address (registration, verification, reset): count/seconds.",
    ),
    def(
        "rate_limit_email_ip",
        "30/3600",
        "Emails asked from one address: count/seconds.",
    ),
    def(
        "rate_limit_setup_ip",
        "10/60",
        "Setup attempts from one address: count/seconds.",
    ),
    def(
        "rate_limit_invitations",
        "100/3600",
        "Members added to one organization: count/seconds.",
    ),
    def(
        "job_chunk_gc_interval_secs",
        "3600",
        "How often unreferenced chunks are collected.",
    ),
    def(
        "job_trash_purge_interval_secs",
        "3600",
        "How often the trash past its retention is purged.",
    ),
    def(
        "job_cleanup_interval_secs",
        "86400",
        "How often expired tokens, invitations, accounts and history are cleaned.",
    ),
    def(
        "job_orphan_check_interval_secs",
        "86400",
        "How often backends are checked for objects without a row.",
    ),
    def(
        "job_email_send_interval_secs",
        "30",
        "How often queued emails are sent.",
    ),
];

/// The settings in memory, swapped whole when they change; whoever waits on
/// them (the jobs' loops) is woken up.
#[derive(Debug, Clone)]
pub struct LiveSettings(Arc<tokio::sync::watch::Sender<Arc<Settings>>>);

impl LiveSettings {
    pub fn new(settings: Settings) -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(Arc::new(
            settings,
        ))))
    }

    /// The current settings (a snapshot: a change later doesn't affect it).
    pub fn get(&self) -> Arc<Settings> {
        self.0.borrow().clone()
    }

    pub fn set(&self, settings: Settings) {
        self.0.send_replace(Arc::new(settings));
    }

    /// Notified at each change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Settings>> {
        self.0.subscribe()
    }
}

/// The SMTP server (settings `smtp_*`).
#[derive(Debug, Clone, PartialEq)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub tls: SmtpTls,
    pub username: Option<String>,
    /// A path, like every secret (decisions.md, Configuration).
    pub password_file: Option<String>,
    /// The sender, e.g. `Theca <noreply@example.com>`.
    pub from: String,
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmtpTls {
    /// Plain, then upgraded (port 587).
    StartTls,
    /// TLS from the start (port 465).
    Tls,
    /// None: only for a server on the same host or network (or tests).
    None,
}

/// The shortest session lifetime: `last_seen` is only written every
/// [`crate::auth::LAST_SEEN_PRECISION`], so shorter could expire active sessions.
const MIN_SESSION_LIFETIME: Duration = Duration::from_secs(600);

impl Settings {
    pub fn session_lifetime(&self) -> crate::db::account::SessionLifetime {
        crate::db::account::SessionLifetime {
            idle: self.session_idle_lifetime,
            max: self.session_max_lifetime,
        }
    }

    /// The stored values (only those set).
    pub async fn stored(pool: &PgPool) -> Result<HashMap<String, String>, Error> {
        Ok(sqlx::query!("SELECT key, value FROM setting")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect())
    }

    /// Loads and validates every setting.
    pub async fn load(pool: &PgPool) -> Result<Self, Error> {
        let (settings, unknown) = Self::parse(Self::stored(pool).await?)?;
        for key in unknown {
            tracing::warn!(
                key,
                "unknown setting ignored (typo, or left by another version?)"
            );
        }
        Ok(settings)
    }

    /// Parses stored values (missing ones take their defaults). Also returns
    /// the keys no setting has.
    pub fn parse(mut values: HashMap<String, String>) -> Result<(Self, Vec<String>), Error> {
        let values = &mut values;
        let job_intervals = DEFINITIONS
            .iter()
            .filter_map(|d| {
                Some((
                    d.key,
                    d.key.strip_prefix("job_")?.strip_suffix("_interval_secs")?,
                ))
            })
            .map(|(key, job)| {
                let interval = take(values, key, |v| match seconds(v)? {
                    Duration::ZERO => Err("at least 1 second".into()),
                    d => Ok(d),
                })?;
                Ok((job.to_owned(), interval))
            })
            .collect::<Result<_, Error>>()?;
        let rate_limits = DEFINITIONS
            .iter()
            .filter_map(|d| Some((d.key, d.key.strip_prefix("rate_limit_")?)))
            .map(|(key, limit)| {
                let parsed = take(values, key, |v| {
                    let (count, secs) = v.split_once('/').ok_or("count/seconds, e.g. 10/60")?;
                    let count: u32 = count.trim().parse().map_err(|e| format!("{e}"))?;
                    match seconds(secs.trim())? {
                        Duration::ZERO => Err("a window of at least 1 second".into()),
                        window => Ok((count, window)),
                    }
                })?;
                Ok((limit.to_owned(), parsed))
            })
            .collect::<Result<_, Error>>()?;
        let settings = Self {
            instance_name: take(values, "instance_name", |v| Ok(v.to_owned()))?,
            gc_grace_period: take(values, "gc_grace_period_secs", seconds)?,
            presigned_url_lifetime: take(values, "presigned_url_lifetime_secs", seconds)?,
            max_pending_uploads: take(values, "max_pending_uploads", |v| {
                v.parse::<u32>().map(i64::from).map_err(|e| format!("{e}"))
            })?,
            session_idle_lifetime: take(values, "session_idle_lifetime_secs", session_lifetime)?,
            session_max_lifetime: take(values, "session_max_lifetime_secs", |v| {
                // 0: no cap.
                match seconds(v)? {
                    Duration::ZERO => Ok(None),
                    d => session_lifetime(v).map(|_| Some(d)),
                }
            })?,
            registration_open: take(values, "registration", |v| match v {
                "open" => Ok(true),
                "closed" => Ok(false),
                _ => Err("open or closed".into()),
            })?,
            password_strength: take(values, "password_strength", |v| match v.parse::<u8>() {
                Ok(level @ 0..=4) => Ok(level),
                _ => Err("0 to 4".into()),
            })?,
            organization_creation_open: take(values, "organization_creation", |v| match v {
                "anyone" => Ok(true),
                "admins" => Ok(false),
                _ => Err("anyone or admins".into()),
            })?,
            smtp: smtp(values)?,
            trash_retention: take(values, "trash_retention_secs", seconds)?,
            oidc_link_by_email: take(values, "oidc_link_by_email", |v| {
                v.parse::<bool>().map_err(|e| format!("{e}"))
            })?,
            oidc_allow_internal_issuers: take(values, "oidc_allow_internal_issuers", |v| {
                v.parse::<bool>().map_err(|e| format!("{e}"))
            })?,
            job_intervals,
            rate_limits,
        };
        Ok((settings, values.keys().cloned().collect()))
    }
}

/// The SMTP settings, if `smtp_host` is set (then `smtp_from` is required).
fn smtp(values: &mut HashMap<String, String>) -> Result<Option<Smtp>, Error> {
    let optional = |values: &mut HashMap<String, String>, key: &str| {
        values.remove(key).filter(|v| !v.is_empty())
    };
    let host = optional(values, "smtp_host");
    let port = take(values, "smtp_port", |v| {
        v.parse::<u16>().map_err(|e| format!("{e}"))
    })?;
    let tls = take(values, "smtp_tls", |v| match v {
        "starttls" => Ok(SmtpTls::StartTls),
        "tls" => Ok(SmtpTls::Tls),
        "none" => Ok(SmtpTls::None),
        _ => Err("starttls, tls or none".into()),
    })?;
    let username = optional(values, "smtp_username");
    let password_file = optional(values, "smtp_password_file");
    let from = optional(values, "smtp_from");
    let Some(host) = host else {
        return Ok(None);
    };
    let from = from.ok_or(Error::InvalidSetting {
        key: "smtp_from",
        reason: "required with smtp_host".into(),
    })?;
    Ok(Some(Smtp {
        host,
        port,
        tls,
        username,
        password_file,
        from,
    }))
}

fn seconds(v: &str) -> Result<Duration, String> {
    v.parse()
        .map(Duration::from_secs)
        .map_err(|e| format!("{e}"))
}

fn session_lifetime(v: &str) -> Result<Duration, String> {
    let d = seconds(v)?;
    if d < MIN_SESSION_LIFETIME {
        return Err(format!(
            "at least {} seconds",
            MIN_SESSION_LIFETIME.as_secs()
        ));
    }
    Ok(d)
}

/// A setting's value (its default if unset), parsed.
fn take<T>(
    values: &mut HashMap<String, String>,
    key: &'static str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, Error> {
    let default = DEFINITIONS
        .iter()
        .find(|d| d.key == key)
        .map(|d| d.default)
        .expect("every setting is in DEFINITIONS");
    let raw = values.remove(key);
    parse(raw.as_deref().unwrap_or(default)).map_err(|reason| Error::InvalidSetting { key, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn set(pool: &PgPool, key: &str, value: &str) {
        sqlx::query!(
            "INSERT INTO setting (key, value) VALUES ($1, $2)",
            key,
            value
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn defaults(pool: PgPool) {
        let settings = Settings::load(&pool).await.unwrap();
        assert_eq!(settings.instance_name, "Theca");
        assert_eq!(settings.gc_grace_period, Duration::from_secs(86_400));
        assert_eq!(
            settings.session_idle_lifetime,
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(settings.session_max_lifetime, None);
    }

    #[sqlx::test]
    async fn stored_values_and_unknown_keys(pool: PgPool) {
        set(&pool, "instance_name", "My cloud").await;
        set(&pool, "gc_grace_period_secs", "3600").await;
        set(&pool, "typo_key", "x").await;
        let settings = Settings::load(&pool).await.unwrap();
        assert_eq!(settings.instance_name, "My cloud");
        assert_eq!(settings.gc_grace_period, Duration::from_secs(3600));
    }

    #[sqlx::test]
    async fn session_lifetimes(pool: PgPool) {
        set(&pool, "session_max_lifetime_secs", "2592000").await;
        let settings = Settings::load(&pool).await.unwrap();
        assert_eq!(
            settings.session_max_lifetime,
            Some(Duration::from_secs(2_592_000))
        );
        for key in ["session_idle_lifetime_secs", "session_max_lifetime_secs"] {
            sqlx::query!("DELETE FROM setting")
                .execute(&pool)
                .await
                .unwrap();
            set(&pool, key, "60").await;
            assert!(
                matches!(
                    Settings::load(&pool).await,
                    Err(Error::InvalidSetting { .. })
                ),
                "{key} below the floor"
            );
        }
    }

    #[sqlx::test]
    async fn invalid_value_is_an_error(pool: PgPool) {
        set(&pool, "gc_grace_period_secs", "one day").await;
        assert!(matches!(
            Settings::load(&pool).await,
            Err(Error::InvalidSetting {
                key: "gc_grace_period_secs",
                ..
            })
        ));
    }
}
