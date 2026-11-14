use super::*;

use pretty_assertions::assert_eq;

#[test]
fn persistent_guidance_is_emitted_once_for_unchanged_state() {
    let section = guidance_section(Some(INSTRUCTIONS));
    let (snapshot, initial) = section.render_diff(PreviousWorldStateSection::Absent);
    let snapshot = snapshot.expect("guidance snapshot");
    let initial = initial.expect("initial guidance");

    assert_eq!(snapshot, json!({ "instructions": INSTRUCTIONS }));
    assert_eq!(initial.body(), &input_text(format!("\n{INSTRUCTIONS}\n")));
    assert!(section.matches_retained_fragment("developer", &rendered(initial)));
    let (unchanged_snapshot, update) =
        section.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(unchanged_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(update, None);
}

#[test]
fn guidance_is_replaced_or_retired_when_configuration_changes() {
    let enabled = guidance_section(Some(INSTRUCTIONS));
    let disabled = guidance_section(None);

    let (snapshot, replaced) = enabled.render_diff(PreviousWorldStateSection::Unknown);
    let snapshot = snapshot.expect("enabled guidance snapshot");
    let replaced = replaced.expect("replacement guidance");
    assert_eq!(
        replaced.body(),
        &input_text(format!("\n{REPLACEMENT_NOTICE}\n\n{INSTRUCTIONS}\n"))
    );

    let (snapshot, retired) = disabled.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(snapshot, Some(json!({ "instructions": null })));
    let retired = retired.expect("retirement guidance");
    assert_eq!(retired.body(), &input_text(format!("\n{REMOVAL_NOTICE}\n")));
}

#[test]
fn prior_turn_local_guidance_is_recognized_for_replacement() {
    let legacy = format!("{LEGACY_START_MARKER}old guidance{LEGACY_END_MARKER}");

    assert!(guidance_section(Some(INSTRUCTIONS)).matches_legacy_fragment("developer", &legacy));
    assert!(!guidance_section(Some(INSTRUCTIONS)).matches_retained_fragment("developer", &legacy));
}

#[test]
fn guidance_requires_an_eligible_role_and_enabled_goals() {
    for role_instructions in [Some(INSTRUCTIONS), Some(HELPER_INSTRUCTIONS), None] {
        let state = GoalGuidanceState {
            role_instructions,
            goals_enabled: true,
        };
        assert_eq!(state.instructions(), role_instructions);
        assert_eq!(
            GoalGuidanceState {
                goals_enabled: false,
                ..state
            }
            .instructions(),
            None
        );
    }
}

#[test]
fn helper_replaces_inherited_root_guidance() {
    let (root, _) =
        guidance_section(Some(INSTRUCTIONS)).render_diff(PreviousWorldStateSection::Absent);
    let root = root.expect("root guidance snapshot");
    let (_, update) = guidance_section(Some(HELPER_INSTRUCTIONS))
        .render_diff(PreviousWorldStateSection::Known(&root));
    let update = update.expect("helper guidance replaces the parent's");
    assert_eq!(
        update.body(),
        &input_text(format!("\n{REPLACEMENT_NOTICE}\n\n{HELPER_INSTRUCTIONS}\n"))
    );
}

fn rendered(fragment: RenderedWorldStateFragment) -> String {
    let (start, end) = fragment.markers();
    let codex_protocol::models::ContentItem::InputText { text } = fragment.body() else {
        panic!("guidance must contain input text");
    };
    format!("{start}{text}{end}")
}
