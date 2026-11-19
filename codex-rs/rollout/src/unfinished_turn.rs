//! Daemon recovery checks the saved turn against the newest lifecycle events.
//! Only same-thread continuations contribute evidence about that turn.

use std::collections::HashSet;
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnContextItem;

use crate::ReverseJsonlScanner;
use crate::RolloutItem;
use crate::ScanOutcome;

/// Returns the last recorded context if the saved turn is still the newest
/// unfinished turn. Each referenced file is read only through its frozen end.
pub async fn unfinished_turn_context(
    codex_home: &Path,
    mut path: PathBuf,
    thread_id: ThreadId,
    turn_id: &str,
) -> io::Result<Option<TurnContextItem>> {
    let mut seen = HashSet::new();
    let mut end = None;
    let mut context = None;
    let mut found_start = false;
    loop {
        let meta = crate::read_session_meta_line(&path).await?.meta;
        if meta.id != thread_id {
            return Ok(None);
        }
        let target = turn_id.to_owned();
        let (stop, cancelled) = tokio::sync::oneshot::channel::<()>();
        let scanned = tokio::task::spawn_blocking(move || {
            let file = crate::open_rollout_seekable_reader(&path, || stop.is_closed())?;
            let mut scanner = match end {
                Some(offset) => ReverseJsonlScanner::new_at(file, offset)?,
                None => ReverseJsonlScanner::new(file)?,
            };
            while let Some(record) = scanner.scan_next_rollout_line()? {
                if stop.is_closed() {
                    return Err(io::Error::other("turn recovery cancelled"));
                }
                let ScanOutcome::Parsed(line) = record else {
                    continue;
                };
                match line.item {
                    RolloutItem::TurnContext(item)
                        if context.is_none()
                            && item.turn_id.as_deref() == Some(target.as_str()) =>
                    {
                        context = Some(item);
                        if found_start {
                            return Ok(ControlFlow::Break(context));
                        }
                    }
                    RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if !found_start => {
                        if event.turn_id != target {
                            return Ok(ControlFlow::Break(None));
                        }
                        if context.is_some() {
                            return Ok(ControlFlow::Break(context));
                        }
                        found_start = true;
                    }
                    RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                        if !found_start && event.turn_id == target =>
                    {
                        return Ok(ControlFlow::Break(None));
                    }
                    RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                        if !found_start
                            && event.turn_id.as_ref().is_none_or(|id| *id == target) =>
                    {
                        return Ok(ControlFlow::Break(None));
                    }
                    _ => {}
                }
            }
            Ok(ControlFlow::Continue((found_start, context)))
        })
        .await
        .map_err(io::Error::other)??;
        drop(cancelled);
        (found_start, context) = match scanned {
            ControlFlow::Break(result) => return Ok(result),
            ControlFlow::Continue(state) => state,
        };
        let Some(base) = meta.history_base else {
            return Ok(None);
        };
        if !seen.insert(base.thread_id) {
            return Err(io::Error::other("cycle in interrupted turn history"));
        }
        path = crate::find_rollout_path_by_rollout_id(codex_home, base.thread_id)
            .await?
            .ok_or_else(|| io::Error::other("missing interrupted turn history"))?;
        end = Some(base.end_byte_offset);
    }
}
