//! Headless Nexus control-plane process.
// CI can select its native PowerShell instead of assuming Windows PowerShell
// exists at the same System32 location on every Windows architecture.
#[cfg(test)]
fn test_powershell() -> std::path::PathBuf {
    std::env::var_os("NEXUS_TEST_POWERSHELL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"))
}

mod config_api;
use config_api::{config_control, config_response_for_paths};
#[cfg(test)]
use config_api::{redact_config_args, redact_config_url};

mod checkpoint_api;
use checkpoint_api::{
    bounded_checkpoint_diagnostic, checkpoint_control, checkpoint_list,
    ensure_checkpoint_mutation_ready, ensure_mutation_ready_for_owner, selected_dsh_version,
};
#[cfg(test)]
use checkpoint_api::{
    checkpoint_create, checkpoint_restore, checkpoint_restore_abort, checkpoint_restore_retry,
};

mod profile_api;
mod dependency_repair;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod macos_terminal;
#[cfg(test)]
use profile_api::profile_list_response;
use profile_api::{profile_control, profile_list};

mod canary;
mod process_recovery;
mod recovery_records;
mod runtime_patches;
mod notifications;
mod desktop_plugins;
mod desktop_profile;
mod market;
mod source_context;

#[cfg(windows)]
pub mod windows_harness;
#[cfg(windows)]
mod windows_terminal;
#[cfg(unix)]
mod unix_harness;

use std::{
    env, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    ops::Deref,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use axum::{
    extract::{Request, State},
    http::{
        header::{
            ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS,
            ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
        },
        HeaderValue, Method, StatusCode,
    },
    middleware::{self, Next},
    response::IntoResponse,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use nexus_core::{
    data_root_identity, discover_harness_candidates_with_paths, load_update_spec, new_instance_id,
    redact_diagnostics_payload, unix_time_seconds, AgentDiscoveryRecord, AgentState,
    CheckpointRestoreIntent, CheckpointRestoreJournal, CheckpointRestoreJournalStore,
    CheckpointRestorePhase, CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec,
    HarnessLogSession, HarnessLogSessionStore, NexusConfig, NexusConfigFile, NexusStateSnapshot,
    ProfileCatalog, ProfileStore, ReleaseCatalog, ReleaseStore, RuntimeConfig, SnapshotsConfig,
    UpdateSpec, DEFAULT_MAX_RELEASE_SLOTS, DEFAULT_PROFILE, HARNESS_ARGS_ENV, HARNESS_PROGRAM_ENV,
    HARNESS_READINESS_TIMEOUT_ENV, HARNESS_READINESS_URL_ENV, HARNESS_WORKING_DIR_ENV,
    UPDATE_BUILD_ARGS_ENV, UPDATE_BUILD_PROGRAM_ENV, UPDATE_GIT_PROGRAM_ENV, UPDATE_REF_ENV,
    UPDATE_SOURCE_ENV, UPDATE_TIMEOUT_ENV, UPDATE_VERIFY_ARGS_ENV, UPDATE_VERIFY_PROGRAM_ENV,
};
use nexus_launcher_core::{
    harness_observation_matches_session, read_harness_ui_info_with_observer,
    unavailable_harness_ui_info, HarnessLogObserver,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointAction, CheckpointCommand, CheckpointContentState,
    CheckpointCreateResponse, CheckpointListResponse, CheckpointManifest,
    CheckpointRestoreResponse, ConfigAction, ConfigCommand, ConfigResponse, DiagnosticsAction,
    DiagnosticsCommand, DiagnosticsResponse, ErrorResponse, HarnessAction, HarnessCommand,
    HarnessDiscoveryResponse, HarnessResponse, HarnessRuntimeInfo, HealthResponse,
    LifecycleAccepted, LifecycleAction, LifecycleCommand, PluginRemoveResponse, ProfileAction,
    ProfileCommand, ProfileListResponse, ProfileOpenPathResponse, ProfileSelectResponse,
    RecoveryLogTail, RecoveryStatusResponse, ReleaseAction, ReleaseCommand, ReleaseListResponse,
    RuntimeInstallMode, RuntimePlanRequest, RuntimeSource, SnapshotReference, StateResponse,
    TagListResponse, UpdateAction, UpdateCommand, UpdateResponse, UpdateState,
};
use tokio::{
    net::TcpListener,
    sync::{watch, Mutex, RwLock},
};

mod cold;
mod compatibility;
mod dsh;
#[doc(hidden)]
pub mod git_worker;
mod launch_inputs;
mod log_retention;
mod preference_capabilities;
mod preflight;
mod profile_archive;
mod profile_repair;
mod request_receipts;
mod runtime;
mod runtime_plan;
mod snapshots;
mod supervisor;
mod updater;

pub use supervisor::{HarnessSupervisor, HarnessSupervisorError};
pub use updater::{UpdateExecutor, UpdateExecutorError};

const DEFAULT_CONSOLE_PORT: u16 = 3091;
const CONSOLE_PORT_ENV: &str = "NEXUS_CONSOLE_PORT";
const CORS_ALLOWED_METHODS: &str = "GET, POST";
const CORS_ALLOWED_HEADERS: &str = "content-type, accept";
const CORS_MAX_AGE_SECS: &str = "300";
const PROXY_DATA_ROOT_HEADER: &str = "x-nexus-data-root-id";
const PROXY_INSTANCE_HEADER: &str = "x-nexus-instance-id";

struct AgentRuntimeLock {
    _file: fs::File,
}

#[derive(Clone)]
struct AppState {
    paths: nexus_core::NexusPaths,
    runtime: Arc<RwLock<AgentState>>,
    agent_revision: Arc<AtomicU64>,
    profiles: ProfileStore,
    checkpoints: CheckpointStore,
    checkpoint_restores: CheckpointRestoreJournalStore,
    releases: ReleaseStore,
    diagnostics: DiagnosticsStore,
    config: ConfigStore,
    updater: UpdateExecutor,
    cold: cold::ColdCoordinator,
    supervisor: HarnessSupervisor,
    snapshots: snapshots::SnapshotCoordinator,
    harness_sync: Arc<Mutex<()>>,
    maintenance_preview: Arc<std::sync::Mutex<MaintenancePreviewScan>>,
    crash_capture_run: Arc<Mutex<CrashCapture>>,
    timeout_capture_run: Arc<Mutex<CrashCapture>>,
    canary: canary::Owner,
    harness_logs: Arc<Mutex<HarnessLogObserver>>,
    #[cfg(test)]
    checkpoint_transition_gate: Arc<Mutex<Option<CheckpointTransitionGate>>>,
    #[cfg(test)]
    agent_persist_failure: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    checkpoint_commit_result_failure: Arc<std::sync::atomic::AtomicBool>,
    shutdown: watch::Sender<bool>,
    data_root_id: String,
    instance_id: String,
}

#[cfg(test)]
struct CheckpointTransitionGate {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
impl AppState {
    async fn wait_for_checkpoint_transition_gate(&self) {
        let gate = self.checkpoint_transition_gate.lock().await.take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.await;
        }
    }
}

#[derive(Clone)]
struct HarnessSnapshot {
    generation: u64,
    runtime: HarnessRuntimeInfo,
    log_session: HarnessLogSession,
}

impl Deref for HarnessSnapshot {
    type Target = HarnessRuntimeInfo;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl HarnessSnapshot {
    fn into_response(self) -> HarnessResponse {
        HarnessResponse::from_observation(
            self.runtime,
            self.generation,
            self.log_session.run_id,
            self.log_session.generation,
            self.log_session.stdout_watermark,
            self.log_session.stderr_watermark,
            self.log_session.stdout_file_identity,
            self.log_session.stderr_file_identity,
            self.log_session.stdout_log_name,
            self.log_session.stderr_log_name,
            self.log_session.launch_pending,
        )
    }
}

/// Run the Agent in the foreground until the lifecycle API or Ctrl+C requests shutdown.
pub async fn run(config: NexusConfig) -> io::Result<()> {
    run_with_instance_id(config, None).await
}

/// Run the Agent with an optional Launcher-provided startup nonce. The nonce
/// closes the spawn/health pairing; standalone invocations receive a fresh ID.
pub async fn run_with_instance_id(
    config: NexusConfig,
    expected_instance_id: Option<String>,
) -> io::Result<()> {
    let paths = config.paths();
    if let Err(rejection) = nexus_core::disk::validate_volume(&paths.root) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Nexus data root is not usable: {} ({})",
                rejection.message(),
                paths.root.display(),
            ),
        ));
    }
    paths.ensure_directories()?;
    let data_root_id = data_root_identity(&paths)?;
    let instance_id = match expected_instance_id {
        Some(value)
            if !value.is_empty() && value.len() <= 192 && !value.chars().any(char::is_control) =>
        {
            value
        }
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Agent instance ID is empty, too long, or contains a control character",
            ))
        }
        None => new_instance_id(),
    };
    let _runtime_lock = acquire_runtime_lock(&paths, &instance_id)?;
    let credential = nexus_core::agent_auth::AgentCredential::publish(&paths, &instance_id)?;
    macro_rules! initialize {
        ($value:expr, $stage:literal) => {
            match $value {
                Ok(value) => value,
                Err(error) => {
                    return run_read_only(
                        &config,
                        &paths,
                        credential.clone(),
                        &data_root_id,
                        &instance_id,
                        format!("{}: {error}", $stage),
                    )
                    .await
                }
            }
        };
    }

    let profiles = ProfileStore::new(paths.clone());
    let checkpoints = CheckpointStore::new(paths.clone());
    let config_store = ConfigStore::new(paths.clone());
    let snapshots = snapshots::SnapshotCoordinator::new(paths.clone(), dsh::resolve_dsh_home());
    let releases = {
        let max_slots = config_store
            .load()
            .ok()
            .and_then(|config| config.releases)
            .map(|releases| releases.max_slots_usize())
            .unwrap_or(DEFAULT_MAX_RELEASE_SLOTS);
        ReleaseStore::new(paths.clone()).with_max_slots(max_slots)
    };
    let checkpoint_restores = CheckpointRestoreJournalStore::new(paths.clone());
    if let Err(error) =
        recover_checkpoint_restore_startup(&paths, &checkpoint_restores, &profiles, &releases, &snapshots)
            .await
    {
        // Recovery evidence remains authoritative. Mutation/start guards read
        // it again; a failed recovery must not remove the repair API itself.
        tracing::warn!(%error, "Checkpoint recovery remains pending; Agent remains available");
    }
    let profile_catalog = initialize!(profiles.load(), "profiles");
    if let Err(error) = profile_archive::recover_if_present(&paths, &profiles) {
        tracing::warn!(%error, "Deleted profile recovery is pending; archive mutations remain blocked");
    }
    let diagnostics = DiagnosticsStore::new(paths.clone());
    let updater = UpdateExecutor::new(paths.clone(), releases.clone());
    let _ = initialize!(updater.recover_unattached(), "update state");
    let cold = cold::ColdCoordinator::new(paths.clone());
    if let Err(error) = cold.recover() {
        tracing::warn!(%error, "Cold recovery remains pending; Agent remains available");
    }
    if let Err(error) = canary::recover_unattached(&paths) {
        tracing::warn!(%error, "Interrupted Canary recovery is pending; retry cancellation after the owned processes exit");
    }
    let release_catalog = initialize!(releases.load(), "release catalog");
    if !release_catalog.unavailable_selections.is_empty() {
        tracing::warn!(releases = ?release_catalog.unavailable_selections,
            "Selected Harness release is missing or incomplete; Agent remains available for reinstallation");
    }
    let supervisor = initialize!(
        HarnessSupervisor::new(paths.clone()),
        "runtime metadata or log session"
    );
    let metadata = supervisor.metadata_store();
    // A restart can only recover a persisted Harness state by proving the
    // configured loopback readiness endpoint; it never reattaches a stale PID.
    let initial_harness = supervisor.recover_unattached().await;
    let mut initial_runtime = AgentState::starting();
    initial_runtime.profile = Some(profile_catalog.active_profile.clone());
    initial_runtime.release = release_catalog.current_release.clone();
    initial_runtime.harness = initial_harness.state;
    initialize!(
        metadata.write_snapshot(&initial_runtime, initial_harness.clone()),
        "runtime metadata"
    );
    let runtime = Arc::new(RwLock::new(initial_runtime));
    let agent_revision = Arc::new(AtomicU64::new(0));
    let instance_id_for_discovery = instance_id.clone();
    let data_root_id_for_discovery = data_root_id.clone();
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let state = AppState {
        paths: paths.clone(),
        runtime: Arc::clone(&runtime),
        agent_revision: Arc::clone(&agent_revision),
        profiles,
        checkpoints,
        checkpoint_restores,
        releases,
        diagnostics,
        config: config_store,
        updater,
        cold,
        supervisor: supervisor.clone(),
        snapshots,
        harness_sync: Arc::new(Mutex::new(())),
        maintenance_preview: Arc::new(std::sync::Mutex::new(
            crate::MaintenancePreviewScan::default(),
        )),
        crash_capture_run: Arc::new(Mutex::new(CrashCapture::default())),
        timeout_capture_run: Arc::new(Mutex::new(CrashCapture::default())),
        canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(
            nexus_launcher_core::HarnessLogObserver::default(),
        )),
        #[cfg(test)]
        checkpoint_transition_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        #[cfg(test)]
        checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown,
        data_root_id: data_root_id.clone(),
        instance_id: instance_id.clone(),
    };

    let listener = TcpListener::bind(config.bind_addr()).await?;
    let bound_address = listener.local_addr()?;
    {
        let mut current = runtime.write().await;
        current.mark_running();
        agent_revision.store(1, Ordering::SeqCst);
        if let Err(error) = metadata.write_snapshot(&current, initial_harness.clone()) {
            drop(current);
            drop(listener);
            return run_read_only(
                &config,
                &paths,
                credential,
                &data_root_id,
                &instance_id,
                format!("runtime metadata: {error}"),
            )
            .await;
        }
    }
    // Publish the bound port for discovery: internal ports are not fixed,
    // so launchers and CLIs locate the Agent through this record instead.
    let discovery_record = AgentDiscoveryRecord {
        port: bound_address.port(),
        instance_id: instance_id_for_discovery,
        data_root_id: data_root_id_for_discovery,
        pid: std::process::id(),
        updated_at_unix: unix_time_seconds(),
    };
    paths
        .publish_agent_discovery(&discovery_record)
        .map_err(|error| {
            io::Error::other(format!(
                "failed to publish the Agent discovery record: {error}"
            ))
        })?;

    tracing::info!(
        address = %bound_address,
        data_root = %paths.root.display(),
        "nexus agent listening"
    );

    let retention = log_retention::start(paths.clone(), shutdown_receiver.clone());
    let crash_observer = start_crash_observer(state.clone(), shutdown_receiver.clone());
    let server = axum::serve(listener, build_router(state, credential))
        .with_graceful_shutdown(wait_for_shutdown(shutdown_receiver));
    let result = server.await;
    retention.abort();
    crash_observer.abort();

    let _ = supervisor.stop().await;
    let harness = supervisor.status().await;
    let mut current = runtime.write().await;
    current.mark_stopped();
    current.harness = harness.state;
    metadata.write_snapshot(&current, harness)?;
    result
}

