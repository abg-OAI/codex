//! Persistence policy for saved threads reached through Saffron runtime paths.
//!
//! Delivery configuration belongs to the sender, while persistence belongs to
//! the target. This module keeps that distinction out of upstream agent and
//! session mechanisms.

use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSource;
use codex_thread_store::StoredThread;

/// Returns the fallback persistence mode for a stored target whose live mode
/// was not retained across eviction.
pub(crate) fn stored_target_is_ephemeral(stored_thread: &StoredThread) -> bool {
    source_is_ephemeral(&stored_thread.source, stored_thread.thread_source.as_ref())
}

/// Returns whether a source identifies a temporary system-owned runtime that
/// must never be converted into a saved user thread.
pub(crate) fn source_is_ephemeral(
    session_source: &SessionSource,
    thread_source: Option<&ThreadSource>,
) -> bool {
    crate::saffron::goal_supervisor::is_helper_source(session_source)
        || matches!(thread_source, Some(ThreadSource::Feature(feature)) if feature == "system")
}
