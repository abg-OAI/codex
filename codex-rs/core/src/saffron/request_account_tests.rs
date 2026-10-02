//! Deterministic contracts for account placement, original recovery and fallback.

use super::*;
use pretty_assertions::assert_eq;

/// Creates ordinary text history with a stable identity for ordering assertions.
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

/// Original requests and intervening messages survive repeated replacement.
#[test]
fn restores_originals_and_intervening_messages() {
    let original = vec![
        message("u1", "user", "Fix parser, run all tests; do not push."),
        message("delivery", "developer", "The coordinator changed the task."),
        message("u2", "user", "Keep that change, restrict edits to CSV."),
        message("checkpoint", "assistant", "Compacted history"),
    ];
    let account = install_account(original.clone(), "Historical account".to_owned());
    let account = model_items(&account).cloned().collect::<Vec<_>>();
    assert_eq!(account.len(), 3);
    assert_eq!(
        account[1..],
        original[1..]
            .iter()
            .filter(|item| !is_request(item))
            .cloned()
            .collect::<Vec<_>>()
    );
    assert!(matches!(&account[0].item, ResponseItem::Message { role, .. } if role == "assistant"));
    assert!(account[0].metadata.as_ref().unwrap().compaction_output);
    assert_eq!(restore_projected_requests(account.clone()), original);

    let again = install_account(
        restore_projected_requests(account),
        "Updated progress".to_owned(),
    );
    assert_eq!(restore_projected_requests(again), original);
}

/// Later input stays after the restored prefix despite inserted initial context.
#[test]
fn restores_around_context_without_reordering_fresh_steer() {
    let user = message("u1", "user", "Earlier work");
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let account = install_account(vec![user.clone(), checkpoint.clone()], "Done".to_owned());
    let mut account = model_items(&account).cloned().collect::<Vec<_>>();
    let context = message("context", "developer", "Persistent instructions");
    account.insert(1, context.clone());
    let steer = message("u2", "user", "New work after the snapshot");
    account.push(steer.clone());
    assert_eq!(
        restore_projected_requests(account),
        vec![context, user, checkpoint, steer]
    );
}

/// Persisted account metadata recovers source bytes without another model call.
#[test]
fn originals_survive_serialized_metadata() -> anyhow::Result<()> {
    let mut user = message("u1", "user", "Use /tmp/data.csv; do not publish.");
    user.metadata = Some(CodexHarnessMetadata {
        user_input_order: Some(7),
        ..Default::default()
    });
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let original = vec![user, checkpoint];
    let account = install_account(
        original.clone(),
        "The user requested a local CSV change.".to_owned(),
    );
    let mut checkpoint: codex_history::CompactedItem =
        serde_json::from_value(serde_json::json!({"message": ""}))?;
    checkpoint.replacement_history = Some(account);
    let checkpoint: codex_history::CompactedItem =
        serde_json::from_str(&serde_json::to_string(&checkpoint)?)?;
    let history = checkpoint.replacement_history.unwrap();
    let projected = model_items(&history).cloned().collect();
    let restored = restore_projected_requests(projected);
    assert_eq!(restored[0], original[0]);
    assert_eq!(
        restored.iter().map(|item| &item.item).collect::<Vec<_>>(),
        original.iter().map(|item| &item.item).collect::<Vec<_>>(),
    );
    Ok(())
}

/// Prior prose informs progress but never substitutes for original request wording.
#[test]
fn helper_receives_original_requests_and_prior_progress_separately() -> anyhow::Result<()> {
    let original = vec![
        message("u1", "user", "Fix parser; do not push."),
        message("checkpoint", "assistant", "Checkpoint"),
    ];
    let previous = install_account(
        original,
        "Parser fix completed; no push permitted.".to_owned(),
    );
    let previous = model_items(&previous).cloned().collect::<Vec<_>>();
    let restored = restore_projected_requests(previous.clone());
    let input: serde_json::Value = serde_json::from_str(&helper_input(&restored, &previous))?;
    assert_eq!(
        input["retained_timeline"][0]["item"]["content"][0]["text"],
        "Fix parser; do not push."
    );
    assert_eq!(
        input["previous_generated_accounts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        input["previous_generated_accounts"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Parser fix completed")
    );
    Ok(())
}

