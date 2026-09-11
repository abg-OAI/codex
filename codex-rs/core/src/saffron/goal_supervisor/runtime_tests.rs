use super::*;

use pretty_assertions::assert_eq;

#[test]
fn failure_retry_is_exponential_and_capped() {
    assert_eq!(failure_retry_delay(1), Duration::from_secs(60));
    assert_eq!(failure_retry_delay(2), Duration::from_secs(120));
    assert_eq!(failure_retry_delay(7), MAX_FAILURE_RETRY);
    assert_eq!(failure_retry_delay(u32::MAX), MAX_FAILURE_RETRY);
}

#[test]
fn checkin_prompt_bounds_large_goal_objectives() {
    let objective = "long objective ".repeat(2_000);

    let prompt = render_checkin_prompt(ThreadId::new(), &objective, "{}");

    assert!(prompt.starts_with("# Supervisor Check-in"));
    assert!(prompt.len() < objective.len());
}
