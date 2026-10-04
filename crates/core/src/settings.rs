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
}

impl Settings {
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
            gc_grace_period: take(&mut values, "gc_grace_period_secs", "86400", |v| {
                v.parse()
                    .map(Duration::from_secs)
                    .map_err(|e| format!("{e}"))
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
