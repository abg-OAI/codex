use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;

use super::LocalThreadStore;
use super::legacy_event;
use super::migration_error;
use crate::ThreadStoreResult;
use crate::local::helpers;
use crate::local::thread_history_materialization;
use crate::local::thread_rollout_resolver;

impl LocalThreadStore {
    /// Converts the display events of an external conversation into paginated items.
    /// Reimporting the same prefix must produce the same item IDs.
    pub fn paginate_import_messages(
        thread_id: ThreadId,
        records: Vec<RolloutItem>,
    ) -> ThreadStoreResult<Vec<RolloutItem>> {
        let mut turn = None;
        let mut records = records.into_iter().enumerate().peekable();
        let mut output = Vec::with_capacity(records.len());
        while let Some((position, record)) = records.next() {
            let (event, metadata) = match record {
                RolloutItem::EventMsg { event, metadata } => (event, metadata),
                other => {
                    output.push(other);
                    continue;
                }
            };
            match &event {
                EventMsg::TurnStarted(start) => turn = Some(start.turn_id.clone()),
                EventMsg::TurnComplete(_) => turn = None,
                EventMsg::UserMessage(_) | EventMsg::AgentMessage(_) => {
                    let turn_id = turn
                        .clone()
                        .ok_or_else(|| migration_error("imported message outside a turn"))?;
                    let has_model_record =
                        matches!(records.peek(), Some((_, RolloutItem::ResponseItem(_))));
                    let mut item_id = || {
                        Ok(if has_model_record {
                            format!("external-import-item-{position}")
                        } else {
                            format!("external-import-notice-{position}")
                        })
                    };
                    let item = match event {
                        EventMsg::UserMessage(user) => {
                            legacy_event::user_message_item(user, &mut item_id)?
                        }
                        agent => {
                            legacy_event::completed_item(&agent, &mut item_id)?
                                .ok_or_else(|| {
                                    migration_error("invalid imported assistant message")
                                })?
                                .0
                        }
                    };
                    output.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                        ItemCompletedEvent {
                            thread_id,
                            turn_id,
                            item,
                            started_at_ms: None,
                            completed_at_ms: 0,
                            content_token_count: None,
                        },
                    )));
                    continue;
                }
                _ => {}
            }
            output.push(RolloutItem::EventMsg { event, metadata });
        }
        Ok(output)
    }

    /// Extends a closed imported root while its recorder is exclusively owned.
    pub async fn extend_paginated_import(
        &self,
        id: ThreadId,
        source: Vec<RolloutItem>,
    ) -> ThreadStoreResult<bool> {
        let store = self.clone();
        tokio::spawn(async move {
            let _local = store.live_writer_locks.lock(id).await;
            store.ensure_live_recorder_absent(id).await?;
            let writer = store.acquire_writer_lock(id)?;
            let Some(head) = thread_rollout_resolver::resolve_current(&store, id).await? else {
                return Ok(false);
            };
            if head.rollout_id != id {
                return Ok(false);
            }
            helpers::recover_checkpoint(&head.path, &writer).await?;
            let path = head.path.clone();
            let plan = tokio::task::spawn_blocking(move || {
                let source = Self::paginate_import_messages(id, source)?;
                imported_suffix(id, &path, source)
            })
            .await
            .map_err(migration_error)??;
            let Some(ImportExtension {
                meta,
                mut suffix,
                next_model_id,
            }) = plan
            else {
                return Ok(false);
            };
            let Some(provider) = meta.model_provider.filter(|s| !s.trim().is_empty()) else {
                return Ok(false);
            };
            if meta.cwd.as_os_str().is_empty() {
                return Ok(false);
            }
            let config = RolloutConfig {
                codex_home: store.config.codex_home.clone(),
                sqlite: store.config.sqlite.clone(),
                cwd: meta.cwd,
                model_provider_id: provider,
                generate_memories: match meta.memory_mode.as_deref() {
                    Some("disabled") => false,
                    None | Some("enabled") => true,
                    _ => return Ok(false),
                },
            };
            codex_rollout::stamp_item_ordinals(next_model_id, &mut suffix);
            let recorder = RolloutRecorder::new_with_writer_lock(
                &config,
                RolloutRecorderParams::resume(head.path),
                writer.clone(),
            )
            .await
            .map_err(migration_error)?;
            recorder
                .record_canonical_items(&suffix)
                .await
                .map_err(migration_error)?;
            recorder.flush().await.map_err(migration_error)?;
            recorder.shutdown().await.map_err(migration_error)?;
            thread_history_materialization::materialize_to_sqlite(
                &store,
                id,
                recorder.rollout_path(),
            )
            .await?;
            Ok(true)
        })
        .await
        .map_err(migration_error)?
    }
}

fn notice(record: &RolloutItem) -> bool {
    matches!(record, RolloutItem::EventMsg { event: EventMsg::ItemCompleted(event), .. } if event.item.id().starts_with("external-import-notice-"))
}