/// A damaged/future document must not require a functioning supervisor to
/// export evidence. This router never invokes normal mutation/state writers.
async fn run_read_only(
    config: &NexusConfig,
    paths: &nexus_core::NexusPaths,
    credential: nexus_core::agent_auth::AgentCredential,
    root_id: &str,
    instance_id: &str,
    reason: String,
) -> io::Result<()> {
    let (safe, _) = redact_diagnostics_payload(reason.as_bytes());
    let reason = String::from_utf8_lossy(&safe).into_owned();
    tracing::warn!(%reason, "Agent entered read-only recovery; repair the original files and restart");
    let (shutdown, receiver) = watch::channel(false);
    let mut health = serde_json::to_value(HealthResponse::healthy(
        root_id.to_owned(),
        instance_id.to_owned(),
    ))?;
    health["build_id"] = serde_json::json!(option_env!("NEXUS_BUILD_ID"));
    health["degraded"] = serde_json::json!(true);
    health["read_only"] = serde_json::json!(true);
    health["recovery_reason"] = serde_json::json!(reason);
    let recovery = serde_json::json!({"paused":true,"degraded":true,"read_only":true,"reason":reason,
        "next":"Export diagnostics, repair the original files without downgrading their format, then restart Agent. No damaged document was replaced."});
    let check = serde_json::json!({"ready":false,"paused":true,"checks":[{"id":"agent_recovery","status":"blocked","reason":reason,"next":recovery["next"]}]});
    let list_paths = paths.clone();
    let export_paths = paths.clone();
    let export_reason = reason.clone();
    let stop = shutdown.clone();
    let lifecycle_stop = shutdown.clone();
    let fallback_reason = reason.clone();
    let app = Router::new()
        .route("/v1/recovery/records", get(recovery_records::inspect).post(recovery_records::prepare).with_state(recovery_records::RecoveryRecords::new(paths.clone())))
        .route("/v1/health", get(move || { let value = health.clone(); async move { Json(value) } }))
        .route("/v1/recovery", get(move || { let value = recovery.clone(); async move { Json(value) } }))
        .route("/v1/preflight", get(move || { let value = check.clone(); async move { Json(value) } }))
        .route("/v1/diagnostics", get(move || { let paths = list_paths.clone(); async move {
            match runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, paths.diagnostics_dir.clone(), move || DiagnosticsStore::new(paths).list_with_warnings()).await {
                Ok((items,warnings)) => Json(serde_json::json!({"api_version":"v1","bundles":items,"warnings":warnings})).into_response(),
                Err(error) => data_error_response(error, "diagnostics_list_failed"),
            }
        }}).post(move |Json(command): Json<DiagnosticsCommand>| { let paths = export_paths.clone(); let reason = export_reason.clone(); async move {
            if command.action != DiagnosticsAction::Export || command.bundle.is_some() || command.file.is_some() {
                return api_error_response(StatusCode::CONFLICT, "agent_recovery_required", "Only diagnostic export is available in read-only recovery");
            }
            let result = runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, paths.diagnostics_dir.clone(), move || {
                let bundle = DiagnosticsStore::new(paths).collect_with_context(command.note, Some(serde_json::json!({"read_only":true,"initialization_error":reason,"build_id":option_env!("NEXUS_BUILD_ID")})))?;
                let path = std::path::PathBuf::from(&bundle.directory).join("export.json");
                #[cfg(not(test))]
                let reveal_error = reveal_diagnostic_export(&path).err().map(|e| e.to_string());
                #[cfg(test)]
                let reveal_error: Option<String> = None;
                Ok(serde_json::json!({"api_version":"v1","bundles":[bundle],"export_path":path,"reveal_error":reveal_error}))
            }).await;
            match result { Ok(value) => (StatusCode::CREATED, Json(value)).into_response(), Err(error) => data_error_response(error, "diagnostics_export_failed") }
        }}))
        .route("/v1/shutdown", post(move || { let stop = stop.clone(); async move { let _ = stop.send(true); (StatusCode::ACCEPTED, Json(LifecycleAccepted::accepted(LifecycleAction::Shutdown))) } }))
        .route("/v1/lifecycle", post(move |Json(command): Json<LifecycleCommand>| { let stop = lifecycle_stop.clone(); async move { let _ = stop.send(true); (StatusCode::ACCEPTED, Json(LifecycleAccepted::accepted(command.action))) } }))
        .fallback(move || { let reason = fallback_reason.clone(); async move {
            api_error_response(StatusCode::SERVICE_UNAVAILABLE, "agent_recovery_required", &format!("Read-only recovery: {reason}. Export diagnostics, repair the original files and restart Agent."))
        }})
        .layer(middleware::from_fn_with_state(ApiAuthorization::new(credential), enforce_api_authorization))
        .layer(middleware::from_fn(local_console_cors))
        .layer(middleware::from_fn(enforce_loopback_host));
    let listener = TcpListener::bind(config.bind_addr()).await?;
    paths.publish_agent_discovery(&AgentDiscoveryRecord {
        port: listener.local_addr()?.port(),
        instance_id: instance_id.to_owned(),
        data_root_id: root_id.to_owned(),
        pid: std::process::id(),
        updated_at_unix: unix_time_seconds(),
    })?;
    axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_shutdown(receiver))
        .await
}

fn build_router(state: AppState, credential: nexus_core::agent_auth::AgentCredential) -> Router {
    let receipts = request_receipts::Receipts::new(state.paths.root.clone());
    Router::new()
        .route(
            "/v1/recovery/records",
            get(recovery_records::inspect_normal).post(recovery_records::control_normal),
        )
        .route(
            "/v1/requests",
            get(request_receipts::list).with_state(receipts.clone()),
        )
        .route("/v1/health", get(health))
        .route("/v1/state", get(current_state))
        .route("/v1/harness", get(harness_status).post(harness_control))
        .route(
            "/v1/harness/startup",
            get(harness_startup_status).post(harness_startup_cancel),
        )
        .route("/v1/harness/discover", get(harness_discover))
        .route("/v1/harness/ui", get(harness_ui))
        .route("/v1/notifications", get(notifications::status).post(notifications::configure))
        .route("/v1/market", get(market::status).post(market::select))
        .route("/v1/desktop/profile", get(desktop_profile::status).post(desktop_profile::select))
        .route("/v1/profiles", get(profile_list).post(profile_control))
        .route("/v1/plugin-manager", post(profile_api::official_plugins))
        .route("/v1/profile-repair", post(profile_repair::handle))
        .route("/v1/canary", get(canary::status).post(canary::control))
        .route("/v1/recovery", get(recovery_status))
        .route("/v1/preflight", get(preflight::check))
        .route("/v1/dependencies", get(dependency_repair::status).post(dependency_repair::repair))
        .route(
            "/v1/checkpoints",
            get(checkpoint_list).post(checkpoint_control),
        )
        .route("/v1/releases", get(release_list).post(release_control))
        .route("/v1/releases/tags", get(release_tags))
        .route("/v1/runtime", get(runtime_status))
        .route("/v1/runtime/plan", post(runtime_plan))
        .route("/v1/updates", get(update_status).post(update_control))
        .route(
            "/v1/diagnostics",
            get(diagnostics_status).post(diagnostics_control),
        )
        .route("/v1/config", get(config_status).post(config_control))
        .route(
            "/v1/maintenance",
            get(maintenance_status).post(maintenance_dispatch),
        )
        .route("/v1/lifecycle", post(lifecycle))
        .route("/v1/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(
            receipts,
            request_receipts::enforce,
        ))
        .layer(middleware::from_fn_with_state(
            ApiAuthorization::new(credential),
            enforce_api_authorization,
        ))
        .layer(middleware::from_fn(local_console_cors))
        .layer(middleware::from_fn(enforce_loopback_host))
        .with_state(state)
}

/// Reject requests whose Host header names anything but the loopback
/// interface. The Agent binds to 127.0.0.1 only, but a DNS-rebinding page
/// can still reach that address from a browser; such requests arrive with
/// the attacker's hostname in Host and are refused here. A missing Host
/// header (HTTP/1.0-style clients and in-process test requests) is allowed.
async fn enforce_loopback_host(request: Request, next: Next) -> Response {
    let ok = match request.headers().get(axum::http::header::HOST) {
        None => true,
        Some(value) => value.to_str().map(host_header_is_loopback).unwrap_or(false),
    };
    if !ok {
        return (StatusCode::FORBIDDEN, "loopback host required").into_response();
    }
    next.run(request).await
}

fn host_header_is_loopback(host: &str) -> bool {
    let host_only = if let Some(bracketed) = host.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((inner, _)) => inner,
            None => bracketed,
        }
    } else if host.matches(':').count() > 1 {
        host
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(
        host_only.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

#[cfg(test)]
mod host_guard_tests {
    use super::host_header_is_loopback;

    #[test]
    fn loopback_hosts_accept_and_rebinds_reject() {
        assert!(host_header_is_loopback("127.0.0.1:3090"));
        assert!(host_header_is_loopback("127.0.0.1"));
        assert!(host_header_is_loopback("localhost"));
        assert!(host_header_is_loopback("localhost:8080"));
        assert!(host_header_is_loopback("LOCALHOST"));
        assert!(host_header_is_loopback("[::1]:3090"));
        assert!(host_header_is_loopback("::1"));
        assert!(!host_header_is_loopback("evil.example"));
        assert!(!host_header_is_loopback("evil.example:80"));
        assert!(!host_header_is_loopback("192.168.1.10:3090"));
        assert!(!host_header_is_loopback("[fe80::1]:3090"));
    }
}

fn ensure_checkpoint_process_quiescent(paths: &nexus_core::NexusPaths, journal: &CheckpointRestoreJournal) -> io::Result<()> {
    process_recovery::reconcile(&paths.run_dir.join("owned-processes"))?;
    if journal.process_owner_version > 1 { return Err(io::Error::other("Unsupported checkpoint process ownership protocol; journal retained")); }
    if journal.process_owner_version == 0 && journal.intent.snapshot.is_some() && journal.phase == CheckpointRestorePhase::Prepared {
        process_recovery::require_legacy_reboot(&paths.run_dir.join("checkpoint-restore.json"))?;
    }
    Ok(())
}

async fn recover_checkpoint_restore_startup(
    paths: &nexus_core::NexusPaths,
    journal_store: &CheckpointRestoreJournalStore,
    profiles: &ProfileStore,
    releases: &ReleaseStore,
    snapshots: &snapshots::SnapshotCoordinator,
) -> io::Result<()> {
    let Some(journal) = journal_store.load()? else {
        return Ok(());
    };
    ensure_checkpoint_process_quiescent(paths, &journal)?;
    if let Some(binding) = journal.intent.snapshot.as_ref() {
        let recovery = async {
            let lease = snapshots.acquire_bound(binding).await?;
            match journal.phase {
                CheckpointRestorePhase::Prepared => {
                    lease.rollback(binding.ticket.clone()).await?;
                    releases.restore_release_pointers(
                        journal.intent.previous_current_release.as_deref(),
                        journal.intent.previous_last_known_good.as_deref(),
                    )?;
                    profiles.write(&journal.intent.previous_profiles)?;
                    journal_store.clear(CheckpointRestorePhase::Prepared, &journal.intent)
                }
                CheckpointRestorePhase::Committed => {
                    validate_committed_checkpoint_restore(&journal, profiles, releases)?;
                    lease.commit(binding.ticket.clone()).await?;
                    journal_store.clear(CheckpointRestorePhase::Committed, &journal.intent)
                }
            }
        }
        .await;
        if let Err(error) = recovery {
            let diagnostic =
                format!("startup checkpoint restore recovery remains pending: {error}");
            let _ = journal_store.record_error(journal.phase, &journal.intent, diagnostic.clone());
            tracing::warn!(error = %diagnostic, "checkpoint restore startup recovery remains available for explicit retry or abort");
        }
        return Ok(());
    }
    match journal.phase {
        CheckpointRestorePhase::Prepared => {
            releases.restore_release_pointers(
                journal.intent.previous_current_release.as_deref(),
                journal.intent.previous_last_known_good.as_deref(),
            )?;
            profiles.write(&journal.intent.previous_profiles)?;
            journal_store.clear(CheckpointRestorePhase::Prepared, &journal.intent)
        }
        CheckpointRestorePhase::Committed => {
            validate_committed_checkpoint_restore(&journal, profiles, releases)?;
            journal_store.clear(CheckpointRestorePhase::Committed, &journal.intent)
        }
    }
}

fn validate_committed_checkpoint_restore(
    journal: &CheckpointRestoreJournal,
    profiles: &ProfileStore,
    releases: &ReleaseStore,
) -> io::Result<()> {
    let profiles = profiles.load()?;
    let (current_release, last_known_good) = releases.stored_release_pointers()?;
    if profiles != journal.intent.target_profiles
        || current_release != journal.intent.target_current_release
        || last_known_good != journal.intent.target_last_known_good
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "committed checkpoint restore does not match durable Harness selection",
        ));
    }
    Ok(())
}

