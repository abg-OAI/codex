//! Message delivery between loaded root threads in one app-server process.
//!
//! The host installs a weak ThreadManager capability; this module owns the
//! model-facing policy and sender attribution. Core owns atomic start-or-steer
//! admission. Delivery never loads a saved thread, changes destination settings,
//! or reaches another process. The receipt confirms acceptance, not completion.

use std::collections::BTreeMap;
use std::sync::Arc;
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
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>, manager: Weak<ThreadManager>) {
    builder.thread_lifecycle_contributor(Arc::new(MessageHost { manager }));
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
    /// Only this registry may resolve the destination.
    manager: Weak<ThreadManager>,
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
    /// Loaded destination in the caller's app-server process.
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
    /// How Core accepted this submission.
    status: Delivery,
}

/// Distinguishes starting new work from steering work already in progress.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Delivery {
    /// A new destination turn accepted the input.
    Started,
    /// The active destination turn accepted steering input.
    Steered,
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
        let destination = manager.get_thread(thread_id).await.map_err(|error| {
            invalid(format!(
                "destination must be loaded in this app-server process; resume it in the host first: {error}"
            ))
        })?;
        if destination.session_source.is_non_root_agent() {
            return Err(invalid(
                "destination must be a root thread; use collaboration tools for subagents",
            ));
        }
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
        let (status, turn_id) = match submitted {
            TurnInputSubmission::Started { turn_id } => (Delivery::Started, turn_id),
            TurnInputSubmission::Steered { turn_id } => (Delivery::Steered, turn_id),
            TurnInputSubmission::NotSubmitted { reason } => {
                return Err(invalid(format!("message was not submitted: {reason:?}")));
            }
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
                description: "Send a message to a root thread already loaded in this app-server process. Starts a turn when idle and steers an active turn without interrupting it. Preserves destination settings and includes your thread ID as the sender. Use only for user-authorized communication. The result reports submission, not completion. Unloaded threads must first be resumed by the host; other processes and subagents are not supported. Do not retry an uncertain delivery automatically.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(BTreeMap::from([
                    ("thread_id".to_string(), JsonSchema::string(Some("Destination root thread ID in this app-server process.".to_string()))),
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
