//! Exercise rollover through native readers, physical identities and append cursors.

use super::*;
use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::local::test_support::test_config;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadMemoryMode;
use tempfile::TempDir;

/// Native parsing, pagination and append must accept a checkpoint continuation and its fork.
#[tokio::test]
async fn continuation_preserves_native_rollout_compatibility()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let thread_id = ThreadId::new();
    let mut params = create_params(&store, thread_id);
    params.base_instructions = BaseInstructions {
        text: "Preserve these instructions across continuation.".into(),
        ..Default::default()
    };
    store.create_thread(params).await?;
    let initial_turn = compatibility_turn(thread_id, "before")?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: initial_turn.clone(),
        })
        .await?;
    let old = store.live_rollout_path(thread_id).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &old,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    let checkpoint = checkpoint(thread_id)?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: checkpoint.clone(),
        })
        .await?;
    let next = store.live_rollout_path(thread_id).await?;
    assert_ne!(next, old);
    let sealed = tokio::fs::read(&old).await?;
    let (original_items, original_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&old).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(original_id, Some(thread_id));
    assert_eq!(
        serde_json::to_value(&original_items[1..initial_turn.len() + 1])?,
        serde_json::to_value(&initial_turn)?
    );
    assert_eq!(
        serde_json::to_value(&original_items[initial_turn.len() + 1..])?,
        serde_json::to_value(&checkpoint)?
    );
    let original_meta = codex_rollout::read_session_meta_line(&old).await?.meta;
    let metadata = codex_rollout::read_session_meta_line(&next).await?.meta;
    assert_eq!(metadata.id, thread_id);
    assert_eq!(metadata.timestamp, original_meta.timestamp);
    assert_eq!(metadata.base_instructions, original_meta.base_instructions);
    assert_eq!(
        metadata.history_base.unwrap().end_byte_offset,
        sealed.len() as u64
    );
    assert_ne!(codex_rollout::rollout_id_from_path(&next), Some(thread_id));
    assert!(tokio::fs::metadata(&next).await?.len() < 64 * 1024);
    store.shutdown_thread(thread_id).await?;
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        context.items.get(1),
        Some(RolloutItem::Compacted(_))
    ));

    // Use the native parser and recorder, not the optimized checkpoint reader, to
    // verify the on-disk contract consumed by vanilla Codex.
    let (native_items, native_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&next).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(native_id, Some(thread_id));
    assert_eq!(
        serde_json::to_value(&native_items[1..])?,
        serde_json::to_value(&checkpoint)?
    );
    let config = RolloutConfig {
        codex_home: home.path().into(),
        sqlite: store.config.sqlite.clone(),
        cwd: home.path().into(),
        model_provider_id: "test-provider".into(),
        generate_memories: true,
    };
    let writer = RolloutRecorder::new(&config, RolloutRecorderParams::resume(next.clone())).await?;
    let appended = compatibility_turn(thread_id, "after")?;
    writer.record_canonical_items(&appended).await?;
    writer.flush().await?;
    writer.shutdown().await?;
    assert_eq!(tokio::fs::read(&old).await?, sealed);
    let lineage = store.resolve_rollout_lineage(thread_id, None).await?;
    assert_eq!(lineage.segments().len(), 2);
    let (native_items, _, parse_errors) = RolloutRecorder::load_rollout_items(&next).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(
        serde_json::to_value(&native_items[1..checkpoint.len() + 1])?,
        serde_json::to_value(&checkpoint)?
    );
    assert_eq!(
        serde_json::to_value(&native_items[checkpoint.len() + 1..])?,
        serde_json::to_value(&appended)?
    );
    // The raw recorder writes JSONL only. Native pagination reads its SQLite
    // projection, so run the same materializer used by the store after a write.
    crate::local::thread_history_materialization::materialize_to_sqlite(
        &store,
        codex_rollout::rollout_id_from_path(&next).unwrap(),
        &next,
    )
    .await?;
    assert_native_turn_pages(&store, thread_id).await?;
    let fork = store
        .prepare_fork(crate::PrepareForkParams {
            thread_id,
            boundary: crate::ForkBoundary::Latest,
        })
        .await?;
    let child = ThreadId::new();
    let mut params = create_params(&store, child);
    params.forked_from_id = Some(thread_id);
    params.history_base = fork.history_base;
    store.create_thread(params).await?;
    store
        .persist_thread(child, crate::PersistContext::Standard)
        .await?;
    let child_path = store.live_rollout_path(child).await?;
    assert!(tokio::fs::metadata(&child_path).await?.len() < 32 * 1024);
    assert_eq!(
        codex_rollout::read_session_meta_line(&child_path)
            .await?
            .meta
            .history_base,
        fork.history_base
    );
    drop(fork);
    store.shutdown_thread(child).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &child_path,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    assert_native_turn_pages(&store, child).await?;
    assert!(
        store
            .delete_thread(crate::DeleteThreadParams { thread_id })
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&old).await?, sealed);
    store
        .delete_thread(crate::DeleteThreadParams { thread_id: child })
        .await?;
    store
        .delete_thread(crate::DeleteThreadParams { thread_id })
        .await?;
    assert!(!old.exists());
    assert!(!next.exists());
    Ok(())
}

