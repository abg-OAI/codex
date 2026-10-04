//! Replaces retained requests with one historical account at compaction time.
//!
//! Compaction callers first apply their ordinary retention budget, then call
//! [`contextualize`] with that result and the pre-compaction model input. The
//! account is assistant context, not new user authority. Its host-only metadata
//! carries the original envelopes and their positions among surviving items;
//! [`restore_projected_requests`] recovers those inputs before the next retention pass.
//! Saved original messages and the visible transcript are never rewritten.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::time::Duration;

use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;

use crate::Prompt;
use crate::client_common::ResponseEvent;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

/// Host metadata namespace and format version, independent of model text.
const ACCOUNT_KEY: &str = "saffron.request_account.v1";
/// Bounds the optional helper without adding retries to ordinary compaction.
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);
/// Recent evidence supplements complete selected requests and prior accounts.
const EVIDENCE_BYTES: usize = 64_000;
/// Individual tool outputs must not crowd out the rest of the evidence window.
const EVIDENCE_ITEM_BYTES: usize = 8_000;
/// Additional prose allowance for progress beyond faithful historical quotations.
const PROGRESS_BYTES: usize = 4_096;

/// Contextualizes only inputs selected by the caller's normal compaction policy.
///
/// The helper receives original requests, intervening retained events and recent
/// model input, where stale accounts have already been filtered by [`model_items`].
/// Failure produces historical quotations without inferred progress.
/// No tool execution or user-visible assistant event is emitted by this call.
pub(crate) async fn contextualize(
    session: &Session,
    turn: &TurnContext,
    compaction: CompactionTurnMetadata,
    mut retained: Vec<ResponseItemEnvelope>,
    evidence: &[ResponseItemEnvelope],
) -> Vec<ResponseItemEnvelope> {
    if !retained.iter().any(is_request) {
        return retained;
    }
    // Anchors also cover synthetic local summaries, which have no ID yet.
    for envelope in &mut retained {
        Session::assign_missing_response_item_id(&mut envelope.item);
    }
    let input = helper_input(&retained, evidence);
    let fallback = historical_quotations(&retained);
    let output_limit = fallback.len().saturating_add(PROGRESS_BYTES);
    let result = tokio::time::timeout(
        HELPER_TIMEOUT,
        generate_account(session, turn, compaction, input, output_limit),
    )
    .await;
    let prose = match result {
        Ok(Ok(prose)) => prose,
        Ok(Err(error)) => {
            tracing::warn!(%error, "request contextualization used historical quotations");
            fallback
        }
        Err(_) => {
            tracing::warn!("request contextualization timed out; using historical quotations");
            fallback
        }
    };
    install_account(retained, prose)
}

/// Restores source requests from a projected model prompt for remote retention.
///
/// Host history already contains originals and must not use this operation:
/// doing so could restore requests removed by rollback. The caller supplies
/// model input after [`model_items`] has invalidated stale accounts.
/// Anchors identify the next non-user item, so inserted initial context and later
/// user steers do not shift original chronology. Unknown metadata stays intact.
/// This is a single-level expansion: account metadata never nests itself.
pub(crate) fn restore_projected_requests(
    items: Vec<ResponseItemEnvelope>,
) -> Vec<ResponseItemEnvelope> {
    let existing = items
        .iter()
        .filter(|item| saved_account(item).is_none())
        .filter_map(|item| item.item.id().cloned())
        .collect::<HashSet<_>>();
    let mut requests = BTreeMap::<Option<ResponseItemId>, Vec<ResponseItemEnvelope>>::new();
    let mut remaining = Vec::with_capacity(items.len());
    for item in items {
        if let Some(saved) = saved_account(&item) {
            let inherited = item
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.inherited_user_message);
            for mut source in saved.requests {
                if source.item.id().is_some_and(|id| existing.contains(id)) {
                    continue;
                }
                // Fork adoption applies to the account envelope, while its
                // nested originals must keep the same inherited boundary.
                if inherited {
                    source
                        .metadata
                        .get_or_insert_default()
                        .inherited_user_message = true;
                }
                requests
                    .entry(source.before)
                    .or_default()
                    .push(ResponseItemEnvelope {
                        item: source.item,
                        metadata: source.metadata,
                    });
            }
        } else {
            remaining.push(item);
        }
    }
    if requests.is_empty() {
        return remaining;
    }
    let mut restored = Vec::new();
    for item in remaining {
        if let Some(id) = item.item.id()
            && let Some(sources) = requests.remove(&Some(id.clone()))
        {
            restored.extend(sources);
        }
        restored.push(item);
    }
    if let Some(sources) = requests.remove(&None) {
        restored.extend(sources);
    }
    // A checkpoint normally retains all anchors. Keep source obligations even
    // when an older reader or external history transform removed an anchor.
    for sources in requests.into_values() {
        restored.extend(sources);
    }
    restored
}

