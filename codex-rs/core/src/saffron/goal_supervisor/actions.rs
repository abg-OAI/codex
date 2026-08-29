//! Side effects selected by a goal supervisor helper.

use std::sync::Arc;
use std::time::Duration;

use super::runtime;
use super::runtime::Action;
use crate::TurnStartOptions;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::saffron::goal_edit::protocol_goal;
use crate::session::session::Session;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;

pub(super) async fn followup(
    helper: &Arc<Session>,
    parent: &Arc<Session>,
    message: String,
) -> Result<(), String> {
    let message = message.trim();
    if message.is_empty() {
        return Err("message must not be empty".to_string());
    }
    let action = Action::Followup;
    runtime::select_action(parent, helper.thread_id, action.clone()).await?;
    if let Err(error) = deliver_parent_message(helper, parent, message).await {
        runtime::clear_failed_action(parent, helper.thread_id, &action).await;
        return Err(error);
    }
    runtime::commit_action(parent, action).await;
    Ok(())
}

pub(super) async fn notify_parent(
    helper: &Arc<Session>,
    parent: &Arc<Session>,
    message: &str,
) -> Result<(), String> {
    let message = message.trim();
    if message.is_empty() {
        return Ok(());
    }
    deliver_parent_message(helper, parent, message).await
}

async fn deliver_parent_message(
    helper: &Arc<Session>,
    parent: &Arc<Session>,
    message: &str,
) -> Result<(), String> {
    let communication = InterAgentCommunication::new(
        helper
            .session_source()
            .await
            .get_agent_path()
            .unwrap_or_else(AgentPath::root),
        parent
            .session_source()
            .await
            .get_agent_path()
            .unwrap_or_else(AgentPath::root),
        Vec::new(),
        message.to_string(),
        /*trigger_turn*/ true,
    );
    let context =
        AgentCommunicationContext::new(AgentCommunicationKind::Followup, helper.thread_id);
    helper
        .services
        .agent_control
        .send_inter_agent_communication(
            parent.thread_id,
            communication,
            context,
            TurnStartOptions::default(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) async fn snooze(
    parent: &Arc<Session>,
    helper_id: ThreadId,
    delay_seconds: u64,
) -> Result<(), String> {
    let action = Action::Snooze { delay_seconds };
    let goal_id = runtime::select_action(parent, helper_id, action.clone()).await?;
    let supervisor_runtime = runtime::runtime(parent);
    let snooze =
        match runtime::snooze_for_active_goal(parent, &goal_id, Duration::from_secs(delay_seconds))
            .await
        {
            Ok(snooze) => snooze,
            Err(error) => {
                runtime::clear_failed_action(parent, helper_id, &action).await;
                return Err(error);
            }
        }
        .reconstructible();
    if let Err(error) = runtime::set_snooze(parent, snooze.clone()).await {
        runtime::clear_failed_action(parent, helper_id, &action).await;
        return Err(error);
    }
    runtime::schedule_wake(parent, &supervisor_runtime, snooze);
    runtime::commit_action(parent, action).await;
    Ok(())
}

pub(super) async fn compact(parent: &Arc<Session>, helper_id: ThreadId) -> Result<String, String> {
    let action = Action::Compact;
    runtime::select_action(parent, helper_id, action.clone()).await?;
    if parent.active_turn.lock().await.is_some() {
        runtime::clear_failed_action(parent, helper_id, &action).await;
        return Err("the parent became busy before compaction could start".to_string());
    }
    let result = async {
        let thread = parent
            .services
            .agent_control
            .get_live_thread(parent.thread_id)
            .await
            .map_err(|error| error.to_string())?;
        thread
            .submit(Op::Compact)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    if result.is_err() {
        runtime::clear_failed_action(parent, helper_id, &action).await;
    } else {
        runtime::commit_action(parent, action).await;
    }
    result
}

pub(super) async fn complete(
    parent: &Arc<Session>,
    helper_id: ThreadId,
) -> Result<ThreadGoal, String> {
    let action = Action::Complete;
    let goal_id = runtime::select_action(parent, helper_id, action.clone()).await?;
    let Some(state_db) = parent.services.state_db.as_ref() else {
        runtime::clear_failed_action(parent, helper_id, &action).await;
        return Err("goal state is unavailable".to_string());
    };
    let update = state_db
        .thread_goals()
        .update_thread_goal(
            parent.thread_id,
            codex_state::GoalUpdate {
                objective: None,
                status: Some(codex_state::ThreadGoalStatus::Complete),
                token_budget: None,
                expected_goal_id: Some(goal_id.clone()),
            },
        )
        .await
        .map_err(|error| error.to_string())
        .and_then(|goal| {
            goal.ok_or_else(|| "the active goal changed before it could be completed".to_string())
        });
    let goal = match update {
        Ok(goal) => goal,
        Err(error) => {
            runtime::clear_failed_action(parent, helper_id, &action).await;
            return Err(error);
        }
    };
    if let Err(error) = runtime::clear_snooze_for_goal(parent, &goal_id).await {
        tracing::warn!(
            thread_id = %parent.thread_id,
            "failed to clear Saffron supervisor wake for completed goal: {error}"
        );
    }
    let goal = protocol_goal(goal);
    parent
        .send_event_raw(Event {
            id: format!("saffron-supervisor-complete-{}", parent.thread_id),
            msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                thread_id: parent.thread_id,
                turn_id: None,
                goal: goal.clone(),
            }),
        })
        .await;
    runtime::commit_action(parent, action).await;
    Ok(goal)
}
