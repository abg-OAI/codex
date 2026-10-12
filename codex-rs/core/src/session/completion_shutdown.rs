//! Conditional teardown after successful terminal persistence.
//!
//! The ordered submission queue makes the final shutdown decision. The runtime
//! witness distinguishes a completed turn from an aborted turn or a newer task,
//! even when the newer task has also finished before the request is handled.

use super::session::Session;

impl Session {
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

    /// Called only by the submission loop immediately before explicit shutdown.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "shutdown must inspect the active-turn reservation and completion witness together"
    )]
    pub(super) async fn can_shutdown_after_turn(&self, turn_id: &str) -> bool {
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
