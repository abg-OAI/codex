//! Creates a native paginated continuation with a new physical rollout identity.
//! Callers own source exclusion, durable publication and the thread's selected-path update.

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;

use super::LocalThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Preserves the source thread settings while starting a distinct referenced rollout.
pub(super) async fn create(
    store: &LocalThreadStore,
    source_meta: codex_rollout::SessionMeta,
    rollout_id: ThreadId,
    history_base: Option<codex_protocol::protocol::HistoryPosition>,
    forked_from_ordinal_exclusive: Option<u64>,
    writer_lock: super::WriterLockGuard,
) -> ThreadStoreResult<RolloutRecorder> {
    let config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite: store.config.sqlite.clone(),
        cwd: source_meta.cwd.clone(),
        model_provider_id: source_meta
            .model_provider
            .clone()
            .unwrap_or_else(|| store.config.default_model_provider_id.clone()),
        generate_memories: source_meta.memory_mode.as_deref() != Some("disabled"),
    };
    let mut params = RolloutRecorderParams::new(
        source_meta.id,
        source_meta.forked_from_id,
        source_meta.parent_thread_id,
        source_meta.source,
        source_meta.thread_source,
        source_meta.originator,
        source_meta.base_instructions.unwrap_or_default(),
        source_meta.dynamic_tools.unwrap_or_default(),
    )
    .with_creator(
        source_meta.creator_user_id,
        source_meta.creator_account_id,
        source_meta.account_scoped,
    )
    .with_session_id(source_meta.session_id)
    .with_attached_thread(source_meta.attached_thread)
    .with_rollout_id(rollout_id)
    .with_selected_capability_roots(source_meta.selected_capability_roots)
    .with_runtime_workspace_roots(source_meta.runtime_workspace_roots)
    .with_multi_agent_version(source_meta.multi_agent_version)
    .with_history_mode(ThreadHistoryMode::Paginated)
    .with_history_base(history_base)
    .with_forked_from_ordinal_exclusive(forked_from_ordinal_exclusive)
    .with_subagent_history_start_ordinal(source_meta.subagent_history_start_ordinal);
    if let Some(context_window) = source_meta.context_window {
        params = params.with_initial_window_id(context_window.window_id);
    }
    RolloutRecorder::new_with_writer_lock(&config, params, writer_lock)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to create reverted rollout: {err}"),
        })
}

