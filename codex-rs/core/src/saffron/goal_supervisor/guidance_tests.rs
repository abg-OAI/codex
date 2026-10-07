use super::*;

use pretty_assertions::assert_eq;

#[test]
fn persistent_guidance_is_emitted_once_for_unchanged_state() {
    let section = guidance_section(true);
    let (snapshot, initial) = section.render_diff(PreviousWorldStateSection::Absent);
    let snapshot = snapshot.expect("guidance snapshot");
    let initial = initial.expect("initial guidance");

    assert_eq!(snapshot, json!({ "instructions": INSTRUCTIONS }));
    assert_eq!(initial.body(), format!("\n{INSTRUCTIONS}\n"));
    assert!(section.matches_retained_fragment("developer", &rendered(initial)));
    let (unchanged_snapshot, update) =
        section.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(unchanged_snapshot.as_ref(), Some(&snapshot));
    assert_eq!(update, None);
}

#[test]
fn guidance_is_replaced_or_retired_when_configuration_changes() {
    let enabled = guidance_section(true);
    let disabled = guidance_section(false);

    let (snapshot, replaced) = enabled.render_diff(PreviousWorldStateSection::Unknown);
    let snapshot = snapshot.expect("enabled guidance snapshot");
    let replaced = replaced.expect("replacement guidance");
    assert_eq!(
        replaced.body(),
        format!("\n{REPLACEMENT_NOTICE}\n\n{INSTRUCTIONS}\n")
    );

    let (snapshot, retired) = disabled.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(snapshot, Some(json!({ "instructions": null })));
    let retired = retired.expect("retirement guidance");
    assert_eq!(retired.body(), format!("\n{REMOVAL_NOTICE}\n"));
}

#[test]
fn prior_turn_local_guidance_is_recognized_for_replacement() {
    let legacy = format!("{LEGACY_START_MARKER}old guidance{LEGACY_END_MARKER}");

    assert!(guidance_section(true).matches_legacy_fragment("developer", &legacy));
    assert!(!guidance_section(true).matches_retained_fragment("developer", &legacy));
}

#[test]
fn guidance_is_root_scoped() {
    assert!(
        GoalGuidanceState {
            is_root: true,
            goals_enabled: true,
        }
        .enabled()
    );
    assert!(
        !GoalGuidanceState {
            is_root: false,
            goals_enabled: true,
        }
        .enabled()
    );
    assert!(
        !GoalGuidanceState {
            is_root: true,
            goals_enabled: false,
        }
        .enabled()
    );
}

fn rendered(fragment: RenderedWorldStateFragment) -> String {
    let (start, end) = fragment.markers();
    format!("{start}{}{end}", fragment.body())
}
