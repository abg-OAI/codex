//! Runtime ownership and lifecycle for one root thread's supervisor helper.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use codex_extension_api::ThreadIdleCause;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TruncationPolicy;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::truncate_text;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::warn;

use super::HELPER_ROLE_NAME;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::next_thread_spawn_depth;
use crate::saffron::storage::GoalWake;
use crate::saffron::storage::SaffronStore;
use crate::session::session::Session;

const INITIAL_FAILURE_RETRY: Duration = Duration::from_secs(60);
const MAX_FAILURE_RETRY: Duration = Duration::from_secs(60 * 60);
const MAX_CHECKIN_PROMPT_TOKENS: usize = 1_000;

/// Ephemeral state attached to the supervised parent thread.
pub(super) struct Runtime {
    transition: Arc<Mutex<()>>,
    state: Mutex<State>,
    wake_generation: AtomicU64,
}

#[derive(Default)]
struct State {
    goal_id: Option<String>,
    active: Option<ActiveHelper>,
    snooze: Option<Snooze>,
    consecutive_failures: u32,
    previous_action: Option<Action>,
}

struct ActiveHelper {
    thread_id: ThreadId,
    goal_revision: codex_state::ThreadGoalRevision,
    edit_state: GoalEditState,
    action: Option<Action>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GoalEditState {
    #[default]
    Available,
    InFlight,
    Committed,
}

#[derive(Clone)]
pub(super) struct Snooze {
    wake: GoalWake,
    pub(super) deadline: Instant,
}

impl Snooze {
    fn for_goal(goal: &codex_state::ThreadGoal, delay: Duration) -> Self {
        let delay_millis = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
        Self {
            wake: GoalWake {
                thread_id: goal.thread_id,
                goal_id: goal.goal_id.clone(),
                goal_objective: goal.objective.clone(),
                goal_updated_at_ms: goal.updated_at.timestamp_millis(),
                wake_at_ms: Utc::now().timestamp_millis().saturating_add(delay_millis),
            },
            deadline: Instant::now() + delay,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Action {
    Followup,
    Snooze { delay_seconds: u64 },
    Compact,
    Complete,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            transition: Arc::new(Mutex::new(())),
            state: Mutex::new(State::default()),
            wake_generation: AtomicU64::new(0),
        }
    }
}

/// Replaces normal root-goal continuation with one supervisor check-in.
///
/// `Ok(false)` means this is not a root thread and the goal extension should
/// retain its normal continuation behavior. `Ok(true)` means Saffron owns this
/// idle transition, including any deferred retry.
pub(crate) async fn start_checkin(
    parent: &Arc<Session>,
    goal: &codex_state::ThreadGoal,
) -> Result<bool, String> {
    let goal_id = goal.goal_id.as_str();
    let parent_source = parent.session_source().await;
    if matches!(parent_source, SessionSource::SubAgent(_)) {
        return Ok(false);
    }

    let runtime = runtime(parent);
    let _transition = Arc::clone(&runtime.transition).lock_owned().await;

    if parent.active_turn.lock().await.is_some()
        || parent
            .input_queue
            .has_pending_input(&parent.active_turn)
            .await
    {
        return Ok(true);
    }

    let now = Instant::now();
    let active = runtime
        .state
        .lock()
        .await
        .active
        .as_ref()
        .map(|active| (active.thread_id, active.goal_revision.goal_id().to_string()));
    if let Some((helper_id, active_goal_id)) = active {
        if active_goal_id == goal_id
            && matches!(
                parent.services.agent_control.get_status(helper_id).await,
                AgentStatus::PendingInit | AgentStatus::Running
            )
        {
            return Ok(true);
        }
        let _ = parent
            .services
            .agent_control
            .shutdown_live_agent(helper_id)
            .await;
    }
    {
        let mut state = runtime.state.lock().await;
        if state.goal_id.as_deref() != Some(goal_id) {
            state.goal_id = Some(goal_id.to_string());
            state.snooze = None;
            state.consecutive_failures = 0;
            state.previous_action = None;
        }
        if let Some(snooze) = state.snooze.as_ref()
            && snooze.wake.goal_id == goal_id
            && snooze.wake.goal_updated_at_ms == goal.updated_at.timestamp_millis()
            && snooze.wake.goal_objective == goal.objective
            && snooze.deadline > now
        {
            schedule_wake(parent, &runtime, snooze.clone());
            return Ok(true);
        }
        state.active = None;
        if state.snooze.take().is_some() {
            runtime.wake_generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    match spawn_helper(parent, goal).await {
        Ok(helper_id) => {
            runtime.state.lock().await.active = Some(ActiveHelper {
                thread_id: helper_id,
                goal_revision: codex_state::ThreadGoalRevision::capture(goal),
                edit_state: GoalEditState::Available,
                action: None,
            });
            watch_helper(Arc::downgrade(parent), helper_id, goal_id.to_string());
        }
        Err(error) => {
            warn!(thread_id = %parent.thread_id, "failed to spawn Saffron goal supervisor: {error}");
            defer_failure(parent, &runtime, goal_id, error).await;
        }
    }
    Ok(true)
}

/// Cancels process-local supervision when no active goal remains.
pub(crate) async fn stop(parent: &Arc<Session>) {
    let runtime = runtime(parent);
    let _transition = Arc::clone(&runtime.transition).lock_owned().await;
    runtime.wake_generation.fetch_add(1, Ordering::AcqRel);
    let helper_id = {
        let mut state = runtime.state.lock().await;
        state.goal_id = None;
        state.snooze = None;
        state.consecutive_failures = 0;
        state.previous_action = None;
        state.active.take().map(|active| active.thread_id)
    };
    if let Some(helper_id) = helper_id
        && let Err(error) = parent
            .services
            .agent_control
            .shutdown_live_agent(helper_id)
            .await
    {
        warn!(%helper_id, "failed to stop Saffron goal supervisor: {error}");
    }
}

pub(in crate::saffron) async fn parent_for_helper(
    helper: &Session,
) -> Result<Arc<Session>, String> {
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        agent_role: Some(role),
        ..
    }) = helper.session_source().await
    else {
        return Err(
            "supervisor tools are available only in a Saffron supervisor helper".to_string(),
        );
    };
    if role != HELPER_ROLE_NAME {
        return Err(
            "supervisor tools are available only in a Saffron supervisor helper".to_string(),
        );
    }
    helper
        .services
        .agent_control
        .get_live_thread(parent_thread_id)
        .await
        .map(|thread| Arc::clone(&thread.session))
        .map_err(|error| error.to_string())
}

pub(super) async fn select_action(
    parent: &Arc<Session>,
    helper_id: ThreadId,
    action: Action,
) -> Result<String, String> {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    let Some(active) = state
        .active
        .as_mut()
        .filter(|active| active.thread_id == helper_id)
    else {
        return Err("this supervisor helper is no longer active".to_string());
    };
    if active.action.is_some() {
        return Err("a supervisor action was already selected for this check-in".to_string());
    }
    if active.edit_state == GoalEditState::InFlight {
        return Err("the supervisor goal edit is still in progress".to_string());
    }
    active.action = Some(action.clone());
    let goal_id = active.goal_revision.goal_id().to_string();
    Ok(goal_id)
}

/// Reserves the one optional goal edit allowed before a disposition action.
pub(in crate::saffron) async fn begin_goal_edit(
    parent: &Arc<Session>,
    helper_id: ThreadId,
) -> Result<codex_state::ThreadGoalRevision, String> {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    let Some(active) = state
        .active
        .as_mut()
        .filter(|active| active.thread_id == helper_id)
    else {
        return Err("this supervisor helper is no longer active".to_string());
    };
    if active.action.is_some() {
        return Err("the supervisor disposition was already selected".to_string());
    }
    match active.edit_state {
        GoalEditState::Available => {
            active.edit_state = GoalEditState::InFlight;
            Ok(active.goal_revision.clone())
        }
        GoalEditState::InFlight => Err("a supervisor goal edit is already in progress".to_string()),
        GoalEditState::Committed => {
            Err("the active goal was already edited during this check-in".to_string())
        }
    }
}

/// Commits the helper's edit reservation without consuming its disposition.
pub(in crate::saffron) async fn commit_goal_edit(parent: &Session, helper_id: ThreadId) {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    if let Some(active) = state
        .active
        .as_mut()
        .filter(|active| active.thread_id == helper_id)
        && active.edit_state == GoalEditState::InFlight
    {
        active.edit_state = GoalEditState::Committed;
    }
}

/// Releases an edit reservation when validation or persistence fails.
pub(in crate::saffron) async fn clear_failed_goal_edit(parent: &Session, helper_id: ThreadId) {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    if let Some(active) = state
        .active
        .as_mut()
        .filter(|active| active.thread_id == helper_id)
        && active.edit_state == GoalEditState::InFlight
    {
        active.edit_state = GoalEditState::Available;
    }
}

pub(super) async fn commit_action(parent: &Session, action: Action) {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    state.previous_action = Some(action);
    state.consecutive_failures = 0;
}

pub(super) async fn clear_failed_action(parent: &Session, helper_id: ThreadId, action: &Action) {
    let runtime = runtime(parent);
    let mut state = runtime.state.lock().await;
    if let Some(active) = state
        .active
        .as_mut()
        .filter(|active| active.thread_id == helper_id)
        && active.action.as_ref() == Some(action)
    {
        active.action = None;
    }
}

pub(super) async fn set_snooze(parent: &Session, snooze: Snooze) -> Result<(), String> {
    let Some(state_db) = parent.services.state_db.as_ref() else {
        return Err("goal state is unavailable".to_string());
    };
    SaffronStore::open(state_db.sqlite())
        .await
        .map_err(|error| error.to_string())?
        .set_goal_wake(&snooze.wake)
        .await
        .map_err(|error| error.to_string())?;
    runtime(parent).state.lock().await.snooze = Some(snooze);
    Ok(())
}

pub(super) async fn snooze_for_active_goal(
    parent: &Session,
    goal_id: &str,
    delay: Duration,
) -> Result<Snooze, String> {
    let Some(state_db) = parent.services.state_db.as_ref() else {
        return Err("goal state is unavailable".to_string());
    };
    let goal = state_db
        .thread_goals()
        .get_thread_goal(parent.thread_id)
        .await
        .map_err(|error| error.to_string())?
        .filter(|goal| {
            goal.goal_id == goal_id && goal.status == codex_state::ThreadGoalStatus::Active
        })
        .ok_or_else(|| "the active goal changed before it could be snoozed".to_string())?;
    Ok(Snooze::for_goal(&goal, delay))
}

pub(super) async fn clear_snooze_for_goal(parent: &Session, goal_id: &str) -> Result<(), String> {
    let Some(state_db) = parent.services.state_db.as_ref() else {
        return Ok(());
    };
    let store = SaffronStore::open(state_db.sqlite())
        .await
        .map_err(|error| error.to_string())?;
    if let Some(wake) = store
        .get_goal_wake(parent.thread_id)
        .await
        .map_err(|error| error.to_string())?
        .filter(|wake| wake.goal_id == goal_id)
    {
        store
            .clear_goal_wake(&wake)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(super) fn runtime(parent: &Session) -> Arc<Runtime> {
    parent
        .services
        .thread_extension_data
        .get_or_init(Runtime::default)
}

async fn spawn_helper(
    parent: &Arc<Session>,
    goal: &codex_state::ThreadGoal,
) -> Result<ThreadId, String> {
    let mut config = parent.effective_session_config().await;
    config.ephemeral = true;
    config.developer_instructions = Some(match config.developer_instructions.take() {
        Some(existing) => format!("{existing}\n\n{}", include_str!("prompt.md")),
        None => include_str!("prompt.md").to_string(),
    });
    let parent_source = parent.session_source().await;
    let helper_path = AgentPath::root().join(HELPER_ROLE_NAME)?;
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent.thread_id,
        depth: next_thread_spawn_depth(&parent_source),
        agent_path: Some(helper_path),
        agent_nickname: None,
        agent_role: Some(HELPER_ROLE_NAME.to_string()),
    });
    let continuity = continuity(parent, goal).await;
    let checkin_time = parent
        .services
        .time_provider
        .current_time(parent.thread_id)
        .await
        .map_err(|error| format!("failed to read current time: {error:#}"))?;
    let prompt =
        render_checkin_prompt(parent.thread_id, checkin_time, &goal.objective, &continuity);
    let helper = Box::pin(
        parent
            .services
            .agent_control
            .spawn_hidden_agent_with_metadata(
                config,
                vec![UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }],
                source,
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: Some("saffron-goal-supervisor".to_string()),
                    fork_mode: Some(SpawnAgentForkMode::FullHistory),
                    parent_thread_id: Some(parent.thread_id),
                    parent_turn_id: None,
                    root_turn_id: None,
                    environments: None,
                    multi_agent_v2_usage_hints: None,
                },
            ),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(helper.thread_id)
}

/// Renders one bounded model-visible assignment for the ephemeral helper.
///
/// The time comes from the parent session's clock for this helper spawn. It is
/// kept in the prefix that middle truncation preserves so inherited history
/// cannot become the helper's only evidence of the current time.
fn render_checkin_prompt(
    parent_id: ThreadId,
    checkin_time: DateTime<Utc>,
    objective: &str,
    continuity: &str,
) -> String {
    let checkin_time = checkin_time.format("%Y-%m-%d %H:%M:%S UTC");
    let prompt = format!(
        "# Supervisor Check-in\n\nCurrent UTC time: {checkin_time}\n\nParent thread: {parent_id}\n\nActive goal:\n{objective}\n\nContinuity:\n{continuity}"
    );
    truncate_text(&prompt, TruncationPolicy::Tokens(MAX_CHECKIN_PROMPT_TOKENS))
}

async fn continuity(parent: &Session, goal: &codex_state::ThreadGoal) -> String {
    let runtime = runtime(parent);
    let state = runtime.state.lock().await;
    serde_json::json!({
        "goal_created_at": goal.created_at.timestamp(),
        "goal_updated_at": goal.updated_at.timestamp(),
        "tokens_used": goal.tokens_used,
        "time_used_seconds": goal.time_used_seconds,
        "previous_action": state.previous_action,
        "consecutive_failures": state.consecutive_failures,
    })
    .to_string()
}

fn watch_helper(parent: Weak<Session>, helper_id: ThreadId, goal_id: String) {
    tokio::spawn(async move {
        let Some(parent) = parent.upgrade() else {
            return;
        };
        let terminal_status = match parent
            .services
            .agent_control
            .subscribe_status(helper_id)
            .await
        {
            Ok(mut status_rx) => loop {
                let status = status_rx.borrow().clone();
                if !matches!(status, AgentStatus::PendingInit | AgentStatus::Running) {
                    break status;
                }
                if status_rx.changed().await.is_err() {
                    break parent.services.agent_control.get_status(helper_id).await;
                }
            },
            Err(_) => parent.services.agent_control.get_status(helper_id).await,
        };
        finish_helper(&parent, helper_id, &goal_id, terminal_status).await;
    });
}

async fn finish_helper(
    parent: &Arc<Session>,
    helper_id: ThreadId,
    goal_id: &str,
    terminal_status: AgentStatus,
) {
    let runtime = runtime(parent);
    let _transition = Arc::clone(&runtime.transition).lock_owned().await;
    let action = {
        let mut state = runtime.state.lock().await;
        let Some(active) = state
            .active
            .as_ref()
            .filter(|active| active.thread_id == helper_id)
        else {
            return;
        };
        let action = active.action.clone();
        state.active = None;
        action
    };
    if let Err(error) = parent
        .services
        .agent_control
        .shutdown_live_agent(helper_id)
        .await
    {
        warn!(%helper_id, "failed to retire Saffron goal supervisor: {error}");
    }
    if action.is_none() {
        let description =
            format!("helper ended as {terminal_status:?} without selecting an action");
        defer_failure(parent, &runtime, goal_id, description).await;
    }
}

async fn defer_failure(
    parent: &Arc<Session>,
    runtime: &Arc<Runtime>,
    goal_id: &str,
    error: String,
) {
    let delay = {
        let mut state = runtime.state.lock().await;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        failure_retry_delay(state.consecutive_failures)
    };
    parent
        .send_event_raw(Event {
            id: format!("saffron-supervisor-retry-{}", parent.thread_id),
            msg: EventMsg::Warning(WarningEvent {
                message: format!(
                    "Saffron goal supervisor failed: {error}. Retrying in {}s.",
                    delay.as_secs()
                ),
            }),
        })
        .await;
    match snooze_for_active_goal(parent, goal_id, delay).await {
        Ok(snooze) => {
            if let Err(persistence_error) = set_snooze(parent, snooze.clone()).await {
                warn!(
                    thread_id = %parent.thread_id,
                    "failed to persist Saffron supervisor failure retry: {persistence_error}"
                );
                runtime.state.lock().await.snooze = Some(snooze.clone());
            }
            schedule_wake(parent, runtime, snooze);
        }
        Err(goal_error) => warn!(
            thread_id = %parent.thread_id,
            "failed to schedule Saffron supervisor failure retry: {goal_error}"
        ),
    }
}

fn failure_retry_delay(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(6);
    INITIAL_FAILURE_RETRY
        .saturating_mul(1_u32 << exponent)
        .min(MAX_FAILURE_RETRY)
}

pub(super) fn schedule_wake(parent: &Arc<Session>, runtime: &Arc<Runtime>, snooze: Snooze) {
    let generation = runtime.wake_generation.fetch_add(1, Ordering::AcqRel) + 1;
    let parent = Arc::downgrade(parent);
    let runtime = Arc::downgrade(runtime);
    tokio::spawn(async move {
        tokio::time::sleep_until(snooze.deadline).await;
        let (Some(parent), Some(runtime)) = (parent.upgrade(), runtime.upgrade()) else {
            return;
        };
        if runtime.wake_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let should_wake = {
            let mut state = runtime.state.lock().await;
            if state
                .snooze
                .as_ref()
                .is_some_and(|current| current.wake == snooze.wake)
            {
                state.snooze = None;
                true
            } else {
                false
            }
        };
        if should_wake {
            if let Some(state_db) = parent.services.state_db.as_ref() {
                let clear_result = match SaffronStore::open(state_db.sqlite()).await {
                    Ok(store) => store.clear_goal_wake(&snooze.wake).await.map(|_| ()),
                    Err(error) => Err(error),
                };
                if let Err(error) = clear_result {
                    warn!(
                        thread_id = %parent.thread_id,
                        "failed to clear due Saffron supervisor wake: {error}"
                    );
                }
            }
            parent
                .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
                .await;
        }
    });
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
