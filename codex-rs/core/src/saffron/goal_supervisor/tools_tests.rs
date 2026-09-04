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
