//! Instance settings: a string-to-string table in the DB, typed here.
//!
//! Each field has a key, a default and a parser. Missing key → default;
//! unparsable value → startup error; unknown key → warning.

use std::collections::HashMap;
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

    /// Loads and validates every setting.
    pub async fn load(pool: &PgPool) -> Result<Self, Error> {
        let mut values: HashMap<String, String> = sqlx::query!("SELECT key, value FROM setting")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect();

        let settings = Self {
            instance_name: take(&mut values, "instance_name", "Theca", |v| Ok(v.to_owned()))?,
            gc_grace_period: take(&mut values, "gc_grace_period_secs", "86400", seconds)?,
            presigned_url_lifetime: take(
                &mut values,
                "presigned_url_lifetime_secs",
                "900",
                seconds,
            )?,
            max_pending_uploads: take(&mut values, "max_pending_uploads", "15", |v| {
                v.parse::<u32>().map(i64::from).map_err(|e| format!("{e}"))
            })?,
            session_idle_lifetime: take(
                &mut values,
                "session_idle_lifetime_secs",
                "604800",
                session_lifetime,
            )?,
            session_max_lifetime: take(&mut values, "session_max_lifetime_secs", "0", |v| {
                // 0: no cap.
                match seconds(v)? {
                    Duration::ZERO => Ok(None),
                    d => session_lifetime(v).map(|_| Some(d)),
                }
            })?,
            oidc_link_by_email: take(&mut values, "oidc_link_by_email", "false", |v| {
                v.parse::<bool>().map_err(|e| format!("{e}"))
            })?,
        };

        for key in values.keys() {
            tracing::warn!(
                key,
                "unknown setting ignored (typo, or left by another version?)"
            );
        }
        Ok(settings)
    }
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

fn take<T>(
    values: &mut HashMap<String, String>,
    key: &'static str,
    default: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, Error> {
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
