//! Developer guidance for root goals and their private supervisors.

use std::sync::Arc;

use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use codex_features::Feature;
use serde_json::Value;
use serde_json::json;

use crate::config::Config;

const SECTION_ID: &str = "saffron_goal_supervisor";
const START_MARKER: &str = "<saffron_goal_supervisor>";
const END_MARKER: &str = "</saffron_goal_supervisor>";
const LEGACY_START_MARKER: &str = "<saffron_goal_waiting>";
const LEGACY_END_MARKER: &str = "</saffron_goal_waiting>";
const REPLACEMENT_NOTICE: &str =
    "These goal-supervisor instructions replace the previously provided instructions.";
const REMOVAL_NOTICE: &str =
    "The previously provided goal-supervisor instructions no longer apply.";
const HELPER_INSTRUCTIONS: &str = include_str!("prompt.md");
const INSTRUCTIONS: &str = concat!(
    "## Goal supervision\n\n",
    "If this turn requests durable continuation, call `create_goal` before claiming the goal ",
    "is active. An active goal continues autonomously. Treat supervisor follow-ups as task ",
    "instructions. Resolve optional clarification and reversible choices with a supported ",
    "default and continue. Permission for one action gates that action. Before entering the ",
    "blocked-goal audit, consider the whole objective: do any authorized work due now, ",
    "including reading a source for an external change. When the next useful step is a ",
    "future check, keep the goal active and finish this turn with the source, when to check ",
    "it, and what to do if it changes. The supervisor can snooze until then. Repeated ",
    "unchanged observations are progress on a watch. If every remaining path requires ",
    "indispensable human input and no useful checkpoint remains, surface the question ",
    "once and follow the blocked-goal audit."
);

/// Installs role-specific goal guidance beside the app-server goal extension.
pub(crate) fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(GoalGuidanceExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.config_contributor(extension.clone());
    builder.prompt_contributor(extension);
}

#[derive(Debug)]
struct GoalGuidanceExtension;

#[derive(Clone, Copy, Debug)]
struct GoalGuidanceState {
    role_instructions: Option<&'static str>,
    goals_enabled: bool,
}

impl GoalGuidanceState {
    fn instructions(self) -> Option<&'static str> {
        self.role_instructions.filter(|_| self.goals_enabled)
    }
}

impl ThreadLifecycleContributor<Config> for GoalGuidanceExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.thread_store.insert(GoalGuidanceState {
                role_instructions: if super::identity::is_helper_source(input.session_source) {
                    Some(HELPER_INSTRUCTIONS)
                } else if input.session_source.is_non_root_agent() {
                    None
                } else {
                    Some(INSTRUCTIONS)
                },
                goals_enabled: input.config.features.enabled(Feature::Goals),
            });
        })
    }
}

impl ConfigContributor<Config> for GoalGuidanceExtension {
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &Config,
        new_config: &Config,
    ) {
        let role_instructions = thread_store
            .get::<GoalGuidanceState>()
            .and_then(|state| state.role_instructions);
        thread_store.insert(GoalGuidanceState {
            role_instructions,
            goals_enabled: new_config.features.enabled(Feature::Goals),
        });
    }
}

impl ContextContributor for GoalGuidanceExtension {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let instructions = input
                .thread_store
                .get::<GoalGuidanceState>()
                .and_then(|state| state.instructions());
            vec![guidance_section(instructions)]
        })
    }
}

fn guidance_section(instructions: Option<&'static str>) -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(SECTION_ID, move |previous| {
        (
            Some(json!({ "instructions": instructions })),
            render_guidance_diff(instructions, previous),
        )
    })
    .with_legacy_matcher(matches_legacy_guidance)
    .with_retained_fragment_matcher(matches_guidance)
}

fn render_guidance_diff(
    instructions: Option<&'static str>,
    previous: PreviousWorldStateSection<'_>,
) -> Option<RenderedWorldStateFragment> {
    let previous_instructions = match previous {
        PreviousWorldStateSection::Absent => Some(None),
        PreviousWorldStateSection::Unknown => None,
        PreviousWorldStateSection::Known(previous) => Some(snapshot_instructions(previous)),
    };
    if previous_instructions == Some(instructions) {
        return None;
    }

    let body = match instructions {
        Some(instructions) if previous_instructions == Some(None) => instructions.to_string(),
        Some(instructions) => format!("{REPLACEMENT_NOTICE}\n\n{instructions}"),
        None if previous_instructions == Some(None) => return None,
        None => REMOVAL_NOTICE.to_string(),
    };
    Some(RenderedWorldStateFragment::new(
        "developer",
        (START_MARKER, END_MARKER),
        input_text(format!("\n{body}\n")),
    ))
}

fn snapshot_instructions(snapshot: &Value) -> Option<&str> {
    snapshot.get("instructions").and_then(Value::as_str)
}

fn matches_guidance(role: &str, text: &str) -> bool {
    matches_marked_guidance(role, text, START_MARKER, END_MARKER)
}

fn matches_legacy_guidance(role: &str, text: &str) -> bool {
    matches_guidance(role, text)
        || matches_marked_guidance(role, text, LEGACY_START_MARKER, LEGACY_END_MARKER)
}

fn matches_marked_guidance(role: &str, text: &str, start: &str, end: &str) -> bool {
    role == "developer" && text.trim_start().starts_with(start) && text.trim_end().ends_with(end)
}

#[cfg(test)]
#[path = "guidance_tests.rs"]
mod tests;
