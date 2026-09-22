use super::*;

use chrono::DateTime;
use chrono::Utc;
use pretty_assertions::assert_eq;

#[test]
fn failure_retry_is_exponential_and_capped() {
    assert_eq!(failure_retry_delay(1), Duration::from_secs(60));
    assert_eq!(failure_retry_delay(2), Duration::from_secs(120));
    assert_eq!(failure_retry_delay(7), MAX_FAILURE_RETRY);
    assert_eq!(failure_retry_delay(u32::MAX), MAX_FAILURE_RETRY);
}

#[test]
fn each_checkin_prompt_includes_its_current_time() {
    let parent_id =
        ThreadId::from_string("018f0000-0000-7000-8000-000000000001").expect("thread id");
    let objective = "Release when CI becomes green.";
    let continuity =
        r#"{"goal_updated_at":1787288448,"previous_action":{"snooze":{"delay_seconds":1477}}}"#;
    let first_checkin_time: DateTime<Utc> = "2026-08-21T05:00:48Z".parse().expect("UTC time");
    let next_checkin_time: DateTime<Utc> = "2026-08-21T05:25:25Z".parse().expect("UTC time");

    let prompts = [
        render_checkin_prompt(parent_id, first_checkin_time, objective, continuity),
        render_checkin_prompt(parent_id, next_checkin_time, objective, continuity),
    ];

    assert_eq!(
        prompts,
        [
            "# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:00:48 UTC\n\nParent thread: 018f0000-0000-7000-8000-000000000001\n\nActive goal:\nRelease when CI becomes green.\n\nContinuity:\n{\"goal_updated_at\":1787288448,\"previous_action\":{\"snooze\":{\"delay_seconds\":1477}}}"
                .to_string(),
            "# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:25:25 UTC\n\nParent thread: 018f0000-0000-7000-8000-000000000001\n\nActive goal:\nRelease when CI becomes green.\n\nContinuity:\n{\"goal_updated_at\":1787288448,\"previous_action\":{\"snooze\":{\"delay_seconds\":1477}}}"
                .to_string(),
        ]
    );
}

#[test]
fn checkin_prompt_bounds_large_goal_objectives_without_losing_current_time() {
    let objective = "long objective ".repeat(2_000);
    let checkin_time: DateTime<Utc> = "2026-08-21T05:25:25Z".parse().expect("UTC time");

    let prompt = render_checkin_prompt(ThreadId::new(), checkin_time, &objective, "{}");

    assert!(
        prompt.starts_with("# Supervisor Check-in\n\nCurrent UTC time: 2026-08-21 05:25:25 UTC")
    );
    assert!(prompt.len() < objective.len());
}
