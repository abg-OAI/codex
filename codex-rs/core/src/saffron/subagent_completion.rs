//! Keeps parent sessions resident while spawned subagents can still hand off results.
//!
//! A spawned MultiAgentV2 turn finishes by sending terminal mail to its direct
//! parent. The app server may otherwise unload an idle, unsubscribed ancestor
//! before that mail reaches the parent's mailbox. This registry gives each
//! running child turn and queued triggering handoff a reference-counted lease
//! on its ancestor chain. A queued handoff transfers its lease to the recipient
//! turn before leaving the mailbox, so acceptance cannot open an unload gap.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::ExtensionData;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use crate::agent::types::MessageDeliveryMode;
use crate::session::session::Session;

/// Process-local retention state shared by one multi-agent tree.
#[derive(Default)]
pub(crate) struct AncestorTurnRetention {
    retained_ancestors: Mutex<HashMap<ThreadId, usize>>,
}

/// Releases one retained ancestor-chain lease when its owner drops it.
pub(crate) struct AncestorTurnRetentionGuard {
    retention: Arc<AncestorTurnRetention>,
    ancestor_thread_ids: Vec<ThreadId>,
}

/// Selects whether a successful result should start an idle parent turn.
///
/// The value is stored in one turn's [`ExtensionData`], so a follow-up turn
/// returns to [`CompletionDelivery::WakeParent`] unless it makes another
/// explicit choice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CompletionDelivery {
    /// Delivers the result and starts an idle parent turn.
    #[default]
    WakeParent,
    /// Delivers the result for the parent's next naturally started turn.
    DeferToParent,
}

/// Reports whether one turn can control the V2 terminal handoff.
pub(crate) fn can_choose_delivery(
    multi_agent_version: MultiAgentVersion,
    source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(
            source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                agent_path: Some(_),
                ..
            })
        )
}

/// Stores the successful-result delivery choice for one turn.
pub(crate) fn set_delivery(turn_store: &ExtensionData, delivery: CompletionDelivery) {
    turn_store.insert(delivery);
}

/// Resolves terminal delivery while forcing every abnormal outcome to wake.
pub(crate) fn terminal_delivery_mode(
    turn_store: &ExtensionData,
    status: &AgentStatus,
) -> MessageDeliveryMode {
    let delivery = turn_store
        .get::<CompletionDelivery>()
        .as_deref()
        .copied()
        .unwrap_or_default();
    if matches!(status, AgentStatus::Completed(_)) && delivery == CompletionDelivery::DeferToParent
    {
        MessageDeliveryMode::QueueOnly
    } else {
        MessageDeliveryMode::TriggerTurn
    }
}

/// Retains the local ancestor chain required for a subagent turn's handoff.
pub(crate) fn retain_ancestors(
    session: &Session,
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
    thread_id: ThreadId,
) -> Option<AncestorTurnRetentionGuard> {
    session
        .services
        .local_agent_runtime
        .control(session.services.agent_control.identity())
        .ancestor_turn_retention_guard(multi_agent_version, session_source, thread_id)
}

/// Reports whether a local descendant lifecycle still needs this session.
pub(crate) fn is_retained(session: &Session, thread_id: ThreadId) -> bool {
    session
        .services
        .local_agent_runtime
        .control(session.services.agent_control.identity())
        .is_retained_for_descendant_completion(thread_id)
}

impl AncestorTurnRetention {
    /// Retains every distinct ancestor until the returned guard is dropped.
    pub(crate) fn retain(
        self: &Arc<Self>,
        ancestor_thread_ids: impl IntoIterator<Item = ThreadId>,
    ) -> Option<AncestorTurnRetentionGuard> {
        let mut seen = HashSet::new();
        let ancestor_thread_ids = ancestor_thread_ids
            .into_iter()
            .filter(|thread_id| seen.insert(*thread_id))
            .collect::<Vec<_>>();
        if ancestor_thread_ids.is_empty() {
            return None;
        }

        let mut retained_ancestors = self
            .retained_ancestors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for thread_id in &ancestor_thread_ids {
            *retained_ancestors.entry(*thread_id).or_default() += 1;
        }
        drop(retained_ancestors);

        Some(AncestorTurnRetentionGuard {
            retention: Arc::clone(self),
            ancestor_thread_ids,
        })
    }

    /// Reports whether a descendant lifecycle retains `thread_id` for handoff.
    pub(crate) fn is_retained(&self, thread_id: ThreadId) -> bool {
        self.retained_ancestors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&thread_id)
    }

    fn release(&self, ancestor_thread_ids: &[ThreadId]) {
        let mut retained_ancestors = self
            .retained_ancestors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for thread_id in ancestor_thread_ids {
            let Some(retention_count) = retained_ancestors.get_mut(thread_id) else {
                continue;
            };
            *retention_count -= 1;
            if *retention_count == 0 {
                retained_ancestors.remove(thread_id);
            }
        }
    }
}

impl Drop for AncestorTurnRetentionGuard {
    fn drop(&mut self) {
        self.retention.release(&self.ancestor_thread_ids);
    }
}

#[cfg(test)]
#[path = "subagent_completion_tests.rs"]
mod tests;
