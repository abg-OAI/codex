use super::*;

use crate::GoalUpdate;
use crate::StateRuntime;
use crate::ThreadGoalStatus;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

async fn test_runtime() -> std::sync::Arc<StateRuntime> {
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(unique_temp_dir().as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state db should initialize")
}

async fn create_active_goal(
    runtime: &StateRuntime,
    thread_id: ThreadId,
    objective: &str,
) -> crate::ThreadGoal {
    let token_budget = None;
    runtime
        .thread_goals()
        .replace_thread_goal(thread_id, objective, ThreadGoalStatus::Active, token_budget)
        .await
        .expect("goal should be created")
}

#[tokio::test]
async fn objective_update_preserves_the_active_goal() {
    let runtime = test_runtime().await;
    let thread_id = ThreadId::new();
    let original = runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "ship the first version",
            ThreadGoalStatus::Active,
            /*token_budget*/ Some(10_000),
        )
        .await
        .expect("goal should be created");
    let revision = ThreadGoalRevision::capture(&original);

    let updated = runtime
        .thread_goals()
        .update_active_thread_goal_objective(
            thread_id,
            &revision,
            "ship the first version with release notes",
        )
        .await
        .expect("objective update should succeed")
        .expect("captured goal should still be active");

    let mut expected = original.clone();
    expected.objective = "ship the first version with release notes".to_string();
    expected.updated_at = updated.updated_at;
    assert_eq!(updated, expected);
    assert!(updated.updated_at > original.updated_at);
}

#[tokio::test]
async fn objective_update_rejects_stale_goal_state() -> anyhow::Result<()> {
    let runtime = test_runtime().await;
    let thread_id = ThreadId::new();
    let original = create_active_goal(&runtime, thread_id, "original objective").await;
    let revision = ThreadGoalRevision::capture(&original);
    let user_goal = runtime
        .thread_goals()
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: Some("user-edited objective".to_string()),
                status: None,
                token_budget: None,
                expected_goal_id: Some(original.goal_id),
            },
        )
        .await?
        .expect("goal should still exist");
    assert!(
        runtime
            .thread_goals()
            .update_active_thread_goal_objective(
                thread_id,
                &revision,
                "stale supervisor objective",
            )
            .await?
            .is_none()
    );

    let revision = ThreadGoalRevision::capture(&user_goal);
    runtime
        .thread_goals()
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: None,
                status: Some(ThreadGoalStatus::Paused),
                token_budget: None,
                expected_goal_id: Some(user_goal.goal_id),
            },
        )
        .await?
        .expect("goal should be paused");
    assert!(
        runtime
            .thread_goals()
            .update_active_thread_goal_objective(thread_id, &revision, "ignore the pause")
            .await?
            .is_none()
    );
    let replaced = create_active_goal(&runtime, thread_id, "replacement objective").await;
    let revision = ThreadGoalRevision::capture(&replaced);
    let replacement = create_active_goal(&runtime, thread_id, "new replacement objective").await;
    assert!(
        runtime
            .thread_goals()
            .update_active_thread_goal_objective(
                thread_id,
                &revision,
                "stale supervisor objective",
            )
            .await?
            .is_none()
    );
    assert_eq!(
        runtime.thread_goals().get_thread_goal(thread_id).await?,
        Some(replacement)
    );

    Ok(())
}
