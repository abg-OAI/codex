//! Reads immutable realtime snapshots by observation time or committed position.
//! Cursors retain the query and snapshot bound so late seals appear in catch-up reads.

use codex_app_server_protocol::ThreadRealtimeCoverage;
use codex_app_server_protocol::ThreadRealtimeEntry;
use codex_app_server_protocol::ThreadRealtimeItem;
use codex_app_server_protocol::ThreadRealtimeOrder;
use codex_protocol::ThreadId;
use codex_protocol::realtime::RealtimeItem;
use serde::Deserialize;
use serde::Serialize;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

use super::super::LocalThreadStore;
use super::read::validate_thread_for_paginated_reads;
use super::sqlite_integer;
use super::thread_history_error;
use crate::ListRealtimeParams;
use crate::RealtimePage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

const OBSERVED_AT: &str = "json_extract(item_json, '$.observed_at_ms')";

#[derive(Deserialize, Serialize)]
struct Cursor {
    version: u8,
    rollout_id: ThreadId,
    query: ListRealtimeParams,
    snapshot_end: u64,
    observed_at_ms: Option<i64>,
    position: u64,
}

pub(in crate::local) async fn list_realtime(
    store: &LocalThreadStore,
    params: ListRealtimeParams,
) -> ThreadStoreResult<RealtimePage> {
    if params.page_size == 0 || params.page_size > 500 {
        return Err(invalid("realtime page size must be between 1 and 500"));
    }
    if params
        .from_observed_at_ms
        .zip(params.to_observed_at_ms)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(invalid("realtime observation time range must be nonempty"));
    }
    let _writer_guard = store.live_writer_locks.lock(params.thread_id).await;
    let snapshot = match realtime_snapshot(store, params.thread_id, RealtimeRead::List).await {
        Ok(snapshot) => snapshot,
        Err(ThreadStoreError::Unsupported { .. }) => {
            return Ok(empty_page(ThreadRealtimeCoverage::Unsupported, 0, 0));
        }
        Err(error) => return Err(error),
    };
    read_page(
        store.thread_history_db().await?,
        &store.config.codex_home,
        params,
        &snapshot,
    )
    .await
}

struct RealtimeSnapshot {
    head: ThreadId,
    projected_end: u64,
    ranges: Vec<RealtimeRange>,
}

#[derive(Serialize)]
struct RealtimeRange {
    rollout: ThreadId,
    start: u64,
    end: Option<u64>,
}

impl RealtimeSnapshot {
    fn rows(&self, select: &'static str) -> ThreadStoreResult<QueryBuilder<Sqlite>> {
        let ranges = serde_json::to_string(&self.ranges).map_err(thread_history_error)?;
        let mut query = QueryBuilder::new(select);
        query.push(" FROM json_each(").push_bind(ranges).push(
            ") AS r JOIN thread_realtime_items ON thread_id = json_extract(r.value, '$.rollout')
             AND rollout_ordinal >= json_extract(r.value, '$.start')
             AND (json_extract(r.value, '$.end') IS NULL OR rollout_ordinal < json_extract(r.value, '$.end'))",
        );
        Ok(query)
    }
}

enum RealtimeRead {
    List,
    Recover,
}

