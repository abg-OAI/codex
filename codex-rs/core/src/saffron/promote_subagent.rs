//! Copies a completed child into an independent root before retiring its runtime.
//!
//! The durable receipt gives retries the same destination. Native creation is
//! attempted only by the reservation winner: an uncertain startup is reported,
//! never replayed over a possibly existing thread. Retirement uses completion
//! witnesses so new work keeps its original owner.

use super::storage::CompletedAgent;
use super::storage::Promotion;
use super::storage::PromotionPhase;
use super::storage::SaffronStore;
use crate::CodexThread;
use crate::ThreadManager;
use crate::agent::api::AgentControl;
use crate::agent::control::LocalAgentControl;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::thread_manager::ForkSnapshot;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_thread_store::ForkBoundary;
use codex_thread_store::PersistContext;
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
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;

/// Installs promotion beside the root host's independent-fork tool.
pub(super) fn register(registry: &mut ToolRegistry, manager: Weak<ThreadManager>) {
    registry.add(Handler { manager });
}

/// Selects a visible child in the caller's tree.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Native thread ID or a task path resolved within the owning root.
    target: String,
}

/// Adapts model requests to the host that owns native thread creation.
struct Handler {
    /// Weak ownership lets host shutdown release thread creation.
    manager: Weak<ThreadManager>,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced("saffron", "promote_subagent")
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "saffron".into(), description: "Saffron thread lifecycle extensions.".into(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "promote_subagent".into(),
                description: "Promote a completed, loaded subagent in this root's tree to a new independent persistent thread, preserving its full history and effective settings. All open descendants must also be loaded and completed, with no pending work or terminal processes. Creates an idle standalone thread before retiring the original subtree; does not transfer a goal or running tasks. Returns the new thread_id. Retry with {\"target\":\"<source_thread_id>\"} to reconcile the same destination after the task name is retired. Retirement proceeds from descendants to the source; a failure can leave some members already closed. If status is not promoted, inspect that destination and the reported error rather than making another fork.".into(),
                strict: false, defer_loading: None,
                parameters: JsonSchema::object(BTreeMap::from([("target".into(), JsonSchema::string(Some("Subagent ID or task name to promote.".into())))]), Some(vec!["target".into()]), Some(false.into())),
                output_schema: None,
            })],
        })
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.promote(invocation))
    }
}
impl CoreToolRuntime for Handler {}

impl Handler {
    /// Resolves ownership before starting a handoff that survives waiter cancellation.
    async fn promote(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(invalid("promote_subagent requires function arguments"));
        };
        let args: Args = parse_arguments(arguments)?;
        if invocation.turn.session_source.is_non_root_agent() {
            return Err(invalid("only the owning root may promote a subagent"));
        }
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| invalid("thread host is shutting down"))?;
        let session = invocation.session.clone();
        let control = local_control(&session);
        let source_id = control
            .resolve(
                session.thread_id(),
                None,
                &invocation.turn.session_source,
                &args.target,
            )
            .await
            .map_err(invalid)?;
        let root_source = invocation.turn.session_source.clone();
        // Dropping the tool waiter must not abandon a saved fork before receipt reconciliation.
        let result =
            tokio::spawn(async move { promote(manager, session, root_source, source_id).await })
                .await
                .map_err(invalid)?
                .map_err(invalid)?;
        Ok(boxed_tool_output(JsonToolOutput::new(result)))
    }
}

/// Uses the caller's existing agent tree rather than a new root registry.
fn local_control(session: &Session) -> LocalAgentControl {
    session
        .services
        .local_agent_runtime
        .control(session.services.agent_control.identity())
}