fn transcript_record(record: &RolloutItem) -> bool {
    matches!(
        record,
        RolloutItem::ResponseItem(_)
            | RolloutItem::EventMsg {
                event: EventMsg::ItemCompleted(_),
                ..
            }
    ) && !notice(record)
}

struct ImportExtension {
    meta: SessionMeta,
    suffix: Vec<RolloutItem>,
    next_model_id: u64,
}

fn imported_suffix(
    id: ThreadId,
    path: &Path,
    source: Vec<RolloutItem>,
) -> ThreadStoreResult<Option<ImportExtension>> {
    let reader = codex_rollout::open_rollout_stream(path).map_err(migration_error)?;
    let mut reader = BufReader::new(reader);
    let mut expected = source
        .iter()
        .enumerate()
        .filter(|(_, r)| transcript_record(r));
    let mut header = None;
    let mut next_model_id = 0;
    let mut found_notice = false;
    let mut ends_at_completion = false;
    let mut buffer = Vec::new();
    for ordinal in 0.. {
        buffer.clear();
        if reader
            .read_until(b'\n', &mut buffer)
            .map_err(migration_error)?
            == 0
        {
            break;
        }
        if !buffer.ends_with(b"\n") {
            return Ok(None);
        }
        let line = codex_rollout::parse_rollout_line_bytes(&buffer).map_err(migration_error)?;
        if line.ordinal != Some(ordinal) {
            return Ok(None);
        }
        next_model_id = next_model_id
            .max(codex_rollout::next_model_id_ordinals(std::slice::from_ref(&line.item)).0);
        if ordinal == 0 && !matches!(&line.item, RolloutItem::SessionMeta(_)) {
            return Ok(None);
        }
        match line.item {
            RolloutItem::SessionMeta(line)
                if line.meta.id == id
                    && line.meta.history_base.is_none()
                    && line.meta.forked_from_id.is_none()
                    && line.meta.history_mode == ThreadHistoryMode::Paginated =>
            {
                header = Some(line.meta)
            }
            RolloutItem::EventMsg {
                event: EventMsg::TurnComplete(_),
                ..
            } => ends_at_completion = true,
            RolloutItem::EventMsg {
                event: EventMsg::TurnStarted(_),
                ..
            } => ends_at_completion = false,
            // The original import notice remains in its original turn when the source grows.
            ref record if notice(record) && !found_notice => {
                found_notice = true;
                ends_at_completion = false;
            }
            RolloutItem::ResponseItem(old) => {
                let Some((_, RolloutItem::ResponseItem(new))) = expected.next() else {
                    return Ok(None);
                };
                if old.item != new.item {
                    return Ok(None);
                }
                ends_at_completion = false;
            }
            RolloutItem::EventMsg {
                event: EventMsg::ItemCompleted(old),
                ..
            } => {
                let Some((
                    _,
                    RolloutItem::EventMsg {
                        event: EventMsg::ItemCompleted(new),
                        ..
                    },
                )) = expected.next()
                else {
                    return Ok(None);
                };
                if old.thread_id != new.thread_id
                    || old.turn_id != new.turn_id
                    || serde_json::to_value(&old.item).map_err(migration_error)?
                        != serde_json::to_value(&new.item).map_err(migration_error)?
                {
                    return Ok(None);
                }
                ends_at_completion = false;
            }
            RolloutItem::EventMsg {
                event: EventMsg::TokenCount(_),
                ..
            }
            | RolloutItem::TokenUsageRecord(_) => {}
            _ => return Ok(None),
        }
    }
    if !found_notice || !ends_at_completion {
        return Ok(None);
    }
    let (
        Some(meta),
        Some((
            index,
            RolloutItem::EventMsg {
                event: EventMsg::ItemCompleted(_),
                ..
            },
        )),
    ) = (header, expected.next())
    else {
        return Ok(None);
    };
    let start = index
        .checked_sub(1)
        .filter(|&i| {
            matches!(
                &source[i],
                RolloutItem::EventMsg {
                    event: EventMsg::TurnStarted(_),
                    ..
                }
            )
        })
        .unwrap_or(index);
    let suffix = source
        .into_iter()
        .skip(start)
        .filter(|r| !notice(r))
        .collect();
    Ok(Some(ImportExtension {
        meta,
        suffix: codex_rollout::into_persisted_rollout_items(suffix, ThreadHistoryMode::Paginated),
        next_model_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paginating_imported_events_keeps_rejected_audio_ids() {
        let wire = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn-1",
                "last_agent_message": null,
                "error": {
                    "message": "Invalid recording",
                    "codex_error_info": "bad_request",
                    "rejected_audio_file_ids": ["audio-1"]
                }
            }
        });
        let record = serde_json::from_value(wire.clone()).unwrap();
        let output =
            LocalThreadStore::paginate_import_messages(ThreadId::new(), vec![record]).unwrap();
        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::json!([wire])
        );
    }
}