async fn settle_checkpoint_restore(state: &AppState) -> Result<(), HarnessSupervisorError> {
    let Some(journal) = state
        .checkpoint_restores
        .load()
        .map_err(HarnessSupervisorError::Persistence)?
    else {
        return Ok(());
    };
    // Content restores wait for explicit Retry or Abort. Read-only routes stay
    // available and ordinary mutation guards reject the pending transaction.
    if journal.intent.snapshot.is_some() {
        return Ok(());
    }
    let (profiles, current_release, last_known_good) = match journal.phase {
        CheckpointRestorePhase::Prepared => (
            journal.intent.previous_profiles.clone(),
            journal.intent.previous_current_release.clone(),
            journal.intent.previous_last_known_good.clone(),
        ),
        CheckpointRestorePhase::Committed => {
            validate_committed_checkpoint_restore(&journal, &state.profiles, &state.releases)
                .map_err(HarnessSupervisorError::Persistence)?;
            (
                journal.intent.target_profiles.clone(),
                journal.intent.target_current_release.clone(),
                journal.intent.target_last_known_good.clone(),
            )
        }
    };
    if journal.phase == CheckpointRestorePhase::Prepared {
        state
            .releases
            .restore_release_pointers(current_release.as_deref(), last_known_good.as_deref())
            .map_err(HarnessSupervisorError::Persistence)?;
        state
            .profiles
            .write(&profiles)
            .map_err(HarnessSupervisorError::Persistence)?;
    }
    let active_profile = profiles.active_profile.clone();
    update_agent_state(state, |current| {
        current.set_profile(active_profile);
        current.set_release(current_release);
    })
    .await?;
    state
        .checkpoint_restores
        .clear(journal.phase, &journal.intent)
        .map_err(HarnessSupervisorError::Persistence)
}

fn acquire_runtime_lock(
    paths: &nexus_core::NexusPaths,
    instance_id: &str,
) -> io::Result<AgentRuntimeLock> {
    let path = paths.run_dir.join("agent.lock");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::AddrInUse,
            "another Nexus Agent already owns this data root",
        ),
        fs::TryLockError::Error(error) => error,
    })?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}", std::process::id())?;
    writeln!(file, "instance_id={instance_id}")?;
    file.sync_all()?;
    Ok(AgentRuntimeLock { _file: file })
}

fn proxy_identity_values_match(
    headers: &axum::http::HeaderMap,
    data_root_id: &str,
    instance_id: &str,
) -> bool {
    let expected_root = headers
        .get(PROXY_DATA_ROOT_HEADER)
        .and_then(|value| value.to_str().ok());
    let expected_instance = headers
        .get(PROXY_INSTANCE_HEADER)
        .and_then(|value| value.to_str().ok());
    match (expected_root, expected_instance) {
        (None, None) => false,
        (Some(root), Some(instance)) => root == data_root_id && instance == instance_id,
        _ => false,
    }
}

#[derive(Clone)]
struct ApiAuthorization {
    credential: nexus_core::agent_auth::AgentCredential,
    nonces: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    body_budget: Arc<tokio::sync::Semaphore>,
}
impl ApiAuthorization {
    fn new(credential: nexus_core::agent_auth::AgentCredential) -> Self {
        Self {
            credential,
            nonces: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            body_budget: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}

async fn enforce_api_authorization(
    State(state): State<ApiAuthorization>,
    request: Request,
    next: Next,
) -> Response {
    use nexus_core::agent_auth as auth;
    if request.method() == Method::GET
        && request
            .uri()
            .path_and_query()
            .is_some_and(|p| p.as_str() == "/v1/health")
    {
        return next.run(request).await;
    }
    if !proxy_identity_values_match(
        request.headers(),
        &state.credential.data_root_id,
        &state.credential.instance_id,
    ) {
        return StatusCode::CONFLICT.into_response();
    }
    let (nonce, time, signature) = {
        let header = |key| {
            request
                .headers()
                .get(key)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        (
            header(auth::NONCE_HEADER),
            header(auth::TIME_HEADER),
            header(auth::SIGNATURE_HEADER),
        )
    };
    let now = auth::unix_seconds();
    if request
        .headers()
        .get(auth::VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        != Some("2")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !auth::valid_hex(&nonce) || !auth::valid_hex(&signature) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(timestamp) = time
        .parse::<u64>()
        .ok()
        .filter(|timestamp| now.abs_diff(*timestamp) <= auth::MAX_CLOCK_SKEW_SECS)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(permit) = state.body_budget.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let (mut parts, body) = request.into_parts();
    let body = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        axum::body::to_bytes(body, 1024 * 1024 + auth::TAG_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    if !state.credential.verify_request(
        parts.method.as_str(),
        path,
        &nonce,
        &time,
        &body,
        &signature,
    ) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(body) = state
        .credential
        .open_request(parts.method.as_str(), path, &nonce, &time, &body)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    {
        let Ok(mut nonces) = state.nonces.lock() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        nonces.retain(|_, timestamp| now <= timestamp.saturating_add(auth::MAX_CLOCK_SKEW_SECS));
        if nonces.contains_key(&nonce) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if nonces.len() >= 16384 {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        nonces.insert(nonce.clone(), timestamp);
    }
    drop(permit);
    let response = next
        .run(Request::from_parts(parts, axum::body::Body::from(body)))
        .await;
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, nexus_launcher_core::MAX_RESPONSE_BODY_BYTES).await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(body) = state
        .credential
        .seal_response(&nonce, parts.status.as_u16(), &body)
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let signature = state
        .credential
        .response_signature(&nonce, parts.status.as_u16(), &body);
    parts.headers.insert(
        auth::RESPONSE_HEADER,
        signature.parse().expect("hex header"),
    );
    parts
        .headers
        .insert(auth::VERSION_HEADER, HeaderValue::from_static("2"));
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    if parts.status != StatusCode::NO_CONTENT {
        parts.headers.insert(
            axum::http::header::CONTENT_LENGTH,
            body.len().to_string().parse().expect("length header"),
        );
    }
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Response::from_parts(parts, axum::body::Body::from(body))
}

/// Allow only the configured local origin used by the dependency-free
/// WebShell. The Agent remains loopback-only; this middleware does not enable
/// remote origins or credentialed browser requests. Launcher sets
/// `NEXUS_CONSOLE_PORT` before spawning the Agent when a non-default Console
/// port is configured.
async fn local_console_cors(request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    if request.headers().contains_key(ORIGIN)
        && !origin.as_deref().is_some_and(is_allowed_console_origin)
    {
        return StatusCode::FORBIDDEN.into_response();
    }

    if request.method() == Method::OPTIONS && origin.is_some() {
        let allowed = origin.as_deref().is_some_and(is_allowed_console_origin)
            && is_allowed_preflight(&request);
        if !allowed {
            return StatusCode::FORBIDDEN.into_response();
        }
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_console_cors_headers(&mut response, origin.as_deref().expect("origin is present"));
        return response;
    }

    let mut response = next.run(request).await;
    if let Some(origin) = origin
        .as_deref()
        .filter(|origin| is_allowed_console_origin(origin))
    {
        add_console_cors_headers(&mut response, origin);
    }
    response
}

fn is_allowed_console_origin(origin: &str) -> bool {
    is_allowed_console_origin_for_port(origin, configured_console_port())
}

fn configured_console_port() -> u16 {
    env::var(CONSOLE_PORT_ENV)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_CONSOLE_PORT)
}

fn is_allowed_console_origin_for_port(origin: &str, expected_port: u16) -> bool {
    if expected_port == 0 {
        return false;
    }
    let Some(authority) = origin.strip_prefix("http://") else {
        return false;
    };
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) || authority.ends_with('.')
    {
        return false;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else {
            return false;
        };
        let Some(port) = suffix.strip_prefix(':') else {
            return false;
        };
        (host, port)
    } else {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return false;
        };
        (host, port)
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    port == expected_port && matches!(host, "127.0.0.1" | "::1")
        || port == expected_port && host.eq_ignore_ascii_case("localhost")
}

fn is_allowed_cors_method(method: &str) -> bool {
    matches!(method.trim().to_ascii_uppercase().as_str(), "GET" | "POST")
}

fn are_allowed_cors_headers(headers: &str) -> bool {
    headers
        .split(',')
        .map(str::trim)
        .filter(|header| !header.is_empty())
        .all(|header| {
            matches!(
                header.to_ascii_lowercase().as_str(),
                "content-type" | "accept"
            )
        })
}

fn is_allowed_preflight(request: &Request) -> bool {
    let Some(method) = request
        .headers()
        .get(ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    if !is_allowed_cors_method(method) {
        return false;
    }
    request
        .headers()
        .get(ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
        .map_or(true, are_allowed_cors_headers)
}

fn add_console_cors_headers(response: &mut Response, origin: &str) {
    let Ok(origin) = HeaderValue::from_str(origin) else {
        return;
    };
    let headers = response.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(CORS_ALLOWED_METHODS),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(CORS_ALLOWED_HEADERS),
    );
    headers.insert(
        ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(CORS_MAX_AGE_SECS),
    );
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let current = state.runtime.read().await;
    let mut response = if current.lifecycle == AgentLifecycleState::ShuttingDown {
        HealthResponse::shutting_down(state.data_root_id.clone(), state.instance_id.clone())
    } else {
        HealthResponse::healthy(state.data_root_id.clone(), state.instance_id.clone())
    };
    response.build_id = option_env!("NEXUS_BUILD_ID").map(str::to_owned);
    // This handler also serves the deliberately unauthenticated exact-match
    // GET /v1/health. binary_path stays there by design: the launcher's
    // freshness binding and standalone diagnostics run before any credential
    // exists, so the agent program identity must be readable pre-auth. The
    // endpoint is loopback-only with a Host check and returns no other
    // filesystem paths.
    if response.binary_path.is_none() {
        response.binary_path = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
    }
    Json(response)
}

fn try_read_lifecycle(
    state: &AppState,
) -> Result<supervisor::HarnessLifecycleGuard, axum::response::Response> {
    // Read routes may settle a checkpoint, so they must retain the same gate
    // as mutations. Busy is observable immediately instead of queueing behind
    // a long compatibility probe or restore owner.
    state.supervisor.try_acquire_lifecycle().ok_or_else(|| {
        api_error_response(
            StatusCode::CONFLICT,
            "lifecycle_busy",
            "NEXUS_LIFECYCLE_BUSY: Harness lifecycle operation is in progress; retry shortly",
        )
    })
}

async fn current_state(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    let _ = sync_harness_state(&state).await;
    let current = state.runtime.read().await;
    (
        StatusCode::OK,
        Json(StateResponse::from_state(current.as_payload())),
    )
        .into_response()
}

async fn harness_status(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    let harness = sync_harness_state(&state).await;
    (StatusCode::OK, Json(harness.into_response())).into_response()
}

const MAX_RECOVERY_LOG_BYTES: u64 = 16 * 1024;

fn redact_recovery_error(error: Option<&str>) -> Option<String> {
    error.map(|error| {
        let bytes = error.as_bytes();
        let bounded = &bytes[..bytes.len().min(4096)];
        String::from_utf8_lossy(&redact_diagnostics_payload(bounded).0).into_owned()
    })
}

fn recovery_log_payload(mut bytes: Vec<u8>, limit: usize, truncated: bool) -> (String, bool) {
    if truncated {
        if let Some(position) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=position);
        }
    }
    // The extra byte is only a truncation sentinel. Drop it before decoding or
    // redacting, then cap the final UTF-8 text after those transformations.
    bytes.truncate(limit);
    let raw = String::from_utf8_lossy(&bytes);
    let fatal = raw.lines().any(|line| {
        let line = line.trim_start().to_ascii_lowercase();
        line.starts_with("fatal:") || line.starts_with("[fatal]") || line.starts_with("fatal ")
    });
    // Windows child processes may mix legacy-encoded diagnostics with a UTF-8
    // Node stack. Preserve the readable stack, still applying line redaction.
    let mostly_text = !bytes.contains(&0)
        && bytes
            .iter()
            .filter(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
            .count()
            * 100
            >= bytes.len().saturating_mul(85);
    let redacted = if std::str::from_utf8(&bytes).is_err() && mostly_text {
        redact_diagnostics_payload(raw.as_bytes()).0
    } else {
        redact_diagnostics_payload(&bytes).0
    };
    let mut content = String::from_utf8_lossy(&redacted).into_owned();
    if content.len() > limit {
        let mut end = limit;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
    }
    (content, fatal)
}

async fn recovery_status(State(state): State<AppState>) -> axum::response::Response {
    let lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    let mut harness = sync_harness_state(&state).await.into_response().harness;
    let harness_stop_required = !state
        .supervisor
        .selection_change_is_quiescent(&lifecycle)
        .await;
    let pending_restore = match state.checkpoint_restores.load() {
        Ok(Some(journal)) => snapshots::restore_status(&journal, None),
        Ok(None) => None,
        Err(error) => return data_error_response(error, "checkpoint_restore_journal_failed"),
    };
    let mut log_tail = Vec::new();
    let mut diagnostic_errors = Vec::new();
    let paused = false;
    let pause_error = None;
    let mut fatal_prefix_observed = false;
    match HarnessLogSessionStore::new(state.paths.clone()).read() {
        Ok(Some(session)) => {
            for (stream, name) in [
                ("stdout", session.stdout_log_name),
                ("stderr", session.stderr_log_name),
            ] {
                match recovery_log_tail(&state.paths, &name) {
                    Ok((content, truncated, fatal)) => {
                        fatal_prefix_observed |= fatal;
                        if !content.is_empty() {
                            log_tail.push(RecoveryLogTail {
                                stream: stream.to_owned(),
                                content,
                                truncated,
                            });
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => diagnostic_errors.push(bounded_checkpoint_diagnostic(
                        &io::Error::other(format!("{stream} log: {error}")),
                    )),
                }
            }
        }
        Ok(None) => diagnostic_errors.push("Harness log session is not available".to_owned()),
        Err(error) => diagnostic_errors.push(bounded_checkpoint_diagnostic(&io::Error::other(
            format!("Harness log session is invalid: {error}"),
        ))),
    }
    let startup_error = redact_recovery_error(harness.error.as_deref());
    harness.error = startup_error.clone();
    (
        StatusCode::OK,
        Json(RecoveryStatusResponse {
            pause_error,
            paused,
            api_version: nexus_protocol::API_VERSION.to_owned(),
            manual_entry_available: true,
            harness_stop_required,
            harness,
            startup_error,
            fatal_prefix_observed,
            log_tail,
            diagnostic_errors,
            pending_restore,
        }),
    )
        .into_response()
}

fn recovery_log_tail(
    paths: &nexus_core::NexusPaths,
    name: &str,
) -> io::Result<(String, bool, bool)> {
    let path = paths.logs_dir.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Harness log is not an ordinary file",
        ));
    }
    let canonical_logs = fs::canonicalize(&paths.logs_dir)?;
    let canonical = fs::canonicalize(&path)?;
    if !canonical.starts_with(&canonical_logs) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Harness log resolves outside Nexus logs",
        ));
    }
    let limit = usize::try_from(MAX_RECOVERY_LOG_BYTES).unwrap_or(usize::MAX);
    let metadata_truncated = metadata.len() > MAX_RECOVERY_LOG_BYTES;
    let start = metadata.len().saturating_sub(MAX_RECOVERY_LOG_BYTES);
    let mut file = fs::File::open(canonical)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_RECOVERY_LOG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let truncated = metadata_truncated || bytes.len() > limit;
    let (content, fatal) = recovery_log_payload(bytes, limit, truncated);
    Ok((content, truncated, fatal))
}

