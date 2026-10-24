//! Client transcript presentation for Saffron's incoming thread messages.
//!
//! Both live item notifications and rebuilt history use this conversion. Core
//! keeps the original tool output for persistence and model input; this module
//! only supplies an attributed commentary row to clients. Unrecognized payloads
//! remain tool outputs rather than acquiring assistant-message semantics.

use crate::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::items::FunctionCallOutputItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::MessagePhase;
use serde::Deserialize;

/// Presents a valid incoming Saffron message without changing its item identity.
pub(crate) fn thread_message(output: FunctionCallOutputItem) -> ThreadItem {
    let message = if output.namespace.as_deref() == Some("saffron")
        && output.name == "send_message_to_thread"
        && let FunctionCallOutputBody::Text(body) = &output.output
    {
        serde_json::from_str::<Message>(body).ok()
    } else {
        None
    };
    if let Some(message) = message {
        let sender = message
            .source_thread_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| message.source_thread_id.to_string());
        let label = escape_link_label(&sender);
        return ThreadItem::AgentMessage {
            id: output.id,
            text: format!(
                "**Message from [{label}](thread://{})**\n\n{}",
                message.source_thread_id, message.input
            ),
            phase: Some(MessagePhase::Commentary),
            memory_citation: None,
            delivery: None,
            questions: None,
        };
    }
    ThreadItem::FunctionCallOutput {
        id: output.id,
        name: output.name,
        namespace: output.namespace,
        output: output.output,
    }
}

/// Saffron-owned envelope; older messages have only identity and input.
#[derive(Deserialize)]
struct Message {
    /// Runtime-derived sender; parsing also constrains the link destination.
    source_thread_id: ThreadId,
    /// Display name captured by the sender runtime when available.
    source_thread_name: Option<String>,
    /// Original message body, including its Markdown formatting.
    input: String,
}

/// Keeps a thread name inside one Markdown link label and one display line.
fn escape_link_label(name: &str) -> String {
    let mut escaped = String::new();
    for character in name.chars() {
        if character.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(if character.is_whitespace() {
            ' '
        } else {
            character
        });
    }
    escaped
}

#[cfg(test)]
mod tests;
