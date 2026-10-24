//! Socket-boundary checks that retain the real framing and RPC decoder.

use super::*;
use codex_protocol::models::FunctionCallOutputPayload;
use tokio_tungstenite::accept_async;

/// Peer outcomes whose distinction affects delivery and retry safety.
#[derive(Clone, Copy)]
enum Scenario {
    /// Loaded root accepts a turn.
    Accepted,
    /// A server request shares the client's current identifier.
    InterleavedRequest,
    /// No loaded owner exists here.
    Unloaded,
    /// The discovered target is parent-owned.
    Child,
    /// The server uses another store.
    DifferentHome,
    /// An older native release may ignore standalone tool output.
    OlderVersion,
    /// An older peer has no ownership of the requested root.
    OlderUnloaded,
    /// Same native release with another downstream build identifier.
    OtherBuild,
    /// No native version can be established.
    MissingVersion,
    /// Input was sent but acknowledgement was lost.
    LostReceipt,
    /// Owner reports a definitive admission rejection.
    Rejected,
}

/// Runs a protocol peer and returns its calls as independent wire evidence.
async fn exercise(
    scenario: Scenario,
) -> anyhow::Result<(Result<Option<SaffronMessageSubmission>, String>, Vec<Value>)> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("peer.sock");
    let mut listener = codex_uds::UnixListener::bind(&path).await?;
    let thread_id = ThreadId::new();
    let reported_home = match scenario {
        Scenario::DifferentHome => root.path().join("another"),
        _ => root.path().to_path_buf(),
    };
    let peer = tokio::spawn(async move {
        let mut socket = accept_async(listener.accept().await?).await?;
        let mut calls = Vec::new();
        while let Some(frame) = socket.next().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let Message::Text(text) = frame else {
                continue;
            };
            let request: Value = serde_json::from_str(&text)?;
            calls.push(request.clone());
            let method = request["method"].as_str().unwrap_or("");
            if matches!(scenario, Scenario::InterleavedRequest) && method == "initialize" {
                socket.send(Message::Text(json!({"id":request["id"],"method":"account/chatgptAuthTokens/refresh","params":{}}).to_string().into())).await?;
            }
            let result = match method {
                "initialize" => {
                    let version = match scenario {
                        Scenario::OlderVersion | Scenario::OlderUnloaded => {
                            "fixture/0.1.0".to_string()
                        }
                        Scenario::MissingVersion => "fixture".to_string(),
                        Scenario::OtherBuild => format!(
                            "fixture/{}+fixture.7 (test)",
                            env!("CARGO_PKG_VERSION").split('+').next().unwrap()
                        ),
                        _ => format!("fixture/{} (test)", env!("CARGO_PKG_VERSION")),
                    };
                    json!({"userAgent":version,"codexHome":reported_home,"platformFamily":"unix","platformOs":"linux"})
                }
                "initialized" => continue,
                "thread/loaded/list" => {
                    json!({"data": if matches!(scenario, Scenario::Unloaded | Scenario::OlderUnloaded) {vec![]} else {vec![thread_id.to_string()]},"nextCursor":null})
                }
                "thread/read" => json!({"thread":{
                    "id":thread_id,"sessionId":thread_id,"preview":"fixture","ephemeral":false,
                    "modelProvider":"fixture","createdAt":0,"updatedAt":0,"status":{"type":"idle"},
                    "cwd":reported_home,"cliVersion":"test","source":"appServer","turns":[],
                    "canAcceptDirectInput":true,"parentThreadId":if matches!(scenario,Scenario::Child) {Some(ThreadId::new().to_string())} else {None}
                }}),
                "turn/start" => {
                    if matches!(scenario, Scenario::LostReceipt) {
                        break;
                    }
                    if matches!(scenario, Scenario::Rejected) {
                        socket.send(Message::Text(json!({"id":request["id"],"error":{"code":-32000,"message":"closing"}}).to_string().into())).await?;
                        break;
                    }
                    json!({"turn":{"id":"accepted-turn","items":[],"status":"inProgress"}})
                }
                _ => anyhow::bail!("unexpected peer method: {method}"),
            };
            socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await?;
            if method == "turn/start" {
                break;
            }
        }
        anyhow::Ok(calls)
    });
    let message = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: None,
        name: Some("send_message_to_thread".to_string()),
        namespace: Some("saffron".to_string()),
        output: FunctionCallOutputPayload::from_text("attributed-message".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let result = send_if_loaded(&path, root.path(), thread_id, &message).await;
    let calls = tokio::time::timeout(Duration::from_secs(10), peer).await???;
    Ok((result, calls))
}

