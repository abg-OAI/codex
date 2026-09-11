//! Regressions for notifications sent to a supervisor helper's parent.

use super::AgentControlHarness;
use super::AgentStatus;
use super::Feature;
use super::StartThreadOptions;
use crate::saffron::goal_supervisor::HELPER_ROLE_NAME;
use codex_protocol::AgentPath;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnCompleteEvent;

#[tokio::test]
async fn supervisor_failure_does_not_queue_a_parent_completion_message() {
    let (home, mut config) = super::test_config().await;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("multi-agent v2 should be available in tests");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent) = harness.start_thread().await;
    let helper_path = AgentPath::try_from("/root/goal_supervisor").expect("supervisor path");
    let helper = harness
        .manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(helper_path.clone()),
                agent_nickname: None,
                agent_role: Some(HELPER_ROLE_NAME.to_string()),
            })),
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("start supervisor helper");
    let turn = helper.thread.session.new_default_turn().await;
    *turn.terminal_error.lock().await = Some(ErrorEvent {
        message: "model unavailable".to_string(),
        codex_error_info: None,
        misalignment: None,
    });

    helper
        .thread
        .session
        .send_event(
            turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    assert_eq!(
        helper.thread.agent_status().await,
        AgentStatus::Errored("model unavailable".to_string())
    );
    assert!(
        !harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| {
                thread_id == parent_thread_id
                    && matches!(
                        op,
                        Op::InterAgentCommunication { communication, .. }
                            if communication.author == helper_path
                    )
            })
    );
}
