use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use axum::{extract::State, http::StatusCode, Json};
use nexus_core::{
    data_root_identity, AgentState, CheckpointRestoreIntent, CheckpointRestoreJournalStore,
    CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec, NexusConfigFile, NexusPaths,
    NexusStateSnapshot, ProfileCatalog, ProfileStore, ReleaseStore, RuntimeConfig, RuntimePin,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointCreateResponse, CheckpointRestoreResponse, HarnessState,
    ReleaseAction, ReleaseCommand, RuntimeInstallMode, RuntimeOwnership, RuntimeSource,
};
use tokio::{
    sync::{oneshot, watch, Mutex, RwLock},
    time::{sleep, timeout},
};

use super::{
    checkpoint_create, checkpoint_restore, checkpoint_restore_abort, checkpoint_restore_retry,
    execute_harness_action, recover_checkpoint_restore_startup, release_control, snapshots,
    sync_harness_state, update_agent_state, AppState, CheckpointTransitionGate, HarnessSupervisor,
    UpdateExecutor, DEFAULT_MAX_RELEASE_SLOTS,
};

fn write_profile_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("synthetic profile parent creates");
    }
    fs::write(path, content).expect("synthetic profile file writes");
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[tokio::test]
async fn harness_preferences_switch_only_the_pointer_and_clear_to_original_home() {
    use nexus_protocol::{ConfigAction, ConfigCommand, HarnessPreferencesPayload};
    let (state, root) = content_test_state("preferences");
    let original = state.snapshots.configured_dsh_home().unwrap();
    let original_bytes = fs::read(original.join("settings.yaml")).unwrap();
    let selected = root.join("new-harness-data");
    let command = ConfigCommand {
        expected_revision: Some(state.config.snapshot().unwrap().revision),
        action: ConfigAction::SetHarnessPreferences,
        harness_preferences: Some(HarnessPreferencesPayload {
            home: Some(selected.to_string_lossy().into_owned()),
            port: Some(0),
            open_browser: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    let lease = state.snapshots.acquire("demo".into()).await.unwrap();
    let blocked = super::config_control(State(state.clone()), Json(command.clone())).await;
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    drop(lease);
    let saved = super::config_control(State(state.clone()), Json(command)).await;
    assert!(saved.status().is_success());
    assert_eq!(state.snapshots.configured_dsh_home().unwrap(), selected);
    assert_eq!(
        crate::dsh::resolve_dsh_home_for_paths(&state.paths).unwrap(),
        selected
    );
    assert!(
        !selected.exists(),
        "saving must not create, copy or move Harness data"
    );
    assert_eq!(
        fs::read(original.join("settings.yaml")).unwrap(),
        original_bytes
    );
    let cleared = super::config_control(
        State(state.clone()),
        Json(ConfigCommand {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: ConfigAction::SetHarnessPreferences,
            harness_preferences: Some(HarnessPreferencesPayload {
                home: Some("  ".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
    )
    .await;
    assert!(cleared.status().is_success());
    assert!(state.config.load().unwrap().harness_preferences.is_none());
    assert_eq!(state.snapshots.configured_dsh_home().unwrap(), original);
    assert!(!selected.exists());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn maintenance_blocks_busy_owners_and_resets_idle_configuration() {
    let (state, root) = content_test_state("maintenance-owners");
    let before = fs::read(&state.paths.config_file).unwrap();
    let request = || {
        Json(super::MaintenanceRequest {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: "reset".into(),
            scope: Some("config".into()),
        })
    };
    let update = state.updater.try_acquire_gate().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(update);
    let snapshot = state.snapshots.try_acquire_configuration().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(snapshot);
    let cold = state.cold.try_acquire_maintenance().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(cold);
    assert_eq!(fs::read(&state.paths.config_file).unwrap(), before);
    let response = super::maintenance_control(State(state.clone()), request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let backup = std::path::PathBuf::from(value["backup_dir"].as_str().unwrap());
    assert_eq!(fs::read(backup.join("config.json")).unwrap(), before);
    assert_eq!(
        state.config.load().unwrap(),
        nexus_core::NexusConfigFile::default()
    );
    assert!(root.join("dsh-home").exists());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn maintenance_restores_previous_only_when_owners_are_idle_without_starting() {
    let (state, root) = content_test_state("maintenance-undo");
    let previous = state.config.load().unwrap();
    let mut current = previous.clone();
    current
        .harness_preferences
        .get_or_insert_with(Default::default)
        .telemetry_disabled = Some(true);
    state.config.write(&current).unwrap();
    let request = || {
        Json(super::MaintenanceRequest {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: "restore_previous".into(),
            scope: Some("config".into()),
        })
    };
    let update = state.updater.try_acquire_gate().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(update);
    let snapshot = state.snapshots.try_acquire_configuration().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(snapshot);
    let cold = state.cold.try_acquire_maintenance().unwrap();
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(cold);
    assert_eq!(state.config.load().unwrap(), current);
    assert_eq!(
        super::maintenance_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(state.config.load().unwrap(), previous);
    assert!(state.supervisor.status().await.pid.is_none());
    let invalid_request = request();
    fs::write(&state.paths.config_file, b"{").unwrap();
    assert_ne!(
        super::maintenance_control(State(state.clone()), invalid_request)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(fs::read(&state.paths.config_file).unwrap(), b"{");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn release_cleanup_owner_survives_http_cancellation() {
    let (state, root) = content_test_state("release-cleanup-owner");
    let id = format!("owner-{}", nexus_core::unix_time_nanos_for_update());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *super::RELEASE_REMOVE_TEST_GATE.lock().unwrap() = Some((id.clone(), entered_tx, release_rx));
    let owned = state.clone();
    let task = tokio::spawn(async move {
        super::release_control(
            State(owned),
            Json(nexus_protocol::ReleaseCommand {
                action: nexus_protocol::ReleaseAction::Remove,
                id: Some(id),
                version: None,
                source: None,
                note: None,
                ..nexus_protocol::ReleaseCommand::default()
            }),
        )
        .await
    });
    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(state.updater.try_acquire_gate().is_err());
    assert!(state.supervisor.try_acquire_lifecycle().is_none());
    release_tx.send(()).unwrap();
    let guard = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.supervisor.acquire_lifecycle(),
    )
    .await
    .unwrap();
    drop(guard);
    assert!(state.updater.try_acquire_gate().is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn standalone_configuration_preserve_api_keeps_external_current() {
    let (state, root) = content_test_state("configuration-preserve-api");
    let current = fs::read(&state.paths.config_file).unwrap();
    nexus_core::write_private_json_atomic(&state.paths.root,&state.paths.root.join("config-write.pending.json"),
        &serde_json::json!({"schema":1,"committed":false,"rotate":false,"old_current":null,"old_previous":null,"target":[123,125]})).unwrap();
    let status = state
        .config
        .pending_configuration_status()
        .unwrap()
        .unwrap();
    let id = status["operation_id"].as_str().unwrap().to_owned();
    let response = super::update_control(
        State(state.clone()),
        Json(nexus_protocol::UpdateCommand {
            action: nexus_protocol::UpdateAction::ConfigurationAbandon,
            operation_id: Some(id),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fs::read(&state.paths.config_file).unwrap(), current);
    assert!(state
        .config
        .pending_configuration_status()
        .unwrap()
        .is_none());
    assert!(super::ensure_checkpoint_mutation_ready(&state)
        .await
        .is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn maintenance_cleanup_uses_existing_owner_gates() {
    let (state, root) = content_test_state("cleanup-owner-gates");
    let request = || {
        Json(serde_json::json!({"action":"cleanup", "preview_id":"test", "item_ids":["item-0"]}))
    };
    let update = state.updater.try_acquire_gate().unwrap();
    assert_eq!(
        super::maintenance_dispatch(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(update);
    let snapshot = state.snapshots.try_acquire_configuration().unwrap();
    assert_eq!(
        super::maintenance_dispatch(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(snapshot);
    let cold = state.cold.try_acquire_maintenance().unwrap();
    assert_eq!(
        super::maintenance_dispatch(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(cold);
    assert_eq!(
        super::maintenance_status(State(state)).await.status(),
        StatusCode::OK
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn harness_preferences_reach_child_without_changing_launch_directory() {
    let (state, root) = content_test_state("preferences-child");
    let selected = root.join("selected-home");
    state
        .releases
        .register("verified-preferences", "0.1.2-rc.1", None, None)
        .unwrap();
    state.releases.promote("verified-preferences").unwrap();
    let slot = state.releases.release_root("verified-preferences").unwrap();
    write_profile_file(
        &slot.join("package.json"),
        r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#,
    );
    let entry = slot.join("apps/cli/lib/bin.js");
    let capture = root.join("child-observed.json");
    write_profile_file(&entry, &format!(
        "require('node:fs').writeFileSync({}, JSON.stringify({{home:process.env.DSH_HOME, telemetry:process.env.DSH_TELEMETRY_DISABLED, args:process.argv.slice(2), cwd:process.cwd()}})); setInterval(()=>{{}},1000);",
        serde_json::to_string(&capture).unwrap()));
    let mut launch = HarnessLaunchSpec::new(
        executable_on_path(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("Node fixture runtime"),
    );
    launch.mode = nexus_protocol::HarnessLaunchMode::Node;
    launch.args = vec![
        entry.to_string_lossy().into_owned(),
        "--profile".into(),
        "{profile}".into(),
    ];
    launch.working_dir = Some(root.clone());
    state
        .config
        .write(&NexusConfigFile {
            external_harness: None,
            harness: Some(launch.clone()),
            harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(selected.to_string_lossy().into_owned()),
                port: Some(0),
                open_browser: Some(false),
                telemetry_disabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    state.supervisor.start_with_profile("web").await.unwrap();
    let observed = timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(bytes) = fs::read(&capture) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    break value;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    state.supervisor.stop().await.unwrap();
    let observed = observed.unwrap();
    assert_eq!(observed["home"], selected.to_string_lossy().as_ref());
    assert_eq!(observed["telemetry"], "");
    assert_eq!(observed["cwd"], root.to_string_lossy().as_ref());
    assert_eq!(
        observed["args"],
        serde_json::json!(["--profile", "web", "--port", "0", "--no-open"])
    );
    assert_eq!(state.config.load().unwrap().harness, Some(launch));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn preflight_collects_missing_slot_entry_and_home_without_starting() {
    let (state, root) = content_test_state("preflight-multiple-blockers");
    let inaccessible = root.join("not-a-directory");
    fs::create_dir(&inaccessible).unwrap();
    let mut spec = HarnessLaunchSpec::new(root.join("node.exe"));
    spec.mode = nexus_protocol::HarnessLaunchMode::Node;
    spec.args = vec!["{release_root}/apps/cli/lib/bin.js".into()];
    state
        .config
        .write(&NexusConfigFile {
            external_harness: None,
            harness: Some(spec),
            harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(inaccessible.to_string_lossy().into_owned()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    fs::remove_dir(&inaccessible).unwrap();
    fs::write(&inaccessible, "preserve").unwrap();
    let (checks, _) = crate::preflight::collect(&state, false);
    for id in ["home", "release", "entry"] {
        assert!(
            checks
                .iter()
                .any(|check| check["id"] == id && check["status"] == "blocked"),
            "{id}: {checks:?}"
        );
    }
    assert_eq!(fs::read_to_string(inaccessible).unwrap(), "preserve");
    assert!(state.releases.load().unwrap().current_release.is_none());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn preflight_custom_command_does_not_require_managed_runtime_or_slot() {
    let (state, root) = content_test_state("preflight-custom");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut spec = HarnessLaunchSpec::new(std::env::current_exe().unwrap());
    spec.readiness_url = Some(format!("tcp://{}", listener.local_addr().unwrap()));
    state
        .config
        .write(&NexusConfigFile {
            external_harness: None,
            harness: Some(spec),
            ..Default::default()
        })
        .unwrap();
    let (checks, runtime) = crate::preflight::collect(&state, false);
    assert!(runtime.is_none());
    assert!(
        !checks.iter().any(|check| check["status"] == "blocked"),
        "{checks:?}"
    );
    state
        .config
        .transaction(|config| {
            config.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
                telemetry_disabled: Some(false),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let (checks, _) = crate::preflight::collect(&state, false);
    assert!(checks
        .iter()
        .any(|check| check["id"] == "preferences" && check["status"] == "blocked"));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn recovery_mode_blocks_start_restart_and_leaves_stopped() {
    let (state, root) = content_test_state("recovery-mode");
    fs::write(state.paths.run_dir.join("harness-recovery.json"), b"{").unwrap();
    let response = super::recovery_status(State(state.clone())).await;
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(report["pause_error"].is_string());
    assert_eq!(report["paused"], true);
    let response = super::recovery_control(
        State(state.clone()),
        Json(super::RecoveryCommand {
            action: super::RecoveryAction::Enter,
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(super::recovery_mode::paused(&state.paths).unwrap());
    let response = axum::response::IntoResponse::into_response(
        super::preflight::check(State(state.clone())).await,
    );
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let check: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(check["paused"], true);
    assert!(check["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == "recovery_mode" && item["status"] == "warning"));
    assert!(matches!(
        state.supervisor.start().await,
        Err(super::HarnessSupervisorError::RecoveryPaused)
    ));
    assert!(matches!(
        state.supervisor.restart().await,
        Err(super::HarnessSupervisorError::RecoveryPaused)
    ));
    let response = super::recovery_control(
        State(state.clone()),
        Json(super::RecoveryCommand {
            action: super::RecoveryAction::Leave,
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!super::recovery_mode::paused(&state.paths).unwrap());
    assert!(
        state
            .supervisor
            .selection_change_is_quiescent(&state.supervisor.acquire_lifecycle().await)
            .await
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn recovery_mode_selects_valid_profile_without_launch_probe_and_creates_blank_profile() {
    let (state, root) = content_test_state("recovery-select");
    let home = state.snapshots.configured_dsh_home().unwrap().clone();
    write_profile_file(
        &home.join("profiles/target/package.json"),
        r#"{"name":"target","dsh":{"profile":{"bundles":["plugin-one"]}}}"#,
    );
    let mut config = state.config.load().unwrap();
    config.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
        home: Some(home.to_string_lossy().into_owned()),
        ..Default::default()
    });
    let mut harness = HarnessLaunchSpec::new(root.join("missing-runtime").join("node.exe"));
    harness.mode = nexus_protocol::HarnessLaunchMode::Node;
    harness.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
    config.harness = Some(harness);
    state.config.write(&config).unwrap();
    state
        .releases
        .register("recovery-version", "test", None, None)
        .unwrap();
    state.releases.promote("recovery-version").unwrap();
    super::recovery_mode::set_paused(&state.paths, true).unwrap();
    for (action, name) in [
        (nexus_protocol::ProfileAction::Select, "target"),
        (nexus_protocol::ProfileAction::Create, "new-empty"),
    ] {
        let response = super::profile_control(
            State(state.clone()),
            Json(nexus_protocol::ProfileCommand {
                action,
                profile: Some(name.to_owned()),
                package: None,
                target: None,
            }),
        )
        .await;
        assert!(response.status().is_success(), "{}", response.status());
    }
    assert_eq!(state.profiles.load().unwrap().active_profile, "target");
    assert!(!state.paths.root.join("compatibility/latest.json").exists());
    for (profile, target) in [(None, None), (Some("other"), None), (None, Some("invalid"))] {
        let response = super::profile_control(
            State(state.clone()),
            Json(nexus_protocol::ProfileCommand {
                action: nexus_protocol::ProfileAction::CompatibilityCheck,
                profile: profile.map(str::to_owned),
                package: None,
                target: target.map(str::to_owned),
            }),
        )
        .await;
        assert!(
            !response.status().is_success(),
            "missing runtime/entry and invalid targets must not pass verification"
        );
        assert!(super::recovery_mode::paused(&state.paths).unwrap());
        assert_eq!(state.profiles.load().unwrap().active_profile, "target");
    }
    // Creation must still honor the same mutation exclusion as configuration edits.
    let _snapshot = state.snapshots.try_acquire_configuration().unwrap();
    let check = super::profile_control(
        State(state.clone()),
        Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::CompatibilityCheck,
            profile: None,
            package: None,
            target: None,
        }),
    )
    .await;
    assert!(!check.status().is_success());
    let response = super::profile_control(
        State(state.clone()),
        Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::Create,
            profile: Some("blocked".to_owned()),
            package: None,
            target: None,
        }),
    )
    .await;
    assert!(!response.status().is_success());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn corrupt_install_journal_keeps_control_plane_and_diagnostics_available() {
    let (state, root) = content_test_state("corrupt-install-journal");
    let journal = state.paths.root.join("install-operation.json");
    fs::write(&journal, "{interrupted-invalid-record").unwrap();
    state
        .updater
        .state_store()
        .write(&nexus_protocol::UpdateRuntimeInfo::running(
            "interrupted".into(),
            1,
        ))
        .unwrap();
    let recovered = state.updater.recover_unattached().unwrap();
    assert_eq!(recovered.state, nexus_protocol::UpdateState::Failed);
    let _ = super::health(State(state.clone())).await;
    assert_eq!(
        super::current_state(State(state.clone())).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        super::harness_status(State(state.clone())).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        super::recovery_status(State(state.clone())).await.status(),
        StatusCode::OK
    );
    let bundle = super::collect_current_diagnostics(&state, None).unwrap();
    assert!(bundle
        .files
        .iter()
        .any(|file| file.name == "install-operation.json"));
    let updates = super::update_status(State(state.clone())).await;
    assert!(!updates.status().is_success());
    let body = axum::body::to_bytes(updates.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("install_operation_unavailable"));
    assert!(state
        .updater
        .install(Some("blocked".into()), Some("test".into()))
        .await
        .is_err());
    let reset = super::maintenance_control(
        State(state.clone()),
        Json(super::MaintenanceRequest {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: "reset".into(),
            scope: None,
        }),
    )
    .await;
    assert!(!reset.status().is_success());
    assert_eq!(
        fs::read_to_string(&journal).unwrap(),
        "{interrupted-invalid-record"
    );
    fs::remove_file(journal).unwrap();
    assert!(state.updater.try_acquire_gate().is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn checkpoint_capture_progress_get_does_not_wait_for_capture_owner() {
    let (state, root) = content_test_state("capture-progress");
    let (lease, id) = state
        .snapshots
        .acquire_capture("demo".into(), "manual")
        .await
        .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        super::checkpoint_list(State(state.clone())),
    )
    .await
    .expect("GET does not wait for capture owner");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["inventory_refresh_pending"], true);
    assert_eq!(value["last_capture"]["state"], "running");
    assert!(value["checkpoints"].is_array());
    state.snapshots.finish_capture(&id, &Ok::<_, io::Error>(()));
    drop(lease);
    drop(state);
    let _ = fs::remove_dir_all(root);
}

fn content_test_state(label: &str) -> (AppState, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "nexus-agent-content-{label}-{}-{}",
        std::process::id(),
        nexus_core::unix_time_nanos_for_update()
    ));
    let paths = NexusPaths::from_root(root.join("nexus-data"));
    paths
        .ensure_directories()
        .expect("Nexus directories create");
    let dsh_home = root.join("dsh-home");
    let profile = dsh_home.join("profiles/demo");
    write_profile_file(
        &profile.join("package.json"),
        r#"{"name":"demo","version":"1.0.0","dependencies":{}}"#,
    );
    write_profile_file(&profile.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n");
    write_profile_file(&profile.join("pnpm-workspace.yaml"), "packages: []\n");
    write_profile_file(&profile.join("cordis.patch.yml"), "[]\n");
    write_profile_file(
        &profile.join(".dsh-market/state.json"),
        r#"{"installed":[]}"#,
    );
    write_profile_file(
        &dsh_home.join("settings.yaml"),
        "provider:\n  apiKey: DUMMY-OLD-SECRET\n  mode: old\n",
    );
    write_profile_file(&dsh_home.join("cordis.patch.yml"), "[]\n");
    let profiles = ProfileStore::new(paths.clone());
    profiles
        .write(&ProfileCatalog::new("demo", vec!["demo".to_owned()]).expect("profile validates"))
        .expect("profile catalog writes");
    let config = ConfigStore::new(paths.clone());
    config
        .write(&NexusConfigFile::default())
        .expect("empty config writes");
    let releases = ReleaseStore::new(paths.clone()).with_max_slots(DEFAULT_MAX_RELEASE_SLOTS);
    let supervisor =
        HarnessSupervisor::with_graceful_wait(paths.clone(), Duration::from_millis(100))
            .expect("supervisor creates");
    let mut runtime = AgentState::starting();
    runtime.mark_running();
    runtime.set_profile("demo".to_owned());
    runtime.set_harness(HarnessState::Stopped);
    let (shutdown, _) = watch::channel(false);
    (
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
            snapshots: snapshots::SnapshotCoordinator::new(paths.clone(), Ok(dsh_home.clone())),
            harness_sync: Arc::new(Mutex::new(())),
            maintenance_preview: Arc::new(std::sync::Mutex::new(
                crate::MaintenancePreviewScan::default(),
            )),
            checkpoint_transition_gate: Arc::new(Mutex::new(None)),
            agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown,
            data_root_id: data_root_identity(&paths).expect("data root identity reads"),
            instance_id: format!("content-{label}"),
            crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())),
            canary: Arc::new(Mutex::new(None)),
            harness_logs: Arc::new(Mutex::new(
                nexus_launcher_core::HarnessLogObserver::default(),
            )),
        },
        root,
    )
}

#[tokio::test]
#[ignore = "requires NEXUS_TEST_NODE_BINARY for the real startup choice flow"]
async fn compatibility_choice_allows_failed_release_to_be_retried() {
    use crate::{compatibility, profile_control};
    use nexus_protocol::{ProfileAction, ProfileCommand};
    let node =
        PathBuf::from(std::env::var_os("NEXUS_TEST_NODE_BINARY").expect("explicit Node runtime"));
    let (state, root) = content_test_state("compatibility-choice");
    let home = state.snapshots.configured_dsh_home().unwrap();
    let manifest = home.join("profiles/demo/package.json");
    let original = br#"{"name":"demo","dsh":{"profile":{"bundles":["unclassified"]}}}"#;
    fs::write(&manifest, original).unwrap();
    state
        .releases
        .register("choice-old", "old", None, None)
        .unwrap();
    state
        .releases
        .register("choice-target", "target", None, None)
        .unwrap();
    state.releases.promote("choice-old").unwrap();
    let target = state.releases.release_root("choice-target").unwrap();
    write_profile_file(
        &target.join("vendor/core/package.json"),
        r#"{"name":"@deepseek-ai/test-core"}"#,
    );
    write_profile_file(
        &target.join("apps/cli/lib/bin.js"),
        r#"
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const manifest = JSON.parse(fs.readFileSync(path.join(process.env.DSH_HOME, 'profiles', process.argv[3], 'package.json')));
if (manifest.dsh.profile.bundles.includes('unclassified')) {
  console.error('failed to apply loader entry fixture (unclassified): unsupported setup'); process.exit(1);
}
const server = http.createServer((req, res) => res.end('<html>ready</html>'));
server.listen(0, '127.0.0.1', () => console.log('dsh web: http://127.0.0.1:' + server.address().port + '/'));
"#,
    );
    let mut config = state.config.load().unwrap();
    let mut spec = HarnessLaunchSpec::new(node);
    spec.mode = nexus_protocol::HarnessLaunchMode::Node;
    spec.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
    config.harness = Some(spec);
    state.config.write(&config).unwrap();
    let promote = ReleaseCommand {
        action: ReleaseAction::Promote,
        id: Some("choice-target".to_owned()),
        rollback_confirmation: state
            .releases
            .promotion_risk_confirmation("choice-target")
            .unwrap(),
        ..ReleaseCommand::default()
    };
    let failed = release_control(State(state.clone()), Json(promote.clone())).await;
    assert!(!failed.status().is_success());
    assert_eq!(
        state.releases.load().unwrap().current_release.as_deref(),
        Some("choice-old")
    );
    let report = compatibility::latest(&state.paths).unwrap();
    assert_eq!(report.status, "needs_choice");
    assert_eq!(report.candidates[0].package, "unclassified");
    let saved = profile_control(
        State(state.clone()),
        Json(ProfileCommand {
            action: ProfileAction::PluginDisable,
            profile: Some("demo".to_owned()),
            package: Some("unclassified".to_owned()),
            target: None,
        }),
    )
    .await;
    assert!(saved.status().is_success());
    assert_eq!(
        compatibility::disabled_plugins(&home, "demo").unwrap(),
        vec!["unclassified"]
    );
    let retried = release_control(State(state.clone()), Json(promote)).await;
    if !retried.status().is_success() {
        let body = axum::body::to_bytes(retried.into_body(), 64 * 1024)
            .await
            .unwrap();
        panic!("retry failed: {}", String::from_utf8_lossy(&body));
    }
    assert_eq!(
        state.releases.load().unwrap().current_release.as_deref(),
        Some("choice-target")
    );
    let report = compatibility::latest(&state.paths).unwrap();
    assert_eq!(report.status, "isolated");
    assert_eq!(report.disabled[0].reason, "Disabled by user");
    assert_eq!(fs::read(&manifest).unwrap(), original);
    let restored = profile_control(
        State(state.clone()),
        Json(ProfileCommand {
            action: ProfileAction::PluginEnable,
            profile: Some("demo".to_owned()),
            package: Some("unclassified".to_owned()),
            target: None,
        }),
    )
    .await;
    assert!(restored.status().is_success());
    assert!(compatibility::disabled_plugins(&home, "demo")
        .unwrap()
        .is_empty());
    assert_eq!(fs::read(&manifest).unwrap(), original);
    // Manual verification in recovery mode executes only the disposable
    // probe, including failure and a subsequent saved isolation choice.
    let mut config = state.config.load().unwrap();
    config.harness.as_mut().unwrap().args = vec![
        "{release_root}/apps/cli/lib/bin.js".into(),
        "--profile".into(),
        "{profile}".into(),
    ];
    state.config.write(&config).unwrap();
    super::recovery_mode::set_paused(&state.paths, true).unwrap();
    let before_profiles = state.profiles.load().unwrap();
    let before_releases = state.releases.load().unwrap();
    let check = ProfileCommand {
        action: ProfileAction::CompatibilityCheck,
        profile: None,
        package: None,
        target: None,
    };
    assert!(!profile_control(State(state.clone()), Json(check.clone()))
        .await
        .status()
        .is_success());
    compatibility::set_plugin_disabled(&home, "demo", "unclassified", true).unwrap();
    let response = profile_control(State(state.clone()), Json(check)).await;
    if !response.status().is_success() {
        panic!(
            "manual verification failed: {}",
            String::from_utf8_lossy(
                &axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap()
            )
        );
    }
    assert!(super::recovery_mode::paused(&state.paths).unwrap());
    assert_eq!(state.profiles.load().unwrap(), before_profiles);
    assert_eq!(state.releases.load().unwrap(), before_releases);
    assert!(
        state
            .supervisor
            .selection_change_is_quiescent(&state.supervisor.acquire_lifecycle().await)
            .await
    );
    let report = compatibility::latest(&state.paths).unwrap();
    assert_eq!(report.trigger.as_deref(), Some("manual_check"));
    assert_eq!(
        report.checked_disabled_plugins,
        Some(vec!["unclassified".to_owned()])
    );
    assert!(!state
        .paths
        .root
        .join("compatibility/owner-pending.json")
        .exists());
    assert_eq!(fs::read(&manifest).unwrap(), original);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn read_routes_fail_fast_without_settling_a_locked_checkpoint() {
    let (state, root) = content_test_state("read-lifecycle-busy");
    state
        .releases
        .register("read-old", "old", None, None)
        .unwrap();
    state
        .releases
        .register("read-target", "target", None, None)
        .unwrap();
    state.releases.promote("read-old").unwrap();
    let entry = state.paths.releases_dir.join("read-old/health-entry.js");
    fs::write(&entry, "fixture").unwrap();
    let mut evidence = state
        .releases
        .healthy_launch_candidate("read-old", &entry, "web", "fixture-config".into())
        .unwrap();
    evidence.run_id = "read-old-run".into();
    evidence.generation = 1;
    state.releases.record_healthy_release(evidence).unwrap();
    let before_releases = state.releases.load().unwrap();
    let before_profiles = state.profiles.load().unwrap();
    let target_profiles = ProfileCatalog::new("partial", vec!["partial".to_owned()]).unwrap();
    let intent = CheckpointRestoreIntent {
        checkpoint_id: "read-busy-checkpoint".to_owned(),
        previous_profiles: before_profiles.clone(),
        previous_current_release: before_releases.current_release.clone(),
        previous_last_known_good: before_releases.last_known_good.clone(),
        target_profiles: target_profiles.clone(),
        target_current_release: Some("read-target".to_owned()),
        target_last_known_good: Some("read-old".to_owned()),
        snapshot: None,
    };
    let owner = state.supervisor.acquire_lifecycle().await;
    state.checkpoint_restores.begin(intent).unwrap();
    state
        .releases
        .restore_release_pointers(Some("read-target"), Some("read-old"))
        .unwrap();
    state.profiles.write(&target_profiles).unwrap();
    let partial_releases = state.releases.load().unwrap();
    let pending = serde_json::to_value(state.checkpoint_restores.load().unwrap()).unwrap();
    let runtime = serde_json::to_value(state.runtime.read().await.as_payload()).unwrap();
    async fn read_route(state: AppState, route: usize) -> axum::response::Response {
        match route {
            0 => super::current_state(State(state)).await,
            1 => super::harness_status(State(state)).await,
            2 => super::recovery_status(State(state)).await,
            3 => super::harness_ui(State(state)).await,
            4 => super::profile_list(State(state)).await,
            5 => super::release_list(State(state)).await,
            _ => super::checkpoint_list(State(state)).await,
        }
    }
    for route in 0..7 {
        let response = timeout(Duration::from_millis(250), read_route(state.clone(), route))
            .await
            .expect("read route must not queue behind lifecycle owner");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "lifecycle_busy");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .starts_with("NEXUS_LIFECYCLE_BUSY:"));
    }
    assert_eq!(state.releases.load().unwrap(), partial_releases);
    assert_eq!(state.profiles.load().unwrap(), target_profiles);
    assert_eq!(
        serde_json::to_value(state.checkpoint_restores.load().unwrap()).unwrap(),
        pending
    );
    assert_eq!(
        serde_json::to_value(state.runtime.read().await.as_payload()).unwrap(),
        runtime
    );
    drop(owner);
    for route in 0..7 {
        let response = timeout(Duration::from_secs(2), read_route(state.clone(), route))
            .await
            .expect("read route resumes after lifecycle owner releases");
        if route == 0 {
            assert_eq!(response.status(), StatusCode::OK);
        }
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("NEXUS_LIFECYCLE_BUSY:"));
    }
    // The first unlocked read still performs the existing Prepared recovery.
    assert_eq!(state.releases.load().unwrap(), before_releases);
    assert_eq!(state.profiles.load().unwrap(), before_profiles);
    assert!(state.checkpoint_restores.load().unwrap().is_none());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reorder_accepts_a_non_active_ordinary_profile_without_selecting_it() {
    let (state, root) = content_test_state("reorder-inactive");
    let home = state.snapshots.configured_dsh_home().unwrap();
    let path = home.join("profiles/other/package.json");
    write_profile_file(
        &path,
        r#"{"name":"other","dsh":{"profile":{"bundles":["a","b"]}},"dependencies":{"a":"1","b":"1"}}"#,
    );
    let before = state.profiles.load().unwrap();
    let response = super::profile_control(
        State(state.clone()),
        Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::PluginMove,
            profile: Some("other".to_owned()),
            package: Some("b".to_owned()),
            target: Some("a".to_owned()),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        super::dsh::native_profile(&home, "other").unwrap().bundles,
        ["b", "a"]
    );
    assert_eq!(state.profiles.load().unwrap(), before);
    let id = super::dsh::order_undo_id(&state.paths, &home, "other")
        .unwrap()
        .unwrap();
    let request = || {
        Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::PluginUndoMove,
            profile: Some("other".into()),
            package: None,
            target: Some(id.clone()),
        })
    };
    let snapshot_owner = state.snapshots.try_acquire_configuration().unwrap();
    assert_eq!(
        super::profile_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(snapshot_owner);
    let update_owner = state.updater.try_acquire_gate().unwrap();
    assert_eq!(
        super::profile_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drop(update_owner);
    assert_eq!(
        super::profile_control(State(state.clone()), request())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        super::dsh::native_profile(&home, "other").unwrap().bundles,
        ["a", "b"]
    );
    assert_eq!(state.profiles.load().unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn profile_selection_preflight_failure_does_not_publish_target() {
    let (state, root) = content_test_state("profile-preflight-failure");
    let home = state.snapshots.configured_dsh_home().unwrap().clone();
    write_profile_file(
        &home.join("profiles/target/package.json"),
        r#"{"name":"target","dsh":{"profile":{"bundles":["plugin-one","plugin-two"]}}}"#,
    );
    state
        .releases
        .register("profile-runtime", "test", None, None)
        .unwrap();
    state.releases.promote("profile-runtime").unwrap();
    let before = state.profiles.load().unwrap();
    let runtime_before = state.runtime.read().await.profile.clone();
    let mut config = state.config.load().unwrap();
    let mut harness = HarnessLaunchSpec::new(root.join("unused-runtime/node.exe"));
    harness.mode = nexus_protocol::HarnessLaunchMode::Node;
    harness.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
    config.harness = Some(harness);
    state.config.write(&config).unwrap();
    let response = super::profile_control(
        State(state.clone()),
        Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::Select,
            profile: Some("target".to_owned()),
            package: None,
            target: None,
        }),
    )
    .await;
    assert!(!response.status().is_success());
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("profile_compatibility_failed"));
    assert_eq!(state.profiles.load().unwrap(), before);
    assert_eq!(state.runtime.read().await.profile, runtime_before);
    // A profile-switch report grants choices for that unselected target,
    // and consecutive choices must preserve the report and current profile.
    let report = nexus_protocol::CompatibilityReport {
        checker_version: 1,
        status: "needs_choice".to_owned(),
        source_profile: "target".to_owned(),
        effective_profile: "nexus-target".to_owned(),
        release_id: "profile-runtime".to_owned(),
        fingerprint: "fixture".to_owned(),
        checked_at_unix: 1,
        checked_disabled_plugins: None,
        trigger: Some("profile_switch".to_owned()),
        last_trigger: Some("profile_switch".to_owned()),
        last_used_at_unix: Some(1),
        cache_reused: false,
        disabled: Vec::new(),
        candidates: Vec::new(),
        error: Some("Choose plugin isolation".to_owned()),
    };
    let directory = state.paths.root.join("compatibility");
    nexus_core::write_json_atomic(&directory, &directory.join("latest.json"), &report).unwrap();
    for package in ["plugin-one", "plugin-two"] {
        let response = super::profile_control(
            State(state.clone()),
            Json(nexus_protocol::ProfileCommand {
                action: nexus_protocol::ProfileAction::PluginDisable,
                profile: Some("target".to_owned()),
                package: Some(package.to_owned()),
                target: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(state.profiles.load().unwrap(), before);
    assert_eq!(super::compatibility::latest(&state.paths), Some(report));
    let policy: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join("profiles/.nexus-plugin-isolation/target.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(policy, serde_json::json!(["plugin-one", "plugin-two"]));
    let response = super::profile_list_response(&state, before).unwrap();
    assert_eq!(response.disabled_plugins, vec!["plugin-one", "plugin-two"]);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn compatibility_preflight_failure_preserves_release_selection() {
    let (state, root) = content_test_state("compatibility-preflight");
    state
        .releases
        .register("compat-old", "old", None, None)
        .unwrap();
    state
        .releases
        .register("compat-target", "target", None, None)
        .unwrap();
    state.releases.promote("compat-old").unwrap();
    let before = state.releases.load().unwrap();
    let mut config = state.config.load().unwrap();
    let mut harness = HarnessLaunchSpec::new(root.join("unused-runtime/node.exe"));
    harness.mode = nexus_protocol::HarnessLaunchMode::Node;
    harness.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
    config.harness = Some(harness);
    state.config.write(&config).unwrap();
    fs::create_dir_all(state.paths.root.join("compatibility")).unwrap();
    let latest = state.paths.root.join("compatibility/latest.json");
    fs::write(&latest, b"previous successful report").unwrap();
    // The synthetic target deliberately lacks the supported CLI entry.
    // Preflight must reject it before any process launch or promotion.
    let response = release_control(
        State(state.clone()),
        Json(ReleaseCommand {
            action: ReleaseAction::Promote,
            id: Some("compat-target".to_owned()),
            version: None,
            source: None,
            note: None,
            ..ReleaseCommand::default()
        }),
    )
    .await;
    assert!(!response.status().is_success());
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("profile_compatibility_failed"));
    assert_eq!(state.releases.load().unwrap(), before);
    assert!(
        !latest.exists(),
        "a failed recheck must invalidate the previous success"
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn manual_promotion_preview_binds_consent_without_changing_the_selection() {
    let (state, root) = content_test_state("promotion-preview");
    for id in ["old", "target"] {
        state.releases.register(id, "1", None, None).unwrap();
    }
    state.releases.promote("old").unwrap();
    let before = fs::read(&state.paths.release_pointers_file).unwrap();
    let response = release_control(
        State(state.clone()),
        Json(ReleaseCommand {
            action: ReleaseAction::Promote,
            id: Some("target".into()),
            inspect_only: true,
            ..ReleaseCommand::default()
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["rollback_confirmation"].as_str(),
        state
            .releases
            .promotion_risk_confirmation("target")
            .unwrap()
            .as_deref()
    );
    assert!(value["rollback_confirmation"]
        .as_str()
        .unwrap()
        .starts_with("unprotected-promotion-"));
    assert_eq!(
        fs::read(&state.paths.release_pointers_file).unwrap(),
        before
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn release_register_cannot_mutate_while_cold_publication_owns_update_gate() {
    let (state, root) = content_test_state("register-gate");
    let _cold_update_gate = state
        .updater
        .try_acquire_gate()
        .expect("synthetic cold publication owns updater gate");
    let response = release_control(
        State(state.clone()),
        Json(ReleaseCommand {
            action: ReleaseAction::Register,
            id: Some("external-slot".to_owned()),
            version: Some("1.0.0".to_owned()),
            source: None,
            note: None,
            ..ReleaseCommand::default()
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!state.paths.releases_dir.join("external-slot").exists());
    fs::remove_dir_all(root).expect("fixture removes");
}

#[tokio::test]
async fn materialization_failure_stays_prepared_blocks_mutations_and_abort_rolls_back() {
    let (mut state, root) = content_test_state("pending-abort");
    let selected_home = state.snapshots.configured_dsh_home().unwrap();
    state
        .config
        .transaction(|document| {
            document.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(selected_home.to_string_lossy().into_owned()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    state.snapshots = super::snapshots::SnapshotCoordinator::new(
        state.paths.clone(),
        Ok(root.join("different-default-home")),
    );
    let response = checkpoint_create(state.clone(), Some("before change".to_owned())).await;
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("checkpoint response reads");
    assert!(!body
        .windows(b"DUMMY-OLD-SECRET".len())
        .any(|window| window == b"DUMMY-OLD-SECRET"));
    let created: CheckpointCreateResponse =
        serde_json::from_slice(&body).expect("checkpoint response parses");
    assert!(created.checkpoint.snapshot.is_some());

    let dsh_home = root.join("dsh-home");
    let profile = dsh_home.join("profiles/demo");
    write_profile_file(
        &profile.join("package.json"),
        r#"{"name":"demo","version":"2.0.0","dependencies":{"changed":"1"}}"#,
    );
    write_profile_file(
        &dsh_home.join("settings.yaml"),
        "provider:\n  apiKey: DUMMY-CURRENT-SECRET\n  mode: current\n",
    );

    let response = checkpoint_restore(state.clone(), created.checkpoint.id.clone()).await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("pending response reads");
    let pending: CheckpointRestoreResponse =
        serde_json::from_slice(&body).expect("pending response parses");
    let pending_status = pending.pending_restore.expect("pending status is explicit");
    assert!(pending_status.materialization_pending);
    assert!(pending_status.retryable);
    assert!(pending_status.abortable);
    assert_eq!(
        state
            .checkpoint_restores
            .load()
            .expect("pending journal reads")
            .expect("Prepared remains")
            .phase,
        nexus_core::CheckpointRestorePhase::Prepared
    );
    assert!(fs::read_to_string(profile.join("package.json"))
        .expect("applied package reads")
        .contains("1.0.0"));
    let applied_settings =
        fs::read_to_string(dsh_home.join("settings.yaml")).expect("applied settings read");
    assert!(applied_settings.contains("mode: old"));
    assert!(applied_settings.contains("DUMMY-CURRENT-SECRET"));
    assert!(!applied_settings.contains("DUMMY-OLD-SECRET"));

    let blocked = checkpoint_create(state.clone(), Some("blocked".to_owned())).await;
    assert_eq!(blocked.status(), axum::http::StatusCode::CONFLICT);
    let config_before = fs::read(&state.paths.config_file).unwrap();
    let pointers_before = fs::read(&state.paths.release_pointers_file).ok();
    for scope in ["config", "slots"] {
        let reset = super::maintenance_control(
            State(state.clone()),
            Json(super::MaintenanceRequest {
                expected_revision: Some(state.config.snapshot().unwrap().revision),
                action: "reset".into(),
                scope: Some(scope.into()),
            }),
        )
        .await;
        assert_eq!(reset.status(), StatusCode::CONFLICT);
        assert_eq!(fs::read(&state.paths.config_file).unwrap(), config_before);
        assert_eq!(
            fs::read(&state.paths.release_pointers_file).ok(),
            pointers_before
        );
        assert_eq!(
            state.snapshots.configured_dsh_home().unwrap(),
            selected_home
        );
    }
    let response = checkpoint_restore_abort(state.clone(), Some(created.checkpoint.id)).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(state
        .checkpoint_restores
        .load()
        .expect("journal reloads")
        .is_none());
    assert!(fs::read_to_string(profile.join("package.json"))
        .expect("rolled-back package reads")
        .contains("2.0.0"));
    let rolled_back_settings =
        fs::read_to_string(dsh_home.join("settings.yaml")).expect("rolled-back settings read");
    assert!(rolled_back_settings.contains("mode: current"));
    assert!(rolled_back_settings.contains("DUMMY-CURRENT-SECRET"));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn retry_reuses_prepared_ticket_and_finishes_after_transient_pnpm_failure() {
    let (state, root) = content_test_state("retry");
    let fake_pnpm = root.join("fake-pnpm.js");
    write_profile_file(
        &fake_pnpm,
        "require('fs').writeFileSync(require('path').join(process.cwd(), 'attempt.txt'), 'failed'); process.exit(19);\n",
    );
    let node = executable_on_path(if cfg!(windows) { "node.exe" } else { "node" })
        .expect("test host provides Node required by the DSH runtime contract");
    let mut config = state.config.load().expect("config loads");
    config.runtime = Some(RuntimeConfig {
        node: Some(RuntimePin {
            path: node,
            ownership: RuntimeOwnership::System,
        }),
        pnpm: Some(RuntimePin {
            path: fake_pnpm.clone(),
            ownership: RuntimeOwnership::System,
        }),
        git: None,
        source: RuntimeSource::Official,
        mode: RuntimeInstallMode::Portable,
    });
    state.config.write(&config).expect("runtime config writes");

    let response = checkpoint_create(state.clone(), Some("retry source".to_owned())).await;
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("checkpoint response reads");
    let created: CheckpointCreateResponse =
        serde_json::from_slice(&body).expect("checkpoint response parses");
    let profile = root.join("dsh-home/profiles/demo");
    write_profile_file(
        &profile.join("package.json"),
        r#"{"name":"demo","version":"2.0.0","dependencies":{"changed":"1"}}"#,
    );
    let response = checkpoint_restore(state.clone(), created.checkpoint.id.clone()).await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        fs::read_to_string(profile.join("attempt.txt")).expect("failed attempt records"),
        "failed"
    );
    let first = state
        .checkpoint_restores
        .load()
        .expect("journal loads")
        .expect("Prepared remains");
    let ticket_id = first
        .intent
        .snapshot
        .as_ref()
        .expect("content binding remains")
        .ticket
        .ticket_id
        .clone();

    write_profile_file(
        &fake_pnpm,
        "require('fs').writeFileSync(require('path').join(process.cwd(), 'attempt.txt'), 'succeeded');\n",
    );
    let response = checkpoint_restore_retry(state.clone(), Some(ticket_id.clone())).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(state
        .checkpoint_restores
        .load()
        .expect("journal reloads")
        .is_none());
    assert_eq!(
        fs::read_to_string(profile.join("attempt.txt")).expect("successful retry records"),
        "succeeded"
    );
    assert!(fs::read_to_string(profile.join("package.json"))
        .expect("restored package reads")
        .contains("1.0.0"));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn startup_rolls_back_prepared_content_and_finishes_committed_content() {
    let (state, root) = content_test_state("startup-content");
    let response = checkpoint_create(state.clone(), Some("startup source".to_owned())).await;
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("checkpoint response reads");
    let checkpoint: CheckpointCreateResponse =
        serde_json::from_slice(&body).expect("checkpoint response parses");
    let snapshot_id = checkpoint
        .checkpoint
        .snapshot
        .as_ref()
        .expect("content checkpoint")
        .snapshot_id
        .clone();
    let dsh_home = root.join("dsh-home");
    let settings = dsh_home.join("settings.yaml");
    let catalog = state.profiles.load().expect("profile catalog loads");
    let base_intent = CheckpointRestoreIntent {
        checkpoint_id: checkpoint.checkpoint.id.clone(),
        previous_profiles: catalog.clone(),
        previous_current_release: None,
        previous_last_known_good: None,
        target_profiles: catalog,
        target_current_release: None,
        target_last_known_good: None,
        snapshot: None,
    };

    write_profile_file(
        &settings,
        "provider:\n  apiKey: DUMMY-CURRENT-SECRET\n  mode: current\n",
    );
    let lease = state
        .snapshots
        .acquire("demo".to_owned())
        .await
        .expect("snapshot owner acquires");
    let ticket = lease
        .prepare(snapshot_id.clone())
        .await
        .expect("restore prepares");
    let mut prepared_intent = base_intent.clone();
    prepared_intent.snapshot = Some(snapshots::binding_for(&lease, ticket.clone()));
    state
        .checkpoint_restores
        .begin(prepared_intent.clone())
        .expect("outer Prepared writes first");
    lease.apply(ticket).await.expect("content applies");
    drop(lease);
    recover_checkpoint_restore_startup(
        &state.checkpoint_restores,
        &state.profiles,
        &state.releases,
        &state.snapshots,
    )
    .await
    .expect("startup rolls Prepared back");
    let rolled_back = fs::read_to_string(&settings).expect("rolled-back settings read");
    assert!(rolled_back.contains("mode: current"));
    assert!(rolled_back.contains("DUMMY-CURRENT-SECRET"));
    assert!(state
        .checkpoint_restores
        .load()
        .expect("journal loads")
        .is_none());

    let lease = state
        .snapshots
        .acquire("demo".to_owned())
        .await
        .expect("snapshot owner reacquires");
    let ticket = lease.prepare(snapshot_id).await.expect("restore prepares");
    let mut committed_intent = base_intent;
    committed_intent.snapshot = Some(snapshots::binding_for(&lease, ticket.clone()));
    state
        .checkpoint_restores
        .begin(committed_intent.clone())
        .expect("outer Prepared writes");
    lease.apply(ticket).await.expect("content reapplies");
    state
        .checkpoint_restores
        .mark_committed(&committed_intent)
        .expect("outer Committed writes");
    drop(lease);
    recover_checkpoint_restore_startup(
        &state.checkpoint_restores,
        &state.profiles,
        &state.releases,
        &state.snapshots,
    )
    .await
    .expect("startup finishes Committed");
    let committed = fs::read_to_string(settings).expect("committed settings read");
    assert!(committed.contains("mode: old"));
    assert!(committed.contains("DUMMY-CURRENT-SECRET"));
    assert!(state
        .checkpoint_restores
        .load()
        .expect("journal loads")
        .is_none());
    let _ = fs::remove_dir_all(root);
}

fn release_marker_command(marker: &Path) -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                format!(
                    "Add-Content -LiteralPath '{}' -Value '{{release}}'; Start-Sleep -Seconds 30",
                    marker.display()
                ),
            ],
        )
    } else {
        (
            PathBuf::from("sh"),
            vec![
                "-c".to_owned(),
                format!(
                    "printf '%s\\n' '{{release}}' >> '{}'; sleep 10",
                    marker.display()
                ),
            ],
        )
    }
}

async fn wait_for_marker_lines(marker: &Path, expected: usize) -> Vec<String> {
    timeout(Duration::from_secs(15), async {
        loop {
            let lines: Vec<_> = fs::read_to_string(marker)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
            if lines.len() >= expected {
                return lines;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Harness release marker is written")
}

#[tokio::test]
async fn legacy_committed_restore_uses_raw_tuple_without_granting_health() {
    let (state, root) = content_test_state("legacy-committed-lkg");
    for id in ["old", "target", "external"] {
        state.releases.register(id, "1", None, None).unwrap();
    }
    let public = state
        .releases
        .restore_release_pointers(Some("target"), Some("old"))
        .unwrap();
    assert!(public.last_known_good.is_none());
    let profiles = state.profiles.load().unwrap();
    let intent = CheckpointRestoreIntent {
        checkpoint_id: "legacy-checkpoint".into(),
        previous_profiles: profiles.clone(),
        target_profiles: profiles,
        previous_current_release: Some("old".into()),
        previous_last_known_good: None,
        target_current_release: Some("target".into()),
        target_last_known_good: Some("old".into()),
        snapshot: None,
    };
    state.checkpoint_restores.begin(intent.clone()).unwrap();
    state.checkpoint_restores.mark_committed(&intent).unwrap();
    recover_checkpoint_restore_startup(
        &state.checkpoint_restores,
        &state.profiles,
        &state.releases,
        &state.snapshots,
    )
    .await
    .unwrap();
    assert!(
        state.checkpoint_restores.load().unwrap().is_none(),
        "legacy committed transaction settles"
    );
    assert_eq!(
        state
            .releases
            .stored_release_pointers()
            .unwrap()
            .1
            .as_deref(),
        Some("old")
    );
    assert!(
        state.releases.load().unwrap().last_known_good.is_none(),
        "raw recovery evidence is not verified health"
    );
    assert!(state.releases.rollback().is_err());
    state.checkpoint_restores.begin(intent.clone()).unwrap();
    state.checkpoint_restores.mark_committed(&intent).unwrap();
    state
        .releases
        .restore_release_pointers(Some("target"), Some("external"))
        .unwrap();
    assert!(recover_checkpoint_restore_startup(
        &state.checkpoint_restores,
        &state.profiles,
        &state.releases,
        &state.snapshots
    )
    .await
    .is_err());
    assert!(
        state.checkpoint_restores.load().unwrap().is_some(),
        "different unverified pointer remains an actual conflict"
    );
    assert_recovery_agent_available(&state.paths).await;
    assert!(state.checkpoint_restores.load().unwrap().is_some());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn dated_record_restore_through_real_agent_survives_restart_without_launch() {
    let (state, root) = content_test_state("dated-record-restore");
    let paths = state.paths.clone();
    let points = nexus_core::profile_history::list(&paths).unwrap();
    let point = points[0]["id"].clone();
    let mut previous_instance = String::new();
    for phase in 0..3 {
        if phase == 0 {
            fs::write(&paths.profiles_file, b"{broken").unwrap();
        }
        if phase == 1 {
            nexus_core::ProfileStore::new(paths.clone())
                .select("other")
                .unwrap();
        }
        let server = tokio::spawn(super::run(nexus_core::NexusConfig {
            data_dir: Some(paths.root.clone()),
            port: 0,
        }));
        let discovery = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(!server.is_finished());
                if let Ok(Some(record)) = paths.read_agent_discovery() {
                    if record.instance_id != previous_instance {
                        break record;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        previous_instance = discovery.instance_id.clone();
        let client = nexus_launcher_core::AgentClient::new(discovery.port)
            .unwrap()
            .with_expected_identity(nexus_launcher_core::AgentIdentity {
                data_root_id: discovery.data_root_id,
                instance_id: discovery.instance_id,
            })
            .with_credential_paths(paths.clone());
        let health: serde_json::Value = client.get_json("/v1/health").await.unwrap();
        assert_eq!(health["degraded"] == true, phase == 0);
        if phase < 2 {
            let listed: serde_json::Value = client.get_json("/v1/recovery/records").await.unwrap();
            let restored:serde_json::Value=client.post_json("/v1/recovery/records",&serde_json::json!({"action":"restore","point_id":point,"expected_revision":listed["expected_revision"]})).await.unwrap();
            assert_eq!(restored["restored"], true);
            assert_eq!(restored["active_profile"], "demo");
            if phase == 1 {
                let current: serde_json::Value = client.get_json("/v1/state").await.unwrap();
                assert_eq!(current["state"]["profile"], "demo");
            }
        }
        if phase > 0 {
            assert!(super::recovery_mode::paused(&paths).unwrap());
            assert!(super::recovery_mode::ensure_start_allowed(&paths).is_err());
            assert!(client
                .post_json::<_, serde_json::Value>(
                    "/v1/harness",
                    &serde_json::json!({"action":"start"})
                )
                .await
                .is_err());
        }
        let _: serde_json::Value = client.post_empty("/v1/shutdown").await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    fs::remove_dir_all(root).unwrap();
}

// Exercise the real initialization, discovery, authorization and route
// chain, not merely the recovery helper's return value.
async fn assert_recovery_agent_available(paths: &nexus_core::NexusPaths) {
    let config = nexus_core::NexusConfig {
        data_dir: Some(paths.root.clone()),
        port: 0,
    };
    let server = tokio::spawn(super::run(config));
    let discovery = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            assert!(
                !server.is_finished(),
                "Agent exited before exposing recovery APIs"
            );
            if let Ok(Some(record)) = paths.read_agent_discovery() {
                break record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Agent publishes discovery");
    let client = nexus_launcher_core::AgentClient::new(discovery.port)
        .unwrap()
        .with_expected_identity(nexus_launcher_core::AgentIdentity {
            data_root_id: discovery.data_root_id,
            instance_id: discovery.instance_id,
        })
        .with_credential_paths(paths.clone());
    let health: serde_json::Value = client.get_json("/v1/health").await.unwrap();
    let _: serde_json::Value = client.get_json("/v1/diagnostics").await.unwrap();
    if health["degraded"] == true {
        assert_eq!(health["read_only"], true);
        let recovery: serde_json::Value = client.get_json("/v1/recovery").await.unwrap();
        assert_eq!(recovery["degraded"], true);
        let records: serde_json::Value = client.get_json("/v1/recovery/records").await.unwrap();
        assert_eq!(records["records"].as_array().unwrap().len(), 5);
        let record = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["can_backup"] == true)
            .unwrap();
        let backup: serde_json::Value = client.post_json("/v1/recovery/records", &serde_json::json!({"action":"backup","record_id":record["id"],"expected_revision":record["revision"]})).await.unwrap();
        assert_eq!(backup["original_unchanged"], true);
        assert!(std::path::Path::new(backup["backup_path"].as_str().unwrap()).is_file());
        for path in [
            "/v1/config",
            "/v1/maintenance",
            "/v1/updates",
            "/v1/profiles",
            "/v1/recovery",
        ] {
            assert!(client
                .post_json::<_, serde_json::Value>(path, &serde_json::json!({"action":"reset"}))
                .await
                .is_err());
        }
        let exported: serde_json::Value = client
            .post_json("/v1/diagnostics", &serde_json::json!({"action":"export"}))
            .await
            .unwrap();
        assert!(std::path::Path::new(exported["export_path"].as_str().unwrap()).is_file());
    }
    let checked: serde_json::Value = client.get_json("/v1/preflight").await.unwrap();
    assert_eq!(checked["ready"], false);
    let error = client
        .post_json::<_, serde_json::Value>("/v1/harness", &serde_json::json!({"action":"start"}))
        .await
        .unwrap_err();
    let nexus_launcher_core::AgentClientError::Http { body, .. } = error else {
        panic!("Expected structured startup rejection: {error}");
    };
    let failure: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        matches!(
            failure["code"].as_str(),
            Some(
                "agent_recovery_required"
                    | "checkpoint_recovery_failed"
                    | "checkpoint_restore_journal_failed"
                    | "cold_publication_pending"
                    | "cold_operation_unavailable"
                    | "cold_cleanup_pending"
            )
        ),
        "{failure}"
    );
    let _: serde_json::Value = client.post_empty("/v1/shutdown").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn startup_recovery_errors_keep_real_agent_api_available() {
    for name in ["checkpoint-restore.json", "cold-publication.json"] {
        let (state, root) = content_test_state("recovery-api");
        let evidence = if name.starts_with("checkpoint") {
            state.paths.run_dir.join(name)
        } else {
            state.paths.root.join(name)
        };
        if name.starts_with("checkpoint") {
            fs::write(&evidence, b"{broken").unwrap();
        } else {
            fs::create_dir(&evidence).unwrap();
        } // Ordinary read I/O failure, not a typed publication conflict.
        assert_recovery_agent_available(&state.paths).await;
        if name.starts_with("checkpoint") {
            assert_eq!(fs::read(&evidence).unwrap(), b"{broken");
        } else {
            assert!(evidence.is_dir());
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn damaged_initialization_documents_keep_read_only_api_and_original_bytes() {
    for name in [
        "profiles.json",
        "update-state.json",
        "release-pointers.json",
        "state.json",
        "run/harness-log-session.json",
        "releases/bad/manifest.json",
    ] {
        let (state, root) = content_test_state("degraded-api");
        let path = state.paths.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = if name == "profiles.json" {
            br#"{"schema_version":999,"active_profile":"demo","profiles":["demo"]}"#.as_slice()
        } else {
            b"{broken"
        };
        fs::write(&path, original).unwrap();
        assert_recovery_agent_available(&state.paths).await;
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "{name} must remain untouched"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn background_crash_capture_without_http_retries_failed_collection() {
    let (state, root) = content_test_state("background-crash");
    let mut launch =
        nexus_core::HarnessLaunchSpec::new(std::path::PathBuf::from("C:/Windows/System32/cmd.exe"));
    launch.mode = nexus_protocol::HarnessLaunchMode::Direct;
    launch.args = vec!["/d".into(), "/c".into(), "exit 7".into()];
    state
        .config
        .transaction(|config| {
            config.harness = Some(launch);
            Ok(())
        })
        .unwrap();
    let _ = state.supervisor.start_with_profile("demo").await;
    // Make only diagnostic collection fail; the supervisor and its logs
    // still work. No status/control HTTP request drives the observer.
    fs::remove_dir(&state.paths.diagnostics_dir).unwrap();
    fs::write(&state.paths.diagnostics_dir, b"temporary obstruction").unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let observer = super::start_crash_observer(state.clone(), receiver);
    timeout(Duration::from_secs(10), async {
        loop {
            if state.crash_capture_run.lock().await.attempts == 1 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    while state
        .crash_capture_run
        .lock()
        .await
        .in_flight
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(!state.crash_capture_run.lock().await.completed);
    fs::remove_file(&state.paths.diagnostics_dir).unwrap();
    fs::create_dir(&state.paths.diagnostics_dir).unwrap();
    timeout(Duration::from_secs(12), async {
        loop {
            if state.crash_capture_run.lock().await.completed {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.crash_capture_run.lock().await.attempts, 2);
    assert_eq!(state.diagnostics.list().unwrap().len(), 1);
    let _ = shutdown.send(true);
    observer.await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn startup_recovers_prepared_and_validates_committed_checkpoint_restore() {
    let root = std::env::temp_dir().join(format!(
        "nexus-agent-checkpoint-recovery-{}-{}",
        std::process::id(),
        nexus_core::unix_time_seconds()
    ));
    let paths = NexusPaths::from_root(root.clone());
    let profiles = ProfileStore::new(paths.clone());
    let releases = {
        let config_store = ConfigStore::new(paths.clone());
        let max_slots = config_store
            .load()
            .ok()
            .and_then(|config| config.releases)
            .map(|releases| releases.max_slots_usize())
            .unwrap_or(DEFAULT_MAX_RELEASE_SLOTS);
        ReleaseStore::new(paths.clone()).with_max_slots(max_slots)
    };
    let journals = CheckpointRestoreJournalStore::new(paths.clone());
    let snapshot_coordinator = snapshots::SnapshotCoordinator::new(
        paths.clone(),
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "test DSH home unused",
        )),
    );
    releases
        .register("harness-a", "a", None, None)
        .expect("release A registers");
    releases
        .register("harness-b", "b", None, None)
        .expect("release B registers");
    releases.promote("harness-a").expect("release A promotes");
    let previous_releases = releases.load().expect("previous release loads");
    let previous_profiles = profiles.load().expect("previous profile loads");
    let target_releases = releases
        .plan_checkpoint_release(Some("harness-b"))
        .expect("target release plans");
    let target_profiles = ProfileCatalog::new("restored", previous_profiles.profiles.clone())
        .expect("target profile validates");
    let intent = CheckpointRestoreIntent {
        checkpoint_id: "checkpoint-recovery".to_owned(),
        previous_profiles: previous_profiles.clone(),
        previous_current_release: previous_releases.current_release.clone(),
        previous_last_known_good: previous_releases.last_known_good.clone(),
        target_profiles: target_profiles.clone(),
        target_current_release: target_releases.current_release.clone(),
        target_last_known_good: target_releases.last_known_good.clone(),
        snapshot: None,
    };

    journals
        .begin(intent.clone())
        .expect("Prepared writes first");
    releases
        .restore_release_pointers(
            intent.target_current_release.as_deref(),
            intent.target_last_known_good.as_deref(),
        )
        .expect("partial target release writes");
    profiles
        .write(&target_profiles)
        .expect("partial target profile writes");
    recover_checkpoint_restore_startup(&journals, &profiles, &releases, &snapshot_coordinator)
        .await
        .expect("Prepared rolls back on startup");
    assert_eq!(profiles.load().expect("profiles reload"), previous_profiles);
    assert_eq!(releases.load().expect("releases reload"), previous_releases);
    assert!(journals.load().expect("journal reloads").is_none());

    journals
        .begin(intent.clone())
        .expect("second Prepared writes");
    releases
        .restore_release_pointers(
            intent.target_current_release.as_deref(),
            intent.target_last_known_good.as_deref(),
        )
        .expect("target release writes");
    profiles
        .write(&target_profiles)
        .expect("target profile writes");
    journals.mark_committed(&intent).expect("Committed writes");
    profiles
        .write(&previous_profiles)
        .expect("committed mismatch injects");
    assert!(recover_checkpoint_restore_startup(
        &journals,
        &profiles,
        &releases,
        &snapshot_coordinator,
    )
    .await
    .is_err());
    assert_eq!(
        journals
            .load()
            .expect("mismatched journal reloads")
            .expect("Committed remains")
            .phase,
        nexus_core::CheckpointRestorePhase::Committed
    );
    profiles
        .write(&target_profiles)
        .expect("target profile repairs");
    recover_checkpoint_restore_startup(&journals, &profiles, &releases, &snapshot_coordinator)
        .await
        .expect("Committed validates on startup");
    assert_eq!(
        profiles.load().expect("target profiles reload"),
        target_profiles
    );
    assert_eq!(
        releases.load().expect("target releases reload"),
        target_releases
    );
    assert!(journals.load().expect("journal reloads").is_none());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn checkpoint_restore_survives_cancellation_serializes_start_and_rolls_back() {
    let root = std::env::temp_dir().join(format!(
        "nexus-agent-checkpoint-release-{}-{}",
        std::process::id(),
        nexus_core::unix_time_seconds()
    ));
    let paths = NexusPaths::from_root(root.clone());
    paths.ensure_directories().expect("directories create");
    let dsh_home = root.with_file_name(format!(
        "nexus-agent-checkpoint-dsh-{}-{}",
        std::process::id(),
        nexus_core::unix_time_nanos_for_update()
    ));
    for profile in ["default", "restored", "web"] {
        let profile_dir = dsh_home.join("profiles").join(profile);
        fs::create_dir_all(&profile_dir).expect("synthetic DSH profile creates");
        fs::write(
            profile_dir.join("package.json"),
            format!(r#"{{"name":"fixture-{profile}","dependencies":{{}}}}"#),
        )
        .expect("synthetic profile package writes");
    }
    let releases = {
        let config_store = ConfigStore::new(paths.clone());
        let max_slots = config_store
            .load()
            .ok()
            .and_then(|config| config.releases)
            .map(|releases| releases.max_slots_usize())
            .unwrap_or(DEFAULT_MAX_RELEASE_SLOTS);
        ReleaseStore::new(paths.clone()).with_max_slots(max_slots)
    };
    releases
        .register("harness-a", "a", None, None)
        .expect("release A registers");
    releases
        .register("harness-b", "b", None, None)
        .expect("release B registers");
    releases.promote("harness-a").expect("release A promotes");

    let checkpoints = CheckpointStore::new(paths.clone());
    let checkpoint = checkpoints
        .create(
            "restored",
            Some("harness-a".to_owned()),
            Some("release A".to_owned()),
            NexusStateSnapshot {
                profile: "restored".to_owned(),
                release: Some("harness-a".to_owned()),
            },
        )
        .expect("release A checkpoint creates");
    releases.promote("harness-b").expect("release B promotes");

    let marker = root.join("release-marker.txt");
    let (program, args) = release_marker_command(&marker);
    let config = ConfigStore::new(paths.clone());
    config
        .write(&NexusConfigFile {
            update_attempt_id: None,
            external_harness: None,
            schema_version: 1,
            harness_preferences: None,
            harness: Some(HarnessLaunchSpec {
                mode: Default::default(),
                program,
                args,
                working_dir: None,
                readiness_url: None,
                readiness_timeout_secs: None,
                readiness_token_required: false,
            }),
            update: None,

            releases: None,
            runtime: None,
            snapshots: None,
        })
        .expect("Harness config writes");
    let profiles = ProfileStore::new(paths.clone());
    profiles.load().expect("default profile creates");
    let supervisor =
        HarnessSupervisor::with_graceful_wait(paths.clone(), Duration::from_millis(100))
            .expect("supervisor creates");
    let mut runtime = AgentState::starting();
    runtime.mark_running();
    runtime.set_release(Some("harness-b".to_owned()));
    runtime.set_harness(HarnessState::Stopped);
    let (shutdown, _) = watch::channel(false);
    let (transition_reached, transition_reached_rx) = oneshot::channel();
    let (transition_release, transition_release_rx) = oneshot::channel();
    let state = AppState {
        paths: paths.clone(),
        runtime: Arc::new(RwLock::new(runtime)),
        agent_revision: Arc::new(AtomicU64::new(0)),
        profiles,
        checkpoints,
        checkpoint_restores: CheckpointRestoreJournalStore::new(paths.clone()),
        releases: releases.clone(),
        diagnostics: DiagnosticsStore::new(paths.clone()),
        config,
        updater: UpdateExecutor::new(paths.clone(), releases.clone()),
        cold: crate::cold::ColdCoordinator::new(paths.clone()),
        supervisor: supervisor.clone(),
        snapshots: snapshots::SnapshotCoordinator::new(paths.clone(), Ok(dsh_home.clone())),
        harness_sync: Arc::new(Mutex::new(())),
        maintenance_preview: Arc::new(std::sync::Mutex::new(
            crate::MaintenancePreviewScan::default(),
        )),
        checkpoint_transition_gate: Arc::new(Mutex::new(Some(CheckpointTransitionGate {
            reached: transition_reached,
            release: transition_release_rx,
        }))),
        agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown,
        data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
        instance_id: "checkpoint-test-agent".to_owned(),
        crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())),
        canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(
            nexus_launcher_core::HarnessLogObserver::default(),
        )),
    };

    let restore_state = state.clone();
    let checkpoint_id = checkpoint.id.clone();
    let restore =
        tokio::spawn(async move { checkpoint_restore(restore_state, checkpoint_id).await });
    transition_reached_rx
        .await
        .expect("checkpoint restore reaches the serialized transition");
    assert!(matches!(
        execute_harness_action(&state, nexus_protocol::HarnessAction::Start).await,
        Err(crate::HarnessSupervisorError::Busy)
    ));
    restore.abort();
    assert!(restore
        .await
        .expect_err("request cancellation aborts handler")
        .is_cancelled());
    transition_release
        .send(())
        .expect("checkpoint transition releases");
    let settled = supervisor.acquire_lifecycle().await;
    drop(settled);
    execute_harness_action(&state, nexus_protocol::HarnessAction::Start)
        .await
        .expect("Harness starts after restore owner settles");
    assert_eq!(
        releases
            .load()
            .expect("release pointers reload")
            .current_release
            .as_deref(),
        Some("harness-a")
    );
    assert!(state
        .checkpoint_restores
        .load()
        .expect("completed journal reloads")
        .is_none());
    assert_eq!(wait_for_marker_lines(&marker, 1).await, ["harness-a"]);
    execute_harness_action(&state, nexus_protocol::HarnessAction::Restart)
        .await
        .expect("Harness restarts from restored release");
    assert_eq!(
        wait_for_marker_lines(&marker, 2).await,
        ["harness-a", "harness-a"]
    );
    // The child writes its marker before the background readiness owner
    // publishes Running. Wait for that observable transition on fast Unix
    // children as well as slower Windows shells.
    timeout(Duration::from_secs(5), async {
        while supervisor.status().await.state == HarnessState::Starting {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restarted Harness readiness settles");
    assert_eq!(
        supervisor.status().await.state,
        HarnessState::Running,
        "checkpoint create conflict is exercised against a running Harness"
    );
    let checkpoint_count = state
        .checkpoints
        .list()
        .expect("checkpoint catalog reads")
        .len();
    let response = checkpoint_create(
        state.clone(),
        Some("must not snapshot a running Harness".to_owned()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        state
            .checkpoints
            .list()
            .expect("checkpoint catalog remains readable")
            .len(),
        checkpoint_count,
        "a rejected online create must not publish a manifest"
    );
    supervisor.stop().await.expect("Harness stops");
    let _ = sync_harness_state(&state).await;

    let response = checkpoint_create(
        state.clone(),
        Some("quiescent Harness selection".to_owned()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    assert_eq!(
        state
            .checkpoints
            .list()
            .expect("quiescent checkpoint catalog reads")
            .len(),
        checkpoint_count + 1
    );

    releases.promote("harness-b").expect("release B promotes");
    let prior_releases = releases.load().expect("prior release pointers load");
    let prior_profiles = state.profiles.select("web").expect("prior profile selects");
    update_agent_state(&state, |runtime| {
        runtime.set_profile("web".to_owned());
        runtime.set_release(Some("harness-b".to_owned()));
    })
    .await
    .expect("prior Agent state publishes");
    let prior_runtime = state.runtime.read().await.clone();
    assert_eq!(prior_runtime.lifecycle, AgentLifecycleState::Running);
    let prior_harness = prior_runtime.harness;
    let (failure_transition_reached, failure_transition_reached_rx) = oneshot::channel();
    let (failure_transition_release, failure_transition_release_rx) = oneshot::channel();
    *state.checkpoint_transition_gate.lock().await = Some(CheckpointTransitionGate {
        reached: failure_transition_reached,
        release: failure_transition_release_rx,
    });
    state
        .agent_persist_failure
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failing_restore_state = state.clone();
    let failing_checkpoint_id = checkpoint.id.clone();
    let failing_restore = tokio::spawn(async move {
        checkpoint_restore(failing_restore_state, failing_checkpoint_id).await
    });
    failure_transition_reached_rx
        .await
        .expect("failing restore reaches its detached transition owner");
    update_agent_state(&state, |runtime| runtime.request_shutdown())
        .await
        .expect("concurrent Agent shutdown publishes");
    assert_eq!(
        state.runtime.read().await.lifecycle,
        AgentLifecycleState::ShuttingDown
    );
    failure_transition_release
        .send(())
        .expect("failing restore transition releases");
    let response = failing_restore.await.expect("failing restore task joins");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        releases.load().expect("release rollback loads"),
        prior_releases
    );
    assert_eq!(
        state.profiles.load().expect("profile rollback loads"),
        prior_profiles
    );
    let rolled_back_runtime = state.runtime.read().await.clone();
    assert_eq!(
        rolled_back_runtime.lifecycle,
        AgentLifecycleState::ShuttingDown,
        "selection rollback must not replay the stale pre-shutdown Agent lifecycle"
    );
    assert_eq!(rolled_back_runtime.harness, prior_harness);
    assert_eq!(rolled_back_runtime.profile, prior_runtime.profile);
    assert_eq!(rolled_back_runtime.release, prior_runtime.release);
    let durable = supervisor
        .metadata_store()
        .read()
        .expect("runtime rollback loads")
        .expect("runtime rollback exists");
    assert_eq!(durable.lifecycle, AgentLifecycleState::ShuttingDown);
    assert_eq!(durable.harness.state, prior_harness);
    assert_eq!(durable.profile, prior_runtime.profile);
    assert_eq!(durable.release, prior_runtime.release);

    update_agent_state(&state, |runtime| runtime.mark_running())
        .await
        .expect("test Agent lifecycle returns to running");

    state
        .checkpoint_commit_result_failure
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let response = checkpoint_restore(state.clone(), checkpoint.id.clone()).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        state
            .releases
            .load()
            .expect("uncertain commit release loads")
            .current_release
            .as_deref(),
        Some("harness-a")
    );
    assert_eq!(
        state
            .profiles
            .load()
            .expect("uncertain commit profile loads")
            .active_profile,
        "restored"
    );
    assert!(state
        .checkpoint_restores
        .load()
        .expect("uncertain commit journal loads")
        .is_none());

    releases.promote("harness-b").expect("release B promotes");
    let quiescent_releases = releases.load().expect("release selection loads");
    let quiescent_profiles = state
        .profiles
        .select("web")
        .expect("profile selection loads");
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
        command
    } else {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("30");
        command
    };
    command.kill_on_drop(true);
    let child = command.spawn().expect("owned Harness test child starts");
    supervisor
        .inject_nonquiescent_failed_state(Some(child), false)
        .await
        .expect("Failed plus owned child is injected");
    let response = checkpoint_restore(state.clone(), checkpoint.id.clone()).await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        releases.load().expect("release remains"),
        quiescent_releases
    );
    assert_eq!(
        state.profiles.load().expect("profile remains"),
        quiescent_profiles
    );
    assert!(state
        .checkpoint_restores
        .load()
        .expect("owned-child journal reads")
        .is_none());
    supervisor.stop().await.expect("owned test child stops");

    supervisor
        .inject_nonquiescent_failed_state(None, true)
        .await
        .expect("Failed plus launch reservation is injected");
    let response = checkpoint_restore(state.clone(), checkpoint.id).await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        releases.load().expect("release remains"),
        quiescent_releases
    );
    assert_eq!(
        state.profiles.load().expect("profile remains"),
        quiescent_profiles
    );
    assert!(state
        .checkpoint_restores
        .load()
        .expect("launch-pending journal reads")
        .is_none());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(dsh_home);
}
