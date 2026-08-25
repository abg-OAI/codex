//! Process-local supervision for active root-thread goals.
//!
//! Codex owns durable goal state and decides when an idle goal needs another
//! opportunity to make progress. This module changes only that opportunity for
//! root threads: it forks a short-lived helper with the parent's full history,
//! lets the helper choose one bounded action, and then retires the helper.
//!
//! Helper identity, retry counters, and continuity hints remain process-local.
//! Snooze and retry deadlines are also recorded in Saffron's auxiliary store,
//! allowing a new runtime to preserve their absolute wake time.

mod actions;
mod identity;
mod runtime;
mod tools;

pub(crate) use identity::HELPER_ROLE_NAME;
pub(crate) use identity::is_helper_source;
pub(super) use runtime::begin_goal_edit;
pub(super) use runtime::clear_failed_goal_edit;
pub(super) use runtime::commit_goal_edit;
pub(crate) use runtime::has_reconstructible_snooze;
pub(super) use runtime::parent_for_helper;
pub(crate) use runtime::should_retain_while_idle;
pub(crate) use runtime::start_checkin;
pub(crate) use runtime::stop;
pub(crate) use tools::register;
