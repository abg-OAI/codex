use super::*;

use pretty_assertions::assert_eq;

#[test]
fn action_specs_share_the_saffron_namespace() {
    let names = [Kind::Followup, Kind::Snooze, Kind::Compact, Kind::Complete]
        .into_iter()
        .map(|kind| {
            let ToolSpec::Namespace(namespace) = Handler::new(kind).spec() else {
                panic!("supervisor action must be a namespace tool");
            };
            assert_eq!(namespace.name, NAMESPACE);
            let [ResponsesApiNamespaceTool::Function(tool)] = namespace.tools.as_slice() else {
                panic!("supervisor action must expose exactly one function");
            };
            tool.name.clone()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "supervisor_followup_parent",
            "supervisor_snooze",
            "supervisor_compact_parent_context",
            "supervisor_close_self",
        ]
    );
}

#[test]
fn snooze_spec_requires_an_external_wait_reason() {
    let ToolSpec::Namespace(namespace) = Handler::new(Kind::Snooze).spec() else {
        panic!("supervisor snooze must be a namespace tool");
    };
    let [ResponsesApiNamespaceTool::Function(tool)] = namespace.tools.as_slice() else {
        panic!("supervisor snooze must expose one function");
    };

    let parameters = serde_json::to_value(&tool.parameters).expect("serializable snooze schema");
    assert_eq!(
        parameters["required"],
        serde_json::json!(["delay_seconds", "reason"])
    );
    assert_eq!(parameters["properties"]["delay_seconds"]["type"], "integer");
    assert_eq!(parameters["properties"]["reason"]["type"], "string");
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn snooze_args_reject_a_missing_reason() {
    assert!(
        parse_arguments::<SnoozeArgs>(r#"{"delay_seconds":300}"#).is_err(),
        "snooze calls must explain the external wait"
    );
}
