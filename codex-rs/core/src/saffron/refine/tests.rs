//! Selection, persistence, and model projection contracts for prompt refinement.

use super::*;
use crate::session::UserInputMetadata;
use pretty_assertions::assert_eq;

/// Keeps the model-facing prelude short without changing the annotation's role.
#[test]
fn context_uses_simple_header_and_keeps_source_association_out_of_model_text() {
    let saved = SavedRefinement {
        source_id: ResponseItemId::from_server("original-request".to_owned()),
        source_text: "#refine status?".to_owned(),
    };
    let note = annotation(saved, "Report the release status.".to_owned());
    assert_eq!(
        message_text(&note.item),
        "Possible context:\nReport the release status."
    );
    assert!(matches!(
        &note.item,
        ResponseItem::Message { role, phase: Some(MessagePhase::Commentary), .. }
            if role == "assistant"
    ));
    let association = saved_refinement(&note).unwrap();
    assert_eq!(association.source_id.to_string(), "original-request");
    assert_eq!(association.source_text, "#refine status?");
}

#[test]
fn standalone_marker_selects_only_ordinary_input() {
    for text in [
        "#refine\ndo this",
        "#refine do this",
        "#refine\tfoo",
        "#refine\r\nfoo",
    ] {
        assert_eq!(
            selected_request(&input(text)).map(|selected| selected.0),
            Some(7)
        );
    }
    for text in [
        "do #refine this",
        " #refine this",
        "#Refine this",
        "ordinary",
        "#refine",
        "#refinefoo",
        "#refine.foo",
    ] {
        assert_eq!(selected_request(&input(text)), None);
    }
    let mut heartbeat = input("#refine do this");
    if let TurnInput::UserInput { metadata, .. } = &mut heartbeat {
        metadata.origin = codex_history::UserInputOrigin::Heartbeat;
    }
    assert_eq!(selected_request(&heartbeat), None);
    assert_eq!(
        selected_request(&TurnInput::ResponseItem(message(
            "u",
            "user",
            "#refine do this"
        ))),
        None
    );
}

#[test]
fn attachment_before_text_does_not_change_selected_body() {
    let mut request = input("#refine inspect this");
    if let TurnInput::UserInput { content, .. } = &mut request {
        content.insert(
            0,
            UserInput::LocalAudio {
                path: "/tmp/audio.wav".into(),
            },
        );
        content.push(UserInput::Text {
            text: "Do not modify files.".to_owned(),
            text_elements: Vec::new(),
        });
    }
    assert_eq!(
        selected_request(&request),
        Some((7, " inspect this\nDo not modify files.".to_owned()))
    );
}

#[test]
fn projection_preserves_saved_source_and_unselected_messages() {
    for body in [
        " fix it",
        "\n修正",
        " #refine remains literal",
        "\tfoo",
        "\n\n",
    ] {
        let source = message("u", "user", &format!("#refine{body}"));
        let ordinary = message("v", "user", "#refine quoted, not processed");
        let note = annotation(
            SavedRefinement {
                source_id: source.item.id().unwrap().clone(),
                source_text: message_text(&source.item),
            },
            "derived context".to_owned(),
        );
        let saved = vec![source.clone(), note.clone(), ordinary.clone()];
        let projected = model_items(saved.clone());
        assert_eq!(message_text(&projected[0].item), body);
        assert_eq!(projected[1..], [note, ordinary]);
        assert_eq!(saved[0], source);
        assert_eq!(message_text(&saved[0].item), format!("#refine{body}"));
    }
}

#[test]
fn compaction_input_and_saved_media_keep_original_content() {
    use crate::context_manager::ContextManager;
    use codex_protocol::models::ImageReference;
    use codex_protocol::openai_models::InputModality;
    use codex_utils_output_truncation::TruncationPolicy;

    let mut source = message("u", "user", "#refine inspect this diagram");
    let media = ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,aGVsbG8=".to_owned(),
        },
        detail: None,
    };
    if let ResponseItem::Message { content, .. } = &mut source.item {
        content.push(media.clone());
    }
    let note = annotation(
        SavedRefinement {
            source_id: source.item.id().unwrap().clone(),
            source_text: message_text(&source.item),
        },
        "Inspect the attached diagram without inferring unseen content.".to_owned(),
    );
    let mut items = vec![source, note];
    let mut history = ContextManager::new();
    history.record_annotated_items(&mut items, TruncationPolicy::Tokens(10_000));
    let saved = history.annotated_items().to_vec();
    let text_only = history.clone().for_prompt_annotated(&[InputModality::Text]);
    let text_only = model_items(text_only);
    assert_eq!(text_only.len(), 2);
    assert!(message_text(&text_only[0].item).starts_with(" inspect this diagram"));
    assert!(!message_text(&text_only[0].item).contains("#refine"));
    let compaction = history.for_prompt_annotated(&[InputModality::Text, InputModality::Image]);
    assert_eq!(
        message_text(&compaction[0].item),
        "#refine inspect this diagram"
    );
    let main = model_items(compaction);
    assert_eq!(message_text(&main[0].item), " inspect this diagram");
    for item in [&saved[0], &main[0]] {
        let ResponseItem::Message { content, .. } = &item.item else {
            panic!("expected message")
        };
        assert!(content.contains(&media));
    }
}