/// Fallback preserves obligations without declaring work complete.
#[test]
fn fallback_quotes_requests_without_inventing_progress() {
    let original = vec![
        message("u1", "user", "Run /tmp/check.sh. Do not push."),
        message("checkpoint", "assistant", "Checkpoint"),
    ];
    let fallback = historical_quotations(&original);
    assert!(fallback.contains("Run /tmp/check.sh. Do not push."));
    assert!(fallback.contains("Progress could not be contextualized"));
    assert_eq!(
        restore_projected_requests(install_account(original.clone(), fallback)),
        original
    );
}

/// Unknown metadata cannot silently remove conversation items.
#[test]
fn invalid_or_unknown_account_metadata_is_preserved() {
    let mut item = message("u1", "user", "Historical account of earlier user requests");
    item.metadata = Some(CodexHarnessMetadata {
        extensions: BTreeMap::from([(
            ACCOUNT_KEY.to_owned(),
            serde_json::json!({"unknown": true}),
        )]),
        ..Default::default()
    });
    assert_eq!(restore_projected_requests(vec![item.clone()]), vec![item]);
}

/// Model generations use numeric ordering and exclude unrelated model variants.
#[test]
fn selects_only_versioned_luna_models() {
    assert!(luna_generation("gpt-10-luna") > luna_generation("gpt-6.1-luna"));
    assert_eq!(luna_generation("gpt-6-luna"), Some(vec![6]));
    assert_eq!(luna_generation("gpt-6-sol"), None);
    assert_eq!(luna_generation("gpt-6-luna-preview"), None);
}

/// Forked history cannot reclassify parent originals as child-local permission.
#[test]
fn restored_fork_requests_keep_inherited_provenance() {
    let user = message("u1", "user", "Permission applies to the parent task.");
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let mut items = install_account(vec![user, checkpoint], "Parent request".to_owned());
    items = model_items(&items).cloned().collect();
    items[0].metadata.as_mut().unwrap().inherited_user_message = true;
    let restored = restore_projected_requests(items);
    assert!(
        restored[0]
            .metadata
            .as_ref()
            .unwrap()
            .inherited_user_message
    );
}

/// User media retains its source role and byte representation beside the account.
#[test]
fn leaves_media_messages_in_place() {
    let user = message("u1", "user", "Review the recording.");
    let mut media = message("u2", "user", "Listen to this.");
    if let ResponseItem::Message { content, .. } = &mut media.item {
        content.push(ContentItem::InputAudio {
            audio_url: "data:audio/wav;base64,AAAA".to_owned(),
        });
    }
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let original = vec![user, media.clone(), checkpoint];
    let account = install_account(original.clone(), "The user requested a review.".to_owned());
    assert_eq!(model_items(&account).nth(1), Some(&media));
    assert_eq!(restore_projected_requests(account), original);
}

/// Rollback removes the latest original and invalidates its combined account.
#[test]
fn rollback_uses_original_turns_and_discards_stale_account() {
    let first = message("u1", "user", "First task");
    let second = message("u2", "user", "Second task");
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let items = install_account(
        vec![first.clone(), second, checkpoint],
        "Both tasks remain.".to_owned(),
    );
    let mut history = crate::context_manager::ContextManager::new();
    history.replace_annotated(items);
    history.drop_last_n_user_turns(1);
    let candidates = crate::compact::collect_annotated_user_messages(
        history.annotated_items(),
        &codex_protocol::AgentPath::root(),
    );
    let mut compacted =
        crate::compact::build_compacted_history(Vec::new(), &candidates, "Checkpoint");
    for item in &mut compacted {
        Session::assign_missing_response_item_id(&mut item.item);
    }
    let compacted = install_account(compacted, "Only the first task remains.".to_owned());
    let projected = model_items(&compacted).cloned().collect();
    let restored = restore_projected_requests(projected);
    assert!(
        !restored
            .iter()
            .any(|item| item.item.id().is_some_and(|id| id.as_str() == "u2"))
    );
    let prompt =
        history.for_prompt_annotated(&[codex_protocol::openai_models::InputModality::Text]);
    assert_eq!(prompt.len(), 1);
    assert!(same_request(&prompt[0].item, &first.item));
    let input: serde_json::Value =
        serde_json::from_str(&helper_input(&compacted, &prompt)).unwrap();
    assert_eq!(input["previous_generated_accounts"], serde_json::json!([]));
}

