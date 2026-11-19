//! Process-level recovery for durable active root goals.
//!
//! The scheduler treats the goal row as durable intent and the supervisor
//! helper as disposable process state. One app-server process owns a Codex
//! home at a time, discovers active root goals in bounded pages, and asks an
//! app-server adapter to materialize each eligible root. Materialization then
//! enters the ordinary idle lifecycle, which reconstructs a fresh Saffron
//! supervisor from the current goal.
//! Successful activations remain scheduled for bounded periodic reconciliation.
//! Exact snooze deadlines live in Saffron's auxiliary database, so a restarted
//! app-server or an unloaded runtime preserves the original wake time.
//!
//! Ownership uses a cross-platform advisory file lock under the existing
//! temporary Codex-home directory rather than a SQLite lock. Goal changes
//! invalidate queued work through [`GoalSchedule`] identity, and eligibility
//! is checked again after activation capacity becomes available.

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSource;
use codex_state::StateRuntime;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::warn;

use super::storage::SaffronStore;

const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const INITIAL_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PAGE_SIZE: usize = 256;
const MAX_CONCURRENT_ACTIVATIONS: usize = 4;
const OWNERSHIP_LOCK_FILE: &str = "saffron-goal-scheduler.lock";

/// Durable identity of one active goal eligible for restart recovery.
///
/// Goal ID, objective, revision timestamp, and optional wake deadline identify
/// the scheduled work. An adapter must activate only the represented root
/// thread; the scheduler revalidates the identity and root eligibility
/// immediately before calling it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalSchedule {
    thread_id: ThreadId,
    goal_id: String,
    objective: String,
    updated_at_millis: i64,
    wake_at_millis: Option<i64>,
}

impl GoalSchedule {
    /// Returns the root thread whose idle lifecycle should be restored.
    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    async fn from_goal(
        goal: &codex_state::ThreadGoal,
        store: &SaffronStore,
    ) -> Result<Self, String> {
        let updated_at_millis = goal.updated_at.timestamp_millis();
        let wake = store
            .get_goal_wake(goal.thread_id)
            .await
            .map_err(|error| error.to_string())?;
        let wake_at_millis = match wake {
            Some(wake)
                if wake.goal_id == goal.goal_id
                    && wake.goal_objective == goal.objective
                    && wake.goal_updated_at_ms == updated_at_millis =>
            {
                Some(wake.wake_at_ms)
            }
            Some(_) => None,
            None => None,
        };
        Ok(Self {
            thread_id: goal.thread_id,
            goal_id: goal.goal_id.clone(),
            objective: goal.objective.clone(),
            updated_at_millis,
            wake_at_millis,
        })
    }
}

/// App-server adapter that materializes a scheduled root and enters idle.
pub type GoalActivator = Arc<
    dyn Fn(GoalSchedule) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>
        + Send
        + Sync
        + 'static,
>;

/// Owns automatic active-goal recovery for one app-server process.
///
/// Dropping or stopping the handle cancels discovery and every activation.
/// Another process sharing the same Codex home remains excluded until this
/// process releases its advisory ownership lock.
pub struct GoalSchedulerHandle {
    task: JoinHandle<()>,
}

impl GoalSchedulerHandle {
    /// Starts discovery and activation against the supplied state database.
    pub fn start(state_db: Arc<StateRuntime>, activator: GoalActivator) -> Self {
        let task = tokio::spawn(run_scheduler(state_db, activator));
        Self { task }
    }

    /// Stops discovery and releases process ownership.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for GoalSchedulerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ScheduledActivation {
    schedule: GoalSchedule,
    // `None` means the latest activation settled and the next scan may probe
    // the app-server again. A loaded runtime with a live helper or snooze
    // rejects that probe before it can disturb session-local scheduling.
    cancellation: Option<CancellationToken>,
}

type OwnedActivation = Pin<Box<dyn Future<Output = GoalSchedule> + Send + 'static>>;

