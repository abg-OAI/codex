//! One bounded RPC connection to the current user's local control socket.
//!
//! Loaded-thread discovery does not attach a subscription or resume a runtime.
//! The peer must report the same Codex home. Once turn/start is sent, any loss of
//! its acknowledgement is uncertain delivery and must not cause local fallback.

use std::path::Path;
use std::time::Duration;

use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::TurnStartResponse;
use codex_core::SaffronMessageDelivery;
use codex_core::SaffronMessageSubmission;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_uds::UnixStream;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Probes only the configured local socket; absence permits native cold resume.
pub(super) async fn send_if_loaded(
    socket: &Path,
    codex_home: &Path,
    thread_id: ThreadId,
    message: &ResponseItem,
) -> Result<Option<SaffronMessageSubmission>, String> {
    let stream = match tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(socket))
        .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Ok(Err(error)) => return Err(format!("local control socket is unavailable: {error}")),
        Err(_) => {
            return Err("local control socket connection timed out before submission".to_string());
        }
    };
    #[cfg(windows)]
    stream
        .ensure_non_elevated_peer()
        .map_err(|error| error.to_string())?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(16 << 20))
        .max_frame_size(Some(16 << 20));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::client_async_with_config("ws://localhost/rpc", stream, Some(config)),
    )
    .await
    .map_err(|_| "local peer handshake timed out before submission".to_string())?
    .map_err(|error| format!("local peer handshake failed: {error}"))?;
    let mut peer = Peer { socket, next_id: 1 };
    let initialized: InitializeResponse = peer.request("initialize", json!({
        "clientInfo": {"name":"saffron_thread_message", "version":env!("CARGO_PKG_VERSION")},
        "capabilities": {"experimentalApi":true}
    })).await.map_err(|error| error.before_submission())?;
    if initialized.codex_home.as_path() != codex_home {
        return Err("local peer uses a different Codex home; message not submitted".to_string());
    }
    peer.socket
        .send(Message::Text(
            json!({"method":"initialized"}).to_string().into(),
        ))
        .await
        .map_err(|error| format!("local peer initialization failed: {error}"))?;
    let mut cursor = None;
    let mut found = false;
    // A bound on pages prevents an incompatible peer from trapping the caller.
    for _ in 0..100 {
        let page: ThreadLoadedListResponse = peer
            .request("thread/loaded/list", json!({"limit":100,"cursor":cursor}))
            .await
            .map_err(|error| error.before_submission())?;
        if page.data.iter().any(|id| id == &thread_id.to_string()) {
            found = true;
            break;
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if !found {
        return Ok(None);
    }
    // Discovery is read-only even on older peers. Once this peer owns the
    // target, require the same native release: experimentalApi is client opt-in,
    // and older servers can ignore toolOutput yet acknowledge an empty turn.
    // Downstream build metadata cannot change the native protocol contract.
    let peer_version = initialized
        .user_agent
        .split_once('/')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|version| version.split('+').next());
    if peer_version != env!("CARGO_PKG_VERSION").split('+').next() {
        return Err(
            "local peer must use the same Codex release; message not submitted".to_string(),
        );
    }
    let read: ThreadReadResponse = peer
        .request(
            "thread/read",
            json!({"threadId":thread_id,"includeTurns":false}),
        )
        .await
        .map_err(|error| error.before_submission())?;
    let source: codex_protocol::protocol::SessionSource = read.thread.source.into();
    if read.thread.id != thread_id.to_string()
        || source.is_non_root_agent()
        || read.thread.parent_thread_id.is_some()
        || read.thread.can_accept_direct_input != Some(true)
    {
        return Err(
            "local peer destination is not an available root thread; message not submitted"
                .to_string(),
        );
    }
    let ResponseItem::FunctionCallOutput {
        name,
        namespace,
        output,
        ..
    } = message
    else {
        return Err("message must be attributed function output".to_string());
    };
    let response: TurnStartResponse = peer
        .request(
            "turn/start",
            json!({
                "threadId":thread_id,"input":[],
                "toolOutput":{"name":name,"namespace":namespace,"output":output.body,"retain":false}
            }),
        )
        .await
        .map_err(|error| match error {
            RequestError::Rejected(message) => format!("local peer rejected message: {message}"),
            RequestError::Transport(message) => {
                format!("delivery is unconfirmed: {message}; do not resend automatically")
            }
        })?;
    Ok(Some(SaffronMessageSubmission {
        turn_id: response.turn.id,
        status: SaffronMessageDelivery::Submitted,
    }))
}

/// Sequential client with no event subscriptions or approval handlers.
struct Peer {
    /// Connection scoped to one delivery attempt.
    socket: WebSocketStream<UnixStream>,
    /// Connection-local request counter.
    next_id: u64,
}

/// Keeps definitive rejection distinct from loss of an acknowledgement.
enum RequestError {
    /// The peer returned an RPC error instead of admitting the input.
    Rejected(String),
    /// The connection failed or the response could not be interpreted.
    Transport(String),
}

impl RequestError {
    /// Preflight failures cannot have submitted the message.
    fn before_submission(self) -> String {
        let message = match self {
            Self::Rejected(message) | Self::Transport(message) => message,
        };
        format!("local peer preflight failed before submission: {message}")
    }
}

/// Minimal RPC response envelope; notifications have no matching identifier.
#[derive(Deserialize)]
struct Envelope {
    /// Server requests use an independent ID space and are not our responses.
    method: Option<String>,
    /// Numeric identifier chosen by this connection.
    id: Option<Value>,
    /// Result decoded into the method's native response type.
    result: Option<Value>,
    /// Definitive RPC rejection.
    error: Option<codex_app_server_protocol::JSONRPCErrorError>,
}

impl Peer {
    /// Performs one RPC with a deadline; never retries writes or handles requests.
    async fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T, RequestError> {
        let id = self.next_id;
        self.next_id += 1;
        tokio::time::timeout(Duration::from_secs(5), async {
            self.socket
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|error| RequestError::Transport(error.to_string()))?;
            while let Some(frame) = self.socket.next().await {
                let frame = frame.map_err(|error| RequestError::Transport(error.to_string()))?;
                let Message::Text(text) = frame else {
                    continue;
                };
                let response: Envelope = serde_json::from_str(&text)
                    .map_err(|error| RequestError::Transport(error.to_string()))?;
                if response.method.is_some() || response.id != Some(json!(id)) {
                    continue;
                }
                if let Some(error) = response.error {
                    return Err(RequestError::Rejected(error.message));
                }
                return serde_json::from_value(response.result.unwrap_or(Value::Null))
                    .map_err(|error| RequestError::Transport(error.to_string()));
            }
            Err(RequestError::Transport("connection closed".to_string()))
        })
        .await
        .map_err(|_| RequestError::Transport(format!("{method} acknowledgement timed out")))?
    }
}

#[cfg(test)]
mod tests;
