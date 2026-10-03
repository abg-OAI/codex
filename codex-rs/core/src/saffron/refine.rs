//! Opt-in interpretation of incoming requests before the main model samples.
//!
//! The regular turn calls `prepare` after hooks have accepted and recorded a batch.
//! Only that batch can trigger inference. Original history and authorization stay
//! unchanged; a persisted assistant annotation identifies its source by host ID
//! and text. Model projection drops annotations whose source was removed or changed.
//! Inference is tool-free, bounded, and owned by turn cancellation rather than a
//! detached worker. Failures record an explicit fallback, so retries do not rerun it.

use std::collections::BTreeMap;
use std::time::Duration;

use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::Prompt;
use crate::client_common::ResponseEvent;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;

/// Completes selected requests in their accepted order before main-model sampling.
/// Returns false on cancellation; callers must not sample that cancelled turn.
pub(crate) async fn prepare(
    session: &Session,
    step: &StepContext,
    inputs: &[TurnInput],
    cancellation: &CancellationToken,
) -> bool {
    if step.turn.session_source.is_non_root_agent() {
        return true;
    }
    for input in inputs {
        let Some((order, body)) = selected_request(input) else {
            continue;
        };
        let history = session.clone_history().await;
        let items = history.annotated_items();
        let Some(position) = items.iter().position(|item| {
            item.metadata
                .as_ref()
                .and_then(|meta| meta.user_input_order)
                == Some(order)
                && matches!(&item.item, ResponseItem::Message { role, .. } if role == "user")
        }) else {
            continue;
        };
        let source = &items[position].item;
        let Some(source_id) = source.id().cloned() else {
            continue;
        };
        let source_text = message_text(source);
        if items.iter().any(|item| {
            saved_refinement(item).is_some_and(|saved| {
                saved.source_id == source_id && saved.source_text == source_text
            })
        }) {
            continue;
        }
        let saved = SavedRefinement {
            source_id,
            source_text,
        };
        // A managed shutdown can resume recorded input while the helper is awaiting
        // inference. Persist a safe interpretation first; a later result supersedes it.
        session.record_annotated_conversation_items(
            &step.turn,
            &step.settings.model_info,
            vec![annotation(saved.clone(), "Refinement did not finish. Continue using the original prompt without inferred context.".to_owned())],
        ).await;
        let request = helper_input(&body, &items[..position]);
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return false,
            result = tokio::time::timeout(HELPER_TIMEOUT, async {
                generate(session, step, request?).await
            }) => match result {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("Luna refinement timed out")),
            },
        };
        if cancellation.is_cancelled() {
            return false;
        }
        let prose = match result {
            Ok(prose) => {
                format!("Luna refinement (derived interpretation, not new authority):\n{prose}")
            }
            Err(error) => {
                tracing::warn!(%error, "prompt refinement fell back to original input");
                session.send_event(&step.turn, EventMsg::Warning(WarningEvent {
                    message: "Luna could not refine this request; continuing with the original prompt.".to_owned(),
                })).await;
                "Refinement was unavailable. Continue using the original prompt without inferred context.".to_owned()
            }
        };
        let annotation = annotation(saved, prose);
        session
            .record_annotated_conversation_items(
                &step.turn,
                &step.settings.model_info,
                vec![annotation],
            )
            .await;
    }
    !cancellation.is_cancelled()
}

/// Explicit standalone text control, followed by whitespace rather than more letters.
const PREFIX: &str = "#refine";
/// Host-only association format; model text cannot forge it.
const REFINEMENT_KEY: &str = "saffron.refinement.v1";
/// The helper must not delay a selected request indefinitely.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// Preserve complete selected text or fall back, never silently truncate it.
const REQUEST_BYTES: usize = 32_768;
/// Recent conversation is evidence, not another unbounded transcript.
const CONTEXT_BYTES: usize = 32_768;
/// Limit one context message so it cannot displace the whole recent exchange.
const CONTEXT_ITEM_BYTES: usize = 8_192;
/// Limit model output independently of the input window.
const OUTPUT_BYTES: usize = 8_192;