/// Under the writer lock, collect this logical thread's visible ranges up to its fork boundary.
async fn realtime_snapshot(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    read: RealtimeRead,
) -> ThreadStoreResult<RealtimeSnapshot> {
    let resolved =
        super::super::thread_rollout_resolver::resolve_current_including_archived(store, thread_id)
            .await?;
    // A new paginated thread can have a writer before its first durable record.
    let mut snapshot = RealtimeSnapshot {
        head: resolved.as_ref().map_or(thread_id, |head| head.rollout_id),
        projected_end: 0,
        ranges: Vec::new(),
    };
    let operation = match read {
        RealtimeRead::List => "thread/realtime/list",
        RealtimeRead::Recover if resolved.is_none() => return Ok(snapshot),
        RealtimeRead::Recover => "load_realtime_session",
    };
    validate_thread_for_paginated_reads(store, thread_id, true, operation).await?;
    if resolved.is_none() {
        return Ok(snapshot);
    }
    let mut lineage =
        super::super::rollout_lineage::LineageWalk::for_history_page(store, thread_id);
    let mut depth = 0;
    while let Some((segment, _)) = lineage.at_depth(depth).await? {
        let meta = codex_rollout::read_session_meta_line(&segment.rollout_path)
            .await
            .map_err(thread_history_error)?
            .meta;
        if meta.id != thread_id {
            break;
        }
        let fork_cutoff =
            codex_rollout::forked_from_ordinal_exclusive(&meta, Some(&segment.rollout_path));
        if meta.forked_from_id.is_some()
            && meta.history_base.is_none()
            && meta.subagent_history_start_ordinal.is_none()
        {
            return Err(ThreadStoreError::Unsupported { operation });
        }
        super::super::thread_history_materialization::materialize_to_sqlite(
            store,
            segment.rollout_id,
            &segment.rollout_path,
        )
        .await?;
        if depth == 0 {
            snapshot.projected_end = super::projection_state(store, segment.rollout_id)
                .await?
                .map_or(0, |state| state.next_ordinal);
        }
        snapshot.ranges.push(RealtimeRange {
            rollout: segment.rollout_id,
            start: segment
                .start_ordinal
                .max(meta.subagent_history_start_ordinal.unwrap_or(0)),
            end: segment.end_ordinal(),
        });
        if meta.history_base.is_none_or(|base| {
            fork_cutoff.is_some_and(|cutoff| base.end_ordinal_exclusive <= cutoff)
        }) {
            break;
        }
        depth += 1;
    }
    Ok(snapshot)
}

/// Recover a logical call from the same authoritative durable-prefix projection as list_realtime.
pub(in crate::local) async fn load_realtime_session(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    session_id: String,
) -> ThreadStoreResult<Option<Vec<RealtimeItem>>> {
    let _writer_guard = store.live_writer_locks.lock(thread_id).await;
    let snapshot = realtime_snapshot(store, thread_id, RealtimeRead::Recover).await?;
    let rows = snapshot
        .rows("SELECT item_json")?
        .push(" WHERE rollout_ordinal < ")
        .push_bind(sqlite_integer(
            snapshot.projected_end,
            "realtime snapshot end",
        )?)
        .push(" AND json_extract(item_json, '$.realtime_session_id') = ")
        .push_bind(session_id)
        .push(" ORDER BY rollout_ordinal")
        .build_query_scalar::<String>()
        .fetch_all(store.thread_history_db().await?)
        .await
        .map_err(thread_history_error)?;
    let items: Vec<RealtimeItem> = rows
        .into_iter()
        .map(|row| serde_json::from_str(&row).map_err(thread_history_error))
        .collect::<ThreadStoreResult<_>>()?;
    Ok(items
        .iter()
        .any(|item| {
            matches!(
                item.content,
                codex_protocol::realtime::RealtimeItemContent::RealtimeSessionStarted
            )
        })
        .then_some(items))
}