/// Saves a native fork before conditionally retiring its completed source subtree.
/// A durable receipt identifies partial effects and prevents another creation attempt.
async fn promote(
    manager: Arc<ThreadManager>,
    session: Arc<Session>,
    root_source: SessionSource,
    source_id: ThreadId,
) -> anyhow::Result<serde_json::Value> {
    let store = SaffronStore::open(session.get_config().await.sqlite_config()).await?;
    if let Some(receipt) = store.promotion(source_id).await? {
        anyhow::ensure!(
            receipt.root_id == session.thread_id(),
            "promotion belongs to a different root"
        );
        return reconcile(&session, &store, receipt).await;
    }
    let control = local_control(&session);
    let metadata = control
        .get_agent_metadata(source_id)
        .ok_or_else(|| anyhow::anyhow!("target is not a subagent of this root"))?;
    anyhow::ensure!(
        source_id != session.thread_id()
            && metadata.agent_path.is_some_and(|path| !path.is_root())
            && !control.is_hidden_agent(source_id),
        "target must be a visible subagent of this root"
    );
    let graph = control
        .runtime
        .agent_graph_store()?
        .ok_or_else(|| anyhow::anyhow!("promotion requires persistent agent ancestry"))?;
    let mut ids = vec![source_id];
    ids.extend(
        graph
            .list_thread_spawn_descendants(source_id, Some(ThreadSpawnEdgeStatus::Open))
            .await?,
    );
    for id in control
        .runtime
        .list_live_agent_subtree_thread_ids(source_id)
        .await?
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let mut threads = Vec::new();
    let mut residency = Vec::new();
    for id in ids {
        let thread = control.get_live_thread(id).await.map_err(|_| {
            anyhow::anyhow!("subagent {id} must be loaded and completed before promotion")
        })?;
        residency.push(
            thread
                .residency_gate
                .clone()
                .try_write_owned()
                .map_err(|_| {
                    anyhow::anyhow!(
                        "subagent {id} has pending delivery or another lifecycle operation"
                    )
                })?,
        );
        threads.push(thread);
    }
    let mut admission = Vec::new();
    let mut members = Vec::new();
    for thread in &threads {
        let slot = thread
            .session
            .active_turn
            .try_lock()
            .map_err(|_| anyhow::anyhow!("subagent is accepting work"))?;
        anyhow::ensure!(slot.is_none(), "subagent has an unfinished turn");
        anyhow::ensure!(
            !thread.session.has_outstanding_durable_sleep()
                && !thread
                    .session
                    .input_queue
                    .has_trigger_turn_mailbox_items()
                    .await
                && !thread.should_retain_while_idle().await,
            "subagent has pending work"
        );
        anyhow::ensure!(
            thread.list_background_terminals().await.is_empty(),
            "subagent has running terminal processes"
        );
        thread
            .session
            .try_ensure_rollout_materialized(PersistContext::Standard)
            .await?;
        thread.flush_rollout().await?;
        let turn_id = thread
            .session
            .checkpoint_idle_completion()
            .await
            .ok_or_else(|| {
                anyhow::anyhow!("subagent must have a completed turn in this runtime")
            })?;
        members.push(CompletedAgent {
            thread_id: thread.session.thread_id(),
            parent_id: thread
                .session_source
                .parent_thread_id()
                .ok_or_else(|| anyhow::anyhow!("target has no parent"))?,
            turn_id,
        });
        admission.push(slot);
    }
    let current_descendants = graph
        .list_thread_spawn_descendants(source_id, Some(ThreadSpawnEdgeStatus::Open))
        .await?;
    anyhow::ensure!(
        current_descendants
            .iter()
            .all(|id| members.iter().any(|member| member.thread_id == *id)),
        "subagent gained a descendant while preparing promotion; retry after it completes"
    );
    let receipt = Promotion {
        source_id,
        root_id: session.thread_id(),
        destination_id: ThreadId::new(),
        members,
        phase: PromotionPhase::Preparing,
    };
    if !store.reserve_promotion(&receipt).await? {
        return Ok(outcome(
            &store
                .promotion(source_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("promotion receipt disappeared"))?,
            "pending",
            Some("another invocation reserved this promotion".into()),
        ));
    }
    let creation = create_root(&manager, &store, &threads[0], root_source, &receipt).await;
    drop(admission);
    if let Err(error) = creation {
        return Ok(outcome(
            &receipt,
            "creation_unconfirmed",
            Some(error.to_string()),
        ));
    }
    if let Err(error) = store
        .set_promotion_phase(source_id, PromotionPhase::Ready)
        .await
    {
        return Ok(outcome(
            &receipt,
            "creation_unconfirmed",
            Some(format!(
                "Standalone created, but its receipt could not be updated: {error:#}. Original retained."
            )),
        ));
    }
    // Conditional shutdown uses the ordered input queue after releasing admission.
    // Retirement reacquires residency and rechecks the recorded completion.
    drop(residency);
    Ok(finish_retirement(&control, &store, &receipt).await)
}