/// Reads one turn per page so the native cursor crosses the inherited segment boundary.
#[tokio::test]
async fn rotated_call_recovers_its_own_frames_and_cursor() -> Result<(), Box<dyn std::error::Error>>
{
    use codex_protocol::realtime::{RealtimeItem, RealtimeItemContent, RealtimeSessionOutcome};
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let id = ThreadId::new();
    store.create_thread(create_params(&store, id)).await?;
    let start = RealtimeItem {
        id: "start".into(),
        realtime_session_id: "call".into(),
        observed_at_ms: Some(10),
        content: RealtimeItemContent::RealtimeSessionStarted,
    };
    let frame = RealtimeItem {
        id: "frame".into(),
        observed_at_ms: Some(11),
        content: RealtimeItemContent::Frame {
            channels: codex_protocol::realtime::RealtimeFrameChannels {
                voice_transcript: codex_protocol::realtime::RealtimeVoiceTranscript {
                    role: codex_protocol::realtime::RealtimeTranscriptRole::User,
                    text: "hello".into(),
                },
            },
        },
        ..start.clone()
    };
    let closed = RealtimeItem {
        id: "closed".into(),
        observed_at_ms: Some(12),
        content: RealtimeItemContent::RealtimeSessionClosed {
            outcome: RealtimeSessionOutcome::Ended,
            final_frame_sequence: None,
        },
        ..start.clone()
    };
    store
        .seal_realtime_items(id, vec![start.clone(), frame.clone()])
        .await?;
    let original = store.live_rollout_path(id).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &original,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    let query = crate::ListRealtimeParams {
        thread_id: id,
        session_id: None,
        from_observed_at_ms: None,
        to_observed_at_ms: None,
        order: codex_app_server_protocol::ThreadRealtimeOrder::Position,
        after_position: None,
        event_id: None,
        cursor: None,
        page_size: 1,
        snapshot_end: None,
        required_through_position: None,
    };
    let page = store.list_realtime(query.clone()).await?;
    assert_eq!(page.data[0].item.id, "start");
    let cursor = page.next_cursor.expect("frame must be on a second page");
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    assert_ne!(store.live_rollout_path(id).await?, original);
    store.seal_realtime_items(id, vec![closed.clone()]).await?;
    let continued = store
        .list_realtime(crate::ListRealtimeParams {
            cursor: Some(cursor),
            ..query.clone()
        })
        .await?;
    assert_eq!(continued.data[0].item.id, "frame");
    assert!(continued.next_cursor.is_none());
    assert_eq!(continued.snapshot_end, page.snapshot_end);
    assert_eq!(
        store.load_realtime_session_head(id).await?.as_deref(),
        Some("call")
    );
    assert_eq!(
        store.load_realtime_session(id, "call".into()).await?,
        Some(vec![start, frame, closed])
    );
    assert!(store.realtime_frames_enabled(id).await?);
    let fork = store
        .prepare_fork(crate::PrepareForkParams {
            thread_id: id,
            boundary: crate::ForkBoundary::Latest,
        })
        .await?;
    let child = ThreadId::new();
    let mut params = create_params(&store, child);
    params.forked_from_id = Some(id);
    params.history_base = fork.history_base;
    store.create_thread(params).await?;
    store
        .persist_thread(child, crate::PersistContext::Standard)
        .await?;
    store.flush_thread(child).await?;
    let child_path = store.live_rollout_path(child).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &child_path,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    assert_eq!(store.load_realtime_session_head(child).await?, None);
    assert_eq!(
        store.load_realtime_session(child, "call".into()).await?,
        None
    );
    assert!(!store.realtime_frames_enabled(child).await?);
    assert!(
        store
            .list_realtime(crate::ListRealtimeParams {
                thread_id: child,
                ..query
            })
            .await?
            .data
            .is_empty()
    );
    Ok(())
}

