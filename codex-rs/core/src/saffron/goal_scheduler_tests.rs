use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::ThreadGoalStatus;
use codex_state::ThreadMetadataBuilder;
use codex_utils_absolute_path::test_support::PathExt;
use tokio::sync::Notify;

use super::*;
use crate::saffron::storage::GoalWake;

struct DropSignal(Arc<AtomicBool>);

struct ActiveGoalFixture {
    _home: tempfile::TempDir,
    state_db: Arc<StateRuntime>,
    store: SaffronStore,
    goal: codex_state::ThreadGoal,
}

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn active_goal_fixture(objective: &str) -> anyhow::Result<ActiveGoalFixture> {
    let home = tempfile::tempdir()?;
    let state_db = StateRuntime::init(
        SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await?;
    let thread_id = ThreadId::new();
    let metadata = ThreadMetadataBuilder::new(
        thread_id,
        home.path().join("rollout.jsonl"),
        Utc::now(),
        SessionSource::Cli,
    )
    .build("test-provider");
    state_db.upsert_thread(&metadata).await?;
    let goal = state_db
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            objective,
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    let store = SaffronStore::open(state_db.sqlite()).await?;
    Ok(ActiveGoalFixture {
        _home: home,
        state_db,
        store,
        goal,
    })
}

fn notifying_activator(activated: Arc<Notify>) -> GoalActivator {
    Arc::new(move |_| {
        let activated = Arc::clone(&activated);
        Box::pin(async move {
            activated.notify_one();
            Ok(())
        })
    })
}

#[tokio::test]
async fn ownership_shutdown_cancels_in_flight_activation_before_unlock() -> anyhow::Result<()> {
    let fixture = active_goal_fixture("keep working").await?;
    let state_db = Arc::clone(&fixture.state_db);

    let activation_started = Arc::new(Notify::new());
    let activation_dropped = Arc::new(AtomicBool::new(false));
    let activator: GoalActivator = Arc::new({
        let activation_started = Arc::clone(&activation_started);
        let activation_dropped = Arc::clone(&activation_dropped);
        move |_| {
            let activation_started = Arc::clone(&activation_started);
            let activation_dropped = Arc::clone(&activation_dropped);
            Box::pin(async move {
                let _drop_signal = DropSignal(activation_dropped);
                activation_started.notify_one();
                pending::<()>().await;
                Ok(())
            })
        }
    });
    let scheduler = GoalSchedulerHandle::start(Arc::clone(&state_db), activator);

    tokio::time::timeout(Duration::from_secs(5), activation_started.notified()).await?;
    drop(scheduler);

    let ownership = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ownership) = try_acquire_ownership(state_db.sqlite().home())? {
                break Ok::<_, io::Error>(ownership);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert!(activation_dropped.load(Ordering::SeqCst));
    drop(ownership);

    Ok(())
}

#[tokio::test]
async fn persisted_deadline_survives_restart_and_unrelated_wake_change() -> anyhow::Result<()> {
    let fixture = active_goal_fixture("wait despite another thread changing").await?;
    let state_db = Arc::clone(&fixture.state_db);
    let store = &fixture.store;
    let goal = &fixture.goal;
    let thread_id = goal.thread_id;
    store
        .set_goal_wake(&GoalWake {
            thread_id,
            goal_id: goal.goal_id.clone(),
            goal_objective: goal.objective.clone(),
            goal_updated_at_ms: goal.updated_at.timestamp_millis(),
            wake_at_ms: Utc::now().timestamp_millis() + 2_000,
        })
        .await?;
    let activated = Arc::new(Notify::new());
    let activator = notifying_activator(Arc::clone(&activated));
    let scheduler = GoalSchedulerHandle::start(Arc::clone(&state_db), activator);
    tokio::time::sleep(Duration::from_millis(100)).await;

    store
        .set_goal_wake(&GoalWake {
            thread_id: ThreadId::new(),
            goal_id: "unrelated-goal".to_string(),
            goal_objective: "unrelated objective".to_string(),
            goal_updated_at_ms: 1,
            wake_at_ms: Utc::now().timestamp_millis() + 60_000,
        })
        .await?;

    assert!(
        tokio::time::timeout(Duration::from_millis(250), activated.notified())
            .await
            .is_err(),
        "an unrelated thread's wake change bypassed the persisted deadline"
    );
    tokio::time::timeout(Duration::from_secs(5), activated.notified()).await?;
    drop(scheduler);
    Ok(())
}

#[tokio::test]
async fn queued_activation_rejects_stale_schedule_after_capacity_is_available() -> anyhow::Result<()>
{
    let fixture = active_goal_fixture("original objective").await?;
    let state_db = Arc::clone(&fixture.state_db);
    let store = &fixture.store;
    let original_goal = &fixture.goal;
    let thread_id = original_goal.thread_id;
    let original_schedule = GoalSchedule::from_goal(original_goal, store)
        .await
        .map_err(anyhow::Error::msg)?;
    let permits = Arc::new(Semaphore::new(1));
    let held_permit = Arc::clone(&permits).acquire_owned().await?;
    let (activated_tx, mut activated_rx) = tokio::sync::mpsc::unbounded_channel();
    let activator: GoalActivator = Arc::new(move |schedule| {
        let activated_tx = activated_tx.clone();
        Box::pin(async move {
            activated_tx
                .send(schedule)
                .map_err(|_| "activation receiver closed".to_string())
        })
    });
    let mut activation = tokio::spawn(run_activation(
        Arc::clone(&state_db),
        store.clone(),
        activator,
        Arc::clone(&permits),
        original_schedule,
        CancellationToken::new(),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut activation)
            .await
            .is_err(),
        "activation settled while all capacity was held"
    );

    state_db
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "replacement objective",
            ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    drop(held_permit);

    tokio::time::timeout(Duration::from_secs(5), activation).await??;
    assert!(
        activated_rx.try_recv().is_err(),
        "queued activation used the stale pre-capacity schedule"
    );
    Ok(())
}

#[tokio::test]
async fn stale_goal_revision_does_not_inherit_persisted_deadline() -> anyhow::Result<()> {
    let fixture = active_goal_fixture("changed after snooze").await?;
    let state_db = Arc::clone(&fixture.state_db);
    let store = &fixture.store;
    let goal = &fixture.goal;
    let thread_id = goal.thread_id;
    let mismatching_wake = GoalWake {
        thread_id,
        goal_id: goal.goal_id.clone(),
        goal_objective: "newer objective in another database".to_string(),
        goal_updated_at_ms: goal.updated_at.timestamp_millis(),
        wake_at_ms: Utc::now().timestamp_millis() + 60_000,
    };
    store.set_goal_wake(&mismatching_wake).await?;
    let activated = Arc::new(Notify::new());
    let activator = notifying_activator(Arc::clone(&activated));
    let scheduler = GoalSchedulerHandle::start(Arc::clone(&state_db), activator);

    tokio::time::timeout(Duration::from_secs(5), activated.notified()).await?;
    assert_eq!(
        store.get_goal_wake(thread_id).await?,
        Some(mismatching_wake)
    );
    drop(scheduler);
    Ok(())
}
