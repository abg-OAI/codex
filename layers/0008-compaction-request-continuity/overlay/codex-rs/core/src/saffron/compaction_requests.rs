//! Keeps admitted task inputs eligible for local and remote compaction.
//!
//! Delivery metadata identifies input accepted by the host, not permission to
//! carry out its contents. Parent instructions also belong to the receiving
//! agent, so copied assignments to another recipient are not selected here.
//! Callers retain source order and use their existing user-message token budget:
//! older inputs can fall outside that window, and boundary text can be shortened.
//! Complete envelopes preserve their identities and metadata for rollout replay;
//! opaque or media content is retained whole only when it fits.

use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

use crate::context_manager::estimate_item_token_count;

/// Identifies account and refinement annotations saved by older versions.
///
/// Originals remain separate history items. Omit only the generated assistant
/// item from model input and token accounting; never restore nested requests,
/// which may no longer survive rollback. Text alone cannot identify an annotation.
pub(crate) fn is_retired_annotation(envelope: &ResponseItemEnvelope) -> bool {
    matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant")
        && envelope.metadata.as_ref().is_some_and(|metadata| {
            metadata
                .extensions
                .contains_key("saffron.request_account.v1")
                || metadata.extensions.contains_key("saffron.refinement.v1")
        })
}

/// Input candidates sharing the local compaction budget in history order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompactedInput<'a> {
    /// Ordinary user text follows the established text and media fallback policy.
    User(crate::compact::CompactedUserMessage<'a>),
    /// An admitted delivery or a parent instruction keeps its protocol envelope.
    Request {
        /// Original model-visible request.
        item: &'a ResponseItem,
        /// Persisted host evidence associated with the request.
        metadata: Option<&'a CodexHarnessMetadata>,
    },
}

/// Selects delivered requests and instructions from this agent's ancestors.
pub(crate) fn is_request(
    item: &ResponseItem,
    metadata: Option<&CodexHarnessMetadata>,
    agent_path: &AgentPath,
) -> bool {
    if is_delivery(item, metadata) {
        return true;
    }
    let ResponseItem::AgentMessage {
        author,
        recipient,
        content,
        ..
    } = item
    else {
        return false;
    };
    recipient == agent_path.as_str()
        && recipient
            .strip_prefix(author)
            .is_some_and(|suffix| suffix.starts_with('/'))
        && matches!(content.first(), Some(AgentMessageInputContent::InputText { text })
            if text.starts_with("Message Type: NEW_TASK\n")
                || text.starts_with("Message Type: MESSAGE\n"))
}

/// Recognizes native Saffron input or a delivery with host admission metadata.
///
/// Paired tool results, quoted delivery text, and inherited sender snapshots do
/// not establish this identity. Native Saffron deliveries already have distinct
/// standalone output names, including in history saved before this selector.
/// No lookup of the current turn is needed on resume.
pub(crate) fn is_delivery(item: &ResponseItem, metadata: Option<&CodexHarnessMetadata>) -> bool {
    let ResponseItem::FunctionCallOutput {
        id: Some(id),
        call_id: None,
        name: Some(name),
        namespace: Some(namespace),
        ..
    } = item
    else {
        return false;
    };
    if namespace == "saffron" && name == "send_message_to_thread" {
        return true;
    }
    name == "send_message_to_thread"
        && matches!(namespace.as_str(), "codex_app" | "codex_tui")
        && metadata
            .and_then(|metadata| metadata.sender_user_messages.as_deref())
            .is_some_and(|sender| sender.receiver_message_id == id.as_str())
}

