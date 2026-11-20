//! Test-only helpers exposed for cross-crate integration tests.
//!
//! Production code should not depend on this module.
//! We prefer this to using a crate feature to avoid building multiple
//! permutations of the crate.

use std::path::PathBuf;
use std::sync::Arc;

use codex_exec_server::EnvironmentManager;
use codex_extension_api::LoadInstructionsFuture;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructionsProvider;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::bundled_models_response;
use codex_models_manager::collaboration_mode_presets;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::test_support::construct_model_info_offline_for_tests;
use codex_models_manager::test_support::get_model_offline_for_tests;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::mcp::OPENAI_FORM_EXTENSION_ID;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::SessionSource;
use once_cell::sync::Lazy;

use crate::ThreadManager;
pub use crate::code_mode_host::ProcessOwnedGrpcCodeModeSessionProvider;
use crate::config::Config;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::subagent_header_value;
use crate::responses_metadata::subagent_metadata_kind;
use crate::thread_manager;
use crate::unified_exec;

/// Opens a legacy rollout written before paginated admission became the local default.
pub async fn start_saved_legacy_thread(
    manager: &ThreadManager,
    mut options: crate::StartThreadOptions,
) -> anyhow::Result<crate::NewThread> {
    use codex_history::{RolloutItem, RolloutLine};
    use codex_protocol::protocol::{SessionMeta, SessionMetaLine};
    use std::io::Write;

    let id = manager.reserve_thread_id();
    let timestamp = "2025-01-05T12:00:00.000Z";
    let directory = options.config.codex_home.join("sessions/2025/01/05");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("rollout-2025-01-05T12-00-00-{id}.jsonl"));
    let source = options.session_source.clone().unwrap_or(SessionSource::Exec);
    let meta = SessionMeta {
        id,
        session_id: source.parent_thread_id().unwrap_or(id).into(),
        parent_thread_id: source.parent_thread_id(),
        agent_path: source.get_agent_path().map(|path| path.to_string()),
        timestamp: timestamp.into(),
        cwd: options.config.cwd.to_path_buf(),
        source,
        thread_source: options.thread_source.clone(),
        model_provider: Some(options.config.model_provider_id.clone()),
        ..SessionMeta::default()
    };
    {
        let header = RolloutItem::SessionMeta(Box::new(SessionMetaLine { meta, git: None }));
        let mut file = std::fs::File::create_new(&path)?;
        for item in std::iter::once(header)
            .chain(options.initial_history.get_rollout_items().iter().cloned())
        {
            serde_json::to_writer(
                &mut file,
                &RolloutLine {
                    timestamp: timestamp.into(),
                    ordinal: None,
                    item,
                },
            )?;
            file.write_all(b"\n")?;
        }
    }
    options.initial_history = crate::rollout::RolloutRecorder::get_rollout_history(&path).await?;
    Ok(manager.start_thread(options).await?)
}

/// Supply fixed model metadata to tests without implementing a provider.
pub fn fixed_model_catalog(
    manager: SharedModelsManager,
) -> Arc<dyn codex_model_provider::ModelCatalogFactory> {
    struct FixedCatalog(SharedModelsManager);

    impl codex_model_provider::ModelCatalogFactory for FixedCatalog {
        fn build(
            &self,
            _provider: codex_model_provider::SharedModelProvider,
            _config_model_catalog: Option<codex_protocol::openai_models::ModelsResponse>,
            _api_key_model_discovery_default: bool,
        ) -> SharedModelsManager {
            Arc::clone(&self.0)
        }

        fn set_api_key_model_discovery_enabled(&self, enabled: bool) {
            self.0.set_api_key_model_discovery_enabled(enabled);
        }
    }

    Arc::new(FixedCatalog(manager))
}

static TEST_MODEL_PRESETS: Lazy<Vec<ModelPreset>> = Lazy::new(|| {
    let mut response = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    response.models.sort_by_key(|model| model.priority);
    let mut presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    presets
});

