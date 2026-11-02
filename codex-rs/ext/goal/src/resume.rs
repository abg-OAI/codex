//! Goal-extension implementation of the host's resumption capability.
//!
//! The capability keeps one resume attempt under the goal runtime's mutation
//! lock through accounting, snapshot validation, and persistence. Runtime
//! effects happen after releasing that lock because idle continuation acquires
//! it again.

use std::sync::Arc;

use codex_core::GoalResumeCapability;
use codex_core::GoalResumeError;
use codex_core::GoalResumeRequest;
use codex_extension_api::ExtensionFuture;
use codex_protocol::protocol::ThreadGoal;

use crate::events::GoalEventEmitter;
use crate::runtime::GoalRuntimeHandle;
use crate::runtime::PreviousGoalSnapshot;
use crate::tool::protocol_goal_from_state;

pub(crate) struct GoalResumeRuntime {
    runtime: GoalRuntimeHandle,
    state_dbs: Arc<codex_state::StateRuntime>,
    event_emitter: GoalEventEmitter,
}

impl GoalResumeRuntime {
    pub(crate) fn new(
        runtime: GoalRuntimeHandle,
        state_dbs: Arc<codex_state::StateRuntime>,
        event_emitter: GoalEventEmitter,
    ) -> Self {
        Self {
            runtime,
            state_dbs,
            event_emitter,
        }
    }

    async fn resume_goal(&self, request: GoalResumeRequest) -> Result<ThreadGoal, GoalResumeError> {
        let goal_state_permit = self
            .runtime
            .goal_state_permit()
            .await
            .map_err(GoalResumeError::Internal)?;
        let current = self
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.runtime.thread_id())
            .await
            .map_err(|error| GoalResumeError::Internal(error.to_string()))?
            .ok_or(GoalResumeError::GoalNotFound)?;
        match current.status {
            codex_state::ThreadGoalStatus::Paused
            | codex_state::ThreadGoalStatus::Blocked
            | codex_state::ThreadGoalStatus::UsageLimited => {}
            codex_state::ThreadGoalStatus::Active => {
                return Err(GoalResumeError::AlreadyActive);
            }
            codex_state::ThreadGoalStatus::BudgetLimited => {
                return Err(GoalResumeError::TokenBudgetExhausted);
            }
            codex_state::ThreadGoalStatus::Complete => {
                return Err(GoalResumeError::Completed);
            }
        }
        self.runtime
            .prepare_external_goal_mutation()
            .await
            .map_err(GoalResumeError::Internal)?;

        let previous_goal = PreviousGoalSnapshot::from(&current);
        let resumed = self
            .state_dbs
            .thread_goals()
            .resume_thread_goal(&current)
            .await
            .map_err(|error| GoalResumeError::Internal(error.to_string()))?
            .ok_or(GoalResumeError::Changed)?;
        self.runtime.clear_pending_turn_start_options().await;
        drop(goal_state_permit);

        let goal = protocol_goal_from_state(resumed.clone());
        self.event_emitter
            .thread_goal_updated(request.event_id, request.turn_id, goal.clone());
        if let Err(error) = self
            .runtime
            .apply_external_goal_set(resumed, Some(previous_goal))
            .await
        {
            tracing::warn!(%error, "failed to apply resumed goal runtime effects");
        }

        Ok(goal)
    }
}

impl GoalResumeCapability for GoalResumeRuntime {
    fn resume<'a>(
        &'a self,
        request: GoalResumeRequest,
    ) -> ExtensionFuture<'a, Result<ThreadGoal, GoalResumeError>> {
        Box::pin(self.resume_goal(request))
    }
}
