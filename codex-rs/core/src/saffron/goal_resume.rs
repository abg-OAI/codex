//! Root-only resumption of a durable goal through an extension-owned capability.
//!
//! Core owns the model-facing authority policy while the goal extension owns
//! the complete state transition and runtime lifecycle. The capability keeps
//! this boundary independent of the Saffron tool name and prevents the handler
//! from coordinating persistence, accounting, continuation, or events.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_extension_api::ExtensionFuture;
use codex_features::Feature;
use codex_protocol::protocol::ThreadGoal;
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

const NAMESPACE: &str = "saffron";
const TOOL_NAME: &str = "resume_goal";

/// Context needed to attribute a completed goal resumption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalResumeRequest {
    /// Identifier used for the resulting goal-updated event.
    pub event_id: String,
    /// Turn to which the goal-updated event belongs, when one is active.
    pub turn_id: Option<String>,
}

/// Stable failures from an extension-owned goal resumption.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GoalResumeError {
    #[error("there is no durable goal to resume")]
    GoalNotFound,
    #[error("the durable goal is already active")]
    AlreadyActive,
    #[error("the durable goal cannot resume because its token budget is exhausted")]
    TokenBudgetExhausted,
    #[error("the completed durable goal cannot be resumed; create a new goal instead")]
    Completed,
    #[error("the durable goal changed before it could be resumed")]
    Changed,
    #[error("failed to resume the durable goal: {0}")]
    Internal(String),
}

/// Performs one complete, revision-guarded goal resumption for a thread.
///
/// Implementations own persistence, runtime accounting, automatic
/// continuation, and event emission. A caller chooses only the event
/// attribution; it must not reproduce any part of the transition.
pub trait GoalResumeCapability: Send + Sync {
    fn resume<'a>(
        &'a self,
        request: GoalResumeRequest,
    ) -> ExtensionFuture<'a, Result<ThreadGoal, GoalResumeError>>;
}

/// Thread-scoped access to the installed goal-resumption capability.
#[derive(Clone)]
pub struct GoalResumeCapabilityHandle {
    inner: Arc<dyn GoalResumeCapability>,
}

impl GoalResumeCapabilityHandle {
    /// Wraps the goal extension implementation installed for one thread.
    pub fn new(capability: impl GoalResumeCapability + 'static) -> Self {
        Self {
            inner: Arc::new(capability),
        }
    }

    /// Runs the complete goal-resumption operation.
    pub async fn resume(&self, request: GoalResumeRequest) -> Result<ThreadGoal, GoalResumeError> {
        self.inner.resume(request).await
    }
}

impl std::fmt::Debug for GoalResumeCapabilityHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoalResumeCapabilityHandle")
            .finish_non_exhaustive()
    }
}

/// Registers resumption only for root threads with a usable goal runtime.
pub(super) fn register_root_if_available(
    session: &Session,
    turn_context: &TurnContext,
    registry: &mut ToolRegistry,
) {
    if turn_context.config.features.get().enabled(Feature::Goals)
        && session.services.state_db.is_some()
        && !turn_context.session_source.is_non_root_agent()
        && let Some(capability) = session
            .services
            .thread_extension_data
            .get::<GoalResumeCapabilityHandle>()
    {
        registry.add(Handler { capability });
    }
}

/// Applies root authority policy before delegating the complete transition.
struct Handler {
    capability: Arc<GoalResumeCapabilityHandle>,
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "goal resumption received an unsupported payload".to_string(),
            ));
        };
        let _: ResumeGoalArgs = parse_arguments(&arguments)?;
        if session.session_source().await.is_non_root_agent() {
            return Err(FunctionCallError::RespondToModel(
                "only a root thread may resume its durable goal".to_string(),
            ));
        }

        let goal = self
            .capability
            .resume(GoalResumeRequest {
                event_id: format!("saffron-resume-goal-{call_id}"),
                turn_id: Some(turn.sub_id.clone()),
            })
            .await
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;

        Ok(boxed_tool_output(JsonToolOutput::new(
            json!({ "goal": goal }),
        )))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: NAMESPACE.to_string(),
            description: "Saffron extensions for long-running process and goal coordination."
                .to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: TOOL_NAME.to_string(),
                description: "Resume this thread's paused, blocked, or usage-limited durable goal only when the user or system explicitly requested resumption. The goal keeps its identity, objective, budget, and accumulated usage. Never revoke a stopped state on your own."
                    .to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::new(),
                    /*required*/ None,
                    /*additional_properties*/ Some(false.into()),
                ),
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

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeGoalArgs {}
