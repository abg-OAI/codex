//! Evidence retained when an idle parent forks its supervisor.
//!
//! Supervisors need tool results and agent messages to recognize completed work
//! even when the parent ends silently. Ordinary agent forks may omit those
//! items. Upstream still owns instruction sanitization and usage bookkeeping.

use codex_history::RolloutItem;
use codex_protocol::protocol::SessionSource;

use super::identity::is_helper_source;

/// Retains conversation evidence omitted by the ordinary fork policy.
///
/// This supplements, rather than replaces, upstream item selection: it does
/// not retain parent usage records or security-review state. The caller must
/// still sanitize inherited instructions and compacted checkpoint metadata.
pub(crate) fn preserves_fork_item(source: &SessionSource, item: &RolloutItem) -> bool {
    is_helper_source(source)
        && matches!(
            item,
            RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
        )
}
