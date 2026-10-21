//! Native tool, completion and archive storage exercised without Desktop.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::ThreadArchivedNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use codex_state::ThreadGoalStatus;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(30);
const FINAL: &str = "The requested work is saved. This conversation is ready for archival.";

#[test_case("await tools.exec_command({cmd: marker_command});"; "further_tool_is_refused")]
#[test_case("for (;;) { await new Promise(resolve => setTimeout(resolve, 1000)); }"; "cell_need_not_finish")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_mode_archival_terminates_active_goal(after_archive: &str) -> Result<()> {
    let server = responses::start_mock_server().await;
    let home = TempDir::new()?;
    let marker = home.path().join("unexpected-tool-effect");
    let before_marker = home.path().join("expected-tool-effect");
    let code = format!(
        "const marker_command = {};\n\
         await tools.exec_command({{cmd: {}}});\n\
         await tools.create_goal({{objective: 'Finish and archive the disposable test'}});\n\
         text(await tools.saffron__archive_self({{final_message: {}}}));\n{after_archive}",
        serde_json::to_string(&format!("echo finished > \"{}\"", marker.display()))?,
        serde_json::to_string(&format!("echo finished > \"{}\"", before_marker.display()))?,
        serde_json::to_string(FINAL)?,
    );
    let capture = responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_response_created("archive-cell"),
            responses::ev_custom_tool_call("archive-cell-call", "exec", &code),
            responses::ev_completed("archive-cell"),
        ])],
    )
    .await;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .enable_feature(Feature::CodeModeOnly)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let target = app
        .start_thread(ThreadStartParams {
            sandbox: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .await?
        .thread
        .id;
    let id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: target.clone(),
            input: vec![UserInput::Text {
                text: "Finish the test and archive this conversation.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(id).await?;
    let completed: TurnCompletedNotification =
        timeout(WAIT, app.read_notification("turn/completed")).await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let archived: ThreadArchivedNotification =
        timeout(WAIT, app.read_notification("thread/archived")).await??;
    assert_eq!(archived.thread_id, target);
    assert_goal_not_completed(&home, &target).await?;
    assert_eq!(
        capture.requests().len(),
        1,
        "no model or goal continuation after terminal acceptance"
    );
    assert!(
        before_marker.exists(),
        "the fixture must permit tool effects before archival"
    );
    assert!(
        !marker.exists(),
        "no tool effects after terminal acceptance"
    );
    let id = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: target,
            include_turns: true,
        })
        .await?;
    let saved: ThreadReadResponse = app.read_response(id).await?;
    assert_eq!(assistant_messages(&saved), vec![FINAL]);
    timeout(WAIT, app.shutdown_gracefully()).await??;
    Ok(())
}

#[test_case(false; "without_final_message")]
#[test_case(true; "with_final_message")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_self_archive_keeps_final_response_across_server_restart(
    with_final_message: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let capture = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("goal"),
                responses::ev_function_call(
                    "goal-call",
                    "create_goal",
                    &json!({"objective":"Finish and archive this disposable conversation"})
                        .to_string(),
                ),
                responses::ev_completed("goal"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("archive"),
                responses::ev_function_call_with_namespace(
                    "archive-call",
                    "saffron",
                    "archive_self",
                    &if with_final_message {
                        json!({"final_message": FINAL})
                    } else {
                        json!({})
                    }
                    .to_string(),
                ),
                responses::ev_completed("archive"),
            ]),
        ],
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::Goals)
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let target = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    let start_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: target.clone(),
            input: vec![UserInput::Text {
                text: "Finish, then archive this conversation.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(start_id).await?;
    let completed: TurnCompletedNotification =
        timeout(WAIT, app.read_notification("turn/completed")).await??;
    assert_eq!(completed.thread_id, target);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let archived: ThreadArchivedNotification =
        timeout(WAIT, app.read_notification("thread/archived")).await??;
    assert_eq!(archived.thread_id, target);
    assert_goal_not_completed(&home, &target).await?;
    let requests = capture.requests();
    assert_eq!(
        requests.len(),
        2,
        "archival must not need another model request or goal turn"
    );
    assert!(
        requests[0]
            .tool_by_name("saffron", "archive_self")
            .is_some()
    );
    let request_id = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: target.clone(),
            include_turns: true,
        })
        .await?;
    let saved: ThreadReadResponse = app.read_response(request_id).await?;
    let expected_messages = if with_final_message {
        vec![FINAL]
    } else {
        vec![]
    };
    assert_eq!(assistant_messages(&saved), expected_messages);
    timeout(WAIT, app.shutdown_gracefully()).await??;
    let mut reopened = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let request_id = reopened
        .send_thread_read_request(ThreadReadParams {
            thread_id: target,
            include_turns: true,
        })
        .await?;
    let saved: ThreadReadResponse = reopened.read_response(request_id).await?;
    assert_eq!(assistant_messages(&saved), expected_messages);
    timeout(WAIT, reopened.shutdown_gracefully()).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_failure_keeps_saved_history_and_reports_warning() -> Result<()> {
    let server = responses::start_mock_server().await;
    responses::mount_sse_sequence(
        &server,
        vec![responses::sse(vec![
            responses::ev_response_created("archive"),
            responses::ev_function_call_with_namespace(
                "archive-call",
                "saffron",
                "archive_self",
                &json!({"final_message": FINAL}).to_string(),
            ),
            responses::ev_completed("archive"),
        ])],
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let target = app
        .start_thread(ThreadStartParams::default())
        .await?
        .thread
        .id;
    // A file in place of the archive directory makes the native storage move fail.
    std::fs::write(home.path().join("archived_sessions"), "occupied")?;
    let start_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: target.clone(),
            input: vec![UserInput::Text {
                text: "Finish and archive.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(start_id).await?;
    let _: TurnCompletedNotification =
        timeout(WAIT, app.read_notification("turn/completed")).await??;
    let notification = timeout(
        WAIT,
        app.read_stream_until_matching_notification("self-archival failure", |notification| {
            notification.method == "warning"
                && notification
                    .params
                    .as_ref()
                    .and_then(|params| params.get("message"))
                    .and_then(|message| message.as_str())
                    .is_some_and(|message| message.starts_with("Self-archival did not complete"))
        }),
    )
    .await??;
    let warning: WarningNotification = serde_json::from_value(notification.params.unwrap())?;
    assert_eq!(warning.thread_id.as_deref(), Some(target.as_str()));
    assert!(warning.message.contains("Self-archival did not complete"));
    let read_id = app
        .send_thread_read_request(ThreadReadParams {
            thread_id: target.clone(),
            include_turns: true,
        })
        .await?;
    let saved: ThreadReadResponse = app.read_response(read_id).await?;
    assert_eq!(assistant_messages(&saved), vec![FINAL]);
    assert!(
        codex_core::find_thread_path_by_id_str(home.path(), &target, None)
            .await?
            .is_some()
    );
    timeout(WAIT, app.shutdown_gracefully()).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ephemeral_thread_cannot_discover_self_archival() -> Result<()> {
    let server = responses::start_mock_server().await;
    let capture = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("answer"),
            responses::ev_assistant_message("answer-message", "Nothing to archive."),
            responses::ev_completed("answer"),
        ]),
    )
    .await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app
        .start_thread(ThreadStartParams {
            ephemeral: Some(true),
            ..Default::default()
        })
        .await?;
    let id = app
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Inspect available tools.".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app.read_response(id).await?;
    let _: TurnCompletedNotification =
        timeout(WAIT, app.read_notification("turn/completed")).await??;
    assert!(
        capture
            .single_request()
            .tool_by_name("saffron", "archive_self")
            .is_none()
    );
    timeout(WAIT, app.shutdown_gracefully()).await??;
    Ok(())
}

async fn assert_goal_not_completed(home: &TempDir, thread_id: &str) -> Result<()> {
    // The goal RPC accepts unarchived threads only. Inspect the saved goal
    // through the state owner without resuming or unarchiving the test thread.
    let state = StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "mock_provider".into(),
    )
    .await?;
    let goal = state
        .thread_goals()
        .get_thread_goal(ThreadId::from_string(thread_id)?)
        .await?;
    assert_eq!(goal.expect("created goal").status, ThreadGoalStatus::Active);
    Ok(())
}

fn assistant_messages(saved: &ThreadReadResponse) -> Vec<&str> {
    saved
        .thread
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
