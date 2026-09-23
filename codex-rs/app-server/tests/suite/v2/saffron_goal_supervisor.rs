use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_root_goal_runs_hidden_saffron_supervisor() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("parent-create-goal"),
                responses::ev_function_call(
                    "create-goal-call",
                    "create_goal",
                    &json!({ "objective": "wait for the deployment" }).to_string(),
                ),
                responses::ev_completed("parent-create-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("parent-idle"),
                responses::ev_assistant_message("parent-message", "Waiting for deployment."),
                responses::ev_completed("parent-idle"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-action"),
                responses::ev_function_call_with_namespace(
                    "supervisor-snooze-call",
                    "saffron",
                    "supervisor_snooze",
                    &json!({ "delay_seconds": 3600 }).to_string(),
                ),
                responses::ev_completed("supervisor-action"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-finished"),
                responses::ev_assistant_message("supervisor-message", "Snoozed."),
                responses::ev_completed("supervisor-finished"),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", server.uri()))
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?;
    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Track the deployment until it completes.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(10), async {
        while model_responses.requests().len() < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("supervisor did not finish its check-in")?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]
            .tool_by_name("saffron", "supervisor_snooze")
            .is_some(),
        "the helper should receive the Saffron supervisor action tools"
    );
    assert!(
        requests[2]
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("# Supervisor Check-in")),
        "the helper should receive the active-goal assignment"
    );
    assert!(
        requests[3]
            .function_call_output_text("supervisor-snooze-call")
            .is_some_and(|output| output.contains("3600")),
        "the action should execute before the helper finishes"
    );

    Ok(())
}
