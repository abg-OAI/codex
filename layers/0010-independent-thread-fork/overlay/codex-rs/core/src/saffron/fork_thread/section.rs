//! Optional Desktop sidebar placement through the client request transport.
//!
//! Desktop owns section membership and host routing. Native thread-store sections
//! are not a substitute for that state. Each request is bounded; a timeout leaves
//! placement unconfirmed and never cancels the independently running fork.
//! Saved registrations select legacy tool spelling, not current availability.
//! Desktop may reject any request; this adapter never changes the tool catalog.

use std::time::Duration;

use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_tools::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;

use crate::tools::context::ToolInvocation;
use crate::tools::handlers::dynamic::request_dynamic_tool;

/// Maximum wait for one optional Desktop request, independent of fork execution.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Placement is independent of creation and assignment submission.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Outcome {
    /// Desktop confirmed the move to the source section.
    Inherited {
        /// Section identifier returned by Desktop.
        section_id: String,
    },
    /// Desktop confirmed the move to the explicitly named destination.
    Placed {
        /// Destination identifier returned by Desktop.
        section_id: String,
        /// Whether this invocation created the destination before moving.
        created: bool,
    },
    /// Placement was not requested or Desktop supplied no source membership or host.
    Skipped {
        /// Why no move was attempted.
        reason: String,
    },
    /// A supported lookup or move did not confirm placement.
    Failed {
        /// Failure or uncertainty; a timed-out move might still complete.
        reason: String,
    },
}

/// Places a saved fork without changing its execution outcome.
///
/// An explicit name overrides inheritance, including opt-out. Creation and move
/// are separate effects: an unsuccessful move can leave a newly created section.
/// Requests are never retried because a missing receipt can hide a completed effect.
pub(super) async fn place(
    invocation: &ToolInvocation,
    fork: ThreadId,
    inherit: bool,
    name: Option<&str>,
) -> Outcome {
    if name.is_none() && !inherit {
        return Outcome::Skipped {
            reason: "inherit_section is false".to_string(),
        };
    }
    let tools = &invocation.turn.dynamic_tools;
    let list = desktop_tool(tools, "list_threads");
    let move_tool = desktop_tool(tools, "move_thread_to_sidebar_section");
    let listing: Listing = match request(invocation, list, json!({"limit": 50}), "list").await {
        Ok(listing) => listing,
        Err(reason) => return Outcome::Failed { reason },
    };
    let source_id = invocation.session.thread_id().to_string();
    let Some(source) = listing
        .threads
        .iter()
        .chain(&listing.pinned_threads)
        .find(|thread| thread.id == source_id && thread.kind == "codex")
    else {
        return Outcome::Skipped {
            reason: "Desktop did not return the caller's host information".to_string(),
        };
    };
    let Some(host_id) = source.host_id.as_ref() else {
        return Outcome::Skipped {
            reason: "Desktop did not return the caller's host information".to_string(),
        };
    };
    let (section_id, created) = if let Some(name) = name {
        match named_destination(invocation, &listing, name).await {
            Ok(destination) => destination,
            Err(reason) => return Outcome::Failed { reason },
        }
    } else {
        match listing.source_section(&source_id) {
            Ok(Some(section)) => (section.section_id.clone(), false),
            Ok(None) => {
                return Outcome::Skipped {
                    reason: "caller has no Desktop sidebar section".to_string(),
                };
            }
            Err(reason) => return Outcome::Failed { reason },
        }
    };
    let fork_id = fork.to_string();
    let receipt: MoveReceipt = match request(
        invocation,
        move_tool,
        json!({
            "threadId": fork_id,
            "hostId": host_id,
            "sectionId": section_id,
        }),
        "move",
    )
    .await
    {
        Ok(receipt) => receipt,
        Err(reason) => {
            return Outcome::Failed {
                reason: format!("{reason}; destination section {section_id} (created: {created})"),
            };
        }
    };
    if receipt.thread_id != fork_id
        || receipt.section_id != section_id
        || receipt.host_id != *host_id
    {
        return Outcome::Failed {
            reason: format!(
                "Desktop did not confirm the requested fork placement; destination section {section_id} (created: {created})"
            ),
        };
    }
    if name.is_some() {
        Outcome::Placed {
            section_id: receipt.section_id,
            created,
        }
    } else {
        Outcome::Inherited {
            section_id: receipt.section_id,
        }
    }
}

