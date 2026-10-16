use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn host_installs_only_for_persistent_roots() {
    for (source, ephemeral, persistent, expected) in [
        (SessionSource::Exec, false, true, true),
        (SessionSource::Exec, true, true, false),
        (SessionSource::Exec, false, false, false),
        (
            SessionSource::SubAgent(SubAgentSource::Review),
            false,
            true,
            false,
        ),
        (
            SessionSource::SubAgent(SubAgentSource::Other("supervisor".into())),
            true,
            true,
            false,
        ),
        (
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::default(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            false,
            true,
            false,
        ),
    ] {
        let (session, turn) = make_session_and_context().await;
        let mut config = turn.config.as_ref().clone();
        config.ephemeral = ephemeral;
        let (sender, _receiver) = mpsc::unbounded_channel();
        Host { sender }
            .on_thread_start(ThreadStartInput {
                config: &config,
                session_source: &source,
                persistent_thread_state_available: persistent,
                environments: &[],
                mcp_resource_client: None,
                extension_metrics: None,
                session_store: &session.services.session_extension_data,
                thread_store: &session.services.thread_extension_data,
            })
            .await;
        assert_eq!(
            session
                .services
                .thread_extension_data
                .get::<PendingArchive>()
                .is_some(),
            expected
        );
    }
}

#[tokio::test]
async fn native_tool_schedules_only_itself_and_repeated_calls_coalesce() {
    let (session, turn, mut receiver) = fixture().await;
    assert_eq!(
        invoke(session.clone(), turn.clone(), "{}").await.unwrap(),
        json!({"status":"scheduled"})
    );
    assert_eq!(
        invoke(session.clone(), turn.clone(), "{}").await.unwrap(),
        json!({"status":"scheduled"})
    );
    assert!(
        receiver.try_recv().is_err(),
        "the tool must not archive its running turn"
    );
    assert!(finish(&session, &turn, true).await);
    let request = receiver.try_recv().unwrap();
    assert_eq!(request.thread_id, session.thread_id());
    assert_eq!(request.turn_id, turn.sub_id);
    assert!(request.matches_runtime(&session.services.thread_extension_data));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn target_argument_is_rejected_without_scheduling() {
    let (session, turn, mut receiver) = fixture().await;
    assert!(
        invoke(session.clone(), turn, r#"{"thread_id":"another-thread"}"#)
            .await
            .is_err()
    );
    assert!(!pending(&session));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn failed_or_cancelled_turn_never_hands_off() {
    for cancel_first in [false, true] {
        let (session, turn, mut receiver) = fixture().await;
        invoke(session.clone(), turn.clone(), "{}").await.unwrap();
        if cancel_first {
            cancel(&session);
        }
        assert!(!finish(&session, &turn, cancel_first).await);
        assert!(!pending(&session));
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn lifecycle_error_cancels_request_even_if_the_turn_later_finishes() {
    let (session, turn, mut receiver) = fixture().await;
    invoke(session.clone(), turn.clone(), "{}").await.unwrap();
    let (sender, _unused) = mpsc::unbounded_channel();
    Host { sender }
        .on_turn_error(TurnErrorInput {
            turn_id: &turn.sub_id,
            error: codex_protocol::protocol::CodexErrorInfo::Other,
            error_details: &codex_protocol::error::CodexErrorDetails::Timeout,
            session_store: &session.services.session_extension_data,
            thread_store: &session.services.thread_extension_data,
            turn_store: &turn.extension_data,
        })
        .await;
    assert!(!finish(&session, &turn, true).await);
    assert!(!pending(&session));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn newer_turn_and_replacement_runtime_revoke_handoff() {
    let (session, turn, mut receiver) = fixture().await;
    invoke(session.clone(), turn.clone(), "{}").await.unwrap();
    assert!(finish(&session, &turn, true).await);
    let request = receiver.try_recv().unwrap();
    let (replacement, _, _) = fixture().await;
    assert!(!request.matches_runtime(&replacement.services.thread_extension_data));
    session.record_started_turn("newer-turn", None).await;
    assert!(!request.matches_runtime(&session.services.thread_extension_data));
    assert!(!pending(&session));
}

#[tokio::test]
async fn unavailable_host_rejects_the_tool() {
    let (session, turn, receiver) = fixture().await;
    drop(receiver);
    assert!(invoke(session.clone(), turn, "{}").await.is_err());
    assert!(!pending(&session));
}

#[tokio::test]
async fn terminal_acceptance_drains_sampling_without_aborting_the_turn() {
    let (session, turn, _receiver) = fixture().await;
    let turn_cancellation = CancellationToken::new();
    let execution = turn_cancellation.child_token();
    invoke(session.clone(), turn.clone(), "{}").await.unwrap();
    let sample = async {
        execution.cancelled().await;
        Err::<(), _>(codex_protocol::error::CodexErr::TurnAborted)
    };
    let result = terminal::run_sampling(&session, &turn, execution.clone(), sample)
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(!turn_cancellation.is_cancelled());
    assert!(terminal::admit_tool(&turn).is_err());
}

#[tokio::test]
async fn revoked_archival_can_resume_after_sampling_drains() {
    let (session, turn, _receiver) = fixture().await;
    invoke(session.clone(), turn.clone(), "{}").await.unwrap();
    cancel(&session);
    let result =
        terminal::run_sampling(&session, &turn, CancellationToken::new(), async { Ok(()) })
            .await
            .unwrap();
    assert!(result.is_none());
    assert!(terminal::resume_if_revoked(&session, &turn));
    assert!(terminal::admit_tool(&turn).is_ok());
    assert!(!pending(&session));
}

#[tokio::test]
async fn terminal_request_does_not_hide_sampling_failure_or_user_interruption() {
    let (session, turn, _receiver) = fixture().await;
    invoke(session.clone(), turn.clone(), "{}").await.unwrap();
    let result = terminal::run_sampling(&session, &turn, CancellationToken::new(), async {
        Err::<(), _>(codex_protocol::error::CodexErr::Fatal(
            "sample failed".into(),
        ))
    })
    .await;
    assert!(result.is_err());
    let interrupted = CancellationToken::new();
    interrupted.cancel();
    assert!(
        terminal::complete(&session, &StepContext::for_test(turn), &interrupted)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unaccepted_tool_result_cannot_authorize_archival() {
    let (session, turn, mut receiver) = fixture().await;
    let state = session
        .services
        .thread_extension_data
        .get::<PendingArchive>()
        .unwrap();
    Handler { state }
        .handle(invocation(session.clone(), turn.clone(), "{}"))
        .await
        .unwrap();
    assert!(!finish(&session, &turn, true).await);
    assert!(!pending(&session));
    assert!(receiver.try_recv().is_err());
}

async fn fixture() -> (
    Arc<Session>,
    Arc<TurnContext>,
    mpsc::UnboundedReceiver<ArchiveRequest>,
) {
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config).ephemeral = false;
    turn.session_source = SessionSource::Exec;
    session.record_started_turn(&turn.sub_id, None).await;
    let (sender, receiver) = mpsc::unbounded_channel();
    session
        .services
        .thread_extension_data
        .insert(PendingArchive {
            turn: Mutex::new(None),
            sender,
        });
    (Arc::new(session), Arc::new(turn), receiver)
}

async fn invoke(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    arguments: &str,
) -> Result<serde_json::Value, FunctionCallError> {
    let mut registry = ToolRegistry::default();
    register(&session, &turn, &mut registry);
    let handler = registry
        .remove(&ToolName::namespaced("saffron", "archive_self"))
        .unwrap();
    let invocation = invocation(session, turn, arguments);
    let output = handler.handle(invocation.clone()).await?;
    handler.on_tool_result_accepted(&invocation, output.as_ref());
    Ok(serde_json::from_str(&output.log_output()).unwrap())
}

fn invocation(session: Arc<Session>, turn: Arc<TurnContext>, arguments: &str) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(turn.clone()),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::default())),
        call_id: "archive-call".into(),
        tool_name: ToolName::namespaced("saffron", "archive_self"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.into(),
        },
    }
}
