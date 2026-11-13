//! Boundary tests use disposable websocket executors and Saffron's own temporary database.

use super::*;
use crate::config::ConfigBuilder;
use crate::config::PermissionProfileSnapshot;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::sandbox::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::SinkExt;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::task::AbortOnDropHandle;

#[tokio::test]
async fn optional_secondaries_share_one_deadline_and_keep_the_local_primary() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.ephemeral = true;
    config.project_doc_max_bytes = 0;
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let (first_url, first_started, _first) = executor(ExecutorBehavior::StallInitialization).await;
    let (second_url, second_started, _second) =
        executor(ExecutorBehavior::StallInitialization).await;
    manager
        .upsert_environment("first".to_string(), first_url, None)
        .unwrap();
    manager
        .upsert_environment("second".to_string(), second_url, None)
        .unwrap();
    let selections = vec![
        selection("local", &config),
        selection("first", &config),
        selection("second", &config),
    ];
    let mut init = ExtensionDataInit::new();
    record_selection_intent(&mut init, true);
    let _clock_guard = freeze_clock();
    let started = Instant::now();
    let task = tokio::spawn(async move {
        prepare(
            &config,
            ThreadId::new(),
            false,
            &init,
            startup(manager, selections),
        )
        .await
    });
    first_started.await.unwrap();
    second_started.await.unwrap();
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(1) + Duration::from_millis(1)).await;
    let admitted = task.await.unwrap().unwrap();
    assert_eq!(
        Instant::now() - started,
        STARTUP_BUDGET + Duration::from_millis(1)
    );
    assert_eq!(admitted.selections[0].environment_id, LOCAL_ENVIRONMENT_ID);
    assert_eq!(
        admitted.selections[0].config,
        EnvironmentConfigState::FromThread
    );
    assert!(
        admitted.selections[1..]
            .iter()
            .all(|selection| selection.config == EnvironmentConfigState::Unavailable)
    );
    assert!(admitted.instructions.is_empty());
    assert!(!home.path().join("saffron_1.sqlite").exists());
}

#[tokio::test]
async fn explicit_executor_fails_when_instruction_discovery_stalls() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.ephemeral = true;
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let (url, discovery_started, _server) = executor(ExecutorBehavior::StallInstructions).await;
    manager
        .upsert_environment("required".to_string(), url, None)
        .unwrap();
    let selections = vec![selection("local", &config), selection("required", &config)];
    let mut init = ExtensionDataInit::new();
    record_selection_intent(&mut init, false);
    let _clock_guard = freeze_clock();
    let task = tokio::spawn(async move {
        prepare(
            &config,
            ThreadId::new(),
            false,
            &init,
            startup(manager, selections),
        )
        .await
    });
    discovery_started.await.unwrap();
    // Tokio rounds timer deadlines up to its next millisecond tick.
    tokio::time::advance(STARTUP_BUDGET + Duration::from_millis(1)).await;
    let error = task
        .await
        .unwrap()
        .err()
        .expect("required executor must fail startup");
    assert!(error.to_string().contains("Required executor `required`"));
}

#[tokio::test]
async fn partial_instruction_reads_do_not_admit_an_optional_executor() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.ephemeral = true;
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let (url, child_read_started, _server) =
        executor(ExecutorBehavior::StallAfterParentInstructions).await;
    manager
        .upsert_environment("partial".to_string(), url, None)
        .unwrap();
    let mut remote = selection("partial", &config);
    remote.cwd = PathUri::from_abs_path(&AbsolutePathBuf::try_from("/project/child").unwrap());
    let selections = vec![selection("local", &config), remote];
    let mut init = ExtensionDataInit::new();
    record_selection_intent(&mut init, true);
    let _clock_guard = freeze_clock();
    let task = tokio::spawn(async move {
        prepare(
            &config,
            ThreadId::new(),
            false,
            &init,
            startup(manager, selections),
        )
        .await
    });
    // The parent AGENTS.md was returned before discovery attempted the child file.
    child_read_started.await.unwrap();
    // Tokio rounds timer deadlines up to its next millisecond tick.
    tokio::time::advance(STARTUP_BUDGET + Duration::from_millis(1)).await;
    let admitted = task.await.unwrap().unwrap();
    assert_eq!(
        admitted.selections[1].config,
        EnvironmentConfigState::Unavailable
    );
    assert!(admitted.instructions.is_empty());
    assert!(admitted.inherited.turn_environments().next().is_none());
}

