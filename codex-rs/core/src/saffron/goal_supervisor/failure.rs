//! Structural failure handling for hidden goal-supervisor helpers.
//!
//! Codex reduces a helper's terminal error to a rendered status message before
//! the supervisor owns helper retirement. Preserve the error category on the
//! helper session so retirement can distinguish a request that cannot succeed
//! from failures that should enter the retry loop.

use std::sync::Arc;

use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::WarningEvent;

use super::is_helper_source;
use crate::saffron::goal_edit::protocol_goal;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// A helper failure whose category affects supervision policy.
#[derive(Debug)]
pub(super) enum TerminalFailure {
    InvalidRequest { message: String },
}

/// Retains terminal error structure until the helper lifecycle observes it.
pub(crate) fn record_turn_error(session: &Session, turn_context: &TurnContext, error: &CodexErr) {
    if !is_helper_source(&turn_context.session_source) {
        return;
    }
    let CodexErrorDetails::InvalidRequest(message) = error.details() else {
        return;
    };
    session
        .services
        .thread_extension_data
        .insert(TerminalFailure::InvalidRequest {
            message: message.clone(),
        });
}

/// Returns the helper failure retained before Codex rendered its status.
pub(super) fn terminal_failure(session: &Session) -> Option<Arc<TerminalFailure>> {
    session
        .services
        .thread_extension_data
        .get::<TerminalFailure>()
}

/// Blocks the matching durable goal and notifies existing app-server clients.
pub(super) async fn block_goal(
    parent: &Arc<Session>,
    goal_id: &str,
    failure: &TerminalFailure,
) -> Result<(), String> {
    let Some(state_db) = parent.services.state_db.as_ref() else {
        return Err("goal state is unavailable".to_string());
    };
    let goal = state_db
        .thread_goals()
        .update_thread_goal(
            parent.thread_id,
            codex_state::GoalUpdate {
                objective: None,
                status: Some(codex_state::ThreadGoalStatus::Blocked),
                token_budget: None,
                expected_goal_id: Some(goal_id.to_string()),
            },
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the active goal changed before it could be blocked".to_string())?;
    parent
        .send_event_raw(Event {
            id: format!("saffron-supervisor-blocked-{}", parent.thread_id),
            msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                thread_id: parent.thread_id,
                turn_id: None,
                goal: protocol_goal(goal),
            }),
        })
        .await;
    let TerminalFailure::InvalidRequest { message } = failure;
    parent
        .send_event_raw(Event {
            id: format!("saffron-supervisor-blocked-warning-{}", parent.thread_id),
            msg: EventMsg::Warning(WarningEvent {
                message: format!("Saffron goal supervisor blocked the active goal: {message}"),
            }),
        })
        .await;
    Ok(())
}
