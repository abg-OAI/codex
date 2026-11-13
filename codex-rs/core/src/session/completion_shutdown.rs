//! Conditional teardown after successful terminal persistence.
//!
//! The ordered submission queue makes the final shutdown decision. The runtime
//! witness distinguishes a completed turn from an aborted turn or a newer task,
//! even when the newer task has also finished before the request is handled.

use super::session::Session;

impl Session {
    /// Certifies the latest finished turn while the caller holds the idle admission slot.
    /// The caller must flush persistence first and keep that slot through its snapshot.
    pub(crate) async fn checkpoint_idle_completion(&self) -> Option<String> {
        if !matches!(
            *self.agent_status.borrow(),
            codex_protocol::protocol::AgentStatus::Completed(_)
        ) {
            return None;
        }
        let mut state = self.state.lock().await;
        let turn_id = state.last_started_turn_id.clone()?;
        state.last_completed_turn_id = Some(turn_id.clone());
        Some(turn_id)
    }

    /// Checkpoints a terminal turn without certifying a superseding task.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "completion must inspect the active-turn reservation and latest task together"
    )]
    pub(crate) async fn checkpoint_completed_turn(&self, turn_id: &str) -> std::io::Result<bool> {
        self.flush_rollout().await?;
        let active = self.active_turn.lock().await;
        let mut state = self.state.lock().await;
        if active.is_some() || state.last_started_turn_id.as_deref() != Some(turn_id) {
            return Ok(false);
        }
        state.last_completed_turn_id = Some(turn_id.to_owned());
        Ok(true)
    }

    /// Checks whether the captured completed turn still permits teardown.
    /// The submission loop checks before shutdown; lifecycle cleanup may check after termination.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "shutdown must inspect the active-turn reservation and completion witness together"
    )]
    pub(crate) async fn can_shutdown_after_turn(&self, turn_id: &str) -> bool {
        let active = self.active_turn.lock().await;
        if active.is_some()
            || self.is_interrupted()
            || self.input_queue.has_trigger_turn_mailbox_items().await
        {
            return false;
        }
        let state = self.state.lock().await;
        state.last_started_turn_id.as_deref() == Some(turn_id)
            && state.last_completed_turn_id.as_deref() == Some(turn_id)
    }
}

#[cfg(test)]
mod tests {
    use crate::session::tests::make_session_and_context;
    use crate::state::ActiveTurn;

    /// An idle snapshot certifies only completed work and cannot certify a later turn.
    #[tokio::test]
    async fn idle_snapshot_requires_completed_status_and_keeps_turn_identity() {
        use codex_protocol::protocol::AgentStatus;
        let (session, _) = make_session_and_context().await;
        session.record_started_turn("one", None).await;
        session.agent_status.send_replace(AgentStatus::Running);
        let slot = session.active_turn.lock().await;
        assert!(session.checkpoint_idle_completion().await.is_none());
        drop(slot);
        session
            .agent_status
            .send_replace(AgentStatus::Completed(None));
        let slot = session.active_turn.lock().await;
        assert_eq!(
            session.checkpoint_idle_completion().await.as_deref(),
            Some("one")
        );
        drop(slot);
        assert!(session.can_shutdown_after_turn("one").await);
        session.record_started_turn("two", None).await;
        session
            .agent_status
            .send_replace(AgentStatus::Completed(None));
        let slot = session.active_turn.lock().await;
        assert_eq!(
            session.checkpoint_idle_completion().await.as_deref(),
            Some("two")
        );
        drop(slot);
        assert!(!session.can_shutdown_after_turn("one").await);
        assert!(session.can_shutdown_after_turn("two").await);
    }

    #[tokio::test]
    async fn shutdown_requires_persisted_latest_completion() {
        let (session, _) = make_session_and_context().await;
        session.record_started_turn("old", None).await;
        assert!(!session.can_shutdown_after_turn("old").await);
        assert!(session.checkpoint_completed_turn("old").await.unwrap());
        assert!(session.can_shutdown_after_turn("old").await);
        session.record_started_turn("new", None).await;
        assert!(!session.can_shutdown_after_turn("old").await);
        assert!(session.checkpoint_completed_turn("new").await.unwrap());
        assert!(!session.can_shutdown_after_turn("old").await);
        assert!(session.can_shutdown_after_turn("new").await);
    }

    #[tokio::test]
    async fn active_or_interrupted_runtime_refuses_shutdown() {
        let (session, _) = make_session_and_context().await;
        session.record_started_turn("turn", None).await;
        assert!(session.checkpoint_completed_turn("turn").await.unwrap());
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        assert!(!session.can_shutdown_after_turn("turn").await);
        *session.active_turn.lock().await = None;
        session.mark_interrupted();
        assert!(!session.can_shutdown_after_turn("turn").await);
    }
}