async fn run_scheduler(state_db: Arc<StateRuntime>, activator: GoalActivator) {
    loop {
        match try_acquire_ownership(state_db.sqlite().home()) {
            Ok(Some(ownership)) => {
                debug!(
                    sqlite_home = %state_db.sqlite().home().display(),
                    "acquired Saffron goal scheduler ownership"
                );
                match SaffronStore::open(state_db.sqlite()).await {
                    Ok(store) => {
                        run_as_owner(state_db, store, activator, ownership).await;
                        return;
                    }
                    Err(error) => warn!(
                        sqlite_home = %state_db.sqlite().home().display(),
                        "failed to open Saffron goal scheduler storage: {error}"
                    ),
                }
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    sqlite_home = %state_db.sqlite().home().display(),
                    "failed to acquire Saffron goal scheduler ownership: {error}"
                );
            }
        }
        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}

fn try_acquire_ownership(sqlite_home: &Path) -> io::Result<Option<File>> {
    // A separate file avoids interfering with SQLite's own byte-range locks.
    let directory = sqlite_home.join(".tmp");
    fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(OWNERSHIP_LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

async fn run_as_owner(
    state_db: Arc<StateRuntime>,
    store: SaffronStore,
    activator: GoalActivator,
    _ownership: File,
) {
    // Keep activation futures local to the ownership future. Cancellation drops
    // them before this function's ownership guard, so no adapter call can
    // survive into the next owner's tenure.
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_ACTIVATIONS));
    let mut scheduled = HashMap::<ThreadId, ScheduledActivation>::new();
    let mut activations = FuturesUnordered::<OwnedActivation>::new();
    let mut scan_interval = tokio::time::interval(SCAN_INTERVAL);
    let mut wake_notifications = SaffronStore::subscribe();

    loop {
        tokio::select! {
            _ = async {
                tokio::select! {
                    _ = scan_interval.tick() => {}
                    _ = wake_notifications.changed() => {}
                }
            } => {
                match scan_eligible_goals(state_db.as_ref(), &store).await {
                    Ok(current) => {
                        scheduled.retain(|thread_id, activation| {
                            let unchanged = current
                                .get(thread_id)
                                .is_some_and(|schedule| schedule == &activation.schedule);
                            if !unchanged
                                && let Some(cancellation) = activation.cancellation.as_ref()
                            {
                                cancellation.cancel();
                            }
                            unchanged
                        });

                        for (thread_id, schedule) in current {
                            let cancellation = CancellationToken::new();
                            match scheduled.entry(thread_id) {
                                std::collections::hash_map::Entry::Occupied(mut entry) => {
                                    if entry.get().cancellation.is_some() {
                                        continue;
                                    }
                                    entry.get_mut().cancellation = Some(cancellation.clone());
                                }
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    entry.insert(ScheduledActivation {
                                        schedule: schedule.clone(),
                                        cancellation: Some(cancellation.clone()),
                                    });
                                }
                            }
                            let completed_schedule = schedule.clone();
                            activations.push(Box::pin(run_activation(
                                Arc::clone(&state_db),
                                store.clone(),
                                Arc::clone(&activator),
                                Arc::clone(&permits),
                                schedule.clone(),
                                cancellation.clone(),
                            )
                            .map(move |()| completed_schedule)));
                        }
                    }
                    Err(error) => warn!("failed to scan active goals for restart recovery: {error}"),
                }
            }
            Some(completed) = activations.next(), if !activations.is_empty() => {
                if let Some(activation) = scheduled.get_mut(&completed.thread_id)
                    && activation.schedule == completed
                {
                    activation.cancellation = None;
                }
            }
        }
    }
}

async fn scan_eligible_goals(
    state_db: &StateRuntime,
    store: &SaffronStore,
) -> Result<HashMap<ThreadId, GoalSchedule>, String> {
    let mut schedules = HashMap::new();
    let mut cursor = None;
    loop {
        let goals = state_db
            .thread_goals()
            .list_active_thread_goals(cursor, PAGE_SIZE)
            .await
            .map_err(|error| error.to_string())?;
        let page_is_full = goals.len() == PAGE_SIZE;
        for goal in goals {
            cursor = Some(goal.thread_id);
            let schedule = GoalSchedule::from_goal(&goal, store).await?;
            if schedule_is_eligible(state_db, &schedule).await? {
                schedules.insert(schedule.thread_id, schedule);
            }
        }
        if !page_is_full {
            break;
        }
    }
    Ok(schedules)
}

