//! Prepares an explicitly selected, unloaded paginated rollout for an offline split.
pub(in crate::local) mod cache;
mod suffix;

use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::SessionMeta;
use tokio::sync::OwnedMutexGuard;

use super::LocalThreadStore;
use super::WriterLockGuard;
use super::rollout_continuation;
use super::thread_history::projection_state;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use suffix::ColdSuffix;
use suffix::read_suffix;

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ColdSplitOutcome {
    Ineligible,
    Eligible {
        retained_bytes: u64,
    },
    Split {
        rollout_path: PathBuf,
        retained_bytes: u64,
    },
}

impl LocalThreadStore {
    /// Inspect one unloaded thread, or publish a native continuation with a local checkpoint copy.
    pub async fn split_closed_rollout(
        &self,
        thread: ThreadId,
        apply: bool,
    ) -> ThreadStoreResult<ColdSplitOutcome> {
        let store = self.clone();
        // Keep the writer lock through selection and reconciliation if the caller disconnects.
        tokio::spawn(async move {
            let Some(plan) = prepare(&store, thread).await? else {
                return Ok(ColdSplitOutcome::Ineligible);
            };
            let retained_bytes = plan.suffix.bytes.len() as u64;
            if !apply {
                return Ok(ColdSplitOutcome::Eligible { retained_bytes });
            }
            let rollout_path = plan.publish(&store).await?;
            Ok(ColdSplitOutcome::Split {
                rollout_path,
                retained_bytes,
            })
        })
        .await
        .map_err(split_error)?
    }
}

struct PreparedSplit {
    source_path: PathBuf,
    source_meta: SessionMeta,
    history_base: HistoryPosition,
    suffix: ColdSuffix,
    writer: WriterLockGuard,
    _local_lock: OwnedMutexGuard<u64>,
}

async fn prepare(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<Option<PreparedSplit>> {
    let local_lock = store.live_writer_locks.lock(thread_id).await;
    let writer = store.acquire_writer_lock(thread_id)?;
    let state = store
        .state_db
        .as_ref()
        .ok_or(ThreadStoreError::Unsupported {
            operation: "split_closed_rollout",
        })?;
    let row = state
        .get_thread(thread_id)
        .await
        .map_err(split_error)?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    if row.archived_at.is_some() || row.history_mode != ThreadHistoryMode::Paginated {
        return Ok(None);
    }
    let path = row.rollout_path;
    let Some(rollout_id) = codex_rollout::rollout_id_from_path(&path) else {
        return Ok(None);
    };
    if path.extension().is_none_or(|ext| ext != "jsonl")
        || !tokio::fs::symlink_metadata(&path)
            .await
            .map_err(split_error)?
            .is_file()
    {
        return Ok(None);
    }
    super::helpers::scoped_rollout_path(
        store.config.codex_home.join(codex_rollout::SESSIONS_SUBDIR),
        &path,
        "sessions",
    )?;
    codex_rollout::ensure_checkpoint_complete(&path).map_err(split_error)?;
    let Some(projection) = projection_state(store, rollout_id).await? else {
        return Ok(None);
    };
    let source_path = path.clone();
    let range = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(source_path)?;
        if file.metadata()?.len() != projection.next_byte_offset {
            return Ok(None);
        }
        use std::io::{BufRead, BufReader, Read};
        let mut header = Vec::new();
        BufReader::new((&mut file).take(suffix::MAX_RECORD_BYTES as u64))
            .read_until(b'\n', &mut header)?;
        if header.last() != Some(&b'\n') {
            return Ok(None);
        }
        let Ok(line) = codex_rollout::parse_rollout_line_bytes(&header) else {
            return Ok(None);
        };
        let (Some(ordinal), codex_rollout::RolloutItem::SessionMeta(meta)) =
            (line.ordinal, line.item)
        else {
            return Ok(None);
        };
        if meta.meta.id != thread_id || meta.meta.history_mode != ThreadHistoryMode::Paginated {
            return Ok(None);
        }
        Ok::<_, std::io::Error>(
            read_suffix(&mut file, thread_id)?
                .filter(|s| {
                    s.start > header.len() as u64
                        && s.first_ordinal > ordinal.saturating_add(1)
                        && s.end_ordinal_exclusive == projection.next_ordinal
                })
                .map(|suffix| (meta.meta, suffix)),
        )
    })
    .await
    .map_err(split_error)?
    .map_err(split_error)?;
    let Some((meta, suffix)) = range else {
        return Ok(None);
    };
    Ok(Some(PreparedSplit {
        source_path: path,
        source_meta: meta,
        history_base: HistoryPosition {
            thread_id: rollout_id,
            end_byte_offset: projection.next_byte_offset,
            end_ordinal_exclusive: projection.next_ordinal,
        },
        suffix,
        writer,
        _local_lock: local_lock,
    }))
}