/// Creates and flushes the reserved independent root with the source's effective settings.
async fn create_root(
    manager: &ThreadManager,
    store: &SaffronStore,
    source: &CodexThread,
    root_source: SessionSource,
    receipt: &Promotion,
) -> anyhow::Result<()> {
    let source_id = source.session.thread_id();
    let (mut options, settings) = manager.fork_options_from_parent(source_id).await?;
    options.config.ephemeral = false;
    options.session_source = Some(root_source);
    options.thread_source = Some(ThreadSource::User);
    options.reserved_thread_id = Some(receipt.destination_id);
    options.dynamic_tools = source.session.dynamic_tools().await.to_vec();
    options
        .thread_extension_init
        .insert(source.session.services.selected_capability_roots.clone());
    let exclusions = store.executor_exclusions(source_id).await?;
    store
        .save_executor_exclusions(receipt.destination_id, &exclusions)
        .await?;
    let stored = source
        .read_thread(
            true,
            options.history_mode == Some(ThreadHistoryMode::Legacy),
        )
        .await?;
    let fork = match options.history_mode {
        Some(ThreadHistoryMode::Paginated) => {
            let prepared = source
                .session
                .services
                .thread_store
                .prepare_fork(PrepareForkParams {
                    thread_id: source_id,
                    boundary: ForkBoundary::Latest,
                })
                .await?;
            manager.fork_prepared_thread(options, prepared).await?
        }
        _ => {
            let history = stored
                .history
                .ok_or_else(|| anyhow::anyhow!("source history unavailable"))?;
            manager
                .fork_thread_from_history(
                    ForkSnapshot::Interrupted,
                    options,
                    InitialHistory::Resumed(ResumedHistory {
                        history_revision: None,
                        conversation_id: source_id,
                        history: Arc::new(history.items),
                        rollout_path: stored.rollout_path,
                    }),
                )
                .await?
        }
    };
    fork.thread.restore_thread_settings(settings).await?;
    if let Some(name) = stored.name {
        fork.thread
            .update_thread_metadata(
                ThreadMetadataPatch {
                    name: Some(Some(name)),
                    ..Default::default()
                },
                true,
            )
            .await?;
    }
    fork.thread
        .session
        .try_ensure_rollout_materialized(PersistContext::Standard)
        .await?;
    fork.thread.flush_rollout().await?;
    manager.notify_thread_created(fork.thread_id);
    Ok(())
}

/// Reports both identities even when retirement or its final receipt update fails.
async fn finish_retirement(
    control: &LocalAgentControl,
    store: &SaffronStore,
    receipt: &Promotion,
) -> serde_json::Value {
    if let Err(error) = retire(control, receipt).await {
        return outcome(
            receipt,
            "created_retirement_pending",
            Some(format!("{error:#}")),
        );
    }
    if let Err(error) = store
        .set_promotion_phase(receipt.source_id, PromotionPhase::Retired)
        .await
    {
        return outcome(
            receipt,
            "retirement_unconfirmed",
            Some(format!(
                "Original retired, but its receipt could not be updated: {error:#}. Retry with target set to the returned source_thread_id."
            )),
        );
    }
    outcome(receipt, "promoted", None)
}

/// Closes completed members from children to parent, refusing superseding work.
async fn retire(control: &LocalAgentControl, receipt: &Promotion) -> anyhow::Result<()> {
    let graph = control
        .runtime
        .agent_graph_store()?
        .ok_or_else(|| anyhow::anyhow!("agent ancestry unavailable"))?;
    for member in receipt.members.iter().rev() {
        let closed = graph
            .list_thread_spawn_children(member.parent_id, Some(ThreadSpawnEdgeStatus::Closed))
            .await?;
        let source = match control.get_live_thread(member.thread_id).await {
            Ok(source) => source,
            Err(_) if closed.contains(&member.thread_id) => continue,
            Err(error) => return Err(error.into()),
        };
        let _residency = source
            .residency_gate
            .clone()
            .try_write_owned()
            .map_err(|_| {
                anyhow::anyhow!("subagent has pending delivery or another lifecycle operation")
            })?;
        let unchanged = if source.is_running() {
            source
                .request_shutdown_after_turn(member.turn_id.clone())
                .await?
        } else {
            source
                .session
                .can_shutdown_after_turn(&member.turn_id)
                .await
        };
        anyhow::ensure!(
            unchanged,
            "subagent {} changed or could not confirm its recorded completion; original retained",
            member.thread_id
        );
        source.wait_until_terminated().await;
        control.close_terminated_agent(&source).await?;
    }
    Ok(())
}

/// Retries retirement only after confirmed creation; uncertain creation remains inspectable.
async fn reconcile(
    session: &Session,
    store: &SaffronStore,
    receipt: Promotion,
) -> anyhow::Result<serde_json::Value> {
    match receipt.phase {
        PromotionPhase::Retired => Ok(outcome(&receipt, "promoted", None)),
        PromotionPhase::Ready => Ok(finish_retirement(&local_control(session), store, &receipt).await),
        PromotionPhase::Preparing => Ok(outcome(
            &receipt,
            "creation_unconfirmed",
            Some("Creation is running or was interrupted. Inspect the reserved destination; do not create another fork. The original has not been retired by this operation.".into()),
        )),
    }
}

/// Reports both identities so callers can recover after the original task name is released.
fn outcome(receipt: &Promotion, status: &str, error: Option<String>) -> serde_json::Value {
    json!({ "source_thread_id": receipt.source_id, "thread_id": receipt.destination_id, "status": status, "error": error })
}

/// Converts validation and host failures into the tool's model-visible error.
fn invalid(error: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

/// Exercises native history, settings, retirement, and durable retry behavior.
#[cfg(test)]
#[path = "promote_subagent/tests.rs"]
mod tests;
