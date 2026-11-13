//! Native creation and persistence are exercised in both supported history modes.

use super::*;
use crate::CodexAppsToolsCache;
use crate::ThreadStoreBuilder;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::build_startup_models;
use crate::init_state_db;
use crate::session::tests::make_session_and_context;
use crate::thread_manager::StartThreadOptions;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_models_manager::manager::ModelCatalogSettings;
use codex_protocol::AgentPath;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::test_codex::run_test_with_large_stack;
use std::time::Duration;
use wiremock::MockServer;

/// Owns a real root, completed child, mock model endpoint, and temporary storage.
struct Fixture {
    /// Retains the temporary configuration and storage directories.
    _session: Session,
    /// Serves deterministic model responses without external requests.
    _server: MockServer,
    /// Native host used for both child creation and promotion.
    manager: Arc<ThreadManager>,
    /// Parent that owns the source and invokes promotion.
    root: Arc<CodexThread>,
    /// Completed source, retained for assertions after runtime retirement.
    child: Arc<CodexThread>,
    /// Original identity used to reconcile retries after task-name removal.
    child_id: ThreadId,
}

impl Fixture {
    /// Runs a child to completion using the requested native history format.
    async fn completed(mode: ThreadHistoryMode) -> anyhow::Result<Self> {
        let server = MockServer::start().await;
        mount_sse_once(
            &server,
            sse(vec![
                ev_response_created("child-response"),
                ev_assistant_message("child-message", "completed-child-history"),
                ev_completed("child-response"),
            ]),
        )
        .await;
        let (fixture, turn) = make_session_and_context().await;
        let mut config = turn.config.as_ref().clone();
        config.ephemeral = false;
        config.experimental_thread_store = crate::config::ThreadStoreConfig::Local;
        let _ = config
            .features
            .disable(codex_features::Feature::BackgroundPaginatedRolloutMigration);
        config.model_reasoning_effort = Some(ReasoningEffort::High);
        config.model_provider.base_url = Some(server.uri());
        config.model_provider.supports_websockets = false;
        let state = init_state_db(&config).await.expect("state database");
        let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
        let manager = Arc::new(ThreadManager::new(
            &config,
            build_startup_models(
                &config,
                auth,
                "promotion-test".into(),
                Arc::new(ModelCatalogSettings::disk(config.codex_home.to_path_buf())),
            ),
            CodexAppsToolsCache::default(),
            SessionSource::Exec,
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            codex_extension_api::empty_extension_registry(),
            Arc::new(crate::test_support::EmptyUserInstructionsProvider),
            None,
            crate::passthrough_image_store(),
            ThreadStoreBuilder::from_config(&config, Some(state.clone())).build(),
            crate::local_agent_graph_store_from_state_db(Some(&state)),
            None,
            None,
        ));
        let root = manager
            .start_thread(StartThreadOptions {
                history_mode: Some(mode),
                ..StartThreadOptions::new(config.clone())
            })
            .await?;
        let control = local_control(&root.thread.session);
        let child = control
            .spawn_agent_with_metadata(
                config,
                vec![UserInput::Text {
                    text: "original-child-assignment".into(),
                    text_elements: Vec::new(),
                }],
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root.thread_id,
                    depth: 1,
                    agent_path: Some(AgentPath::root().join("worker").unwrap()),
                    agent_nickname: None,
                    agent_role: None,
                }),
                SpawnAgentOptions {
                    parent_thread_id: Some(root.thread_id),
                    fork_mode: (mode == ThreadHistoryMode::Legacy)
                        .then_some(SpawnAgentForkMode::FullHistory),
                    fork_parent_spawn_call_id: (mode == ThreadHistoryMode::Legacy)
                        .then(|| "promotion-fixture-spawn".into()),
                    ..Default::default()
                },
            )
            .await?;
        let thread = manager.get_thread(child.thread_id).await?;
        let mut status = thread.subscribe_status();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let observed = status.borrow_and_update().clone();
                match observed {
                    AgentStatus::Completed(_) => break,
                    AgentStatus::Errored(error) => panic!("child failed: {error}"),
                    _ => status.changed().await.unwrap(),
                }
            }
        })
        .await?;
        thread.flush_rollout().await?;
        Ok(Self {
            _session: fixture,
            _server: server,
            manager,
            root: root.thread,
            child: thread,
            child_id: child.thread_id,
        })
    }

    /// Calls the feature with the owning root and the existing source identity.
    async fn promote(&self) -> anyhow::Result<serde_json::Value> {
        promote(
            self.manager.clone(),
            self.root.session.clone(),
            SessionSource::Exec,
            self.child_id,
        )
        .await
    }
}