/// Only a single turn/start carries attributed content; no resume or subscription.
#[tokio::test]
async fn peer_acceptance_does_not_attach_or_override_settings() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::Accepted).await?;
    let submission = result
        .map_err(anyhow::Error::msg)?
        .expect("loaded root accepts input");
    assert_eq!(submission.turn_id, "accepted-turn");
    assert!(matches!(
        submission.status,
        SaffronMessageDelivery::Submitted
    ));
    assert_eq!(
        calls
            .iter()
            .filter_map(|call| call["method"].as_str())
            .collect::<Vec<_>>(),
        [
            "initialize",
            "initialized",
            "thread/loaded/list",
            "thread/read",
            "turn/start"
        ]
    );
    let params = &calls.last().unwrap()["params"];
    assert_eq!(params.as_object().unwrap().len(), 3);
    assert_eq!(params["toolOutput"]["output"], "attributed-message");
    assert_eq!(params["toolOutput"]["namespace"], "saffron");
    assert_eq!(params["input"], json!([]));
    Ok(())
}

/// An acknowledgement loss is not eligible for cold-resume fallback.
#[tokio::test]
async fn lost_receipt_is_uncertain_and_is_not_retried() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::LostReceipt).await?;
    assert!(result.err().unwrap().contains("delivery is unconfirmed"));
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["method"] == "turn/start")
            .count(),
        1
    );
    Ok(())
}

/// Independent server request IDs cannot be mistaken for RPC acknowledgements.
#[tokio::test]
async fn interleaved_server_request_is_not_answered_or_treated_as_response() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::InterleavedRequest).await?;
    assert!(result.map_err(anyhow::Error::msg)?.is_some());
    assert!(calls.iter().all(|call| call.get("method").is_some()));
    Ok(())
}

/// Definitive rejections do not become accepted receipts or another attempt.
#[tokio::test]
async fn peer_rejection_is_reported() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::Rejected).await?;
    assert!(
        result
            .err()
            .unwrap()
            .contains("peer rejected message: closing")
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["method"] == "turn/start")
            .count(),
        1
    );
    Ok(())
}

/// Missing ownership permits local native resume, without mutating the peer.
#[tokio::test]
async fn unloaded_peer_is_not_resumed() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::Unloaded).await?;
    assert!(result.map_err(anyhow::Error::msg)?.is_none());
    assert_eq!(calls.last().unwrap()["method"], "thread/loaded/list");
    Ok(())
}

/// Root-only policy applies to the other process too.
#[tokio::test]
async fn peer_child_is_rejected_before_submission() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::Child).await?;
    assert!(result.err().unwrap().contains("not an available root"));
    assert!(!calls.iter().any(|call| call["method"] == "turn/start"));
    Ok(())
}

/// A socket for another store cannot gain authority through a matching ID.
#[tokio::test]
async fn other_home_is_rejected_before_discovery() -> anyhow::Result<()> {
    let (result, calls) = exercise(Scenario::DifferentHome).await?;
    assert!(result.err().unwrap().contains("different Codex home"));
    assert_eq!(calls.len(), 1);
    Ok(())
}

/// No daemon is an ordinary cold-resume case, not a transport failure.
#[tokio::test]
async fn absent_socket_allows_local_resume() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let result = send_if_loaded(
        &root.path().join("absent.sock"),
        root.path(),
        ThreadId::new(),
        &ResponseItem::Other,
    )
    .await;
    assert!(result.map_err(anyhow::Error::msg)?.is_none());
    Ok(())
}

/// Unverified protocol support cannot produce an empty-turn false receipt.
#[test_case::test_case(Scenario::OlderVersion; "older")]
#[test_case::test_case(Scenario::MissingVersion; "missing")]
#[tokio::test]
async fn incompatible_owner_is_rejected_before_submission(
    scenario: Scenario,
) -> anyhow::Result<()> {
    let (result, calls) = exercise(scenario).await?;
    assert!(result.err().unwrap().contains("same Codex release"));
    assert_eq!(calls.last().unwrap()["method"], "thread/loaded/list");
    Ok(())
}

/// An unrelated older daemon does not prevent local cold resume.
#[tokio::test]
async fn older_peer_without_target_allows_local_resume() -> anyhow::Result<()> {
    let (result, _) = exercise(Scenario::OlderUnloaded).await?;
    assert!(result.map_err(anyhow::Error::msg)?.is_none());
    Ok(())
}

/// Native compatibility does not depend on downstream build metadata.
#[tokio::test]
async fn same_release_other_build_is_compatible() -> anyhow::Result<()> {
    let (result, _) = exercise(Scenario::OtherBuild).await?;
    assert!(result.map_err(anyhow::Error::msg)?.is_some());
    Ok(())
}