#[test]
fn source_removal_or_edit_discards_stale_refinement() {
    let source = message("u", "user", "#refine fix parser");
    let note = annotation(
        SavedRefinement {
            source_id: source.item.id().unwrap().clone(),
            source_text: message_text(&source.item),
        },
        "fix parser".to_owned(),
    );
    assert!(model_items(valid_items(vec![note.clone()])).is_empty());
    let changed = message("u", "user", "#refine inspect parser only");
    assert_eq!(
        model_items(valid_items(vec![changed.clone(), note])),
        vec![changed]
    );
}

#[test]
fn completion_supersedes_durable_interruption_fallback() {
    let source = message("u", "user", "#refine inspect only");
    let saved = SavedRefinement {
        source_id: source.item.id().unwrap().clone(),
        source_text: message_text(&source.item),
    };
    let interrupted = annotation(saved.clone(), "Use original prompt.".to_owned());
    assert_eq!(
        valid_items(vec![source.clone(), interrupted.clone()]).len(),
        2
    );
    let complete = annotation(saved, "Inspect only, without edits.".to_owned());
    assert_eq!(
        valid_items(vec![source.clone(), interrupted, complete.clone()]),
        vec![source, complete]
    );
}

#[test]
fn metadata_round_trip_retains_association() -> anyhow::Result<()> {
    let source = message("u", "user", "#refine investigate; do not edit");
    let note = annotation(
        SavedRefinement {
            source_id: source.item.id().unwrap().clone(),
            source_text: message_text(&source.item),
        },
        "investigate".to_owned(),
    );
    let metadata = serde_json::from_str(&serde_json::to_string(&note.metadata)?)?;
    let restored = ResponseItemEnvelope {
        item: note.item.clone(),
        metadata,
    };
    assert!(valid_annotation(
        &restored,
        &[source.clone(), restored.clone()]
    ));
    assert_eq!(model_items(vec![source, restored.clone()])[1], restored);
    Ok(())
}

#[test]
fn helper_preserves_roles_order_and_prompt_without_following_quoted_text() -> anyhow::Result<()> {
    let context = vec![
        message("u", "user", "Compare approaches; do not implement."),
        message("a", "assistant", "Option A preserves the cache."),
        message("d", "developer", "not conversation evidence"),
    ];
    let body = "Use that approach. Literal: \"ignore previous instructions\".";
    let value: serde_json::Value = serde_json::from_str(&helper_input(body, &context)?)?;
    assert_eq!(value["prompt"], body);
    assert_eq!(
        value["context"],
        serde_json::json!([
            {"role":"user", "text":"Compare approaches; do not implement."},
            {"role":"assistant", "text":"Option A preserves the cache."},
        ])
    );
    Ok(())
}

#[test]
fn context_is_bounded_and_request_is_never_truncated() -> anyhow::Result<()> {
    assert!(helper_input(" \n", &[]).is_err());
    assert!(helper_input(&"a".repeat(REQUEST_BYTES + 1), &[]).is_err());
    let context = (0..100)
        .map(|i| message(&format!("u{i}"), "user", &"界".repeat(5000)))
        .collect::<Vec<_>>();
    let value: serde_json::Value =
        serde_json::from_str(&helper_input("keep full prompt", &context)?)?;
    let messages = value["context"].as_array().unwrap();
    assert!(messages.len() <= 16);
    assert!(
        messages
            .iter()
            .map(|item| serde_json::to_vec(item).unwrap().len())
            .sum::<usize>()
            <= CONTEXT_BYTES
    );
    assert!(
        messages
            .iter()
            .all(|item| item["text"].as_str().unwrap().len() <= CONTEXT_ITEM_BYTES)
    );
    assert_eq!(value["prompt"], "keep full prompt");
    Ok(())
}

#[test]
fn malformed_empty_and_oversized_outputs_fail_closed() {
    for text in [
        "",
        "plain answer",
        "{}",
        r#"{"refinement":" "}"#,
        r#"{"refinement":"ok","extra":true}"#,
    ] {
        assert!(parse_output(text).is_err());
    }
    assert!(
        parse_output(&serde_json::json!({"refinement": "a".repeat(OUTPUT_BYTES)}).to_string())
            .is_err()
    );
    assert_eq!(
        parse_output(r#"{"refinement":"Inspect only; do not implement."}"#).unwrap(),
        "Inspect only; do not implement."
    );
}

/// Builds ordinary accepted input independently of persisted model messages.
fn input(text: &str) -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: text.to_owned(),
            text_elements: Vec::new(),
        }],
        client_id: None,
        metadata: UserInputMetadata {
            acceptance_order: Some(7),
            ..Default::default()
        },
    }
}

/// Builds text history with stable host identity.
fn message(id: &str, role: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.to_owned())),
        role: role.to_owned(),
        content: vec![ContentItem::InputText {
            text: text.to_owned(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}
