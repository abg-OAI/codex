//! Fork-boundary regressions using the upstream agent-control harness.

use super::AgentControlHarness;
use super::assistant_message;
use super::text_input;
use super::user_message;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::saffron::goal_supervisor::HELPER_ROLE_NAME;
use codex_protocol::AgentPath;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn paginated_supervisor_inherits_agent_completion_after_a_silent_turn() {
    let harness = AgentControlHarness::new().await;
    let (parent_id, parent) = harness.start_paginated_thread().await;
    let completed_scan = InterAgentCommunication::new(
        AgentPath::try_from("/root/scanner").expect("scanner path"),
        AgentPath::root(),
        Vec::new(),
        "Scan completed. No changes. Next check tomorrow.".to_string(),
        /*trigger_turn*/ false,
    );
    parent
        .session
        .inject_no_new_turn(
            vec![
                user_message("Check for releases daily; keep unchanged results silent."),
                completed_scan.to_model_input_item(),
                assistant_message("", Some(MessagePhase::FinalAnswer)),
            ],
            /*current_turn_context*/ None,
        )
        .await;
    let parent_evidence = parent
        .session
        .clone_history()
        .await
        .raw_items()
        .filter(|item| matches!(item, ResponseItem::AgentMessage { .. }))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(parent_evidence.len(), 1);

    let helper = harness
        .control
        .spawn_hidden_agent_with_metadata(
            harness.config.clone(),
            text_input("Decide whether the completed scan needs another turn."),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent_id,
                depth: 1,
                agent_path: Some(
                    AgentPath::root()
                        .join(HELPER_ROLE_NAME)
                        .expect("supervisor path"),
                ),
                agent_nickname: None,
                agent_role: Some(HELPER_ROLE_NAME.to_string()),
            }),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some("saffron-goal-supervisor".to_string()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                parent_thread_id: Some(parent_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn supervisor");
    let child = harness
        .manager
        .get_thread(helper.thread_id)
        .await
        .expect("supervisor thread");
    let inherited_evidence = child
        .session
        .clone_history()
        .await
        .raw_items()
        .filter(|item| matches!(item, ResponseItem::AgentMessage { .. }))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        strip_response_item_ids(&inherited_evidence),
        strip_response_item_ids(&parent_evidence),
        "supervision must retain scanner results independently of the parent final answer"
    );

    harness
        .control
        .shutdown_live_agent(helper.thread_id)
        .await
        .expect("retire supervisor");
}
