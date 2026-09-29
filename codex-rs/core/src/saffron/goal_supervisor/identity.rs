//! Stable identity for private goal-supervisor helper threads.

use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

/// Role marker assigned by Saffron to its process-local goal supervisor.
pub(crate) const HELPER_ROLE_NAME: &str = "goal_supervisor";

/// Returns whether a session is the private helper created by Saffron.
///
/// Upstream agent accounting and lifecycle code use this narrow predicate to
/// keep the helper outside user-facing agent limits and notifications.
pub(crate) fn is_helper_source(source: &SessionSource) -> bool {
    matches!(
        source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_role: Some(role),
            ..
        }) if role == HELPER_ROLE_NAME
    )
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