/// A promoted copy keeps all conversation items and settings and outlives its former parent.
#[test_case::test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case::test_case(ThreadHistoryMode::Paginated; "paginated")]
fn promotion_preserves_completed_history_settings_and_independence(
    mode: ThreadHistoryMode,
) -> anyhow::Result<()> {
    run_test_with_large_stack("promotion", move || async move {
        let fixture = Fixture::completed(mode).await?;
        let before = fixture.child.config_snapshot().await;
        assert_eq!(before.history_mode, mode);
        let original_messages = saved_conversation(&fixture.child).await?;
        let output = fixture.promote().await?;
        assert_eq!(output["status"], "promoted");
        let destination = ThreadId::from_string(output["thread_id"].as_str().unwrap())?;
        assert_ne!(destination, fixture.child_id);
        let root = fixture.manager.get_thread(destination).await?;
        let saved = root.read_thread(true, false).await?;
        assert!(!saved.source.is_non_root_agent());
        assert!(saved.parent_thread_id.is_none());
        let copied_messages = saved_conversation(&root).await?;
        assert!(copied_messages.starts_with(&original_messages));
        let history = serde_json::to_string(&copied_messages)?;
        assert!(history.contains("original-child-assignment"));
        assert!(history.contains("completed-child-history"));
        let after = root.config_snapshot().await;
        assert_eq!(after.model, before.model);
        assert_eq!(after.reasoning_effort, before.reasoning_effort);
        assert_eq!(after.approval_policy, before.approval_policy);
        assert_eq!(after.approvals_reviewer, before.approvals_reviewer);
        assert_eq!(after.service_tier, before.service_tier);
        assert_eq!(after.permission_profile, before.permission_profile);
        assert_eq!(after.workspace_roots, before.workspace_roots);
        assert_eq!(after.cwd(), before.cwd());
        assert_eq!(after.disabled_plugin_ids, before.disabled_plugin_ids);
        assert_eq!(after.collaboration_mode, before.collaboration_mode);
        assert_eq!(after.history_mode, before.history_mode);
        assert!(!after.ephemeral);
        assert!(fixture.manager.get_thread(fixture.child_id).await.is_err());
        let retry = fixture.promote().await?;
        assert_eq!(retry, output);
        fixture.root.shutdown_and_wait().await?;
        assert!(root.is_running());
        assert!(!root.has_active_turn().await);
        mount_sse_once(
            &fixture._server,
            sse(vec![
                ev_response_created("standalone-response"),
                ev_assistant_message("standalone-message", "independent continuation"),
                ev_completed("standalone-response"),
            ]),
        )
        .await;
        let mut status = root.subscribe_status();
        root.start_or_steer_turn(crate::TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue independently".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let observed = status.borrow_and_update().clone();
                match observed {
                    AgentStatus::Completed(Some(answer))
                        if answer.contains("independent continuation") =>
                    {
                        break;
                    }
                    AgentStatus::Errored(error) => panic!("standalone failed: {error}"),
                    _ => status.changed().await.unwrap(),
                }
            }
        })
        .await?;
        root.shutdown_and_wait().await?;
        Ok(())
    })
}

/// Reads every saved conversation item through the history format's supported API.
async fn saved_conversation(thread: &CodexThread) -> anyhow::Result<Vec<serde_json::Value>> {
    if thread.config_snapshot().await.history_mode == ThreadHistoryMode::Legacy {
        return thread
            .read_thread(true, true)
            .await?
            .history
            .unwrap()
            .items
            .iter()
            .filter(|item| matches!(item, codex_history::RolloutItem::ResponseItem(_)))
            .map(|item| serde_json::to_value(item).map_err(Into::into))
            .collect();
    }
    let mut cursor = None;
    let mut items = Vec::new();
    loop {
        let page = thread
            .session
            .services
            .thread_store
            .list_items(codex_thread_store::ListItemsParams {
                thread_id: thread.session.thread_id(),
                turn_id: None,
                include_archived: true,
                position: cursor.map(codex_thread_store::ListItemsPosition::Cursor),
                item_ids: None,
                page_size: 1,
                sort_direction: codex_thread_store::SortDirection::Asc,
                sort_key: codex_thread_store::ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
                before_updated_at_ordinal: None,
            })
            .await?;
        for item in page.items {
            items.push(serde_json::json!({
                "turn_id": item.turn_id,
                "item_id": item.item_id,
                "created_at_ms": item.created_at_ms,
                "started_at_ms": item.started_at_ms,
                "completed_at_ms": item.completed_at_ms,
                "item": serde_json::from_slice::<serde_json::Value>(&item.item_json)?,
            }));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(items);
        }
    }
}

