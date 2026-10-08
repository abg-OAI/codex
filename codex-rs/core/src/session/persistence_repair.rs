use super::*;
use codex_protocol::items::FunctionCallOutputItem;
use codex_thread_store::LoadThreadHistoryParams;
use std::sync::OnceLock;

/// Teardown closes writer attachment before it observes the current live writer.
#[derive(Default)]
pub(super) enum PersistenceRepairState {
    #[default]
    Active,
    ShuttingDown,
}

/// Boxed session state keeps repair coordination from increasing the size of
/// every session startup future.
#[derive(Default)]
pub(super) struct PersistenceRepair {
    /// Acquire after the main session state when both locks are needed.
    pub(super) state: Mutex<PersistenceRepairState>,
    /// Published only after recovered history is durable.
    pub(super) repaired: OnceLock<Box<RepairedPersistence>>,
}

/// Persistence attached to an existing [`Session`] after its saved-thread
/// intent is established.
///
/// Publishing the writer, database, and rollout path as one bundle prevents
/// later callers from observing a partially repaired session.
pub(super) struct RepairedPersistence {
    pub(super) live_thread: LiveThread,
    pub(super) state_db: Option<state_db::StateDbHandle>,
    pub(super) rollout_path: Option<PathBuf>,
}

impl Session {
    pub(super) async fn close_persistence_repair(&self) {
        *self.persistence_repair.state.lock().await = PersistenceRepairState::ShuttingDown;
    }

    pub(crate) fn repaired_rollout_path(&self) -> Option<PathBuf> {
        self.persistence_repair
            .repaired
            .get()
            .and_then(|persistence| persistence.rollout_path.clone())
    }

