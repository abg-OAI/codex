use crate::agent::LocalAgentControl;
use crate::agent::types::AgentListingVisibility;
use crate::agent::types::AgentMetadata;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;

fn control_with_limit(max_threads: usize) -> LocalAgentControl {
    let control = LocalAgentControl::default();
    control.agent_execution_limiter.initialize(max_threads);
    control
}

#[test]
fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .agent_execution_limiter
        .initialize(/*max_threads*/ 2);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let thread_id = ThreadId::new();

    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit");
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source, thread_id)
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.ensure_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    drop(first);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity should be released when the running task drops");
}

#[test]
fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);
    let thread_id = ThreadId::new();

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &SessionSource::Cli, thread_id)
            .is_none()
    );
    assert!(
        control
            .execution_guard(
                MultiAgentVersion::V1,
                &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
                thread_id,
            )
            .is_none()
    );
}

#[test]
fn execution_guards_ignore_hidden_internal_helpers() {
    let control = control_with_limit(/*max_threads*/ 0);
    let thread_id = ThreadId::new();
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("internal_helper".to_string()),
    });
    control
        .state
        .reserve_internal_spawn_slot()
        .commit(AgentMetadata {
            agent_id: Some(thread_id),
            agent_path: Some(
                AgentPath::root()
                    .join("internal_helper")
                    .expect("valid helper path"),
            ),
            agent_role: Some("internal_helper".to_string()),
            visibility: AgentListingVisibility::Hidden,
            ..Default::default()
        });

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &source, thread_id)
            .is_none()
    );
}

#[test]
fn descendant_turn_guard_retains_every_registered_ancestor() {
    let control = control_with_limit(/*max_threads*/ 4);
    let root_thread_id = ThreadId::new();
    let worker_thread_id = ThreadId::new();
    let tester_thread_id = ThreadId::new();
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let tester_path = worker_path.join("tester").expect("tester path");
    control.state.register_root_thread(root_thread_id);
    control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve worker")
        .commit(AgentMetadata {
            agent_id: Some(worker_thread_id),
            agent_path: Some(worker_path),
            ..Default::default()
        });
    control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve tester")
        .commit(AgentMetadata {
            agent_id: Some(tester_thread_id),
            agent_path: Some(tester_path.clone()),
            ..Default::default()
        });
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: worker_thread_id,
        depth: 2,
        agent_path: Some(tester_path),
        agent_nickname: None,
        agent_role: None,
    });

    let guard = control
        .ancestor_turn_retention_guard(MultiAgentVersion::V2, &source, tester_thread_id)
        .expect("listed V2 child should retain ancestors");
    assert!(control.is_retained_for_descendant_completion(root_thread_id));
    assert!(control.is_retained_for_descendant_completion(worker_thread_id));

    drop(guard);
    assert!(!control.is_retained_for_descendant_completion(root_thread_id));
    assert!(!control.is_retained_for_descendant_completion(worker_thread_id));
}
