//! Seals completed paginated prefixes behind native history references.
//!
//! A new physical rollout gets its own projection cursor. The old rollout is never
//! truncated or rewritten. The existing SQLite selected-path update is the commit
//! point, as it is for native revert. Callers hold the thread's writer lock and run
//! publication to completion even if the initiating request is cancelled.

use super::*;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;

/// Recognizes native complete checkpoint batches; older partial compactions stay inline.
pub(super) fn is_checkpoint(items: &[RolloutItem]) -> bool {
    matches!(items.first(), Some(RolloutItem::Compacted(item))
        if item.replacement_history.is_some()
            && item.window_number.is_some()
            && item.resume_metadata.as_ref().is_some_and(|metadata|
                metadata.dynamic_tools.is_some()))
        && matches!(
            items.last(),
            Some(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)))
        )
}

/// Reuses an identical terminal checkpoint without writing another copy for a repeated fork.
/// The comparison includes all companions and durable ordinals; any intervening item prevents reuse.
pub(super) async fn matches_tail(
    path: &std::path::Path,
    items: &[RolloutItem],
) -> ThreadStoreResult<bool> {
    if !tokio::fs::try_exists(path)
        .await
        .map_err(thread_store_io_error)?
    {
        return Ok(false);
    }
    let Some(previous) = super::super::model_context::scan_current_checkpoint(path).await? else {
        return Ok(false);
    };
    if previous.len() != items.len() {
        return Ok(false);
    }
    let encode = |items: &[RolloutItem]| {
        serde_json::to_vec(items).map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })
    };
    Ok(encode(&previous)? == encode(items)?)
}

/// Rotates only when the sealed prefix is materially larger than its restart state.
///
/// The batch has already been flushed to the old file. A failure before the path swap
/// leaves that file authoritative; a failure after it cannot lose the batch.
/// The duplicate checkpoint is bounded by current context, not lifetime history.
pub(super) async fn rotate(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    recorder: &RolloutRecorder,
    rollout_id: ThreadId,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    let Some(state_db) = store.state_db().await else {
        return Ok(());
    };
    let checkpoint_bytes = serde_json::to_vec(items)
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?
        .len() as u64;
    let source_path = recorder.rollout_path();
    let file_bytes = tokio::fs::metadata(source_path)
        .await
        .map_err(thread_store_io_error)?
        .len();
    if file_bytes
        < checkpoint_bytes
            .saturating_mul(4)
            .saturating_add(1024 * 1024)
    {
        return Ok(());
    }
    let Some(projection) =
        super::super::thread_history::projection_state(store, rollout_id).await?
    else {
        return Ok(());
    };
    if projection.next_byte_offset != file_bytes || !terminal_turn(source_path).await? {
        return Ok(());
    }
    let meta = codex_rollout::read_session_meta_line(source_path)
        .await
        .map_err(thread_store_io_error)?
        .meta;
    let fork_cutoff = codex_rollout::forked_from_ordinal_exclusive(&meta, Some(source_path));
    let writer_lock = store
        .live_recorders
        .lock()
        .await
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?
        .writer_lock
        .clone();
    let session_timestamp = meta.timestamp.clone();
    let next_id = ThreadId::new();
    let next = super::super::rollout_continuation::create(
        store,
        meta,
        next_id,
        Some(HistoryPosition {
            thread_id: rollout_id,
            end_ordinal_exclusive: projection.next_ordinal,
            end_byte_offset: projection.next_byte_offset,
        }),
        fork_cutoff,
        writer_lock,
        Some(session_timestamp),
    )
    .await?;
    let publication = async {
        next.record_canonical_items(items)
            .await
            .map_err(thread_store_io_error)?;
        next.persist().await.map_err(thread_store_io_error)?;
        next.flush().await.map_err(thread_store_io_error)?;
        state_db
            .replace_rollout_path_if_current(thread_id, source_path, next.rollout_path())
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to publish continuation: {error}"),
            })
    }
    .await;
    let publication = publication.and_then(|published| {
        if published {
            Ok(())
        } else {
            Err(ThreadStoreError::Conflict {
                message: "thread changed during segment publication".into(),
            })
        }
    });
    if let Err(error) = publication {
        // A failed response does not establish whether SQLite selected the new file.
        // Preserve both files unless the authoritative path resolves the outcome.
        match state_db.get_thread(thread_id).await {
            Ok(Some(selected)) if selected.rollout_path == next.rollout_path() => {}
            Ok(Some(selected)) if selected.rollout_path == source_path => {
                next.discard().await.map_err(thread_store_io_error)?;
                return Err(error);
            }
            _ => {
                store.live_recorders.lock().await.remove(&thread_id);
                let _ = recorder.shutdown().await;
                let _ = next.shutdown().await;
                return Err(error);
            }
        }
    }
    {
        let mut entries = store.live_recorders.lock().await;
        let entry = entries
            .get_mut(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        entry.recorder = next.clone();
        entry.rollout_id = next_id;
        entry
            .rollout_path
            .send_replace(next.rollout_path().to_path_buf());
    }
    // The new path is committed and installed before stopping the former writer.
    if let Err(error) = recorder.shutdown().await {
        warn!("failed to close sealed rollout for {thread_id}: {error}");
    }
    if let Err(error) = super::super::thread_history_materialization::materialize_to_sqlite(
        store,
        next_id,
        next.rollout_path(),
    )
    .await
    {
        warn!("failed to project continuation for {thread_id}: {error}");
    }
    Ok(())
}

/// A checkpoint may be captured for an active fork. Only a persisted terminal turn
/// permits sealing: vanilla daemon recovery reads the active file's lifecycle tail.
async fn terminal_turn(path: &std::path::Path) -> ThreadStoreResult<bool> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(path)?;
        let mut scanner = codex_rollout::ReverseJsonlScanner::new(file)?;
        let mut completed = std::collections::HashSet::new();
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            let codex_rollout::ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            match line.item {
                RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                    completed.insert(event.turn_id);
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                    let Some(turn_id) = event.turn_id else {
                        return Ok(false);
                    };
                    completed.insert(turn_id);
                }
                RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                    // A predecessor's stop hook may complete after this turn started.
                    return Ok(completed.contains(&event.turn_id));
                }
                RolloutItem::SessionMeta(_) => return Ok(!completed.is_empty()),
                _ => {}
            }
        }
        Ok::<_, std::io::Error>(false)
    })
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("failed to join terminal turn scan: {error}"),
    })?
    .map_err(thread_store_io_error)
}

#[cfg(test)]
mod tests;
