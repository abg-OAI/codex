//! Selects the bounded suffix that an offline split can retain beside an immutable rollout.
//! The caller must hold the thread's writer lock while reading and publishing this range.
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_rollout::{RolloutItem, RolloutLine, parse_rollout_line_bytes};

pub(super) const SCAN_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub(super) struct ColdSuffix {
    pub(super) start: u64,
    pub(super) first_ordinal: u64,
    pub(super) end_ordinal_exclusive: u64,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn read_suffix(file: &mut File, owner: ThreadId) -> io::Result<Option<ColdSuffix>> {
    let end = file.metadata()?.len();
    let start = end.saturating_sub(SCAN_BYTES);
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(start.saturating_sub(1)))?;
    file.take(SCAN_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.last() != Some(&b'\n') {
        return Ok(None);
    }
    // Reading the preceding delimiter distinguishes a whole first line from a partial one.
    let offset = if start > 0 {
        bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(bytes.len())
            + 1
    } else {
        0
    };
    let Some(suffix) = bytes.get(offset..) else {
        return Ok(None);
    };
    let Some((turn_start, first, end_ordinal)) = select_suffix(suffix, owner) else {
        return Ok(None);
    };
    let begin = offset + turn_start;
    bytes.drain(..begin);
    Ok(Some(ColdSuffix {
        start: start.saturating_sub(1) + begin as u64,
        first_ordinal: first,
        end_ordinal_exclusive: end_ordinal,
        bytes,
    }))
}

fn parse(record: &[u8]) -> Option<RolloutLine> {
    if record.len() > MAX_RECORD_BYTES {
        return None;
    }
    parse_rollout_line_bytes(record).ok()
}

fn select_suffix(bytes: &[u8], owner: ThreadId) -> Option<(usize, u64, u64)> {
    // Look backward so a partial newer compaction or rollback cannot expose an obsolete checkpoint.
    let mut after = bytes.len();
    let mut checkpoint = None;
    for record in bytes.strip_suffix(b"\n")?.rsplit(|&b| b == b'\n') {
        let begin = after.checked_sub(record.len() + 1)?;
        let line = parse(record)?;
        match line.item {
            RolloutItem::EventMsg {
                event: EventMsg::ThreadRolledBack(event),
                ..
            } if event.num_turns > 0 => {
                return None;
            }
            RolloutItem::Compacted(item) if checkpoint.is_none() => {
                item.replacement_history.as_ref()?;
                item.window_number?;
                let metadata = item.resume_metadata?;
                metadata.dynamic_tools.as_ref()?;
                let turn_id = metadata.last_started_turn_id?;
                let mut companions = bytes[after..].split(|&b| b == b'\n').map(parse);
                if !matches!(companions.next()??.item, RolloutItem::WorldState(world) if world.full)
                    || !matches!(companions.next()??.item, RolloutItem::TurnContext(ctx) if ctx.turn_id.as_deref() == Some(turn_id.as_str()))
                    || !matches!(companions.next()??.item, RolloutItem::EventMsg { event: EventMsg::ThreadSettingsApplied(settings), .. } if settings.thread_id == Some(owner))
                {
                    return None;
                }
                checkpoint = Some(turn_id);
            }
            RolloutItem::EventMsg {
                event: EventMsg::TurnStarted(event),
                ..
            } if checkpoint.is_some() => {
                if checkpoint.as_deref() != Some(event.turn_id.as_str()) {
                    return None;
                }
                let first = line.ordinal?;
                let next = validate_retained_turns(&bytes[begin..], first)?;
                return Some((begin, first, next));
            }
            _ => {}
        }
        after = begin;
    }
    None
}

fn validate_retained_turns(bytes: &[u8], first: u64) -> Option<u64> {
    let mut turns = HashSet::new();
    let mut next = first;
    for record in bytes.strip_suffix(b"\n")?.split(|&b| b == b'\n') {
        let line = parse(record)?;
        if line.ordinal != Some(next) {
            return None;
        }
        next = next.checked_add(1)?;
        if let RolloutItem::EventMsg {
            event: EventMsg::TurnStarted(event),
            ..
        } = &line.item
        {
            turns.insert(event.turn_id.clone());
        }
        let changes = codex_app_server_protocol::project_rollout_line(&line);
        if changes
            .changed_turns
            .iter()
            .any(|change| !turns.contains(&change.turn_id))
            || changes
                .changed_items
                .iter()
                .any(|change| !turns.contains(&change.turn_id))
        {
            return None;
        }
    }
    Some(next)
}