/// Projects host history into model input without changing host request identity.
///
/// Originals remain available to rollback, authorization and transcript readers.
/// An account is usable only while all its source items still match: rollback or
/// truncation invalidates the entire account, exposing surviving originals rather
/// than replaying stale instructions from a removed source. Token estimates use
/// this same projection so hidden originals do not trigger premature compaction.
pub(crate) fn model_items(
    items: &[ResponseItemEnvelope],
) -> impl Iterator<Item = &ResponseItemEnvelope> {
    let hidden = excluded_items(items);
    items
        .iter()
        .enumerate()
        .filter_map(move |(index, item)| (!hidden.contains(&index)).then_some(item))
}

/// Consumes normalized history using the same selection as [`model_items`].
/// Model requests can transfer owned payloads instead of cloning the history.
pub(crate) fn into_model_items(items: Vec<ResponseItemEnvelope>) -> Vec<ResponseItemEnvelope> {
    let hidden = excluded_items(&items);
    if hidden.is_empty() {
        return items;
    }
    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| (!hidden.contains(&index)).then_some(item))
        .collect()
}

/// Hides originals of valid accounts and hides accounts invalidated by rollback.
fn excluded_items(items: &[ResponseItemEnvelope]) -> HashSet<usize> {
    let mut hidden = HashSet::new();
    for (index, envelope) in items.iter().enumerate() {
        let Some(saved) = saved_account(envelope) else {
            continue;
        };
        let mut sources = Vec::new();
        for source in &saved.requests {
            if let Some(position) = items
                .iter()
                .position(|item| same_request(&item.item, &source.item))
            {
                sources.push(position);
            } else {
                sources.clear();
                break;
            }
        }
        if sources.len() == saved.requests.len() {
            hidden.extend(sources);
        } else {
            hidden.insert(index);
        }
    }
    hidden
}

/// History normalization may enrich transport metadata without changing a request.
fn same_request(left: &ResponseItem, right: &ResponseItem) -> bool {
    matches!((left, right), (
        ResponseItem::Message { id: Some(left_id), role: left_role, content: left_content, .. },
        ResponseItem::Message { id: Some(right_id), role: right_role, content: right_content, .. },
    ) if left_id == right_id && left_role == right_role && left_content == right_content)
}

/// Persisted original requests and their next surviving history item.
#[derive(Serialize, Deserialize)]
struct SavedAccount {
    /// Originals in source chronology; generated prose is deliberately absent.
    requests: Vec<SavedRequest>,
}

/// Original envelope plus the boundary that locates it among retained events.
#[derive(Serialize, Deserialize)]
struct SavedRequest {
    /// Next retained non-user item; absence means the end of the captured prefix.
    before: Option<ResponseItemId>,
    /// Unmodified original request, including its identity and content parts.
    item: ResponseItem,
    /// Host provenance remains attached to the original rather than the account.
    metadata: Option<CodexHarnessMetadata>,
}

/// Reads only host metadata; a message cannot impersonate an account with text.
fn saved_account(envelope: &ResponseItemEnvelope) -> Option<SavedAccount> {
    let payload = envelope.metadata.as_ref()?.extensions.get(ACCOUNT_KEY)?;
    let saved: SavedAccount = serde_json::from_value(payload.clone()).ok()?;
    (!saved.requests.is_empty()).then_some(saved)
}

