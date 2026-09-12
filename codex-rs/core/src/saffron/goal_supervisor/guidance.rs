//! Turn-local guidance for activating and deferring durable goals.

use std::sync::Arc;

use codex_context_fragments::AdditionalContextDeveloperFragment;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_features::Feature;
use codex_protocol::user_input::UserInput;

use crate::config::Config;

const CONTEXT_KEY: &str = "saffron_goal_waiting";
const INSTRUCTIONS: &str = concat!(
    "If this turn requests durable continuation, call create_goal before claiming the goal is ",
    "active. When progress is blocked until a future time or slow external change, end the root ",
    "turn so Saffron supervision can snooze until the earliest useful check-in."
);

/// Installs the model-visible routing hint beside the app-server goal extension.
pub(crate) fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let extension = Arc::new(GoalGuidanceExtension);
    builder.thread_lifecycle_contributor(extension.clone());
    builder.config_contributor(extension.clone());
    builder.turn_input_contributor(extension);
}

#[derive(Debug)]
struct GoalGuidanceExtension;

#[derive(Debug)]
struct GoalGuidanceEnabled(bool);

impl ThreadLifecycleContributor<Config> for GoalGuidanceExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            set_enabled(input.thread_store, input.config);
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
        set_enabled(thread_store, new_config);
    }
}

impl TurnInputContributor for GoalGuidanceExtension {
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext,
        _extension_metrics: Option<Arc<dyn codex_extension_api::ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            if !thread_store
                .get::<GoalGuidanceEnabled>()
                .is_some_and(|enabled| enabled.0)
                || !mentions_goal(&input.user_input)
            {
                return Vec::new();
            }

            vec![Box::new(AdditionalContextDeveloperFragment::new(
                CONTEXT_KEY.to_string(),
                INSTRUCTIONS.to_string(),
            )) as Box<dyn ContextualUserFragment + Send>]
        })
    }
}

fn set_enabled(thread_store: &ExtensionData, config: &Config) {
    thread_store.insert(GoalGuidanceEnabled(config.features.enabled(Feature::Goals)));
}

/// Goal terminology is uncommon enough to be a useful routing signal, while
/// the conditional wording in `INSTRUCTIONS` keeps explanatory questions from
/// becoming requests to create a goal.
fn mentions_goal(input: &[UserInput]) -> bool {
    input.iter().any(|item| {
        let UserInput::Text { text, .. } = item else {
            return false;
        };
        text.split(|character: char| !character.is_alphanumeric())
            .any(|word| word.eq_ignore_ascii_case("goal") || word.eq_ignore_ascii_case("goals"))
    })
}