/// Inspect the same resolved environment configurations used to construct sampling steps.
pub async fn environment_windows_sandbox_types(
    thread: &crate::CodexThread,
) -> Vec<(String, codex_sandboxing::SandboxType)> {
    thread
        .session
        .services
        .turn_environments
        .snapshot()
        .await
        .turn_environments()
        .map(|environment| {
            (
                environment.selection.environment_id.clone(),
                environment.config().windows_sandbox_type,
            )
        })
        .collect()
}

/// Reattaches request-only observations to a completed turn's history for capture assertions.
/// Tests inspect this separately from the destination-filtered HTTP/WS request.
pub async fn history_with_tool_call_metadata(
    thread: &crate::CodexThread,
) -> Vec<codex_protocol::models::ResponseItem> {
    let history = thread.conversation_history_snapshot().await;
    let mut items = history.items().cloned().collect::<Vec<_>>();
    thread
        .session
        .services
        .executed_tool_calls
        .attach_to_prompt(&mut items, &mut Default::default());
    items
}

/// Retains the active turn's timing after completion without exporting telemetry.
/// Returns `None` when there is no running task. The observer reports exclusive
/// durations for the caller's interval in inference, automatic review, local
/// tools, overhead, and unknown order.
pub async fn observe_turn_timing(
    thread: &crate::CodexThread,
) -> Option<
    impl Fn(std::time::Instant, std::time::Instant) -> Option<[std::time::Duration; 5]> + use<>,
> {
    let active = thread.session.active_turn.lock().await;
    let timing = Arc::clone(
        &active
            .as_ref()?
            .task
            .as_ref()?
            .turn_context
            .turn_timing_state,
    );
    Some(move |start, end| {
        timing.snapshot_path(end).map(|path| {
            let totals = path.durations(start, end);
            [
                totals.sampling.saturating_add(totals.compaction),
                totals.auto_review,
                totals.tools,
                totals.overhead,
                totals.unknown,
            ]
        })
    })
}

/// Test-only provider that supplies no user instructions.
#[derive(Debug, Default)]
pub struct EmptyUserInstructionsProvider;

impl UserInstructionsProvider for EmptyUserInstructionsProvider {
    fn load_user_instructions(&self) -> LoadInstructionsFuture<'_> {
        Box::pin(async { LoadedUserInstructions::default() })
    }
}

pub fn set_thread_manager_test_mode(enabled: bool) {
    thread_manager::set_thread_manager_test_mode_for_tests(enabled);
}

pub fn set_deterministic_process_ids(enabled: bool) {
    unified_exec::set_deterministic_process_ids_for_tests(enabled);
}

pub fn auth_manager_from_auth(auth: CodexAuth) -> Arc<AuthManager> {
    AuthManager::from_auth_for_testing(auth)
}

pub fn auth_manager_from_auth_with_home(auth: CodexAuth, codex_home: PathBuf) -> Arc<AuthManager> {
    AuthManager::from_auth_for_testing_with_home(auth, codex_home)
}

pub fn with_code_mode_host_program(
    thread_manager: ThreadManager,
    host_program: PathBuf,
    config: &crate::config::Config,
) -> ThreadManager {
    thread_manager.with_code_mode_host_program_for_tests(host_program, config)
}

pub fn thread_manager_with_models_provider(
    auth: CodexAuth,
    provider: ModelProviderInfo,
) -> ThreadManager {
    ThreadManager::with_models_provider_for_tests(auth, provider)
}

pub fn thread_manager_with_models_provider_and_home(
    auth: CodexAuth,
    provider: ModelProviderInfo,
    codex_home: PathBuf,
    environment_manager: Arc<EnvironmentManager>,
) -> ThreadManager {
    ThreadManager::with_models_provider_and_home_for_tests(
        auth,
        provider,
        codex_home,
        environment_manager,
    )
}