async fn assert_native_turn_pages(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cursor = None;
    for label in ["before", "after"] {
        let page = store
            .list_turns(crate::ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor,
                page_size: 1,
                sort_direction: crate::SortDirection::Asc,
                items_view: crate::StoredTurnItemsView::Summary,
            })
            .await?;
        assert_eq!(page.turns.len(), 1);
        let turn = &page.turns[0];
        assert_eq!(turn.turn_id, label);
        assert_eq!(turn.status, crate::StoredTurnStatus::Completed);
        assert_eq!(turn.items.len(), 2);
        let user: serde_json::Value = serde_json::from_slice(&turn.items[0].item_json)?;
        let agent: serde_json::Value = serde_json::from_slice(&turn.items[1].item_json)?;
        assert_eq!(user["content"][0]["text"], format!("question {label}"));
        assert_eq!(agent["text"], format!("answer {label}"));
        cursor = page.next_cursor;
        assert_eq!(cursor.is_some(), label == "before");
    }
    Ok(())
}

fn compatibility_turn(thread_id: ThreadId, label: &str) -> serde_json::Result<Vec<RolloutItem>> {
    serde_json::from_value(serde_json::json!([
        {"type":"event_msg", "payload":{"type":"turn_started","turn_id":label}},
        {"type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":format!("question {label}")}] }},
        {"type":"event_msg", "payload":{"type":"item_completed", "thread_id":thread_id,
            "turn_id":label, "item":{"type":"UserMessage", "id":format!("user-{label}"),
                "content":[{"type":"text", "text":format!("question {label}"), "text_elements":[]}]}}},
        {"type":"event_msg", "payload":{"type":"item_completed", "thread_id":thread_id,
            "turn_id":label, "item":{"type":"AgentMessage", "id":format!("agent-{label}"),
                "content":[{"type":"Text", "text":format!("answer {label}")}]}}},
        {"type":"event_msg", "payload":{"type":"turn_complete","turn_id":label}}
    ]))
}

