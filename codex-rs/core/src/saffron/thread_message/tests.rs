//! Delivery checks through real thread managers and the turn-input boundary.

use super::*;
use crate::CodexAppsToolsCache;
use crate::build_models_manager;
use crate::init_state_db;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use crate::thread_manager::NewThread;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::thread_store_from_config;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::test_codex::run_test_with_large_stack;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

/// Idle delivery is attributed tool output and does not replace saved settings.
#[test_case::test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case::test_case(ThreadHistoryMode::Paginated; "paginated")]
fn idle_delivery_preserves_destination(history_mode: ThreadHistoryMode) -> anyhow::Result<()> {
    run_test_with_large_stack("idle thread message", move || async move {
        let server = MockServer::start().await;
        let response = mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("message-response"),
                ev_assistant_message("message-answer", "received"),
                ev_completed("message-response"),
            ]),
        )
        .await;
        let host = TestHost::new(&server, history_mode).await?;
        let before = host.destination.thread.config_snapshot().await;
        let receipt = host
            .send(host.destination.thread_id, "handoff-marker")
            .await?;
        assert_eq!(receipt["status"], "started");
        assert_eq!(
            receipt["source_thread_id"],
            host.source.thread_id.to_string()
        );
        assert_eq!(receipt["thread_id"], host.destination.thread_id.to_string());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let EventMsg::TurnComplete(event) =
                    host.destination.thread.next_event().await?.msg
                {
                    assert_eq!(event.turn_id, receipt["turn_id"].as_str().unwrap());
                    assert_eq!(event.last_agent_message.as_deref(), Some("received"));
                    assert!(event.error.is_none());
                    break;
                }
            }
            anyhow::Ok(())
        })
        .await??;
        let request = response.single_request().body_json();
        let delivered = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["namespace"] == "saffron")
            .expect("the model receives an attributed standalone tool output");
        assert_eq!(delivered["name"], "send_message_to_thread");
        let content: serde_json::Value =
            serde_json::from_str(delivered["output"].as_str().unwrap())?;
        assert_eq!(
            content,
            json!({"source_thread_id": host.source.thread_id, "input": "handoff-marker"})
        );
        let after = host.destination.thread.config_snapshot().await;
        assert_eq!(after.model, before.model);
        assert_eq!(after.approval_policy, before.approval_policy);
        assert_eq!(after.cwd(), before.cwd());
        assert_eq!(after.ephemeral, before.ephemeral);
        let instructions = request.to_string();
        assert!(instructions.contains("destination-instructions-marker"));
        assert!(!instructions.contains("sender-instructions-marker"));
        host.destination.thread.flush_rollout().await?;
        assert!(
            host.destination
                .thread
                .read_thread(true, false)
                .await?
                .rollout_path
                .is_some()
        );
        host.shutdown().await
    })
}

