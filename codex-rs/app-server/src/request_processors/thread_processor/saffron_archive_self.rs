//! Completes Core's self-archival handoff through the native archive owner.
//!
//! The process owns the worker, not the requesting turn or its shutdown hooks.
//! A runtime identity and a conditional shutdown prevent stale requests from
//! stopping replacement work. Requests do not survive process exit.

use std::time::Duration;

use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchivedNotification;
use codex_core::SaffronArchiveRequest;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ThreadRequestProcessor;

impl ThreadRequestProcessor {
    /// Stops accepting handoffs on cancellation; an admitted archive finishes before drain.
    pub(crate) fn start_saffron_archive_worker(
        &self,
        mut receiver: mpsc::UnboundedReceiver<SaffronArchiveRequest>,
    ) -> CancellationToken {
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let processor = self.clone();
        self.background_tasks.spawn(async move {
            loop {
                let request = tokio::select! {
                    biased;
                    _ = worker_stop.cancelled() => break,
                    request = receiver.recv() => match request { Some(request) => request, None => break },
                };
                if let Err(error) = processor.archive_self(&request).await {
                    tracing::warn!(thread_id = %request.thread_id, %error, "self-archival failed");
                    // Native teardown may already have removed subscriptions. A global
                    // warning still reaches connected clients without implying success.
                    processor.outgoing.send_server_notification(ServerNotification::Warning(
                        codex_app_server_protocol::WarningNotification {
                            thread_id: Some(request.thread_id.to_string()),
                            message: format!("Self-archival did not complete: {error}. The thread remains saved; inspect its archive status before retrying."),
                        }
                    )).await;
                }
                request.cancel();
            }
        });
        stop
    }

    /// Holds the native lifecycle permit from shutdown admission through archival.
    async fn archive_self(&self, request: &SaffronArchiveRequest) -> anyhow::Result<()> {
        let _permit = self
            .acquire_thread_list_state_permit()
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let Ok(thread) = self.thread_manager.get_thread(request.thread_id).await else {
            return Ok(());
        };
        if !request.matches_runtime(thread.thread_extension_data()) {
            return Ok(());
        }
        let thread_state = self
            .thread_state_manager
            .thread_state(request.thread_id)
            .await;
        let drain = {
            let mut state = thread_state.lock().await;
            state
                .listener_matches(&thread)
                .then(|| state.register_shutdown_drain_waiter())
        };
        if !thread
            .request_shutdown_after_turn(request.turn_id.clone())
            .await?
        {
            thread_state.lock().await.take_shutdown_drain_waiter();
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(10), thread.wait_until_terminated()).await?;
        if let Some(drain) = drain {
            // Core termination does not imply that the client listener has
            // delivered the final message and completion event ahead of shutdown.
            tokio::time::timeout(Duration::from_secs(10), drain).await??;
        }
        // Shutdown closes the live recorder. Remove only that stopped runtime
        // so native archival reads saved history instead of persisting it again.
        if self
            .thread_manager
            .remove_thread_if_matches(&request.thread_id, &thread)
            .await
            .is_none()
        {
            anyhow::bail!("the requesting runtime changed before archival");
        }
        let (_, archived) = self
            .thread_archive_response(ThreadArchiveParams {
                thread_id: request.thread_id.to_string(),
            })
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        for thread_id in archived {
            self.outgoing
                .send_server_notification(ServerNotification::ThreadArchived(
                    ThreadArchivedNotification { thread_id },
                ))
                .await;
        }
        Ok(())
    }
}
