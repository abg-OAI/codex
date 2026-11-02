//! Attributed message delivery between independent roots on the same host.
//!
//! The host installs a weak ThreadManager capability; this module owns the
//! model-facing policy and sender attribution. Core owns atomic start-or-steer
//! admission. The optional host capability owns saved-thread restoration and
//! local process routing. The receipt confirms acceptance, not completion.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;

use crate::ThreadManager;
use crate::TurnInputRequest;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;

/// Installs messaging for roots owned by this app-server's thread registry.
pub fn install(
    builder: &mut ExtensionRegistryBuilder<Config>,
    manager: Weak<ThreadManager>,
    delivery: Option<Arc<ThreadMessageHostHandle>>,
) {
    builder.thread_lifecycle_contributor(Arc::new(MessageHost { manager, delivery }));
}

/// Completes delivery outside the caller's loaded thread registry.
///
/// Implementations preserve destination settings and reject non-root targets.
/// They own loading, listener lifetime and admission even if the caller stops
/// waiting. Errors must distinguish unconfirmed delivery from rejection, and
/// must never trigger an automatic resend after a possible submission.
pub trait ThreadMessageHost: Send + Sync {
    /// Submits one attributed item without granting it human authority.
    fn send(
        &self,
        thread_id: ThreadId,
        message: ResponseItem,
    ) -> ExtensionFuture<'_, Result<MessageSubmission, String>>;
}

/// Breaks the manager-to-host ownership cycle during process construction.
#[derive(Default)]
pub struct ThreadMessageHostHandle {
    /// Installed once after the app-server has constructed its processors.
    host: OnceLock<Weak<dyn ThreadMessageHost>>,
}

impl ThreadMessageHostHandle {
    /// Installs the process-owned implementation without retaining its manager.
    ///
    /// # Panics
    /// Panics if process construction installs the capability more than once.
    pub fn set(&self, host: Weak<dyn ThreadMessageHost>) {
        assert!(
            self.host.set(host).is_ok(),
            "message host is installed once"
        );
    }
}

/// Acceptance of a message, without a claim that the target completed work.
pub struct MessageSubmission {
    /// Turn that accepted the message.
    pub turn_id: String,
    /// Admission detail available from the receiving runtime.
    pub status: Delivery,
}

/// Registers the tool only for roots with a host-installed capability.
pub(super) fn register(session: &Session, turn: &TurnContext, registry: &mut ToolRegistry) {
    if !turn.session_source.is_non_root_agent()
        && let Some(host) = session.services.thread_extension_data.get::<MessageHost>()
    {
        registry.add(Handler { host });
    }
}

/// Process registry access without a thread-to-manager ownership cycle.
struct MessageHost {
    /// Resolves destinations already loaded by this process.
    manager: Weak<ThreadManager>,
    /// App-server-owned routing, absent in standalone Core fixtures.
    delivery: Option<Arc<ThreadMessageHostHandle>>,
}

impl ThreadLifecycleContributor<Config> for MessageHost {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !input.session_source.is_non_root_agent() {
                input.thread_store.insert(MessageHost {
                    manager: self.manager.clone(),
                    delivery: self.delivery.clone(),
                });
            }
        })
    }
}

/// Validates a delivery and submits it through the destination's turn API.
struct Handler {
    /// Host capability captured when the caller started.
    host: Arc<MessageHost>,
}

/// Destination and message selected by the model; identity comes from runtime.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Destination root in the same host and Codex home.
    thread_id: String,
    /// Message content, delivered as agent output rather than user instructions.
    prompt: String,
}

/// Runtime attribution saved with the message so transcript replay needs no lookup.
#[derive(Serialize)]
struct Message {
    /// Sender identity supplied by the session, never by tool arguments.
    source_thread_id: ThreadId,
    /// Best-effort display name at send time; absence does not prevent delivery.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_thread_name: Option<String>,
    /// Agent-provided body retaining its original authority as tool output.
    input: String,
}

/// Confirmation of accepted input, not of completed processing.
#[derive(Serialize)]
struct Receipt {
    /// Destination that accepted the input.
    thread_id: ThreadId,
    /// Runtime-derived sender identity, also included in delivered content.
    source_thread_id: ThreadId,
    /// Newly started or already active destination turn.
    turn_id: String,
    /// Admission detail reported by the destination runtime.
    status: Delivery,
}

