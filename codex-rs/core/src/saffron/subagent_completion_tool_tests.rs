use super::*;

#[test]
fn tool_description_distinguishes_actionable_and_deferrable_results() {
    let ToolSpec::Namespace(namespace) = Handler.spec() else {
        panic!("completion delivery should be a namespace tool");
    };
    let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0] else {
        panic!("completion delivery should be a function tool");
    };

    assert!(tool.description.contains("useful action now"));
    assert!(
        tool.description
            .contains("immediate parent attention adds little value")
    );
    assert!(tool.description.contains("abnormal termination wake"));
    assert!(tool.description.contains("defaults to wake_parent"));
}

#[test]
fn delivery_argument_accepts_both_supported_choices() {
    for (arguments, expected) in [
        (
            r#"{"delivery":"wake_parent"}"#,
            CompletionDelivery::WakeParent,
        ),
        (
            r#"{"delivery":"defer_to_parent"}"#,
            CompletionDelivery::DeferToParent,
        ),
    ] {
        let args: SetCompletionDeliveryArgs =
            serde_json::from_str(arguments).expect("supported delivery choice");
        assert_eq!(CompletionDelivery::from(args.delivery), expected);
    }
}
