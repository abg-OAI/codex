//! Durable auxiliary state owned exclusively by Saffron.
//!
//! Saffron databases use their own filenames, schemas, and SQLx migration
//! ledgers. They must never attach to or modify a Codex-owned database. This
//! module owns the complete storage contract so upstream integration points
//! need only provide the configured SQLite home.

use std::sync::LazyLock;

use codex_protocol::ThreadId;
use codex_state::SqliteConfig;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use tokio::sync::watch;

const SAFFRON_DB_FILENAME: &str = "saffron_1.sqlite";
static SAFFRON_MIGRATOR: Migrator = sqlx::migrate!("./src/saffron/migrations");

// A process-local change signal lets the scheduler observe a snooze recorded
// by a helper without polling until the next reconciliation pass. Durability
// and cross-process recovery come from SQLite; this signal carries no state.
static WAKE_CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0_u64).0);

/// Exact durable identity of one deferred supervisor check-in.
///
/// Goal ID, objective, and revision timestamp form the revision identity. The
/// objective distinguishes edits that share a millisecond timestamp. Exact
/// conditional deletion prevents an obsolete timer from removing a newer
/// schedule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GoalWake {
    pub(super) thread_id: ThreadId,
    pub(super) goal_id: String,
    pub(super) goal_objective: String,
    pub(super) goal_updated_at_ms: i64,
    pub(super) wake_at_ms: i64,
}

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

    /// Subscribes to successful schedule mutations in this process.
    pub(super) fn subscribe() -> watch::Receiver<u64> {
        WAKE_CHANGED.subscribe()
    }

    /// Returns the persisted wake for `thread_id`, including stale identities.
    pub(super) async fn get_goal_wake(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<GoalWake>> {
        let row = sqlx::query(
            r#"
SELECT thread_id, goal_id, goal_objective, goal_updated_at_ms, wake_at_ms
FROM goal_supervisor_wakes
WHERE thread_id = ?
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(GoalWake {
                thread_id: ThreadId::from_string(row.try_get::<String, _>("thread_id")?.as_str())?,
                goal_id: row.try_get("goal_id")?,
                goal_objective: row.try_get("goal_objective")?,
                goal_updated_at_ms: row.try_get("goal_updated_at_ms")?,
                wake_at_ms: row.try_get("wake_at_ms")?,
            })
        })
        .transpose()
    }

    /// Replaces the wake for a thread and notifies process-local schedulers.
    pub(super) async fn set_goal_wake(&self, wake: &GoalWake) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO goal_supervisor_wakes (
    thread_id,
    goal_id,
    goal_objective,
    goal_updated_at_ms,
    wake_at_ms
) VALUES (?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO UPDATE SET
    goal_id = excluded.goal_id,
    goal_objective = excluded.goal_objective,
    goal_updated_at_ms = excluded.goal_updated_at_ms,
    wake_at_ms = excluded.wake_at_ms
            "#,
        )
        .bind(wake.thread_id.to_string())
        .bind(&wake.goal_id)
        .bind(&wake.goal_objective)
        .bind(wake.goal_updated_at_ms)
        .bind(wake.wake_at_ms)
        .execute(&self.pool)
        .await?;
        notify_wake_changed();
        Ok(())
    }

    /// Deletes `wake` only if no newer identity has replaced it.
    pub(super) async fn clear_goal_wake(&self, wake: &GoalWake) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
DELETE FROM goal_supervisor_wakes
WHERE thread_id = ?
  AND goal_id = ?
  AND goal_objective = ?
  AND goal_updated_at_ms = ?
  AND wake_at_ms = ?
            "#,
        )
        .bind(wake.thread_id.to_string())
        .bind(&wake.goal_id)
        .bind(&wake.goal_objective)
        .bind(wake.goal_updated_at_ms)
        .bind(wake.wake_at_ms)
        .execute(&self.pool)
        .await?;
        let cleared = result.rows_affected() != 0;
        if cleared {
            notify_wake_changed();
        }
        Ok(cleared)
    }
}

fn notify_wake_changed() {
    WAKE_CHANGED.send_modify(|generation| *generation = generation.wrapping_add(1));
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
