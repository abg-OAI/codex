//! Seals terminal paginated prefixes at checkpoint operations behind native references.
//!
//! Capture follows fork initialization and native compaction, not ordinary turn
//! completion. Active compaction checkpoints stay inline: vanilla recovery reads
//! lifecycle records from the selected file. Native revert owns rollback sealing.
//!
//! A new physical rollout gets its own projection cursor. The old rollout is never
//! truncated or rewritten. The existing SQLite selected-path update is the commit
//! point, as it is for native revert. Callers hold the thread's writer lock and run
//! publication to completion even if the initiating request is cancelled. If the
//! selected path cannot be read after an error, the live writer owns both recorders
//! and reconciles before any later append, flush, or shutdown. Discard stops both
//! actors and preserves their files for recovery.

use super::*;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;

/// Recognizes batches eligible for physical rotation, including persisted settings.
/// Native model-context reads stop at replacement history with a window number;
/// they do not require these additional write-batch companions.
pub(super) fn is_rotation_checkpoint(items: &[RolloutItem]) -> bool {
    matches!(items.first(), Some(RolloutItem::Compacted(item))
        if item.replacement_history.is_some()
            && item.window_number.is_some()
            && item.resume_metadata.as_ref().is_some_and(|metadata|
                metadata.dynamic_tools.is_some()))
        && matches!(
            items.last(),
            Some(RolloutItem::EventMsg {
                event: EventMsg::ThreadSettingsApplied(_),
                ..
            })
        )
}

/// Reuses an identical terminal checkpoint when its producer retries the same snapshot.
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

/// Rotates an eligible operation checkpoint once the persisted turn is terminal.
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
    let source_path = recorder.rollout_path();
    let file_bytes = tokio::fs::metadata(source_path)
        .await
        .map_err(thread_store_io_error)?
        .len();
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
    {
        let mut entries = store.live_recorders.lock().await;
        let entry = entries
            .get_mut(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        entry.pending_continuation = Some(super::super::PendingContinuation {
            recorder: next.clone(),
            rollout_id: next_id,
        });
    }
    match publication {
        Ok(()) => install_continuation(store, thread_id).await?,
        Err(error) => {
            // A failed response does not establish which path SQLite selected.
            // Retain ownership and block later writes until selection can be read.
            reconcile_publication(store, thread_id).await?;
            if live_writer_parts(store, thread_id).await?.1 != next_id {
                return Err(error);
            }
        }
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

/// Reconciles before any later mutation under the per-thread writer lock.
/// An unreadable or unexpected selection preserves both files and both recorder
/// handles. No append or path synchronization may choose a writer in that state.
pub(super) async fn reconcile_publication(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let (source_path, pending) = {
        let entries = store.live_recorders.lock().await;
        let Some(entry) = entries.get(&thread_id) else {
            return Ok(());
        };
        let Some(pending) = entry.pending_continuation.clone() else {
            return Ok(());
        };
        (entry.recorder.rollout_path().to_path_buf(), pending)
    };
    let state = store
        .state_db()
        .await
        .ok_or(ThreadStoreError::Unsupported {
            operation: "continuation_publication",
        })?;
    let selected = state
        .get_thread(thread_id)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!("cannot reconcile continuation publication: {error}"),
        })?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    if selected.rollout_path == pending.recorder.rollout_path() {
        install_continuation(store, thread_id).await
    } else if selected.rollout_path == source_path {
        // Discard stops the actor; it does not remove an already persisted file.
        // A retry can encounter an actor stopped by an earlier cancelled cleanup.
        if let Err(error) = pending.recorder.discard().await {
            warn!("failed to stop unselected continuation for {thread_id}: {error}");
        }
        match tokio::fs::remove_file(pending.recorder.rollout_path()).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(thread_store_io_error(error)),
        }
        if let Some(entry) = store.live_recorders.lock().await.get_mut(&thread_id) {
            entry.pending_continuation = None;
        }
        Ok(())
    } else {
        Err(ThreadStoreError::Conflict {
            message: "thread selected an unexpected path during continuation publication".into(),
        })
    }
}

async fn install_continuation(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let former = {
        let mut entries = store.live_recorders.lock().await;
        let entry = entries
            .get_mut(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        let Some(pending) = entry.pending_continuation.take() else {
            return Ok(());
        };
        entry.rollout_id = pending.rollout_id;
        entry
            .rollout_path
            .send_replace(pending.recorder.rollout_path().to_path_buf());
        std::mem::replace(&mut entry.recorder, pending.recorder)
    };
    // The selected writer is installed before the former writer is stopped.
    if let Err(error) = former.shutdown().await {
        warn!("failed to close sealed rollout for {thread_id}: {error}");
    }
    Ok(())
}

/// Only a persisted terminal turn permits sealing. Active snapshots supplied by
/// other persistence callers stay inline for vanilla daemon recovery.
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
