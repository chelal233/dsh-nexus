use std::{
    fs, io,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use axum::{extract::State, Json};
use nexus_core::{
    data_root_identity, AgentState, CheckpointRestoreJournalStore, CheckpointStore, ConfigStore,
    DiagnosticsStore, HarnessLaunchSpec, NexusConfigFile, NexusPaths, ProfileStore, ReleaseStore,
    UpdateSpec,
};
use nexus_protocol::{
    ConfigAction, ConfigCommand, RuntimeConfigPayload, RuntimeInstallMode, RuntimeSource,
    UpdateAction, UpdateCommand,
};
use tokio::{
    sync::{oneshot, watch, Mutex, RwLock},
    time::timeout,
};

use super::{
    config_control, snapshots, update_control, AppState, HarnessSupervisor, UpdateExecutor,
};

pub(crate) fn switch_test_state(label: &str) -> AppState {
    let root = std::env::temp_dir().join(format!(
        "nexus-switch-{label}-{}-{}",
        std::process::id(),
        nexus_core::unix_time_nanos_for_update()
    ));
    let paths = NexusPaths::from_root(root);
    paths.ensure_directories().expect("test directories create");
    let update = UpdateSpec {
        source: "https://example.invalid/repo".to_owned(),
        ref_name: "main".to_owned(),
        git_program: if cfg!(windows) {
            PathBuf::from("cmd.exe")
        } else {
            PathBuf::from("/bin/false")
        },
        build_program: None,
        build_args: Vec::new(),
        verify_program: None,
        verify_args: Vec::new(),
        timeout_secs: Some(5),
    };
    let config = ConfigStore::new(paths.clone());
    config
        .write(&NexusConfigFile {
            update_attempt_id: None,
            external_harness: None,
            schema_version: 1,
            harness_preferences: None,
            harness: None,
            update: Some(update),
            releases: None,
            runtime: None,
            snapshots: None,
        })
        .expect("update config writes");
    let releases = ReleaseStore::new(paths.clone());
    let profiles = ProfileStore::new(paths.clone());
    profiles.load().expect("default profile creates");
    let supervisor = HarnessSupervisor::new(paths.clone()).expect("supervisor creates");
    let mut runtime = AgentState::starting();
    runtime.mark_running();
    let (shutdown, _) = watch::channel(false);
    AppState {
        paths: paths.clone(),
        runtime: Arc::new(RwLock::new(runtime)),
        agent_revision: Arc::new(AtomicU64::new(0)),
        profiles,
        checkpoints: CheckpointStore::new(paths.clone()),
        checkpoint_restores: CheckpointRestoreJournalStore::new(paths.clone()),
        releases: releases.clone(),
        diagnostics: DiagnosticsStore::new(paths.clone()),
        config,
        updater: UpdateExecutor::new(paths.clone(), releases),
        cold: crate::cold::ColdCoordinator::new(paths.clone()),
        supervisor,
        snapshots: snapshots::SnapshotCoordinator::new(
            paths.clone(),
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "test DSH home unavailable",
            )),
        ),
        harness_sync: Arc::new(Mutex::new(())),
        maintenance_preview: Arc::new(std::sync::Mutex::new(
            crate::MaintenancePreviewScan::default(),
        )),
        checkpoint_transition_gate: Arc::new(Mutex::new(None)),
        agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown,
        data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
        instance_id: "switch-test-agent".to_owned(),
        crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())),
        timeout_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())),
        canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(
            nexus_launcher_core::HarnessLogObserver::default(),
        )),
    }
}

