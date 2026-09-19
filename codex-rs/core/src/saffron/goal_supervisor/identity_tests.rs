use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn helper_identity_requires_the_private_role() {
    let parent_thread_id = ThreadId::new();
    let helper = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(
            AgentPath::root()
                .join(HELPER_ROLE_NAME)
                .expect("valid helper path"),
        ),
        agent_nickname: None,
        agent_role: Some(HELPER_ROLE_NAME.to_string()),
    });
    let ordinary_agent = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(
            AgentPath::root()
                .join("researcher")
                .expect("valid agent path"),
        ),
        agent_nickname: None,
        agent_role: Some("researcher".to_string()),
    });

    assert_eq!(
        [
            is_helper_source(&helper),
            is_helper_source(&ordinary_agent),
            is_helper_source(&SessionSource::Cli),
        ],
        [true, false, false]
    );
}
