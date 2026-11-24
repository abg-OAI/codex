//! Controllable regular-task completion for agent lifecycle tests.
//! Callers release `finish` to produce a final answer without model sampling.

use super::SessionTask;
use super::SessionTaskResult;
use super::regular::RegularTaskOutput;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Waits for the test to release completion, then returns the answer "done".
pub(crate) struct CompletingTask {
    /// Releases the pending task when the test has established its lifecycle state.
    pub(crate) finish: Arc<Notify>,
}

impl SessionTask for CompletingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.completing"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        self.finish.notified().await;
        Ok(Some(RegularTaskOutput {
            last_agent_message: Some("done".to_string()),
            mcp_startup_requirements: Default::default(),
        }))
    }
}