/// Separate store handles and reopening preserve one reservation winner and its destination.
#[tokio::test]
async fn promotion_receipt_survives_reopen_and_has_one_reservation_winner() -> anyhow::Result<()> {
    let (_fixture, turn) = make_session_and_context().await;
    let sqlite = turn.config.sqlite_config();
    let first = SaffronStore::open(sqlite).await?;
    let second = SaffronStore::open(sqlite).await?;
    let source = ThreadId::new();
    let receipt = Promotion {
        source_id: source,
        root_id: ThreadId::new(),
        destination_id: ThreadId::new(),
        members: vec![CompletedAgent {
            thread_id: source,
            parent_id: ThreadId::new(),
            turn_id: "completed".into(),
        }],
        phase: PromotionPhase::Preparing,
    };
    let (a, b) = tokio::join!(
        first.reserve_promotion(&receipt),
        second.reserve_promotion(&receipt)
    );
    assert_ne!(a?, b?);
    drop(first);
    drop(second);
    let reopened = SaffronStore::open(sqlite).await?;
    let saved = reopened.promotion(source).await?.unwrap();
    assert_eq!(saved.destination_id, receipt.destination_id);
    assert_eq!(saved.members[0].turn_id, "completed");
    assert_eq!(saved.phase, PromotionPhase::Preparing);
    Ok(())
}

/// Admission held by another operation refuses promotion before any destination is reserved.
#[test]
fn promotion_refuses_unfinished_work_before_reserving_destination() -> anyhow::Result<()> {
    run_test_with_large_stack("promotion refusal", || async {
        let fixture = Fixture::completed(ThreadHistoryMode::Legacy).await?;
        let slot = fixture.child.session.active_turn.lock().await;
        let error = fixture.promote().await.unwrap_err();
        assert!(error.to_string().contains("accepting work"), "{error:#}");
        drop(slot);
        let store =
            SaffronStore::open(fixture.root.session.get_config().await.sqlite_config()).await?;
        assert!(store.promotion(fixture.child_id).await?.is_none());
        fixture.child.shutdown_and_wait().await?;
        fixture.root.shutdown_and_wait().await?;
        Ok(())
    })
}

/// An unconfirmed creation is reported with its existing identity and leaves the source alive.
#[test]
fn retry_of_uncertain_creation_returns_reserved_identity_without_retirement() -> anyhow::Result<()>
{
    run_test_with_large_stack("promotion recovery", || async {
        let fixture = Fixture::completed(ThreadHistoryMode::Legacy).await?;
        let store =
            SaffronStore::open(fixture.root.session.get_config().await.sqlite_config()).await?;
        let receipt = Promotion {
            source_id: fixture.child_id,
            root_id: fixture.root.session.thread_id(),
            destination_id: ThreadId::new(),
            members: Vec::new(),
            phase: PromotionPhase::Preparing,
        };
        assert!(store.reserve_promotion(&receipt).await?);
        let output = fixture.promote().await?;
        assert_eq!(output["status"], "creation_unconfirmed");
        assert_eq!(output["thread_id"], receipt.destination_id.to_string());
        assert!(fixture.child.is_running());
        assert!(
            fixture
                .manager
                .get_thread(receipt.destination_id)
                .await
                .is_err()
        );
        fixture.child.shutdown_and_wait().await?;
        fixture.root.shutdown_and_wait().await?;
        Ok(())
    })
}

