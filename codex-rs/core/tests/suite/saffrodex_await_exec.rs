use anyhow::Result;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

const SESSION_ID: i32 = 1000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_exec_returns_when_output_arrives() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = "saff-await-output-start";
    let await_call_id = "saff-await-output";
    let start_args = json!({
        "cmd": "sleep 0.5; printf 'ready\\n'; sleep 0.5",
        "yield_time_ms": 250,
    });
    let await_args = json!({
        "session_id": SESSION_ID,
        "timeout_ms": 2_000,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    start_call_id,
                    "exec_command",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    await_call_id,
                    "saffron",
                    "await_exec",
                    &serde_json::to_string(&await_args)?,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_approval_and_permission_profile(
        "wait for process output",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = responses.requests();
    assert!(
        requests[0].tool_by_name("saffron", "await_exec").is_some(),
        "saffron.await_exec should be model-visible with unified exec"
    );
    let output = requests[2]
        .function_call_output_text(await_call_id)
        .expect("await_exec output should be present");
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(
        result,
        json!({
            "reason": "output",
            "chunk_id": result["chunk_id"],
            "wall_time_seconds": result["wall_time_seconds"],
            "session_id": SESSION_ID,
            "original_token_count": result["original_token_count"],
            "output": "ready\n",
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_exec_exit_mode_ignores_output_until_exit() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = "saff-await-exit-start";
    let await_call_id = "saff-await-exit";
    let start_args = json!({
        "cmd": "sleep 0.4; printf 'intermediate\\n'; sleep 0.4; exit 7",
        "yield_time_ms": 250,
    });
    let await_args = json!({
        "session_id": SESSION_ID,
        "return_on": "exit",
        "timeout_ms": 2_000,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    start_call_id,
                    "exec_command",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    await_call_id,
                    "saffron",
                    "await_exec",
                    &serde_json::to_string(&await_args)?,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_approval_and_permission_profile(
        "wait for process exit",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let requests = responses.requests();
    let output = requests[2]
        .function_call_output_text(await_call_id)
        .expect("await_exec output should be present");
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["reason"], "exit");
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["session_id"], Value::Null);
    assert_eq!(result["output"], "intermediate\n");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_exec_timeout_keeps_the_session_reusable() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = "saff-await-timeout-start";
    let await_call_id = "saff-await-timeout";
    let start_args = json!({
        "cmd": "sleep 1",
        "yield_time_ms": 250,
    });
    let await_args = json!({
        "session_id": SESSION_ID,
        "timeout_ms": 100,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    start_call_id,
                    "exec_command",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    await_call_id,
                    "saffron",
                    "await_exec",
                    &serde_json::to_string(&await_args)?,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_approval_and_permission_profile(
        "wait briefly without terminating the process",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let output = responses.requests()[2]
        .function_call_output_text(await_call_id)
        .expect("await_exec output should be present");
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["reason"], "timeout");
    assert_eq!(result["session_id"], SESSION_ID);
    assert_eq!(result["exit_code"], Value::Null);
    assert_eq!(result["output"], "");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_exec_wakes_on_exit_before_inherited_output_closes() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command fixture");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = "saff-await-remote-exit-start";
    let await_call_id = "saff-await-remote-exit";
    let start_args = json!({
        "cmd": "sleep 0.4; sh -c 'sleep 3 &' ; exit 7",
        "yield_time_ms": 250,
    });
    let await_args = json!({
        "session_id": SESSION_ID,
        "return_on": "exit",
        "timeout_ms": 2_000,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    start_call_id,
                    "exec_command",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    await_call_id,
                    "saffron",
                    "await_exec",
                    &serde_json::to_string(&await_args)?,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-4", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_approval_and_permission_profile(
        "wait for the parent process to exit",
        AskForApproval::Never,
        PermissionProfile::Disabled,
    )
    .await?;

    let output = responses.requests()[2]
        .function_call_output_text(await_call_id)
        .expect("await_exec output should be present");
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["reason"], "exit");
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["session_id"], Value::Null);
    assert!(
        result["wall_time_seconds"]
            .as_f64()
            .is_some_and(|seconds| seconds < 1.5),
        "await_exec should wake on process exit before its inherited output stream closes"
    );

    Ok(())
}
