//! Captures native restart state at child initialization, without changing live context.
//! Uses only native compaction and companion records so vanilla readers can resume it.

use super::*;

impl Session {
    /// Capture child startup state after reconstruction, before its new assignment.
    /// The caller holds settings admission through persistence and appends this
    /// after the copied or synthesized history, so model
    /// items already included in replacement history never appear again after it.
    pub(super) async fn fork_checkpoint_items(
        &self,
        history: &[RolloutItem],
    ) -> Option<Vec<RolloutItem>> {
        let observation_clock_floor_ms = match &self.realtime_history {
            Some(history) => history.lock().await.observation_clock.floor_ms(),
            None => None,
        };
        let state = self.state.lock().await;
        if state.session_configuration.history_mode != ThreadHistoryMode::Paginated {
            return None;
        }
        // Older histories can carry a turn baseline without native world-state records.
        // Vanilla replay would clear that baseline across this checkpoint, so keep its history.
        if state.reference_context_item().is_some()
            && state.history.world_state_checkpoint().is_none()
        {
            return None;
        }
        let _persistence = self.persistence_repair.state.lock().await;
        let mut items = self.restart_checkpoint_items(&state, observation_clock_floor_ms);
        // Cold resume derives interrupted status from the native event suffix. The
        // replacement history covers model items, but does not carry this lifecycle state.
        let latest_status = history.iter().rev().find(|item| {
            matches!(item,
            RolloutItem::EventMsg(event) if agent_status_from_event(event).is_some())
        });
        if let Some(terminal @ RolloutItem::EventMsg(EventMsg::TurnAborted(event))) = latest_status
            && matches!(
                event.reason,
                TurnAbortReason::Interrupted | TurnAbortReason::BudgetLimited
            )
        {
            items.push(terminal.clone());
        }
        Some(items)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::make_session_and_context;
    use super::*;

    /// Ordinary completion and aborts persist lifecycle records without a context snapshot.
    #[tokio::test]
    async fn ordinary_turn_endings_do_not_capture_checkpoints() {
        use codex_thread_store::CreateThreadParams;
        use codex_thread_store::LoadThreadHistoryParams;
        use codex_thread_store::LocalThreadStore;
        use codex_thread_store::LocalThreadStoreConfig;
        use codex_thread_store::ThreadPersistenceMetadata;
        use codex_thread_store::ThreadStore;

        let (mut session, mut turn) = make_session_and_context().await;
        turn.history_mode = ThreadHistoryMode::Paginated;
        let config = session.get_config().await;
        let home = tempfile::tempdir().unwrap();
        let mut local_config = LocalThreadStoreConfig::from_config(config.as_ref());
        local_config.codex_home = home.path().to_path_buf();
        let store = Arc::new(LocalThreadStore::new(local_config, None));
        let thread_store: Arc<dyn ThreadStore> = store.clone();
        let live = LiveThread::create(
            thread_store.clone(),
            CreateThreadParams {
                attached_thread: false,
                creator_user_id: None,
                creator_account_id: None,
                session_id: session.session_id(),
                thread_id: session.thread_id,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::Exec,
                thread_source: None,
                originator: "checkpoint-test".into(),
                base_instructions: BaseInstructions::default(),
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: None,
                history_mode: ThreadHistoryMode::Paginated,
                subagent_history_start_ordinal: None,
                history_base: None,
                inherited_history_visibility: Default::default(),
                initial_window_id: ContextWindowId::new_uuid_v7(),
                runtime_workspace_roots: None,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(config.cwd.to_path_buf()),
                    model_provider: config.model_provider_id.clone(),
                    memory_mode: codex_protocol::protocol::ThreadMemoryMode::Disabled,
                },
            },
        )
        .await
        .unwrap();
        session.services.thread_store = thread_store;
        session.services.live_thread = Some(live);
        session
            .state
            .lock()
            .await
            .session_configuration
            .history_mode = ThreadHistoryMode::Paginated;
        let live = session.live_thread().unwrap().clone();
        let history: RolloutItem = serde_json::from_value(serde_json::json!({
            "type":"response_item", "payload":{"type":"message", "role":"user",
            "content":[{"type":"input_text", "text":"archived".repeat(300_000)}]}
        }))
        .unwrap();
        live.append_items(&[history]).await.unwrap();
        let path = live.local_rollout_path_snapshot().unwrap();
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        for reason in [
            None,
            Some(TurnAbortReason::Interrupted),
            Some(TurnAbortReason::BudgetLimited),
        ] {
            let before = tokio::fs::metadata(&path).await.unwrap().len();
            let finish = Arc::new(tokio::sync::Notify::new());
            session
                .spawn_task(
                    turn.clone(),
                    Vec::new(),
                    crate::tasks::completion_test_support::CompletingTask {
                        finish: finish.clone(),
                    },
                )
                .await;
            let interrupted = reason.is_some();
            if let Some(reason) = reason {
                session.abort_all_tasks(reason).await;
            } else {
                finish.notify_one();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while session.active_turn.lock().await.is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            live.flush().await.unwrap();
            let context = store
                .load_latest_model_context(LoadThreadHistoryParams {
                    thread_id: session.thread_id,
                    include_archived: false,
                })
                .await
                .unwrap();
            assert!(
                !context
                    .items
                    .iter()
                    .any(|item| matches!(item, RolloutItem::Compacted(_)))
            );
            if interrupted {
                assert_eq!(
                    context.items.iter().rev().find_map(|item| match item {
                        RolloutItem::EventMsg(event) => agent_status_from_event(event),
                        _ => None,
                    }),
                    Some(AgentStatus::Interrupted),
                );
            }
            assert_eq!(live.local_rollout_path_snapshot().unwrap(), path);
            assert!(tokio::fs::metadata(&path).await.unwrap().len() - before < 64 * 1024);
        }
        live.shutdown().await.unwrap();
    }

    /// Older turn baselines stay on native replay until a full world-state baseline exists.
    #[tokio::test]
    async fn restart_checkpoint_keeps_history_until_native_baseline_is_complete() {
        use codex_thread_store::CreateThreadParams;
        use codex_thread_store::InMemoryThreadStore;
        use codex_thread_store::LoadThreadHistoryParams;
        use codex_thread_store::ThreadPersistenceMetadata;
        use codex_thread_store::ThreadStore;

        let (mut session, mut turn) = make_session_and_context().await;
        turn.history_mode = ThreadHistoryMode::Paginated;
        let config = session.get_config().await;
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_store: Arc<dyn ThreadStore> = store.clone();
        let live = LiveThread::create(
            thread_store.clone(),
            CreateThreadParams {
                attached_thread: false,
                creator_user_id: None,
                creator_account_id: None,
                session_id: session.session_id(),
                thread_id: session.thread_id,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::Exec,
                thread_source: None,
                originator: "checkpoint-test".into(),
                base_instructions: BaseInstructions::default(),
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: None,
                history_mode: ThreadHistoryMode::Paginated,
                subagent_history_start_ordinal: None,
                history_base: None,
                inherited_history_visibility: Default::default(),
                initial_window_id: ContextWindowId::new_uuid_v7(),
                runtime_workspace_roots: None,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(config.cwd.to_path_buf()),
                    model_provider: config.model_provider_id.clone(),
                    memory_mode: codex_protocol::protocol::ThreadMemoryMode::Disabled,
                },
            },
        )
        .await
        .unwrap();
        session.services.thread_store = thread_store;
        session.services.live_thread = Some(live);
        {
            let mut state = session.state.lock().await;
            state.session_configuration.history_mode = ThreadHistoryMode::Paginated;
            state.set_reference_context_item(Some(turn.to_turn_context_item()));
        }
        if let Some(items) = session.fork_checkpoint_items(&[]).await {
            session
                .live_thread()
                .unwrap()
                .append_items(&items)
                .await
                .unwrap();
        }
        let params = || LoadThreadHistoryParams {
            thread_id: session.thread_id,
            include_archived: false,
        };
        let before = store.load_latest_model_context(params()).await.unwrap();
        assert!(
            !before
                .items
                .iter()
                .any(|item| matches!(item, RolloutItem::Compacted(_)))
        );

        let turn = Arc::new(turn);
        let step = StepContext::for_test(turn);
        session
            .record_context_updates_and_set_reference_context_item(&step)
            .await
            .unwrap();
        if let Some(items) = session.fork_checkpoint_items(&[]).await {
            session
                .live_thread()
                .unwrap()
                .append_items(&items)
                .await
                .unwrap();
        }
        let after = store.load_latest_model_context(params()).await.unwrap();
        assert!(
            after
                .items
                .iter()
                .any(|item| matches!(item, RolloutItem::Compacted(_)))
        );
    }