#[tokio::test]
async fn exhausted_instruction_budget_skips_an_unreachable_stalled_child() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.ephemeral = true;
    config.project_doc_max_bytes = b"parent instruction".len();
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let (url, mut child_read_started, _server) =
        executor(ExecutorBehavior::StallAfterParentInstructions).await;
    manager
        .upsert_environment("bounded".to_string(), url, None)
        .unwrap();
    let mut remote = selection("bounded", &config);
    remote.cwd = PathUri::from_abs_path(&AbsolutePathBuf::try_from("/project/child").unwrap());
    let selections = vec![selection("local", &config), remote];
    let mut init = ExtensionDataInit::new();
    record_selection_intent(&mut init, true);
    let admitted = prepare(
        &config,
        ThreadId::new(),
        false,
        &init,
        startup(manager, selections),
    )
    .await
    .unwrap();
    assert_eq!(admitted.instructions.len(), 1);
    assert_eq!(admitted.inherited.turn_environments().count(), 1);
    assert!(matches!(
        child_read_started.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn cold_resume_preserves_exclusions_but_a_fresh_thread_can_retry() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.project_doc_max_bytes = 0;
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let selections = vec![selection("local", &config), selection("missing", &config)];
    let mut init = ExtensionDataInit::new();
    record_selection_intent(&mut init, true);
    let thread_id = ThreadId::new();
    let admitted = prepare(
        &config,
        thread_id,
        false,
        &init,
        startup(Arc::clone(&manager), selections.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        admitted.selections[1].config,
        EnvironmentConfigState::Unavailable
    );
    drop(admitted);
    let (url, _unused, _server) = executor(ExecutorBehavior::Ready).await;
    manager
        .upsert_environment("missing".to_string(), url, None)
        .unwrap();
    manager
        .get_environment("missing")
        .unwrap()
        .wait_until_ready()
        .await
        .unwrap();
    // Recovery does not restore the old thread's authority, even when defaults omit the ID.
    let resumed = prepare(
        &config,
        thread_id,
        true,
        &init,
        startup(Arc::clone(&manager), vec![selections[0].clone()]),
    )
    .await
    .unwrap();
    assert_eq!(resumed.selections[1].environment_id, "missing");
    assert_eq!(
        resumed.selections[1].config,
        EnvironmentConfigState::Unavailable
    );
    assert!(matches!(
        crate::mcp::McpEnvironmentScope::Selected(&resumed.selections).authority_for("missing"),
        codex_mcp::McpEnvironmentAuthority::Unavailable
    ));
    let child_id = ThreadId::new();
    let mut child_startup = startup(Arc::clone(&manager), selections.clone());
    child_startup.inherited = resumed.inherited.clone();
    let child = prepare(
        &config,
        child_id,
        false,
        &ExtensionDataInit::new(),
        child_startup,
    )
    .await
    .unwrap();
    assert_eq!(
        child.selections[1].config,
        EnvironmentConfigState::Unavailable
    );
    let child_resumed = prepare(
        &config,
        child_id,
        true,
        &init,
        startup(Arc::clone(&manager), vec![selections[0].clone()]),
    )
    .await
    .unwrap();
    assert_eq!(
        child_resumed.selections[1].config,
        EnvironmentConfigState::Unavailable
    );

    let fresh = prepare(
        &config,
        ThreadId::new(),
        false,
        &init,
        startup(manager, selections),
    )
    .await
    .unwrap();
    assert_eq!(fresh.selections.len(), 2);
    assert_eq!(fresh.inherited.turn_environments().count(), 1);
    assert_ne!(
        fresh.selections[1].config,
        EnvironmentConfigState::Unavailable
    );
    let store = SaffronStore::open(config.sqlite_config()).await.unwrap();
    assert_eq!(store.executor_exclusions(thread_id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_pending_owner_keeps_deferred_startup() {
    let home = TempDir::new().unwrap();
    let mut config = config(&home).await;
    config.ephemeral = true;
    config.features.enable(Feature::DeferredExecutor).unwrap();
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let mut remote = selection("pending-owner", &config);
    remote.config = EnvironmentConfigState::Pending;
    let init = ExtensionDataInit::new();
    let admitted = prepare(
        &config,
        ThreadId::new(),
        false,
        &init,
        startup(manager, vec![remote]),
    )
    .await
    .unwrap();
    assert_eq!(
        admitted.selections[0].config,
        EnvironmentConfigState::Pending
    );
}

async fn config(home: &TempDir) -> Config {
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    config.cwd = AbsolutePathBuf::try_from(home.path()).unwrap();
    config.project_doc_max_bytes = 1024;
    config
}

fn selection(id: &str, config: &Config) -> TurnEnvironmentSelection {
    TurnEnvironmentSelection {
        environment_id: id.to_string(),
        cwd: PathUri::from_abs_path(&config.cwd),
        workspace_roots: Vec::new(),
        selected_capability_roots: Default::default(),
        config: EnvironmentConfigState::FromThread,
    }
}

fn startup(
    manager: Arc<EnvironmentManager>,
    selections: Vec<TurnEnvironmentSelection>,
) -> StartupEnvironments {
    StartupEnvironments {
        manager,
        shell: crate::shell::default_user_shell(),
        defaults: ThreadEnvironmentDefaults::new(
            EnvironmentConfig {
                environment_hooks: Default::default(),
                allow_login_shell: true,
                workspace_roots: Vec::new(),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_type: SandboxType::None,
                use_legacy_landlock: false,
                permission_profile: PermissionProfileSnapshot::legacy(PermissionProfile::Disabled),
                shell_environment_policy: Default::default(),
                exec_policy: None,
                mcp_policy: None,
                network_policy: None,
                selected_capability_roots: Vec::new(),
            },
            SandboxType::None,
        ),
        selections,
        inherited: TurnEnvironmentSnapshot::default(),
        project_instructions_pinned: false,
    }
}

// A blocking task prevents automatic clock advances while the socket handshake is in flight.
fn freeze_clock() -> std::sync::mpsc::Sender<()> {
    tokio::time::pause();
    let (guard, dropped) = std::sync::mpsc::channel();
    tokio::task::spawn_blocking(move || {
        let _ = dropped.recv();
    });
    guard
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecutorBehavior {
    StallInitialization,
    StallInstructions,
    StallAfterParentInstructions,
    Ready,
}

/// Serves a disposable executor and exposes when its selected boundary is reached.
async fn executor(
    behavior: ExecutorBehavior,
) -> (String, oneshot::Receiver<()>, AbortOnDropHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (started, receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut started = Some(started);
        while let Some(message) = socket.next().await {
            let message = message.unwrap();
            let request: Value = match message {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                Message::Binary(data) => serde_json::from_slice(&data).unwrap(),
                _ => continue,
            };
            let method = request["method"].as_str().unwrap();
            if (behavior == ExecutorBehavior::StallInitialization && method == "initialize")
                || (behavior == ExecutorBehavior::StallInstructions && method == "fs/getMetadata")
                || (behavior == ExecutorBehavior::StallAfterParentInstructions
                    && method == "fs/readFile"
                    && request["params"]["path"] == "file:///project/child/AGENTS.md")
            {
                started.take().unwrap().send(()).unwrap();
                std::future::pending::<()>().await;
            }
            let response = match method {
                "initialize" => json!({"sessionId":"startup-test"}),
                "initialized" => continue,
                "environment/info" => {
                    json!({"shell":{"name":"bash","path":"/bin/bash"},"userHomeDir":"file:///home/test","platformOs":"linux","temporaryDirectories":["file:///tmp"],"capabilities":{}})
                }
                "fs/getMetadata" if behavior == ExecutorBehavior::StallAfterParentInstructions => {
                    let path = request["params"]["path"].as_str().unwrap();
                    let directory = path == "file:///project/.git";
                    let file = matches!(
                        path,
                        "file:///project/AGENTS.md" | "file:///project/child/AGENTS.md"
                    );
                    if !directory && !file {
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": request["id"],
                                    "error": {"code": -32004, "message": "not found"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                        continue;
                    }
                    json!({"isDirectory":directory,"isFile":file,"isSymlink":false,"size":18,"createdAtMs":0,"modifiedAtMs":0})
                }
                "fs/readFile" if behavior == ExecutorBehavior::StallAfterParentInstructions => {
                    assert_eq!(request["params"]["path"], "file:///project/AGENTS.md");
                    json!({"dataBase64":"cGFyZW50IGluc3RydWN0aW9u"})
                }
                other => panic!("unexpected request {other}"),
            };
            socket
                .send(Message::Text(
                    json!({"id":request["id"],"result":response})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        }
    });
    (url, receiver, AbortOnDropHandle::new(server))
}
