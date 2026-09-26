//! Process-local supervision for active root-thread goals.
//!
//! Codex owns durable goal state and decides when an idle goal needs another
//! opportunity to make progress. This module changes only that opportunity for
//! root threads: it forks a short-lived helper with the parent's full history,
//! lets the helper choose one bounded action, and then retires the helper.
//!
//! Helper identity, snooze deadlines, retry state, and continuity hints are
//! intentionally in memory. A restart discards them and the normal goal idle
//! lifecycle reconstructs supervision from the durable active goal.

use crate::agent::LocalAgentControl;
use crate::session::session::Session;
use codex_protocol::protocol::SessionSource;

mod actions;
pub(crate) mod guidance;
mod history;
mod identity;
mod runtime;
mod tools;

pub(crate) use identity::HELPER_ROLE_NAME;
pub(crate) use identity::is_helper_source;
pub(super) use runtime::begin_goal_edit;
pub(super) use runtime::clear_failed_goal_edit;
pub(super) use runtime::commit_goal_edit;
pub(super) use runtime::parent_for_helper;
pub(crate) use runtime::start_checkin;
pub(crate) use runtime::stop;
pub(crate) use tools::register;

pub(crate) use history::preserves_fork_item;

/// Returns the local controller that owns Saffron's hidden supervisor helper.
///
/// Saffron supervision is process-local: the helper must share the parent's
/// registry and runtime even when the ordinary agent-control interface is
/// backed by another implementation.
fn local_agent_control(session: &Session) -> LocalAgentControl {
    session
        .services
        .local_agent_runtime
        .control(session.services.agent_control.identity())
}

/// Confirms both the private source identity and hidden registry membership.
pub(crate) fn is_helper_session(session: &Session, source: &SessionSource) -> bool {
    is_helper_source(source) && local_agent_control(session).is_hidden_agent(session.thread_id)
}
