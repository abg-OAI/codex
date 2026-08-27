use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use codex_protocol::protocol::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tokio::time::Instant;

use super::*;
use crate::unified_exec::ProcessEntry;

const PROCESS_ID: i32 = 1000;

async fn manager_with_live_process() -> (UnifiedExecProcessManager, Arc<UnifiedExecProcess>) {
    let manager = UnifiedExecProcessManager::default();
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let mut store = manager.process_store.lock().await;
    store.reserved_process_ids.insert(PROCESS_ID);
    store.processes.insert(
        PROCESS_ID,
        ProcessEntry {
            process: Arc::clone(&process),
            plugin_metrics_sidecar: None,
            call_id: "call-1".to_string(),
            process_id: PROCESS_ID,
            cwd: PathUri::parse("file:///tmp").expect("test cwd should be valid"),
            initial_exec_command_active: Arc::new(AtomicBool::new(false)),
            hook_command: "sleep 60".to_string(),
            tty: false,
            network_approval: None,
            session: Weak::new(),
            last_used: Instant::now(),
        },
    );
    drop(store);
    (manager, process)
}

#[tokio::test]
async fn acquisition_timeout_rejects_a_removed_process() {
    let (manager, process) = manager_with_live_process().await;
    let interaction_guard = process.interaction_lock().lock_owned().await;
    let deadline = Instant::now() + Duration::from_millis(20);

    let (result, removed) = tokio::join!(
        manager.begin_process_interaction(PROCESS_ID, Some(deadline)),
        async {
            tokio::task::yield_now().await;
            manager.process_store.lock().await.remove(PROCESS_ID)
        }
    );
    drop(interaction_guard);

    assert!(removed.is_some());
    assert!(matches!(
        result,
        Err(UnifiedExecError::UnknownProcessId {
            process_id: PROCESS_ID
        })
    ));
}

#[tokio::test]
async fn cancelled_finalization_leaves_output_for_the_next_interaction() {
    let (manager, process) = manager_with_live_process().await;
    process
        .output_handles()
        .output_buffer
        .lock()
        .await
        .push_chunk(b"retained\n");
    let interaction = match manager
        .begin_process_interaction(PROCESS_ID, /*deadline*/ None)
        .await
        .expect("process should be available")
    {
        ProcessInteractionAcquisition::Acquired(interaction) => interaction,
        ProcessInteractionAcquisition::TimedOut(_) => {
            panic!("an interaction without a deadline cannot time out")
        }
    };

    let output_buffer = Arc::clone(&process.output_handles().output_buffer);
    let (lock_acquired_tx, lock_acquired_rx) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn(move || {
        let _output_lock = output_buffer.blocking_lock();
        lock_acquired_tx
            .send(())
            .expect("test should still be waiting for the output lock");
        std::thread::sleep(Duration::from_millis(50));
    });
    lock_acquired_rx
        .recv()
        .expect("output-lock thread should start");
    {
        let finish = interaction.finish_and_drain();
        tokio::pin!(finish);
        tokio::select! {
            _ = &mut finish => panic!("finalization unexpectedly completed"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    lock_thread
        .join()
        .expect("output-lock thread should finish");

    let interaction = match manager
        .begin_process_interaction(PROCESS_ID, /*deadline*/ None)
        .await
        .expect("cancelled finalization should leave the process available")
    {
        ProcessInteractionAcquisition::Acquired(interaction) => interaction,
        ProcessInteractionAcquisition::TimedOut(_) => {
            panic!("an interaction without a deadline cannot time out")
        }
    };
    let output = interaction
        .finish_and_drain()
        .await
        .expect("the next interaction should finish")
        .into_tool_output(
            Duration::ZERO,
            TruncationPolicy::Tokens(1_000),
            /*max_output_tokens*/ None,
        );

    assert_eq!(output.raw_output, b"retained\n");
    assert_eq!(output.process_id, Some(PROCESS_ID));
}
