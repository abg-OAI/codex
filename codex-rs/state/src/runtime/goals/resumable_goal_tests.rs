use crate::GoalAccountingMode;
use crate::GoalAccountingOutcome;
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

async fn create_stopped_goal(
    runtime: &StateRuntime,
    thread_id: ThreadId,
    status: ThreadGoalStatus,
) -> crate::ThreadGoal {
    runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "finish the release",
            status,
            /*token_budget*/ Some(10_000),
        )
        .await
        .expect("goal should be created")
}

#[tokio::test]
async fn resume_preserves_each_supported_stopped_goal() -> anyhow::Result<()> {
    for status in [
        ThreadGoalStatus::Paused,
        ThreadGoalStatus::Blocked,
        ThreadGoalStatus::UsageLimited,
    ] {
        let runtime = test_runtime().await;
        let thread_id = ThreadId::new();
        let original = create_stopped_goal(&runtime, thread_id, status).await;
        let accounted = runtime
            .thread_goals()
            .account_thread_goal_usage(
                thread_id,
                /*time_delta_seconds*/ 7,
                /*token_delta*/ 300,
                GoalAccountingMode::ActiveOrStopped,
                Some(&original.goal_id),
            )
            .await?;
        let GoalAccountingOutcome::Updated(original) = accounted else {
            panic!("stopped goal usage should be recorded");
        };

        let resumed = runtime
            .thread_goals()
            .resume_thread_goal(&original)
            .await?
            .expect("supported stopped goal should resume");

        let mut expected = original.clone();
        expected.status = ThreadGoalStatus::Active;
        expected.updated_at = resumed.updated_at;
        assert_eq!(resumed, expected);
        assert!(resumed.updated_at > original.updated_at);
    }

    Ok(())
}

#[tokio::test]
async fn resume_rejects_a_stale_goal_snapshot() -> anyhow::Result<()> {
    let runtime = test_runtime().await;
    let thread_id = ThreadId::new();
    let original = create_stopped_goal(&runtime, thread_id, ThreadGoalStatus::Paused).await;
    let edited = runtime
        .thread_goals()
        .update_thread_goal(
            thread_id,
            GoalUpdate {
                objective: Some("finish the release and announce it".to_string()),
                status: None,
                token_budget: None,
                expected_goal_id: Some(original.goal_id.clone()),
            },
        )
        .await?
        .expect("goal should still exist");

    assert!(
        runtime
            .thread_goals()
            .resume_thread_goal(&original)
            .await?
            .is_none()
    );
    assert_eq!(
        runtime.thread_goals().get_thread_goal(thread_id).await?,
        Some(edited)
    );

    Ok(())
}

#[tokio::test]
async fn resume_rejects_unsupported_or_budget_exhausted_goals() -> anyhow::Result<()> {
    for status in [
        ThreadGoalStatus::Active,
        ThreadGoalStatus::BudgetLimited,
        ThreadGoalStatus::Complete,
    ] {
        let runtime = test_runtime().await;
        let thread_id = ThreadId::new();
        let goal = runtime
            .thread_goals()
            .replace_thread_goal(
                thread_id,
                "finish the release",
                status,
                /*token_budget*/ None,
            )
            .await?;
        assert!(
            runtime
                .thread_goals()
                .resume_thread_goal(&goal)
                .await?
                .is_none()
        );
    }

    let runtime = test_runtime().await;
    let thread_id = ThreadId::new();
    let goal = runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "finish the release",
            ThreadGoalStatus::Paused,
            /*token_budget*/ Some(100),
        )
        .await?;
    let GoalAccountingOutcome::Updated(exhausted) = runtime
        .thread_goals()
        .account_thread_goal_usage(
            thread_id,
            /*time_delta_seconds*/ 0,
            /*token_delta*/ 100,
            GoalAccountingMode::ActiveOrStopped,
            Some(&goal.goal_id),
        )
        .await?
    else {
        panic!("stopped goal usage should be recorded");
    };
    assert_eq!(exhausted.status, ThreadGoalStatus::BudgetLimited);
    assert!(
        runtime
            .thread_goals()
            .resume_thread_goal(&exhausted)
            .await?
            .is_none()
    );

    Ok(())
}
