//! Model-facing action tools for a Saffron goal supervisor helper.

use std::collections::BTreeMap;

use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;

use super::actions;
use super::runtime;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;

const NAMESPACE: &str = "saffron";
const MAX_SNOOZE_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Copy)]
enum Kind {
    Followup,
    Snooze,
    Compact,
    Complete,
}

pub(crate) fn register(registry: &mut ToolRegistry) {
    registry.add(Handler::new(Kind::Followup));
    registry.add(Handler::new(Kind::Snooze));
    registry.add(Handler::new(Kind::Compact));
    registry.add(Handler::new(Kind::Complete));
}

/// Executes exactly one supervisor decision against the live parent thread.
struct Handler {
    kind: Kind,
}

impl Handler {
    const fn new(kind: Kind) -> Self {
        Self { kind }
    }

    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session, payload, ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "supervisor action received an unsupported payload".to_string(),
            ));
        };
        let parent = runtime::parent_for_helper(&session)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let value = match self.kind {
            Kind::Followup => {
                let args: FollowupArgs = parse_arguments(&arguments)?;
                actions::followup(&session, &parent, args.message)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                json!({ "delivered": true })
            }
            Kind::Snooze => {
                let args: SnoozeArgs = parse_arguments(&arguments)?;
                if !(1..=MAX_SNOOZE_SECONDS).contains(&args.delay_seconds) {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "delay_seconds must be between 1 and {MAX_SNOOZE_SECONDS}"
                    )));
                }
                actions::snooze(&parent, session.thread_id, args.delay_seconds)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                json!({ "delay_seconds": args.delay_seconds })
            }
            Kind::Compact => {
                let _: CompactArgs = parse_arguments(&arguments)?;
                let submission_id = actions::compact(&parent, session.thread_id)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                json!({ "submitted": true, "submission_id": submission_id })
            }
            Kind::Complete => {
                let args: CompleteArgs = parse_arguments(&arguments)?;
                let goal = actions::complete(&parent, session.thread_id)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                if let Some(message) = args.message.filter(|message| !message.trim().is_empty())
                    && let Err(error) = actions::notify_parent(&session, &parent, &message).await
                {
                    tracing::warn!("failed to deliver supervisor completion message: {error}");
                }
                json!({ "completed": true, "goal": goal })
            }
        };
        Ok(boxed_tool_output(Output(value)))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, self.kind.name())
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: NAMESPACE.to_string(),
            description: "Saffron extensions for long-running process and goal coordination."
                .to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: self.kind.name().to_string(),
                description: self.kind.description().to_string(),
                strict: false,
                defer_loading: None,
                parameters: self.kind.parameters(),
                output_schema: None,
            })],
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Followup => "supervisor_followup_parent",
            Self::Snooze => "supervisor_snooze",
            Self::Compact => "supervisor_compact_parent_context",
            Self::Complete => "supervisor_close_self",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Followup => {
                "Wake the supervised parent with one concrete next task. Use this when the active goal can make useful progress now."
            }
            Self::Snooze => {
                "Keep the active goal idle for a bounded interval, then run another supervisor check-in. Use this only when waiting is the next useful action."
            }
            Self::Compact => {
                "Request context compaction for the idle supervised parent. Use this only when context pressure is blocking useful progress."
            }
            Self::Complete => {
                "Mark the supervised active goal complete. Use this only when inherited evidence establishes that no required work remains."
            }
        }
    }

    fn parameters(self) -> JsonSchema {
        match self {
            Self::Followup => JsonSchema::object(
                BTreeMap::from([(
                    "message".to_string(),
                    JsonSchema::string(Some(
                        "Concrete next task and any evidence the parent needs to resume."
                            .to_string(),
                    )),
                )]),
                Some(vec!["message".to_string()]),
                Some(false.into()),
            ),
            Self::Snooze => JsonSchema::object(
                BTreeMap::from([(
                    "delay_seconds".to_string(),
                    JsonSchema::integer(Some(format!(
                        "Whole seconds to wait, from 1 through {MAX_SNOOZE_SECONDS}."
                    ))),
                )]),
                Some(vec!["delay_seconds".to_string()]),
                Some(false.into()),
            ),
            Self::Compact => JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
            Self::Complete => JsonSchema::object(
                BTreeMap::from([(
                    "message".to_string(),
                    JsonSchema::string(Some(
                        "Optional completion summary to deliver to the parent.".to_string(),
                    )),
                )]),
                None,
                Some(false.into()),
            ),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowupArgs {
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnoozeArgs {
    delay_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteArgs {
    message: Option<String>,
}

struct Output(JsonValue);

impl ToolOutput for Output {
    fn log_output(&self) -> String {
        JsonToolOutput::new(self.0.clone()).log_output()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        JsonToolOutput::new(self.0.clone()).to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.0.clone()
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
