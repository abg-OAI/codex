use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadCompactStartParams;
use codex_app_server_protocol::ThreadCompactStartResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_lifecycle_guidance_persists_without_keyword_routing() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            assistant_response("goal-turn"),
            assistant_response("ordinary-turn"),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;

    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    submit_turn(
        &mut app_server,
        &thread.thread.id,
        "Summarize the current changes.",
    )
    .await?;
    submit_turn(
        &mut app_server,
        &thread.thread.id,
        "Continue with the next task.",
    )
    .await?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 2);
    let guidance = goal_guidance(&requests[0]);
    assert_eq!(guidance.len(), 1);
    assert!(guidance[0].contains("An active goal continues autonomously"));
    assert!(guidance[0].contains("blocked-goal audit"));
    assert!(guidance[0].contains("snooze"));
    assert!(!guidance[0].contains("await_exec"));
    assert_eq!(goal_guidance(&requests[1]).len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_lifecycle_guidance_survives_compaction_and_resume_once() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            assistant_response("seed-turn"),
            assistant_response("compact-turn"),
            assistant_response("post-compact-turn"),
            assistant_response("resumed-turn"),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;

    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    let thread_id = thread.thread.id;
    submit_turn(&mut app_server, &thread_id, "Start the work.").await?;

    let compact_id = app_server
        .send_thread_compact_start_request(ThreadCompactStartParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let _: ThreadCompactStartResponse =
        timeout(RESPONSE_TIMEOUT, app_server.read_response(compact_id)).await??;
    let _: TurnCompletedNotification = timeout(
        RESPONSE_TIMEOUT,
        app_server.read_notification("turn/completed"),
    )
    .await??;

    submit_turn(&mut app_server, &thread_id, "Continue after compaction.").await?;
    timeout(RESPONSE_TIMEOUT, app_server.shutdown_gracefully()).await??;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(RESPONSE_TIMEOUT, app_server.read_response(resume_id)).await??;
    submit_turn(&mut app_server, &thread_id, "Continue after resume.").await?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| goal_guidance(request).len() == 1)
    );

    Ok(())
}

fn assistant_response(response_id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(response_id),
        responses::ev_assistant_message(&format!("{response_id}-message"), "Done."),
        responses::ev_completed(response_id),
    ])
}

async fn submit_turn(app_server: &mut TestAppServer, thread_id: &str, input: &str) -> Result<()> {
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: input.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    Ok(())
}

fn goal_guidance(request: &responses::ResponsesRequest) -> Vec<String> {
    request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.contains("<saffron_goal_supervisor>"))
        .collect()
}
