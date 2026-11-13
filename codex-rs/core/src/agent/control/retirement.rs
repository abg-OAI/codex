//! Unregisters a terminated child without reopening its closed history writer.

use super::LocalAgentControl;
use crate::CodexThread;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use std::sync::Arc;

impl LocalAgentControl {
    /// Persist closure and release membership only for the supplied terminated runtime.
    /// Callers keep its residency write guard until this operation completes.
    pub(crate) async fn close_terminated_agent(&self, expected: &Arc<CodexThread>) -> Result<()> {
        let manager = self.runtime.upgrade()?;
        let id = expected.session.thread_id();
        let mut threads = manager.threads.write().await;
        if expected.is_running()
            || !threads
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            return Err(CodexErr::InvalidRequest(
                "agent is still running or its runtime was replaced".into(),
            ));
        }
        if let Some(graph) = manager.agent_graph_store() {
            graph
                .set_thread_spawn_edge_status(id, ThreadSpawnEdgeStatus::Closed)
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to persist agent closure: {error}"))
                })?;
        }
        threads.remove(&id);
        expected.session.services.mcp_runtime.revoke_ema();
        self.forget_v2_residency(id);
        self.runtime.registry.release_spawned_thread(id);
        self.runtime.mailboxes.remove(id);
        Ok(())
    }
}