/// Model token estimation does not charge originals hidden by a compact account.
#[test]
fn token_estimate_matches_projected_model_input() {
    let first = message("u1", "user", &"long original request ".repeat(200));
    let checkpoint = message("checkpoint", "assistant", "Checkpoint");
    let items = install_account(
        vec![first, checkpoint],
        "The requested work is complete.".to_owned(),
    );
    let mut history = crate::context_manager::ContextManager::new();
    history.replace_annotated(items);
    let estimated = history.estimate_token_count_with_base_instructions(&BaseInstructions {
        text: String::new(),
        ..Default::default()
    });
    let visible =
        history.for_prompt_annotated(&[codex_protocol::openai_models::InputModality::Text]);
    let visible_tokens = visible
        .iter()
        .map(|item| crate::context_manager::estimate_item_token_count(&item.item))
        .sum::<i64>();
    assert_eq!(estimated, Some(visible_tokens));
    assert!(visible_tokens < 200);
}

/// Host authorization evidence remains original when model requests become prose.
#[test]
fn compaction_keeps_original_authorization_records() {
    let mut originals = vec![message("u1", "user", "Make local edits; do not deploy.")];
    let mut history = crate::context_manager::ContextManager::new();
    history.record_annotated_items(
        &mut originals,
        codex_protocol::protocol::TruncationPolicy::Tokens(20_000),
    );
    let before = history.retained_context().clone();
    let items = install_account(
        originals,
        "Local edits requested; deployment remains prohibited.".to_owned(),
    );
    history.replace_compacted(items, None);
    assert_eq!(history.retained_context(), &before);
    assert!(
        history
            .raw_items()
            .any(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
    );
    assert!(
        !history
            .for_prompt(&[codex_protocol::openai_models::InputModality::Text])
            .iter()
            .any(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
    );
}

/// Different placements preserve content, multiplicity and order across projection.
#[test]
fn original_history_round_trips_for_interleaved_inputs() {
    for mask in 1_u8..32 {
        let mut original = (0..5)
            .map(|index| {
                message(
                    &format!("item_{index}"),
                    if mask & (1 << index) != 0 {
                        "user"
                    } else {
                        "developer"
                    },
                    &format!("Message {index}"),
                )
            })
            .collect::<Vec<_>>();
        original.push(message("checkpoint", "assistant", "Checkpoint"));
        let stored = install_account(original.clone(), "Historical requests.".to_owned());
        assert_eq!(
            restore_projected_requests(stored.clone()),
            original,
            "host history mask {mask}"
        );
        let borrowed = model_items(&stored).cloned().collect::<Vec<_>>();
        let projected = into_model_items(stored);
        assert_eq!(projected, borrowed, "projection agreement mask {mask}");
        assert_eq!(
            restore_projected_requests(projected),
            original,
            "model history mask {mask}"
        );
    }
}

/// Local summary metadata and both remote markers locate subsequent activity.
#[test]
fn evidence_recognizes_local_and_remote_checkpoints() -> anyhow::Result<()> {
    let checkpoints = [
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "opaque".to_owned(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ContextCompaction {
            id: None,
            encrypted_content: Some("opaque".to_owned()),
            internal_chat_message_metadata_passthrough: None,
        },
        crate::context::ContextualUserFragment::into(crate::context::CompactionSummary::new(
            format!("{}\nSummary", crate::compact::SUMMARY_PREFIX),
        )),
    ];
    for mut checkpoint in checkpoints {
        Session::assign_missing_response_item_id(&mut checkpoint);
        let retained = vec![
            message("u1", "user", "Finish the report."),
            ResponseItemEnvelope::new(checkpoint),
        ];
        let history = install_account(retained.clone(), "The report remains due.".to_owned());
        let mut evidence = into_model_items(history);
        let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
        assert_eq!(input["history_after_previous_account"]["item_count"], 0);
        evidence.push(message("later", "assistant", "The report is now finished."));
        let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
        assert_eq!(input["history_after_previous_account"]["item_count"], 1);
        assert_eq!(
            input["history_after_previous_account"]["items_omitted_from_excerpts"],
            0
        );
        assert_eq!(
            input["available_evidence"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["after_previous_account"],
            true
        );
    }
    Ok(())
}

/// No new raw history after a real checkpoint establishes zero subsequent activity.
#[test]
fn evidence_distinguishes_zero_activity_from_unknown_boundary() -> anyhow::Result<()> {
    let mut checkpoint =
        crate::context::ContextualUserFragment::into(crate::context::CompactionSummary::new(
            format!("{}\nSummary", crate::compact::SUMMARY_PREFIX),
        ));
    Session::assign_missing_response_item_id(&mut checkpoint);
    let retained = vec![
        message("u1", "user", "Finish the report."),
        ResponseItemEnvelope::new(checkpoint),
    ];
    let history = install_account(retained.clone(), "The report remains due.".to_owned());
    let mut evidence = into_model_items(history);
    let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
    assert_eq!(input["history_after_previous_account"]["item_count"], 0);
    assert_eq!(
        input["history_after_previous_account"]["items_omitted_from_excerpts"],
        0
    );

    // The account's compaction_output flag does not establish a checkpoint.
    evidence.pop();
    assert!(evidence[0].metadata.as_ref().unwrap().compaction_output);
    let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
    assert_eq!(
        input["history_after_previous_account"]["item_count"],
        serde_json::Value::Null
    );
    assert_eq!(
        input["history_after_previous_account"]["items_omitted_from_excerpts"],
        serde_json::Value::Null
    );
    Ok(())
}

/// Raw activity counts survive excerpt truncation and filtering of request text.
#[test]
fn evidence_counts_activity_omitted_from_excerpts() -> anyhow::Result<()> {
    let mut checkpoint = ResponseItem::Compaction {
        id: None,
        encrypted_content: "opaque".to_owned(),
        internal_chat_message_metadata_passthrough: None,
    };
    Session::assign_missing_response_item_id(&mut checkpoint);
    let retained = vec![
        message("u1", "user", "Finish the report."),
        ResponseItemEnvelope::new(checkpoint),
    ];
    let history = install_account(retained.clone(), "The report remains due.".to_owned());
    let mut evidence = into_model_items(history);
    evidence.push(message("u2", "user", "Also check spelling."));
    let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
    assert_eq!(input["history_after_previous_account"]["item_count"], 1);
    assert_eq!(
        input["history_after_previous_account"]["items_omitted_from_excerpts"],
        1
    );
    assert_eq!(input["available_evidence"], serde_json::json!([]));
    for index in 0..9 {
        evidence.push(message(
            &format!("later_{index}"),
            "assistant",
            &"detail ".repeat(EVIDENCE_ITEM_BYTES),
        ));
    }
    let input: serde_json::Value = serde_json::from_str(&helper_input(&retained, &evidence))?;
    assert_eq!(input["history_after_previous_account"]["item_count"], 10);
    assert_eq!(
        input["history_after_previous_account"]["items_omitted_from_excerpts"],
        2
    );
    assert_eq!(input["available_evidence"].as_array().unwrap().len(), 8);
    Ok(())
}
