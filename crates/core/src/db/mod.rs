//! The database: connection, compatibility check, migrations, and every query
//! (handlers never write SQL). Refusals by named constraints are mapped to API
//! errors in `api::error`.

pub mod account;
pub mod admin;
pub mod chunk;
pub mod file;
pub mod identity;
pub mod library;
pub mod node;
pub mod organization;
pub mod setup;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::Error;

/// Connects, checks the database is supported, and applies pending migrations.
pub async fn connect(url: &str) -> Result<PgPool, Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    check(&pool).await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

/// Postgres 18+ (built-in `uuidv7()`) with the built-in `C.UTF-8` collation: case-insensitive name
/// uniqueness (`lower()`) depends on it being identical on every install.
pub async fn check(pool: &PgPool) -> Result<(), Error> {
    let row = sqlx::query!(
        r#"SELECT current_setting('server_version_num')::int AS "version!",
                  datlocprovider::text AS "provider!",
                  datlocale AS "locale?"
           FROM pg_database WHERE datname = current_database()"#
    )
    .fetch_one(pool)
    .await?;
    if row.version < 180_000 {
        return Err(Error::UnsupportedDatabase(format!(
            "PostgreSQL 18 or newer is required (found version number {})",
            row.version
        )));
    }
    if row.provider != "b" || row.locale.as_deref() != Some("C.UTF-8") {
        return Err(Error::UnsupportedDatabase(
            "the database must use the built-in C.UTF-8 collation \
             (initdb --locale-provider=builtin --builtin-locale=C.UTF-8)"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgConnectOptions;

    #[sqlx::test]
    async fn accepts_the_test_database(pool: PgPool) {
        check(&pool).await.unwrap();
    }

    #[sqlx::test(migrations = false)]
    async fn rejects_another_collation(_: PgPoolOptions, options: PgConnectOptions) {
        let admin = PgPool::connect_with(options.clone()).await.unwrap();
        let name = format!("theca_libc_{}", std::process::id());
        sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
            .execute(&admin)
            .await
            .unwrap();
        sqlx::query(&format!(
            "CREATE DATABASE {name} TEMPLATE template0 LOCALE_PROVIDER libc LOCALE 'C'"
        ))
        .execute(&admin)
        .await
        .unwrap();
        let libc = PgPool::connect_with(options.database(&name)).await.unwrap();
        let result = check(&libc).await;
        libc.close().await;
        sqlx::query(&format!("DROP DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        assert!(matches!(result, Err(Error::UnsupportedDatabase(_))));
    }
}
