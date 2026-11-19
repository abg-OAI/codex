//! Archive publication preserves the logical ordinals and older fork bookmarks.

use super::*;

#[tokio::test]
async fn consolidated_history_keeps_completed_turns_and_older_fork_cutoffs()
-> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let (store, parent, child, saved_realtime) = write_archived_parent_and_fork(&home, 0).await?;
    for (thread_id, labels) in [
        (parent, vec!["first", "second", "third", "fourth"]),
        (child, vec!["first", "second"]),
    ] {
        let mut cursor = None;
        for (index, label) in labels.iter().enumerate() {
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
            assert_eq!(&turn.turn_id, label);
            assert_eq!(turn.status, crate::StoredTurnStatus::Completed);
            let items: Vec<serde_json::Value> = turn
                .items
                .iter()
                .map(|item| serde_json::from_slice(&item.item_json))
                .collect::<Result<_, _>>()?;
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["content"][0]["text"], format!("question {label}"));
            assert_eq!(items[1]["text"], format!("answer {label}"));
            cursor = page.next_cursor;
            assert_eq!(cursor.is_some(), index + 1 < labels.len());
        }
    }
    store
        .revert_thread(crate::RevertThreadParams {
            thread_id: parent,
            before_turn_id: "third".into(),
            multi_agent_version: None,
        })
        .await?;
    assert!(matches!(
        store.list_realtime(saved_realtime).await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));
    Ok(())
}

/// Exposes this fixture to the released app-server compatibility probe.
#[tokio::test]
#[ignore = "manual post-turn continuation with a released stock binary"]
async fn write_archived_switch_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let home = TempDir::new()?;
    let (store, parent, child, _) =
        write_archived_parent_and_fork(&home, MAX_CHECKPOINT_SEGMENTS - 4).await?;
    drop(store);
    println!(
        "ARCHIVE_FIXTURE={}",
        serde_json::json!({
            "home": home.keep(), "thread_id": parent, "child": child,
        })
    );
    Ok(())
}

async fn write_archived_parent_and_fork(
    home: &TempDir,
    extra_rotations: usize,
) -> Result<
    (
        LocalThreadStore,
        ThreadId,
        ThreadId,
        crate::ListRealtimeParams,
    ),
    Box<dyn std::error::Error>,