/// Compaction seals the prefix immediately and the same turn finishes in its continuation.
#[tokio::test]
async fn active_turn_checkpoint_rotates_before_completion() -> Result<(), Box<dyn std::error::Error>>
{
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let id = ThreadId::new();
    create(&store, id).await?;
    let mut prefix = compatibility_turn(id, "before")?;
    let completion = prefix.split_off(4);
    let answer = prefix.split_off(3);
    let recovery_context: RolloutItem = serde_json::from_value(serde_json::json!({
        "type": "turn_context", "payload": {
            "turn_id": "before", "cwd": home.path(), "approval_policy":"never",
            "permission_profile":codex_protocol::models::PermissionProfile::read_only(),
            "model":"original-model"
        }
    }))?;
    prefix.push(recovery_context);
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: prefix,
        })
        .await?;
    let path = store.live_rollout_path(id).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &path,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    let path_subscription = store.live_rollout_path_subscription(id).await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    let next = store.live_rollout_path(id).await?;
    assert_ne!(next, path);
    assert_eq!(*path_subscription.borrow(), next);
    let sealed_len = tokio::fs::metadata(&path).await?.len();
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: false,
        })
        .await?;
    assert!(serde_json::to_vec(&context.items)?.len() < 64 * 1024);
    assert!(matches!(
        context.items.get(1),
        Some(RolloutItem::Compacted(_))
    ));
    let (items, _, _) = RolloutRecorder::load_rollout_items(&path).await?;
    assert!(items.iter().any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == "before")));
    let fork = store
        .prepare_fork(crate::PrepareForkParams {
            thread_id: id,
            boundary: crate::ForkBoundary::Latest,
        })
        .await?;
    let child = ThreadId::new();
    let mut params = create_params(&store, child);
    params.forked_from_id = Some(id);
    params.history_base = fork.history_base;
    store.create_thread(params).await?;
    store
        .persist_thread(child, crate::PersistContext::Standard)
        .await?;
    drop(fork);
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: answer,
        })
        .await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    let last = store.live_rollout_path(id).await?;
    assert_ne!(last, next);
    let recovered = codex_rollout::unfinished_turn_context(home.path(), last.clone(), id, "before")
        .await?
        .expect("the saved turn is still open in the sealed prefix");
    assert_eq!(recovered.model, "original-model");
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: completion,
        })
        .await?;
    assert_eq!(store.live_rollout_path(id).await?, last);
    assert!(
        codex_rollout::unfinished_turn_context(home.path(), last, id, "before")
            .await?
            .is_none()
    );
    assert_eq!(tokio::fs::metadata(&path).await?.len(), sealed_len);
    let (items, _, _) = RolloutRecorder::load_rollout_items(&next).await?;
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    );
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: compatibility_turn(id, "after")?,
        })
        .await?;
    assert_native_turn_pages(&store, id).await?;
    let child_path = store.live_rollout_path(child).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &child_path,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    let child_page = store
        .list_turns(crate::ListTurnsParams {
            thread_id: child,
            include_archived: false,
            cursor: None,
            page_size: 4,
            sort_direction: crate::SortDirection::Asc,
            items_view: crate::StoredTurnItemsView::Summary,
        })
        .await?;
    let [frozen] = child_page.turns.as_slice() else {
        panic!("fork should see only the turn present at its cutoff");
    };
    assert_eq!(frozen.turn_id, "before");
    assert_eq!(frozen.items.len(), 1);
    assert_eq!(frozen.items[0].item_id, "user-before");
    store.shutdown_thread(child).await?;
    store.shutdown_thread(id).await?;
    Ok(())
}

/// A rejected selected-path update leaves the original writer and checkpoint usable.
#[tokio::test]
async fn failed_publication_preserves_original_writer() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let pool = config
        .sqlite
        .open_read_write_pool(&config.sqlite.state_db_path())
        .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let id = ThreadId::new();
    create(&store, id).await?;
    let items: Vec<RolloutItem> = serde_json::from_value(serde_json::json!([
        {"type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"small history"}]}},
        {"type":"event_msg", "payload":{"type":"turn_complete","turn_id":"done"}}
    ]))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items,
        })
        .await?;
    let old = store.live_rollout_path(id).await?;
    assert!(tokio::fs::metadata(&old).await?.len() < 64 * 1024);
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &old,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    sqlx::query("CREATE TRIGGER reject_segment BEFORE UPDATE OF rollout_path ON threads WHEN OLD.rollout_path != NEW.rollout_path BEGIN SELECT RAISE(ABORT, 'reject publication'); END").execute(&pool).await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    assert_eq!(store.live_rollout_path(id).await?, old);
    assert_eq!(state.get_thread(id).await?.unwrap().rollout_path, old);
    assert_eq!(rollout_files(old.parent().unwrap()).len(), 1);
    let appended: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"after rejected publication"}]}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![appended],
        })
        .await?;
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        context.items.last(),
        Some(RolloutItem::ResponseItem(_))
    ));
    store.shutdown_thread(id).await?;
    Ok(())
}

/// A failed publication and unreadable selection can recover before the next append.
#[tokio::test]
async fn unresolved_publication_recovers_before_next_append()
-> Result<(), Box<dyn std::error::Error>> {
    exercise_unresolved_publication("BEFORE", Recovery::Append).await
}

