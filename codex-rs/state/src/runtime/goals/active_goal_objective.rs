//! Conditional objective updates for active goals.
//!
//! Goal supervisors act on a snapshot captured before their model turn begins.
//! This module keeps that snapshot from becoming overwrite authority: an edit
//! commits only while the same goal, objective, revision, and active status are
//! still stored. The update changes only the objective and revision timestamp.

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;

use super::GoalStore;
use super::datetime_to_epoch_millis;
use super::thread_goal_from_row;

/// Identifies the goal snapshot against which an objective edit was chosen.
///
/// The stored objective participates in the comparison because goal timestamps
/// have millisecond resolution. It prevents a same-millisecond user edit from
/// being overwritten even if the timestamp alone cannot distinguish it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadGoalRevision {
    goal_id: String,
    objective: String,
    updated_at: DateTime<Utc>,
}

impl ThreadGoalRevision {
    /// Captures the fields needed to reject a stale objective edit.
    pub fn capture(goal: &crate::ThreadGoal) -> Self {
        Self {
            goal_id: goal.goal_id.clone(),
            objective: goal.objective.clone(),
            updated_at: goal.updated_at,
        }
    }

    /// Returns the durable identity of the captured goal.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }
}

impl GoalStore {
    /// Replaces an active goal's objective if `expected` is still current.
    ///
    /// A missing result means the goal was removed, replaced, made inactive,
    /// or changed after the revision was captured. In that case the database
    /// is left unchanged. A successful update preserves every field other than
    /// the objective and `updated_at` timestamp.
    pub async fn update_active_thread_goal_objective(
        &self,
        thread_id: ThreadId,
        expected: &ThreadGoalRevision,
        objective: &str,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let row = sqlx::query(
            r#"
UPDATE thread_goals
SET
    objective = ?,
    updated_at_ms = MAX(?, updated_at_ms + 1)
WHERE thread_id = ?
  AND goal_id = ?
  AND objective = ?
  AND updated_at_ms = ?
  AND status = 'active'
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
        .bind(objective)
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(&expected.goal_id)
        .bind(&expected.objective)
        .bind(datetime_to_epoch_millis(expected.updated_at))
        .fetch_optional(self.pool.as_ref())
        .await?;

        row.map(|row| thread_goal_from_row(&row)).transpose()
    }
}

#[cfg(test)]
#[path = "active_goal_objective_tests.rs"]
mod tests;