> {
    let config = test_config(home.path());
    let state = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await?;
    let store = LocalThreadStore::new(config, Some(state.clone()));
    let parent = ThreadId::new();
    let child = ThreadId::new();
    create(&store, parent).await?;
    let mut old_files = Vec::new();
    let mut saved_pages = None;
    for (index, label) in ["first", "second", "third", "fourth"]
        .into_iter()
        .enumerate()
    {
        let mut items = compatibility_turn(parent, label)?;
        let finish = items.split_off(3);
        store
            .append_items(AppendThreadItemsParams {
                thread_id: parent,
                items,
            })
            .await?;
        let path = store.live_rollout_path(parent).await?;
        if index == 0 {
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
        }
        if index == 3 && extra_rotations > 0 {
            for window in 0..extra_rotations {
                let mut extra = checkpoint(parent)?;
                let RolloutItem::Compacted(item) = &mut extra[0] else {
                    unreachable!()
                };
                item.window_number = Some(10 + window as u64);
                store
                    .append_items(AppendThreadItemsParams {
                        thread_id: parent,
                        items: extra,
                    })
                    .await?;
            }
        }
        if index == 3 && extra_rotations == 0 {
            // Exercise the production archive path with a small real lineage.
            store
                .live_recorders
                .lock()
                .await
                .get_mut(&parent)
                .unwrap()
                .rotations_until_archive = Some(0);
        }
        let mut snapshot = checkpoint(parent)?;
        let RolloutItem::Compacted(record) = &mut snapshot[0] else {
            unreachable!()
        };
        record.window_number = Some(index as u64);
        let retained: codex_protocol::models::ResponseItem = serde_json::from_value(
            serde_json::json!({
                "type":"message","role":"user","content":[{"type":"input_text","text":format!("retained {label}")}]
            }),
        )?;
        record.replacement_history = Some(vec![retained.into()]);
        let started = std::time::Instant::now();
        store
            .append_items(AppendThreadItemsParams {
                thread_id: parent,
                items: snapshot,
            })
            .await?;
        if extra_rotations > 0 {
            println!("ROTATION_US {label} {}", started.elapsed().as_micros());
        }
        let selected = store.live_rollout_path(parent).await?;
        assert_ne!(path, selected);
        old_files.push((path.clone(), tokio::fs::read(&path).await?));
        store
            .append_items(AppendThreadItemsParams {
                thread_id: parent,
                items: finish,
            })
            .await?;
        if index == 2 {
            let call_events = ["start-first", "start-second"]
                .into_iter()
                .enumerate()
                .map(|(index, id)| codex_protocol::realtime::RealtimeItem {
                    id: id.into(),
                    realtime_session_id: id.into(),
                    observed_at_ms: Some(100 + index as i64),
                    content: codex_protocol::realtime::RealtimeItemContent::RealtimeSessionStarted,
                })
                .collect();
            store.seal_realtime_items(parent, call_events).await?;
            saved_pages = Some(save_pages(&store, parent).await?);
            // Fork through the previous completed turn inside an old segment.
            let fork = store
                .prepare_fork(crate::PrepareForkParams {
                    thread_id: parent,
                    boundary: crate::ForkBoundary::ThroughTurn("second".into()),
                })
                .await?;
            let mut params = create_params(&store, child);
            params.forked_from_id = Some(parent);
            params.history_base = fork.history_base;
            store.create_thread(params).await?;
            store
                .persist_thread(child, crate::PersistContext::Standard)
                .await?;
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
            store.shutdown_thread(child).await?;
        }
    }
    let source = store.live_rollout_path(parent).await?;
    let lineage = store.resolve_rollout_lineage(parent, Some(source)).await?;
    assert_eq!(lineage.segments().len(), 3);
    let archive = &lineage.segments()[1];
    assert!(
        old_files
            .iter()
            .all(|(path, _)| path != &archive.rollout_path)
    );
    for (path, bytes) in old_files {
        assert_eq!(tokio::fs::read(path).await?, bytes);
    }
    store.shutdown_thread(parent).await?;
    let (turns, items, realtime, snapshot_end) = saved_pages.unwrap();
    assert_eq!(store.list_turns(turns).await?.turns[0].turn_id, "second");
    assert_eq!(
        store.list_items(items).await?.items[0].item_id,
        "agent-second"
    );
    let page = store.list_realtime(realtime.clone()).await?;
    assert_eq!(page.data[0].item.id, "start-second");
    assert_eq!(page.snapshot_end, snapshot_end);
    assert!(page.next_cursor.is_none());
    Ok((store, parent, child, realtime))
}

async fn save_pages(
    store: &LocalThreadStore,
    parent: ThreadId,
) -> Result<
    (
        crate::ListTurnsParams,
        crate::ListItemsParams,
        crate::ListRealtimeParams,
        u64,
    ),
    Box<dyn std::error::Error>,
> {
    let mut turns = crate::ListTurnsParams {
        thread_id: parent,
        include_archived: false,
        cursor: None,
        page_size: 1,
        sort_direction: crate::SortDirection::Asc,
        items_view: crate::StoredTurnItemsView::Summary,
    };
    let page = store.list_turns(turns.clone()).await?;
    assert_eq!(page.turns[0].turn_id, "first");
    turns.cursor = Some(page.next_cursor.expect("more turns"));
    let mut items = crate::ListItemsParams {
        thread_id: parent,
        turn_id: Some("second".into()),
        include_archived: false,
        position: None,
        item_ids: None,
        page_size: 1,
        sort_direction: crate::SortDirection::Asc,
        sort_key: crate::ItemSortKey::CreatedAtOrdinal,
        after_updated_at_ordinal: None,
        before_updated_at_ordinal: None,
    };
    let page = store.list_items(items.clone()).await?;
    assert_eq!(page.items[0].item_id, "user-second");
    items.position = Some(crate::ListItemsPosition::Cursor(
        page.next_cursor.expect("agent reply"),
    ));
    let mut realtime = crate::ListRealtimeParams {
        thread_id: parent,
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
    let page = store.list_realtime(realtime.clone()).await?;
    assert_eq!(page.data[0].item.id, "start-first");
    realtime.cursor = Some(page.next_cursor.expect("second event"));
    Ok((turns, items, realtime, page.snapshot_end))
}