#[tokio::test]
async fn uncertain_committed_publication_recovers_before_next_append()
-> Result<(), Box<dyn std::error::Error>> {
    exercise_unresolved_publication("AFTER", Recovery::Append).await
}

#[tokio::test]
async fn discard_closes_both_writers_after_uncertain_publication()
-> Result<(), Box<dyn std::error::Error>> {
    exercise_unresolved_publication("AFTER", Recovery::Discard).await
}

#[tokio::test]
async fn realtime_seal_reconciles_uncertain_publication() -> Result<(), Box<dyn std::error::Error>>
{
    exercise_unresolved_publication("AFTER", Recovery::RealtimeSeal).await
}

enum Recovery {
    Append,
    Discard,
    RealtimeSeal,
}

async fn exercise_unresolved_publication(
    timing: &str,
    recovery: Recovery,
) -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let pool = config
        .sqlite
        .open_read_write_pool(&config.sqlite.state_db_path())
        .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let id = ThreadId::new();
    create(&store, id).await?;
    let items: Vec<RolloutItem> = serde_json::from_value(serde_json::json!([
        {"type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"x".repeat(2 * 1024 * 1024)}]}},
        {"type":"event_msg", "payload":{"type":"turn_complete","turn_id":"done"}}
    ]))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items,
        })
        .await?;
    let old = store.live_rollout_path(id).await?;
    codex_rollout::state_db::reconcile_rollout(
        Some(&state),
        &old,
        "test-provider",
        None,
        &[],
        Some(false),
        None,
    )
    .await;
    sqlx::query(if timing == "BEFORE" {
        "CREATE TRIGGER reject_segment BEFORE UPDATE OF rollout_path ON threads WHEN OLD.rollout_path != NEW.rollout_path BEGIN UPDATE threads SET history_mode = 'unreadable' WHERE id = OLD.id; SELECT RAISE(FAIL, 'reject publication'); END"
    } else {
        "CREATE TRIGGER reject_segment AFTER UPDATE OF rollout_path ON threads WHEN OLD.rollout_path != NEW.rollout_path BEGIN UPDATE threads SET history_mode = 'unreadable' WHERE id = OLD.id; SELECT RAISE(FAIL, 'reject publication'); END"
    }).execute(&pool).await?;
    assert!(
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: checkpoint(id)?,
            })
            .await
            .is_err()
    );
    assert!(state.get_thread(id).await.is_err());
    let paths = rollout_files(old.parent().unwrap());
    assert_eq!(paths.len(), 2);
    let before = paths
        .iter()
        .map(std::fs::read)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: checkpoint(id)?,
            })
            .await
            .is_err()
    );
    let final_input = codex_protocol::realtime::RealtimeItem {
        id: "final-input".into(),
        realtime_session_id: "call-1".into(),
        observed_at_ms: None,
        content: codex_protocol::realtime::RealtimeItemContent::SessionInputSaved {
            reason: codex_protocol::realtime::SessionInputReason::SessionEnd,
            source_event_ids: Vec::new(),
            input_item_id: "input-tail".into(),
            input: "last words".into(),
        },
    };
    if matches!(recovery, Recovery::RealtimeSeal) {
        assert!(
            store
                .seal_realtime_items(id, vec![final_input.clone()])
                .await
                .is_err()
        );
    }
    assert!(store.flush_thread(id).await.is_err());
    assert!(store.shutdown_thread(id).await.is_err());
    assert_eq!(
        paths
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()?,
        before
    );
    if matches!(recovery, Recovery::Discard) {
        let (current, pending) = {
            let entries = store.live_recorders.lock().await;
            let entry = entries.get(&id).unwrap();
            (
                entry.recorder.clone(),
                entry
                    .pending_continuation
                    .as_ref()
                    .unwrap()
                    .recorder
                    .clone(),
            )
        };
        store.discard_thread(id).await?;
        assert!(current.flush().await.is_err());
        assert!(pending.flush().await.is_err());
        assert_eq!(
            paths
                .iter()
                .map(std::fs::read)
                .collect::<Result<Vec<_>, _>>()?,
            before
        );
        return Ok(());
    }
    sqlx::query("DROP TRIGGER reject_segment")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE threads SET history_mode = 'paginated' WHERE id = ?")
        .bind(id.to_string())
        .execute(&pool)
        .await?;
    let selected = state.get_thread(id).await?.unwrap().rollout_path;
    assert_eq!(selected == old, timing == "BEFORE");
    if matches!(recovery, Recovery::RealtimeSeal) {
        store
            .seal_realtime_items(id, vec![final_input.clone()])
            .await?;
        store
            .seal_realtime_items(id, vec![final_input.clone()])
            .await?;
        assert_eq!(store.live_rollout_path(id).await?, selected);
        assert_eq!(state.get_thread(id).await?.unwrap().rollout_path, selected);
        let original = paths.iter().position(|p| p == &old).unwrap();
        assert_eq!(std::fs::read(&old)?, before[original]);
        let (items, _, _) = RolloutRecorder::load_rollout_items(&selected).await?;
        assert_eq!(items.iter().filter(|item| matches!(item, RolloutItem::RealtimeItem(item) if item.id == final_input.id)).count(), 1);
        store.shutdown_thread(id).await?;
        return Ok(());
    }
    let appended: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"after rejected publication"}]}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![appended],
        })
        .await?;
    assert_eq!(store.live_rollout_path(id).await?, selected);
    assert_eq!(
        rollout_files(old.parent().unwrap()).len(),
        if timing == "BEFORE" { 1 } else { 2 }
    );
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        context.items.last(),
        Some(RolloutItem::ResponseItem(_))
    ));
    store.shutdown_thread(id).await?;
    Ok(())
}

