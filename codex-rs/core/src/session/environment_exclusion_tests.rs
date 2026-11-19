//! Late owner results cannot restore an executor excluded during startup.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn excluded_executor_rejects_late_owner_configuration() {
    let (session, context) = make_session_and_context().await;
    let environment = context.initial_environments.primary().unwrap();
    let owner_config = environment.config().clone();
    let mut excluded = environment.selection.clone();
    excluded.environment_id = "excluded".to_string();
    excluded.config = EnvironmentConfigState::Unavailable;
    session
        .state
        .lock()
        .await
        .session_configuration
        .environments
        .push(excluded.clone());

    let error = session
        .environment_ready(&excluded, owner_config)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unavailable for this thread"));
    assert!(
        session
            .environment_failed(&excluded, "late connection failure".to_string())
            .await
            .is_err()
    );
    let state = session.state.lock().await;
    let retained = state
        .session_configuration
        .environments
        .iter()
        .find(|selection| selection.environment_id == "excluded")
        .unwrap();
    assert_eq!(retained, &excluded);
}