/// Return the current validated Harness URL/token observation for UI clients.
///
/// The parser only reads the Agent-owned bounded log tail. The surrounding
/// checks bind that observation to the current running Harness generation and
/// durable log-session marker. A PID-less process may be a healthy descendant
/// recovered after an Agent/Harness replacement; it is eligible only when the
/// current session carries the fresh launch boundary established by recovery.
/// Lifecycle control remains PID-gated in the GUI, so this read-only handoff
/// cannot authorize an unowned stop or restart.
async fn harness_ui(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }

    fn unavailable_harness_ui_response(message: impl Into<String>) -> axum::response::Response {
        (StatusCode::OK, Json(unavailable_harness_ui_info(message))).into_response()
    }

    let mut observer = state.harness_logs.lock().await;
    let session = match HarnessLogSessionStore::new(state.paths.clone()).read() {
        Ok(Some(session)) => session,
        Ok(None) => {
            observer.invalidate();
            return unavailable_harness_ui_response(
                "Harness log session marker is not available; restart Harness to establish a safe token boundary",
            );
        }
        Err(error) => {
            observer.invalidate();
            return unavailable_harness_ui_response(format!(
                "Harness log session marker is invalid: {error}"
            ));
        }
    };
    let first = sync_harness_state(&state).await.into_response();
    if !harness_ui_process_is_presentable(&first, &session) {
        observer.invalidate();
        return unavailable_harness_ui_response(format!(
            "Harness is {:?}; a current authentication token is not available",
            first.harness.state
        ));
    }
    if !harness_observation_matches_session(&first, &session) {
        observer.invalidate();
        return unavailable_harness_ui_response(
            "Agent Harness observation does not match the durable log session marker",
        );
    }

    let second = sync_harness_state(&state).await.into_response();
    if first != second
        || !harness_ui_process_is_presentable(&second, &session)
        || !harness_observation_matches_session(&second, &session)
    {
        observer.invalidate();
        return unavailable_harness_ui_response(
            "Harness changed state while its token was being observed; refresh after it is running",
        );
    }

    let info = read_harness_ui_info_with_observer(&state.paths, &mut observer, Some(&session));
    let final_session = HarnessLogSessionStore::new(state.paths.clone()).read();
    let final_observation = sync_harness_state(&state).await.into_response();
    if !matches!(final_session, Ok(Some(ref current)) if current == &session)
        || final_observation != second
    {
        observer.invalidate();
        return unavailable_harness_ui_response(
            "Harness changed state while its token was being observed; refresh after it is running",
        );
    }

    let mut response = serde_json::to_value(info).expect("Harness UI response is serializable");
    response["browser_health"] = desktop_plugins::browser_health(&state.paths, &session.run_id);
    response["open_browser_after_ready"] = desktop_plugins::browser_open_deferred(&state.paths, &session.run_id).into();
    (StatusCode::OK, Json(response)).into_response()
}

/// A PID is required for lifecycle control, but a recovered descendant has no
/// PID that this Agent can safely claim. Such a process may still publish its
/// current URL/token after the durable log session was rotated, because the
/// parser will accept only bytes emitted after that boundary. Keeping this
/// check separate from the UI parser makes the read-only takeover contract
/// explicit and keeps endpoint regressions easy to test.
fn harness_ui_process_is_presentable(
    response: &HarnessResponse,
    session: &HarnessLogSession,
) -> bool {
    response.harness.state == nexus_protocol::HarnessState::Running
        && match response.harness.pid {
            Some(pid) => pid > 0,
            None => session.launch_pending,
        }
}

async fn harness_startup_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.supervisor.startup_status().await)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupCancel {
    action: String,
    operation_id: String,
}
async fn harness_startup_cancel(
    State(state): State<AppState>,
    Json(command): Json<StartupCancel>,
) -> axum::response::Response {
    if command.action != "cancel" || command.operation_id.len() > 160 {
        return data_error_response(
            io::Error::other("Invalid startup cancellation request"),
            "startup_cancel_invalid",
        );
    }
    if !state.supervisor.cancel_startup(&command.operation_id).await {
        return (StatusCode::CONFLICT,Json(serde_json::json!({"code":"startup_cancel_stale","message":"This startup is no longer cancellable; refresh its status. Use Stop after process creation."}))).into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(state.supervisor.startup_status().await),
    )
        .into_response()
}

async fn harness_control(
    state: State<AppState>,
    command: Json<HarnessCommand>,
) -> axum::response::Response {
    if command.0.action == HarnessAction::Status {
        return harness_control_inner(state, command).await;
    }
    // A disconnected caller must not release the lifecycle gate while the
    // blocking compatibility process still owns a probe and its files.
    match tokio::spawn(harness_control_inner(state, command)).await {
        Ok(response) => response,
        Err(error) => {
            data_error_response(io::Error::other(error.to_string()), "harness_owner_failed")
        }
    }
}

async fn harness_control_inner(
    State(state): State<AppState>,
    Json(command): Json<HarnessCommand>,
) -> axum::response::Response {
    if command.action != HarnessAction::Status {
        if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
            return response;
        }
    }
    let result = execute_harness_action(&state, command.action).await;

    match result {
        Ok(_) => {
            let harness = sync_harness_state(&state).await;
            (StatusCode::OK, Json(harness.into_response())).into_response()
        }
        Err(error) => {
            // Pre-spawn failures have no Harness log session yet. Preserve the
            // rejected action in Agent logs, which diagnostic bundles include.
            let (diagnostic, _) = redact_diagnostics_payload(error.to_string().as_bytes());
            tracing::warn!(action = ?command.action,
                error = %String::from_utf8_lossy(&diagnostic).trim(),
                "Harness control failed");
            let _ = sync_harness_state(&state).await;
            harness_error_response(error)
        }
    }
}

async fn harness_discover(State(state): State<AppState>) -> impl IntoResponse {
    let response: HarnessDiscoveryResponse = discover_harness_candidates_with_paths(&state.paths);
    (StatusCode::OK, Json(response))
}

async fn execute_harness_action(
    state: &AppState,
    action: HarnessAction,
) -> Result<HarnessRuntimeInfo, HarnessSupervisorError> {
    if action == HarnessAction::Stop { state.supervisor.finish_startup(false, true).await; }
    let lifecycle = if matches!(action, HarnessAction::Start | HarnessAction::Restart) {
        state
            .supervisor
            .try_acquire_lifecycle()
            .ok_or(HarnessSupervisorError::Busy)?
    } else {
        state.supervisor.acquire_lifecycle().await
    };
    if action == HarnessAction::Status {
        settle_checkpoint_restore(state).await?;
        return Ok(state.supervisor.status().await);
    }
    ensure_checkpoint_mutation_ready(state).await.map_err(|_| {
        HarnessSupervisorError::Persistence(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a content restore is pending; use checkpoint retry or checkpoint abort",
        ))
    })?;
    let profile = state
        .runtime
        .read()
        .await
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    if matches!(action, HarnessAction::Start | HarnessAction::Restart) {
        state.supervisor.begin_startup().await?;
        let result = async {
            let (report, prepared) = preflight::evaluate(state.clone()).await;
            if report["ready"] != true || report["paused"] == true {
                return Err(HarnessSupervisorError::Preflight(report));
            }
            let prepared = prepared.ok_or_else(|| {
                HarnessSupervisorError::Configuration(io::Error::other(
                    "Verified startup context is unavailable",
                ))
            })?;
            state
                .supervisor
                .start_prepared(
                    &profile,
                    &lifecycle,
                    prepared,
                    action == HarnessAction::Restart,
                )
                .await
        }
        .await;
        state
            .supervisor
            .finish_startup(
                result.is_ok(),
                matches!(result, Err(HarnessSupervisorError::Cancelled)),
            )
            .await;
        if result.is_ok() {
            let operation = state.supervisor.startup_status().await;
            tokio::spawn(observe_startup_failure(state.clone(), profile.clone(), operation));
        }
        return result;
    }
    match action {
        HarnessAction::Start => {
            state
                .supervisor
                .start_with_profile_locked(&profile, &lifecycle)
                .await
        }
        HarnessAction::Stop => {
            state.supervisor.finish_startup(false, true).await;
            state.supervisor.stop_locked(&lifecycle).await
        },
        HarnessAction::Restart => {
            state
                .supervisor
                .restart_with_profile_locked(&profile, &lifecycle)
                .await
        }
        HarnessAction::Status => Ok(state.supervisor.status().await),
    }
}

async fn observe_startup_failure(state: AppState, profile: String, operation: serde_json::Value) {
    observe_startup_failure_until(state, profile, operation, std::time::Duration::from_secs(660)).await;
}

async fn observe_startup_failure_until(state: AppState, profile: String, operation: serde_json::Value, timeout: std::time::Duration) {
    use nexus_protocol::HarnessState;
    let (_, _, initial_logs) = state.supervisor.status_observation().await;
    let revision = state.config.snapshot().ok().map(|snapshot| snapshot.revision);
    let initial_source = source_context::resolve(&state.paths, &state.releases).ok();
    let deadline = tokio::time::Instant::now() + timeout;
    let (generation, failed_run) = loop {
        let current_operation = state.supervisor.startup_status().await;
        if current_operation["operation_id"] != operation["operation_id"]
            || current_operation["phase"] == "cancelled" { return; }
        let (current, runtime, logs) = state.supervisor.status_observation().await;
        if logs.stderr_log_name != initial_logs.stderr_log_name || logs.stdout_log_name != initial_logs.stdout_log_name
            || logs.stderr_file_identity != initial_logs.stderr_file_identity { return; }
        if runtime.state == HarnessState::Stopped { return; }
        if runtime.state == HarnessState::Running {
            let health = desktop_plugins::browser_health(&state.paths, &logs.run_id);
            if health["state"] == "active" { return; }
            if health["state"] == "blocked" {
                // Keep the live process available for inspection; do not repair beneath it.
                schedule_crash_capture(&state, &HarnessSnapshot { generation: current, runtime, log_session: logs }).await;
                return;
            }
        }
        if runtime.state == HarnessState::Failed {
            let evidence = recovery_log_tail(&state.paths, &logs.stderr_log_name).ok().map(|value| value.0).unwrap_or_default();
            // A generic timeout or plugin exception is not authority to change dependencies.
            if !evidence.contains("ERR_MODULE_NOT_FOUND") && !evidence.contains("MODULE_NOT_FOUND") { return; }
            let failed_run = logs.run_id.clone();
            schedule_crash_capture(&state, &HarnessSnapshot { generation: current, runtime, log_session: logs }).await;
            break (current, failed_run);
        }
        if tokio::time::Instant::now() >= deadline {
            schedule_startup_capture(&state, &HarnessSnapshot { generation: current, runtime, log_session: logs },
                "startup verification timed out; readiness remains unverified", &state.timeout_capture_run).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    let Some(lifecycle) = state.supervisor.try_acquire_lifecycle() else { return; };
    if state.supervisor.startup_status().await["phase"] == "cancelled"
        || state.supervisor.startup_status().await["operation_id"] != operation["operation_id"]
        || state.config.snapshot().ok().map(|snapshot| snapshot.revision) != revision { return; }
    let (current, runtime, logs) = state.supervisor.status_observation().await;
    if current != generation || logs.run_id != failed_run || runtime.state != HarnessState::Failed { return; }
    if ensure_checkpoint_mutation_ready(&state).await.is_err()
        || ensure_harness_stopped(&state, &lifecycle).await.is_err() { return; }
    let Ok(update) = state.updater.try_acquire_gate() else { return; };
    if ensure_update_idle(&state).is_err() { return; }
    let Ok(snapshot) = state.snapshots.try_acquire_configuration() else { return; };
    let Ok(cold) = state.cold.try_acquire_maintenance() else { return; };
    let repair = (|| -> io::Result<serde_json::Value> {
        nexus_core::terminal_lease::ensure_all_idle(&state.paths)?;
        let source = source_context::resolve(&state.paths, &state.releases)?;
        if source.external { return Err(io::Error::other("External source is not automatically repaired")); }
        if !initial_source.as_ref().is_some_and(|initial| initial.release_id == source.release_id && initial.root == source.root) {
            return Err(io::Error::other("Selected release changed before startup recovery"));
        }
        let root = source.root.ok_or_else(|| io::Error::other("No selected release"))?;
        dependency_repair::ensure_startup(&root, &state.paths.root)
    })();
    drop((cold, snapshot, update));
    match repair {
        Ok(value) if value["phase"] == "repaired" => {
            tracing::info!(record = ?value["record"], "Startup failed; repaired local dependency links, retrying once");
            let (report, prepared) = preflight::evaluate(state.clone()).await;
            if report["ready"] != true || report["paused"] == true { return; }
            if state.supervisor.startup_status().await["phase"] == "cancelled"
                || state.supervisor.startup_status().await["operation_id"] != operation["operation_id"]
                || state.config.snapshot().ok().map(|snapshot| snapshot.revision) != revision
                || state.runtime.read().await.profile.as_deref().unwrap_or(DEFAULT_PROFILE) != profile { return; }
            if let Some(prepared) = prepared {
                // Deliberately do not schedule another recovery observer for the retry.
                let result = state.supervisor.start_prepared(&profile, &lifecycle, prepared, false).await;
                if let Err(error) = result { tracing::warn!(%error, "One-time startup retry failed"); }
            }
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "Failed startup requires manual dependency repair"),
    }
}

async fn sync_harness_state(state: &AppState) -> HarnessSnapshot {
    let snapshot = match update_agent_state(state, |_| {}).await {
        Ok((_, harness)) => harness,
        Err(error) => {
            tracing::warn!(error = %error, "failed to persist refreshed Harness state");
            let (generation, runtime, log_session) = state.supervisor.status_observation().await;
            HarnessSnapshot {
                generation,
                runtime,
                log_session,
            }
        }
    };
    schedule_healthy_snapshot(state, &snapshot);
    snapshot
}

#[derive(Default)]
struct CrashCapture {
    run_id: String,
    attempts: u8,
    completed: bool,
    in_flight: Arc<std::sync::atomic::AtomicBool>,
    next_attempt: Option<std::time::Instant>,
}
struct CrashAttempt(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CrashAttempt {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn start_crash_observer(
    state: AppState,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let (generation, runtime, log_session) = state.supervisor.status_observation().await;
            schedule_crash_capture(
                &state,
                &HarnessSnapshot {
                    generation,
                    runtime,
                    log_session,
                },
            )
            .await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                _ = shutdown.changed() => { if *shutdown.borrow() { return; } },
            }
        }
    })
}