/// A real running grandchild prevents retirement or copying of its completed parent.
#[test]
fn unfinished_descendant_keeps_the_original_tree() -> anyhow::Result<()> {
    run_test_with_large_stack("promotion descendant refusal", || async {
        let fixture = Fixture::completed(ThreadHistoryMode::Legacy).await?;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                core_test_support::responses::sse_response(sse(vec![
                    ev_response_created("pending-descendant"),
                    ev_completed("pending-descendant"),
                ]))
                .set_delay(Duration::from_secs(60)),
            )
            .mount(&fixture._server)
            .await;
        let control = local_control(&fixture.child.session);
        let descendant = control
            .spawn_agent_with_metadata(
                fixture.child.session.get_config().await.as_ref().clone(),
                vec![UserInput::Text {
                    text: "unfinished-descendant".into(),
                    text_elements: Vec::new(),
                }],
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: fixture.child_id,
                    depth: 2,
                    agent_path: Some(
                        AgentPath::root()
                            .join("worker")
                            .unwrap()
                            .join("pending")
                            .unwrap(),
                    ),
                    agent_nickname: None,
                    agent_role: None,
                }),
                SpawnAgentOptions {
                    parent_thread_id: Some(fixture.child_id),
                    ..Default::default()
                },
            )
            .await?;
        let descendant = fixture.manager.get_thread(descendant.thread_id).await?;
        let mut status = descendant.subscribe_status();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(*status.borrow_and_update(), AgentStatus::Running) {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await?;
        assert!(fixture.promote().await.is_err());
        let store =
            SaffronStore::open(fixture.root.session.get_config().await.sqlite_config()).await?;
        assert!(store.promotion(fixture.child_id).await?.is_none());
        assert!(fixture.child.is_running());
        assert!(descendant.has_active_turn().await);
        descendant.shutdown_and_wait().await?;
        fixture.child.shutdown_and_wait().await?;
        fixture.root.shutdown_and_wait().await?;
        Ok(())
    })
}

/// Work accepted after copying prevents retirement, even after the newer turn completes.
#[test]
fn ready_retry_preserves_newer_completed_work_and_the_existing_copy() -> anyhow::Result<()> {
    run_test_with_large_stack("promotion changed original", || async {
        let fixture = Fixture::completed(ThreadHistoryMode::Legacy).await?;
        let store =
            SaffronStore::open(fixture.root.session.get_config().await.sqlite_config()).await?;
        let slot = fixture.child.session.active_turn.lock().await;
        let receipt = Promotion {
            source_id: fixture.child_id,
            root_id: fixture.root.session.thread_id(),
            destination_id: ThreadId::new(),
            members: vec![CompletedAgent {
                thread_id: fixture.child_id,
                parent_id: fixture.root.session.thread_id(),
                turn_id: fixture
                    .child
                    .session
                    .checkpoint_idle_completion()
                    .await
                    .unwrap(),
            }],
            phase: PromotionPhase::Preparing,
        };
        assert!(store.reserve_promotion(&receipt).await?);
        create_root(
            &fixture.manager,
            &store,
            &fixture.child,
            SessionSource::Exec,
            &receipt,
        )
        .await?;
        store
            .set_promotion_phase(fixture.child_id, PromotionPhase::Ready)
            .await?;
        drop(slot);
        mount_sse_once(
            &fixture._server,
            sse(vec![
                ev_response_created("newer-response"),
                ev_assistant_message("newer-message", "newly-completed-work"),
                ev_completed("newer-response"),
            ]),
        )
        .await;
        let mut status = fixture.child.subscribe_status();
        local_control(&fixture.root.session)
            .send_input(
                fixture.child_id,
                vec![UserInput::Text {
                    text: "new-work-after-copy".into(),
                    text_elements: Vec::new(),
                }],
                Default::default(),
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(&*status.borrow_and_update(), AgentStatus::Completed(Some(text)) if text.contains("newly-completed-work")) { break; }
                status.changed().await.unwrap();
            }
        }).await?;
        let result = fixture.promote().await?;
        assert_eq!(result["status"], "created_retirement_pending");
        assert_eq!(result["thread_id"], receipt.destination_id.to_string());
        assert!(fixture.child.is_running());
        let copied = fixture.manager.get_thread(receipt.destination_id).await?;
        assert!(copied.is_running());
        let copied_history =
            serde_json::to_string(&copied.read_thread(true, true).await?.history.unwrap().items)?;
        assert!(!copied_history.contains("new-work-after-copy"));
        let retry = fixture.promote().await?;
        assert_eq!(retry["thread_id"], result["thread_id"]);
        copied.shutdown_and_wait().await?;
        fixture.child.shutdown_and_wait().await?;
        fixture.root.shutdown_and_wait().await?;
        Ok(())
    })
}