pub(in crate::local) async fn load_realtime_session_head(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<Option<String>> {
    let _writer_guard = store.live_writer_locks.lock(thread_id).await;
    let snapshot = realtime_snapshot(store, thread_id, RealtimeRead::Recover).await?;
    snapshot
        .rows("SELECT json_extract(item_json, '$.realtime_session_id')")?
        .push(" WHERE rollout_ordinal < ")
        .push_bind(sqlite_integer(
            snapshot.projected_end,
            "realtime snapshot end",
        )?)
        .push(" AND item_type = 'realtime_session_started' ORDER BY rollout_ordinal DESC LIMIT 1")
        .build_query_scalar()
        .fetch_optional(store.thread_history_db().await?)
        .await
        .map_err(thread_history_error)
}

/// A canonical start marker makes enrollment sticky when the rollout flag is disabled.
pub(in crate::local) async fn has_canonical_realtime_session(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<bool> {
    let _writer_guard = store.live_writer_locks.lock(thread_id).await;
    let snapshot = match realtime_snapshot(store, thread_id, RealtimeRead::Recover).await {
        Err(ThreadStoreError::Unsupported {
            operation: "load_realtime_session",
        }) => return Ok(false),
        result => result?,
    };
    let exists = snapshot.rows("SELECT 1")?
        .push(" WHERE rollout_ordinal < ")
        .push_bind(sqlite_integer(snapshot.projected_end, "realtime snapshot end")?)
        .push(" AND item_type = 'realtime_session_started' AND json_extract(item_json, '$.observed_at_ms') IS NOT NULL LIMIT 1")
        .build_query_scalar::<i64>()
        .fetch_optional(store.thread_history_db().await?)
        .await
        .map_err(thread_history_error)?;
    Ok(exists.is_some())
}

async fn read_page(
    pool: &sqlx::SqlitePool,
    codex_home: &std::path::Path,
    params: ListRealtimeParams,
    snapshot: &RealtimeSnapshot,
) -> ThreadStoreResult<RealtimePage> {
    let projected_end = snapshot.projected_end;
    let mut query = params.clone();
    query.cursor = None;
    query.page_size = 0;
    // The effective snapshot is bound independently, so callers can omit it on later pages.
    query.snapshot_end = None;
    let cursor = params
        .cursor
        .as_deref()
        .map(serde_json::from_str::<Cursor>)
        .transpose()
        .map_err(|_| invalid("invalid realtime cursor"))?;
    if let Some(cursor) = &cursor
        && (cursor.version != 1
            || cursor.query != query
            || params
                .snapshot_end
                .is_some_and(|end| end != cursor.snapshot_end)
            || !snapshot.ranges.iter().any(|span| {
                cursor.position >= span.start && span.end.is_none_or(|end| cursor.position < end)
            })
            || cursor.position >= cursor.snapshot_end)
    {
        return Err(invalid("realtime cursor does not match this query"));
    }
    if let Some(cursor) = &cursor
        && !snapshot.ranges.iter().any(|span| {
            span.rollout == cursor.rollout_id
                && span.end.is_none_or(|end| end >= cursor.snapshot_end)
        })
    {
        let sealed_ranges = snapshot
            .ranges
            .iter()
            .filter_map(|span| Some((span.rollout, span.start, span.end?)));
        if !super::super::rollout_archive::contains_snapshot(
            codex_home,
            params.thread_id,
            cursor.rollout_id,
            cursor.snapshot_end,
            sealed_ranges,
        )
        .await?
        {
            return Err(invalid("realtime cursor does not match this query"));
        }
    }
    let snapshot_end = cursor
        .as_ref()
        .map(|cursor| cursor.snapshot_end)
        .or(params.snapshot_end)
        .unwrap_or(projected_end);
    let exact_id_uses_current_snapshot =
        params.event_id.is_some() && cursor.is_none() && params.snapshot_end.is_none();
    if projected_end < snapshot_end
        || params
            .required_through_position
            .is_some_and(|position| projected_end <= position)
    {
        return Ok(empty_page(
            ThreadRealtimeCoverage::Pending,
            snapshot_end,
            projected_end,
        ));
    }
    let snapshot_end_sql = sqlite_integer(snapshot_end, "snapshot end")?;
    let mut sql = snapshot.rows("SELECT rollout_ordinal, item_json")?;
    // Only new, enrolled calls write a canonical timestamp.
    sql.push(" WHERE ")
        .push(OBSERVED_AT)
        .push(" IS NOT NULL AND item_type != 'transcript_segment'");
    if !exact_id_uses_current_snapshot {
        sql.push(" AND rollout_ordinal < ")
            .push_bind(snapshot_end_sql);
    }
    if let Some(after) = params.after_position {
        sql.push(" AND rollout_ordinal > ")
            .push_bind(sqlite_integer(after, "after position")?);
    }
    if let Some(session) = &params.session_id {
        sql.push(" AND json_extract(item_json, '$.realtime_session_id') = ")
            .push_bind(session);
    }
    if let Some(event_id) = &params.event_id {
        sql.push(" AND item_id = ").push_bind(event_id);
    }
    if let Some(from) = params.from_observed_at_ms {
        sql.push(" AND ")
            .push(OBSERVED_AT)
            .push(" >= ")
            .push_bind(from);
    }
    if let Some(to) = params.to_observed_at_ms {
        sql.push(" AND ")
            .push(OBSERVED_AT)
            .push(" < ")
            .push_bind(to);
    }
    if let Some(cursor) = &cursor {
        match params.order {
            ThreadRealtimeOrder::ObservedAt => {
                sql.push(" AND (")
                    .push(OBSERVED_AT)
                    .push(", rollout_ordinal) > (")
                    .push_bind(cursor.observed_at_ms)
                    .push(", ")
                    .push_bind(sqlite_integer(cursor.position, "cursor position")?)
                    .push(")");
            }
            ThreadRealtimeOrder::Position => {
                sql.push(" AND rollout_ordinal > ")
                    .push_bind(sqlite_integer(cursor.position, "cursor position")?);
            }
        }
    }
    sql.push(" ORDER BY ");
    if params.order == ThreadRealtimeOrder::ObservedAt {
        sql.push(OBSERVED_AT).push(", ");
    }
    sql.push("rollout_ordinal LIMIT ")
        .push_bind(i64::try_from(params.page_size + 1).map_err(thread_history_error)?);
    // Classify an exact-ID miss against the same projection snapshot as the page query.
    let mut transaction = pool.begin().await.map_err(thread_history_error)?;
    let rows = sql
        .build_query_as::<(i64, String)>()
        .fetch_all(&mut *transaction)
        .await
        .map_err(thread_history_error)?;
    // A second process can advance the projection after we capture its watermark.
    // An acknowledged ID outside that implicit snapshot is still waiting for coverage.
    if exact_id_uses_current_snapshot
        && rows
            .iter()
            .any(|(position, _)| *position >= snapshot_end_sql)
    {
        return Ok(empty_page(
            ThreadRealtimeCoverage::Pending,
            snapshot_end,
            projected_end,
        ));
    }
    let has_more = rows.len() > params.page_size;
    let data = rows
        .into_iter()
        .take(params.page_size)
        .map(|(position, json)| {
            Ok(ThreadRealtimeEntry {
                position: u64::try_from(position).map_err(thread_history_error)?,
                item: ThreadRealtimeItem::from_persisted(
                    serde_json::from_str::<RealtimeItem>(&json).map_err(thread_history_error)?,
                ),
            })
        })
        .collect::<ThreadStoreResult<Vec<_>>>()?;
    let next_cursor = if has_more {
        data.last()
            .map(|entry| {
                serde_json::to_string(&Cursor {
                    version: 1,
                    rollout_id: snapshot.head,
                    query,
                    snapshot_end,
                    observed_at_ms: entry.item.observed_at_ms,
                    position: entry.position,
                })
                .map_err(thread_history_error)
            })
            .transpose()?
    } else {
        None
    };
    let coverage = if let Some(event_id) = &params.event_id
        && data.is_empty()
    {
        let exists = snapshot
            .rows("SELECT 1")?
            .push(" WHERE item_id = ")
            .push_bind(event_id.clone())
            .push(" LIMIT 1")
            .build_query_scalar::<i64>()
            .fetch_optional(&mut *transaction)
            .await
            .map_err(thread_history_error)?
            .is_some();
        if exists {
            ThreadRealtimeCoverage::Ready
        } else {
            ThreadRealtimeCoverage::Pending
        }
    } else {
        ThreadRealtimeCoverage::Ready
    };
    transaction.commit().await.map_err(thread_history_error)?;
    Ok(RealtimePage {
        data,
        next_cursor,
        snapshot_end,
        projected_through_item_index: projected_end,
        coverage,
    })
}

fn empty_page(
    coverage: ThreadRealtimeCoverage,
    snapshot_end: u64,
    projected_through_item_index: u64,
) -> RealtimePage {
    RealtimePage {
        data: Vec::new(),
        next_cursor: None,
        snapshot_end,
        projected_through_item_index,
        coverage,
    }
}

fn invalid(message: &str) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: message.to_owned(),
    }
}

#[cfg(test)]
#[path = "realtime_stream_tests.rs"]
mod tests;