/// One observer owns collection; a failure gets at most three attempts per
/// run with a five-second cooldown. Only a successful bundle is completed.
async fn schedule_crash_capture(state: &AppState, observation: &HarnessSnapshot) {
    if observation.runtime.state != nexus_protocol::HarnessState::Failed
        && !(observation.runtime.state == nexus_protocol::HarnessState::Running
            && desktop_plugins::browser_health(&state.paths, &observation.log_session.run_id)["state"] == "blocked")
        || observation.log_session.run_id.is_empty()
    {
        return;
    }
    schedule_startup_capture(state, observation, "startup failure", &state.crash_capture_run).await;
}

async fn schedule_startup_capture(state: &AppState, observation: &HarnessSnapshot, reason: &str, capture: &Arc<Mutex<CrashCapture>>) {
    if observation.log_session.run_id.is_empty() { return; }
    let run_id = observation.log_session.run_id.clone();
    {
        let mut captured = capture.lock().await;
        if captured.run_id != run_id {
            *captured = CrashCapture {
                run_id: run_id.clone(),
                ..Default::default()
            };
        }
        if captured.in_flight.load(Ordering::SeqCst)
            || captured.completed
            || captured.attempts >= 3
            || captured
                .next_attempt
                .is_some_and(|at| std::time::Instant::now() < at)
        {
            return;
        }
        captured.attempts += 1;
        captured.in_flight.store(true, Ordering::SeqCst);
        captured.next_attempt = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
    }
    let owned = state.clone();
    let attempt = CrashAttempt(capture.lock().await.in_flight.clone());
    let capture = capture.clone();
    let note = format!("auto: {reason}; evidence for run {run_id}");
    let result = runtime::RuntimeRequestContext::production()
        .run_blocking_io(
            runtime::BlockingStage::Diagnostics,
            state.paths.diagnostics_dir.clone(),
            move || {
                let _attempt = attempt; // Also releases admission if queued work is dropped before execution.
                let result = collect_current_diagnostics(&owned, Some(note));
                let mut captured = capture.blocking_lock();
                if captured.run_id == run_id {
                    captured.completed = result.is_ok();
                    captured.next_attempt =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                }
                result
            },
        )
        .await;
    match result {
        Ok(bundle) => tracing::info!(bundle = %bundle.id, "captured automatic crash evidence"),
        Err(error) => {
            let (safe, _) = redact_diagnostics_payload(error.to_string().as_bytes());
            tracing::warn!(error = %String::from_utf8_lossy(&safe), "Automatic crash evidence collection failed; bounded retry remains available");
        }
    }
}

fn schedule_healthy_snapshot(state: &AppState, observation: &HarnessSnapshot) {
    if observation.runtime.state != nexus_protocol::HarnessState::Running
        || observation.log_session.run_id.is_empty()
        || observation.log_session.healthy_snapshot_attempted
    {
        return;
    }
    let state = state.clone();
    let run_id = observation.log_session.run_id.clone();
    let generation = observation.log_session.generation;
    tokio::spawn(async move {
        let profile = match state.runtime.read().await.profile.clone() {
            Some(profile) => profile,
            None => return,
        };
        let dsh_home = match state.snapshots.configured_dsh_home() {
            Ok(home) => home.clone(),
            Err(_) => return,
        };
        let profile_for_check = profile.clone();
        let initialized = tokio::task::spawn_blocking(move || {
            dsh::profile_is_initialized(&dsh_home, &profile_for_check)
        })
        .await;
        if !matches!(initialized, Ok(Ok(true))) {
            return;
        }
        match state
            .supervisor
            .claim_healthy_snapshot_attempt(&run_id, generation)
            .await
        {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                let result = Err(io::Error::other(format!(
                    "failed to claim healthy snapshot for the current Harness run: {error}"
                )));
                state.snapshots.record_healthy_result(&result);
                return;
            }
        }
        let release = state.runtime.read().await.release.clone();
        let version = selected_dsh_version(&state.releases, release.as_deref());
        let result = state.snapshots.capture_healthy(profile, version).await;
        if let Err(error) = &result {
            tracing::warn!(error = %error, "healthy Harness snapshot attempt failed");
        }
        state.snapshots.record_healthy_result(&result);
    });
}

async fn update_agent_state<F>(
    state: &AppState,
    mutation: F,
) -> Result<(AgentState, HarnessSnapshot), HarnessSupervisorError>
where
    F: FnOnce(&mut AgentState),
{
    update_agent_state_inner(state, mutation, false).await
}

async fn update_agent_state_inner<F>(
    state: &AppState,
    mutation: F,
    inject_checkpoint_persist_failure: bool,
) -> Result<(AgentState, HarnessSnapshot), HarnessSupervisorError>
where
    F: FnOnce(&mut AgentState),
{
    #[cfg(not(test))]
    let _ = inject_checkpoint_persist_failure;
    // This mutex defines the Agent-side publication revision domain. Every
    // post-startup mutation is serialized here and receives a monotonically
    // increasing revision before the generation-bound supervisor write.
    let _harness_sync = state.harness_sync.lock().await;
    let (generation, runtime, log_session) = state.supervisor.status_observation().await;
    let mut snapshot = HarnessSnapshot {
        generation,
        runtime,
        log_session,
    };
    let mut mutation = Some(mutation);
    for _ in 0..3 {
        let (current, revision) = {
            let mut current = state.runtime.write().await;
            if let Some(mutation) = mutation.take() {
                mutation(&mut current);
            }
            if current.harness != snapshot.runtime.state {
                current.set_harness(snapshot.runtime.state);
            }
            let revision = state.agent_revision.fetch_add(1, Ordering::SeqCst) + 1;
            (current.clone(), revision)
        };
        #[cfg(test)]
        if inject_checkpoint_persist_failure
            && state
                .agent_persist_failure
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(HarnessSupervisorError::Persistence(io::Error::other(
                "injected Agent snapshot persistence failure after runtime mutation",
            )));
        }
        match state
            .supervisor
            .persist_agent_snapshot(snapshot.generation, revision, &current, &snapshot.runtime)
            .await
            .map_err(HarnessSupervisorError::Persistence)?
        {
            true => return Ok((current, snapshot)),
            false => {
                let (generation, harness, log_session) =
                    state.supervisor.status_observation().await;
                snapshot = HarnessSnapshot {
                    generation,
                    runtime: harness,
                    log_session,
                };
            }
        }
    }
    Err(HarnessSupervisorError::Persistence(io::Error::new(
        io::ErrorKind::WouldBlock,
        "Agent or Harness state changed while runtime metadata was being published",
    )))
}

fn release_list_response(catalog: ReleaseCatalog) -> ReleaseListResponse {
    let mut response = ReleaseListResponse::new(
        catalog.current_release,
        catalog.last_known_good,
        catalog.releases,
    );
    response.unavailable_selections = catalog.unavailable_selections;
    response
}

async fn release_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    match state.releases.load() {
        Ok(catalog) => (StatusCode::OK, Json(release_list_response(catalog))).into_response(),
        Err(error) => data_error_response(error, "release_catalog_unavailable"),
    }
}

async fn runtime_status(State(state): State<AppState>) -> axum::response::Response {
    let runtime_request = runtime::RuntimeRequestContext::production();
    runtime_status_for_parts(state.paths.clone(), state.config.clone(), runtime_request).await
}

async fn runtime_status_for_parts(
    paths: nexus_core::NexusPaths,
    config_store: ConfigStore,
    request: runtime::RuntimeRequestContext,
) -> axum::response::Response {
    let config_path = config_store.paths().config_file.clone();
    let owned_config_store = config_store.clone();
    let config = match request
        .run_blocking_io(runtime::BlockingStage::ConfigFile, config_path, move || {
            owned_config_store.load()
        })
        .await
    {
        Ok(config) => config,
        Err(error) => return data_error_response(error, "config_unavailable"),
    };
    let response =
        runtime::observe_runtime_selection_until(&paths, config.runtime.as_ref(), &request).await;
    (StatusCode::OK, Json(response)).into_response()
}

async fn runtime_plan(
    State(state): State<AppState>,
    Json(request): Json<RuntimePlanRequest>,
) -> axum::response::Response {
    let runtime_request = runtime::RuntimeRequestContext::production();
    match runtime_plan::plan_registered_release(
        &state.releases,
        &state.config,
        request,
        &runtime_request,
    )
    .await
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "runtime_plan_failed"),
    }
}

#[cfg(test)]
mod runtime_route_tests {
    use super::runtime_status_for_paths;
    use axum::{body::to_bytes, http::StatusCode};
    use nexus_core::NexusPaths;

