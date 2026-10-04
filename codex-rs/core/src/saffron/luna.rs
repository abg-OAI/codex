//! Catalog selection shared by Saffron's tool-free Luna helpers.

use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// Selects the newest catalog Luna and requires its Fast service tier.
pub(super) async fn select(
    session: &Session,
    turn: &TurnContext,
) -> anyhow::Result<(ModelInfo, ReasoningEffort)> {
    let models = session
        .services
        .models_manager
        .list_models(RefreshStrategy::Offline, turn.config.http_client_factory())
        .await;
    let selected = models
        .iter()
        .filter_map(|model| generation(&model.model).map(|generation| (generation, model)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, model)| model)
        .ok_or_else(|| anyhow::anyhow!("no Luna model in catalog"))?;
    let model = session
        .services
        .models_manager
        .get_model_info(&selected.model, &turn.config.to_models_manager_config())
        .await;
    anyhow::ensure!(
        model.supports_service_tier(ServiceTier::Fast.request_value()),
        "Luna model does not support Fast service"
    );
    Ok((model, selected.default_reasoning_effort.clone()))
}

/// Orders published Luna generations numerically rather than by display name.
pub(super) fn generation(model: &str) -> Option<Vec<u32>> {
    model
        .strip_prefix("gpt-")?
        .strip_suffix("-luna")?
        .split('.')
        .map(str::parse)
        .collect::<Result<Vec<_>, _>>()
        .ok()
}
