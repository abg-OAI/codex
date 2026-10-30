//! Native-tool checks against disposable debug app-server processes.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use core_test_support::responses;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::MockServer;

mod lifecycle;
#[cfg(unix)]
mod socket;

/// Saved roots retain their provider, cwd and attributed input across a restart.
#[test_case::test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case::test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_root_is_resumed_with_its_settings(mode: ThreadHistoryMode) -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture.destination(&mut first, mode).await?;
    first.shutdown_gracefully().await?;
    let mut second = fixture.server().await?;
    let requests = responses::mount_sse_once(&fixture.destination_model, finish("received")).await;
    let sender = fixture.sender(&mut second).await?;
    let reply = fixture
        .send(&mut second, &sender, &destination, "cold-message")
        .await?;
    assert!(reply.contains("started"), "{reply}");
    let request = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let requests = requests.requests();
            if let Some(request) = requests.first() {
                break request.body_json();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(request["model"], "gpt-5.1");
    let delivered = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "send_message_to_thread")
        .expect("attributed delivery");
    assert_eq!(delivered["type"], "function_call_output");
    assert_eq!(delivered["namespace"], "saffron");
    let body: Value = serde_json::from_str(delivered["output"].as_str().unwrap())?;
    assert_eq!(body["source_thread_id"], sender);
    assert_eq!(body["input"], "cold-message");
    let read = second
        .send_thread_read_request(ThreadReadParams {
            thread_id: destination,
            include_turns: false,
        })
        .await?;
    let read: ThreadReadResponse = second.read_response(read).await?;
    assert_eq!(read.thread.model_provider, "destination");
    assert_eq!(read.thread.cwd.as_path(), fixture.cwd.path());
    second.shutdown_gracefully().await?;
    Ok(())
}

/// Native writer ownership prevents creating a competing runtime for a stdio owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_owner_is_not_replaced() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut owner = fixture.server().await?;
    let destination = fixture
        .destination(&mut owner, ThreadHistoryMode::Legacy)
        .await?;
    let mut sender_host = fixture.server().await?;
    let sender = fixture.sender(&mut sender_host).await?;
    let result = fixture
        .send(&mut sender_host, &sender, &destination, "must-not-deliver")
        .await?;
    assert!(!result.contains("\"status\":\"started\""), "{result}");
    assert!(
        result.contains("already") || result.contains("writer") || result.contains("locked"),
        "{result}"
    );
    assert_eq!(
        fixture
            .destination_model
            .received_requests()
            .await
            .unwrap()
            .len(),
        1
    );
    sender_host.shutdown_gracefully().await?;
    owner.shutdown_gracefully().await?;
    Ok(())
}

/// Archival remains a rejection, rather than an implicit unarchive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_root_is_not_resumed() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut host = fixture.server().await?;
    let destination = fixture
        .destination(&mut host, ThreadHistoryMode::Legacy)
        .await?;
    let id = host
        .send_request("thread/archive", Some(json!({"threadId":destination})))
        .await?;
    let _: Value = host.read_response(id).await?;
    let sender = fixture.sender(&mut host).await?;
    let result = fixture
        .send(&mut host, &sender, &destination, "must-not-deliver")
        .await?;
    assert!(result.contains("archived"), "{result}");
    assert_eq!(
        fixture
            .destination_model
            .received_requests()
            .await
            .unwrap()
            .len(),
        1
    );
    host.shutdown_gracefully().await?;
    Ok(())
}

/// Fixture data and controlled providers are independent of working chats.
struct Fixture {
    /// Disposable native store for the standard app-server test harness.
    home: TempDir,
    /// Destination workspace survives its original process.
    cwd: TempDir,
    /// Supplies sender tool calls and captures receipts.
    sender_model: MockServer,
    /// Proves the destination did not inherit the sender provider.
    destination_model: MockServer,
}

impl Fixture {
    /// Configures two providers in a disposable app-server fixture.
    async fn new() -> Result<Self> {
        let home = TempDir::new()?;
        let cwd = TempDir::new()?;
        let sender_model = responses::start_mock_server().await;
        let destination_model = responses::start_mock_server().await;
        MockResponsesConfig::new(&sender_model.uri()).enable_feature(codex_features::Feature::Goals).with_root_config("thread_unload_delay_secs = 1").with_extra_config(&format!(
            "[model_providers.destination]\nname = 'Destination'\nbase_url = '{}/v1'\nwire_api = 'responses'\nrequest_max_retries = 0\nstream_max_retries = 0\n", destination_model.uri()
        )).write(home.path())?;
        Ok(Self {
            home,
            cwd,
            sender_model,
            destination_model,
        })
    }

    /// Starts a native binary with the fixture's saved state.
    async fn server(&self) -> Result<TestAppServer> {
        TestAppServer::builder()
            .with_codex_home(self.home.path())
            .without_auto_env()
            .build_initialized()
            .await
    }

    /// Persists a destination with settings different from its future sender.
    async fn destination(
        &self,
        host: &mut TestAppServer,
        mode: ThreadHistoryMode,
    ) -> Result<String> {
        responses::mount_sse_once(&self.destination_model, finish("ready")).await;
        let request = host
            .send_thread_start_request(ThreadStartParams {
                model: Some("gpt-5.1".to_string()),
                model_provider: Some("destination".to_string()),
                cwd: Some(self.cwd.path().display().to_string()),
                history_mode: Some(mode),
                environments: Some(vec![]),
                ..Default::default()
            })
            .await?;
        let response: codex_app_server_protocol::ThreadStartResponse =
            host.read_response(request).await?;
        let destination = response.thread.id;
        let completed = host
            .start_turn_and_wait_for_completion(input(&destination, "persist fixture"))
            .await?;
        assert_eq!(completed.turn.status, TurnStatus::Completed);
        Ok(destination)
    }

    /// Starts the caller without changing destination settings.
    async fn sender(&self, host: &mut TestAppServer) -> Result<String> {
        let request = host
            .send_thread_start_request(ThreadStartParams {
                environments: Some(vec![]),
                ..Default::default()
            })
            .await?;
        let response: codex_app_server_protocol::ThreadStartResponse =
            host.read_response(request).await?;
        Ok(response.thread.id)
    }

    /// Calls the actual registered tool and returns the model-visible receipt.
    async fn send(
        &self,
        host: &mut TestAppServer,
        source: &str,
        destination: &str,
        text: &str,
    ) -> Result<String> {
        responses::mount_sse_once(
            &self.sender_model,
            responses::sse(vec![
                responses::ev_response_created("send"),
                responses::ev_function_call_with_namespace(
                    "send-call",
                    "saffron",
                    "send_message_to_thread",
                    &json!({"thread_id":destination,"prompt":text}).to_string(),
                ),
                responses::ev_completed("send"),
            ]),
        )
        .await;
        let receipt = responses::mount_sse_once(&self.sender_model, finish("done")).await;
        let completed = host
            .start_turn_and_wait_for_completion(input(source, "run fixture delivery"))
            .await?;
        assert_eq!(completed.turn.status, TurnStatus::Completed);
        Ok(receipt
            .single_request()
            .function_call_output("send-call")
            .to_string())
    }
}

/// A regular user turn used only to drive the controlled fixture.
fn input(thread: &str, text: &str) -> TurnStartParams {
    TurnStartParams {
        thread_id: thread.to_string(),
        input: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: vec![],
        }],
        ..Default::default()
    }
}

/// Finishes one controlled model request.
fn finish(text: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created("response"),
        responses::ev_assistant_message("answer", text),
        responses::ev_completed("response"),
    ])
}