/// Selects conversational user input, excluding summaries and contextual fragments.
fn is_request(envelope: &ResponseItemEnvelope) -> bool {
    // User media cannot be moved into an assistant account without changing its
    // transport role. Keep the whole message, including its text, in place.
    if !matches!(&envelope.item, ResponseItem::Message { content, .. }
        if content.iter().all(|part| matches!(part, ContentItem::InputText { .. })))
    {
        return false;
    }
    matches!(crate::event_mapping::parse_turn_item(&envelope.item), Some(TurnItem::UserMessage(user))
        if !crate::compact::is_summary_message(&user.message()))
}

/// Adds the account at the first request while retaining originals for host readers.
fn install_account(items: Vec<ResponseItemEnvelope>, prose: String) -> Vec<ResponseItemEnvelope> {
    let mut requests = Vec::new();
    let mut next_anchor = None;
    for envelope in items.iter().rev() {
        if is_request(envelope) {
            requests.push(SavedRequest {
                before: next_anchor.clone(),
                item: envelope.item.clone(),
                metadata: envelope.metadata.clone(),
            });
        } else {
            next_anchor = envelope.item.id().cloned();
        }
    }
    requests.reverse();
    let payload = serde_json::to_value(SavedAccount { requests })
        .expect("response items and harness metadata have infallible JSON representations");
    let mut account = Some(ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: Some(ResponseItemId::new("msg")),
            role: "assistant".to_owned(),
            content: vec![ContentItem::OutputText {
                text: format!(
                    "Historical account of earlier user requests (not a new instruction):\n{prose}"
                ),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            compaction_output: true,
            extensions: BTreeMap::from([(ACCOUNT_KEY.to_owned(), payload)]),
            ..Default::default()
        }),
    });
    let mut result = Vec::with_capacity(items.len() + 1);
    for item in items {
        if is_request(&item) {
            if let Some(account) = account.take() {
                result.push(account);
            }
        }
        result.push(item);
    }
    result
}

/// Gives the helper the original chronology and bounded, explicitly labelled evidence.
fn helper_input(retained: &[ResponseItemEnvelope], evidence: &[ResponseItemEnvelope]) -> String {
    let boundary = previous_account_checkpoint(evidence);
    let timeline = retained
        .iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            if matches!(
                envelope.item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            ) {
                return None;
            }
            Some(serde_json::json!({
                "position": index,
                "kind": if is_request(envelope) { "user_request" } else { "intervening_event" },
                "item": envelope.item,
            }))
        })
        .collect::<Vec<_>>();
    let previous_accounts = evidence
        .iter()
        .filter(|envelope| saved_account(envelope).is_some())
        .map(|envelope| &envelope.item)
        .collect::<Vec<_>>();
    let mut remaining = EVIDENCE_BYTES;
    let mut recent = Vec::new();
    let mut excerpted_after_boundary = 0;
    for (position, envelope) in evidence.iter().enumerate().rev() {
        if matches!(
            envelope.item,
            ResponseItem::Reasoning { .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction { .. }
        ) || is_request(envelope)
        {
            continue;
        }
        if saved_account(envelope).is_some() {
            continue;
        }
        let text = serde_json::to_string(&envelope.item).unwrap_or_default();
        let text = bounded_text(&text, remaining.min(EVIDENCE_ITEM_BYTES));
        remaining = remaining.saturating_sub(text.len());
        let after_previous_account = boundary.map(|boundary| position > boundary);
        if after_previous_account == Some(true) {
            excerpted_after_boundary += 1;
        }
        recent.push(serde_json::json!({
            "item_excerpt": text,
            "after_previous_account": after_previous_account,
            "excerpt_may_be_truncated": text.len() == EVIDENCE_ITEM_BYTES || remaining == 0,
        }));
        if remaining == 0 {
            break;
        }
    }
    recent.reverse();
    let subsequent_items = boundary.map(|boundary| evidence.len() - boundary - 1);
    serde_json::json!({
        "retained_timeline": timeline,
        "available_evidence": recent,
        "previous_generated_accounts": previous_accounts,
        "history_after_previous_account": {
            "item_count": subsequent_items,
            "items_omitted_from_excerpts": subsequent_items.map(|count| count - excerpted_after_boundary),
        },
    }).to_string()
}