    #[tokio::test]
    async fn runtime_route_returns_versioned_tool_list() {
        let root =
            std::env::temp_dir().join(format!("nexus-agent-runtime-route-{}", std::process::id()));
        let paths = NexusPaths::from_root(root.clone());
        std::fs::create_dir_all(paths.root.join("runtimes")).expect("runtime root creates");

        let response = runtime_status_for_paths(&paths).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("runtime response body reads");
        let body = std::str::from_utf8(&body).expect("runtime response is UTF-8");
        assert!(body.contains("\"api_version\":\"v1\""));
        assert!(body.contains("\"name\":\"git\""));
        assert!(body.contains("\"name\":\"node\""));
        assert!(body.contains("\"name\":\"pnpm\""));
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
async fn runtime_status_for_paths(paths: &nexus_core::NexusPaths) -> axum::response::Response {
    runtime_status_for_parts(
        paths.clone(),
        ConfigStore::new(paths.clone()),
        runtime::RuntimeRequestContext::production(),
    )
    .await
}

async fn release_tags(State(state): State<AppState>) -> axum::response::Response {
    let spec = match load_update_spec(&state.paths) {
        Ok(Some(spec)) => spec,
        Ok(None) => UpdateSpec {
            source: cold::ColdCoordinator::approved_upstream().to_owned(),
            ref_name: "main".to_owned(),
            git_program: std::path::PathBuf::from("git"),
            build_program: None,
            build_args: Vec::new(),
            verify_program: None,
            verify_args: Vec::new(),
            timeout_secs: Some(120),
        },
        Err(error) => return data_error_response(error, "update_spec_unavailable"),
    };
    let command_timeout = std::time::Duration::from_secs(spec.timeout_secs.unwrap_or(120));
    let runtime = match cold::resolved_runtime_config(&state).await {
        Ok(runtime) => runtime,
        Err(error) => return data_error_response(error, "runtime_selection_failed"),
    };
    let external = match git_worker::selected_external(&runtime) {
        Ok(external) => external,
        Err(error) => return data_error_response(error, "runtime_selection_failed"),
    };
    match git_worker::list_tags(
        &spec.source,
        &state.paths.run_dir,
        command_timeout,
        external,
    )
    .await
    {
        Ok(tags) => (
            StatusCode::OK,
            Json(TagListResponse::new(spec.source.clone(), tags)),
        )
            .into_response(),
        Err(error) => data_error_response(io::Error::other(error.to_string()), "tag_list_failed"),
    }
}

async fn release_control(
    state: State<AppState>,
    command: Json<ReleaseCommand>,
) -> axum::response::Response {
    // Keep ownership of a potentially long compatibility check when a caller
    // disconnects. Publication and process cleanup still settle under the locks.
    if matches!(
        command.0.action,
        ReleaseAction::Promote | ReleaseAction::Rollback
    ) {
        match tokio::spawn(release_control_inner(state, command)).await {
            Ok(response) => response,
            Err(error) => {
                data_error_response(io::Error::other(error.to_string()), "release_owner_failed")
            }
        }
    } else {
        release_control_inner(state, command).await
    }
}

async fn release_control_inner(
    State(state): State<AppState>,
    Json(command): Json<ReleaseCommand>,
) -> axum::response::Response {
    match command.action {
        ReleaseAction::List | ReleaseAction::Current => release_list(State(state)).await,
        ReleaseAction::Register => {
            let Some(id) = command.id.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "release id is required"),
                    "release_invalid",
                );
            };
            let Some(version) = command.version.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "release version is required"),
                    "release_invalid",
                );
            };
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            match state
                .releases
                .register(id, version, command.source, command.note)
            {
                Ok(catalog) => {
                    (StatusCode::CREATED, Json(release_list_response(catalog))).into_response()
                }
                Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                    data_error_response(error, "release_slots_full")
                }
                Err(error) => data_error_response(error, "release_register_failed"),
            }
        }
        ReleaseAction::Promote => {
            let Some(id) = command.id.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "release id is required"),
                    "release_invalid",
                );
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_harness_selection_quiescent(
                &state,
                &lifecycle,
                "release_change_conflict",
                "cannot promote a release until Harness is positively stopped and unowned",
            )
            .await
            {
                return response;
            }
            let external = match state.config.load() {
                Ok(config) => config.external_harness.is_some(),
                Err(error) => return data_error_response(error, "config_read_failed"),
            };
            if command.inspect_only {
                return match if external {
                    Ok(None)
                } else {
                    state.releases.promotion_risk_confirmation(id)
                } {
                    Ok(confirmation) => {
                        Json(serde_json::json!({"rollback_confirmation": confirmation}))
                            .into_response()
                    }
                    Err(error) => data_error_response(error, "release_promote_failed"),
                };
            }
            match cold::initialize_selected_release(&state, id, command.rollback_confirmation.as_deref()).await {
                Ok(Some(catalog)) => return apply_release_catalog(&state, catalog).await,
                Ok(None) => {},
                Err(error) => return data_error_response(error, "release_promote_failed"),
            }
            if let Err(error) = compatibility::for_release(
                &state,
                id,
                false,
                &nexus_core::CancellationToken::default(),
            )
            .await
            {
                return data_error_response(error, "profile_compatibility_failed");
            }
            let catalog = match if external {
                state.releases.promote(id)
            } else {
                state
                    .releases
                    .promote_confirmed(id, command.rollback_confirmation.as_deref())
            } {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "release_promote_failed"),
            };
            apply_release_catalog(&state, catalog).await
        }
        ReleaseAction::Remove => {
            let Some(id) = command.id.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "release id is required"),
                    "release_invalid",
                );
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_harness_selection_quiescent(
                &state,
                &lifecycle,
                "release_change_conflict",
                "cannot remove a release until Harness is positively stopped and unowned",
            )
            .await
            {
                return response;
            }
            let id = id.to_owned();
            let releases = state.releases.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _owners = (lifecycle, _update_gate);
                #[cfg(test)]
                wait_release_remove_test_gate(&id);
                releases.remove(&id)
            })
            .await
            .unwrap_or_else(|error| {
                Err(io::Error::other(format!(
                    "Release cleanup owner failed: {error}"
                )))
            });
            let catalog = match result {
                Ok(catalog) => catalog,
                Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                    return data_error_response(error, "release_slot_protected");
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return data_error_response(error, "release_not_found");
                }
                Err(error) => return data_error_response(error, "release_remove_failed"),
            };
            apply_release_catalog(&state, catalog).await
        }
        ReleaseAction::Rollback => {
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_harness_selection_quiescent(
                &state,
                &lifecycle,
                "release_change_conflict",
                "cannot roll back a release until Harness is positively stopped and unowned",
            )
            .await
            {
                return response;
            }
            let previous = match state.releases.load() {
                Ok(catalog) => catalog.last_known_good,
                Err(error) => return data_error_response(error, "release_rollback_failed"),
            };
            if let Some(id) = previous.as_deref() {
                if let Err(error) = compatibility::for_release(
                    &state,
                    id,
                    false,
                    &nexus_core::CancellationToken::default(),
                )
                .await
                {
                    return data_error_response(error, "profile_compatibility_failed");
                }
            }
            let catalog = match state.releases.rollback() {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "release_rollback_failed"),
            };
            apply_release_catalog(&state, catalog).await
        }
    }
}

async fn apply_release_catalog(
    state: &AppState,
    catalog: ReleaseCatalog,
) -> axum::response::Response {
    if let Err(error) = persist_release_catalog_state(state, &catalog, false).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "release_state_persistence_failed",
        );
    }
    (StatusCode::OK, Json(release_list_response(catalog))).into_response()
}

async fn persist_release_catalog_state(
    state: &AppState,
    catalog: &ReleaseCatalog,
    inject_agent_persist_failure: bool,
) -> Result<(), HarnessSupervisorError> {
    let current_release = catalog.current_release.clone();
    update_agent_state_inner(
        state,
        |current| current.set_release(current_release),
        inject_agent_persist_failure,
    )
    .await
    .map(|_| ())
}

async fn update_status(State(state): State<AppState>) -> axum::response::Response {
    let update = match state.updater.status() {
        Ok(update) => update,
        Err(error) => return update_error_response(error),
    };
    let release = update
        .release_id
        .as_deref()
        .and_then(|id| state.releases.get(id).ok());
    let operation = match state.cold.load() {
        Ok(operation) => operation,
        Err(error) => return data_error_response(error, "cold_operation_unavailable"),
    };
    let mut response = UpdateResponse::new(update, release);
    if let Some(operation) = operation {
        response.offline_progress = cold::offline::progress(&operation);
        response = response.with_operation(operation);
    }
    response.configuration_recovery = match state.config.pending_configuration_status() {
        Ok(status) => status,
        Err(error) => return data_error_response(error, "config_recovery_unavailable"),
    };
    response.publication_recovery = match state.cold.publication_status() {
        Ok(status) => status,
        Err(error) => return data_error_response(error, "publication_recovery_unavailable"),
    };
    response.install_operation = match state.updater.install_operation() {
        Ok(operation) => operation,
        Err(error) => return data_error_response(error, "install_operation_unavailable"),
    };
    (StatusCode::OK, Json(response)).into_response()
}

async fn update_control(
    State(state): State<AppState>,
    Json(command): Json<UpdateCommand>,
) -> axum::response::Response {
    match command.action {
        UpdateAction::Status => update_status(State(state)).await,
        UpdateAction::OfflineInspect => {
            let Some(archive) = command.archive_path.as_deref() else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "offline_path_required",
                    "archive_path is required",
                );
            };
            match cold::offline::inspect(&state, archive).await {
                Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                Err(error) => data_error_response(error, "offline_preview_failed"),
            }
        }
        UpdateAction::Install => {
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            match state
                .updater
                .install(command.release_id, command.version)
                .await
            {
                Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
                Err(error) => update_error_response(error),
            }
        }
        UpdateAction::OfflineImport | UpdateAction::OfflineExport => {
            let Some(archive) = command.archive_path.as_deref() else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "offline_path_required",
                    "archive_path is required",
                );
            };
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            match cold::offline::begin(
                &state,
                command.action,
                archive,
                command.release_id.as_deref(),
                command.offline_contents,
            )
            .await
            {
                Ok(operation) => {
                    tokio::spawn(cold::prepare(state.clone(), operation.operation_id.clone()));
                    (
                        StatusCode::ACCEPTED,
                        Json(
                            UpdateResponse::new(
                                state
                                    .updater
                                    .status()
                                    .unwrap_or_else(|_| nexus_protocol::UpdateRuntimeInfo::idle()),
                                None,
                            )
                            .with_operation(operation),
                        ),
                    )
                        .into_response()
                }
                Err(error) => data_error_response(error, "offline_operation_rejected"),
            }
        }
        UpdateAction::Switch => {
            let Some(tag) = command.tag.clone() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "tag is required"),
                    "update_tag_required",
                );
            };
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let source = command.source.unwrap_or(RuntimeSource::Official);
            let mode = command.mode.unwrap_or(RuntimeInstallMode::Portable);
            match state.cold.begin(tag, source, mode).await {
                Ok(operation) => {
                    let owner_state = state.clone();
                    let operation_id = operation.operation_id.clone();
                    tokio::spawn(async move {
                        cold::prepare(owner_state, operation_id).await;
                    });
                    (
                        StatusCode::ACCEPTED,
                        Json(
                            UpdateResponse::new(
                                state
                                    .updater
                                    .status()
                                    .unwrap_or_else(|_| nexus_protocol::UpdateRuntimeInfo::idle()),
                                None,
                            )
                            .with_operation(operation),
                        ),
                    )
                        .into_response()
                }
                Err(error) => data_error_response(error, "cold_operation_rejected"),
            }
        }
        UpdateAction::PublicationRetry
        | UpdateAction::PublicationAbandon
        | UpdateAction::ConfigurationRetry
        | UpdateAction::ConfigurationAbandon => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_operation_id_required",
                    "operation_id is required",
                );
            };
            let configuration_only = matches!(
                command.action,
                UpdateAction::ConfigurationRetry | UpdateAction::ConfigurationAbandon
            );
            if configuration_only && state.cold.publication_pending() {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "publication_recovery_pending",
                    "Use publication recovery to resolve both pending operations together",
                );
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            match state.checkpoint_restores.load() {
                Ok(None) => {}
                Ok(Some(_)) => {
                    return api_error_response(
                        StatusCode::CONFLICT,
                        "checkpoint_recovery_pending",
                        "Finish checkpoint recovery first",
                    )
                }
                Err(error) => return data_error_response(error, "checkpoint_recovery_unavailable"),
            }
            let update = match state.updater.try_acquire_gate() {
                Ok(value) => value,
                Err(error) => return update_error_response(error),
            };
            // A persisted Running update may be this interrupted cold publication.
            // The exclusive owner gate plus ordinary-install record proves quiescence.
            match state.updater.install_operation() {
                Ok(Some(operation))
                    if !operation.owner_quiescent
                        || operation.cleanup_pending
                        || operation.phase == "installing" =>
                {
                    return api_error_response(
                        StatusCode::CONFLICT,
                        "install_recovery_pending",
                        "Finish ordinary installation recovery first",
                    )
                }
                Ok(_) => {}
                Err(error) => return data_error_response(error, "install_recovery_unavailable"),
            }
            let snapshots = match state.snapshots.try_acquire_configuration() {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "publication_recovery_conflict"),
            };
            let cold = match state.cold.acquire_recovery().await {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "publication_recovery_conflict"),
            };
            let coordinator = state.cold.clone();
            let config_store = state.config.clone();
            let preserve = matches!(
                command.action,
                UpdateAction::PublicationAbandon | UpdateAction::ConfigurationAbandon
            );
            let result = tokio::task::spawn_blocking(move || {
                let _guards = (lifecycle, update, snapshots, cold);
                if configuration_only {
                    if preserve {
                        config_store.preserve_current_configuration(&operation_id)
                    } else {
                        config_store.retry_configuration(&operation_id)
                    }
                } else {
                    coordinator.recover_explicit(&operation_id, preserve)
                }
            })
            .await
            .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
            if let Err(error) = result {
                return data_error_response(error, "publication_recovery_failed");
            }
            let catalog = match state.releases.load() {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "release_catalog_unavailable"),
            };
            if let Err(error) = persist_release_catalog_state(&state, &catalog, false).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "release_state_persistence_failed",
                );
            }
            update_status(State(state)).await
        }
        UpdateAction::ClearFinished => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_operation_id_required",
                    "operation_id is required",
                );
            };
            match state.cold.clear_finished(&operation_id).await {
                Ok(()) => update_status(State(state)).await,
                Err(error) => data_error_response(error, "cold_clear_failed"),
            }
        }
        UpdateAction::Confirm => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_operation_id_required",
                    "operation_id is required",
                );
            };
            let Some(confirmation) = command.confirmation else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_confirmation_required",
                    "confirmation is required",
                );
            };
            // Supply confirmation is retired: installs run without a pause,
            // so the endpoint rejects every confirmation.
            let _ = (operation_id, confirmation);
            api_error_response(
                StatusCode::CONFLICT,
                "cold_confirmation_stale",
                "cold supply confirmation is retired; runtime provisioning by download was removed",
            )
        }
        UpdateAction::Cancel => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_operation_id_required",
                    "operation_id is required",
                );
            };
            if operation_id.starts_with("install-") {
                let updater = state.updater.clone();
                let result =
                    tokio::spawn(async move { updater.cancel_install(&operation_id).await })
                        .await
                        .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
                return match result {
                    Ok(operation) => {
                        let mut response = UpdateResponse::new(
                            state
                                .updater
                                .status()
                                .unwrap_or_else(|_| nexus_protocol::UpdateRuntimeInfo::idle()),
                            None,
                        );
                        response.install_operation = Some(operation);
                        (StatusCode::OK, Json(response)).into_response()
                    }
                    Err(error) => data_error_response(error, "install_cancel_failed"),
                };
            }
            match state.cold.cancel(&operation_id).await {
                Ok(operation) => (
                    StatusCode::OK,
                    Json(
                        UpdateResponse::new(
                            state
                                .updater
                                .status()
                                .unwrap_or_else(|_| nexus_protocol::UpdateRuntimeInfo::idle()),
                            None,
                        )
                        .with_operation(operation),
                    ),
                )
                    .into_response(),
                Err(error) => data_error_response(error, "cold_cancel_failed"),
            }
        }
    }
}

