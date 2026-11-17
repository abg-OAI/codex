//! Bounded admission of remote executors before a thread exposes their capabilities.

use super::storage::ExecutorExclusion;
use super::storage::SaffronStore;
use crate::agents_md::DiscoveredProjectInstructions;
use crate::agents_md::discover_project_instructions;
use crate::config::Config;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::TurnEnvironmentState;
use crate::environment_selection::retain_unavailable_selections;
use crate::session::ThreadEnvironmentDefaults;
use crate::shell::Shell;
use crate::shell_snapshot::ShellSnapshot;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionDataInit;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::EnvironmentCapabilityRoots;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentRequest;
use codex_protocol::protocol::TurnEnvironmentSelection;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const STARTUP_BUDGET: Duration = Duration::from_secs(10);

/// Records caller intent before native defaults turn an omitted selection into a list.
#[derive(Clone, Copy)]
struct ImplicitSelection(bool);

/// Marks automatically chosen executors without adding a user configuration option.
pub fn record_selection_intent(init: &mut ExtensionDataInit, implicit: bool) {
    if init.get::<ImplicitSelection>().is_none() {
        init.insert(ImplicitSelection(implicit));
    }
}

/// Inputs needed to admit executor bindings without starting their shell snapshots.
pub(crate) struct StartupEnvironments {
    pub(crate) manager: Arc<EnvironmentManager>,
    pub(crate) shell: Shell,
    pub(crate) defaults: ThreadEnvironmentDefaults,
    pub(crate) selections: Vec<TurnEnvironmentSelection>,
    pub(crate) inherited: TurnEnvironmentSnapshot,
    pub(crate) project_instructions_pinned: bool,
}

/// Admitted selections, reusable bindings and completed instruction discovery.
pub(crate) struct AdmittedEnvironments {
    pub(crate) selections: Vec<TurnEnvironmentSelection>,
    pub(crate) inherited: TurnEnvironmentSnapshot,
    pub(crate) instructions: Vec<DiscoveredProjectInstructions>,
    pub(crate) warnings: Vec<String>,
}

/// Restores exclusions and admits remote startup before exposing executor capabilities.
/// An implicit local primary may omit failed secondaries; explicit selections must succeed.
/// Explicit pending owners stay deferred when DeferredExecutor is enabled.
/// Pinned review snapshots already carry ready inherited bindings and their instructions.
pub(crate) async fn prepare(
    config: &Config,
    thread_id: ThreadId,
    resumed: bool,
    init: &ExtensionDataInit,
    mut startup: StartupEnvironments,
) -> anyhow::Result<AdmittedEnvironments> {
    let mut store = if resumed && !config.ephemeral {
        Some(SaffronStore::open(config.sqlite_config()).await?)
    } else {
        None
    };
    if let Some(store) = &store {
        let roots = EnvironmentCapabilityRoots::collect(
            startup
                .selections
                .iter()
                .map(|selection| &selection.selected_capability_roots),
        );
        for exclusion in store.executor_exclusions(thread_id).await? {
            let selection = TurnEnvironmentSelection::new(
                TurnEnvironmentRequest {
                    environment_id: exclusion.environment_id,
                    cwd: exclusion.cwd,
                    workspace_roots: exclusion.workspace_roots,
                    config: EnvironmentConfigState::Unavailable,
                },
                &roots,
            );
            startup
                .selections
                .retain(|selected| selected.environment_id != selection.environment_id);
            startup
                .selections
                .insert(exclusion.position.min(startup.selections.len()), selection);
        }
    }
    retain_unavailable_selections(&startup.inherited.all_selections(), &mut startup.selections);
    let implicit = init
        .get::<ImplicitSelection>()
        .is_some_and(|intent| intent.0);
    let optional_secondaries = implicit
        && startup.selections.first().is_some_and(|selection| {
            startup
                .manager
                .get_environment(&selection.environment_id)
                .is_some_and(|environment| !environment.is_remote())
        });
    let mut result = AdmittedEnvironments {
        selections: startup.selections,
        inherited: startup.inherited,
        instructions: Vec::new(),
        warnings: Vec::new(),
    };
    if !startup.project_instructions_pinned {
        admit(
            config,
            startup.manager,
            startup.shell,
            startup.defaults,
            optional_secondaries,
            &mut result,
        )
        .await?;
    }
    let exclusions: Vec<_> = result
        .selections
        .iter()
        .enumerate()
        .filter_map(|(position, selection)| {
            matches!(selection.config, EnvironmentConfigState::Unavailable).then(|| {
                ExecutorExclusion {
                    position,
                    environment_id: selection.environment_id.clone(),
                    cwd: selection.cwd.clone(),
                    workspace_roots: selection.workspace_roots.clone(),
                }
            })
        })
        .collect();
    if !exclusions.is_empty() && !config.ephemeral {
        if store.is_none() {
            store = Some(SaffronStore::open(config.sqlite_config()).await?);
        }
        if let Some(store) = &store {
            store
                .save_executor_exclusions(thread_id, &exclusions)
                .await?;
        }
    }
    // Exclusions have no attachment, so they remain present even if the registry forgets the ID.
    for selection in &result.selections {
        if matches!(selection.config, EnvironmentConfigState::Unavailable) {
            result
                .inherited
                .environments
                .retain(|state| state_selection(state).environment_id != selection.environment_id);
            result
                .inherited
                .environments
                .push(unavailable(selection.clone()));
        }
    }
    // The owner preserves ordering, including the original local primary.
    result.inherited.environments.sort_by_key(|state| {
        result
            .selections
            .iter()
            .position(|selection| selection.environment_id == state_selection(state).environment_id)
            .unwrap_or(usize::MAX)
    });
    Ok(result)
}