/// Cancelling the requester does not release the checkpoint's reserved append order.
#[tokio::test]
async fn cancelled_checkpoint_keeps_append_order() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let store = LocalThreadStore::new(test_config(home.path()), None);
    let id = ThreadId::new();
    create(&store, id).await?;
    let writer = store
        .live_writer_locks
        .coordination(id)
        .await
        .writer
        .clone();
    let entries = store.live_recorders.lock().await;
    let checkpoint_store = store.clone();
    let items = checkpoint(id)?;
    let requester = tokio::spawn(async move {
        checkpoint_store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if writer.try_lock().is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    requester.abort();
    let _ = requester.await;
    drop(entries);
    let after: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"after cancelled requester"}]}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![after.clone()],
        })
        .await?;
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: id,
            include_archived: false,
        })
        .await?;
    assert_eq!(
        serde_json::to_value(context.items.last()).unwrap(),
        serde_json::to_value(Some(&after)).unwrap()
    );
    assert!(matches!(
        context.items.get(1),
        Some(RolloutItem::Compacted(_))
    ));
    store.shutdown_thread(id).await?;
    Ok(())
}

/// Repeated forks can reuse a complete checkpoint, but an intervening write requires a new one.
#[tokio::test]
async fn identical_checkpoint_does_not_copy_context_again() -> Result<(), Box<dyn std::error::Error>>
{
    let home = TempDir::new()?;
    let store = LocalThreadStore::new(test_config(home.path()), None);
    let id = ThreadId::new();
    create(&store, id).await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    let path = store.live_rollout_path(id).await?;
    let first = tokio::fs::read(&path).await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    assert_eq!(tokio::fs::read(&path).await?, first);
    let after: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"intervening item"}]}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![after],
        })
        .await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    let (items, _, _) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, RolloutItem::Compacted(_)))
            .count(),
        2
    );
    store.shutdown_thread(id).await?;
    Ok(())
}