#[allow(dead_code)]
async fn complete_release_switch(
    state: AppState,
    tag: String,
    update_gate: tokio::sync::OwnedMutexGuard<()>,
) -> axum::response::Response {
    let response = match state.updater.switch_tag_owned(tag, &update_gate).await {
        Ok(response) => response,
        Err(error) => return update_error_response(error),
    };
    let catalog = match state.releases.load() {
        Ok(catalog) => catalog,
        Err(error) => {
            let error = UpdateExecutorError::Persistence(io::Error::new(
                error.kind(),
                format!("failed to load promoted release catalog: {error}"),
            ));
            let error = state.updater.record_switch_failure(
                response.update.release_id.clone(),
                response.update.started_at_unix,
                error,
                &update_gate,
            );
            return update_error_response(error);
        }
    };
    if let Err(error) = persist_release_catalog_state(&state, &catalog, true).await {
        let error = UpdateExecutorError::Persistence(io::Error::other(format!(
            "failed to persist Agent current release after promotion: {error}"
        )));
        let error = state.updater.record_switch_failure(
            response.update.release_id.clone(),
            response.update.started_at_unix,
            error,
            &update_gate,
        );
        return update_error_response(error);
    }
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn diagnostics_status(State(state): State<AppState>) -> axum::response::Response {
    diagnostics_status_with_budget(state, runtime::RuntimeRequestContext::production()).await
}
async fn diagnostics_status_with_budget(
    state: AppState,
    request: runtime::RuntimeRequestContext,
) -> axum::response::Response {
    match request
        .run_blocking_io(
            runtime::BlockingStage::Diagnostics,
            state.paths.diagnostics_dir.clone(),
            move || state.diagnostics.list_with_warnings(),
        )
        .await
    {
        Ok((bundles, warnings)) => {
            let mut response =
                serde_json::to_value(DiagnosticsResponse::new(bundles)).unwrap_or_default();
            response["log_retention"] = log_retention::status();
            response["warnings"] = serde_json::json!(warnings);
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(error) => data_error_response(error, "diagnostics_list_failed"),
    }
}

fn collect_current_diagnostics(
    state: &AppState,
    note: Option<String>,
) -> io::Result<nexus_protocol::DiagnosticsBundle> {
    let mut context = match state
        .config
        .snapshot()
        .and_then(|config| config_response_for_paths(&state.paths, config))
    {
        Ok(config) => serde_json::to_value(config)?,
        Err(error) => serde_json::json!({"config_error": error.to_string()}),
    };
    if let Some(prompt) = context.pointer_mut("/harness_preferences/system_prompt") {
        *prompt = serde_json::json!("[REDACTED]");
    }
    let context = serde_json::json!({"configuration": context,
        "runtime_patches": runtime_patches::diagnostic_summary(&state.paths),
        "harness_home": crate::dsh::resolve_dsh_home_for_paths(&state.paths).ok(),
        "operation_context": request_receipts::observed_context(&state.paths),
        "launch": request_receipts::launch_context(&state.paths),
        "request_history": request_receipts::diagnostic_summary(&state.paths),
        "log_retention": log_retention::status(),
        "observed_at_unix": nexus_core::unix_time_seconds()});
    state.diagnostics.collect_with_context(note, Some(context))
}

async fn diagnostics_control(
    State(state): State<AppState>,
    Json(command): Json<DiagnosticsCommand>,
) -> axum::response::Response {
    match command.action {
        DiagnosticsAction::Status => diagnostics_status(State(state)).await,
        DiagnosticsAction::OpenPath => {
            let result = runtime::RuntimeRequestContext::production()
                .run_blocking_io(
                    runtime::BlockingStage::Diagnostics,
                    state.paths.diagnostics_dir.clone(),
                    move || {
                        command
                            .bundle
                            .as_deref()
                            .ok_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidInput, "bundle is required")
                            })
                            .and_then(|id| state.diagnostics.open_path(id, command.file.as_deref()))
                            .and_then(|path| {
                                #[cfg(windows)]
                                let mut opener = {
                                    use std::os::windows::process::CommandExt;
                                    // Always view collected text in an editor, never execute a log by extension.
                                    let mut opener =
                                        std::process::Command::new(if command.file.is_some() {
                                            "notepad.exe"
                                        } else {
                                            "explorer.exe"
                                        });
                                    opener.creation_flags(0x0800_0000);
                                    opener
                                };
                                #[cfg(target_os = "macos")]
                                let mut opener = std::process::Command::new("open");
                                #[cfg(all(unix, not(target_os = "macos")))]
                                let mut opener = std::process::Command::new("xdg-open");
                                opener.arg(&path).spawn()?;
                                Ok(path)
                            })
                    },
                )
                .await;
            match result {
                Ok(path) => (
                    StatusCode::OK,
                    Json(serde_json::json!({ "api_version": "v1", "path": path })),
                )
                    .into_response(),
                Err(error) => data_error_response(error, "diagnostics_open_failed"),
            }
        }
        DiagnosticsAction::Export => {
            if command.bundle.is_some() || command.file.is_some() {
                return data_error_response(io::Error::new(io::ErrorKind::InvalidInput, "Export collects the current diagnostic context; bundle and file are not accepted"), "diagnostics_invalid");
            }
            let collected = runtime::RuntimeRequestContext::production()
                .run_blocking_io(
                    runtime::BlockingStage::Diagnostics,
                    state.paths.diagnostics_dir.clone(),
                    move || {
                        let bundle = collect_current_diagnostics(&state, command.note)?;
                        let path = std::path::PathBuf::from(&bundle.directory).join("export.json");
                        let reveal_error = reveal_diagnostic_export(&path)
                            .err()
                            .map(|error| error.to_string());
                        Ok((bundle, path, reveal_error))
                    },
                )
                .await;
            match collected {
                Ok((bundle, path, reveal_error)) => {
                    (StatusCode::CREATED, Json(serde_json::json!({"api_version":"v1", "bundles":[bundle], "export_path":path, "reveal_error":reveal_error}))).into_response()
                }
                Err(error) => data_error_response(error, "diagnostics_export_failed"),
            }
        }
        DiagnosticsAction::Collect => {
            // Diagnostics must remain available while a restore is pending.
            // Collection owns only its diagnostics gate and survives disconnects.
            let collected = runtime::RuntimeRequestContext::production()
                .run_blocking_io(
                    runtime::BlockingStage::Diagnostics,
                    state.paths.diagnostics_dir.clone(),
                    move || collect_current_diagnostics(&state, command.note),
                )
                .await;
            match collected {
                Ok(bundle) => (
                    StatusCode::CREATED,
                    Json(DiagnosticsResponse::new(vec![bundle])),
                )
                    .into_response(),
                Err(error) => data_error_response(error, "diagnostics_collect_failed"),
            }
        }
    }
}

fn reveal_diagnostic_export(path: &std::path::Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .creation_flags(0x0800_0000)
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(
                path.parent()
                    .ok_or_else(|| io::Error::other("Diagnostic export has no parent"))?,
            )
            .spawn()?;
    }
    Ok(())
}

#[cfg(test)]
static RELEASE_REMOVE_TEST_GATE: std::sync::Mutex<
    Option<(
        String,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);
#[cfg(test)]
fn wait_release_remove_test_gate(id: &str) {
    let gate = {
        let mut stored = RELEASE_REMOVE_TEST_GATE.lock().unwrap();
        if stored.as_ref().is_some_and(|entry| entry.0 == id) {
            stored.take()
        } else {
            None
        }
    };
    if let Some((_, entered, release)) = gate {
        entered.send(()).unwrap();
        release
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }
}

async fn config_status(State(state): State<AppState>) -> axum::response::Response {
    match state.config.snapshot() {
        Ok(document) => match config_response_for_paths(&state.paths, document) {
            Ok(mut response) => {
                let next = crate::launch_inputs::next(&state.paths).ok();
                let current = state.supervisor.launch_input_identity().and_then(
                    |(generation, status, session)| {
                        crate::launch_inputs::current(&state.paths, generation, status, &session)
                    },
                );
                response.launch_inputs =
                    Some(serde_json::json!({ "next_launch": next, "running_launch": current }));
                (StatusCode::OK, Json(response)).into_response()
            }
            Err(error) => data_error_response(error, "config_unavailable"),
        },
        Err(error) => {
            let code = if nexus_core::is_config_transaction_error(&error) {
                "config_recovery_pending"
            } else {
                "config_unavailable"
            };
            data_error_response(error, code)
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MaintenanceRequest {
    #[serde(default)]
    expected_revision: Option<String>,
    action: String,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceRequest {
    action: String,
    #[serde(default)]
    retention_days: Option<u32>,
    #[serde(default)]
    preview_id: Option<String>,
    #[serde(default)]
    item_ids: Vec<String>,
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum MaintenancePreviewPhase {
    #[default]
    Idle,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Default, serde::Serialize)]
struct MaintenancePreviewScan {
    state: MaintenancePreviewPhase,
    operation_id: String,
    retention_days: u32,
    error: Option<String>,
    wait_message: Option<String>,
}

// One reservation per Agent, retained by the disk worker after the HTTP wait
// expires. No lifecycle/update lock is needed for this read-only inspection.
struct MaintenancePreviewWorker(Arc<std::sync::Mutex<MaintenancePreviewScan>>, bool);
impl MaintenancePreviewWorker {
    fn finish(&mut self, error: Option<String>) {
        let mut scan = self.0.lock().unwrap_or_else(|e| e.into_inner());
        self.1 = true;
        scan.state = if error.is_some() {
            MaintenancePreviewPhase::Failed
        } else {
            MaintenancePreviewPhase::Completed
        };
        scan.error = error;
        scan.wait_message = None;
    }
}
impl Drop for MaintenancePreviewWorker {
    fn drop(&mut self) {
        if self.1 {
            return;
        }
        let mut scan = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(scan.state, MaintenancePreviewPhase::Running) {
            scan.state = MaintenancePreviewPhase::Failed;
            scan.error = Some("Cleanup preview worker stopped before saving a result".into());
        }
    }
}

fn maintenance_preview_snapshot(state: &AppState) -> MaintenancePreviewScan {
    state
        .maintenance_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn maintenance_response(
    status: StatusCode,
    saved: Option<nexus_core::maintenance::MaintenanceStatus>,
    scan: MaintenancePreviewScan,
) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "preview": saved.as_ref().and_then(|s| s.preview.as_ref()),
            "result": saved.as_ref().and_then(|s| s.result.as_ref()),
            "preview_scan": scan,
        })),
    )
        .into_response()
}

async fn maintenance_status(State(state): State<AppState>) -> axum::response::Response {
    let scan = maintenance_preview_snapshot(&state);
    if matches!(scan.state, MaintenancePreviewPhase::Running) {
        return maintenance_response(StatusCode::OK, None, scan);
    }
    let paths = state.paths.clone();
    match runtime::RuntimeRequestContext::production()
        .run_blocking_io(
            runtime::BlockingStage::Maintenance,
            paths.run_dir.clone(),
            move || nexus_core::maintenance::MaintenanceStore::new(paths).status(),
        )
        .await
    {
        Ok(status) => maintenance_response(
            StatusCode::OK,
            Some(status),
            maintenance_preview_snapshot(&state),
        ),
        Err(error) => data_error_response(error, "maintenance_status_failed"),
    }
}

async fn maintenance_preview_with_budget(
    state: AppState,
    retention_days: u32,
    request: runtime::RuntimeRequestContext,
) -> axum::response::Response {
    if !(1..=3650).contains(&retention_days) {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "maintenance_invalid_retention",
            "Retention must be between 1 and 3650 days",
        );
    }
    {
        let mut scan = state
            .maintenance_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if matches!(scan.state, MaintenancePreviewPhase::Running) {
            return maintenance_response(StatusCode::ACCEPTED, None, scan.clone());
        }
        *scan = MaintenancePreviewScan {
            state: MaintenancePreviewPhase::Running,
            operation_id: nexus_core::new_instance_id(),
            retention_days,
            error: None,
            wait_message: None,
        };
    }
    let paths = state.paths.clone();
    let mut worker = MaintenancePreviewWorker(state.maintenance_preview.clone(), false);
    let result = request
        .run_blocking_io(
            runtime::BlockingStage::Maintenance,
            paths.root.clone(),
            move || {
                let result = (|| {
                    let protected_logs = nexus_core::HarnessLogSessionStore::new(paths.clone())
                        .read()?
                        .map(|session| vec![session.stdout_log_name, session.stderr_log_name])
                        .unwrap_or_default();
                    nexus_core::maintenance::MaintenanceStore::new(paths)
                        .preview(retention_days, &protected_logs)
                })();
                worker.finish(result.as_ref().err().map(ToString::to_string));
                result
            },
        )
        .await;
    match result {
        Ok(status) => maintenance_response(
            StatusCode::OK,
            Some(status),
            maintenance_preview_snapshot(&state),
        ),
        Err(error) => {
            let mut scan = state
                .maintenance_preview
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if error.kind() == io::ErrorKind::TimedOut
                && matches!(scan.state, MaintenancePreviewPhase::Running)
            {
                scan.wait_message = Some(error.to_string());
                maintenance_response(StatusCode::ACCEPTED, None, scan.clone())
            } else {
                data_error_response(error, "maintenance_preview_failed")
            }
        }
    }
}