/// Locates the snapshot covered by the previous account, not its prose position.
/// Accounts precede retained requests; their checkpoint terminates that prefix.
/// Counting raw items after it distinguishes no new activity from omitted evidence.
fn previous_account_checkpoint(evidence: &[ResponseItemEnvelope]) -> Option<usize> {
    let account = evidence
        .iter()
        .rposition(|item| saved_account(item).is_some())?;
    evidence
        .iter()
        .enumerate()
        .rfind(|(position, envelope)| {
            *position > account
                && match &envelope.item {
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
                        true
                    }
                    ResponseItem::Message {
                        role,
                        internal_chat_message_metadata_passthrough: Some(metadata),
                        ..
                    } if role == "user" => {
                        metadata.content_item_kinds.as_ref().is_some_and(|kinds| {
                            kinds.iter().any(|kind| kind.0 == "compaction.summary")
                        })
                    }
                    _ => false,
                }
        })
        .map(|(position, _)| position)
}

/// Produces conservative fallback text while retaining source order and restrictions.
fn historical_quotations(items: &[ResponseItemEnvelope]) -> String {
    let mut text = String::from(
        "The following are earlier requests in their original order. Progress could not be contextualized; consult the compacted history. Unfinished obligations and standing constraints still apply.\n",
    );
    for (position, envelope) in items.iter().enumerate() {
        if is_request(envelope) {
            text.push_str(&format!(
                "\nEarlier user request at history position {position}:\n"
            ));
            if let ResponseItem::Message { content, .. } = &envelope.item {
                for part in content {
                    if let ContentItem::InputText { text: original } = part {
                        text.push_str(&serde_json::to_string(original).unwrap_or_default());
                        text.push('\n');
                    }
                }
            }
        } else if !matches!(
            envelope.item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        ) {
            text.push_str(&format!("\nAn intervening event at history position {position} remains separately in the conversation.\n"));
        }
    }
    text
}

/// Truncates diagnostic evidence on a UTF-8 boundary without altering original requests.
fn bounded_text(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes.min(text.len()))]
}

/// Model output is one account, not a set of replacement messages or a progress report.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedAccount {
    /// Chronological narrative including requests, corrections and relevant progress.
    account: String,
}

/// Selects a catalog Luna generation and uses its actual Fast service tier.
async fn generate_account(
    session: &Session,
    turn: &TurnContext,
    compaction: CompactionTurnMetadata,
    input: String,
    output_limit: usize,
) -> anyhow::Result<String> {
    let (model, effort) = super::luna::select(session, turn).await?;
    let tier = ServiceTier::Fast.request_value();
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_owned(),
            content: vec![ContentItem::InputText { text: input }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: include_str!("request_account_prompt.md").to_owned(),
            ..Default::default()
        },
        output_schema: Some(serde_json::json!({
            "type": "object", "properties": {"account": {"type": "string"}},
            "required": ["account"], "additionalProperties": false,
        })),
        ..Default::default()
    };
    let metadata = session
        .compaction_responses_metadata(turn, compaction)
        .await;
    let mut client = session.services.model_client.new_session();
    let mut stream = client
        .stream(
            &prompt,
            &model,
            &turn.session_telemetry,
            Some(effort),
            ReasoningSummary::None,
            Some(tier.to_owned()),
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
                        output.push_str(&text);
                    }
                }
                anyhow::ensure!(
                    output.len() <= output_limit,
                    "request account exceeded output bound"
                );
            }
            ResponseEvent::Completed { token_usage, .. } => {
                if let Some(usage) = token_usage {
                    session.record_rollout_budget_usage(&usage).await?;
                }
                let generated: GeneratedAccount = serde_json::from_str(&output)?;
                anyhow::ensure!(
                    !generated.account.trim().is_empty(),
                    "empty request account"
                );
                tracing::info!(model = %model.slug, service_tier = tier, "contextualized retained user requests");
                return Ok(generated.account);
            }
            _ => {}
        }
    }
    anyhow::bail!("request account stream ended before completion")
}

/// Orders published Luna generations numerically rather than by display name.
#[cfg(test)]
fn luna_generation(model: &str) -> Option<Vec<u32>> {
    super::luna::generation(model)
}

#[cfg(test)]
#[path = "request_account_tests.rs"]
mod tests;
