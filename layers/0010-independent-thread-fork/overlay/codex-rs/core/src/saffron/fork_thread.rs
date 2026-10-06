//! Independent persistent forks initiated by the model in a root thread.
//!
//! The process ThreadManager owns execution after creation; the caller owns only
//! the initial assignment. Fork ancestry is history provenance, never agent-tree
//! ownership. Goal state is deliberately absent from this operation. Desktop
//! organization is a separate, optional effect after the assignment starts.

mod section;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;

use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_thread_store::ForkBoundary;
use codex_thread_store::PrepareForkParams;
use codex_thread_store::ThreadMetadataPatch;
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

use crate::StartIfIdleSubmission;
use crate::ThreadManager;
use crate::TurnInput;
use crate::TurnInputRequest;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::thread_manager::ForkSnapshot;
use crate::thread_manager::StartThreadOptions;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;

/// Makes independent forking available to root threads on this host.
///
/// The weak reference avoids retaining the process registry through its threads.
pub fn install(builder: &mut ExtensionRegistryBuilder<Config>, manager: Weak<ThreadManager>) {
    builder.thread_lifecycle_contributor(Arc::new(ForkHost { manager }));
}

/// Process capability copied into each root's extension data at startup.
struct ForkHost {
    /// Registry whose lifecycle owns all independently executing roots.
    manager: Weak<ThreadManager>,
}

impl ThreadLifecycleContributor<Config> for ForkHost {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !input.session_source.is_non_root_agent() {
                input.thread_store.insert(ForkHost {
                    manager: self.manager.clone(),
                });
            }
        })
    }
}

/// Exposes the tool only when an independent host registry is available.
pub(super) fn register(session: &Session, turn: &TurnContext, registry: &mut ToolRegistry) {
    if !turn.session_source.is_non_root_agent()
        && let Some(host) = session.services.thread_extension_data.get::<ForkHost>()
    {
        registry.add(Handler { host });
    }
}

/// Owns the fork, assignment submission, and optional placement operation.
struct Handler {
    /// Process capability, independent of this thread's agent tree.
    host: Arc<ForkHost>,
}

/// Model choices for one independent assignment.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Assignment supplied after the inherited completed history.
    prompt: String,
    /// Optional persistent title for locating the new thread.
    title: Option<String>,
    /// Request inheritance of the caller's Desktop sidebar section.
    #[serde(default = "inherit_section_by_default")]
    inherit_section: bool,
    /// Named Desktop destination, created if absent; overrides inheritance.
    section: Option<String>,
}

/// Omission requests the caller's section, without requiring Desktop.
fn inherit_section_by_default() -> bool {
    true
}

/// Agent assignment with sender identity taken from the running parent session.
///
/// Saved as standalone tool output so replay preserves attribution without
/// promoting the parent's interpretation to a human instruction.
#[derive(Serialize)]
struct Assignment {
    source_thread_id: ThreadId,
    /// A missing display name does not prevent assignment delivery.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_thread_name: Option<String>,
    input: String,
}

/// Creation survives later failures; the ID always identifies that saved fork.
#[derive(Serialize)]
struct Outcome {
    /// Persistent root created by this invocation.
    thread_id: ThreadId,
    /// Submission outcome, flattened for the model-facing response.
    #[serde(flatten)]
    execution: Execution,
    /// Independent Desktop placement outcome.
    section: section::Outcome,
}

/// Distinguishes accepted execution from a saved thread needing recovery.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Execution {
    /// The host accepted the initial assignment.
    Started {
        /// Turn created for the assignment.
        turn_id: String,
    },
    /// Creation succeeded, but preparing or submitting the assignment failed.
    CreatedNotStarted {
        /// Failure the caller can address using the existing thread ID.
        error: String,
    },
}

