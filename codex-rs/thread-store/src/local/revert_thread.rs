use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;

use super::LocalThreadStore;
use super::paginated_fork;
use super::thread_rollout_resolver;
use crate::ForkBoundary;
use crate::RevertThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Revert an unloaded paginated thread by creating a new immutable rollout file.
///
/// Old rollouts stay intact. The new file references the retained prefix, and the only mutable
/// cutover is the existing SQLite rollout-path pointer for the thread.
pub(super) async fn revert(
    store: &LocalThreadStore,
    params: RevertThreadParams,
) -> ThreadStoreResult<()> {
    let RevertThreadParams {
        thread_id,
        before_turn_id,
        multi_agent_version,
    } = params;
    let state_db = store
        .state_db()
        .await
        .ok_or(ThreadStoreError::Unsupported {
            operation: "revert_thread",
        })?;
    let _lifecycle_guard = store.live_writer_locks.lock_lifecycle(thread_id).await;
    let _live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    store.ensure_live_recorder_absent(thread_id).await?;
    let writer_lock = store.acquire_writer_lock(thread_id)?;

    // Resolution may return a compressed sibling. Keep SQLite's exact stored path for the CAS.
    let stored_metadata = state_db
        .get_thread(thread_id)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read thread metadata for {thread_id}: {err}"),
        })?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let expected_sqlite_path = stored_metadata.rollout_path;
    let current_rollout = thread_rollout_resolver::resolve_current(store, thread_id)
        .await?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let source_path = current_rollout.path;
    super::helpers::recover_checkpoint(&source_path, &writer_lock).await?;
    let mut source_meta = codex_rollout::read_session_meta_line(source_path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read current paginated rollout {}: {err}",
                source_path.display()
            ),
        })?
        .meta;
    if source_meta.id != current_rollout.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("current rollout for {thread_id} belongs to another thread"),
        });
    }
    if source_meta.history_mode != ThreadHistoryMode::Paginated {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {thread_id} does not use paginated history"),
        });
    }

    // Older binaries can omit creator fields when replacing a rollout during revert.
    source_meta.creator_user_id = source_meta
        .creator_user_id
        .or(stored_metadata.creator_user_id);
    source_meta.creator_account_id = source_meta
        .creator_account_id
        .or(stored_metadata.creator_account_id);

    // Preserve old-reader compatibility when introducing the first reference to a standalone
    // source. Already-shared ancestors stay read-only; their offsets address decoded JSONL bytes.
    let lineage = store
        .resolve_rollout_lineage(thread_id, /*initial_path*/ None)
        .await?;
    let boundary = &ForkBoundary::BeforeTurn(before_turn_id);
    let standalone_rollout_id = source_meta
        .history_base
        .is_none()
        .then_some(current_rollout.rollout_id);
    let history_base = lineage
        .read_with_fallback(store, |mut lineage| async move {
            for segment in &mut lineage.segments {
                if Some(segment.rollout_id()) == standalone_rollout_id {
                    segment.rollout_path = codex_rollout::materialize_rollout_for_reference(
                        segment.rollout_path.as_path(),
                    )
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to materialize rollout {} for revert: {err}",
                            segment.rollout_path.display()
                        ),
                    })?;
                }
                super::thread_history_materialization::materialize_to_sqlite_with_reads(
                    store,
                    segment.rollout_id(),
                    segment.rollout_path.as_path(),
                    &lineage.reads,
                )
                .await?;
            }
            paginated_fork::history_base_at_boundary(store, thread_id, boundary, &lineage).await
        })
        .await?;

    let forked_from_ordinal_exclusive =
        codex_rollout::forked_from_ordinal_exclusive(&source_meta, Some(source_path.as_path()))
            .map(|cutoff| {
                // Reverting into inherited history can shrink, but never grow, the parent prefix.
                cutoff.min(history_base.map_or(0, |base| base.end_ordinal_exclusive))
            });

    source_meta.multi_agent_version = multi_agent_version.or(source_meta.multi_agent_version);
    let rollout_id = ThreadId::new();
    let recorder = super::rollout_continuation::create(
        store,
        source_meta,
        rollout_id,
        history_base,
        forked_from_ordinal_exclusive,
        writer_lock,
        None,
    )
    .await?;
    let replacement_path = recorder.rollout_path().to_path_buf();
    recorder.persist().await.map_err(thread_store_io_error)?;
    recorder.shutdown().await.map_err(thread_store_io_error)?;

    let replaced = state_db
        .replace_rollout_path_if_current(
            thread_id,
            expected_sqlite_path.as_path(),
            replacement_path.as_path(),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to switch thread {thread_id} to reverted rollout: {err}"),
        })?;
    if !replaced {
        let _ = tokio::fs::remove_file(replacement_path.as_path()).await;
        return Err(ThreadStoreError::Conflict {
            message: format!("thread {thread_id} changed while it was being reverted"),
        });
    }
    Ok(())
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}

#[cfg(test)]
#[path = "revert_thread_tests.rs"]
mod tests;