/// Retains a selected request under the caller's remaining token allowance.
///
/// Text uses the existing middle-truncation marker. Structured text retains its
/// content boundaries; media and encrypted content cannot be shortened safely.
/// An omitted boundary request must still consume the caller's remaining budget
/// so older instructions do not displace it.
pub(crate) fn retain(
    mut envelope: ResponseItemEnvelope,
    max_tokens: usize,
) -> Option<ResponseItemEnvelope> {
    if max_tokens == 0 {
        return None;
    }
    if usize::try_from(estimate_item_token_count(&envelope.item)).unwrap_or(usize::MAX)
        <= max_tokens
    {
        return Some(envelope);
    }

    match &mut envelope.item {
        ResponseItem::FunctionCallOutput {
            name,
            namespace,
            output,
            ..
        } => {
            let overhead = approx_token_count(name.as_deref().unwrap_or_default())
                + approx_token_count(namespace.as_deref().unwrap_or_default());
            let mut remaining = max_tokens.checked_sub(overhead)?;
            if remaining == 0 {
                return None;
            }
            match &mut output.body {
                FunctionCallOutputBody::Text(text) => shorten_text(text, &mut remaining),
                FunctionCallOutputBody::ContentItems(content) => {
                    if !content
                        .iter()
                        .all(|part| matches!(part, FunctionCallOutputContentItem::InputText { .. }))
                    {
                        return None;
                    }
                    for part in content.iter_mut() {
                        if let FunctionCallOutputContentItem::InputText { text } = part {
                            shorten_text(text, &mut remaining);
                        }
                    }
                    content.retain(|part| {
                        matches!(part,
                        FunctionCallOutputContentItem::InputText { text } if !text.is_empty())
                    });
                }
            }
        }
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => {
            if !content
                .iter()
                .all(|part| matches!(part, AgentMessageInputContent::InputText { .. }))
            {
                return None;
            }
            let overhead = approx_token_count(author) + approx_token_count(recipient);
            let mut remaining = max_tokens.checked_sub(overhead)?;
            if remaining == 0 {
                return None;
            }
            for part in content.iter_mut() {
                if let AgentMessageInputContent::InputText { text } = part {
                    shorten_text(text, &mut remaining);
                }
            }
            content.retain(|part| {
                matches!(part,
                AgentMessageInputContent::InputText { text } if !text.is_empty())
            });
        }
        _ => return None,
    }
    if let Some(metadata) = &mut envelope.metadata {
        metadata.mark_retained_sources_incomplete();
    }
    Some(envelope)
}

/// Spends the remaining text allowance without flattening content parts.
fn shorten_text(text: &mut String, remaining: &mut usize) {
    if *remaining == 0 {
        text.clear();
        return;
    }
    let tokens = approx_token_count(text);
    if tokens > *remaining {
        *text = truncate_text(text, TruncationPolicy::Tokens(*remaining));
    }
    *remaining = remaining.saturating_sub(tokens);
}

#[cfg(test)]
pub(crate) mod tests {
    //! Synthetic delivery fixtures and request eligibility regressions.

    use super::*;
    use codex_history::SenderUserMessages;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;
    use pretty_assertions::assert_eq;
    use test_case::test_case;

    /// Models a standalone request and the host evidence recorded on admission.
    pub(crate) fn admitted_delivery(text: &str, order: u64) -> ResponseItemEnvelope {
        let id = ResponseItemId::with_suffix("fco", format!("delivery_{order}"));
        let turn_id = format!("delivery_turn_{order}");
        ResponseItemEnvelope {
            metadata: Some(CodexHarnessMetadata {
                user_input_order: Some(order),
                sender_user_messages: Some(Box::new(SenderUserMessages {
                    receiver_turn_id: turn_id.clone(),
                    receiver_message_id: id.to_string(),
                    text: "Host: Sender context is unavailable.".to_owned(),
                })),
                ..Default::default()
            }),
            item: ResponseItem::FunctionCallOutput {
                id: Some(id),
                call_id: None,
                name: Some("send_message_to_thread".to_owned()),
                namespace: Some("codex_app".to_owned()),
                output: FunctionCallOutputPayload::from_text(text.to_owned()),
                internal_chat_message_metadata_passthrough: Some(
                    InternalChatMessageMetadataPassthrough {
                        turn_id: Some(turn_id),
                        ..Default::default()
                    },
                ),
            },
        }
    }

    /// Admission evidence must name this delivery, even after a turn changes.
    #[test]
    fn delivery_identity_excludes_ordinary_tool_output_and_quoted_requests() {
        let mut delivery = admitted_delivery("Investigate the orchard export.", 2);
        assert!(is_delivery(&delivery.item, delivery.metadata.as_ref()));
        assert!(!is_delivery(&delivery.item, None));

        let metadata = delivery.metadata.as_mut().unwrap();
        metadata
            .sender_user_messages
            .as_mut()
            .unwrap()
            .receiver_message_id = "fco_another_delivery".to_owned();
        assert!(!is_delivery(&delivery.item, delivery.metadata.as_ref()));

        let mut paired_output = admitted_delivery("Investigate the orchard export.", 2);
        if let ResponseItem::FunctionCallOutput { call_id, .. } = &mut paired_output.item {
            *call_id = Some("tool_call".to_owned());
        }
        assert!(!is_delivery(
            &paired_output.item,
            paired_output.metadata.as_ref()
        ));
    }

