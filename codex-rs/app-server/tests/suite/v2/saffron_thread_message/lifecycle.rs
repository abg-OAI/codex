//! Cold runtime admission and ownership boundaries through the native tool.

use super::*;
use std::io::Write;

/// Saved child history does not acquire root authority by being unloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_subagent_is_not_resumed() -> Result<()> {
    let fixture = Fixture::new().await?;
    let child = app_test_support::create_fake_rollout_with_source(
        fixture.home.path(),
        "2026-01-01T00-00-00",
        "2026-01-01T00:00:00Z",
        "synthetic child",
        Some("mock_provider"),
        None,
        codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Other("fixture-helper".to_string()),
        ),
    )?;
    let mut host = fixture.server().await?;
    let sender = fixture.sender(&mut host).await?;
    let result = fixture
        .send(&mut host, &sender, &child, "must-not-deliver")
        .await?;
    assert!(result.contains("root thread"), "{result}");
    let id = host
        .send_request("thread/loaded/list", Some(json!({})))
        .await?;
    let loaded: Value = host.read_response(id).await?;
    assert!(
        !loaded["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &child)
    );
    host.shutdown_gracefully().await?;
    Ok(())
}

/// The message, rather than an idle-triggered goal wake, starts a cold root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_goal_cold_resume_admits_message_before_continuation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    // Isolate message-triggered restoration from the separate process scheduler.
    // The target's native goal extension remains enabled and active.
    std::fs::create_dir_all(fixture.home.path().join(".tmp"))?;
    let scheduler = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.home.path().join(".tmp/saffron-goal-scheduler.lock"))?;
    scheduler.try_lock()?;
    let mut host = fixture.server().await?;
    let goal = host
        .send_request(
            "thread/goal/set",
            Some(json!({
                "threadId":destination,"objective":"finish the synthetic fixture","status":"active"
            })),
        )
        .await?;
    let goal: Value = host.read_response(goal).await?;
    assert_eq!(goal["goal"]["status"], "active");
    let first_request = responses::mount_sse_once(
        &fixture.destination_model,
        responses::sse(vec![
            responses::ev_response_created("finish-goal"),
            responses::ev_function_call(
                "finish-goal-call",
                "update_goal",
                r#"{"status":"complete"}"#,
            ),
            responses::ev_completed("finish-goal"),
        ]),
    )
    .await;
    responses::mount_sse_once(&fixture.destination_model, finish("goal finished")).await;
    let sender = fixture.sender(&mut host).await?;
    let receipt = fixture
        .send(&mut host, &sender, &destination, "goal-message-marker")
        .await?;
    assert!(receipt.contains("started"), "{receipt}");
    tokio::time::timeout(Duration::from_secs(20), async {
        while first_request.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(
        first_request.single_request().body_json()["input"]
            .to_string()
            .contains("goal-message-marker")
    );
    host.shutdown_gracefully().await?;
    Ok(())
}

/// Concurrent misses share native resume serialization and deliver both inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_cold_sends_share_one_runtime() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    fixture.destination_model.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(finish("received")),
        )
        .mount(&fixture.destination_model)
        .await;
    let destination_for_reply = destination.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let output = if body["input"].as_array().unwrap().iter().any(|item| {
                item["call_id"] == "parallel-call" && item["type"] == "function_call_output"
            }) {
                finish("done")
            } else {
                let marker = if body["input"].to_string().contains("first-caller") {
                    "parallel-one"
                } else {
                    "parallel-two"
                };
                responses::sse(vec![
                    responses::ev_response_created("parallel"),
                    responses::ev_function_call_with_namespace(
                        "parallel-call",
                        "saffron",
                        "send_message_to_thread",
                        &json!({"thread_id":destination_for_reply,"prompt":marker}).to_string(),
                    ),
                    responses::ev_completed("parallel"),
                ])
            };
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(output)
        })
        .mount(&fixture.sender_model)
        .await;
    let mut host = fixture.server().await?;
    let one = fixture.sender(&mut host).await?;
    let two = fixture.sender(&mut host).await?;
    let one_request = host
        .send_turn_start_request(input(&one, "first-caller"))
        .await?;
    let two_request = host
        .send_turn_start_request(input(&two, "second-caller"))
        .await?;
    let _: Value = host.read_response(one_request).await?;
    let _: Value = host.read_response(two_request).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let requests = fixture.sender_model.received_requests().await.unwrap();
            let receipts = requests
                .iter()
                .filter_map(|request| request.body_json::<Value>().ok())
                .flat_map(|body| body["input"].as_array().cloned().unwrap_or_default())
                .filter(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == "parallel-call"
                })
                .collect::<Vec<_>>();
            if receipts.len() == 2 {
                for receipt in receipts {
                    let receipt: Value =
                        serde_json::from_str(receipt["output"].as_str().unwrap()).unwrap();
                    assert_eq!(receipt["thread_id"], destination);
                    assert!(matches!(
                        receipt["status"].as_str(),
                        Some("started" | "steered")
                    ));
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let input = fixture
                .destination_model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|request| String::from_utf8_lossy(&request.body).into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            if input.contains("parallel-one") && input.contains("parallel-two") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    host.shutdown_gracefully().await?;
    Ok(())
}

/// Interrupting the caller does not strand a target admitted without subscribers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sender_interruption_keeps_delivery_and_releases_idle_writer() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    fixture.destination_model.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(finish("received after sender stops"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&fixture.destination_model)
        .await;
    responses::mount_sse_once(
        &fixture.sender_model,
        responses::sse(vec![
            responses::ev_response_created("cancel-send"),
            responses::ev_function_call_with_namespace(
                "cancel-call",
                "saffron",
                "send_message_to_thread",
                &json!({"thread_id":destination,"prompt":"survive-cancellation"}).to_string(),
            ),
            responses::ev_completed("cancel-send"),
        ]),
    )
    .await;
    responses::mount_response_once(
        &fixture.sender_model,
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(finish("sender would finish later"))
            .set_delay(Duration::from_secs(30)),
    )
    .await;
    let mut host = fixture.server().await?;
    let sender = fixture.sender(&mut host).await?;
    let started = host
        .send_turn_start_request(input(&sender, "send then await interruption"))
        .await?;
    let started: Value = host.read_response(started).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        while fixture
            .destination_model
            .received_requests()
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let interrupted = host
        .send_request(
            "turn/interrupt",
            Some(json!({"threadId":sender,"turnId":started["turn"]["id"]})),
        )
        .await?;
    let _: Value = host.read_response(interrupted).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let id = host
                .send_request("thread/loaded/list", Some(json!({})))
                .await?;
            let loaded: Value = host.read_response(id).await?;
            if !loaded["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == &destination)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    let mut observer = fixture.server().await?;
    let id = observer
        .send_request("thread/resume", Some(json!({"threadId":destination})))
        .await?;
    let resumed: Value = observer.read_response(id).await?;
    assert!(
        resumed.to_string().contains("survive-cancellation"),
        "{resumed}"
    );
    assert!(
        resumed.to_string().contains("received after sender stops"),
        "{resumed}"
    );
    observer.shutdown_gracefully().await?;
    host.shutdown_gracefully().await?;
    Ok(())
}

/// A cancelled tool waiter cannot abandon a runtime halfway through startup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interruption_during_cold_startup_still_settles_delivery() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    let mut host = fixture.server().await?;
    let sender = fixture.sender(&mut host).await?;
    let barrier = fixture.home.path().join("allow-message-startup");
    let pid_file = fixture.home.path().join("message-mcp.pid");
    // Existing caller configuration has no MCP server. Only the subsequent
    // cold resume loads this new project-independent fixture requirement.
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.home.path().join("config.toml"))?;
    writeln!(
        config,
        "\n[mcp_servers.message_barrier]\ncommand = {}\nrequired = true\nstartup_timeout_sec = 30\n[mcp_servers.message_barrier.env]\nMCP_TEST_INITIALIZE_BARRIER_FILE = {}\nMCP_TEST_PID_FILE = {}",
        toml::Value::String(core_test_support::stdio_server_bin()?),
        toml::Value::String(barrier.display().to_string()),
        toml::Value::String(pid_file.display().to_string())
    )?;
    let received = responses::mount_sse_once(
        &fixture.destination_model,
        finish("settled after cancellation"),
    )
    .await;
    responses::mount_sse_once(
        &fixture.sender_model,
        responses::sse(vec![
            responses::ev_response_created("pending-send"),
            responses::ev_function_call_with_namespace(
                "pending-call",
                "saffron",
                "send_message_to_thread",
                &json!({"thread_id":destination,"prompt":"pending-startup-marker"}).to_string(),
            ),
            responses::ev_completed("pending-send"),
        ]),
    )
    .await;
    let started = host
        .send_turn_start_request(input(&sender, "send through startup barrier"))
        .await?;
    let started: Value = host.read_response(started).await?;
    core_test_support::fs_wait::wait_for_path_exists(&pid_file, Duration::from_secs(15)).await?;
    assert!(
        fixture
            .sender_model
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.method == "POST"),
        "caller sampled before target startup blocked"
    );
    assert!(
        received.requests().is_empty(),
        "destination has not begun sampling"
    );
    let interrupted = host
        .send_request(
            "turn/interrupt",
            Some(json!({"threadId":sender,"turnId":started["turn"]["id"]})),
        )
        .await?;
    let cancelled = tokio::time::timeout(
        Duration::from_secs(5),
        host.read_response::<Value>(interrupted),
    )
    .await;
    std::fs::write(&barrier, "continue")?;
    cancelled??;
    tokio::time::timeout(Duration::from_secs(20), async {
        while received.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(
        received.single_request().body_json()["input"]
            .to_string()
            .contains("pending-startup-marker")
    );
    host.shutdown_gracefully().await?;
    Ok(())
}