#[tokio::test]
async fn update_source_only_preserves_latest_private_commands_and_defaults() {
    let state = switch_test_state("source-only");
    let root = state.paths.root.clone();
    state
        .config
        .transaction(|document| {
            let update = document.update.as_mut().unwrap();
            update.ref_name = "custom-ref".into();
            update.build_program = Some("custom-build".into());
            update.build_args = vec!["--token".into(), "BUILD-SECRET".into()];
            update.verify_program = Some("custom-verify".into());
            update.verify_args = vec!["--password=VERIFY-SECRET".into()];
            update.timeout_secs = Some(987);
            Ok(())
        })
        .unwrap();
    let mut stale = state.config.load().unwrap().update.unwrap().to_payload();
    stale.source = "https://example.test/updated".into();
    stale.build_args = vec!["[REDACTED]".into()];
    state
        .config
        .transaction(|document| {
            document.update.as_mut().unwrap().ref_name = "newer-concurrent-ref".into();
            Ok(())
        })
        .unwrap();
    let response = config_control(
        State(state.clone()),
        Json(ConfigCommand {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: ConfigAction::SetUpdateSource,
            update: Some(stale),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("BUILD-SECRET"));
    assert!(!text.contains("VERIFY-SECRET"));
    let update = state.config.load().unwrap().update.unwrap();
    assert_eq!(update.source, "https://example.test/updated");
    assert_eq!(update.ref_name, "newer-concurrent-ref");
    assert_eq!(update.build_args, vec!["--token", "BUILD-SECRET"]);
    assert_eq!(update.verify_args, vec!["--password=VERIFY-SECRET"]);
    assert_eq!(update.build_program.unwrap().to_str(), Some("custom-build"));
    assert_eq!(
        update.verify_program.unwrap().to_str(),
        Some("custom-verify")
    );
    assert_eq!(update.timeout_secs, Some(987));
    state
        .config
        .transaction(|document| {
            document.update = None;
            Ok(())
        })
        .unwrap();
    let command: ConfigCommand = serde_json::from_value(serde_json::json!({"action":"set_update_source","expected_revision":state.config.snapshot().unwrap().revision,"update":{"source":"https://example.test/new"}})).unwrap();
    assert_eq!(
        config_control(State(state.clone()), Json(command))
            .await
            .status(),
        axum::http::StatusCode::OK
    );
    let update = state.config.load().unwrap().update.unwrap();
    assert_eq!(update.ref_name, "main");
    assert_eq!(update.git_program.to_str(), Some("git"));
    drop(state);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn cold_switch_waiting_and_cancel_do_not_hold_lifecycle_or_update_gate() {
    let state = switch_test_state("cancel-owner");
    let root = state.paths.root.clone();
    let operation = state
        .cold
        .begin(
            "v-test".to_owned(),
            RuntimeSource::Official,
            RuntimeInstallMode::Portable,
        )
        .await
        .expect("operation begins");
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    assert!(
        state
            .supervisor
            .selection_change_is_quiescent(&lifecycle)
            .await
    );
    drop(lifecycle);
    assert!(state.updater.try_acquire_gate().is_ok());
    let cancelled = state
        .cold
        .cancel(&operation.operation_id)
        .await
        .expect("operation cancels");
    let _ = fs::remove_dir_all(root);
    assert_eq!(
        cancelled.phase,
        nexus_protocol::ColdOperationPhase::Cancelling
    );
}

#[tokio::test]
async fn config_api_requires_revision_and_rejects_stale_drafts_and_maintenance() {
    let state = switch_test_state("config-cas-api");
    let root = state.paths.root.clone();
    let first = state.config.snapshot().unwrap();
    let status = super::config_status(State(state.clone())).await;
    assert_eq!(status.status(), axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(status.into_body(), 128 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json["revision"], first.revision);
    let absent = config_control(
        State(state.clone()),
        Json(ConfigCommand {
            action: ConfigAction::ClearUpdate,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(
        absent.status(),
        axum::http::StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(state.config.snapshot().unwrap().revision, first.revision);
    let saved = config_control(
        State(state.clone()),
        Json(ConfigCommand {
            expected_revision: Some(first.revision.clone()),
            action: ConfigAction::ClearUpdate,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(saved.status(), axum::http::StatusCode::OK);
    let saved_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(saved.into_body(), 128 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved_json["revision"],
        state.config.snapshot().unwrap().revision
    );
    assert_ne!(saved_json["revision"], first.revision);
    let before = fs::read(&state.paths.config_file).unwrap();
    let conflict = config_control(
        State(state.clone()),
        Json(ConfigCommand {
            expected_revision: Some(first.revision.clone()),
            action: ConfigAction::ClearUpdate,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(conflict.status(), axum::http::StatusCode::CONFLICT);
    let error: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(conflict.into_body(), 128 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(error["code"], "config_revision_conflict");
    let maintenance = super::maintenance_control(
        State(state.clone()),
        Json(super::MaintenanceRequest {
            expected_revision: Some(first.revision),
            action: "reset".into(),
            scope: Some("config".into()),
        }),
    )
    .await;
    assert_eq!(maintenance.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(fs::read(&state.paths.config_file).unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn set_harness_waits_for_lifecycle_while_update_config_stays_try_gate_only() {
    let state = switch_test_state("set-harness-lifecycle");
    let root = state.paths.root.clone();
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let update_payload = state
        .config
        .load()
        .expect("config loads")
        .update
        .expect("update config exists")
        .to_payload();
    let set_update = timeout(
        Duration::from_secs(3),
        config_control(
            State(state.clone()),
            Json(ConfigCommand {
                expected_revision: Some(state.config.snapshot().unwrap().revision),
                action: ConfigAction::SetUpdate,
                update: Some(update_payload),
                ..Default::default()
            }),
        ),
    )
    .await
    .expect("SetUpdate does not wait for lifecycle");
    let clear_update = timeout(
        Duration::from_secs(3),
        config_control(
            State(state.clone()),
            Json(ConfigCommand {
                expected_revision: Some(state.config.snapshot().unwrap().revision),
                action: ConfigAction::ClearUpdate,
                ..Default::default()
            }),
        ),
    )
    .await
    .expect("ClearUpdate does not wait for lifecycle");
    assert_eq!(set_update.status(), axum::http::StatusCode::OK);
    assert_eq!(clear_update.status(), axum::http::StatusCode::OK);
    let (set_harness_attempt, set_harness_attempt_rx) = oneshot::channel();
    state
        .supervisor
        .observe_next_lifecycle_wait(set_harness_attempt)
        .await;
    let set_harness_state = state.clone();
    let set_harness_payload =
        HarnessLaunchSpec::new(PathBuf::from("missing-switch-test-harness")).to_payload();
    let set_harness = tokio::spawn(async move {
        config_control(
            State(set_harness_state),
            Json(ConfigCommand {
                expected_revision: Some(state.config.snapshot().unwrap().revision),
                action: ConfigAction::SetHarness,
                harness: Some(set_harness_payload),
                ..Default::default()
            }),
        )
        .await
    });
    timeout(Duration::from_secs(3), set_harness_attempt_rx)
        .await
        .expect("SetHarness attempts lifecycle before deadline")
        .expect("SetHarness lifecycle wait signal arrives");
    assert!(
        !set_harness.is_finished(),
        "SetHarness must wait for lifecycle"
    );
    drop(lifecycle);
    let response = timeout(Duration::from_secs(3), set_harness)
        .await
        .expect("SetHarness resumes after lifecycle releases")
        .expect("SetHarness task joins");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn runtime_config_waits_for_lifecycle_then_uses_try_update_gate() {
    let state = switch_test_state("runtime-config-lock-order");
    let root = state.paths.root.clone();
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let (attempt, attempt_rx) = oneshot::channel();
    state.supervisor.observe_next_lifecycle_wait(attempt).await;
    let task_state = state.clone();
    let expected_revision = Some(state.config.snapshot().unwrap().revision);
    let set_runtime = tokio::spawn(async move {
        config_control(
            State(task_state),
            Json(ConfigCommand {
                expected_revision,
                action: ConfigAction::SetRuntime,
                runtime: Some(RuntimeConfigPayload::default()),
                ..Default::default()
            }),
        )
        .await
    });
    timeout(Duration::from_secs(3), attempt_rx)
        .await
        .expect("SetRuntime attempts lifecycle before deadline")
        .expect("SetRuntime lifecycle wait signal arrives");
    assert!(!set_runtime.is_finished());
    drop(lifecycle);
    let response = timeout(Duration::from_secs(3), set_runtime)
        .await
        .expect("SetRuntime resumes")
        .expect("SetRuntime task joins");
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let update_gate = state
        .updater
        .try_acquire_gate()
        .expect("test owns update gate");
    let response = config_control(
        State(state.clone()),
        Json(ConfigCommand {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: ConfigAction::ClearRuntime,
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    drop(update_gate);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn fetching_existing_tag_does_not_publish_agent_selection() {
    let state = switch_test_state("agent-persistence");
    let root = state.paths.root.clone();
    state
        .releases
        .register("fast-slot", "v-fast", None, None)
        .expect("fast slot registers");
    state
        .agent_persist_failure
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let response = update_control(
        State(state.clone()),
        Json(UpdateCommand {
            action: UpdateAction::Switch,
            release_id: None,
            version: None,
            tag: Some("v-fast".to_owned()),
            ..UpdateCommand::default()
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let terminal = timeout(Duration::from_secs(3), async {
        loop {
            let operation = state
                .cold
                .load()
                .expect("operation loads")
                .expect("operation exists");
            if operation.phase.is_terminal() {
                break operation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background fast switch terminates");
    assert_eq!(terminal.phase, nexus_protocol::ColdOperationPhase::Prepared);
    assert!(terminal.error.is_none());
    assert_eq!(
        state
            .releases
            .load()
            .expect("release selection loads")
            .current_release
            .as_deref(),
        None,
        "fetching an existing version never changes the current pointer"
    );
    assert!(state.updater.try_acquire_gate().is_ok());
    let _ = fs::remove_dir_all(root);
}