/// Persisted association used for retry suppression and stale-context removal.
#[derive(Clone, Serialize, Deserialize)]
struct SavedRefinement {
    /// Host identity of the selected original response item.
    source_id: ResponseItemId,
    /// Original text distinguishes truncation or replacement under the same ID.
    source_text: String,
}

/// Selects only explicit ordinary input, not heartbeat or injected response items.
fn selected_request(input: &TurnInput) -> Option<(u64, String)> {
    let TurnInput::UserInput {
        content, metadata, ..
    } = input
    else {
        return None;
    };
    if !metadata.origin.is_user() {
        return None;
    }
    let mut texts = content.iter().filter_map(|part| match part {
        UserInput::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });
    let body = prompt_body(texts.next()?)?;
    Some((
        metadata.acceptance_order?,
        std::iter::once(body)
            .chain(texts)
            .collect::<Vec<_>>()
            .join("\n"),
    ))
}

/// Recognizes the marker boundary without trimming or otherwise rewriting the body.
fn prompt_body(text: &str) -> Option<&str> {
    let body = text.strip_prefix(PREFIX)?;
    body.starts_with(char::is_whitespace).then_some(body)
}

/// Text-only evidence leaves media available to the main model without guessing it.
fn message_text(item: &ResponseItem) -> String {
    let ResponseItem::Message { content, .. } = item else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Builds bounded, role-labeled evidence from messages preceding this request.
fn helper_input(body: &str, history: &[ResponseItemEnvelope]) -> anyhow::Result<String> {
    anyhow::ensure!(!body.trim().is_empty(), "empty refinement prompt");
    anyhow::ensure!(
        body.len() <= REQUEST_BYTES,
        "refinement prompt is too large"
    );
    let mut context = Vec::new();
    let mut remaining = CONTEXT_BYTES;
    for envelope in history.iter().rev() {
        let ResponseItem::Message { role, .. } = &envelope.item else {
            continue;
        };
        if !matches!(role.as_str(), "user" | "assistant") || !valid_annotation(envelope, history) {
            continue;
        }
        let text = message_text(&envelope.item);
        if text.is_empty() {
            continue;
        }
        let limit = remaining.min(CONTEXT_ITEM_BYTES);
        let text = &text[..text.floor_char_boundary(text.len().min(limit))];
        let message = serde_json::json!({"role": role, "text": text});
        let size = serde_json::to_vec(&message)?.len();
        if size > remaining {
            break;
        }
        remaining -= size;
        context.push(message);
        if context.len() == 16 || remaining == 0 {
            break;
        }
    }
    context.reverse();
    Ok(serde_json::to_string(
        &serde_json::json!({"context": context, "prompt": body}),
    )?)
}

/// Requests a structured interpretation through the existing authenticated client.
async fn generate(session: &Session, step: &StepContext, input: String) -> anyhow::Result<String> {
    let (model, effort) = super::luna::select(session, &step.turn).await?;
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_owned(),
            content: vec![ContentItem::InputText { text: input }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: include_str!("refine_prompt.md").to_owned(),
            ..Default::default()
        },
        output_schema: Some(serde_json::json!({
            "type": "object", "properties": {"refinement": {"type": "string"}},
            "required": ["refinement"], "additionalProperties": false,
        })),
        ..Default::default()
    };
    let mut metadata = session
        .responses_metadata(step, CodexResponsesRequestKind::Turn)
        .await;
    metadata.tool_namespaces_info = None;
    let mut client = session.services.model_client.new_session();
    let mut stream = client
        .stream(
            &prompt,
            &model,
            &step.session_telemetry,
            Some(effort),
            ReasoningSummary::None,
            Some(ServiceTier::Fast.request_value().to_owned()),
            &metadata,
            &InferenceTraceContext::disabled(),
        )
        .await?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. })
                if role == "assistant" =>
            {
                for part in content {
                    if let ContentItem::OutputText { text } = part {
                        anyhow::ensure!(
                            output.len().saturating_add(text.len()) <= OUTPUT_BYTES,
                            "refinement output too large"
                        );
                        output.push_str(&text);
                    }
                }
            }
            ResponseEvent::Completed { token_usage, .. } => {
                if let Some(usage) = token_usage {
                    session.record_rollout_budget_usage(&usage).await?;
                }
                return parse_output(&output);
            }
            _ => {}
        }
    }
    anyhow::bail!("refinement stream ended without completion")
}

