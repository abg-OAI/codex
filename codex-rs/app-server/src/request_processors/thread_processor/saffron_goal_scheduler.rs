//! App-server adapter for Saffron's process-level goal scheduler.
//!
//! This module owns only app-server mechanics: cold resume, listener
//! attachment, idle lifecycle entry, reconciliation with a loaded runtime, and
//! safe release of roots materialized without a subscriber. Discovery,
//! eligibility, ownership, and retry policy remain in `codex_core`'s Saffron
//! scheduler.

use super::*;
use codex_core::SaffronGoalSchedule;

const MATERIALIZED_RELEASE_POLL_INTERVAL: Duration = Duration::from_secs(1);

impl ThreadRequestProcessor {
    /// Restores one scheduled root to its ordinary idle lifecycle.
    pub(crate) async fn activate_saffron_goal_schedule(
        &self,
        schedule: SaffronGoalSchedule,
    ) -> Result<(), String> {
        self.activate_saffron_goal_schedule_inner(schedule)
            .await
            .map_err(|error| error.message)
    }

    async fn activate_saffron_goal_schedule_inner(
        &self,
        schedule: SaffronGoalSchedule,
    ) -> Result<(), JSONRPCErrorError> {
        let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
        let thread_id = schedule.thread_id();
        if self
            .pending_thread_unloads
            .lock()
            .await
            .contains(&thread_id)
        {
            return Err(invalid_request(format!(
                "thread {thread_id} is closing; retry goal recovery after it closes"
            )));
        }

        if let Ok(observed_thread) = self.thread_manager.get_thread(thread_id).await
            && let Ok(thread) = self.thread_manager.get_thread(thread_id).await
            && Arc::ptr_eq(&observed_thread, &thread)
        {
            self.ensure_background_listener_for_saffron_goal(thread_id, Arc::clone(&thread))
                .await?;
            if thread.should_retain_while_idle().await {
                // A live helper or process-local wake already owns this
                // runtime's next transition. Re-emitting idle would replace
                // its wake task, so periodic probes leave it untouched.
                return Ok(());
            }
            thread
                .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
                .await;
            return Ok(());
        }

        let thread_id_string = thread_id.to_string();
        let stored_thread = self
            .read_stored_thread_for_resume(
                &thread_id_string,
                /*path*/ None,
                /*include_history*/ false,
            )
            .await?;
        let (thread_history, _stored_thread) = self
            .load_resume_initial_history_from_stored_thread(stored_thread)
            .await?;
        let history_cwd = thread_history.session_cwd();
        let mut request_overrides = None;
        let mut typesafe_overrides = ConfigOverrides {
            codex_linux_sandbox_exe: self.arg0_paths.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: self.arg0_paths.main_execve_wrapper_exe.clone(),
            ..Default::default()
        };
        let has_explicit_model_resume_override =
            has_model_resume_override(request_overrides.as_ref(), &typesafe_overrides);
        let persisted_metadata = self
            .load_and_apply_persisted_resume_metadata(
                &thread_history,
                &mut request_overrides,
                &mut typesafe_overrides,
            )
            .await;
        let mut config = self
            .config_manager
            .load_for_cwd(request_overrides, typesafe_overrides, history_cwd)
            .await
            .map_err(|error| config_load_error(&error))?;
        if !has_explicit_model_resume_override {
            preserve_unset_persisted_reasoning_effort(&mut config, persisted_metadata.as_ref());
        }

        let NewThread {
            thread_id: resumed_thread_id,
            thread,
            ..
        } = self
            .thread_manager
            .resume_thread_with_history(
                config,
                thread_history,
                self.auth_manager.clone(),
                /*parent_trace*/ None,
                Default::default(),
            )
            .await
            .map_err(|error| internal_error(format!("error resuming thread: {error}")))?;
        if resumed_thread_id != thread_id {
            return Err(internal_error(format!(
                "goal recovery resumed thread {resumed_thread_id} for expected thread {thread_id}"
            )));
        }

        self.ensure_background_listener_for_saffron_goal(thread_id, Arc::clone(&thread))
            .await?;
        thread
            .emit_thread_idle_lifecycle_if_idle(ThreadIdleCause::Completed)
            .await;

        let processor = self.clone();
        tokio::spawn(async move {
            processor
                .release_saffron_materialized_thread(schedule, thread)
                .await;
        });
        Ok(())
    }

    async fn ensure_background_listener_for_saffron_goal(
        &self,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
    ) -> Result<(), JSONRPCErrorError> {
        self.thread_watch_manager
            .upsert_thread(&thread_id.to_string())
            .await;
        let thread_state = self.thread_state_manager.thread_state(thread_id).await;
        self.ensure_listener_task_running(thread_id, thread, thread_state)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "subscriber attachment and scheduler-owned unload must be serialized"
    )]
    async fn release_saffron_materialized_thread(
        &self,
        schedule: SaffronGoalSchedule,
        thread: Arc<CodexThread>,
    ) {
        let Some(state_db) = self.state_db.as_ref() else {
            return;
        };
        loop {
            tokio::time::sleep(MATERIALIZED_RELEASE_POLL_INTERVAL).await;
            let thread_id = schedule.thread_id();
            if !self
                .thread_state_manager
                .subscribed_connection_ids(thread_id)
                .await
                .is_empty()
            {
                return;
            }
            let Ok(loaded_thread) = self.thread_manager.get_thread(thread_id).await else {
                return;
            };
            if !Arc::ptr_eq(&loaded_thread, &thread) {
                return;
            }

            let current_goal = match state_db.thread_goals().get_thread_goal(thread_id).await {
                Ok(current_goal) => current_goal,
                Err(error) => {
                    warn!(
                        %thread_id,
                        "failed to inspect scheduler-materialized goal before release: {error}"
                    );
                    continue;
                }
            };
            let goal_is_inactive = current_goal
                .as_ref()
                .is_none_or(|goal| goal.status != codex_state::ThreadGoalStatus::Active);
            let reconstructible_snooze = thread.has_reconstructible_saffron_goal_snooze().await;
            if (!goal_is_inactive && !reconstructible_snooze)
                || matches!(thread.agent_status().await, AgentStatus::Running)
            {
                continue;
            }

            {
                let mut pending_thread_unloads = self.pending_thread_unloads.lock().await;
                if !self
                    .thread_state_manager
                    .subscribed_connection_ids(thread_id)
                    .await
                    .is_empty()
                    || pending_thread_unloads.contains(&thread_id)
                {
                    return;
                }
                pending_thread_unloads.insert(thread_id);
            }
            super::super::thread_lifecycle::unload_thread_without_subscribers(
                Arc::clone(&self.thread_manager),
                Arc::clone(&self.outgoing),
                Arc::clone(&self.pending_thread_unloads),
                self.thread_state_manager.clone(),
                self.thread_watch_manager.clone(),
                thread_id,
                thread,
            )
            .await;
            return;
        }
    }
}
