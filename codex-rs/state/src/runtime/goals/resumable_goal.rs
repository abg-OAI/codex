//! Conditional transitions from a stopped goal back to active work.
//!
//! A resume decision is made from a complete durable snapshot. This module
//! applies that decision only while the same snapshot remains stored, so a
//! concurrent user edit, status transition, accounting update, or replacement
//! takes precedence over the stale decision.

use chrono::Utc;

use super::GoalStore;
use super::datetime_to_epoch_millis;
use super::thread_goal_from_row;

impl GoalStore {
    /// Resumes `expected` if the complete goal snapshot is still current.
    ///
    /// A missing result means the goal was removed, replaced, changed, is not
    /// in a resumable status, or has exhausted its token budget. A successful
    /// update preserves every field other than status and `updated_at`.
    pub async fn resume_thread_goal(
        &self,
        expected: &crate::ThreadGoal,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let row = sqlx::query(
            r#"
UPDATE thread_goals
SET
    status = 'active',
    updated_at_ms = MAX(?, updated_at_ms + 1)
WHERE thread_id = ?
  AND goal_id = ?
  AND objective = ?
  AND status = ?
  AND token_budget IS ?
  AND tokens_used = ?
  AND time_used_seconds = ?
  AND created_at_ms = ?
  AND updated_at_ms = ?
  AND status IN ('paused', 'blocked', 'usage_limited')
  AND (token_budget IS NULL OR tokens_used < token_budget)
RETURNING
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
            "#,
        )
        .bind(now_ms)
        .bind(expected.thread_id.to_string())
        .bind(&expected.goal_id)
        .bind(&expected.objective)
        .bind(expected.status.as_str())
        .bind(expected.token_budget)
        .bind(expected.tokens_used)
        .bind(expected.time_used_seconds)
        .bind(datetime_to_epoch_millis(expected.created_at))
        .bind(datetime_to_epoch_millis(expected.updated_at))
        .fetch_optional(self.pool.as_ref())
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }
}

#[cfg(test)]
#[path = "resumable_goal_tests.rs"]
mod tests;
