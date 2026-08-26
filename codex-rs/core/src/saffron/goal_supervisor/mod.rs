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

mod actions;
mod identity;
mod runtime;
mod tools;

pub(crate) use identity::HELPER_ROLE_NAME;
pub(crate) use identity::is_helper_source;
pub(crate) use runtime::start_checkin;
pub(crate) use runtime::stop;
pub(crate) use tools::register;
