use crate::agent::AgentControl;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentVisibility;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;

fn control_with_limit(max_threads: usize) -> AgentControl {
    let control = AgentControl::default();
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
            visibility: AgentVisibility::Hidden,
            ..Default::default()
        });

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &source, thread_id)
            .is_none()
    );
}