/// Distinguishes starting new work from steering work already in progress.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// A new destination turn accepted the input.
    Started,
    /// The active destination turn accepted steering input.
    Steered,
    /// A local peer accepted input but its protocol omits the admission mode.
    Submitted,
}

impl Handler {
    /// Rejects unsupported destinations before submitting any input.
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(invalid(
                "send_message_to_thread requires function arguments",
            ));
        };
        let args: Args = parse_arguments(arguments)?;
        let thread_id = ThreadId::from_string(&args.thread_id).map_err(invalid)?;
        if args.prompt.trim().is_empty() {
            return Err(invalid("prompt must not be empty"));
        }
        if invocation
            .session
            .session_source()
            .await
            .is_non_root_agent()
        {
            return Err(invalid(
                "only root threads may send independent thread messages",
            ));
        }
        let manager = self
            .host
            .manager
            .upgrade()
            .ok_or_else(|| invalid("messaging host is shutting down"))?;
        let source_thread_id = invocation.session.thread_id();
        let source_thread_name = if let Ok(source) = manager.get_thread(source_thread_id).await {
            source
                .read_thread(true, false)
                .await
                .ok()
                .and_then(|thread| thread.name)
        } else {
            None
        };
        let message = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: None,
            name: Some("send_message_to_thread".to_string()),
            namespace: Some("saffron".to_string()),
            output: FunctionCallOutputPayload::from_text(
                serde_json::to_string(&Message {
                    source_thread_id,
                    source_thread_name,
                    input: args.prompt,
                })
                .map_err(invalid)?,
            ),
            internal_chat_message_metadata_passthrough: None,
        };
        let (status, turn_id) = if let Ok(destination) = manager.get_thread(thread_id).await {
            if destination.session_source.is_non_root_agent() {
                return Err(invalid(
                    "destination must be a root thread; use collaboration tools for subagents",
                ));
            }
            let submitted = destination
                .start_or_steer_turn(
                    TurnInputRequest::new(TurnInput::ResponseItem(message)).on_start(
                        TurnStartOptions {
                            turn_trigger: Some("saffron_thread_message".to_string()),
                            ..Default::default()
                        },
                    ),
                )
                .await
                .map_err(invalid)?;
            match submitted {
                TurnInputSubmission::Started { turn_id, .. } => (Delivery::Started, turn_id),
                TurnInputSubmission::Steered { turn_id, .. } => (Delivery::Steered, turn_id),
                TurnInputSubmission::NotSubmitted { reason } => {
                    return Err(invalid(format!("message was not submitted: {reason:?}")));
                }
            }
        } else {
            let host = self
                .host
                .delivery
                .as_ref()
                .and_then(|handle| handle.host.get())
                .and_then(Weak::upgrade)
                .ok_or_else(|| {
                    invalid("destination is not loaded and same-host messaging is unavailable")
                })?;
            let submitted = host.send(thread_id, message).await.map_err(invalid)?;
            (submitted.status, submitted.turn_id)
        };
        Ok(boxed_tool_output(JsonToolOutput::new(
            serde_json::to_value(Receipt {
                thread_id,
                source_thread_id,
                turn_id,
                status,
            })
            .map_err(invalid)?,
        )))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("saffron", "send_message_to_thread")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "saffron".to_string(),
            description: "Saffron extensions for independent thread and task coordination.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "send_message_to_thread".to_string(),
                description: "Send a message to a root thread on this host in the same Codex home. Starts a turn when idle and steers an active turn without interrupting it. Preserves destination settings and includes your thread ID as the sender. Saved roots can be resumed; another process must own the target through the local control socket and use the same Codex release. Archived threads, subagents and other hosts are unsupported. Use only for user-authorized communication. The result reports submission, not completion; submitted means the peer accepted input without reporting start versus steer. Cancellation does not recall a delivery. Do not retry an uncertain delivery automatically.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(BTreeMap::from([
                    ("thread_id".to_string(), JsonSchema::string(Some("Destination root thread ID on this host in the same Codex home.".to_string()))),
                    ("prompt".to_string(), JsonSchema::string(Some("Message to deliver to the destination.".to_string()))),
                ]), Some(vec!["thread_id".to_string(), "prompt".to_string()]), Some(false.into())),
                output_schema: None,
            })],
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for Handler {}

/// Makes submission failures visible without suggesting another delivery occurred.
fn invalid(error: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

#[cfg(test)]
#[path = "thread_message/tests.rs"]
mod tests;
