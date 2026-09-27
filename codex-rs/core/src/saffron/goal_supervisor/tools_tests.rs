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

    assert_eq!(
        tool.parameters,
        JsonSchema::object(
            BTreeMap::from([
                (
                    "delay_seconds".to_string(),
                    JsonSchema::integer(Some(format!(
                        "Whole seconds to a known scheduled boundary or a bounded, evidence-based recheck of an already-running non-human process, from 1 through {MAX_SNOOZE_SECONDS}."
                    ))),
                ),
                (
                    "reason".to_string(),
                    JsonSchema::string(Some(
                        "Concise observed non-human process or scheduled boundary that prevents useful parent action."
                            .to_string(),
                    )),
                ),
            ]),
            Some(vec!["delay_seconds".to_string(), "reason".to_string()]),
            Some(false.into()),
        )
    );
}

#[test]
fn snooze_args_reject_a_missing_reason() {
    assert!(
        parse_arguments::<SnoozeArgs>(r#"{"delay_seconds":300}"#).is_err(),
        "snooze calls must explain the external wait"
    );
}
