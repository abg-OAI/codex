//! Wake policy for `saffron.await_exec`.
//!
//! This module translates [`ReturnOn`] into one transport-neutral process
//! wait. Unified exec owns serialization, output retention, process-store
//! transitions, deferred approval cleanup, and hook identity. The Saffron
//! operation owns only the selected wake condition, the absolute event
//! deadline, and the final reason reported to the model.

use std::time::Duration;

use codex_protocol::protocol::TruncationPolicy;
use tokio::time::Instant;

use super::ReturnOn;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::ProcessInteractionAcquisition;
use crate::unified_exec::ProcessWaitReason;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;

/// Model-visible reason that an await operation returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WakeReason {
    Output,
    Exit,
    Timeout,
}

impl From<ProcessWaitReason> for WakeReason {
    fn from(reason: ProcessWaitReason) -> Self {
        match reason {
            ProcessWaitReason::Output => Self::Output,
            ProcessWaitReason::Exit => Self::Exit,
            ProcessWaitReason::Timeout => Self::Timeout,
        }
    }
}

/// Complete input for one wait on an existing unified-exec session.
///
/// `timeout` is measured from entry to this operation and covers both
/// same-session serialization and the selected event wait; `None` waits
/// indefinitely. Required lifecycle cleanup may complete after the deadline.
/// `max_output_tokens: None` uses the 10,000-token default, and the handler
/// applies the lower of that request budget and `truncation_policy`.
pub(super) struct AwaitExecRequest {
    pub(super) process_id: i32,
    pub(super) return_on: ReturnOn,
    pub(super) timeout: Option<Duration>,
    pub(super) max_output_tokens: Option<usize>,
    pub(super) truncation_policy: TruncationPolicy,
}

/// Canonical unified-exec output plus the final wake reason.
///
/// Lifecycle reconciliation promotes the final reason to `Exit` whenever it
/// observes termination, even if output or the deadline was observed first.
/// The embedded output contains a process ID only while the session remains
/// reusable.
pub(super) struct AwaitExecResult {
    pub(super) reason: WakeReason,
    pub(super) output: ExecCommandToolOutput,
}

impl UnifiedExecProcessManager {
    /// Waits on an existing session without polling or consuming output before
    /// a response can be completed.
    pub(super) async fn await_exec(
        &self,
        request: AwaitExecRequest,
    ) -> Result<AwaitExecResult, UnifiedExecError> {
        let started_at = Instant::now();
        let deadline = request.timeout.map(|timeout| started_at + timeout);
        let acquisition = self
            .begin_process_interaction(request.process_id, deadline)
            .await?;

        let (observed_reason, result) = match acquisition {
            ProcessInteractionAcquisition::Acquired(interaction) => {
                let observed_reason = match request.return_on {
                    ReturnOn::OutputOrExit => interaction.wait_for_output_or_exit(deadline).await,
                    ReturnOn::Exit => interaction.wait_for_exit(deadline).await,
                };
                let result = interaction.finish_and_drain().await?;
                (observed_reason, result)
            }
            ProcessInteractionAcquisition::TimedOut(timeout) => {
                let output = timeout.into_tool_output(
                    started_at.elapsed(),
                    request.truncation_policy,
                    request.max_output_tokens,
                );
                return Ok(AwaitExecResult {
                    reason: WakeReason::Timeout,
                    output,
                });
            }
        };

        let reason = if result.has_exited() {
            WakeReason::Exit
        } else {
            observed_reason.into()
        };
        let output = result.into_tool_output(
            started_at.elapsed(),
            request.truncation_policy,
            request.max_output_tokens,
        );

        Ok(AwaitExecResult { reason, output })
    }
}
