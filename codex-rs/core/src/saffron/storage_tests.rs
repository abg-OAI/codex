use codex_state::SqliteConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn saffron_database_has_an_independent_empty_ledger() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let state =
        codex_state::StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await?;
    let state_versions_before = migration_versions(state.sqlite().state_db_path(), &sqlite).await?;

    let _store = SaffronStore::open(&sqlite).await?;

    assert_eq!(
        migration_versions(home.path().join(SAFFRON_DB_FILENAME), &sqlite).await?,
        Vec::<i64>::new()
    );
    assert_eq!(
        migration_versions(state.sqlite().state_db_path(), &sqlite).await?,
        state_versions_before
    );
    Ok(())
}

async fn migration_versions(
    path: std::path::PathBuf,
    sqlite: &SqliteConfig,
) -> anyhow::Result<Vec<i64>> {
    let pool = sqlite.open_read_write_pool(&path).await?;
    let versions = sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(versions)
}