pub async fn start_thread_with_user_shell_override(
    thread_manager: &ThreadManager,
    config: Config,
    user_shell_override: crate::shell::Shell,
    supports_openai_form_elicitation: bool,
) -> codex_protocol::error::Result<crate::NewThread> {
    thread_manager
        .start_thread_with_user_shell_override_for_tests(
            config,
            user_shell_override,
            ClientMcpExtensions::new(
                supports_openai_form_elicitation
                    .then(|| (OPENAI_FORM_EXTENSION_ID.to_string(), serde_json::json!({}))),
            ),
        )
        .await
}

pub async fn resume_legacy_thread_from_rollout_with_user_shell_override(
    thread_manager: &ThreadManager,
    config: Config,
    rollout_path: PathBuf,
    auth_manager: Arc<AuthManager>,
    user_shell_override: crate::shell::Shell,
    supports_openai_form_elicitation: bool,
) -> codex_protocol::error::Result<crate::NewThread> {
    thread_manager
        .resume_legacy_thread_from_rollout_with_user_shell_override_for_tests(
            config,
            rollout_path,
            auth_manager,
            user_shell_override,
            ClientMcpExtensions::new(
                supports_openai_form_elicitation
                    .then(|| (OPENAI_FORM_EXTENSION_ID.to_string(), serde_json::json!({}))),
            ),
        )
        .await
}

pub fn models_manager_with_provider(
    codex_home: PathBuf,
    auth_manager: Arc<AuthManager>,
    provider: ModelProviderInfo,
) -> SharedModelsManager {
    let provider = create_model_provider(provider, Some(auth_manager));
    provider.models_manager(codex_home, /*config_model_catalog*/ None)
}

pub fn default_http_client_factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}

pub fn get_model_offline(model: Option<&str>) -> String {
    get_model_offline_for_tests(model)
}

pub fn construct_model_info_offline(model: &str, config: &Config) -> ModelInfo {
    Arc::unwrap_or_clone(
        construct_model_info_offline_for_tests(model, &config.to_models_manager_config()).model,
    )
}

#[derive(Clone, Copy)]
pub enum TestCodexResponsesRequestKind {
    Turn,
    Prewarm,
    WebsocketConnection,
}

#[allow(clippy::too_many_arguments)]
pub fn responses_metadata(
    installation_id: &str,
    session_id: &str,
    thread_id: &str,
    turn_id: Option<&str>,
    window_id: String,
    session_source: &SessionSource,
    parent_thread_id: Option<ThreadId>,
    request_kind: TestCodexResponsesRequestKind,
) -> CodexResponsesMetadata {
    let request_kind = match request_kind {
        TestCodexResponsesRequestKind::Turn => Some(CodexResponsesRequestKind::Turn),
        TestCodexResponsesRequestKind::Prewarm => Some(CodexResponsesRequestKind::Prewarm),
        TestCodexResponsesRequestKind::WebsocketConnection => None,
    };
    CodexResponsesMetadata {
        turn_id: request_kind.and(turn_id.map(ToString::to_string)),
        request_kind,
        parent_thread_id,
        subagent_header: subagent_header_value(session_source),
        subagent_kind: request_kind.and_then(|_| subagent_metadata_kind(session_source)),
        ..CodexResponsesMetadata::new(
            installation_id.to_string(),
            session_id.to_string(),
            thread_id.to_string(),
            window_id,
        )
    }
}

pub fn with_parent_turn(mut metadata: CodexResponsesMetadata, id: &str) -> CodexResponsesMetadata {
    metadata.parent_turn_id = Some(id.to_string());
    metadata
}

pub fn all_model_presets() -> &'static Vec<ModelPreset> {
    &TEST_MODEL_PRESETS
}

pub fn builtin_collaboration_mode_presets() -> Vec<CollaborationModeMask> {
    collaboration_mode_presets::builtin_collaboration_mode_presets()
}
