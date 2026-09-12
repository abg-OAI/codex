use super::*;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::session::tests::make_session_and_context;

#[tokio::test]
async fn passive_agent_mail_does_not_suppress_a_supervisor_spawn_attempt() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    let mail = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "worker finished".to_string(),
        /*trigger_turn*/ false,
    );
    parent
        .input_queue
        .enqueue_mailbox_communication(mail.clone(), Default::default())
        .await;
    let goal = test_goal(parent.thread_id);

    assert!(
        start_checkin(&parent, &goal)
            .await
            .expect_err("fixture has no durable goal store")
            .contains("retry could not be scheduled")
    );

    assert_eq!(runtime(&parent).state.lock().await.consecutive_failures, 1);
    assert_eq!(
        parent.input_queue.drain_mailbox_input_items().await.0,
        vec![crate::session::TurnInput::InterAgentCommunication(mail)]
    );
}

#[tokio::test]
async fn triggering_agent_mail_owns_continuation_before_a_supervisor_starts() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    parent
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path"),
                AgentPath::root(),
                Vec::new(),
                "continue now".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;

    assert_eq!(
        start_checkin(&parent, &test_goal(parent.thread_id))
            .await
            .expect("start check-in"),
        CheckinStart::Deferred {
            owner: ContinuationOwner::TriggeringMailbox,
        }
    );
    assert_eq!(runtime(&parent).state.lock().await.consecutive_failures, 0);
}

#[tokio::test]
async fn active_root_turn_owns_continuation_before_a_supervisor_starts() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    *parent.active_turn.lock().await = Some(crate::state::ActiveTurn::default());

    assert_eq!(
        start_checkin(&parent, &test_goal(parent.thread_id))
            .await
            .expect("start check-in"),
        CheckinStart::Deferred {
            owner: ContinuationOwner::RootTurn,
        }
    );
    assert_eq!(runtime(&parent).state.lock().await.consecutive_failures, 0);
}

#[test]
fn a_new_continuation_owner_revokes_an_older_permit() {
    let runtime = Runtime::default();
    let permit = runtime.claim_continuation();

    assert!(runtime.owns_continuation(permit));
    runtime.supersede_continuation();
    assert!(!runtime.owns_continuation(permit));
}

#[tokio::test]
async fn root_work_revokes_a_supervisor_spawn_in_flight() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    let runtime = runtime(&parent);
    let continuation = runtime.claim_continuation();
    runtime.state.lock().await.starting = Some(continuation);

    claim_root_continuation(&parent, ContinuationOwner::RootTurn).await;

    assert!(!runtime.owns_continuation(continuation));
    assert_eq!(runtime.state.lock().await.starting, None);
}

#[tokio::test]
async fn root_work_retires_an_uncommitted_supervisor_owner() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    let runtime = runtime(&parent);
    let continuation = runtime.claim_continuation();
    let helper_id = ThreadId::new();
    runtime.state.lock().await.active = Some(ActiveHelper {
        thread_id: helper_id,
        continuation,
        goal_revision: codex_state::ThreadGoalRevision::capture(&test_goal(parent.thread_id)),
        edit_state: GoalEditState::Available,
        action: None,
    });

    claim_root_continuation(&parent, ContinuationOwner::RootTurn).await;

    assert!(!runtime.owns_continuation(continuation));
    assert!(runtime.state.lock().await.active.is_none());
    assert_eq!(
        select_action(&parent, helper_id, Action::Complete)
            .await
            .expect_err("retired helper must not select an action"),
        "this supervisor helper is no longer active"
    );
}

#[tokio::test]
async fn passive_mail_waking_a_sleeping_root_retires_the_supervisor_owner() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    parent
        .services
        .thread_extension_data
        .insert(codex_extension_items::sleep::SleepItem {
            id: "sleeping-root".to_string(),
            duration_ms: 60_000,
        });
    parent
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path"),
                AgentPath::root(),
                Vec::new(),
                "worker finished".to_string(),
                /*trigger_turn*/ false,
            ),
            Default::default(),
        )
        .await;
    let runtime = runtime(&parent);
    let continuation = runtime.claim_continuation();
    runtime.state.lock().await.active = Some(ActiveHelper {
        thread_id: ThreadId::new(),
        continuation,
        goal_revision: codex_state::ThreadGoalRevision::capture(&test_goal(parent.thread_id)),
        edit_state: GoalEditState::Available,
        action: None,
    });

    parent.maybe_start_turn_for_pending_work().await;

    assert!(!runtime.owns_continuation(continuation));
    assert!(runtime.state.lock().await.active.is_none());
    parent
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Replaced)
        .await;
}

