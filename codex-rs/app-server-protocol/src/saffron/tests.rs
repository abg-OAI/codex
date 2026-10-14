//! Checks attributed presentation across live events and persisted history.

use super::*;
use crate::ServerNotification;
use crate::build_turns_from_rollout_items;
use crate::item_event_to_server_notification;
use crate::project_rollout_line;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use serde_json::json;

/// A received message has identical attribution live and after history reload.
#[test]
fn live_and_replayed_message_keep_identity_and_commentary() {
    let sender = ThreadId::new();
    let receiver = ThreadId::new();
    let output = incoming_message(json!({
        "source_thread_id": sender,
        "source_thread_name": "worker [c]",
        "input": "First line.\n\n**Second line.**"
    }));
    let original = serde_json::to_value(&output).unwrap();
    let started = EventMsg::ItemStarted(ItemStartedEvent {
        thread_id: receiver,
        turn_id: "receiving-turn".into(),
        item: TurnItem::FunctionCallOutput(output.clone()),
        started_at_ms: 1000,
    });
    let completed = EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: receiver,
        turn_id: "receiving-turn".into(),
        item: TurnItem::FunctionCallOutput(output.clone()),
        started_at_ms: Some(1000),
        completed_at_ms: 1001,
    });
    let ServerNotification::ItemStarted(live_start) =
        item_event_to_server_notification(started.clone(), &receiver.to_string(), "receiving-turn")
    else {
        panic!("expected start notification")
    };
    let ServerNotification::ItemCompleted(live_end) = item_event_to_server_notification(
        completed.clone(),
        &receiver.to_string(),
        "receiving-turn",
    ) else {
        panic!("expected completion notification")
    };
    let turns = build_turns_from_rollout_items(&[
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_attribution: None,
            turn_id: "receiving-turn".into(),
            root_turn_id: None,
            trace_id: None,
            started_at: Some(1),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        RolloutItem::EventMsg(started),
        RolloutItem::EventMsg(completed),
    ]);
    let expected = ThreadItem::AgentMessage {
        id: "incoming-message".into(),
        text: format!(
            "**Message from [worker \\[c\\]](thread://{sender})**\n\nFirst line.\n\n**Second line.**"
        ),
        phase: Some(MessagePhase::Commentary),
        memory_citation: None,
        delivery: None,
        questions: None,
    };
    assert_eq!(live_start.item, expected);
    assert_eq!(live_end.item, expected);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].items, vec![expected]);
    assert_eq!(serde_json::to_value(output).unwrap(), original);
}

/// Messages saved before display names were included still link to their sender.
#[test]
fn legacy_envelope_uses_sender_identity() {
    let sender = ThreadId::from_string("01a10009-cf4e-7650-bb64-c0fd72a05f77").unwrap();
    let item = thread_message(incoming_message(json!({
        "source_thread_id": sender, "input": "hello"
    })));
    let ThreadItem::AgentMessage { text, .. } = item else {
        panic!("expected message")
    };
    assert_eq!(
        text,
        format!(
            "**Message from [01a10009\\-cf4e\\-7650\\-bb64\\-c0fd72a05f77](thread://{sender})**\n\nhello"
        )
    );
}

/// Newly materialized history retains the same row through storage serialization.
#[test]
fn paginated_history_stores_the_presented_message() {
    let output = incoming_message(json!({
        "source_thread_id": ThreadId::new(), "input": "persisted message"
    }));
    let expected = thread_message(output.clone());
    let line = RolloutLine {
        timestamp: "2026-10-03T04:30:00Z".into(),
        ordinal: Some(3),
        item: RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: ThreadId::new(),
            turn_id: "receiving-turn".into(),
            item: TurnItem::FunctionCallOutput(output),
            started_at_ms: Some(1000),
            completed_at_ms: 1001,
        })),
    };
    let changes = project_rollout_line(&line);
    assert_eq!(changes.changed_items.len(), 1);
    let serialized = serde_json::to_string(&changes.changed_items[0].item).unwrap();
    let reloaded: ThreadItem = serde_json::from_str(&serialized).unwrap();
    assert_eq!(reloaded, expected);
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = line.item else {
        panic!("expected original completion event")
    };
    assert!(matches!(event.item, TurnItem::FunctionCallOutput(_)));
}

/// A display name cannot close its link or add a second Markdown block.
#[test]
fn sender_name_is_literal_but_message_markdown_is_preserved() {
    let sender = ThreadId::new();
    let item = thread_message(incoming_message(json!({
        "source_thread_id": sender,
        "source_thread_name": "worker](https://example.test)\n**other**",
        "input": "`body`"
    })));
    let ThreadItem::AgentMessage { text, .. } = item else {
        panic!("expected message")
    };
    assert_eq!(
        text,
        format!(
            "**Message from [worker\\]\\(https\\:\\/\\/example\\.test\\) \\*\\*other\\*\\*](thread://{sender})**\n\n`body`"
        )
    );
}

/// Invalid or unrelated tool output keeps its original representation and body.
#[test]
fn other_outputs_are_not_reclassified() {
    for (namespace, name, body) in [
        (
            "codex_app",
            "send_message_to_thread",
            json!({"source_thread_id": ThreadId::new(), "input": "relay"}).to_string(),
        ),
        (
            "saffron",
            "fork_thread",
            json!({"source_thread_id": ThreadId::new(), "input": "receipt"}).to_string(),
        ),
        ("saffron", "send_message_to_thread", "not json".into()),
        (
            "saffron",
            "send_message_to_thread",
            json!({"source_thread_id": "bad-id](https://example.test)", "input": "body"})
                .to_string(),
        ),
        (
            "saffron",
            "send_message_to_thread",
            json!({"source_thread_id": ThreadId::new()}).to_string(),
        ),
    ] {
        let output = FunctionCallOutputItem {
            id: "unchanged".into(),
            name: name.into(),
            namespace: Some(namespace.into()),
            output: FunctionCallOutputBody::Text(body.clone()),
        };
        assert_eq!(
            thread_message(output),
            ThreadItem::FunctionCallOutput {
                id: "unchanged".into(),
                name: name.into(),
                namespace: Some(namespace.into()),
                output: FunctionCallOutputBody::Text(body),
            }
        );
    }
}

/// Constructs a delivered envelope at the boundary owned by the sender runtime.
fn incoming_message(body: serde_json::Value) -> FunctionCallOutputItem {
    FunctionCallOutputItem {
        id: "incoming-message".into(),
        name: "send_message_to_thread".into(),
        namespace: Some("saffron".into()),
        output: FunctionCallOutputBody::Text(body.to_string()),
    }
}