impl Handler {
    /// Creates a saved root before submission and reports any subsequent failure.
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(invalid("fork_thread requires function arguments"));
        };
        let args: Args = parse_arguments(arguments)?;
        if args.prompt.trim().is_empty() {
            return Err(invalid("prompt must not be empty"));
        }
        let section_name = args.section.as_deref().map(str::trim);
        if section_name == Some("") {
            return Err(invalid("section must not be empty"));
        }
        let title = args
            .title
            .as_deref()
            .map(|title| {
                crate::util::normalize_thread_name(title)
                    .ok_or_else(|| invalid("title must not be empty"))
            })
            .transpose()?;
        let session = &invocation.session;
        let turn = &invocation.turn;
        if turn.session_source.is_non_root_agent() {
            return Err(invalid("only root threads may create independent forks"));
        }
        let manager = self
            .host
            .manager
            .upgrade()
            .ok_or_else(|| invalid("fork host is shutting down"))?;
        let source = manager
            .get_thread(session.thread_id())
            .await
            .map_err(invalid)?;
        source.flush_rollout().await.map_err(invalid)?;
        let source_thread_name = source
            .read_thread(true, false)
            .await
            .ok()
            .and_then(|thread| thread.name);
        let mut config = turn.config.as_ref().clone();
        let settings = &invocation.step_context.settings;
        config.ephemeral = false;
        config.model = Some(settings.model_info.slug.clone());
        config.model_provider = turn.provider.info().clone();
        config.model_reasoning_effort = settings.effective_reasoning_effort();
        config.model_reasoning_summary = Some(settings.reasoning_summary);
        config.service_tier = settings.service_tier.clone();
        config.developer_instructions = turn.developer_instructions.clone();
        config.token_budget = turn.configured_token_budget.clone();
        config
            .permissions
            .approval_policy
            .set(settings.approval_policy())
            .map_err(invalid)?;
        config.approvals_reviewer = settings.approvals_reviewer();
        let base = session.get_base_instructions().await;
        config.base_instructions = Some(base.text);
        config.base_instructions_provenance = base.provenance;
        let mut thread_extension_init = ExtensionDataInit::new();
        thread_extension_init.insert(session.services.selected_capability_roots.clone());
        let options = StartThreadOptions {
            history_mode: Some(turn.history_mode),
            session_source: Some(turn.session_source.clone()),
            dynamic_tools: turn.dynamic_tools.clone(),
            inherited_environments: Some(invocation.step_context.environments.clone()),
            client_mcp_extensions: session.services.client_mcp_extensions.clone(),
            disabled_plugin_ids: Some(turn.disabled_plugin_ids.clone()),
            thread_extension_init,
            ..StartThreadOptions::new(config)
        };
        let fork = match turn.history_mode {
            ThreadHistoryMode::Paginated => {
                let prepared = session
                    .services
                    .thread_store
                    .prepare_fork(PrepareForkParams {
                        thread_id: session.thread_id(),
                        boundary: ForkBoundary::BeforeTurn(turn.sub_id.clone()),
                    })
                    .await
                    .map_err(invalid)?;
                manager.fork_prepared_thread(options, prepared).await
            }
            ThreadHistoryMode::Legacy => {
                let stored = source.read_thread(true, true).await.map_err(invalid)?;
                let history = stored
                    .history
                    .ok_or_else(|| invalid("source history is unavailable"))?;
                let items = crate::truncate_rollout_before_turn_id(history.items, &turn.sub_id)
                    .map_err(invalid)?;
                manager
                    .fork_thread_from_history(
                        ForkSnapshot::Interrupted,
                        options,
                        InitialHistory::Resumed(ResumedHistory {
                            history_revision: None,
                            conversation_id: session.thread_id(),
                            history: Arc::new(items),
                            rollout_path: stored.rollout_path,
                        }),
                    )
                    .await
            }
        }
        .map_err(invalid)?;
        manager.notify_thread_created(fork.thread_id);
        // The goal extension starts empty for this new ID. Only the app-server's
        // opt-in goal-copy operation can transfer a source goal; it is not used here.
        let execution = async {
            if let Some(title) = title {
                fork.thread
                    .update_thread_metadata(
                        ThreadMetadataPatch {
                            name: Some(Some(title)),
                            ..Default::default()
                        },
                        true,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            let assignment = ResponseItem::FunctionCallOutput {
                id: None,
                call_id: None,
                name: Some("fork_thread".to_owned()),
                namespace: Some("saffron".to_owned()),
                output: FunctionCallOutputPayload::from_text(
                    serde_json::to_string(&Assignment {
                        source_thread_id: session.thread_id(),
                        source_thread_name,
                        input: args.prompt,
                    })
                    .map_err(|error| error.to_string())?,
                ),
                internal_chat_message_metadata_passthrough: None,
            };
            let submitted = fork
                .thread
                .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(assignment)))
                .await
                .map_err(|error| error.to_string())?;
            match submitted {
                StartIfIdleSubmission::Started { turn_id } => Ok(turn_id),
                other => Err(format!("initial assignment was not started: {other:?}")),
            }
        }
        .await;
        let execution = match execution {
            Ok(turn_id) => Execution::Started { turn_id },
            Err(error) => Execution::CreatedNotStarted { error },
        };
        let section = section::place(
            &invocation,
            fork.thread_id,
            args.inherit_section,
            section_name,
        )
        .await;
        let outcome = Outcome {
            thread_id: fork.thread_id,
            execution,
            section,
        };
        Ok(boxed_tool_output(JsonToolOutput::new(
            serde_json::to_value(outcome).map_err(invalid)?,
        )))
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("saffron", "fork_thread")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "saffron".to_string(),
            description: "Saffron extensions for independent thread and task coordination.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "fork_thread".to_string(),
                description: "Start an independent persistent root thread with this thread's completed history, excluding the current turn. Supply its assignment in prompt. The fork inherits model, working directory and permissions, but no goal. It runs independently without a completion notification to this thread. Desktop placement is best effort: section overrides inherit_section and creates the named section if absent; otherwise inheritance defaults to true. Placement failure never undoes the fork. If status is created_not_started, recover using the returned thread_id instead of forking again.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(BTreeMap::from([
                    ("prompt".to_string(), JsonSchema::string(Some("The new thread's assignment.".to_string()))),
                    ("title".to_string(), JsonSchema::string(Some("Optional persistent thread title.".to_string()))),
                    ("inherit_section".to_string(), JsonSchema::boolean(Some("Inherit the caller's Desktop sidebar section when available; defaults to true.".to_string()))),
                    ("section".to_string(), JsonSchema::string(Some("Desktop section name, matched case-sensitively after trimming surrounding whitespace. Overrides inherit_section, reuses a unique matching section or creates one when absent. Duplicate names leave placement unconfirmed.".to_string()))),
                ]), Some(vec!["prompt".to_string()]), Some(false.into())),
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

/// Renders an operational failure before a persistent fork exists.
fn invalid(error: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

#[cfg(test)]
#[path = "fork_thread/tests.rs"]
mod tests;
