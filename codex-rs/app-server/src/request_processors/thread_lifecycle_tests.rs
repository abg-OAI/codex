use std::time::Duration;

use codex_app_server_protocol::ThreadStatus;
use tokio::sync::watch;

use super::*;

#[test]
fn observed_activity_restarts_ordinary_unload_timer() {
    let delay = Duration::from_secs(30 * 60);
    let (_has_subscribers_tx, has_subscribers_rx) = watch::channel(false);
    let (_thread_status_tx, thread_status_rx) = watch::channel(ThreadStatus::Idle);
    let idle_since = Instant::now() - delay;
    let mut unloading_state = UnloadingState {
        delay,
        has_subscribers_rx,
        has_subscribers: (false, idle_since),
        thread_status_rx,
        is_active: (false, idle_since),
    };

    assert!(unloading_state.should_unload_now());
    unloading_state.note_thread_activity_observed();
    assert!(!unloading_state.should_unload_now());

    unloading_state.is_active.1 = idle_since;
    assert!(unloading_state.should_unload_now());
}