/// An active regular turn receives the message without replacement or interruption.
#[test]
fn busy_delivery_steers_the_existing_turn() -> anyhow::Result<()> {
    run_test_with_large_stack("busy thread message", || async {
        let server = MockServer::start().await;
        let host = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        let (_, mut active_context) = make_session_and_context().await;
        active_context.sub_id = "active-destination-turn".to_string();
        let active_context = Arc::new(active_context);
        host.destination
            .thread
            .session
            .spawn_task(
                Arc::clone(&active_context),
                Vec::new(),
                WaitingTask(TaskKind::Regular),
            )
            .await;
        let receipt = host
            .send(host.destination.thread_id, "steering-marker")
            .await?;
        assert_eq!(receipt["status"], "steered");
        assert_eq!(receipt["turn_id"], "active-destination-turn");
        let pending = host
            .destination
            .thread
            .session
            .input_queue
            .get_pending_input(&host.destination.thread.session.active_turn)
            .await
            .0;
        assert_eq!(pending.len(), 1);
        let crate::session::TurnInput::FunctionCallOutput(output) = &pending[0] else {
            panic!("steering must retain tool-output provenance");
        };
        let ResponseItem::FunctionCallOutput {
            output, namespace, ..
        } = &output.item
        else {
            panic!("expected a standalone output");
        };
        assert_eq!(namespace.as_deref(), Some("saffron"));
        let content: serde_json::Value = serde_json::from_str(output.text_content().unwrap())?;
        assert_eq!(content["input"], "steering-marker");
        assert_eq!(
            content["source_thread_id"],
            host.source.thread_id.to_string()
        );
        assert!(
            host.destination
                .thread
                .session
                .active_turn
                .lock()
                .await
                .is_some()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        host.shutdown().await
    })
}

/// A task that does not support steering is left running and receives no input.
#[test]
fn nonsteerable_turn_is_not_interrupted() -> anyhow::Result<()> {
    run_test_with_large_stack("message review boundary", || async {
        let server = MockServer::start().await;
        let host = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        let (_, context) = make_session_and_context().await;
        host.destination
            .thread
            .session
            .spawn_task(Arc::new(context), Vec::new(), WaitingTask(TaskKind::Review))
            .await;
        let error = host
            .send(host.destination.thread_id, "not delivered")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ActiveTurnNotSteerable"));
        let pending = host
            .destination
            .thread
            .session
            .input_queue
            .get_pending_input(&host.destination.thread.session.active_turn)
            .await
            .0;
        assert!(pending.is_empty());
        assert!(
            host.destination
                .thread
                .session
                .active_turn
                .lock()
                .await
                .is_some()
        );
        host.shutdown().await
    })
}

/// A thread loaded by another registry cannot be reached even on the same host.
#[test]
fn foreign_and_unloaded_destinations_are_not_resumed() -> anyhow::Result<()> {
    run_test_with_large_stack("message process scope", || async {
        let server = MockServer::start().await;
        let host = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        let foreign = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        let error = host
            .send(foreign.destination.thread_id, "not delivered")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("destination must be loaded in this app-server process")
        );
        host.destination.thread.shutdown_and_wait().await?;
        host.manager
            .remove_thread(&host.destination.thread_id)
            .await;
        let error = host
            .send(host.destination.thread_id, "not resumed")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("resume it in the host first"));
        assert!(
            host.manager
                .get_thread(host.destination.thread_id)
                .await
                .is_err()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        host.source.thread.shutdown_and_wait().await?;
        foreign.shutdown().await
    })
}

/// Caller input cannot supply sender identity, configuration, or blank content.
#[test]
fn invalid_arguments_have_no_delivery_effect() -> anyhow::Result<()> {
    run_test_with_large_stack("message argument validation", || async {
        let server = MockServer::start().await;
        let host = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        for arguments in [
            json!({"thread_id": host.destination.thread_id, "prompt": " \n\t"}),
            json!({"thread_id": "not-a-thread", "prompt": "hello"}),
            json!({"thread_id": host.destination.thread_id, "prompt": "hello", "source_thread_id": "forged"}),
            json!({"thread_id": host.destination.thread_id, "prompt": "hello", "model": "override"}),
        ] {
            assert!(host.invoke(arguments).await.is_err());
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        host.shutdown().await
    })
}

/// Parent-owned agents keep their existing collaboration and lifecycle contract.
#[test]
fn subagent_destination_is_rejected() -> anyhow::Result<()> {
    run_test_with_large_stack("message root boundary", || async {
        let server = MockServer::start().await;
        let host = TestHost::new(&server, ThreadHistoryMode::Legacy).await?;
        let child = host
            .manager
            .start_thread(StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                    "test-helper".to_string(),
                ))),
                ..StartThreadOptions::new(host.turn.config.as_ref().clone())
            })
            .await?;
        let error = host
            .send(child.thread_id, "not delivered")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("destination must be a root thread")
        );
        assert!(
            child
                .thread
                .session
                .services
                .thread_extension_data
                .get::<MessageHost>()
                .is_none()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        child.thread.shutdown_and_wait().await?;
        host.shutdown().await
    })
}