    /// Restores a saved target's writer without replacing its live runtime.
    ///
    /// The caller establishes the target's persistence intent independently
    /// of the session configuration that originally loaded this runtime.
    pub(crate) async fn restore_saved_thread_persistence(&self) -> anyhow::Result<()> {
        if self.live_thread().is_some() {
            return Ok(());
        }

        let turn_context = self.new_inject_items_context().await;
        let _settings = self.thread_settings_persistence.acquire().await?;
        let mut state = self.state.lock().await;
        let repair = self.persistence_repair.state.lock().await;
        anyhow::ensure!(
            matches!(*repair, PersistenceRepairState::Active),
            "cannot restore saved thread persistence during shutdown"
        );
        if self.live_thread().is_some() {
            return Ok(());
        }
        anyhow::ensure!(
            !crate::saffron::saved_thread_persistence::source_is_ephemeral(
                &state.session_configuration.session_source,
                state.session_configuration.thread_source.as_ref(),
            ),
            "temporary system agents cannot be converted to saved threads"
        );

        let store = &self.services.thread_store;
        let stored = store
            .read_thread(ReadThreadParams {
                thread_id: self.thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        anyhow::ensure!(
            stored.history_mode == state.session_configuration.history_mode,
            "saved thread history mode differs from the live session"
        );

        let config = &state.session_configuration.original_config_do_not_use;
        // Supplying paginated context prevents LiveThread from falling back to
        // the unsupported full-history read. Recovery reloads the context
        // after acquiring the writer, so this preliminary snapshot cannot
        // make the comparison stale.
        let (resume_history, history_revision) = match stored.history_mode {
            ThreadHistoryMode::Paginated => {
                let context = store
                    .load_latest_model_context(LoadThreadHistoryParams {
                        thread_id: self.thread_id,
                        include_archived: true,
                    })
                    .await?;
                (Some(Arc::new(context.items)), context.revision)
            }
            ThreadHistoryMode::Legacy => (None, None),
        };
        let params = ResumeThreadParams {
            thread_id: self.thread_id,
            rollout_path: stored.rollout_path,
            history: resume_history,
            history_revision,
            include_archived: true,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(state.session_configuration.cwd().to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: if config.memories.generate_memories {
                    ThreadMemoryMode::Enabled
                } else {
                    ThreadMemoryMode::Disabled
                },
            },
        };
        let mut guard = LiveThreadInitGuard::default();
        let resume_store = Arc::clone(store);
        let history_mode = stored.history_mode;
        let live_thread = guard
            .acquire(async move {
                let (live_thread, _history) =
                    LiveThread::resume(resume_store, history_mode, params).await?;
                Ok(live_thread)
            })
            .await?;

        let result: anyhow::Result<RepairedPersistence> = async {
            // Read after reserving the writer so no other process can append
            // between the comparison and recovery.
            let history_params = LoadThreadHistoryParams {
                thread_id: self.thread_id,
                include_archived: true,
            };
            let saved = match stored.history_mode {
                ThreadHistoryMode::Paginated => {
                    store.load_latest_model_context(history_params).await?.items
                }
                ThreadHistoryMode::Legacy => store.load_history(history_params).await?.items,
            };
            let reconstructed = self
                .reconstruct_history_from_rollout(&turn_context, &saved)
                .await;
            let mut saved_counts = HashMap::<Vec<u8>, usize>::new();
            for envelope in &reconstructed.history {
                *saved_counts
                    .entry(recovery_item_key(&envelope.item)?)
                    .or_default() += 1;
            }

            let mut recovered = Vec::new();
            let mut presentations = Vec::new();
            let mut occurrences = HashMap::<Vec<u8>, usize>::new();
            let mut tool_calls = HashMap::new();
            for envelope in state.history.annotated_items() {
                match &envelope.item {
                    ResponseItem::FunctionCall { call_id, .. }
                    | ResponseItem::CustomToolCall { call_id, .. } => {
                        tool_calls.insert(call_id.as_str(), &envelope.item);
                    }
                    _ => {}
                }
            }
            for envelope in state.history.annotated_items() {
                let key = recovery_item_key(&envelope.item)?;
                let occurrence = occurrences.entry(key.clone()).or_default();
                let item_id = envelope
                    .item
                    .id()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| {
                        let seed = format!("{}:{occurrence}:", self.thread_id);
                        let mut bytes = seed.into_bytes();
                        bytes.extend_from_slice(&key);
                        format!("recovered-{}", Uuid::new_v5(&Uuid::NAMESPACE_OID, &bytes))
                    });
                *occurrence += 1;
                if let Some(count) = saved_counts.get_mut(&key)
                    && *count > 0
                {
                    *count -= 1;
                    continue;
                }

                if let Some(item) = recovery_turn_item(&envelope.item, item_id, &tool_calls) {
                    let turn_id = envelope
                        .item
                        .turn_id()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("recovered-{}", self.thread_id));
                    presentations.push((turn_id, item));
                }
                recovered.push(RolloutItem::ResponseItem(envelope.clone()));
            }

            let turn_ids = presentations
                .iter()
                .map(|(turn_id, _)| turn_id.clone())
                .collect::<HashSet<_>>();
            let mut existing_turns = match stored.history_mode {
                ThreadHistoryMode::Paginated if !turn_ids.is_empty() => {
                    let local_store = store
                        .as_any()
                        .downcast_ref::<LocalThreadStore>()
                        .ok_or_else(|| {
                            anyhow::anyhow!("saved history turn lookup is unavailable")
                        })?;
                    local_store
                        .existing_history_turns(self.thread_id, &turn_ids)
                        .await?
                }
                ThreadHistoryMode::Legacy | ThreadHistoryMode::Paginated => saved
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                            Some(event.turn_id.clone())
                        }
                        _ => None,
                    })
                    .collect(),
            };
            let recovered_at_ms = Utc::now().timestamp_millis();
            let mut presentation_items = Vec::new();
            for (turn_id, item) in presentations {
                if existing_turns.insert(turn_id.clone()) {
                    presentation_items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                        TurnStartedEvent {
                            turn_attribution: None,
                            turn_id: turn_id.clone(),
                            root_turn_id: None,
                            trace_id: None,
                            started_at: None,
                            model_context_window: None,
                            collaboration_mode_kind: Default::default(),
                        },
                    )));
                }
                if stored.history_mode == ThreadHistoryMode::Legacy {
                    presentation_items.extend(
                        item.as_legacy_events(/*show_raw_agent_reasoning*/ false)
                            .into_iter()
                            .map(RolloutItem::EventMsg),
                    );
                }
                presentation_items.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
                    ItemCompletedEvent {
                        thread_id: self.thread_id,
                        turn_id,
                        item,
                        started_at_ms: None,
                        completed_at_ms: recovered_at_ms,
                    },
                )));
            }

            // Presentation precedes raw responses so a partially persisted
            // batch cannot hide a response whose UI item was not saved.
            presentation_items.append(&mut recovered);
            recovered = presentation_items;
            recovered.extend(self.persistence_repair_checkpoint(&state));
            live_thread.append_items(&recovered).await?;
            live_thread.flush().await?;

            let rollout_path = live_thread.local_rollout_path().await?;
            let state_db = match store.as_any().downcast_ref::<LocalThreadStore>() {
                Some(local_store) => local_store.state_db().await,
                None => None,
            };
            Ok(RepairedPersistence {
                live_thread,
                state_db,
                rollout_path,
            })
        }
        .await;

        let restored = match result {
            Ok(restored) => restored,
            Err(error) => {
                // A partial prefix stays available for comparison on retry.
                // Transfer cleanup ownership before awaiting so cancellation
                // cannot abandon the acquired writer.
                let cleanup = tokio::spawn(async move { guard.discard().await });
                if let Err(cleanup_error) = cleanup.await {
                    warn!("saved thread persistence cleanup task failed: {cleanup_error}");
                }
                return Err(error);
            }
        };

        anyhow::ensure!(
            self.persistence_repair
                .repaired
                .set(Box::new(restored))
                .is_ok(),
            "saved thread persistence was attached concurrently"
        );
        Arc::make_mut(&mut state.session_configuration.original_config_do_not_use).ephemeral =
            false;
        guard.commit();
        Ok(())
    }

    fn persistence_repair_checkpoint(&self, state: &SessionState) -> Vec<RolloutItem> {
        let window_ids = state.auto_compact_window_ids();
        let history = state.clone_history();
        let compacted = CompactedItem {
            message: String::new(),
            replacement_history: Some(history.annotated_items().to_vec()),
            guardian_history: history.guardian_history_checkpoint(),
            retained_context: Some(history.retained_context().clone()),
            mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
            window_number: Some(state.auto_compact_window_number()),
            first_window_id: Some(window_ids.first_window_id.to_string()),
            previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
            window_id: Some(window_ids.window_id.to_string()),
            compaction_response_id: None,
            latest_token_usage_record: state.latest_token_usage_record.clone(),
            resume_metadata: Some(CompactionResumeMetadata {
                multi_agent_version: self.multi_agent_version(),
                last_started_turn_id: state.last_started_turn_id.clone(),
                turn_attribution: state.turn_attribution.clone(),
                previous_turn_settings: state.previous_turn_settings(),
            }),
        };
        let mut items = vec![RolloutItem::Compacted(compacted)];
        if let Some(world_state) = history.world_state_checkpoint() {
            items.push(RolloutItem::WorldState(world_state));
        }
        if let Some(turn_context) = state.reference_context_item() {
            items.push(RolloutItem::TurnContext(turn_context));
        }
        items.push(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
            codex_protocol::protocol::ThreadSettingsAppliedEvent {
                thread_id: Some(self.thread_id),
                thread_settings: state
                    .session_configuration
                    .thread_settings_snapshot(&state.session_configuration.environments),
            },
        )));
        items
    }
}

