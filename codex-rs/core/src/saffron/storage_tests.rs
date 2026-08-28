use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn saffron_migrations_have_an_independent_ledger() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let state =
        codex_state::StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await?;
    let state_versions_before = migration_versions(state.sqlite().state_db_path(), &sqlite).await?;

    let _store = SaffronStore::open(&sqlite).await?;

    assert_eq!(
        migration_versions(home.path().join(SAFFRON_DB_FILENAME), &sqlite).await?,
        vec![1]
    );
    assert_eq!(
        migration_versions(state.sqlite().state_db_path(), &sqlite).await?,
        state_versions_before
    );
    Ok(())
}

#[tokio::test]
async fn replacement_and_conditional_cleanup_preserve_the_newest_wake() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let store = SaffronStore::open(&sqlite).await?;
    let thread_id = ThreadId::new();
    let old = GoalWake {
        thread_id,
        goal_id: "old-goal".to_string(),
        goal_objective: "old objective".to_string(),
        goal_updated_at_ms: 10,
        wake_at_ms: 100,
    };
    let replacement = GoalWake {
        thread_id,
        goal_id: "new-goal".to_string(),
        goal_objective: "new objective".to_string(),
        goal_updated_at_ms: 20,
        wake_at_ms: 200,
    };

    store.set_goal_wake(&old).await?;
    store.set_goal_wake(&replacement).await?;

    assert!(!store.clear_goal_wake(&old).await?);
    assert_eq!(
        store.get_goal_wake(thread_id).await?,
        Some(replacement.clone())
    );
    assert!(store.clear_goal_wake(&replacement).await?);
    assert_eq!(store.get_goal_wake(thread_id).await?, None);
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
