//! Model-facing edits to the objective of an active durable goal.
//!
//! The tool derives its target from the caller's authorized session identity;
//! callers cannot supply thread or goal IDs. Root calls capture the current
//! goal when the tool executes. Supervisor calls use the revision captured
//! before the helper was spawned, so a later user edit, status change, or goal
//! replacement takes precedence over the helper's stale decision.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_features::Feature;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::validate_thread_goal_objective;
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

use super::goal_supervisor;
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
const TOOL_NAME: &str = "edit_active_goal";

#[derive(Clone, Copy)]
enum Authority {
    Root,
    Supervisor,
}

/// Registers root editing only where the built-in goal lifecycle is usable.
pub(super) fn register_root_if_available(
    session: &Session,
    turn_context: &TurnContext,
    registry: &mut ToolRegistry,
) {
    if turn_context.config.features.get().enabled(Feature::Goals)
        && session.services.state_db.is_some()
        && !matches!(turn_context.session_source, SessionSource::SubAgent(_))
    {
        registry.add(Handler::new(Authority::Root));
    }
}

/// Registers editing for a helper already authorized as a Saffron supervisor.
pub(super) fn register_supervisor(registry: &mut ToolRegistry) {
    registry.add(Handler::new(Authority::Supervisor));
}

/// Executes an objective-only edit against the goal selected by caller identity.
struct Handler {
    authority: Authority,
}

impl Handler {
    const fn new(authority: Authority) -> Self {
        Self { authority }
    }

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
                "goal edit received an unsupported payload".to_string(),
            ));
        };
        let args: EditGoalArgs = parse_arguments(&arguments)?;
        let objective = args.objective.trim();
        validate_thread_goal_objective(objective).map_err(FunctionCallError::RespondToModel)?;

        let target = self
            .resolve_target(&session, turn.sub_id.clone())
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let Some(state_db) = target.session.services.state_db.as_ref() else {
            self.clear_failed_edit(&target.session, session.thread_id)
                .await;
            return Err(FunctionCallError::RespondToModel(
                "goal state is unavailable".to_string(),
            ));
        };
        let updated = state_db
            .thread_goals()
            .update_active_thread_goal_objective(
                target.session.thread_id,
                &target.revision,
                objective,
            )
            .await;
        let goal = match updated {
            Ok(Some(goal)) => goal,
            Ok(None) => {
                self.clear_failed_edit(&target.session, session.thread_id)
                    .await;
                return Err(FunctionCallError::RespondToModel(
                    "the active goal changed before its objective could be edited".to_string(),
                ));
            }
            Err(error) => {
                self.clear_failed_edit(&target.session, session.thread_id)
                    .await;
                return Err(FunctionCallError::RespondToModel(format!(
                    "failed to edit the active goal: {error}"
                )));
            }
        };

        let goal = protocol_goal(goal);
        target
            .session
            .send_event_raw(Event {
                id: format!("saffron-edit-goal-{call_id}"),
                msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                    thread_id: target.session.thread_id,
                    turn_id: target.turn_id,
                    goal: goal.clone(),
                }),
            })
            .await;
        if matches!(self.authority, Authority::Supervisor) {
            goal_supervisor::commit_goal_edit(&target.session, session.thread_id).await;
        }

        Ok(boxed_tool_output(JsonToolOutput::new(
            json!({ "goal": goal }),
        )))
    }

    async fn resolve_target(
        &self,
        caller: &Arc<Session>,
        caller_turn_id: String,
    ) -> Result<EditTarget, String> {
        match self.authority {
            Authority::Root => {
                if matches!(caller.session_source().await, SessionSource::SubAgent(_)) {
                    return Err("only a root thread may edit its active goal".to_string());
                }
                let state_db = caller
                    .services
                    .state_db
                    .as_ref()
                    .ok_or_else(|| "goal state is unavailable".to_string())?;
                let goal = state_db
                    .thread_goals()
                    .get_thread_goal(caller.thread_id)
                    .await
                    .map_err(|error| format!("failed to read the active goal: {error}"))?
                    .filter(|goal| goal.status == codex_state::ThreadGoalStatus::Active)
                    .ok_or_else(|| "there is no active goal to edit".to_string())?;
                Ok(EditTarget {
                    session: Arc::clone(caller),
                    revision: codex_state::ThreadGoalRevision::capture(&goal),
                    turn_id: Some(caller_turn_id),
                })
            }
            Authority::Supervisor => {
                let parent = goal_supervisor::parent_for_helper(caller).await?;
                let revision = goal_supervisor::begin_goal_edit(&parent, caller.thread_id).await?;
                Ok(EditTarget {
                    session: parent,
                    revision,
                    turn_id: None,
                })
            }
        }
    }

    async fn clear_failed_edit(&self, target: &Session, helper_id: codex_protocol::ThreadId) {
        if matches!(self.authority, Authority::Supervisor) {
            goal_supervisor::clear_failed_goal_edit(target, helper_id).await;
        }
    }
}

struct EditTarget {
    session: Arc<Session>,
    revision: codex_state::ThreadGoalRevision,
    turn_id: Option<String>,
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
                description: "Replace only the objective of the active durable goal for this thread. The edit preserves goal identity, status, budget, and accumulated usage, and fails if the selected goal changed before the edit commits. Use it only when the objective no longer accurately states the user-authorized outcome; preserve explicit requirements and do not use the objective as a progress log."
                    .to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([(
                        "objective".to_string(),
                        JsonSchema::string(Some(
                            "Complete replacement objective. Preserve every still-applicable user requirement and do not broaden the authorized scope."
                                .to_string(),
                        )),
                    )]),
                    Some(vec!["objective".to_string()]),
                    /*additional_properties*/ Some(false.into()),
                ),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditGoalArgs {
    objective: String,
}

pub(super) fn protocol_goal(goal: codex_state::ThreadGoal) -> ThreadGoal {
    ThreadGoal {
        thread_id: goal.thread_id,
        objective: goal.objective,
        status: match goal.status {
            codex_state::ThreadGoalStatus::Active => ThreadGoalStatus::Active,
            codex_state::ThreadGoalStatus::Paused => ThreadGoalStatus::Paused,
            codex_state::ThreadGoalStatus::Blocked => ThreadGoalStatus::Blocked,
            codex_state::ThreadGoalStatus::UsageLimited => ThreadGoalStatus::UsageLimited,
            codex_state::ThreadGoalStatus::BudgetLimited => ThreadGoalStatus::BudgetLimited,
            codex_state::ThreadGoalStatus::Complete => ThreadGoalStatus::Complete,
        },
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at.timestamp(),
        updated_at: goal.updated_at.timestamp(),
    }
}