    /// Native reconstruction preserves current context and its baselines without summarization.
    #[tokio::test]
    async fn restart_checkpoint_preserves_native_context_and_settings() {
        let (session, mut turn) = make_session_and_context().await;
        turn.history_mode = ThreadHistoryMode::Paginated;
        let turn = Arc::new(turn);
        let step = StepContext::for_test(Arc::clone(&turn));
        session
            .record_context_updates_and_set_reference_context_item(&step)
            .await
            .expect("world state should build");
        let message: ResponseItem = serde_json::from_value(serde_json::json!({
            "type":"message", "role":"user", "content":[{"type":"input_text", "text":"keep this context"}]
        })).unwrap();
        session
            .record_conversation_items(&turn, turn.model_info(), &[message])
            .await;
        let context = turn.to_turn_context_item();
        let (checkpoint, expected, expected_tokens) = {
            let mut state = session.state.lock().await;
            state.session_configuration.history_mode = ThreadHistoryMode::Paginated;
            state.set_reference_context_item(Some(context.clone()));
            state.last_started_turn_id = Some("saved-turn".into());
            let tokens: TokenUsageInfo = serde_json::from_value(serde_json::json!({
                "total_token_usage":{"input_tokens":17,"cached_input_tokens":3,"output_tokens":2,"reasoning_output_tokens":1,"total_tokens":19},
                "last_token_usage":{"input_tokens":17,"cached_input_tokens":3,"output_tokens":2,"reasoning_output_tokens":1,"total_tokens":19},
                "model_context_window":100000
            })).unwrap();
            state.set_token_info(Some(tokens.clone()));
            (
                session.restart_checkpoint_items(&state, Some(12345)),
                state.clone_history(),
                tokens,
            )
        };
        let serialized = serde_json::to_vec(&checkpoint).unwrap();
        let persisted: Vec<RolloutItem> = serde_json::from_slice(&serialized).unwrap();
        let restored = session
            .reconstruct_history_from_rollout(&turn, &persisted)
            .await;
        assert_eq!(restored.history, expected.annotated_items());
        assert_eq!(restored.retained_context, *expected.retained_context());
        assert_eq!(
            restored.world_state_baseline.as_ref().map(|state| {
                codex_protocol::protocol::WorldStateItem::full(state.clone().into_object())
            }),
            expected.world_state_checkpoint()
        );
        assert_eq!(
            restored.guardian_history,
            expected.guardian_history_checkpoint()
        );
        assert_eq!(restored.last_started_turn_id.as_deref(), Some("saved-turn"));
        assert_eq!(
            serde_json::to_value(restored.reference_context_item).unwrap(),
            serde_json::to_value(Some(context)).unwrap()
        );
        assert_eq!(
            Session::last_token_info_from_rollout(&persisted),
            Some(expected_tokens)
        );
        let RolloutItem::Compacted(compacted) = &persisted[0] else {
            panic!("native checkpoint")
        };
        let resume = compacted.resume_metadata.as_ref().unwrap();
        assert_eq!(resume.observation_clock_floor_ms, Some(12345));
        assert!(resume.dynamic_tools.as_ref().unwrap().tools.is_empty());
        assert!(matches!(
            persisted.last(),
            Some(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)))
        ));
        assert_eq!(
            session.clone_history().await.annotated_items(),
            expected.annotated_items()
        );
    }
}
