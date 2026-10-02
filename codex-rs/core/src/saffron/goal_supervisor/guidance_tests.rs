use super::*;

use pretty_assertions::assert_eq;

#[test]
fn persistent_guidance_is_emitted_once_for_unchanged_state() {
    let section = guidance_section(true);
    let initial = section
        .render_diff(PreviousWorldStateSection::Absent)
        .expect("initial guidance");

    assert_eq!(initial.body(), format!("\n{INSTRUCTIONS}\n"));
    assert!(section.matches_retained_fragment("developer", &rendered(initial)));
    assert_eq!(
        section.render_diff(PreviousWorldStateSection::Known(section.snapshot())),
        None
    );
}

#[test]
fn guidance_is_replaced_or_retired_when_configuration_changes() {
    let enabled = guidance_section(true);
    let disabled = guidance_section(false);

    let replaced = enabled
        .render_diff(PreviousWorldStateSection::Unknown)
        .expect("replacement guidance");
    assert_eq!(
        replaced.body(),
        format!("\n{REPLACEMENT_NOTICE}\n\n{INSTRUCTIONS}\n")
    );

    let retired = disabled
        .render_diff(PreviousWorldStateSection::Known(enabled.snapshot()))
        .expect("retirement guidance");
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
