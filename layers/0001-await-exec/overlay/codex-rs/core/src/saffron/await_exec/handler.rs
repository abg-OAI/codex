//! Model-facing adapter for `saffron.await_exec`.

use std::collections::BTreeMap;
use std::time::Duration;

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
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;

use super::ReturnOn;
use super::wait::AwaitExecRequest;
use super::wait::AwaitExecResult;
use super::wait::WakeReason;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::unified_exec::post_unified_exec_tool_use_payload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::resolve_max_tokens;

const NAMESPACE: &str = "saffron";
const TOOL_NAME: &str = "await_exec";

/// Registers and executes the `saffron.await_exec` tool.
///
/// The runtime may dispatch waits for different sessions in parallel. The
/// process operation serializes calls that target the same session with
/// `write_stdin` and other output-draining interactions.
pub(crate) struct Handler;

/// Validated arguments for one wait on an existing unified-exec session.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AwaitExecArgs {
    /// Session identifier returned by `exec_command`.
    session_id: i32,

    /// Events that may complete the wait.
    #[serde(default)]
    return_on: ReturnOn,

    /// Optional independent deadline for this wait, in milliseconds.
    #[serde(default)]
    timeout_ms: Option<u32>,

    /// Requested response budget before the model policy applies its cap.
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

/// Tool result retained in unified-exec form until a rendering surface asks
/// for it.
///
/// Keeping the canonical result preserves the originating command's hook
/// identity and prevents JSON rendering from becoming a second source of
/// lifecycle truth.
struct AwaitExecToolOutput {
    result: AwaitExecResult,
}

/// JSON response returned to the model.
///
/// `reason` identifies why this call returned. The optional process fields
/// describe the state observed afterward: `session_id` is present only while
/// the process remains available for another interaction, and `exit_code` is
/// present when unified exec obtained a numeric status.
#[derive(Serialize)]
struct AwaitExecResponse {
    reason: WakeReason,
    chunk_id: String,
    wall_time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<i32>,
    original_token_count: usize,
    output: String,
}

impl AwaitExecToolOutput {
    /// Renders the policy-bounded response shared by direct calls and Code
    /// Mode.
    fn response_value(&self) -> JsonValue {
        let output = &self.result.output;
        let Some(original_token_count) = output.original_token_count else {
            return json!({ "error": "await_exec result omitted original token count" });
        };
        // A call may request less output, but it cannot raise the model's
        // configured truncation ceiling.
        let max_output_tokens = resolve_max_tokens(output.max_output_tokens)
            .min(output.truncation_policy.token_budget());
        serde_json::to_value(AwaitExecResponse {
            reason: self.result.reason,
            chunk_id: output.chunk_id.clone(),
            wall_time_seconds: output.wall_time.as_secs_f64(),
            exit_code: output.exit_code,
            session_id: output.process_id,
            original_token_count,
            output: output.truncated_output(max_output_tokens),
        })
        .unwrap_or_else(
            |err| json!({ "error": format!("failed to serialize await result: {err}") }),
        )
    }
}

impl ToolOutput for AwaitExecToolOutput {
    fn log_output(&self) -> String {
        JsonToolOutput::new(self.response_value()).log_output()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        JsonToolOutput::new(self.response_value()).to_response_item(call_id, payload)
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        self.result.output.post_tool_use_id(call_id)
    }

    fn post_tool_use_input(&self, payload: &ToolPayload) -> Option<JsonValue> {
        self.result.output.post_tool_use_input(payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<JsonValue> {
        self.result.output.post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.response_value()
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_tool_spec()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: AwaitExecArgs = parse_arguments(&arguments)?;
            if args.timeout_ms == Some(0) {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be greater than zero when provided".to_string(),
                ));
            }

            let result = session
                .services
                .unified_exec_manager
                .await_exec(AwaitExecRequest {
                    process_id: args.session_id,
                    return_on: args.return_on,
                    timeout: args
                        .timeout_ms
                        .map(|timeout_ms| Duration::from_millis(u64::from(timeout_ms))),
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: turn.model_info().truncation_policy.into(),
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("await_exec failed: {err}"))
                })?;

            Ok(boxed_tool_output(AwaitExecToolOutput { result }))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        None
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // Awaiting reports progress for the originating shell command. Reuse
        // its hook identity so an exit completes that command instead of
        // appearing as an unrelated tool invocation.
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

/// Declares the operational contract presented to another agent.
fn create_tool_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "session_id".to_string(),
            JsonSchema::integer(Some(
                "Signed 32-bit session ID returned by exec_command for a command that is still running. Reuse the session_id returned by this tool to wait again."
                    .to_string(),
            )),
        ),
        (
            "return_on".to_string(),
            JsonSchema::string_enum(
                vec![json!("output_or_exit"), json!("exit")],
                Some(
                    "Event to wait for. output_or_exit (default) resumes on new output or exit. exit keeps buffering output and resumes only when the process exits or timeout_ms elapses."
                        .to_string(),
                ),
            ),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::integer(Some(
                "Independent wait deadline in milliseconds (up to 4294967295). Omit unless the agent must resume without output or exit. A timeout does not terminate the process; required cleanup may finish afterward."
                    .to_string(),
            )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::integer(Some(
                "Non-negative maximum tokens of accumulated process output to return. Defaults to 10000 and may be reduced by model policy; excess middle output is marked as omitted."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: NAMESPACE.to_string(),
        description: "Saffron extensions for coordinating long-running exec_command sessions."
            .to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: concat!(
                "Wait for output or exit from a running exec_command session. Use this instead of ",
                "empty write_stdin calls when the command needs no input. Pass exec_command's ",
                "session_id. By default, buffered or new output and exit resume the call; ",
                "return_on=exit buffers output until exit. Set timeout_ms only for an independent ",
                "deadline; a timeout does not terminate the command. Returned output is consumed ",
                "and not repeated. Never call await_exec and write_stdin concurrently for the same ",
                "session. If session_id is returned, call await_exec again or use write_stdin to ",
                "interact. Invalid or stale sessions and process or approval failures are errors; ",
                "start a new exec_command session instead of retrying a stale ID. This tool does ",
                "not start commands, send input, or terminate them."
            )
            .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["session_id".to_string()]),
                Some(false.into()),
            ),
            output_schema: Some(json!({
                "type": "object",
                "description": "One bounded chunk of process output plus the event and process state observed by this wait.",
                "properties": {
                    "reason": {
                        "type": "string",
                        "enum": ["output", "exit", "timeout"],
                        "description": "Why this call returned. exit takes precedence if termination is observed while resolving another wake event."
                    },
                    "chunk_id": {
                        "type": "string",
                        "description": "Identifier for this response's output chunk, which may be empty."
                    },
                    "wall_time_seconds": {
                        "type": "number",
                        "description": "Elapsed wall time for this operation, including same-session access and required lifecycle cleanup, in seconds."
                    },
                    "exit_code": {
                        "type": "integer",
                        "description": "Numeric process status when one is available after exit; otherwise omitted."
                    },
                    "session_id": {
                        "type": "integer",
                        "description": "Reusable session ID when the process is still running. Omitted after exit."
                    },
                    "original_token_count": {
                        "type": "integer",
                        "description": "Approximate token count of output before response truncation."
                    },
                    "output": {
                        "type": "string",
                        "description": "Output consumed by this call since the preceding process interaction. In exit mode it includes retained intermediate output, subject to the output limit."
                    }
                },
                "required": [
                    "reason",
                    "chunk_id",
                    "wall_time_seconds",
                    "original_token_count",
                    "output"
                ],
                "additionalProperties": false
            })),
        })],
    })
}