#[tokio::test]
async fn superseded_helper_cannot_select_an_action_after_finishing_an_edit() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    let runtime = runtime(&parent);
    let continuation = runtime.claim_continuation();
    let helper_id = ThreadId::new();
    runtime.state.lock().await.active = Some(ActiveHelper {
        thread_id: helper_id,
        continuation,
        goal_revision: codex_state::ThreadGoalRevision::capture(&test_goal(parent.thread_id)),
        edit_state: GoalEditState::InFlight,
        action: None,
    });

    claim_root_continuation(&parent, ContinuationOwner::RootTurn).await;
    commit_goal_edit(&parent, helper_id).await;

    assert_eq!(
        select_action(&parent, helper_id, Action::Complete)
            .await
            .expect_err("superseded helper must not select an action"),
        "the parent claimed continuation before this action was selected"
    );
}

#[tokio::test(start_paused = true)]
async fn due_wake_remains_until_idle_continuation_establishes_an_owner() {
    let (session, _) = make_session_and_context().await;
    let parent = Arc::new(session);
    let runtime = runtime(&parent);
    let snooze = Snooze {
        wake: GoalWake {
            thread_id: parent.thread_id,
            goal_id: "goal-id".to_string(),
            goal_objective: "finish the release".to_string(),
            goal_updated_at_ms: 1,
            wake_at_ms: 2,
        },
        deadline: Instant::now() + Duration::from_secs(60),
        idle_retention: IdleRetention::Reconstructible,
    };
    runtime.state.lock().await.snooze = Some(snooze.clone());

    schedule_wake(&parent, &runtime, snooze);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;

    assert!(runtime.state.lock().await.snooze.is_some());
}

#[tokio::test]
async fn persisted_due_wake_is_settled_only_after_ownership_transfer() {
    let sqlite_home = tempfile::tempdir().expect("SQLite home");
    let state_db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(sqlite_home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime");
    let (mut session, _) = make_session_and_context().await;
    session.services.state_db = Some(Arc::clone(&state_db));
    let parent = Arc::new(session);
    let goal = test_goal(parent.thread_id);
    let wake = GoalWake {
        thread_id: parent.thread_id,
        goal_id: goal.goal_id.clone(),
        goal_objective: goal.objective.clone(),
        goal_updated_at_ms: goal.updated_at.timestamp_millis(),
        wake_at_ms: Utc::now().timestamp_millis().saturating_sub(1),
    };
    let store = SaffronStore::open(state_db.sqlite())
        .await
        .expect("Saffron store");
    store.set_goal_wake(&wake).await.expect("persist wake");

    let restored = restore_persisted_snooze(&parent, &goal)
        .await
        .expect("restore wake")
        .expect("matching wake");

    assert!(restored.deadline <= Instant::now());
    assert_eq!(
        store
            .get_goal_wake(parent.thread_id)
            .await
            .expect("read wake"),
        Some(wake.clone())
    );

    let supervisor_runtime = runtime(&parent);
    let stale = supervisor_runtime.claim_continuation();
    supervisor_runtime.supersede_continuation();
    settle_persisted_wake(&parent, &goal, &supervisor_runtime, stale).await;
    assert_eq!(
        store
            .get_goal_wake(parent.thread_id)
            .await
            .expect("read wake after stale settlement"),
        Some(wake)
    );

    let current = supervisor_runtime.claim_continuation();
    settle_persisted_wake(&parent, &goal, &supervisor_runtime, current).await;

    assert_eq!(
        store
            .get_goal_wake(parent.thread_id)
            .await
            .expect("read settled wake"),
        None
    );
}

#[tokio::test]
async fn due_wake_with_passive_mail_becomes_a_durable_retry_after_spawn_failure() {
    let sqlite_home = tempfile::tempdir().expect("SQLite home");
    let state_db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(sqlite_home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime");
    let (mut session, _) = make_session_and_context().await;
    session.services.state_db = Some(Arc::clone(&state_db));
    let parent = Arc::new(session);
    let goal = state_db
        .thread_goals()
        .replace_thread_goal(
            parent.thread_id,
            "finish the release",
            codex_state::ThreadGoalStatus::Active,
            None,
        )
        .await
        .expect("active goal");
    let due_wake = GoalWake {
        thread_id: parent.thread_id,
        goal_id: goal.goal_id.clone(),
        goal_objective: goal.objective.clone(),
        goal_updated_at_ms: goal.updated_at.timestamp_millis(),
        wake_at_ms: Utc::now().timestamp_millis().saturating_sub(1),
    };
    let store = SaffronStore::open(state_db.sqlite())
        .await
        .expect("Saffron store");
    store
        .set_goal_wake(&due_wake)
        .await
        .expect("persist due wake");
    parent
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path"),
                AgentPath::root(),
                Vec::new(),
                "worker finished".to_string(),
                /*trigger_turn*/ false,
            ),
            Default::default(),
        )
        .await;

    assert_eq!(
        start_checkin(&parent, &goal).await.expect("start check-in"),
        CheckinStart::RetryScheduled
    );

    let retry = store
        .get_goal_wake(parent.thread_id)
        .await
        .expect("read retry")
        .expect("durable retry");
    assert!(retry.wake_at_ms > due_wake.wake_at_ms);
    assert!(parent.input_queue.has_pending_mailbox_items().await);
}

