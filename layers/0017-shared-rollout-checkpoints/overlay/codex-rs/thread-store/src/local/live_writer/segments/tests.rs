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

/// A rollover preserves the old bytes, references their prefix and accepts further native writes.
#[tokio::test]
async fn continuation_preserves_history_and_native_append() -> Result<(), Box<dyn std::error::Error>>
{
    let home = TempDir::new()?;
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let thread_id = ThreadId::new();
    create(&store, thread_id).await?;
    let message: RolloutItem = serde_json::from_value(serde_json::json!({
        "type": "response_item", "payload": {
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "x".repeat(8)}]
        }
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![message],
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
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![serde_json::from_value(serde_json::json!({
                "type":"event_msg", "payload":{"type":"turn_complete", "turn_id":"completed"}
            }))?],
        })
        .await?;
    let checkpoint = checkpoint(thread_id)?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: checkpoint,
        })
        .await?;
    let next = store.live_rollout_path(thread_id).await?;
    assert_ne!(next, old);
    let sealed = tokio::fs::read(&old).await?;
    let original_meta = codex_rollout::read_session_meta_line(&old).await?.meta;
    let metadata = codex_rollout::read_session_meta_line(&next).await?.meta;
    assert_eq!(metadata.id, thread_id);
    assert_eq!(metadata.timestamp, original_meta.timestamp);
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

    // This native recorder operation is unchanged by segmentation.
    let config = RolloutConfig {
        codex_home: home.path().into(),
        sqlite: store.config.sqlite.clone(),
        cwd: home.path().into(),
        model_provider_id: "test-provider".into(),
        generate_memories: true,
    };
    let writer = RolloutRecorder::new(&config, RolloutRecorderParams::resume(next.clone())).await?;
    let appended: RolloutItem = serde_json::from_value(serde_json::json!({
        "type": "response_item", "payload": {
            "type": "message", "role": "user", "content": [{"type":"input_text","text":"after rotation"}]
        }
    }))?;
    writer.record_canonical_items(&[appended]).await?;
    writer.flush().await?;
    writer.shutdown().await?;
    assert_eq!(tokio::fs::read(&old).await?, sealed);
    let lineage = store.resolve_rollout_lineage(thread_id, None).await?;
    assert_eq!(lineage.segments().len(), 2);
    let context = store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    assert!(matches!(
        context.items.last(),
        Some(RolloutItem::ResponseItem(_))
    ));
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

/// An active compaction checkpoint keeps lifecycle recovery in the selected file.
#[tokio::test]
async fn active_turn_checkpoint_keeps_lifecycle_in_one_file()
-> Result<(), Box<dyn std::error::Error>> {
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
    let start: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"event_msg", "payload":{"type":"turn_started","turn_id":"active"}
    }))?;
    let message: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"response_item", "payload":{"type":"message","role":"user",
            "content":[{"type":"input_text","text":"x".repeat(2 * 1024 * 1024)}]}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![start, message],
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
    let late_completion: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"event_msg", "payload":{"type":"turn_complete","turn_id":"previous"}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![late_completion],
        })
        .await?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    assert_eq!(store.live_rollout_path(id).await?, path);
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
    assert!(items.iter().any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(event)) if event.turn_id == "active")));
    let terminal: RolloutItem = serde_json::from_value(serde_json::json!({
        "type":"event_msg", "payload":{"type":"turn_complete","turn_id":"active"}
    }))?;
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: vec![terminal],
        })
        .await?;
    // An ordinary terminal event does not capture or rotate another checkpoint.
    assert_eq!(store.live_rollout_path(id).await?, path);
    store
        .append_items(AppendThreadItemsParams {
            thread_id: id,
            items: checkpoint(id)?,
        })
        .await?;
    let next = store.live_rollout_path(id).await?;
    assert_ne!(next, path);
    assert_eq!(*path_subscription.borrow(), next);
    let (items, _, _) = RolloutRecorder::load_rollout_items(&next).await?;
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
    );
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
    exercise_unresolved_publication("BEFORE", false).await
}

#[tokio::test]
async fn uncertain_committed_publication_recovers_before_next_append()
-> Result<(), Box<dyn std::error::Error>> {
    exercise_unresolved_publication("AFTER", false).await
}

#[tokio::test]
async fn discard_closes_both_writers_after_uncertain_publication()
-> Result<(), Box<dyn std::error::Error>> {
    exercise_unresolved_publication("AFTER", true).await
}

async fn exercise_unresolved_publication(
    timing: &str,
    discard: bool,
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
    assert!(store.flush_thread(id).await.is_err());
    assert!(store.shutdown_thread(id).await.is_err());
    assert_eq!(
        paths
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()?,
        before
    );
    if discard {
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
        let items: Vec<RolloutItem> = serde_json::from_value(serde_json::json!([
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
        store
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: checkpoint(id)?,
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
