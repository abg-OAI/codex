use super::*;
use codex_history::CodexHarnessMetadata;

/// Tool-output pretrimming must not erase an admitted request before compaction.
#[test]
fn rewritten_output_excludes_admitted_delivery() {
    let delivery = crate::saffron::compaction_requests::tests::admitted_delivery(
        "Investigate the orchard export.",
        2,
    );
    assert!(rewritten_output_for_context_window(&delivery).is_none());

    let mut ordinary_output = delivery;
    ordinary_output.metadata = None;
    assert!(rewritten_output_for_context_window(&ordinary_output).is_some());
}

#[test]
fn rewritten_output_preserves_harness_metadata() {
    let envelope = ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("large output".repeat(100)),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata::default()),
    };

    let rewritten = rewritten_output_for_context_window(&envelope)
        .expect("function output should be rewritten");

    assert_eq!(rewritten.metadata, envelope.metadata);
    assert_ne!(rewritten.item, envelope.item);
}
