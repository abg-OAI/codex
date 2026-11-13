//! Delegation guidance for spawned children, independent of inherited root modes.
//!
//! Session prompt assembly asks this policy before resolving configured or model
//! catalog hints. Tool availability is unchanged; roots retain upstream policy.

use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

/// Selects explicit delegation guidance for spawned children, including resumes.
pub(crate) fn mode(source: &SessionSource) -> Option<MultiAgentMode> {
    matches!(
        source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
    )
    .then(|| MultiAgentMode::Custom(include_str!("child_delegation.txt").trim().to_owned()))
}