/// Resolves a unique displayed name, creating only when the listing has no match.
async fn named_destination(
    invocation: &ToolInvocation,
    listing: &Listing,
    name: &str,
) -> Result<(String, bool), String> {
    let mut matches = listing
        .sections
        .iter()
        .filter(|section| section.name == name);
    let found = matches.next();
    if matches.next().is_some() {
        return Err(format!(
            "Desktop returned multiple sections named {name:?}; no destination selected"
        ));
    }
    if let Some(section) = found {
        return Ok((section.section_id.clone(), false));
    }
    let create = desktop_tool(&invocation.turn.dynamic_tools, "create_sidebar_section");
    let section: Section = request(invocation, create, json!({"name": name}), "create").await?;
    if section.name != name || section.section_id.is_empty() {
        return Err("Desktop did not confirm the requested section creation; inspect sections before retrying".to_string());
    }
    Ok((section.section_id, true))
}

/// Uses an advertised spelling when present, otherwise the namespaced handler.
///
/// Registrations can outlive the client's catalog. Missing registrations do not
/// establish handler availability; the bounded request obtains the client result.
fn desktop_tool(tools: &[DynamicToolSpec], name: &str) -> ToolName {
    for tool in tools {
        match tool {
            DynamicToolSpec::Namespace(namespace) if namespace.name == "codex_app" => {
                if namespace.tools.iter().any(|tool| matches!(tool, DynamicToolNamespaceTool::Function(function) if function.name == name)) {
                    return ToolName::namespaced("codex_app", name);
                }
            }
            DynamicToolSpec::Function(function) if function.name == format!("codex_app__{name}") => {
                return ToolName::new(None, function.name.clone());
            }
            _ => {}
        }
    }
    ToolName::namespaced("codex_app", name)
}

/// Calls the existing client transport, completing pending bookkeeping on timeout.
async fn request<T: DeserializeOwned>(
    invocation: &ToolInvocation,
    tool: ToolName,
    arguments: Value,
    suffix: &str,
) -> Result<T, String> {
    let call_id = format!("{}-section-{suffix}", invocation.call_id);
    let future = request_dynamic_tool(
        &invocation.session,
        &invocation.turn,
        call_id.clone(),
        tool,
        arguments,
        invocation.cancellation_token.clone(),
    );
    tokio::pin!(future);
    let response = tokio::select! {
        response = &mut future => response,
        _ = tokio::time::sleep(REQUEST_TIMEOUT) => {
            // Resolve the existing pending request so the item has a terminal
            // event. A late client receipt cannot restart or duplicate the move.
            invocation.session.notify_dynamic_tool_response(&call_id, DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "Desktop placement request timed out; its outcome is unconfirmed".to_string(),
                }],
                success: false,
            }).await;
            future.await
        }
    }.ok_or_else(|| "Desktop placement request was cancelled".to_string())?;
    decode(response)
}

/// Decodes the app's JSON text without treating human error text as success.
fn decode<T: DeserializeOwned>(response: DynamicToolResponse) -> Result<T, String> {
    let text = response
        .content_items
        .into_iter()
        .filter_map(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !response.success {
        return Err(format!("Desktop placement request failed: {text}"));
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("Desktop placement response is invalid: {error}"))
}

/// Fields owned by Desktop's list tool that are needed for placement.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listing {
    /// Thread identities include routing, unlike sidebar item keys.
    threads: Vec<Thread>,
    /// Pinned threads are excluded from the ordinary thread page.
    #[serde(default)]
    pinned_threads: Vec<Thread>,
    /// Complete sidebar sections returned alongside the thread page.
    sections: Vec<Section>,
}

impl Listing {
    /// Sidebar keys carry a source label, not the public remote-host identifier.
    fn source_section(&self, thread_id: &str) -> Result<Option<&Section>, String> {
        let suffix = format!(":{thread_id}");
        let mut matches = self.sections.iter().filter(|section| {
            section
                .item_keys
                .iter()
                .any(|key| key.starts_with("codex:thread:") && key.ends_with(&suffix))
        });
        let section = matches.next();
        if matches.next().is_some() {
            return Err("Desktop returned ambiguous section membership".to_string());
        }
        Ok(section)
    }
}