/// A pair of independent roots in a host with the production extension installed.
struct TestHost {
    /// Keeps the temporary home and base configuration alive.
    _fixture: Session,
    /// Registry that defines the process boundary under test.
    manager: Arc<ThreadManager>,
    /// Runtime sender, independent of model-provided arguments.
    source: NewThread,
    /// Root whose lifecycle and settings must survive delivery.
    destination: NewThread,
    /// Captured calling context used to invoke the model-facing handler.
    turn: Arc<TurnContext>,
}

impl TestHost {
    /// Starts roots with distinct instruction settings and a local model endpoint.
    async fn new(server: &MockServer, history_mode: ThreadHistoryMode) -> anyhow::Result<Self> {
        let (fixture, turn) = make_session_and_context().await;
        let mut config = turn.config.as_ref().clone();
        config.ephemeral = false;
        config.model_provider.base_url = Some(server.uri());
        config.model_provider.supports_websockets = false;
        let state = init_state_db(&config).await.expect("test state database");
        let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let manager = Arc::new_cyclic(|manager| {
            let mut builder = ExtensionRegistryBuilder::default();
            install(&mut builder, manager.clone());
            ThreadManager::new(
                &config,
                Arc::clone(&auth),
                build_models_manager(&config, Arc::clone(&auth)),
                CodexAppsToolsCache::default(),
                SessionSource::Exec,
                Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
                Arc::new(builder.build()),
                Arc::new(crate::test_support::EmptyUserInstructionsProvider),
                None,
                crate::passthrough_image_store(),
                thread_store_from_config(&config, Some(state)),
                None,
                "message-test".to_string(),
                None,
                None,
            )
        });
        config.developer_instructions = Some("sender-instructions-marker".to_string());
        let source = manager
            .start_thread(StartThreadOptions {
                history_mode: Some(history_mode),
                ..StartThreadOptions::new(config.clone())
            })
            .await?;
        config.developer_instructions = Some("destination-instructions-marker".to_string());
        let destination = manager
            .start_thread(StartThreadOptions {
                history_mode: Some(history_mode),
                ..StartThreadOptions::new(config)
            })
            .await?;
        Ok(Self {
            _fixture: fixture,
            manager,
            source,
            destination,
            turn: Arc::new(turn),
        })
    }

    /// Invokes the handler with only the caller-controlled argument object.
    async fn invoke(&self, arguments: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let mut registry = ToolRegistry::default();
        register(&self.source.thread.session, &self.turn, &mut registry);
        let handler = registry
            .remove(&ToolName::namespaced("saffron", "send_message_to_thread"))
            .expect("production lifecycle and registration expose messaging on roots");
        let output = handler
            .handle(ToolInvocation {
                session: Arc::clone(&self.source.thread.session),
                step_context: StepContext::for_test(Arc::clone(&self.turn)),
                turn: Arc::clone(&self.turn),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
                call_id: "message-call".to_string(),
                tool_name: ToolName::namespaced("saffron", "send_message_to_thread"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            })
            .await?;
        Ok(serde_json::from_str(&output.log_output())?)
    }

    /// Sends one ordinary message and decodes the acceptance receipt.
    async fn send(&self, destination: ThreadId, prompt: &str) -> anyhow::Result<serde_json::Value> {
        self.invoke(json!({"thread_id": destination, "prompt": prompt}))
            .await
    }

    /// Joins both session loops before their temporary home can be released.
    async fn shutdown(self) -> anyhow::Result<()> {
        self.source.thread.shutdown_and_wait().await?;
        self.destination.thread.shutdown_and_wait().await?;
        Ok(())
    }
}

/// Holds a regular turn open until normal shutdown cancels it.
struct WaitingTask(
    /// Controls whether the existing task permits steering.
    TaskKind,
);

impl SessionTask for WaitingTask {
    fn kind(&self) -> TaskKind {
        self.0
    }

    fn span_name(&self) -> &'static str {
        "session_task.thread_message_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<crate::session::TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        Ok(None)
    }
}