async fn admit(
    config: &Config,
    manager: Arc<EnvironmentManager>,
    shell: Shell,
    defaults: ThreadEnvironmentDefaults,
    optional_secondaries: bool,
    result: &mut AdmittedEnvironments,
) -> anyhow::Result<()> {
    let remote: Vec<_> = result
        .selections
        .iter()
        .filter(|selection| {
            !matches!(selection.config, EnvironmentConfigState::Unavailable)
                && !(config.features.enabled(Feature::DeferredExecutor)
                    && !optional_secondaries
                    && matches!(selection.config, EnvironmentConfigState::Pending))
                && manager
                    .get_environment(&selection.environment_id)
                    .is_none_or(|environment| environment.is_remote())
        })
        .cloned()
        .collect();
    if remote.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + STARTUP_BUDGET;
    let discovery = ThreadEnvironments::new(
        manager,
        shell,
        defaults,
        ShellSnapshot::disabled(),
        result.inherited.clone(),
        true,
    )
    .with_unavailable_selections(&result.selections);
    discovery.update_configured_selections(&remote);
    let snapshot = discovery.configured_snapshot_now();
    let mut pending = futures::stream::iter(remote)
        .map(|selection| {
            let state = snapshot
                .environments
                .iter()
                .find(|state| state_selection(state).environment_id == selection.environment_id)
                .cloned();
            async move {
                let admitted = tokio::time::timeout_at(deadline, async {
                    let state =
                        state.ok_or_else(|| anyhow::anyhow!("executor is not registered"))?;
                    if let TurnEnvironmentState::Starting(starting) = &state {
                        starting.wait_until_ready().await?;
                    }
                    let snapshot = TurnEnvironmentSnapshot {
                        environments: vec![state],
                    }
                    .refresh_readiness();
                    let environment = snapshot
                        .turn_environments()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("executor is not ready"))?
                        .clone();
                    let instructions = discover_project_instructions(config, &environment).await?;
                    Ok::<_, anyhow::Error>((environment, instructions))
                })
                .await
                .map_err(|_| anyhow::anyhow!("shared 10-second startup deadline elapsed"))
                .and_then(|result| result);
                (selection, admitted)
            }
        })
        .buffer_unordered(16);
    while let Some((selection, admitted)) = pending.next().await {
        match admitted {
            Ok((environment, instructions)) => {
                result.inherited.environments.retain(|state| {
                    state_selection(state).environment_id != selection.environment_id
                });
                result
                    .inherited
                    .environments
                    .push(TurnEnvironmentState::Ready(environment));
                result.instructions.push(instructions);
            }
            Err(error) if optional_secondaries => {
                if let Some(selected) = result
                    .selections
                    .iter_mut()
                    .find(|selected| selected.environment_id == selection.environment_id)
                {
                    selected.config = EnvironmentConfigState::Unavailable;
                }
                result.warnings.push(format!(
                    "Executor `{}` is unavailable for this thread: {error}",
                    selection.environment_id
                ));
            }
            Err(error) => {
                return Err(CodexErr::InvalidRequest(format!(
                    "Required executor `{}` could not complete startup: {error}",
                    selection.environment_id
                ))
                .into());
            }
        }
    }
    Ok(())
}

fn state_selection(state: &TurnEnvironmentState) -> &TurnEnvironmentSelection {
    match state {
        TurnEnvironmentState::Ready(environment) => &environment.selection,
        TurnEnvironmentState::Starting(environment) => &environment.selection,
        TurnEnvironmentState::Failed { selection, .. } => selection,
    }
}

fn unavailable(selection: TurnEnvironmentSelection) -> TurnEnvironmentState {
    TurnEnvironmentState::Failed {
        selection,
        required_skills: Vec::new(),
        error: "executor is unavailable for this thread".to_string(),
    }
}

#[cfg(test)]
#[path = "executor_startup_tests.rs"]
mod tests;
