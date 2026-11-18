//! Exercises retained requests through compaction and a subsequent model turn.

use anyhow::Result;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::json;
use wiremock::MockServer;

/// Admitted agent messages retain attribution through compaction and cold resume.
#[test_case::test_case(false, "send_message_to_thread"; "local_message")]
#[test_case::test_case(true, "send_message_to_thread"; "remote_message")]
#[test_case::test_case(false, "fork_thread"; "local_assignment")]
#[test_case::test_case(true, "fork_thread"; "remote_assignment")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saffron_delivery_survives_compaction_and_resume(remote: bool, tool: &str) -> Result<()> {
    use codex_core::TurnInput;
    use codex_core::TurnInputRequest;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;

    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mut builder = test_codex();
    let compact = if remote {
        builder = builder.with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        responses::sse(vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "compaction", "encrypted_content": "checkpoint"
            }}),
            responses::ev_completed("compact"),
        ])
    } else {
        let mut provider =
            ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
        provider.name = "Local compaction test".to_owned();
        provider.supports_websockets = false;
        builder = builder
            .with_model("gpt-5.4")
            .with_config(move |config| config.model_provider = provider);
        responses::sse(vec![
            responses::ev_assistant_message("summary", "The report is unfinished."),
            responses::ev_completed("compact"),
        ])
    };
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![responses::ev_completed("initial")]),
            compact.clone(),
            responses::sse(vec![responses::ev_completed("continued")]),
            compact,
            responses::sse(vec![responses::ev_completed("resumed")]),
        ],
    )
    .await;
    let test = builder.build(&server).await?;
    let body = json!({
        "source_thread_id": "01900000-0000-7000-8000-000000000001",
        "input": "Inspect the orchard export; do not publish."
    })
    .to_string();
    test.codex
        .start_turn_if_idle(TurnInputRequest::new(TurnInput::ResponseItem(
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: None,
                name: Some(tool.to_owned()),
                namespace: Some("saffron".to_owned()),
                output: FunctionCallOutputPayload::from_text(body.clone()),
                internal_chat_message_metadata_passthrough: None,
            },
        )))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("Continue the inspection.").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let resumed = builder.restart(&server, &test).await?;
    resumed.submit_turn("Report the result.").await?;
    resumed.codex.shutdown_and_wait().await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 5);
    for index in [2, 4] {
        let input = requests[index].input();
        let deliveries: Vec<_> = input
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output"
                    && item["namespace"] == "saffron"
                    && item["name"] == tool
            })
            .collect();
        assert_eq!(deliveries.len(), 1, "delivery in request {index}");
        assert_eq!(deliveries[0]["output"], body);
        assert!(
            deliveries[0]
                .get("call_id")
                .is_none_or(serde_json::Value::is_null)
        );
    }
    Ok(())
}

/// Local compaction preserves the request without an additional model helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_compaction_retains_requests() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mut provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", server.uri())));
    provider.name = "Local compaction test".to_owned();
    provider.supports_websockets = false;
    let builder = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| config.model_provider = provider);
    assert_retained_request(
        &server,
        builder,
        responses::sse(vec![
            responses::ev_assistant_message("summary", "The report is unfinished."),
            responses::ev_completed("compact"),
        ]),
    )
    .await
}

/// Remote compaction preserves the request without generated commentary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compaction_retains_requests() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let builder = test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    assert_retained_request(
        &server,
        builder,
        responses::sse(vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "compaction", "encrypted_content": "checkpoint"
            }}),
            responses::ev_completed("compact"),
        ]),
    )
    .await
}

/// Checks stored compaction and the request sent when model work continues.
async fn assert_retained_request(
    server: &MockServer,
    mut builder: TestCodexBuilder,
    compact: String,
) -> Result<()> {
    let requests = responses::mount_sse_sequence(
        server,
        vec![
            responses::sse(vec![responses::ev_completed("initial")]),
            compact,
            responses::sse(vec![responses::ev_completed("continued")]),
        ],
    )
    .await;
    let test = builder.build(server).await?;
    test.submit_turn("#refine Prepare the report; do not publish.")
        .await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::ItemCompleted(event) = event {
            assert!(
                !matches!(event.item, TurnItem::AgentMessage(_)),
                "no generated account display"
            );
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("Continue the report.").await?;
    test.codex.flush_rollout().await?;
    let history = test.codex.load_history(false).await?;
    assert!(
        history
            .items
            .iter()
            .any(|item| matches!(item, RolloutItem::Compacted(_)))
    );
    test.codex.shutdown_and_wait().await?;
    let requests = requests.requests();
    assert_eq!(
        requests.len(),
        3,
        "initial turn, compaction and continuation only"
    );
    let input = requests[2].input();
    assert_eq!(
        input
            .iter()
            .filter(|item| item["role"] == "user"
                && item["content"].as_array().is_some_and(|content| content
                    .iter()
                    .any(|part| part["text"] == "#refine Prepare the report; do not publish.")))
            .count(),
        1
    );
    assert!(
        !input.iter().any(
            |item| item["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|part| part["text"]
                    .as_str()
                    .is_some_and(
                        |text| text.starts_with("Historical account of earlier user requests")
                    )))
        )
    );
    Ok(())
}
