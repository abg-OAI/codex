//! Serialized interactions with stored unified-exec processes.
//!
//! This module is the lifecycle boundary for consumers that need to wait on or
//! drain an existing process. [`ProcessInteraction`] owns the per-process lock
//! from acquisition through lifecycle reconciliation. Consumers may choose
//! which process event to await, but they do not coordinate process-store
//! removal, deferred network approval, failure precedence, or hook identity.
//!
//! Simultaneous events have a stable precedence. Acquiring the interaction
//! lock wins over its deadline. Once acquired, process exit wins over output
//! and the deadline, and available output wins over the deadline.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use tokio::sync::OwnedMutexGuard;
use tokio::time::Instant;

use super::fail_process_with_message;
use super::finish_deferred_network_approval_after_process_exit_for_session;
use super::finish_deferred_network_approval_for_session;
use super::network_denial_message_for_session;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::network_approval::DeferredNetworkApproval;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputHandles;

const POST_EXIT_OUTPUT_CLOSE_WAIT: Duration = Duration::from_millis(50);

/// Result of attempting to serialize an interaction before its deadline.
pub(crate) enum ProcessInteractionAcquisition<'a> {
    Acquired(Box<ProcessInteraction<'a>>),

    /// The process still belongs to another interaction. No output was
    /// consumed, and the snapshot records that the same stored process was
    /// reusable when the deadline was revalidated.
    TimedOut(ProcessInteractionTimeout),
}

/// A process event observed while holding serialized interaction access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessWaitReason {
    Output,
    Exit,
    Timeout,
}

/// Exclusive access to one stored process and its draining output buffer.
///
/// Dropping an unfinished interaction releases serialization without removing
/// the process or consuming output. Call [`Self::finish_and_drain`] after the
/// selected wait completes so unified exec can reconcile lifecycle state and
/// return the output exactly once.
pub(crate) struct ProcessInteraction<'a> {
    manager: &'a UnifiedExecProcessManager,
    process: Arc<UnifiedExecProcess>,
    output: OutputHandles,
    session: Option<Arc<crate::session::session::Session>>,
    network_approval: Option<DeferredNetworkApproval>,
    call_id: String,
    hook_command: String,
    process_id: i32,
    _guard: OwnedMutexGuard<()>,
}

/// Lifecycle state and bounded output produced by one completed interaction.
///
/// The process ID is retained internally only while the session remains
/// reusable. Converting this result to [`ExecCommandToolOutput`] preserves the
/// originating command's event and hook identity.
pub(crate) struct ProcessInteractionResult {
    outcome: ProcessInteractionOutcome,
    output: HeadTailBuffer,
}

/// Live-session metadata captured when interaction acquisition times out.
///
/// Constructing this snapshot revalidates the stored process after the
/// deadline. It does not claim that an interaction was acquired or finalized,
/// and it never contains process output.
pub(crate) struct ProcessInteractionTimeout {
    outcome: ProcessInteractionOutcome,
}

struct ProcessInteractionOutcome {
    process_id: Option<i32>,
    exit_code: Option<i32>,
    event_call_id: String,
    hook_command: String,
}

impl UnifiedExecProcessManager {
    /// Acquires exclusive interaction access before `deadline`.
    ///
    /// The deadline covers time waiting behind another interaction; `None`
    /// waits indefinitely. On timeout the returned snapshot contains no output
    /// and records a process that was live and stored when revalidated. Once
    /// acquired, the interaction updates the process's recency and validates
    /// that its ID still refers to the process selected before the lock wait.
    pub(crate) async fn begin_process_interaction(
        &self,
        process_id: i32,
        deadline: Option<Instant>,
    ) -> Result<ProcessInteractionAcquisition<'_>, UnifiedExecError> {
        let (process, call_id, hook_command) = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            (
                Arc::clone(&entry.process),
                entry.call_id.clone(),
                entry.hook_command.clone(),
            )
        };

        let interaction_lock = process.interaction_lock();
        let acquire = interaction_lock.lock_owned();
        tokio::pin!(acquire);
        let guard = tokio::select! {
            biased;
            guard = &mut acquire => guard,
            _ = wait_for_deadline(deadline) => {
                let store = self.process_store.lock().await;
                let process_is_still_reusable = store
                    .processes
                    .get(&process_id)
                    .is_some_and(|entry| {
                        Arc::ptr_eq(&entry.process, &process) && !process.has_exited()
                    });
                if !process_is_still_reusable {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
                return Ok(ProcessInteractionAcquisition::TimedOut(
                    ProcessInteractionTimeout {
                        outcome: ProcessInteractionOutcome {
                            process_id: Some(process_id),
                            exit_code: None,
                            event_call_id: call_id,
                            hook_command,
                        },
                    },
                ));
            }
        };

        let mut store = self.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
        if !Arc::ptr_eq(&entry.process, &process) {
            return Err(UnifiedExecError::UnknownProcessId { process_id });
        }
        entry.last_used = Instant::now();
        let session = entry.session.upgrade();

        Ok(ProcessInteractionAcquisition::Acquired(Box::new(
            ProcessInteraction {
                manager: self,
                process,
                output: entry.process.output_handles().clone(),
                session,
                network_approval: entry.network_approval.clone(),
                call_id: entry.call_id.clone(),
                hook_command: entry.hook_command.clone(),
                process_id: entry.process_id,
                _guard: guard,
            },
        )))
    }
}

