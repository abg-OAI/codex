use super::*;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::multi_agents_spec::create_supervisor_followup_parent_tool;
use crate::tools::handlers::multi_agents_spec::create_supervisor_tools_namespace;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_tools::ToolSpec;

/// Delivers the Goal Supervisor's actionable result without extending `collaboration.*`.
pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("supervisor", "followup_parent")
    }

    fn spec(&self) -> ToolSpec {
        create_supervisor_tools_namespace(vec![create_supervisor_followup_parent_tool()])
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            handle_followup_parent(invocation)
                .await
                .map(boxed_tool_output)
        })
    }
}

async fn handle_followup_parent(
    invocation: ToolInvocation,
) -> Result<SupervisorFollowupParentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        source,
        ..
    } = invocation;
    let arguments = function_arguments(payload)?;
    let args: SupervisorFollowupParentArgs = parse_arguments(&arguments)?;
    if args.message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to the supervised parent".to_string(),
        ));
    }
    let Some(parent_thread_id) = session
        .services
        .agent_control
        .goal_supervisor_parent_for_helper(session.thread_id)
        .await
    else {
        return Err(FunctionCallError::RespondToModel(
            "supervisor.followup_parent is only available in goal supervisor check-in threads."
                .to_string(),
        ));
    };
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(parent_thread_id)
        .unwrap_or_default();
    let receiver_path = receiver_agent.agent_path.unwrap_or_else(AgentPath::root);
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let communication = if matches!(source, ToolCallSource::DirectPlaintextMessage) {
        let content = InterAgentMessage::new(
            InterAgentMessageType::NewTask,
            receiver_path.clone(),
            author.clone(),
            args.message,
        )
        .render();
        InterAgentCommunication::new(
            author,
            receiver_path,
            Vec::new(),
            content,
            /*trigger_turn*/ true,
        )
    } else {
        InterAgentCommunication::new_encrypted(
            author,
            receiver_path,
            Vec::new(),
            args.message,
            /*trigger_turn*/ true,
        )
    };
    let context =
        AgentCommunicationContext::new(AgentCommunicationKind::Followup, session.thread_id);
    session
        .services
        .agent_control
        .send_inter_agent_communication(
            parent_thread_id,
            communication.clone(),
            context,
            Some(turn.sub_id.clone()),
        )
        .await
        .map_err(|err| frodex_agent_error(parent_thread_id, err))?;
    session
        .services
        .agent_control
        .record_goal_supervisor_followup_action(parent_thread_id, &communication)
        .await
        .map_err(|err| frodex_agent_error(parent_thread_id, err))?;
    let delivered = session
        .services
        .agent_control
        .finish_goal_supervisor_helper_after_followup(session.thread_id)
        .await
        .map_err(|err| frodex_agent_error(parent_thread_id, err))?;

    Ok(SupervisorFollowupParentResult { delivered })
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupervisorFollowupParentArgs {
    message: String,
}

#[derive(Debug, Serialize)]
struct SupervisorFollowupParentResult {
    delivered: bool,
}

impl ToolOutput for SupervisorFollowupParentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "followup_parent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn terminal_no_response(&self) -> bool {
        self.delivered
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "followup_parent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "followup_parent")
    }
}
