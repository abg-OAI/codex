use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_waiting_guidance_is_limited_to_goal_turns() -> Result<()> {
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

    submit_turn(&mut app_server, "Track this as a durable goal.").await?;
    submit_turn(&mut app_server, "Summarize the current changes.").await?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 2);
    let guidance = goal_guidance(&requests[0]).expect("goal guidance");
    assert!(guidance.contains("snooze"));
    assert!(!guidance.contains("await_exec"));
    assert!(goal_guidance(&requests[1]).is_none());

    Ok(())
}

fn assistant_response(response_id: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(response_id),
        responses::ev_assistant_message(&format!("{response_id}-message"), "Done."),
        responses::ev_completed(response_id),
    ])
}

async fn submit_turn(app_server: &mut TestAppServer, input: &str) -> Result<()> {
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: input.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    Ok(())
}

fn goal_guidance(request: &responses::ResponsesRequest) -> Option<String> {
    request
        .message_input_texts("developer")
        .into_iter()
        .find(|text| text.contains("<saffron_goal_waiting>"))
}