impl ProcessInteraction<'_> {
    /// Waits until buffered or newly published output is available, the
    /// process exits, or `deadline` is reached. `None` waits indefinitely.
    ///
    /// This method observes but does not drain output. Registering the output
    /// notification while inspecting the buffer prevents a producer from
    /// publishing between the empty-buffer check and the sleep. Process exit
    /// is transport-neutral: exec-server `Exited` events wake the wait even if
    /// inherited output streams remain open. If events become ready together,
    /// exit wins over output and the deadline, and output wins over the
    /// deadline.
    pub(crate) async fn wait_for_output_or_exit(
        &self,
        deadline: Option<Instant>,
    ) -> ProcessWaitReason {
        loop {
            let output_published = self.output.output_notify.notified();
            tokio::pin!(output_published);
            let has_output = {
                let output = self.output.output_buffer.lock().await;
                let has_output = output.retained_bytes() > 0 || output.omitted_bytes() > 0;
                output_published.as_mut().enable();
                has_output
            };

            if self.process.has_exited() {
                self.wait_for_trailing_output().await;
                return ProcessWaitReason::Exit;
            }
            if has_output {
                return ProcessWaitReason::Output;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return ProcessWaitReason::Timeout;
            }

            let process_exited = self.process.wait_for_exit();
            tokio::pin!(process_exited);
            tokio::select! {
                biased;
                _ = &mut process_exited => {}
                _ = &mut output_published => {}
                _ = wait_for_deadline(deadline) => return ProcessWaitReason::Timeout,
            }
        }
    }

    /// Waits for process exit or `deadline` without consuming or waking on
    /// intermediate output. `None` waits indefinitely.
    ///
    /// The shared bounded buffer retains intermediate output. Cancellation of
    /// the caller therefore leaves those bytes available to the next process
    /// interaction. Exit wins a simultaneous race with the deadline.
    pub(crate) async fn wait_for_exit(&self, deadline: Option<Instant>) -> ProcessWaitReason {
        if self.process.has_exited() {
            self.wait_for_trailing_output().await;
            return ProcessWaitReason::Exit;
        }

        let process_exited = self.process.wait_for_exit();
        tokio::pin!(process_exited);
        let reason = tokio::select! {
            biased;
            _ = &mut process_exited => ProcessWaitReason::Exit,
            _ = wait_for_deadline(deadline) => ProcessWaitReason::Timeout,
        };
        if reason == ProcessWaitReason::Exit {
            self.wait_for_trailing_output().await;
        }
        reason
    }

    /// Reconciles lifecycle state and then drains the interaction's output.
    ///
    /// All fallible asynchronous cleanup precedes the drain. If the caller is
    /// cancelled during waiting or cleanup, output remains in the manager-owned
    /// bounded buffer for a later interaction.
    pub(crate) async fn finish_and_drain(
        mut self,
    ) -> Result<ProcessInteractionResult, UnifiedExecError> {
        let mut exit_cleanup_finished = false;
        loop {
            self.finish_failure().await?;

            if self.process.has_exited() && !exit_cleanup_finished {
                if let Err(message) =
                    finish_deferred_network_approval_after_process_exit_for_session(
                        self.session.as_ref(),
                        self.network_approval.take(),
                    )
                    .await
                {
                    self.manager.release_process_id(self.process_id).await;
                    return Err(fail_process_with_message(self.process.as_ref(), message));
                }
                exit_cleanup_finished = true;
            }

            // Waiting for output or retrying a busy store leaves both owners
            // unchanged. After both guards exist, no cancellation point occurs
            // before output draining and process removal commit together.
            let mut output = self.output.output_buffer.lock().await;
            let Ok(mut store) = self.manager.process_store.try_lock() else {
                drop(output);
                tokio::task::yield_now().await;
                continue;
            };
            let Some(entry) = store.processes.get(&self.process_id) else {
                return Err(UnifiedExecError::UnknownProcessId {
                    process_id: self.process_id,
                });
            };
            if !Arc::ptr_eq(&entry.process, &self.process) {
                return Err(UnifiedExecError::UnknownProcessId {
                    process_id: self.process_id,
                });
            }

            let has_exited = self.process.has_exited();
            if self
                .network_approval
                .as_ref()
                .is_some_and(DeferredNetworkApproval::is_cancelled)
                || self.process.failure_message().is_some()
                || (has_exited && !exit_cleanup_finished)
            {
                drop(store);
                drop(output);
                continue;
            }

            let exit_code = self.process.exit_code();
            let process_id = if has_exited {
                store.remove(self.process_id);
                None
            } else {
                Some(self.process_id)
            };
            let output = output.drain();
            let outcome = ProcessInteractionOutcome {
                process_id,
                exit_code,
                event_call_id: self.call_id.clone(),
                hook_command: self.hook_command.clone(),
            };
            return Ok(ProcessInteractionResult { outcome, output });
        }
    }

    /// Resolves terminal failures before any output is consumed.
    ///
    /// An already recorded process failure remains authoritative; otherwise a
    /// network denial or approval-cleanup failure supplies the terminal error.
    /// Every failure removes the session and returns with output unconsumed.
    async fn finish_failure(&mut self) -> Result<(), UnifiedExecError> {
        if self
            .network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message = network_denial_message_for_session(
                self.session.as_ref(),
                self.network_approval.take(),
            )
            .await;
            self.manager.release_process_id(self.process_id).await;
            return Err(fail_process_with_message(self.process.as_ref(), message));
        }

        if let Some(message) = self.process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                self.session.as_ref(),
                self.network_approval.take(),
            )
            .await;
            self.manager.release_process_id(self.process_id).await;
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(self.process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }

        Ok(())
    }

    /// After exit, the process state may arrive before its output reader closes.
    /// Wait briefly for that reader so immediately trailing bytes are included,
    /// but never let inherited open streams postpone the exit response without
    /// bound.
    async fn wait_for_trailing_output(&self) {
        if self.output.output_closed.load(Ordering::Acquire) {
            return;
        }

        let deadline = Instant::now() + POST_EXIT_OUTPUT_CLOSE_WAIT;
        loop {
            let output_closed = self.output.output_closed_notify.notified();
            tokio::pin!(output_closed);
            output_closed.as_mut().enable();
            if self.output.output_closed.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                _ = &mut output_closed => {}
                _ = tokio::time::sleep_until(deadline) => return,
            }
        }
    }
}

