//! Model-facing choice for delivering a successful subagent result.
//!
//! Registration limits the tool to spawned collaboration subagents. The tool
//! writes only to the current turn's extension store; terminal routing later
//! consumes that state while preserving wake-on-failure behavior.

use std::collections::BTreeMap;

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

use super::subagent_completion;
use super::subagent_completion::CompletionDelivery;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;

/// Namespace shared by Saffrodex-owned model tools.
const NAMESPACE: &str = "saffron";
/// Tool name within the Saffron namespace.
const TOOL_NAME: &str = "set_completion_delivery";

/// Registers the completion-delivery choice for a spawned subagent turn.
pub(super) fn register(registry: &mut ToolRegistry) {
    registry.add(Handler);
}

/// Stores one model-selected delivery choice in the current turn.
struct Handler;

impl Handler {
    /// Applies a valid subagent choice without changing result contents.
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation { turn, payload, .. } = invocation;
        if !subagent_completion::can_choose_delivery(turn.multi_agent_version, &turn.session_source)
        {
            return Err(FunctionCallError::RespondToModel(
                "only a spawned subagent may choose completion delivery".to_string(),
            ));
        }
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "completion delivery received an unsupported payload".to_string(),
            ));
        };
        let args: SetCompletionDeliveryArgs = parse_arguments(&arguments)?;
        let delivery = CompletionDelivery::from(args.delivery);
        subagent_completion::set_delivery(&turn.extension_data, delivery);

        Ok(boxed_tool_output(JsonToolOutput::new(json!({
            "delivery": args.delivery,
        }))))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: NAMESPACE.to_string(),
            description: "Saffron extensions for coordinating long-running work.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: TOOL_NAME.to_string(),
                description: "Choose whether your successful terminal result should start a new turn for an idle parent. The result is delivered in either mode. Use wake_parent when the result enables or requires useful action now, changes the parent's next step, unblocks dependent work, needs evaluation, or is time-sensitive. Use defer_to_parent when immediate parent attention adds little value, including unchanged monitoring results, routine status, low-urgency findings, or information that can be consumed alongside other work. Failures and abnormal termination wake the parent automatically. If this tool is not called, completion defaults to wake_parent. Ask whether starting the parent now would improve what happens next."
                    .to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([(
                        "delivery".to_string(),
                        JsonSchema::string_enum(
                            vec![
                                json!(CompletionDeliveryArg::WakeParent.as_str()),
                                json!(CompletionDeliveryArg::DeferToParent.as_str()),
                            ],
                            Some(
                                "wake_parent starts an idle parent turn; defer_to_parent queues the result for the parent's next natural turn."
                                    .to_string(),
                            ),
                        ),
                    )]),
                    Some(vec!["delivery".to_string()]),
                    Some(false.into()),
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

/// Arguments accepted by `saffron.set_completion_delivery`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetCompletionDeliveryArgs {
    /// Delivery behavior for a successful terminal result.
    delivery: CompletionDeliveryArg,
}

/// Model-facing spellings for the two successful-result dispositions.
#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CompletionDeliveryArg {
    /// Starts an idle parent turn after delivering the result.
    WakeParent,
    /// Leaves the delivered result queued for a later parent turn.
    DeferToParent,
}

impl CompletionDeliveryArg {
    /// Returns the JSON spelling advertised by the tool schema.
    const fn as_str(self) -> &'static str {
        match self {
            Self::WakeParent => "wake_parent",
            Self::DeferToParent => "defer_to_parent",
        }
    }
}

impl From<CompletionDeliveryArg> for CompletionDelivery {
    fn from(value: CompletionDeliveryArg) -> Self {
        match value {
            CompletionDeliveryArg::WakeParent => Self::WakeParent,
            CompletionDeliveryArg::DeferToParent => Self::DeferToParent,
        }
    }
}

#[cfg(test)]
#[path = "subagent_completion_tool_tests.rs"]
mod tests;