async fn run_activation(
    state_db: Arc<StateRuntime>,
    store: SaffronStore,
    activator: GoalActivator,
    permits: Arc<Semaphore>,
    schedule: GoalSchedule,
    cancellation: CancellationToken,
) {
    if let Some(wake_at_millis) = schedule.wake_at_millis {
        loop {
            let remaining_millis = wake_at_millis.saturating_sub(Utc::now().timestamp_millis());
            let Ok(remaining_millis) = u64::try_from(remaining_millis) else {
                break;
            };
            let remaining = Duration::from_millis(remaining_millis);
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = tokio::time::sleep(remaining.min(SCAN_INTERVAL)) => {}
            }
        }
    }

    let mut retry_interval = INITIAL_RETRY_INTERVAL;
    loop {
        let permit = match tokio::select! {
            _ = cancellation.cancelled() => return,
            result = Arc::clone(&permits).acquire_owned() => result,
        } {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let current = match current_schedule(state_db.as_ref(), &store, schedule.thread_id).await {
            Ok(current) => current,
            Err(error) => {
                drop(permit);
                warn!(
                    thread_id = %schedule.thread_id,
                    "failed to revalidate queued goal restart recovery: {error}"
                );
                if cancelled_during_retry(retry_interval, &cancellation).await {
                    return;
                }
                retry_interval = next_retry_interval(retry_interval);
                continue;
            }
        };
        if current.as_ref() != Some(&schedule) {
            return;
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = activator(schedule.clone()) => result,
        };
        drop(permit);
        let Err(error) = result else {
            return;
        };
        warn!(
            thread_id = %schedule.thread_id,
            goal_id = %schedule.goal_id,
            "failed to activate goal restart recovery: {error}"
        );
        if cancelled_during_retry(retry_interval, &cancellation).await {
            return;
        }
        retry_interval = next_retry_interval(retry_interval);
    }
}

async fn cancelled_during_retry(
    retry_interval: Duration,
    cancellation: &CancellationToken,
) -> bool {
    tokio::select! {
        _ = cancellation.cancelled() => true,
        _ = tokio::time::sleep(retry_interval) => false,
    }
}

async fn current_schedule(
    state_db: &StateRuntime,
    store: &SaffronStore,
    thread_id: ThreadId,
) -> Result<Option<GoalSchedule>, String> {
    let Some(goal) = state_db
        .thread_goals()
        .get_thread_goal(thread_id)
        .await
        .map_err(|error| error.to_string())?
        .filter(|goal| goal.status == codex_state::ThreadGoalStatus::Active)
    else {
        return Ok(None);
    };
    let schedule = GoalSchedule::from_goal(&goal, store).await?;
    if !schedule_is_eligible(state_db, &schedule).await? {
        return Ok(None);
    }
    Ok(Some(schedule))
}

async fn schedule_is_eligible(
    state_db: &StateRuntime,
    schedule: &GoalSchedule,
) -> Result<bool, String> {
    if state_db
        .thread_goals()
        .has_thread_goal_continuation_deferral(schedule.thread_id)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(false);
    }
    let Some(metadata) = state_db
        .get_thread(schedule.thread_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    if metadata.archived_at.is_some() {
        return Ok(false);
    }
    if matches!(metadata.thread_source, Some(ThreadSource::Subagent)) {
        return Ok(false);
    }
    let source = serde_json::from_str::<SessionSource>(&metadata.source)
        .or_else(|_| serde_json::from_value(serde_json::Value::String(metadata.source)));
    Ok(!matches!(source, Ok(SessionSource::SubAgent(_))))
}

fn next_retry_interval(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_INTERVAL)
}

#[cfg(test)]
#[path = "goal_scheduler_tests.rs"]
mod tests;
