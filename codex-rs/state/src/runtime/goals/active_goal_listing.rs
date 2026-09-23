//! Bounded discovery of durable active goals.
//!
//! The store owns status filtering and stable pagination. Callers retain
//! policy decisions that require thread metadata or process-local state.

use codex_protocol::ThreadId;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

use super::GoalStore;
use super::thread_goal_from_row;

impl GoalStore {
    /// Lists active goals in ascending thread-id order.
    ///
    /// `after_thread_id` is an exclusive cursor. The caller owns pagination
    /// and any additional eligibility rules that depend on thread metadata.
    pub async fn list_active_thread_goals(
        &self,
        after_thread_id: Option<ThreadId>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::ThreadGoal>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
WHERE status = 'active'
            "#,
        );
        if let Some(after_thread_id) = after_thread_id {
            builder
                .push(" AND thread_id > ")
                .push_bind(after_thread_id.to_string());
        }
        builder
            .push(" ORDER BY thread_id ASC LIMIT ")
            .push_bind(i64::try_from(limit).unwrap_or(i64::MAX));

        builder
            .build()
            .fetch_all(self.pool.as_ref())
            .await?
            .iter()
            .map(thread_goal_from_row)
            .collect()
    }
}
