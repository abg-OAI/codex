use codex_extension_api::ExtensionData;
use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

use super::*;
use crate::agent::types::MessageDeliveryMode;

#[test]
fn completion_delivery_choice_is_limited_to_spawned_v2_subagents() {
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: Some(AgentPath::root().join("worker").expect("worker path")),
        agent_nickname: None,
        agent_role: None,
    });

    assert!(can_choose_delivery(MultiAgentVersion::V2, &source));
    assert!(!can_choose_delivery(MultiAgentVersion::V1, &source));
    assert!(!can_choose_delivery(
        MultiAgentVersion::V2,
        &SessionSource::default()
    ));
}

#[test]
fn deferred_success_queues_the_terminal_result() {
    let turn_store = ExtensionData::new("turn-1");
    set_delivery(&turn_store, CompletionDelivery::DeferToParent);

    assert!(matches!(
        terminal_delivery_mode(
            &turn_store,
            &AgentStatus::Completed(Some("done".to_string()))
        ),
        MessageDeliveryMode::QueueOnly
    ));
}

#[test]
fn abnormal_outcomes_wake_after_a_defer_request() {
    let turn_store = ExtensionData::new("turn-1");
    set_delivery(&turn_store, CompletionDelivery::DeferToParent);

    assert!(matches!(
        terminal_delivery_mode(&turn_store, &AgentStatus::Errored("failed".to_string())),
        MessageDeliveryMode::TriggerTurn
    ));
}

#[test]
fn delivery_choice_is_scoped_to_one_turn() {
    let first_turn_store = ExtensionData::new("turn-1");
    let next_turn_store = ExtensionData::new("turn-2");
    set_delivery(&first_turn_store, CompletionDelivery::DeferToParent);

    assert!(matches!(
        terminal_delivery_mode(
            &first_turn_store,
            &AgentStatus::Completed(Some("first".to_string()))
        ),
        MessageDeliveryMode::QueueOnly
    ));
    assert!(matches!(
        terminal_delivery_mode(
            &next_turn_store,
            &AgentStatus::Completed(Some("second".to_string()))
        ),
        MessageDeliveryMode::TriggerTurn
    ));
}

#[test]
fn sibling_turns_retain_shared_ancestors_until_both_finish() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();
    let parent_thread_id = ThreadId::new();

    let first = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("first child should retain ancestors");
    let second = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("second child should retain ancestors");

    drop(first);
    assert!(retention.is_retained(root_thread_id));
    assert!(retention.is_retained(parent_thread_id));

    drop(second);
    assert!(!retention.is_retained(root_thread_id));
    assert!(!retention.is_retained(parent_thread_id));
}

#[test]
fn nested_turns_release_only_their_own_ancestor_chain() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();
    let parent_thread_id = ThreadId::new();

    let child = retention
        .retain([root_thread_id])
        .expect("child should retain root");
    let grandchild = retention
        .retain([root_thread_id, parent_thread_id])
        .expect("grandchild should retain both ancestors");

    drop(child);
    assert!(retention.is_retained(root_thread_id));
    assert!(retention.is_retained(parent_thread_id));

    drop(grandchild);
    assert!(!retention.is_retained(root_thread_id));
    assert!(!retention.is_retained(parent_thread_id));
}

#[test]
fn duplicate_ancestor_ids_count_as_one_lease() {
    let retention = Arc::new(AncestorTurnRetention::default());
    let root_thread_id = ThreadId::new();

    let guard = retention
        .retain([root_thread_id, root_thread_id])
        .expect("ancestor should be retained");
    drop(guard);

    assert!(!retention.is_retained(root_thread_id));
}