    /// Native delivery identity survives replay without sender snapshot metadata.
    #[test]
    fn native_delivery_excludes_paired_outputs_and_other_tools() {
        let mut delivery = admitted_delivery("Inspect the orchard export.", 2).item;
        if let ResponseItem::FunctionCallOutput { namespace, .. } = &mut delivery {
            *namespace = Some("saffron".to_owned());
        }
        assert!(is_delivery(&delivery, None));
        let restored: ResponseItem =
            serde_json::from_str(&serde_json::to_string(&delivery).unwrap()).unwrap();
        assert!(is_delivery(&restored, None));
        if let ResponseItem::FunctionCallOutput { call_id, .. } = &mut delivery {
            *call_id = Some("paired_call".to_owned());
        }
        assert!(!is_delivery(&delivery, None));
        if let ResponseItem::FunctionCallOutput { call_id, name, .. } = &mut delivery {
            *call_id = None;
            *name = Some("await_exec".to_owned());
        }
        assert!(!is_delivery(&delivery, None));
    }

    /// Parent instructions are addressed to this agent; reports are not requests.
    #[test_case("/root", "/root/fruit", "NEW_TASK", true; "opening assignment")]
    #[test_case("/root", "/root/fruit", "MESSAGE", true; "parent steer")]
    #[test_case("/root", "/root/vegetables", "NEW_TASK", false; "sibling assignment")]
    #[test_case("/root/fruit/child", "/root/fruit", "MESSAGE", false; "child report")]
    #[test_case("/root/vegetables", "/root/fruit", "MESSAGE", false; "peer report")]
    #[test_case("/root", "/root/fruit", "FINAL_ANSWER", false; "completion report")]
    fn parent_request_scope(author: &str, recipient: &str, kind: &str, expected: bool) {
        let item = ResponseItem::AgentMessage {
            id: None,
            author: author.to_owned(),
            recipient: recipient.to_owned(),
            content: vec![AgentMessageInputContent::InputText {
                text: format!("Message Type: {kind}\nPayload:\nInspect the orchard."),
            }],
            internal_chat_message_metadata_passthrough: None,
        };
        let agent_path = AgentPath::try_from("/root/fruit").unwrap();
        assert_eq!(is_request(&item, None, &agent_path), expected);
    }

    /// Boundary text keeps its delivery identity and a visible truncation marker.
    #[test]
    fn boundary_delivery_keeps_identity_after_shortening() {
        let original = admitted_delivery(&"inspect the orchard ".repeat(200), 2);
        let retained = retain(original.clone(), 100).unwrap();
        assert_ne!(retained.item, original.item);
        assert_eq!(retained.item.id(), original.item.id());
        assert_eq!(retained.metadata, original.metadata);
        assert!(is_delivery(&retained.item, retained.metadata.as_ref()));
        let ResponseItem::FunctionCallOutput { output, .. } = retained.item else {
            panic!("delivery remains a standalone output");
        };
        assert!(output.body.to_text().unwrap().contains("truncated"));
    }

    /// Encrypted assignments survive whole, or consume the boundary without corruption.
    #[test]
    fn encrypted_assignment_is_never_partially_rewritten() {
        let assignment = ResponseItemEnvelope::new(ResponseItem::AgentMessage {
            id: None,
            author: "/root".to_owned(),
            recipient: "/root/fruit".to_owned(),
            content: vec![
                AgentMessageInputContent::InputText {
                    text: "Message Type: NEW_TASK\nPayload:\n".to_owned(),
                },
                AgentMessageInputContent::EncryptedContent {
                    encrypted_content: "opaque".repeat(1000),
                },
            ],
            internal_chat_message_metadata_passthrough: None,
        });
        assert_eq!(retain(assignment.clone(), 10_000), Some(assignment.clone()));
        assert_eq!(retain(assignment, 100), None);
    }
}
