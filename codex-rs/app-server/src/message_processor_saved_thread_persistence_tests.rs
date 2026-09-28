use super::*;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_core::StartThreadOptions;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_thread_store::LiveThread;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use pretty_assertions::assert_eq;

const LIVE_MESSAGE: &str = "message accepted only by the writerless saved runtime";

fn marker(text: &str, turn_id: &str) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", text)),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: Some(MessagePhase::FinalAnswer),
        internal_chat_message_metadata_passthrough: None,
    };
    item.set_turn_id_if_missing(turn_id);
    item
}

fn assert_live_message_once<'a>(items: impl Iterator<Item = &'a ThreadItem>) {
    assert_eq!(
        items
            .filter_map(|item| match item {
                ThreadItem::AgentMessage { text, .. } if text == LIVE_MESSAGE => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![LIVE_MESSAGE],
    );
}

#[test]
#[serial(app_server_tracing)]
fn saved_thread_read_repairs_live_runtime_and_rejects_stale_fallback() -> Result<()> {
    run_current_thread_test_with_stack("saved-thread-api-repair", async {
        for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
            exercise_saved_thread_repair(history_mode).await?;
        }
        Ok(())
    })
}

async fn exercise_saved_thread_repair(history_mode: ThreadHistoryMode) -> Result<()> {
    let mut harness = TracingHarness::new_with_sqlite().await?;
    harness
        .processor
        .connection_initialized(TEST_CONNECTION_ID, harness.session.request_attestation())
        .await;
    let (manager, store) = harness
        .processor
        .thread_processor
        .saved_thread_persistence_test_resources();
    let config = ConfigBuilder::default()
        .codex_home(harness._codex_home.path().to_path_buf())
        .build()
        .await?;
    let mut saved_options = StartThreadOptions::new(config.clone());
    if history_mode == ThreadHistoryMode::Legacy {
        saved_options.history_mode = Some(ThreadHistoryMode::Legacy);
    }
    let saved = manager.start_thread(saved_options).await?;
    let thread_id = saved.thread_id;
    assert_eq!(
        saved.thread.config_snapshot().await.history_mode,
        history_mode
    );
    saved
        .thread
        .inject_response_items(vec![marker("already saved", "saved-api-turn")])
        .await?;
    saved.thread.ensure_rollout_materialized().await;
    saved.thread.flush_rollout().await?;
    let rollout_path = saved.thread.rollout_path().expect("saved rollout");
    saved.thread.shutdown_and_wait().await?;
    assert!(manager.remove_thread(&thread_id).await.is_some());
    drop(saved);

    // Bypass normal cold-resume policy to reproduce a saved target that an
    // older runtime loaded without a writer.
    let broken = harness
        .processor
        .thread_processor
        .resume_saved_thread_without_persistence_for_test(config.clone(), thread_id)
        .await?;
    assert!(!broken.has_persistence());
    let live_marker = marker(LIVE_MESSAGE, "live-api-turn");
    broken
        .inject_response_items(vec![live_marker.clone()])
        .await?;
    let stored_before = match history_mode {
        ThreadHistoryMode::Legacy => {
            store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: true,
                })
                .await?
                .items
        }
        ThreadHistoryMode::Paginated => {
            store
                .load_latest_model_context(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: true,
                })
                .await?
                .items
        }
    };
    assert!(!stored_before.iter().any(|item| matches!(
        item,
        RolloutItem::ResponseItem(response) if response.item.id() == live_marker.id()
    )));

    let blocker = LiveThread::resume(
        Arc::clone(&store),
        history_mode,
        ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout_path.clone()),
            history: (history_mode == ThreadHistoryMode::Paginated)
                .then(|| Arc::new(stored_before.clone())),
            include_archived: true,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await?;
    let request_id = RequestId::Integer(10);
    harness
        .processor
        .process_request(
            TEST_CONNECTION_ID,
            request_from_client_request(ClientRequest::ThreadRead {
                request_id: request_id.clone(),
                params: ThreadReadParams {
                    thread_id: thread_id.to_string(),
                    include_turns: true,
                },
            }),
            &AppServerTransport::Stdio,
            Arc::clone(&harness.session),
        )
        .await;
    let error = read_repair_error(&mut harness.outgoing_rx, request_id).await?;
    assert_eq!(error.code, crate::error_code::INTERNAL_ERROR_CODE);
    assert!(error.message.contains("failed to access loaded thread"));
    assert!(
        error
            .message
            .contains("failed to restore saved thread persistence")
    );
    assert!(!broken.has_persistence());
    blocker.shutdown().await?;

    let read: ThreadReadResponse = harness
        .request(
            ClientRequest::ThreadRead {
                request_id: RequestId::Integer(11),
                params: ThreadReadParams {
                    thread_id: thread_id.to_string(),
                    include_turns: true,
                },
            },
            /*trace*/ None,
        )
        .await;
    assert!(!read.thread.ephemeral);
    assert!(broken.has_persistence());
    assert!(broken.state_db().is_some());
    assert_live_message_once(read.thread.turns.iter().flat_map(|turn| &turn.items));

    let resumed: ThreadResumeResponse = harness
        .request(
            ClientRequest::ThreadResume {
                request_id: RequestId::Integer(12),
                params: ThreadResumeParams {
                    thread_id: thread_id.to_string(),
                    ..Default::default()
                },
            },
            /*trace*/ None,
        )
        .await;
    assert_live_message_once(resumed.thread.turns.iter().flat_map(|turn| &turn.items));
    assert!(Arc::ptr_eq(&broken, &manager.get_thread(thread_id).await?));

    broken.shutdown_and_wait().await?;
    assert!(manager.remove_thread(&thread_id).await.is_some());
    let reopened: ThreadResumeResponse = harness
        .request(
            ClientRequest::ThreadResume {
                request_id: RequestId::Integer(13),
                params: ThreadResumeParams {
                    thread_id: thread_id.to_string(),
                    ..Default::default()
                },
            },
            /*trace*/ None,
        )
        .await;
    assert_live_message_once(reopened.thread.turns.iter().flat_map(|turn| &turn.items));
    let reopened = manager.get_thread(thread_id).await?;

    let mut ephemeral_config = config;
    ephemeral_config.ephemeral = true;
    let ephemeral = manager
        .start_thread(StartThreadOptions::new(ephemeral_config))
        .await?;
    let ephemeral_id = ephemeral.thread_id;
    let ephemeral = ephemeral.thread;
    assert!(ephemeral.config_snapshot().await.ephemeral);
    assert!(!ephemeral.has_persistence());
    assert!(!manager.get_thread(ephemeral_id).await?.has_persistence());

    reopened.shutdown_and_wait().await?;
    ephemeral.shutdown_and_wait().await?;
    harness.shutdown().await;
    Ok(())
}

async fn read_repair_error(
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    request_id: RequestId,
) -> Result<codex_app_server_protocol::JSONRPCErrorError> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let envelope = outgoing_rx.recv().await.expect("outgoing channel");
            let crate::outgoing_message::OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } = envelope
            else {
                continue;
            };
            if connection_id != TEST_CONNECTION_ID {
                continue;
            }
            match message {
                crate::outgoing_message::OutgoingMessage::Error(error)
                    if error.id == request_id =>
                {
                    return error.error;
                }
                crate::outgoing_message::OutgoingMessage::Response(response)
                    if response.id == request_id =>
                {
                    panic!("repair error returned stale data: {response:?}");
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(Into::into)
}
