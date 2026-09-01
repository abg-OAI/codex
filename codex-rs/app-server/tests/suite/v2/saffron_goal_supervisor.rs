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
async fn root_can_edit_its_active_goal_without_changing_other_goal_state() -> Result<()> {
    let server = responses::start_mock_server().await;
    let model_responses = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("create-goal"),
                responses::ev_function_call(
                    "create-goal-call",
                    "create_goal",
                    &json!({
                        "objective": "ship the release",
                        "token_budget": 20_000,
                    })
                    .to_string(),
                ),
                responses::ev_completed("create-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("edit-goal"),
                responses::ev_function_call_with_namespace(
                    "edit-goal-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({ "objective": "ship the release with release notes" }).to_string(),
                ),
                responses::ev_completed("edit-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("complete-goal"),
                responses::ev_function_call(
                    "complete-goal-call",
                    "update_goal",
                    &json!({ "status": "complete" }).to_string(),
                ),
                responses::ev_completed("complete-goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("root-finished"),
                responses::ev_assistant_message("root-message", "Release shipped."),
                responses::ev_completed("root-finished"),
            ]),
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
    let rollout_path = thread.thread.path.clone().context("thread rollout path")?;

    app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Ship the release and keep the goal accurate.".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1]
            .tool_by_name("saffron", "edit_active_goal")
            .is_some(),
        "the root should receive the active-goal editing tool"
    );
    assert!(
        requests[2]
            .function_call_output_text("edit-goal-call")
            .is_some_and(|output| {
                output.contains("ship the release with release notes")
                    && output.contains("20000")
                    && output.contains("active")
            }),
        "the edit result should retain the goal budget and active status"
    );

    let persisted_rollout = std::fs::read_to_string(rollout_path)?;
    assert!(persisted_rollout.lines().any(|line| {
        line.contains(r#""type":"thread_goal_updated""#)
            && line.contains(r#""objective":"ship the release with release notes""#)
    }));

    Ok(())
}

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
                responses::ev_response_created("supervisor-edit"),
                responses::ev_function_call_with_namespace(
                    "supervisor-edit-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({
                        "objective": "wait for the deployment and verify production health",
                    })
                    .to_string(),
                ),
                responses::ev_completed("supervisor-edit"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("supervisor-second-edit"),
                responses::ev_function_call_with_namespace(
                    "supervisor-second-edit-call",
                    "saffron",
                    "edit_active_goal",
                    &json!({ "objective": "replace the objective a second time" }).to_string(),
                ),
                responses::ev_completed("supervisor-second-edit"),
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
        while model_responses.requests().len() < 6 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("supervisor did not finish its check-in")?;

    let requests = model_responses.requests();
    assert_eq!(requests.len(), 6);
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
            .function_call_output_text("supervisor-edit-call")
            .is_some_and(|output| output.contains("verify production health")),
        "the supervisor should observe the committed replacement objective"
    );
    assert!(
        requests[4]
            .function_call_output_text("supervisor-second-edit-call")
            .is_some_and(|output| output.contains("already edited")),
        "the supervisor should not be able to edit the goal twice"
    );
    assert!(
        requests[5]
            .function_call_output_text("supervisor-snooze-call")
            .is_some_and(|output| output.contains("3600")),
        "the edit should not consume the required disposition action"
    );

    Ok(())
}