fn test_goal(thread_id: ThreadId) -> codex_state::ThreadGoal {
    let now = Utc::now();
    codex_state::ThreadGoal {
        thread_id,
        goal_id: "goal-id".to_string(),
        objective: "finish the release".to_string(),
        status: codex_state::ThreadGoalStatus::Active,
        token_budget: None,
        tokens_used: 0,
        time_used_seconds: 0,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn failure_retry_is_exponential_and_capped() {
    assert_eq!(failure_retry_delay(1), Duration::from_secs(60));
    assert_eq!(failure_retry_delay(2), Duration::from_secs(120));
    assert_eq!(failure_retry_delay(7), MAX_FAILURE_RETRY);
    assert_eq!(failure_retry_delay(u32::MAX), MAX_FAILURE_RETRY);
}

#[test]
fn only_process_local_wake_requires_idle_retention() {
    let mut state = State::default();
    assert_eq!(state.idle_disposition(), IdleDisposition::Quiescent);

    state.snooze = Some(Snooze {
        wake: GoalWake {
            thread_id: ThreadId::new(),
            goal_id: "goal-id".to_string(),
            goal_objective: "goal objective".to_string(),
            goal_updated_at_ms: 1,
            wake_at_ms: 2,
        },
        deadline: Instant::now() + Duration::from_secs(30 * 24 * 60 * 60),
        idle_retention: IdleRetention::Required,
    });
    assert_eq!(state.idle_disposition(), IdleDisposition::ProcessLocalWork);

    state.snooze.as_mut().expect("snooze").idle_retention = IdleRetention::Reconstructible;
    assert_eq!(
        state.idle_disposition(),
        IdleDisposition::ReconstructibleSnooze
    );
}

#[tokio::test]
async fn idle_disposition_waits_for_transition_to_settle() {
    let runtime = Arc::new(Runtime::default());
    let transition = Arc::clone(&runtime.transition).lock_owned().await;
    assert_eq!(
        runtime.state.lock().await.idle_disposition(),
        IdleDisposition::Quiescent
    );
    let (observation_started_tx, observation_started_rx) = tokio::sync::oneshot::channel();
    let observer = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        async move {
            let _ = observation_started_tx.send(());
            runtime.idle_disposition().await
        }
    });

    observation_started_rx.await.expect("observer started");
    tokio::task::yield_now().await;
    assert!(!observer.is_finished());

    runtime.state.lock().await.snooze = Some(Snooze {
        wake: GoalWake {
            thread_id: ThreadId::new(),
            goal_id: "goal-id".to_string(),
            goal_objective: "goal objective".to_string(),
            goal_updated_at_ms: 1,
            wake_at_ms: 2,
        },
        deadline: Instant::now() + Duration::from_secs(60),
        idle_retention: IdleRetention::Required,
    });
    drop(transition);

    let disposition = tokio::time::timeout(Duration::from_secs(5), observer)
        .await
        .expect("observer timed out")
        .expect("observer completed");
    assert_eq!(disposition, IdleDisposition::ProcessLocalWork);
}

#[test]
fn each_checkin_prompt_includes_its_current_time() {
    let parent_id =
        ThreadId::from_string("018f0000-0000-7000-8000-000000000001").expect("thread id");
    let objective = "Release when CI becomes green.";
    let continuity =
        r#"{"goal_updated_at":1787288448,"previous_action":{"snooze":{"delay_seconds":1477}}}"#;
    let first_checkin_time: DateTime<Utc> = "2026-08-21T05:00:48Z".parse().expect("UTC time");
    let next_checkin_time: DateTime<Utc> = "2026-08-21T05:25:25Z".parse().expect("UTC time");

    let prompts = [
        render_checkin_prompt(parent_id, first_checkin_time, objective, continuity),
        render_checkin_prompt(parent_id, next_checkin_time, objective, continuity),
    ];

    assert_eq!(
        prompts,
        [
            "# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:00:48 UTC\n\nParent thread: 018f0000-0000-7000-8000-000000000001\n\nActive goal:\nRelease when CI becomes green.\n\nContinuity:\n{\"goal_updated_at\":1787288448,\"previous_action\":{\"snooze\":{\"delay_seconds\":1477}}}"
                .to_string(),
            "# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:25:25 UTC\n\nParent thread: 018f0000-0000-7000-8000-000000000001\n\nActive goal:\nRelease when CI becomes green.\n\nContinuity:\n{\"goal_updated_at\":1787288448,\"previous_action\":{\"snooze\":{\"delay_seconds\":1477}}}"
                .to_string(),
        ]
    );
}

#[test]
fn checkin_prompt_bounds_large_goal_objectives_without_losing_current_time() {
    let objective = "long objective ".repeat(2_000);
    let checkin_time: DateTime<Utc> = "2026-08-21T05:25:25Z".parse().expect("UTC time");

    let prompt = render_checkin_prompt(ThreadId::new(), checkin_time, &objective, "{}");

    assert!(
        prompt.starts_with("# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:25:25 UTC")
    );
    assert!(prompt.len() < objective.len());
}
