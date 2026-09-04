use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadGoalUpdatedNotification;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_can_edit_its_active_goal_without_changing_other_goal_state() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("create-goal"),
                responses::ev_function_call(
                    "create-goal-call",
                    "create_goal",
                    &json!({
                        "objective": "ship the release",
                        "token_budget": 20_000,
                    })
                    .to_string(),
                ),
                responses::ev_completed("create-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("edit-goal"),
                responses::ev_function_call_with_namespace(
                    "edit-goal-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({ "objective": "ship the release with release notes" }).to_string(),
                ),
                responses::ev_completed("edit-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("complete-goal"),
                responses::ev_function_call(
                    "complete-goal-call",
                    "update_goal",
                    &json!({ "status": "complete" }).to_string(),
                ),
                responses::ev_completed("complete-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("root-finished"),
                responses::ev_assistant_message("root-message", "Release shipped."),
                responses::ev_completed("root-finished"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    let rollout_path = thread.thread.path.clone().context("thread rollout path")?;

    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Ship the release and keep the goal accurate.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1]
            .tool_by_name("saffron", "edit_active_goal")
            .is_some(),
        "the root should receive the active-goal editing tool"
    );
    assert!(
        requests[2]
            .function_call_output_text("edit-goal-call")
            .is_some_and(|output| {
                output.contains("ship the release with release notes")
                    && output.contains("20000")
                    && output.contains("active")
            }),
        "the edit result should retain the goal budget and active status"
    );

    let persisted_rollout = std::fs::read_to_string(rollout_path)?;
    assert!(persisted_rollout.lines().any(|line| {
        line.contains(r#""type":"thread_goal_updated""#)
            && line.contains(r#""objective":"ship the release with release notes""#)
    }));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_root_goal_runs_hidden_saffron_supervisor() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("parent-create-goal"),
                responses::ev_function_call(
                    "create-goal-call",
                    "create_goal",
                    &json!({ "objective": "wait for the deployment" }).to_string(),
                ),
                responses::ev_completed("parent-create-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("parent-idle"),
                responses::ev_assistant_message("parent-message", "Waiting for deployment."),
                responses::ev_completed("parent-idle"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-edit"),
                responses::ev_function_call_with_namespace(
                    "supervisor-edit-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({
                        "objective": "wait for the deployment and verify production health",
                    })
                    .to_string(),
                ),
                responses::ev_completed("supervisor-edit"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-second-edit"),
                responses::ev_function_call_with_namespace(
                    "supervisor-second-edit-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({ "objective": "replace the objective a second time" }).to_string(),
                ),
                responses::ev_completed("supervisor-second-edit"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-action"),
                responses::ev_function_call_with_namespace(
                    "supervisor-snooze-call",
                    "saffron",
                    "supervisor_snooze",
                    &json!({ "delay_seconds": 3600 }).to_string(),
                ),
                responses::ev_completed("supervisor-action"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-finished"),
                responses::ev_assistant_message("supervisor-message", "Snoozed."),
                responses::ev_completed("supervisor-finished"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", server.uri()))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Track the deployment until it completes.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(10), async {
        while model_responses.requests().len() < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("supervisor did not finish its check-in")?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 6);
    assert!(
        requests[2]
            .tool_by_name("saffron", "supervisor_snooze")
            .is_some(),
        "the helper should receive the Saffron supervisor action tools"
    );
    assert!(
        requests[2]
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("# Supervisor Check-in")),
        "the helper should receive the active-goal assignment"
    );
    assert!(
        requests[3]
            .function_call_output_text("supervisor-edit-call")
            .is_some_and(|output| output.contains("verify production health")),
        "the supervisor should observe the committed replacement objective"
    );
    assert!(
        requests[4]
            .function_call_output_text("supervisor-second-edit-call")
            .is_some_and(|output| output.contains("already edited")),
        "the supervisor should not be able to edit the goal twice"
    );
    assert!(
        requests[5]
            .function_call_output_text("supervisor-snooze-call")
            .is_some_and(|output| output.contains("3600")),
        "the edit should not consume the required disposition action"
    );

    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_invalid_request_blocks_parent_goal() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("parent-create-goal"),
                responses::ev_function_call(
                    "create-goal-call",
                    "create_goal",
                    &json!({ "objective": "wait for the deployment" }).to_string(),
                ),
                responses::ev_completed("parent-create-goal"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("parent-idle"),
                responses::ev_assistant_message("parent-message", "Waiting for deployment."),
                responses::ev_completed("parent-idle"),
            ])),
            ResponseTemplate::new(/*status*/ 400).set_body_json(json!({
                "error": {
                    "message": "this model requires a newer Codex version",
                    "type": "invalid_request_error",
                    "param": null,
                    "code": null,
                }
            })),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", server.uri()))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Track the deployment until it completes.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    let goal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let notification: ThreadGoalUpdatedNotification =
                app_server.read_notification("thread/goal/updated").await?;
            if notification.goal.status == ThreadGoalStatus::Blocked {
                return Ok::<_, anyhow::Error>(notification.goal);
            }
        }
    })
    .await
    .context("supervisor did not block the goal")??;

    assert_eq!(goal.status, ThreadGoalStatus::Blocked);
    assert_eq!(model_responses.requests().len(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovers_active_goal_without_thread_resume() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, thread_id) = restart_goal_fixture(&server, "recover after restart").await?;

    let _app_server = start_restart_app(&codex_home).await?;

    wait_for_supervisor_requests(&server, 1).await?;
    let bodies = supervisor_request_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains(&thread_id.to_string()));
    assert!(bodies[0].contains("recover after restart"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovery_preserves_unset_persisted_reasoning_effort() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, _) = restart_goal_fixture(&server, "recover persisted settings").await?;
    let config_path = codex_home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path)?;
    config.push_str("\nmodel_reasoning_effort = \"high\"\n");
    std::fs::write(config_path, config)?;

    let _app_server = start_restart_app(&codex_home).await?;

    wait_for_supervisor_requests(&server, 1).await?;
    let bodies = supervisor_request_bodies(&server).await;
    let request_body: serde_json::Value = serde_json::from_str(&bodies[0])?;
    assert_eq!(request_body["reasoning"]["effort"], serde_json::Value::Null);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_scheduler_has_one_owner_per_codex_home() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, _) = restart_goal_fixture(&server, "recover exactly once").await?;

    let _first = start_restart_app(&codex_home).await?;
    let _second = start_restart_app(&codex_home).await?;

    wait_for_supervisor_requests(&server, 1).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(supervisor_request_bodies(&server).await.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_app_server_unloads_snoozed_root_and_recovers_at_deadline() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("supervisor-snooze"),
                responses::ev_function_call_with_namespace(
                    "supervisor-snooze-call",
                    "saffron",
                    "supervisor_snooze",
                    &json!({ "delay_seconds": 3 }).to_string(),
                ),
                responses::ev_completed("supervisor-snooze"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-finished"),
                responses::ev_assistant_message("supervisor-message", "Snoozed."),
                responses::ev_completed("supervisor-finished"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("recovered-supervisor"),
                responses::ev_assistant_message(
                    "recovered-supervisor-message",
                    "Recovered after session recreation.",
                ),
                responses::ev_completed("recovered-supervisor"),
            ]),
        ],
    )
    .await;

    let (codex_home, thread_id) = restart_goal_fixture(&server, "wait for the release").await?;
    let mut app_server = start_restart_app(&codex_home).await?;
    app_server.initialize().await?;

    timeout(RESTART_TIMEOUT, async {
        while model_responses.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("initial supervisor did not finish snoozing")?;

    timeout(RESTART_TIMEOUT, async {
        loop {
            let request_id = app_server
                .send_thread_loaded_list_request(ThreadLoadedListParams::default())
                .await
                .expect("send loaded-thread request");
            let loaded: ThreadLoadedListResponse = app_server
                .read_response(request_id)
                .await
                .expect("read loaded-thread response");
            if !loaded.data.contains(&thread_id.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("snoozed recovered root remained loaded")?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        model_responses.requests().len(),
        2,
        "session recreation must preserve the absolute snooze deadline"
    );

    timeout(RESTART_TIMEOUT, async {
        while model_responses.requests().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("same-process session unload did not recover its supervisor")?;
    let bodies = supervisor_request_bodies(&server).await;
    assert_eq!(bodies.len(), 3);
    assert!(bodies[2].contains("wait for the release"));
    assert_eq!(model_responses.requests().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_ignores_goal_paused_before_activation() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, thread_id) = restart_goal_fixture(&server, "pause before restart").await?;
    let state_db = test_state_db(codex_home.path()).await?;
    state_db
        .thread_goals()
        .pause_active_thread_goal(thread_id)
        .await?;

    let _app_server = start_restart_app(&codex_home).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(
        supervisor_request_bodies(&server).await,
        Vec::<String>::new()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_ignores_archived_active_goal() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, thread_id) = restart_goal_fixture(&server, "archive before restart").await?;
    let state_db = test_state_db(codex_home.path()).await?;
    let metadata = state_db
        .get_thread(thread_id)
        .await?
        .context("seeded thread metadata")?;
    state_db
        .mark_archived(thread_id, &metadata.rollout_path, metadata.updated_at)
        .await?;

    let _app_server = start_restart_app(&codex_home).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(
        supervisor_request_bodies(&server).await,
        Vec::<String>::new()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_goal_does_not_immediately_unload_root_after_failed_checkin() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("No action selected.").await;
    let (codex_home, thread_id) = restart_goal_fixture(&server, "retry after failure").await?;
    let mut app_server = start_restart_app(&codex_home).await?;

    wait_for_supervisor_requests(&server, 1).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    test_state_db(codex_home.path())
        .await?
        .thread_goals()
        .pause_active_thread_goal(thread_id)
        .await?;
    app_server.initialize().await?;

    let loaded_id = app_server
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let loaded: ThreadLoadedListResponse =
        timeout(RESTART_TIMEOUT, app_server.read_response(loaded_id)).await??;
    assert!(loaded.data.contains(&thread_id.to_string()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_checkin_unloads_unsubscribed_recovered_root() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("supervisor-complete"),
                responses::ev_function_call_with_namespace(
                    "supervisor-complete-call",
                    "saffron",
                    "supervisor_close_self",
                    &json!({}).to_string(),
                ),
                responses::ev_completed("supervisor-complete"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-finished"),
                responses::ev_assistant_message("supervisor-message", "Goal complete."),
                responses::ev_completed("supervisor-finished"),
            ]),
        ],
    )
    .await;
    let (codex_home, thread_id) = restart_goal_fixture(&server, "finish after restart").await?;
    let mut app_server = start_restart_app(&codex_home).await?;

    timeout(RESTART_TIMEOUT, async {
        while model_responses.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("recovered supervisor did not complete the goal")?;
    app_server.initialize().await?;

    timeout(RESTART_TIMEOUT, async {
        loop {
            let request_id = app_server
                .send_thread_loaded_list_request(ThreadLoadedListParams::default())
                .await
                .expect("send loaded-thread request");
            let loaded: ThreadLoadedListResponse = app_server
                .read_response(request_id)
                .await
                .expect("read loaded-thread response");
            if !loaded.data.contains(&thread_id.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("completed recovered root remained loaded")?;
    Ok(())
}

async fn restart_goal_fixture(server: &MockServer, objective: &str) -> Result<(TempDir, ThreadId)> {
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let thread_id = seed_active_goal(codex_home.path(), objective).await?;
    Ok((codex_home, thread_id))
}

async fn start_restart_app(codex_home: &TempDir) -> Result<TestAppServer> {
    TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build()
        .await
}

async fn seed_active_goal(codex_home: &Path, objective: &str) -> Result<ThreadId> {
    let thread_id = create_fake_rollout(
        codex_home,
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "restart recovery fixture",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let mut setup_server = TestAppServer::builder()
        .with_codex_home(codex_home)
        .without_managed_config()
        .build_initialized()
        .await?;
    setup_server.shutdown_gracefully().await?;

    let thread_id = ThreadId::from_string(&thread_id)?;
    test_state_db(codex_home)
        .await?
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            objective,
            codex_state::ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await?;
    Ok(thread_id)
}

async fn test_state_db(codex_home: &Path) -> Result<std::sync::Arc<StateRuntime>> {
    StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(codex_home.abs()),
        "mock_provider".into(),
    )
    .await
}

async fn wait_for_supervisor_requests(server: &MockServer, expected: usize) -> Result<()> {
    timeout(RESTART_TIMEOUT, async {
        while supervisor_request_bodies(server).await.len() < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("restart recovery did not launch a supervisor")
}

async fn supervisor_request_bodies(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .filter(|body| body.contains("# Supervisor Check-in"))
        .collect()
}