/// Public thread routing supplied by Desktop, never inferred from sidebar keys.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    /// Saved thread identifier.
    id: String,
    /// Only Codex threads are relevant to this operation.
    kind: String,
    /// Client routing for both source and the fork created on the same host.
    host_id: Option<String>,
}

/// Desktop-owned section membership.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Section {
    /// Stable section identifier accepted by the move tool.
    section_id: String,
    /// Displayed section name used for explicit destination selection.
    name: String,
    /// Desktop's opaque sidebar identities, including the saved thread suffix.
    item_keys: Vec<String>,
}

/// Confirmation returned by the move tool.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveReceipt {
    /// Host on which the moved fork resides.
    host_id: String,
    /// Moved fork identifier.
    thread_id: String,
    /// Destination confirmed by Desktop.
    section_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
    use crate::state::ActiveTurn;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolPayload;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
    use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::EventMsg;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// The observed Desktop representation routes remote threads separately from keys.
    #[test]
    fn remote_host_is_not_inferred_from_local_sidebar_key() {
        let listing: Listing = serde_json::from_value(json!({
            "threads": [{"id": "chatgpt", "kind": "chatgpt"}],
            "pinnedThreads": [{"id": "parent", "kind": "codex", "hostId": "remote-ssh-discovered:abg-c"}],
            "sections": [{"sectionId": "section", "name": "Source", "itemKeys": ["codex:thread:local:parent"]}]
        })).unwrap();
        assert_eq!(
            listing
                .source_section("parent")
                .unwrap()
                .unwrap()
                .section_id,
            "section"
        );
        assert_eq!(
            listing.pinned_threads[0].host_id.as_deref(),
            Some("remote-ssh-discovered:abg-c")
        );
        assert!(listing.source_section("absent").unwrap().is_none());
    }

    /// Error text never becomes a placement confirmation.
    #[test]
    fn failed_tool_response_is_not_decoded_as_success() {
        let response = DynamicToolResponse {
            success: false,
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "unavailable".to_string(),
            }],
        };
        assert!(decode::<Listing>(response).is_err());
    }

    /// Namespaced registrations retain client routing and move confirmation.
    #[test_case::test_case(true, "inherited"; "confirmed")]
    #[test_case::test_case(false, "failed"; "rejected")]
    #[tokio::test]
    async fn placement_uses_client_routing_and_reports_move_result(
        move_succeeds: bool,
        expected_status: &str,
    ) {
        let (session, turn, events) =
            make_session_and_context_with_dynamic_tools_and_rx(vec![DynamicToolSpec::Namespace(
                DynamicToolNamespaceSpec {
                    name: "codex_app".to_string(),
                    description: "Desktop tools".to_string(),
                    tools: ["list_threads", "move_thread_to_sidebar_section"]
                        .into_iter()
                        .map(|name| {
                            DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
                                name: name.to_string(),
                                description: name.to_string(),
                                input_schema: json!({"type": "object"}),
                                defer_loading: false,
                            })
                        })
                        .collect(),
                },
            )])
            .await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        let fork = ThreadId::new();
        let invocation = ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            call_id: "placement-test".to_string(),
            tool_name: ToolName::namespaced("saffron", "fork_thread"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        let client = async {
            let mut requests = 0;
            while requests < 2 {
                let event = events.recv().await.unwrap();
                let EventMsg::ItemStarted(event) = event.msg else {
                    continue;
                };
                let TurnItem::DynamicToolCall(call) = event.item else {
                    continue;
                };
                assert_eq!(call.namespace.as_deref(), Some("codex_app"));
                let (value, success) = if call.tool == "list_threads" {
                    assert_eq!(call.arguments, json!({"limit": 50}));
                    (
                        json!({
                            "threads": [{"id": "chatgpt", "kind": "chatgpt"}],
                            "pinnedThreads": [{"id": session.thread_id(), "kind": "codex", "hostId": "remote-ssh-discovered:abg-c"}],
                            "sections": [{"sectionId": "source-section", "name": "Source", "itemKeys": [format!("codex:thread:local:{}", session.thread_id())]}],
                        }),
                        true,
                    )
                } else {
                    assert_eq!(call.tool, "move_thread_to_sidebar_section");
                    assert_eq!(
                        call.arguments,
                        json!({"threadId": fork, "hostId": "remote-ssh-discovered:abg-c", "sectionId": "source-section"})
                    );
                    (call.arguments.clone(), move_succeeds)
                };
                session
                    .notify_dynamic_tool_response(
                        &call.id,
                        DynamicToolResponse {
                            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                                text: value.to_string(),
                            }],
                            success,
                        },
                    )
                    .await;
                requests += 1;
            }
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(place(&invocation, fork, true, None), client)
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(outcome).unwrap()["status"],
            expected_status
        );
    }

    /// An explicit name wins over both inherited membership and opting out.
    #[test_case::test_case(true; "inherit_enabled")]
    #[test_case::test_case(false; "inherit_disabled")]
    #[tokio::test]
    async fn explicit_existing_section_overrides_inheritance(inherit: bool) {
        let (invocation, events) =
            desktop_session(&["list_threads", "move_thread_to_sidebar_section"]).await;
        let fork = ThreadId::new();
        let client = async {
            reply_to_desktop(&invocation, &events, "list_threads", json!({"limit": 50}), json!({
                "threads": [{"id": invocation.session.thread_id(), "kind": "codex", "hostId": "remote"}],
                "sections": [
                    {"sectionId": "source", "name": "Source", "itemKeys": [format!("codex:thread:local:{}", invocation.session.thread_id())]},
                    {"sectionId": "destination", "name": "Research", "itemKeys": []},
                ],
            }), true).await;
            let placement =
                json!({"threadId": fork, "hostId": "remote", "sectionId": "destination"});
            reply_to_desktop(
                &invocation,
                &events,
                "move_thread_to_sidebar_section",
                placement.clone(),
                placement,
                true,
            )
            .await;
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(place(&invocation, fork, inherit, Some("Research")), client)
        })
        .await
        .unwrap();
        assert_eq!(
            outcome,
            Outcome::Placed {
                section_id: "destination".to_string(),
                created: false
            }
        );
    }

    /// Missing registrations do not prevent explicit creation and placement.
    #[test_case::test_case(&["list_threads", "create_sidebar_section", "move_thread_to_sidebar_section"][..], true; "advertised")]
    #[test_case::test_case(&[][..], true; "no_registrations")]
    #[test_case::test_case(&["list_threads"][..], true; "list_only")]
    #[test_case::test_case(&[][..], false; "move_rejected_after_creation")]
    #[tokio::test]
    async fn explicit_missing_section_is_created_before_placement(
        advertised: &[&str],
        move_succeeds: bool,
    ) {
        let (invocation, events) = desktop_session(advertised).await;
        let fork = ThreadId::new();
        let client = async {
            reply_to_desktop(&invocation, &events, "list_threads", json!({"limit": 50}), json!({
                "threads": [{"id": invocation.session.thread_id(), "kind": "codex", "hostId": "remote"}],
                "sections": [],
            }), true).await;
            reply_to_desktop(
                &invocation,
                &events,
                "create_sidebar_section",
                json!({"name": "Research"}),
                json!({
                    "sectionId": "created-section", "name": "Research", "itemKeys": [],
                }),
                true,
            )
            .await;
            let placement =
                json!({"threadId": fork, "hostId": "remote", "sectionId": "created-section"});
            reply_to_desktop(
                &invocation,
                &events,
                "move_thread_to_sidebar_section",
                placement.clone(),
                placement,
                move_succeeds,
            )
            .await;
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(place(&invocation, fork, false, Some("Research")), client)
        })
        .await
        .unwrap();
        if move_succeeds {
            assert_eq!(
                outcome,
                Outcome::Placed {
                    section_id: "created-section".to_string(),
                    created: true
                }
            );
        } else {
            let Outcome::Failed { reason } = outcome else {
                panic!("unconfirmed placement must not report success")
            };
            assert!(reason.contains("created-section"));
            assert!(reason.contains("created: true"));
        }
    }

    /// A rejected or mismatched creation receipt cannot authorize a move.
    #[test_case::test_case(false, "Research"; "creation_rejected")]
    #[test_case::test_case(true, "Different"; "creation_mismatch")]
    #[tokio::test]
    async fn unconfirmed_creation_does_not_move(success: bool, returned_name: &str) {
        let (invocation, events) = desktop_session(&[]).await;
        let fork = ThreadId::new();
        let client = async {
            reply_to_desktop(&invocation, &events, "list_threads", json!({"limit": 50}), json!({
                "threads": [{"id": invocation.session.thread_id(), "kind": "codex", "hostId": "remote"}],
                "sections": [],
            }), true).await;
            reply_to_desktop(
                &invocation,
                &events,
                "create_sidebar_section",
                json!({"name": "Research"}),
                json!({"sectionId": "created", "name": returned_name, "itemKeys": []}),
                success,
            )
            .await;
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(place(&invocation, fork, false, Some("Research")), client)
        })
        .await
        .unwrap();
        assert!(matches!(result, Outcome::Failed { .. }));
    }

    /// Ambiguous names must not select or create another section.
    #[tokio::test]
    async fn duplicate_names_are_reported_without_mutation() {
        let (invocation, events) = desktop_session(&["create_sidebar_section"]).await;
        let listing: Listing = serde_json::from_value(json!({"threads": [], "sections": [
            {"sectionId": "one", "name": "Research", "itemKeys": []},
            {"sectionId": "two", "name": "Research", "itemKeys": []},
        ]}))
        .unwrap();
        let error = named_destination(&invocation, &listing, "Research")
            .await
            .unwrap_err();
        assert!(error.contains("multiple sections"));
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.msg, EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::DynamicToolCall(_)))
            );
        }
    }

    /// A missing catalog still permits inheritance through the client transport.
    #[test_case::test_case(&[][..], "source"; "no_registrations")]
    #[test_case::test_case(&["list_threads"][..], "source"; "list_only")]
    #[test_case::test_case(&[][..], "wrong"; "mismatched_move_receipt")]
    #[tokio::test]
    async fn missing_registrations_do_not_skip_inheritance(
        advertised: &[&str],
        returned_section: &str,
    ) {
        let (invocation, events) = desktop_session(advertised).await;
        let fork = ThreadId::new();
        let client = async {
            reply_to_desktop(
                &invocation,
                &events,
                "list_threads",
                json!({"limit": 50}),
                json!({
                    "threads": [{"id": invocation.session.thread_id(), "kind": "codex", "hostId": "remote"}],
                    "sections": [{"sectionId": "source", "name": "Source", "itemKeys": [format!("codex:thread:local:{}", invocation.session.thread_id())]}],
                }),
                true,
            )
            .await;
            let placement = json!({"threadId": fork, "hostId": "remote", "sectionId": "source"});
            let receipt =
                json!({"threadId": fork, "hostId": "remote", "sectionId": returned_section});
            reply_to_desktop(
                &invocation,
                &events,
                "move_thread_to_sidebar_section",
                placement,
                receipt,
                true,
            )
            .await;
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(place(&invocation, fork, true, None), client)
        })
        .await
        .unwrap();
        if returned_section == "source" {
            assert_eq!(
                outcome,
                Outcome::Inherited {
                    section_id: "source".to_string()
                }
            );
        } else {
            assert!(matches!(outcome, Outcome::Failed { .. }));
        }
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.msg, EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::DynamicToolCall(_)))
            );
        }
    }

    /// Opens an active source turn with the supplied Desktop capabilities.
    async fn desktop_session(
        names: &[&str],
    ) -> (
        ToolInvocation,
        async_channel::Receiver<codex_protocol::protocol::Event>,
    ) {
        let tools = names
            .iter()
            .map(|name| {
                DynamicToolSpec::Function(DynamicToolFunctionSpec {
                    name: format!("codex_app__{name}"),
                    description: name.to_string(),
                    input_schema: json!({"type": "object"}),
                    defer_loading: false,
                })
            })
            .collect();
        let (session, turn, events) =
            make_session_and_context_with_dynamic_tools_and_rx(tools).await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        (
            ToolInvocation {
                session,
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
                call_id: "explicit-section-test".to_string(),
                tool_name: ToolName::namespaced("saffron", "fork_thread"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            events,
        )
    }

    /// Checks the next Desktop request and answers through the client transport.
    async fn reply_to_desktop(
        invocation: &ToolInvocation,
        events: &async_channel::Receiver<codex_protocol::protocol::Event>,
        tool: &str,
        arguments: Value,
        response: Value,
        success: bool,
    ) {
        loop {
            let event = events.recv().await.unwrap();
            let EventMsg::ItemStarted(event) = event.msg else {
                continue;
            };
            let TurnItem::DynamicToolCall(call) = event.item else {
                continue;
            };
            let legacy_name = format!("codex_app__{tool}");
            let legacy_advertised = invocation.turn.dynamic_tools.iter().any(|spec| {
                matches!(spec, DynamicToolSpec::Function(function) if function.name == legacy_name)
            });
            if legacy_advertised {
                assert_eq!(call.namespace, None);
                assert_eq!(call.tool, legacy_name);
            } else {
                assert_eq!(call.namespace.as_deref(), Some("codex_app"));
                assert_eq!(call.tool, tool);
            }
            assert_eq!(call.arguments, arguments);
            invocation
                .session
                .notify_dynamic_tool_response(
                    &call.id,
                    DynamicToolResponse {
                        content_items: vec![DynamicToolCallOutputContentItem::InputText {
                            text: response.to_string(),
                        }],
                        success,
                    },
                )
                .await;
            return;
        }
    }

    /// Without registrations or a client response, placement ends at its deadline.
    #[tokio::test]
    async fn timed_out_request_finishes_pending_dynamic_call() {
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        let invocation = ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            call_id: "timeout-test".to_string(),
            tool_name: ToolName::namespaced("saffron", "fork_thread"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        tokio::time::pause();
        let result = place(&invocation, ThreadId::new(), true, None).await;
        let Outcome::Failed { reason } = result else {
            panic!("unconfirmed placement must report failure, got {result:?}");
        };
        assert!(reason.contains("outcome is unconfirmed"));
        loop {
            let event = events.try_recv().unwrap();
            if let EventMsg::ItemCompleted(event) = event.msg
                && let TurnItem::DynamicToolCall(call) = event.item
            {
                assert_eq!(call.success, Some(false));
                assert_eq!(call.id, "timeout-test-section-list");
                break;
            }
        }
    }

    /// Opting out must not perform even a sidebar lookup when tools are available.
    #[tokio::test]
    async fn opt_out_makes_no_desktop_request() {
        let tools = ["list_threads", "move_thread_to_sidebar_section"]
            .into_iter()
            .map(|name| {
                DynamicToolSpec::Function(DynamicToolFunctionSpec {
                    name: format!("codex_app__{name}"),
                    description: name.to_string(),
                    input_schema: json!({"type": "object"}),
                    defer_loading: false,
                })
            })
            .collect();
        let (session, turn, events) =
            make_session_and_context_with_dynamic_tools_and_rx(tools).await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        let invocation = ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            call_id: "opt-out-test".to_string(),
            tool_name: ToolName::namespaced("saffron", "fork_thread"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        assert_eq!(
            place(&invocation, ThreadId::new(), false, None).await,
            Outcome::Skipped {
                reason: "inherit_section is false".to_string(),
            }
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.msg, EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::DynamicToolCall(_)))
            );
        }
    }

    /// A call whose source turn ended must not wait on an unregistered response.
    #[tokio::test]
    async fn missing_active_turn_returns_without_a_client_request() {
        let (session, turn, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        assert!(session.active_turn.lock().await.is_none());
        let invocation = ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            call_id: "ended-turn-test".to_string(),
            tool_name: ToolName::namespaced("saffron", "fork_thread"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        tokio::time::pause();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            request::<Listing>(
                &invocation,
                ToolName::namespaced("codex_app", "list_threads"),
                json!({}),
                "list",
            ),
        )
        .await
        .expect("ended turns must not wait for a placement timeout");
        assert_eq!(
            result.err().as_deref(),
            Some("Desktop placement request was cancelled")
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.msg, EventMsg::ItemStarted(event) if matches!(event.item, TurnItem::DynamicToolCall(_)))
            );
        }
    }
}
