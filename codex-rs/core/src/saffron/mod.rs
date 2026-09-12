//! Saffrodex-owned extensions to the upstream Codex core.
//!
//! This module is the production-code boundary for behavior maintained by the
//! Saffrodex project. Upstream modules expose narrow, reusable primitives;
//! Saffron modules compose those primitives into complete features and export
//! only the integration points that upstream registration needs.

pub(crate) mod await_exec;
mod goal_edit;
pub(crate) mod goal_supervisor;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::registry::ToolRegistry;

/// Registers the Saffron tools authorized for the current model step.
///
/// Root goal editing follows the goal feature and durable-state lifecycle.
/// Supervisor tools instead require both the unforgeable hidden-agent marker
/// and the Saffron helper role before they become model-visible.
pub(crate) fn register_tools(
    session: &Session,
    turn_context: &TurnContext,
    registry: &mut ToolRegistry,
) {
    let is_supervisor = goal_supervisor::is_helper_source(&turn_context.session_source)
        && session
            .services
            .agent_control
            .is_hidden_agent(session.thread_id);
    if is_supervisor {
        goal_edit::register_supervisor(registry);
        goal_supervisor::register(registry);
        return;
    }

    goal_edit::register_root_if_available(session, turn_context, registry);
}