/// Generates a disposable segmented store for a published vanilla app-server probe.
/// Run explicitly with `--ignored --nocapture`; the printed directory is intentionally retained.
#[tokio::test]
#[ignore = "manual compatibility fixture for an unmodified published binary"]
async fn write_vanilla_compatibility_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let id = ThreadId::new();
    create(&store, id).await?;
    for turn in 0..3 {
        let mut items: Vec<RolloutItem> = serde_json::from_value(serde_json::json!([
            {"type":"event_msg", "payload":{"type":"turn_started","turn_id":format!("turn-{turn}")}},
            {"type":"response_item", "payload":{"type":"message","role":"user",
                "content":[{"type":"input_text","text":format!("message-{turn}: {}", "x".repeat(2 * 1024 * 1024))}]}},
            {"type":"event_msg", "payload":{"type":"item_completed", "thread_id":id, "turn_id":format!("turn-{turn}"),
                "item":{"type":"UserMessage", "id":format!("user-{turn}"),
                    "content":[{"type":"text", "text":format!("display-message-{turn}"), "text_elements":[]}]}}},
            {"type":"event_msg", "payload":{"type":"item_completed", "thread_id":id, "turn_id":format!("turn-{turn}"),
                "item":{"type":"AgentMessage", "id":format!("agent-{turn}"),
                    "content":[{"type":"Text", "text":format!("display-answer-{turn}")}]}}},
            {"type":"event_msg", "payload":{"type":"turn_complete","turn_id":format!("turn-{turn}")}}
        ]))?;
        let completion = items.split_off(3);
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items,
            })
            .await?;
        let path = store.live_rollout_path(id).await?;
        codex_rollout::state_db::reconcile_rollout(
            Some(&state),
            &path,
            "test-provider",
            None,
            &[],
            Some(false),
            None,
        )
        .await;
        let mut snapshot = checkpoint(id)?;
        let RolloutItem::Compacted(compacted) = &mut snapshot[0] else {
            panic!("expected compaction");
        };
        let retained: codex_protocol::models::ResponseItem =
            serde_json::from_value(serde_json::json!({
                "type":"message", "role":"user",
                "content":[{"type":"input_text","text":format!("retained model context {turn}")}]
            }))?;
        compacted.replacement_history = Some(vec![retained.into()]);
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: snapshot,
            })
            .await?;
        assert_ne!(store.live_rollout_path(id).await?, path);
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: completion,
            })
            .await?;
    }
    let path = store.live_rollout_path(id).await?;
    store.shutdown_thread(id).await?;
    let lineage = store.resolve_rollout_lineage(id, None).await?;
    assert_eq!(lineage.segments().len(), 4);
    println!(
        "COMPAT_FIXTURE={}",
        serde_json::json!({"home":home.keep(), "thread_id":id,"path":path})
    );
    Ok(())
}

/// Native checkpoint fields carry an empty catalog and complete current settings.
fn checkpoint(thread_id: ThreadId) -> serde_json::Result<Vec<RolloutItem>> {
    serde_json::from_value(serde_json::json!([
        {"type":"compacted", "payload": {
            "message":"", "replacement_history":[], "window_number":0,
            "resume_metadata": {"dynamic_tools":{"thread_id":thread_id,"tools":[]},
                "multi_agent_version":"v2", "last_started_turn_id":null,"previous_turn_settings":null}
        }},
        {"type":"event_msg", "payload": {"type":"thread_settings_applied", "thread_id":thread_id,
            "thread_settings":{
                "model":"test-model", "model_provider_id":"test-provider",
                "approval_policy":"never", "approvals_reviewer":"user",
                "permission_profile":codex_protocol::models::PermissionProfile::workspace_write(),
                "cwd":"/tmp", "collaboration_mode":{
                    "mode":"default", "settings":{"model":"test-model"}
                }
            }}}
    ]))
}

/// Creates a paginated writer with native metadata and no test-specific storage behavior.
async fn create(store: &LocalThreadStore, thread_id: ThreadId) -> ThreadStoreResult<()> {
    store.create_thread(create_params(store, thread_id)).await
}

/// Supplies native new-thread metadata for roots and referenced children.
fn create_params(store: &LocalThreadStore, thread_id: ThreadId) -> CreateThreadParams {
    CreateThreadParams {
        attached_thread: false,
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "test".into(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: vec![],
        selected_capability_roots: vec![],
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Paginated,
        history_base: None,
        inherited_history_visibility: Default::default(),
        subagent_history_start_ordinal: None,
        initial_window_id: codex_protocol::ContextWindowId::new_uuid_v7(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: Some(store.config.codex_home.clone()),
            model_provider: "test-provider".into(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn rollout_files(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}