async fn maintenance_dispatch(
    State(state): State<AppState>,
    Json(value): Json<serde_json::Value>,
) -> axum::response::Response {
    if matches!(
        value.get("action").and_then(|v| v.as_str()),
        Some("reset" | "restore_previous")
    ) {
        return match serde_json::from_value(value) {
            Ok(request) => maintenance_control(State(state), Json(request)).await,
            Err(error) => data_error_response(
                io::Error::new(io::ErrorKind::InvalidInput, error),
                "maintenance_invalid_request",
            ),
        };
    }
    let request: SpaceRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => {
            return data_error_response(
                io::Error::new(io::ErrorKind::InvalidInput, error),
                "maintenance_invalid_request",
            )
        }
    };
    let paths = state.paths.clone();
    if request.action == "preview" {
        return maintenance_preview_with_budget(
            state,
            request.retention_days.unwrap_or(30),
            runtime::RuntimeRequestContext::production(),
        )
        .await;
    }
    let protected_logs = match nexus_core::HarnessLogSessionStore::new(paths.clone()).read() {
        Ok(session) => session
            .map(|s| vec![s.stdout_log_name, s.stderr_log_name])
            .unwrap_or_default(),
        Err(error) => return data_error_response(error, "maintenance_logs_unavailable"),
    };
    if request.action != "cleanup" {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "maintenance_invalid_action",
            "Choose preview, cleanup, or reset",
        );
    }
    let Some(preview_id) = request.preview_id else {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "maintenance_preview_required",
            "Create and confirm a cleanup preview first",
        );
    };
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
        return response;
    }
    let update_guard = match state.updater.try_acquire_gate() {
        Ok(guard) => guard,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) {
        return response;
    }
    let snapshot_guard = match state.snapshots.try_acquire_configuration() {
        Ok(guard) => guard,
        Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    let cold_guard = match state.cold.try_acquire_maintenance() {
        Ok(guard) => guard,
        Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    // The worker owns every gate until the durable result is written, even if
    // the HTTP connection closes. Cleanup never follows a client-supplied path.
    let result = tokio::task::spawn_blocking(move || {
        let _guards = (lifecycle, update_guard, snapshot_guard, cold_guard);
        nexus_core::maintenance::MaintenanceStore::new(paths).cleanup(
            &preview_id,
            &request.item_ids,
            &protected_logs,
        )
    })
    .await;
    match result {
        Ok(Ok(status)) => (StatusCode::OK, Json(status)).into_response(),
        Ok(Err(error)) => data_error_response(error, "maintenance_cleanup_failed"),
        Err(error) => data_error_response(
            io::Error::other(error.to_string()),
            "maintenance_cleanup_failed",
        ),
    }
}

async fn maintenance_control(
    State(state): State<AppState>,
    Json(request): Json<MaintenanceRequest>,
) -> axum::response::Response {
    let Some(expected) = request.expected_revision.filter(|value| !value.is_empty()) else {
        return api_error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "config_revision_required",
            "Refresh configuration before maintenance; expected_revision is required",
        );
    };
    let restore_previous = request.action == "restore_previous";
    if request.action != "reset" && !restore_previous {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "maintenance_invalid_action",
            "maintenance action must be \"reset\"",
        );
    }
    let scope = match request.scope.as_deref() {
        None | Some("config") => "config",
        Some("slots") => "slots",
        _ => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "maintenance_invalid_scope",
                "maintenance scope must be \"config\" or \"slots\"",
            )
        }
    };
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
        return response;
    }
    let update_guard = match state.updater.try_acquire_gate() {
        Ok(guard) => guard,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) {
        return response;
    }
    let snapshot_guard = match state.snapshots.try_acquire_configuration() {
        Ok(guard) => guard,
        Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    let cold_guard = match state.cold.try_acquire_maintenance() {
        Ok(guard) => guard,
        Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    if restore_previous {
        if scope != "config" {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "maintenance_invalid_scope",
                "Previous configuration restore does not change the slot registry",
            );
        }
        let config = state.config.clone();
        let restored = tokio::task::spawn_blocking(move || {
            let _guards = (lifecycle, update_guard, snapshot_guard, cold_guard);
            config.restore_previous_if_revision(&expected)
        })
        .await;
        return match restored {
            Ok(Ok(_)) => (StatusCode::OK, Json(serde_json::json!({"status":"ok", "action":"restore_previous", "restart_required":true,
                "note":"Previous valid Nexus configuration restored. Harness was not started."}))).into_response(),
            Ok(Err(error)) => data_error_response(error, "config_restore_failed"),
            Err(error) => data_error_response(io::Error::other(error.to_string()), "config_restore_failed"),
        };
    }
    let paths = state.paths.clone();
    let owned_scope = scope.to_owned();
    let config = state.config.clone();
    let task_scope = owned_scope.clone();
    // Guards belong to the blocking owner, so a disconnected HTTP caller
    // cannot unlock configuration while this reset is still writing.
    let reset = tokio::task::spawn_blocking(move || {
        let _guards = (lifecycle, update_guard, snapshot_guard, cold_guard);
        nexus_core::UpdateStateStore::new(paths.clone()).load()?;
        if task_scope == "slots" {
            nexus_core::ReleaseStore::new(paths.clone()).load()?;
        }
        let (_, (backup_dir, backed_up)) =
            config.transaction_if_revision(&expected, |document| {
                let backup = backup_reset_targets(&paths, &task_scope)?;
                *document = nexus_core::NexusConfigFile::default();
                Ok(backup)
            })?;
        let removed = clear_reset_targets(&paths, &task_scope)?;
        Ok::<_, io::Error>((backup_dir, backed_up, removed))
    })
    .await;
    let (backup_dir, backed_up, removed) = match reset {
        Ok(Ok(values)) => values,
        Ok(Err(error)) => return data_error_response(error, "maintenance_reset_failed"),
        Err(error) => {
            return data_error_response(
                io::Error::other(error.to_string()),
                "maintenance_reset_failed",
            )
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "scope": owned_scope,
            "backup_dir": backup_dir.to_string_lossy(),
            "backed_up": backed_up,
            "removed": removed,
            "warning": null,
        })),
    )
        .into_response()
}

/// Copy every file the reset will replace or remove into the diagnostics
/// backup directory (named with nanosecond precision so two resets can never
/// share a directory and overwrite each other's originals).
fn backup_reset_targets(
    paths: &nexus_core::NexusPaths,
    scope: &str,
) -> io::Result<(std::path::PathBuf, Vec<String>)> {
    paths.ensure_directories()?;
    let backup_dir = paths.diagnostics_dir.join(format!(
        "reset-backup-{}",
        nexus_core::unix_time_nanos_for_update()
    ));
    std::fs::create_dir_all(&backup_dir)?;
    let mut backed_up: Vec<String> = Vec::new();
    backup_file(
        &paths.config_file,
        &backup_dir,
        "config.json",
        &mut backed_up,
    )?;
    if paths.update_state_file.exists() {
        backup_file(
            &paths.update_state_file,
            &backup_dir,
            "update-state.json",
            &mut backed_up,
        )?;
    }
    if scope == "slots" {
        backup_file(
            &paths.release_pointers_file,
            &backup_dir,
            "release-pointers.json",
            &mut backed_up,
        )?;
    }
    Ok((backup_dir, backed_up))
}

/// Remove the reset targets after the backup and the locked default rewrite
/// completed. Slot directories on disk are never touched.
fn clear_reset_targets(paths: &nexus_core::NexusPaths, scope: &str) -> io::Result<Vec<String>> {
    let mut removed: Vec<String> = Vec::new();
    if paths.update_state_file.exists() {
        std::fs::remove_file(&paths.update_state_file)?;
        removed.push("update-state.json".to_owned());
    }
    if scope == "slots" && paths.release_pointers_file.exists() {
        std::fs::remove_file(&paths.release_pointers_file)?;
        removed.push("release-pointers.json".to_owned());
    }
    Ok(removed)
}

fn backup_file(
    source: &std::path::Path,
    backup_dir: &std::path::Path,
    name: &str,
    backed_up: &mut Vec<String>,
) -> io::Result<()> {
    if let Some(bytes) = nexus_core::read_regular_file_bounded(source, 4 * 1024 * 1024)? {
        nexus_core::write_private_bytes_atomic(backup_dir, &backup_dir.join(name), &bytes)?;
        backed_up.push(name.to_owned());
    }
    Ok(())
}

async fn ensure_harness_stopped(
    state: &AppState,
    lifecycle: &supervisor::HarnessLifecycleGuard,
) -> Result<(), axum::response::Response> {
    ensure_harness_selection_quiescent(
        state,
        lifecycle,
        "config_change_conflict",
        "cannot change Harness launch configuration until Harness is positively stopped and unowned",
    )
    .await
}

async fn ensure_harness_selection_quiescent(
    state: &AppState,
    lifecycle: &supervisor::HarnessLifecycleGuard,
    code: &str,
    message: &str,
) -> Result<(), axum::response::Response> {
    let _ = sync_harness_state(state).await;
    if state
        .supervisor
        .selection_change_is_quiescent(lifecycle)
        .await
    {
        Ok(())
    } else {
        Err(api_error_response(StatusCode::CONFLICT, code, message))
    }
}

fn ensure_update_idle(state: &AppState) -> Result<(), axum::response::Response> {
    match state.updater.status() {
        Ok(update) if update.state == UpdateState::Running => Err(api_error_response(
            StatusCode::CONFLICT,
            "config_change_conflict",
            "cannot change update configuration while an update is running",
        )),
        Ok(_) => Ok(()),
        Err(error) => Err(update_error_response(error)),
    }
}

fn api_error_response(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(ErrorResponse {
            api_version: nexus_protocol::API_VERSION.to_owned(),
            code: code.to_owned(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn data_error_response(error: io::Error, fallback_code: &str) -> axum::response::Response {
    if nexus_core::is_config_revision_conflict(&error) {
        return api_error_response(
            StatusCode::CONFLICT,
            "config_revision_conflict",
            &error.to_string(),
        );
    }
    let status = match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy => StatusCode::CONFLICT,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let kind = match error.kind() {
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::ResourceBusy | io::ErrorKind::WouldBlock => "busy",
        io::ErrorKind::TimedOut | io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset => "unavailable",
        _ => "other",
    };
    (status, Json(serde_json::json!({
        "api_version": nexus_protocol::API_VERSION, "code": fallback_code,
        "message": error.to_string(), "kind": kind,
        "retryable": matches!(kind, "busy" | "unavailable"),
    }))).into_response()
}

fn harness_error_response(error: HarnessSupervisorError) -> axum::response::Response {
    if let HarnessSupervisorError::Preflight(report) = &error {
        return (StatusCode::CONFLICT,Json(serde_json::json!({"api_version":nexus_protocol::API_VERSION,"code":"harness_preflight_blocked","message":error.to_string(),"preflight":report}))).into_response();
    }
    let (status, code) = match &error {
        HarnessSupervisorError::Preflight(_) => unreachable!(),
        HarnessSupervisorError::Cancelled => (StatusCode::CONFLICT, "harness_start_cancelled"),
        HarnessSupervisorError::Busy => (StatusCode::CONFLICT, "harness_operation_busy"),
        HarnessSupervisorError::NotConfigured => {
            (StatusCode::UNPROCESSABLE_ENTITY, "harness_not_configured")
        }
        HarnessSupervisorError::AlreadyRunning => (StatusCode::CONFLICT, "harness_already_running"),
        HarnessSupervisorError::Unattached => (StatusCode::CONFLICT, "harness_unattached"),
        HarnessSupervisorError::InvalidProfile(_) => {
            (StatusCode::BAD_REQUEST, "harness_profile_invalid")
        }
        HarnessSupervisorError::Configuration(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "harness_configuration_error",
        ),
        HarnessSupervisorError::Readiness(_) => {
            (StatusCode::BAD_GATEWAY, "harness_readiness_failed")
        }
        HarnessSupervisorError::Spawn(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "harness_spawn_failed")
        }
        HarnessSupervisorError::Process(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "harness_process_error")
        }
        HarnessSupervisorError::Persistence(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "harness_state_persistence_failed",
        ),
    };
    (
        status,
        Json(ErrorResponse {
            api_version: nexus_protocol::API_VERSION.to_owned(),
            code: code.to_owned(),
            message: error.to_string(),
        }),
    )
        .into_response()
}

fn update_error_response(error: UpdateExecutorError) -> axum::response::Response {
    let (status, code) = match &error {
        UpdateExecutorError::NotConfigured => {
            (StatusCode::UNPROCESSABLE_ENTITY, "update_not_configured")
        }
        UpdateExecutorError::AlreadyRunning => (StatusCode::CONFLICT, "update_already_running"),
        UpdateExecutorError::Configuration(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "update_configuration_error",
        ),
        UpdateExecutorError::Spawn { .. }
        | UpdateExecutorError::Process { .. }
        | UpdateExecutorError::Failed { .. }
        | UpdateExecutorError::TimedOut { .. } => {
            (StatusCode::BAD_GATEWAY, "update_execution_failed")
        }
        UpdateExecutorError::Persistence(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "update_state_persistence_failed",
        ),
    };
    (
        status,
        Json(ErrorResponse {
            api_version: nexus_protocol::API_VERSION.to_owned(),
            code: code.to_owned(),
            message: error.to_string(),
        }),
    )
        .into_response()
}

async fn lifecycle(
    State(state): State<AppState>,
    Json(command): Json<LifecycleCommand>,
) -> impl IntoResponse {
    match command.action {
        LifecycleAction::Shutdown => accept_shutdown(state, command.action).await.into_response(),
        LifecycleAction::ShutdownIfIdle => shutdown_if_idle(state).await,
    }
}

async fn shutdown_if_idle(state: AppState) -> axum::response::Response {
    let Some(lifecycle) = state.supervisor.try_acquire_lifecycle() else {
        return api_error_response(StatusCode::CONFLICT, "desktop_update_busy", "Harness lifecycle is busy");
    };
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await { return response; }
    let _update = match state.updater.try_acquire_gate() {
        Ok(guard) => guard, Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let _snapshot = match state.snapshots.try_acquire_configuration() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "desktop_update_busy"),
    };
    let _cold = match state.cold.try_acquire_maintenance() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "desktop_update_busy"),
    };
    // Latch under the lifecycle guard so an already queued start cannot race
    // the idle observation. It is reset only by a fresh Agent process.
    state.supervisor.seal_for_desktop_update(&lifecycle);
    accept_shutdown(state, LifecycleAction::ShutdownIfIdle).await.into_response()
}

async fn shutdown(State(state): State<AppState>) -> impl IntoResponse {
    accept_shutdown(state, LifecycleAction::Shutdown).await
}

async fn accept_shutdown(
    state: AppState,
    action: LifecycleAction,
) -> (StatusCode, Json<LifecycleAccepted>) {
    if let Err(error) = update_agent_state(&state, |current| {
        current.request_shutdown();
    })
    .await
    {
        tracing::warn!(error = %error, "failed to persist Agent shutdown state");
    }
    let _ = state.shutdown.send(true);
    (
        StatusCode::ACCEPTED,
        Json(LifecycleAccepted::accepted(action)),
    )
}

async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }

    tokio::select! {
        result = receiver.changed() => {
            let _ = result;
        }
        result = tokio::signal::ctrl_c() => {
            let _ = result;
        }
    }
}

#[cfg(test)]
#[path = "tests/cors_tests.rs"]
mod cors_tests;

#[cfg(test)]
#[path = "tests/checkpoint_tests.rs"]
mod checkpoint_tests;

#[cfg(test)]
#[path = "tests/switch_ownership_tests.rs"]
mod switch_ownership_tests;
