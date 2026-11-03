//! Delivery to a different native app-server through its private Unix socket.

use super::*;
use anyhow::Context;
use futures::SinkExt;
use futures::StreamExt;
use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Uses real discovery and turn/start without attaching the sending process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_socket_owner_accepts_idle_delivery() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    let mut owner = Owner::start(&fixture).await?;
    owner
        .call(
            "thread/resume",
            json!({"threadId":destination,"excludeTurns":true}),
        )
        .await?;
    let received =
        responses::mount_sse_once(&fixture.destination_model, finish("received by owner")).await;
    let mut sender_host = fixture.server().await?;
    let sender = fixture.sender(&mut sender_host).await?;
    let receipt = fixture
        .send(&mut sender_host, &sender, &destination, "socket-marker")
        .await?;
    assert!(receipt.contains("submitted"), "{receipt}");
    tokio::time::timeout(Duration::from_secs(20), async {
        while received.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let request = received.single_request().body_json();
    assert_eq!(request["model"], "gpt-5.1");
    assert!(request["input"].to_string().contains("socket-marker"));
    assert!(request["input"].to_string().contains(&sender));
    let loaded = sender_host
        .send_request("thread/loaded/list", Some(json!({})))
        .await?;
    let loaded: Value = sender_host.read_response(loaded).await?;
    assert!(
        !loaded["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &destination)
    );
    sender_host.shutdown_gracefully().await?;
    owner.stop().await?;
    Ok(())
}

/// A loaded peer receives steering without replacing its active turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_socket_owner_steers_busy_turn() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut first = fixture.server().await?;
    let destination = fixture
        .destination(&mut first, ThreadHistoryMode::Legacy)
        .await?;
    first.shutdown_gracefully().await?;
    let mut owner = Owner::start(&fixture).await?;
    owner
        .call(
            "thread/resume",
            json!({"threadId":destination,"excludeTurns":true}),
        )
        .await?;
    let mut sender_host = fixture.server().await?;
    let sender = fixture.sender(&mut sender_host).await?;
    fixture.destination_model.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(finish("working"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&fixture.destination_model)
        .await;
    let active = owner
        .call(
            "turn/start",
            serde_json::to_value(input(&destination, "busy fixture"))?,
        )
        .await?;
    let receipt = fixture
        .send(
            &mut sender_host,
            &sender,
            &destination,
            "busy-socket-marker",
        )
        .await?;
    let receipt: Value = serde_json::from_str(&receipt)?;
    let receipt: Value = serde_json::from_str(receipt["output"].as_str().unwrap())?;
    assert_eq!(receipt["status"], "submitted");
    assert_eq!(receipt["turn_id"], active["turn"]["id"]);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if fixture
                .destination_model
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| {
                    request
                        .body_json::<Value>()
                        .is_ok_and(|body| body["input"].to_string().contains("busy-socket-marker"))
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    sender_host.shutdown_gracefully().await?;
    owner.stop().await?;
    Ok(())
}

/// Native daemon and a retained observing client for fixture-owned threads.
struct Owner {
    /// Terminated on drop if a test fails before explicit cleanup.
    process: Child,
    /// Observer keeps the loaded target from idle unloading during the test.
    socket: WebSocketStream<codex_uds::UnixStream>,
    /// Client request sequence, independent of server request IDs.
    next_id: u64,
}

impl Owner {
    /// Starts the debug binary using the standard test fixture's private store.
    async fn start(fixture: &Fixture) -> Result<Self> {
        let process = Command::new(codex_utils_cargo_bin::cargo_bin("codex-app-server")?)
            .env("CODEX_HOME", fixture.home.path())
            .args([
                "--disable-plugin-startup-tasks-for-tests",
                "--listen",
                "unix://",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let path = codex_app_server_transport::app_server_control_socket_path(fixture.home.path())?;
        let stream = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Ok(stream) = codex_uds::UnixStream::connect(path.as_path()).await {
                    break stream;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let (socket, _) = tokio_tungstenite::client_async("ws://localhost/rpc", stream).await?;
        let mut owner = Self {
            process,
            socket,
            next_id: 1,
        };
        owner.call("initialize", json!({"clientInfo":{"name":"saffron_message_test","version":"1"},"capabilities":{"experimentalApi":true}})).await?;
        owner
            .socket
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await?;
        Ok(owner)
    }

    /// Sends fixture operations while leaving server requests unanswered.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = self.socket.next().await.context("owner disconnected")??;
                if let Message::Text(text) = frame {
                    let response: Value = serde_json::from_str(&text)?;
                    if response.get("method").is_none() && response["id"] == id {
                        anyhow::ensure!(response.get("error").is_none(), "{response}");
                        return Ok(response["result"].clone());
                    }
                }
            }
        })
        .await?
    }

    /// Stops only this disposable owner and waits for process cleanup.
    async fn stop(mut self) -> Result<()> {
        let pid = self.process.id().context("fixture owner already exited")?;
        let signaled = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await?;
        anyhow::ensure!(signaled.success(), "could not stop fixture owner");
        tokio::time::timeout(Duration::from_secs(20), self.process.wait()).await??;
        Ok(())
    }
}