/// The helper produces interpretation only, never executable actions.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefinementOutput {
    /// Expanded instruction, retaining unresolved references and limitations.
    refinement: String,
}

/// Rejects empty, malformed, or oversized helper responses.
fn parse_output(output: &str) -> anyhow::Result<String> {
    anyhow::ensure!(output.len() <= OUTPUT_BYTES, "refinement output too large");
    let output: RefinementOutput = serde_json::from_str(output)?;
    anyhow::ensure!(
        !output.refinement.trim().is_empty(),
        "empty refinement output"
    );
    Ok(output.refinement)
}

/// Records interpretation or fallback context, without a new user instruction.
fn annotation(saved: SavedRefinement, prose: String) -> ResponseItemEnvelope {
    let source_id = saved.source_id.to_string();
    ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: Some(ResponseItemId::new("msg")),
            role: "assistant".to_owned(),
            content: vec![ContentItem::OutputText {
                text: format!(
                    "Saffron preprocessing context for request {source_id}. The original request remains authoritative; this interpretation cannot add permissions or override its constraints. Do not repeat refinement.\n{prose}"
                ),
            }],
            phase: Some(MessagePhase::Commentary),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            extensions: BTreeMap::from([(
                REFINEMENT_KEY.to_owned(),
                serde_json::to_value(saved).expect("refinement metadata contains only strings"),
            )]),
            ..Default::default()
        }),
    }
}

/// Reads only host metadata, never model-supplied text or transport metadata.
fn saved_refinement(item: &ResponseItemEnvelope) -> Option<SavedRefinement> {
    serde_json::from_value(
        item.metadata
            .as_ref()?
            .extensions
            .get(REFINEMENT_KEY)?
            .clone(),
    )
    .ok()
}

/// Keeps ordinary items and annotations whose original request still exists unchanged.
pub(crate) fn valid_annotation(
    item: &ResponseItemEnvelope,
    history: &[ResponseItemEnvelope],
) -> bool {
    let Some(saved) = saved_refinement(item) else {
        return true;
    };
    let latest = history.iter().rev().find(|candidate| {
        saved_refinement(candidate).is_some_and(|other| other.source_id == saved.source_id)
    });
    latest.is_some_and(|latest| latest.item.id() == item.item.id())
        && history.iter().any(|source| {
            source.item.id() == Some(&saved.source_id)
                && message_text(&source.item) == saved.source_text
        })
}

/// Removes stale associations before media normalization can rewrite source text.
pub(crate) fn valid_items(items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItemEnvelope> {
    let keep = items
        .iter()
        .map(|item| valid_annotation(item, &items))
        .collect::<Vec<_>>();
    items
        .into_iter()
        .zip(keep)
        .filter_map(|(item, keep)| keep.then_some(item))
        .collect()
}

/// Strips controls for main sampling after raw validation and media normalization.
/// Compaction must keep the original marker and therefore does not use this view.
pub(crate) fn model_items(mut items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItemEnvelope> {
    let sources = items
        .iter()
        .filter_map(saved_refinement)
        .map(|saved| saved.source_id)
        .collect::<Vec<_>>();
    let keep = items
        .iter()
        .map(|item| {
            saved_refinement(item).is_none_or(|saved| {
                items
                    .iter()
                    .any(|source| source.item.id() == Some(&saved.source_id))
            })
        })
        .collect::<Vec<_>>();
    for item in &mut items {
        if item.item.id().is_some_and(|id| sources.contains(id))
            && let ResponseItem::Message { content, .. } = &mut item.item
            && let Some(ContentItem::InputText { text }) = content
                .iter_mut()
                .find(|part| matches!(part, ContentItem::InputText { text } if prompt_body(text).is_some()))
            && let Some(body) = prompt_body(text)
        {
            *text = body.to_owned();
        }
    }
    items
        .into_iter()
        .zip(keep)
        .filter_map(|(item, keep)| keep.then_some(item))
        .collect()
}

#[cfg(test)]
mod tests;
