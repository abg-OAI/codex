//! Saffrodex-owned extensions to the upstream Codex core.
//!
//! This module is the production-code boundary for behavior maintained by the
//! Saffrodex project. Upstream modules expose narrow, reusable primitives;
//! Saffron modules compose those primitives into complete features and export
//! only the integration points that upstream registration needs.

pub(crate) mod await_exec;
pub(crate) mod compaction_requests;
pub(crate) mod fork_thread;
mod goal_edit;
pub(crate) mod goal_resume;
pub(crate) mod goal_scheduler;
pub(crate) mod goal_supervisor;
mod luna;
pub(crate) mod request_account;
pub(crate) mod saved_thread_persistence;
mod storage;
pub(crate) mod subagent_completion;
mod subagent_completion_tool;
pub(crate) mod thread_message;

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
    let is_supervisor = goal_supervisor::is_helper_session(session, &turn_context.session_source);
    if is_supervisor {
        goal_edit::register_supervisor(registry);
        goal_supervisor::register(registry);
        return;
    }

    if subagent_completion::can_choose_delivery(
        turn_context.multi_agent_version,
        &turn_context.session_source,
    ) {
        subagent_completion_tool::register(registry);
    }

    goal_edit::register_root_if_available(session, turn_context, registry);
    goal_resume::register_root_if_available(session, turn_context, registry);
    fork_thread::register(session, turn_context, registry);
    thread_message::register(session, turn_context, registry);
}
