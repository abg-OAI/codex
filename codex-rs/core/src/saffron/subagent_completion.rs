//! Keeps parent sessions resident while spawned subagents can still hand off results.
//!
//! A spawned MultiAgentV2 turn finishes by sending terminal mail to its direct
//! parent. The app server may otherwise unload an idle, unsubscribed ancestor
//! before that mail reaches the parent's mailbox. This registry gives each
//! running child turn and queued triggering handoff a reference-counted lease
//! on its ancestor chain. A queued handoff transfers its lease to the recipient
//! turn before leaving the mailbox, so acceptance cannot open an unload gap.

use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

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