impl PreparedSplit {
    async fn publish(self, store: &LocalThreadStore) -> ThreadStoreResult<PathBuf> {
        let state = store
            .state_db
            .as_ref()
            .ok_or(ThreadStoreError::Unsupported {
                operation: "split_closed_rollout",
            })?;
        let owner = self.source_meta.id;
        let id = ThreadId::new();
        let base = self.history_base;
        let timestamp = self.source_meta.timestamp.clone();
        let fork_cutoff = codex_rollout::forked_from_ordinal_exclusive(
            &self.source_meta,
            Some(&self.source_path),
        );
        let next = rollout_continuation::create(
            store,
            self.source_meta,
            id,
            Some(base),
            fork_cutoff,
            self.writer.clone(),
            Some(timestamp),
        )
        .await?;
        let path = next.rollout_path().to_path_buf();
        let staged = async {
            next.stage_continuation(Vec::new())
                .await
                .map_err(split_error)?;
            let head = path.clone();
            let tail = self.suffix.bytes;
            tokio::task::spawn_blocking(move || cache::write(&head, owner, id, base, &tail))
                .await
                .map_err(split_error)?
                .map_err(split_error)?;
            next.shutdown().await.map_err(split_error)?;
            super::thread_history_materialization::materialize_to_sqlite(store, id, &path).await?;
            Ok::<_, ThreadStoreError>(())
        }
        .await;
        let selection = match staged {
            Ok(()) => state
                .replace_rollout_path_if_current(owner, &self.source_path, &path)
                .await
                .map_err(split_error),
            Err(error) => Err(error),
        };
        match selection {
            Ok(true) => Ok(path),
            outcome => {
                let selected = state
                    .get_thread(owner)
                    .await
                    .map_err(split_error)?
                    .ok_or(ThreadStoreError::ThreadNotFound { thread_id: owner })?;
                if selected.rollout_path == path {
                    return Ok(path);
                }
                if selected.rollout_path == self.source_path {
                    let _ = next.discard().await;
                    super::thread_history::delete_thread(store, id).await?;
                    for file in [cache::path(&path), path] {
                        match tokio::fs::remove_file(file).await {
                            Ok(()) => {}
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                            Err(err) => return Err(split_error(err)),
                        }
                    }
                }
                Err(outcome.err().unwrap_or_else(|| ThreadStoreError::Conflict {
                    message: "rollout selection changed while splitting".into(),
                }))
            }
        }
    }
}

fn split_error(err: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("could not split rollout: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::test_support::test_config;

    #[tokio::test]
    async fn busy_thread_is_declined_before_history_is_opened() {
        let home = tempfile::tempdir().unwrap();
        let config = test_config(home.path());
        let owner = LocalThreadStore::new(config.clone(), None);
        let operator = LocalThreadStore::new(config, None);
        let thread = ThreadId::new();
        let guard = owner.acquire_writer_lock(thread).unwrap();
        assert!(matches!(
            prepare(&operator, thread).await,
            Err(ThreadStoreError::Conflict { .. })
        ));
        drop(guard);
        assert!(matches!(
            prepare(&operator, thread).await,
            Err(ThreadStoreError::Unsupported { .. })
        ));
    }
}
