//! Durable auxiliary state owned exclusively by Saffron.
//!
//! Saffron databases use their own filenames, schemas, and SQLx migration
//! ledgers. They must never attach to or modify a Codex-owned database. This
//! module owns the complete storage contract so upstream integration points
//! need only provide the configured SQLite home.

use codex_state::SqliteConfig;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

const SAFFRON_DB_FILENAME: &str = "saffron_1.sqlite";
static SAFFRON_MIGRATOR: Migrator = sqlx::migrate!("./src/saffron/migrations");

/// Connection owner for Saffron's independent auxiliary database.
#[derive(Clone)]
pub(super) struct SaffronStore {
    pool: SqlitePool,
}

impl SaffronStore {
    /// Opens `saffron_1.sqlite` and applies only Saffron-owned migrations.
    pub(super) async fn open(sqlite: &SqliteConfig) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(sqlite.home()).await?;
        let path = sqlite.home().join(SAFFRON_DB_FILENAME);
        let pool = sqlite.open_read_write_pool(&path).await?;
        if let Err(error) = SAFFRON_MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self { pool })
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