impl ProcessInteractionResult {
    /// Whether lifecycle reconciliation observed process termination.
    pub(crate) fn has_exited(&self) -> bool {
        self.outcome.process_id.is_none()
    }

    /// Converts the result to the canonical unified-exec tool representation.
    ///
    /// The token count measures all bytes observed before model-facing
    /// truncation, including bytes omitted by the bounded head-tail buffer.
    pub(crate) fn into_tool_output(
        self,
        wall_time: Duration,
        truncation_policy: TruncationPolicy,
        max_output_tokens: Option<usize>,
    ) -> ExecCommandToolOutput {
        self.outcome
            .into_tool_output(self.output, wall_time, truncation_policy, max_output_tokens)
    }
}

impl ProcessInteractionTimeout {
    /// Converts the live timeout snapshot to an empty unified-exec response.
    pub(crate) fn into_tool_output(
        self,
        wall_time: Duration,
        truncation_policy: TruncationPolicy,
        max_output_tokens: Option<usize>,
    ) -> ExecCommandToolOutput {
        self.outcome.into_tool_output(
            HeadTailBuffer::default(),
            wall_time,
            truncation_policy,
            max_output_tokens,
        )
    }
}

impl ProcessInteractionOutcome {
    fn into_tool_output(
        self,
        output: HeadTailBuffer,
        wall_time: Duration,
        truncation_policy: TruncationPolicy,
        max_output_tokens: Option<usize>,
    ) -> ExecCommandToolOutput {
        let original_token_count =
            usize::try_from(approx_tokens_from_byte_count(output.total_bytes()))
                .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(output.omitted_bytes());

        ExecCommandToolOutput {
            event_call_id: self.event_call_id,
            chunk_id: generate_chunk_id(),
            wall_time,
            raw_output: output.to_bytes_with_omission_marker(),
            truncation_policy,
            max_output_tokens,
            process_id: self.process_id,
            exit_code: self.exit_code,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(self.hook_command),
        }
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[cfg(unix)]
#[path = "interaction_tests.rs"]
mod tests;