// Compare payloads as well as IDs so a surviving revision does not disappear
// behind an older saved version with the same ID. Preserve multiplicity.
fn recovery_item_key(item: &ResponseItem) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(item)
}

/// Reuses native presentation items without inferring turn completion or tool success.
fn recovery_turn_item(
    response: &ResponseItem,
    id: String,
    tool_calls: &HashMap<&str, &ResponseItem>,
) -> Option<TurnItem> {
    if let Some(mut item) = parse_turn_item(response) {
        match &mut item {
            TurnItem::UserMessage(item) => item.id = id,
            TurnItem::AgentMessage(item) => item.id = id,
            TurnItem::Reasoning(item) => item.id = id,
            TurnItem::WebSearch(item) => item.id = id,
            TurnItem::ImageGeneration(item) => item.id = id,
            TurnItem::HookPrompt(item) => item.id = id,
            _ => return None,
        }
        return Some(item);
    }

    let (call_id, name, namespace, output) = match response {
        ResponseItem::FunctionCallOutput {
            call_id,
            name,
            namespace,
            output,
            ..
        } => (
            call_id.as_deref(),
            name.clone(),
            namespace.clone(),
            output.body.clone(),
        ),
        ResponseItem::CustomToolCallOutput {
            call_id,
            name,
            output,
            ..
        } => (
            Some(call_id.as_str()),
            name.clone(),
            None,
            output.body.clone(),
        ),
        _ => return None,
    };
    let (name, namespace) = match (name, call_id.and_then(|id| tool_calls.get(id))) {
        (Some(name), _) => (name, namespace),
        (
            None,
            Some(
                ResponseItem::FunctionCall {
                    name, namespace, ..
                }
                | ResponseItem::CustomToolCall {
                    name, namespace, ..
                },
            ),
        ) => (name.clone(), namespace.clone()),
        (None, _) => (
            "Recovered tool output (name unavailable)".to_owned(),
            namespace,
        ),
    };
    Some(TurnItem::FunctionCallOutput(FunctionCallOutputItem {
        id,
        name,
        namespace,
        output,
    }))
}
