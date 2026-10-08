//! Self-archival requests owned by a saved root's current turn.
//!
//! Core accepts the tool and checkpoints successful completion. The app-server
//! receives a runtime-bound request and owns shutdown, storage and notifications.
//! Requests are transient: interruption, steering, another turn or process exit
//! cancels them. No archival work runs inside the caller's tool or shutdown hook.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::ThreadId;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

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

/// Installs the tool only where the host can complete native archival.
pub fn install(
    builder: &mut ExtensionRegistryBuilder<Config>,
    sender: mpsc::UnboundedSender<ArchiveRequest>,
) {
    let host = Arc::new(Host { sender });
    builder.thread_lifecycle_contributor(host.clone());
    builder.turn_lifecycle_contributor(host);
}

pub(super) fn register(session: &Session, turn: &TurnContext, registry: &mut ToolRegistry) {
    if !turn.session_source.is_non_root_agent()
        && !turn.config.ephemeral
        && let Some(state) = session
            .services
            .thread_extension_data
            .get::<PendingArchive>()
    {
        registry.add(Handler { state });
    }
}

/// Nonpersistent identity of the request and the runtime that accepted it.
pub struct ArchiveRequest {
    /// Caller identity supplied by Core, never by the model's arguments.
    pub thread_id: ThreadId,
    /// Successfully persisted terminal turn that authorized this handoff.
    pub turn_id: String,
    state: Arc<PendingArchive>,
}

impl ArchiveRequest {
    /// A resumed runtime under the same thread ID must not inherit an old request.
    pub fn matches_runtime(&self, store: &ExtensionData) -> bool {
        store
            .get::<PendingArchive>()
            .is_some_and(|state| Arc::ptr_eq(&state, &self.state))
            && self.state.matches(&self.turn_id)
    }

    /// Releases suppression after rejection or failure, without cancelling a newer request.
    pub fn cancel(&self) {
        let mut pending = self
            .state
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.as_deref() == Some(&self.turn_id) {
            *pending = None;
        }
    }
}

struct Host {
    sender: mpsc::UnboundedSender<ArchiveRequest>,
}

/// Retained through the host handoff to suppress idle unload and goal continuation.
struct PendingArchive {
    turn: Mutex<Option<String>>,
    sender: mpsc::UnboundedSender<ArchiveRequest>,
}

impl PendingArchive {
    fn matches(&self, turn_id: &str) -> bool {
        self.turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref()
            == Some(turn_id)
    }
}

impl ThreadLifecycleContributor<Config> for Host {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !input.session_source.is_non_root_agent()
                && !input.config.ephemeral
                && input.persistent_thread_state_available
            {
                input.thread_store.insert(PendingArchive {
                    turn: Mutex::new(None),
                    sender: self.sender.clone(),
                });
            }
        })
    }
}

impl TurnLifecycleContributor for Host {
    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            cancel_store(input.thread_store);
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            cancel_store(input.thread_store);
        })
    }
}

pub(crate) fn pending(session: &Session) -> bool {
    session
        .services
        .thread_extension_data
        .get::<PendingArchive>()
        .is_some_and(|state| {
            state
                .turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        })
}

pub(crate) fn cancel(session: &Session) {
    cancel_store(&session.services.thread_extension_data);
}

fn cancel_store(store: &ExtensionData) {
    if let Some(state) = store.get::<PendingArchive>() {
        *state
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// A successful handoff owns idle continuation. Failure leaves normal lifecycle intact.
pub(crate) async fn finish(session: &Session, turn: &TurnContext, succeeded: bool) -> bool {
    let Some(state) = session
        .services
        .thread_extension_data
        .get::<PendingArchive>()
    else {
        return false;
    };
    if !state.matches(&turn.sub_id) {
        return false;
    }
    let request = ArchiveRequest {
        thread_id: session.thread_id(),
        turn_id: turn.sub_id.clone(),
        state,
    };
    if !succeeded
        || session.is_interrupted()
        || session.input_queue.has_trigger_turn_mailbox_items().await
    {
        request.cancel();
        return false;
    }
    match session.checkpoint_completed_turn(&turn.sub_id).await {
        Ok(true) if request.state.matches(&turn.sub_id) => {
            let sender = request.state.sender.clone();
            if let Err(error) = sender.send(request) {
                error.0.cancel();
                report_cancelled(session, turn, "the archival host stopped accepting work").await;
            } else {
                return true;
            }
        }
        Ok(_) => request.cancel(),
        Err(error) => {
            request.cancel();
            tracing::warn!(%error, "self-archival cancelled: terminal persistence failed");
            report_cancelled(session, turn, "the final turn could not be saved").await;
        }
    }
    false
}

async fn report_cancelled(session: &Session, turn: &TurnContext, reason: &str) {
    session
        .send_event(
            turn,
            codex_protocol::protocol::EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                message: format!(
                    "Self-archival cancelled: {reason}. This conversation was not archived."
                ),
            }),
        )
        .await;
}

struct Handler {
    state: Arc<PendingArchive>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {}

impl Handler {
    async fn execute(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "archive_self requires function arguments".into(),
            ));
        };
        let _: Args = parse_arguments(arguments)?;
        if invocation.turn.session_source.is_non_root_agent()
            || invocation.turn.config.ephemeral
            || invocation.cancellation_token.is_cancelled()
            || self.state.sender.is_closed()
        {
            return Err(FunctionCallError::RespondToModel(
                "self-archival requires a saved root in a running app-server".into(),
            ));
        }
        *self
            .state
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(invocation.turn.sub_id.clone());
        Ok(boxed_tool_output(JsonToolOutput::new(
            json!({"status":"scheduled"}),
        )))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("saffron", "archive_self")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "saffron".into(),
            description: "Saffron extensions for independent thread and task coordination.".into(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "archive_self".into(),
                description: "Schedule archival of this conversation after your current turn finishes successfully. Use only when the user requested archival. Finish your final response normally; scheduled does not mean archived. Steering, a newer turn, interruption, failure, or server restart cancels the request. Requires a saved root thread and a running app-server, but not Desktop. Native archival also archives spawned descendants, not independent forks. Takes no thread ID.".into(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
                output_schema: None,
            })],
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.execute(invocation))
    }
}

impl CoreToolRuntime for Handler {}

#[cfg(test)]
#[path = "archive_self/tests.rs"]
mod tests;
