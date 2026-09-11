//! Headless Nexus control-plane process.
mod runtime_patches;
mod canary;
mod source_context;
mod recovery_records;

#[cfg(windows)]
mod windows_terminal;
#[cfg(windows)]
pub mod windows_harness;

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
    AgentDiscoveryRecord, data_root_identity, discover_harness_candidates_with_paths,
    load_update_spec, new_instance_id, redact_diagnostics_payload, AgentState,
    CheckpointRestoreIntent, CheckpointRestoreJournal, CheckpointRestoreJournalStore,
    CheckpointRestorePhase, CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec,
    HarnessLogSession, HarnessLogSessionStore, NexusConfig, NexusConfigFile, NexusStateSnapshot,
    ProfileCatalog, ProfileStore, ReleaseCatalog, ReleaseStore, RuntimeConfig, SnapshotsConfig,
    unix_time_seconds, UpdateSpec, DEFAULT_MAX_RELEASE_SLOTS, DEFAULT_PROFILE, HARNESS_ARGS_ENV, HARNESS_PROGRAM_ENV,
    HARNESS_READINESS_TIMEOUT_ENV, HARNESS_READINESS_URL_ENV, HARNESS_WORKING_DIR_ENV,
    UPDATE_BUILD_ARGS_ENV, UPDATE_BUILD_PROGRAM_ENV, UPDATE_GIT_PROGRAM_ENV, UPDATE_REF_ENV,
    UPDATE_SOURCE_ENV, UPDATE_TIMEOUT_ENV, UPDATE_VERIFY_ARGS_ENV, UPDATE_VERIFY_PROGRAM_ENV,
};
use nexus_launcher_core::{
    harness_observation_matches_session, read_harness_ui_info_with_observer, HarnessLogObserver, unavailable_harness_ui_info,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointAction, CheckpointCommand, CheckpointContentState,
    CheckpointCreateResponse, CheckpointListResponse, CheckpointRestoreResponse, ConfigAction,
    ConfigCommand, ConfigResponse, DiagnosticsAction, DiagnosticsCommand, DiagnosticsResponse,
    ErrorResponse, HarnessAction, HarnessCommand, HarnessDiscoveryResponse, HarnessResponse,
    HarnessRuntimeInfo, HealthResponse, LifecycleAccepted, LifecycleAction, LifecycleCommand,
    PluginRemoveResponse, ProfileAction, ProfileCommand, ProfileListResponse,
    ProfileOpenPathResponse,
    ProfileSelectResponse, RecoveryLogTail, RecoveryStatusResponse, ReleaseAction, ReleaseCommand,
    ReleaseListResponse, RuntimeInstallMode, RuntimePlanRequest, RuntimeSource,
    CheckpointManifest, SnapshotReference, StateResponse, TagListResponse,
    UpdateAction, UpdateCommand, UpdateResponse, UpdateState,
};
use tokio::{
    net::TcpListener,
    sync::{watch, Mutex, RwLock},
};

mod cold;
mod log_retention;
mod request_receipts;
#[doc(hidden)]
pub mod git_worker;
mod compatibility;
mod dsh;
mod runtime;
mod preflight;
mod preference_capabilities;
mod launch_inputs;
mod recovery_mode;
mod profile_archive;
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
        ($value:expr, $stage:literal) => { match $value {
            Ok(value) => value,
            Err(error) => return run_read_only(&config, &paths, credential.clone(), &data_root_id, &instance_id,
                format!("{}: {error}", $stage)).await,
        } };
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
    if let Err(error) = recover_checkpoint_restore_startup(&checkpoint_restores, &profiles, &releases, &snapshots).await {
        // Recovery evidence remains authoritative. Mutation/start guards read
        // it again; a failed recovery must not remove the repair API itself.
        tracing::warn!(%error, "Checkpoint recovery remains pending; Agent remains available");
    }
    let profile_catalog = initialize!(profiles.load(), "profiles");
    if let Err(error)=profile_archive::recover_if_present(&paths, &profiles) {
        tracing::warn!(%error, "Deleted profile recovery is pending; archive mutations remain blocked");
    }
    let diagnostics = DiagnosticsStore::new(paths.clone());
    let updater = UpdateExecutor::new(paths.clone(), releases.clone());
    let _ = initialize!(updater.recover_unattached(), "update state");
    let cold = cold::ColdCoordinator::new(paths.clone());
    if let Err(error) = cold.recover() {
        tracing::warn!(%error, "Cold recovery remains pending; Agent remains available");
    }
    let release_catalog = initialize!(releases.load(), "release catalog");
    if !release_catalog.unavailable_selections.is_empty() {
        tracing::warn!(releases = ?release_catalog.unavailable_selections,
            "Selected Harness release is missing or incomplete; Agent remains available for reinstallation");
    }
    let supervisor = initialize!(HarnessSupervisor::new(paths.clone()), "runtime metadata or log session");
    let metadata = supervisor.metadata_store();
    // A restart can only recover a persisted Harness state by proving the
    // configured loopback readiness endpoint; it never reattaches a stale PID.
    let initial_harness = supervisor.recover_unattached().await;
    let mut initial_runtime = AgentState::starting();
    initial_runtime.profile = Some(profile_catalog.active_profile.clone());
    initial_runtime.release = release_catalog.current_release.clone();
    initial_runtime.harness = initial_harness.state;
    initialize!(metadata.write_snapshot(&initial_runtime, initial_harness.clone()), "runtime metadata");
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
        maintenance_preview: Arc::new(std::sync::Mutex::new(crate::MaintenancePreviewScan::default())),
        crash_capture_run: Arc::new(Mutex::new(CrashCapture::default())),
        canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(nexus_launcher_core::HarnessLogObserver::default())),
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
            return run_read_only(&config, &paths, credential, &data_root_id, &instance_id, format!("runtime metadata: {error}")).await;
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
async fn run_read_only(config: &NexusConfig, paths: &nexus_core::NexusPaths,
    credential: nexus_core::agent_auth::AgentCredential, root_id: &str, instance_id: &str, reason: String) -> io::Result<()> {
    let (safe, _) = redact_diagnostics_payload(reason.as_bytes());
    let reason = String::from_utf8_lossy(&safe).into_owned();
    tracing::warn!(%reason, "Agent entered read-only recovery; repair the original files and restart");
    let (shutdown, receiver) = watch::channel(false);
    let mut health = serde_json::to_value(HealthResponse::healthy(root_id.to_owned(), instance_id.to_owned()))?;
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
    paths.publish_agent_discovery(&AgentDiscoveryRecord { port:listener.local_addr()?.port(), instance_id:instance_id.to_owned(),
        data_root_id:root_id.to_owned(), pid:std::process::id(), updated_at_unix:unix_time_seconds() })?;
    axum::serve(listener, app).with_graceful_shutdown(wait_for_shutdown(receiver)).await
}

fn build_router(state: AppState, credential: nexus_core::agent_auth::AgentCredential) -> Router {
    let receipts = request_receipts::Receipts::new(state.paths.root.clone());
    Router::new()
        .route("/v1/recovery/records", get(recovery_records::inspect_normal).post(recovery_records::control_normal))
        .route("/v1/requests", get(request_receipts::list).with_state(receipts.clone()))
        .route("/v1/health", get(health))
        .route("/v1/state", get(current_state))
        .route("/v1/harness", get(harness_status).post(harness_control))
        .route("/v1/harness/startup", get(harness_startup_status).post(harness_startup_cancel))
        .route("/v1/harness/discover", get(harness_discover))
        .route("/v1/harness/ui", get(harness_ui))
        .route("/v1/profiles", get(profile_list).post(profile_control))
        .route("/v1/canary", get(canary::status).post(canary::control))
        .route("/v1/recovery", get(recovery_status).post(recovery_control))
        .route("/v1/preflight", get(preflight::check))
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
        .route("/v1/maintenance", get(maintenance_status).post(maintenance_dispatch))
        .route("/v1/lifecycle", post(lifecycle))
        .route("/v1/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(receipts, request_receipts::enforce))
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
async fn enforce_loopback_host(
    request: Request,
    next: Next,
) -> Response {
    let ok = match request.headers().get(axum::http::header::HOST) {
        None => true,
        Some(value) => value
            .to_str()
            .map(host_header_is_loopback)
            .unwrap_or(false),
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

async fn recover_checkpoint_restore_startup(
    journal_store: &CheckpointRestoreJournalStore,
    profiles: &ProfileStore,
    releases: &ReleaseStore,
    snapshots: &snapshots::SnapshotCoordinator,
) -> io::Result<()> {
    let Some(journal) = journal_store.load()? else {
        return Ok(());
    };
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
        Self { credential, nonces: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())), body_budget: Arc::new(tokio::sync::Semaphore::new(4)) }
    }
}

async fn enforce_api_authorization(
    State(state): State<ApiAuthorization>,
    request: Request,
    next: Next,
) -> Response {
    use nexus_core::agent_auth as auth;
    if request.method() == Method::GET && request.uri().path_and_query().is_some_and(|p| p.as_str() == "/v1/health") {
        return next.run(request).await;
    }
    if !proxy_identity_values_match(request.headers(), &state.credential.data_root_id, &state.credential.instance_id) {
        return StatusCode::CONFLICT.into_response();
    }
    let (nonce, time, signature) = {
        let header = |key| request.headers().get(key).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        (header(auth::NONCE_HEADER), header(auth::TIME_HEADER), header(auth::SIGNATURE_HEADER))
    };
    let now = auth::unix_seconds();
    if request.headers().get(auth::VERSION_HEADER).and_then(|v| v.to_str().ok()) != Some("2") { return StatusCode::UNAUTHORIZED.into_response(); }
    if !auth::valid_hex(&nonce) || !auth::valid_hex(&signature) { return StatusCode::UNAUTHORIZED.into_response(); }
    let Some(timestamp) = time.parse::<u64>().ok().filter(|timestamp| now.abs_diff(*timestamp) <= auth::MAX_CLOCK_SKEW_SECS) else { return StatusCode::UNAUTHORIZED.into_response(); };
    let Ok(permit) = state.body_budget.clone().try_acquire_owned() else { return StatusCode::TOO_MANY_REQUESTS.into_response(); };
    let (mut parts, body) = request.into_parts();
    let body = match tokio::time::timeout(std::time::Duration::from_secs(5), axum::body::to_bytes(body, 1024 * 1024 + auth::TAG_BYTES)).await {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    if !state.credential.verify_request(parts.method.as_str(), path, &nonce, &time, &body, &signature) { return StatusCode::UNAUTHORIZED.into_response(); }
    let Ok(body) = state.credential.open_request(parts.method.as_str(), path, &nonce, &time, &body) else { return StatusCode::UNAUTHORIZED.into_response(); };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    {
        let Ok(mut nonces) = state.nonces.lock() else { return StatusCode::SERVICE_UNAVAILABLE.into_response(); };
        nonces.retain(|_, timestamp| now <= timestamp.saturating_add(auth::MAX_CLOCK_SKEW_SECS));
        if nonces.contains_key(&nonce) { return StatusCode::UNAUTHORIZED.into_response(); }
        if nonces.len() >= 16384 { return StatusCode::TOO_MANY_REQUESTS.into_response(); }
        nonces.insert(nonce.clone(), timestamp);
    }
    drop(permit);
    let response = next.run(Request::from_parts(parts, axum::body::Body::from(body))).await;
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, nexus_launcher_core::MAX_RESPONSE_BODY_BYTES).await else { return StatusCode::INTERNAL_SERVER_ERROR.into_response(); };
    let Ok(body) = state.credential.seal_response(&nonce, parts.status.as_u16(), &body) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response(); };
    let signature = state.credential.response_signature(&nonce, parts.status.as_u16(), &body);
    parts.headers.insert(auth::RESPONSE_HEADER, signature.parse().expect("hex header"));
    parts.headers.insert(auth::VERSION_HEADER, HeaderValue::from_static("2"));
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    if parts.status != StatusCode::NO_CONTENT { parts.headers.insert(axum::http::header::CONTENT_LENGTH, body.len().to_string().parse().expect("length header")); }
    parts.headers.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
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

    if request.headers().contains_key(ORIGIN) && !origin.as_deref().is_some_and(is_allowed_console_origin) {
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
    if response.binary_path.is_none() {
        response.binary_path = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
    }
    Json(response)
}

fn try_read_lifecycle(state: &AppState) -> Result<supervisor::HarnessLifecycleGuard, axum::response::Response> {
    // Read routes may settle a checkpoint, so they must retain the same gate
    // as mutations. Busy is observable immediately instead of queueing behind
    // a long compatibility probe or restore owner.
    state.supervisor.try_acquire_lifecycle().ok_or_else(|| api_error_response(
        StatusCode::CONFLICT, "lifecycle_busy",
        "NEXUS_LIFECYCLE_BUSY: Harness lifecycle operation is in progress; retry shortly",
    ))
}

async fn current_state(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
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
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
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
    let mostly_text = !bytes.contains(&0) && bytes.iter().filter(|b| b.is_ascii_graphic() || b.is_ascii_whitespace()).count() * 100 >= bytes.len().saturating_mul(85);
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCommand { action: RecoveryAction }
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecoveryAction { Enter, Leave }

async fn recovery_control(State(state): State<AppState>, Json(command): Json<RecoveryCommand>) -> axum::response::Response {
    // Keep the operation owned if the requesting window closes.
    match tokio::spawn(async move {
        let lifecycle = state.supervisor.acquire_lifecycle().await;
        let paused = matches!(command.action, RecoveryAction::Enter);
        if let Err(error) = recovery_mode::set_paused(&state.paths, paused) {
            return data_error_response(error, "recovery_mode_invalid");
        }
        if paused {
            if let Err(error) = state.supervisor.stop_locked(&lifecycle).await {
                return harness_error_response(error);
            }
        }
        drop(lifecycle);
        recovery_status(State(state)).await
    }).await {
        Ok(response) => response,
        Err(error) => data_error_response(io::Error::other(error.to_string()), "recovery_mode_failed"),
    }
}

async fn recovery_status(State(state): State<AppState>) -> axum::response::Response {
    let lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
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
    let (paused, pause_error) = match recovery_mode::paused(&state.paths) {
        Ok(value) => (value, None),
        Err(error) => {
            let message = bounded_checkpoint_diagnostic(&error);
            diagnostic_errors.push(format!("Recovery mode record: {message}"));
            (true, Some(message))
        }
    };
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
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
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
            )
        }
        Err(error) => {
            observer.invalidate();
            return unavailable_harness_ui_response(format!(
                "Harness log session marker is invalid: {error}"
            ))
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

    (StatusCode::OK, Json(info)).into_response()
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

async fn harness_startup_status(State(state):State<AppState>)->Json<serde_json::Value>{Json(state.supervisor.startup_status().await)}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupCancel { action:String,operation_id:String }
async fn harness_startup_cancel(State(state):State<AppState>,Json(command):Json<StartupCancel>)->axum::response::Response {
    if command.action!="cancel"||command.operation_id.len()>160{return data_error_response(io::Error::other("Invalid startup cancellation request"),"startup_cancel_invalid");}
    if !state.supervisor.cancel_startup(&command.operation_id).await{return (StatusCode::CONFLICT,Json(serde_json::json!({"code":"startup_cancel_stale","message":"This startup is no longer cancellable; refresh its status. Use Stop after process creation."}))).into_response();}
    (StatusCode::ACCEPTED,Json(state.supervisor.startup_status().await)).into_response()
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
        Err(error) => data_error_response(io::Error::other(error.to_string()), "harness_owner_failed"),
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
    let lifecycle = if matches!(action,HarnessAction::Start|HarnessAction::Restart) {state.supervisor.try_acquire_lifecycle().ok_or(HarnessSupervisorError::Busy)?} else {state.supervisor.acquire_lifecycle().await};
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
    if matches!(action,HarnessAction::Start|HarnessAction::Restart) {
        state.supervisor.begin_startup().await?;
        let result=async {
            let (report,prepared)=preflight::evaluate(state.clone()).await;
            if report["ready"]!=true || report["paused"]==true {return Err(HarnessSupervisorError::Preflight(report));}
            let prepared=prepared.ok_or_else(||HarnessSupervisorError::Configuration(io::Error::other("Verified startup context is unavailable")))?;
            state.supervisor.start_prepared(&profile,&lifecycle,prepared,action==HarnessAction::Restart).await
        }.await;
        state.supervisor.finish_startup(result.is_ok(),matches!(result,Err(HarnessSupervisorError::Cancelled))).await;
        return result;
    }
    match action {
        HarnessAction::Start => {
            state
                .supervisor
                .start_with_profile_locked(&profile, &lifecycle)
                .await
        }
        HarnessAction::Stop => state.supervisor.stop_locked(&lifecycle).await,
        HarnessAction::Restart => {
            state
                .supervisor
                .restart_with_profile_locked(&profile, &lifecycle)
                .await
        }
        HarnessAction::Status => Ok(state.supervisor.status().await),
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
struct CrashCapture { run_id: String, attempts: u8, completed: bool, in_flight: Arc<std::sync::atomic::AtomicBool>, next_attempt: Option<std::time::Instant> }
struct CrashAttempt(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CrashAttempt { fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); } }

fn start_crash_observer(state: AppState, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() { return; }
            let (generation, runtime, log_session) = state.supervisor.status_observation().await;
            schedule_crash_capture(&state, &HarnessSnapshot { generation, runtime, log_session }).await;
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
        || observation.log_session.run_id.is_empty()
    {
        return;
    }
    let run_id = observation.log_session.run_id.clone();
    {
        let mut captured = state.crash_capture_run.lock().await;
        if captured.run_id != run_id { *captured = CrashCapture { run_id:run_id.clone(), ..Default::default() }; }
        if captured.in_flight.load(Ordering::SeqCst) || captured.completed || captured.attempts >= 3 || captured.next_attempt.is_some_and(|at| std::time::Instant::now() < at) {
            return;
        }
        captured.attempts += 1;
        captured.in_flight.store(true, Ordering::SeqCst);
        captured.next_attempt = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
    }
    let owned = state.clone();
    let attempt = CrashAttempt(state.crash_capture_run.lock().await.in_flight.clone());
    let note = format!("auto: crash evidence for run {run_id}");
    let result = runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, state.paths.diagnostics_dir.clone(), move || {
        let _attempt = attempt; // Also releases admission if queued work is dropped before execution.
        let result = collect_current_diagnostics(&owned, Some(note));
        let mut captured = owned.crash_capture_run.blocking_lock();
        if captured.run_id == run_id {
            captured.completed = result.is_ok();
            captured.next_attempt = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }
        result
    }).await;
    match result {
        Ok(bundle) => tracing::info!(bundle = %bundle.id, "captured automatic crash evidence"),
        Err(error) => {
            let (safe, _) = redact_diagnostics_payload(error.to_string().as_bytes());
            tracing::warn!(error = %String::from_utf8_lossy(&safe), "Automatic crash evidence collection failed; bounded retry remains available");
        },
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

fn profile_list_response(
    state: &AppState,
    catalog: ProfileCatalog,
) -> io::Result<ProfileListResponse> {
    let home = state.snapshots.configured_dsh_home()?;
    let mut manifests = dsh::native_profiles(&home)?;
    for manifest in &mut manifests { manifest.order_undo_id = dsh::order_undo_id(&state.paths, &home, &manifest.name)?; }
    let names = manifests
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let mut response = ProfileListResponse::new(catalog.active_profile, names).with_manifests(manifests);
    response.compatibility = compatibility::latest_for_selection(
        &state.paths,
        &state.snapshots.configured_dsh_home()?,
        &response.active_profile,
        source_context::compatibility_id(&state.paths,&state.releases)?.as_deref(),
    );
    let policy_profile = response.compatibility.as_ref().map(|report| report.source_profile.as_str()).unwrap_or(&response.active_profile);
    response.disabled_plugins = compatibility::disabled_plugins(&state.snapshots.configured_dsh_home()?, policy_profile)?;
    Ok(response)
}

async fn profile_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    match state
        .profiles
        .load()
        .and_then(|catalog| profile_list_response(&state, catalog))
    {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "profile_catalog_unavailable"),
    }
}

async fn profile_control(
    state: State<AppState>,
    command: Json<ProfileCommand>,
) -> axum::response::Response {
    if matches!(command.0.action, ProfileAction::Select | ProfileAction::CompatibilityCheck | ProfileAction::Delete | ProfileAction::RestoreDeleted) {
        // Selecting a profile now owns a long startup probe. Keep its lifecycle
        // and update gates until it finishes even if the caller disconnects.
        return match tokio::spawn(profile_control_inner(state, command)).await {
            Ok(response) => response,
            Err(error) => data_error_response(io::Error::other(error.to_string()), "profile_owner_failed"),
        };
    }
    profile_control_inner(state, command).await
}

async fn profile_control_inner(
    State(state): State<AppState>,
    Json(command): Json<ProfileCommand>,
) -> axum::response::Response {
    match command.action {
        ProfileAction::DeletedList => {
            let _lifecycle = match try_read_lifecycle(&state) { Ok(guard)=>guard,Err(response)=>return response };
            let result=state.snapshots.configured_dsh_home().and_then(|home|profile_archive::list(&state.paths,&home));
            match result {Ok(value)=>Json(value).into_response(),Err(error)=>data_error_response(error,"profile_archive_unavailable")}
        }
        ProfileAction::Delete | ProfileAction::RestoreDeleted => {
            let restore=command.action==ProfileAction::RestoreDeleted;
            let Some(target)=command.profile else {return data_error_response(io::Error::other("Choose a profile"),"profile_invalid");};
            if command.package.is_some() || command.target.is_some() {return data_error_response(io::Error::other("Unexpected profile archive parameters"),"profile_invalid");}
            let lifecycle=state.supervisor.acquire_lifecycle().await;
            if let Err(response)=ensure_checkpoint_mutation_ready(&state).await {return response;}
            if let Err(response)=ensure_harness_stopped(&state,&lifecycle).await {return response;}
            let update=match state.updater.try_acquire_gate() {Ok(guard)=>guard,Err(error)=>return update_error_response(error)};
            if let Err(response)=ensure_update_idle(&state) {return response;}
            let snapshots=match state.snapshots.try_acquire_configuration() {Ok(guard)=>guard,Err(error)=>return data_error_response(error,"profile_archive_conflict")};
            let cold=match state.cold.try_acquire_maintenance() {Ok(guard)=>guard,Err(error)=>return data_error_response(error,"profile_archive_conflict")};
            let home=match state.snapshots.configured_dsh_home() {Ok(home)=>home,Err(error)=>return data_error_response(error,"profile_archive_unavailable")};
            let result=tokio::task::spawn_blocking(move || {
                let _guards=(lifecycle,update,snapshots,cold);
                profile_archive::recover_if_present(&state.paths,&state.profiles)?;
                profile_archive::change(&state.paths,&state.profiles,&home,&target,restore)
            }).await;
            match result {Ok(Ok(value))=>Json(value).into_response(),Ok(Err(error))=>data_error_response(error,"profile_archive_failed"),Err(error)=>data_error_response(io::Error::other(error),"profile_archive_failed")}
        }
        ProfileAction::List | ProfileAction::Status | ProfileAction::PluginInventory => {
            if command.profile.is_some() || command.package.is_some() {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "profile inventory actions do not accept parameters",
                    ),
                    "profile_invalid",
                );
            }
            profile_list(State(state)).await
        }
        ProfileAction::Select => {
            if command.package.is_some() {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "profile select does not accept a package",
                    ),
                    "profile_invalid",
                );
            }
            let Some(profile) = command.profile.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "profile is required"),
                    "profile_invalid",
                );
            };
            let manifests =
                match dsh::native_profiles(&match state.snapshots.configured_dsh_home() {
                    Ok(home) => home,
                    Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
                }) {
                    Ok(manifests) => manifests,
                    Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
                };
            if !manifests.iter().any(|item| item.name == profile) {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "profile has no valid native manifest",
                    ),
                    "profile_invalid",
                );
            }
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
                "profile_change_conflict",
                "cannot switch profile until Harness is positively stopped and unowned",
            )
            .await
            {
                return response;
            }
            let paused = match recovery_mode::paused(&state.paths) {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "recovery_mode_invalid"),
            };
            if !paused {
                if let Err(error) = compatibility::for_profile_selection(&state, profile).await {
                    return data_error_response(error, "profile_compatibility_failed");
                }
            }
            let catalog = match ProfileCatalog::new(
                profile,
                manifests.iter().map(|item| item.name.clone()).collect(),
            )
            .and_then(|catalog| {
                state.profiles.write(&catalog)?;
                Ok(catalog)
            }) {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "profile_invalid"),
            };
            let active_profile = catalog.active_profile.clone();
            let current = match update_agent_state(&state, |current| {
                current.set_profile(active_profile);
            })
            .await
            {
                Ok((current, _)) => current,
                Err(error) => {
                    return data_error_response(
                        io::Error::other(error.to_string()),
                        "profile_state_persistence_failed",
                    )
                }
            };
            (
                StatusCode::OK,
                Json(ProfileSelectResponse::selected(
                    current
                        .profile
                        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned()),
                    catalog.profiles,
                )),
            )
                .into_response()
        }
        ProfileAction::PluginRemove => profile_plugin_remove(state, command).await,
        ProfileAction::CompatibilityCheck => profile_compatibility_check(state, command).await,
        ProfileAction::PluginMove | ProfileAction::PluginUndoMove => profile_plugin_move(state, command).await,
        ProfileAction::PluginDisable | ProfileAction::PluginEnable => profile_plugin_isolation(state, command).await,
        ProfileAction::OpenPath => profile_open_path(state, command).await,
        ProfileAction::OpenTerminal => profile_open_terminal(state, command).await,
        ProfileAction::Create => profile_create(state, command).await,
    }
}

async fn profile_compatibility_check(state: AppState, command: ProfileCommand) -> axum::response::Response {
    if command.package.is_some() || command.target.is_some() {
        return data_error_response(io::Error::new(io::ErrorKind::InvalidInput,
            "plugin verification does not accept package or target"), "profile_invalid");
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let _update = match state.updater.try_acquire_gate() {
        Ok(gate) => gate, Err(error) => return update_error_response(error),
    };
    let _cold = match state.cold.try_acquire_maintenance() {
        Ok(gate) => gate, Err(error) => return data_error_response(error, "profile_check_conflict"),
    };
    let _configuration = match state.snapshots.try_acquire_configuration() {
        Ok(gate) => gate, Err(error) => return data_error_response(error, "profile_check_conflict"),
    };
    if let Err(response) = ensure_harness_selection_quiescent(&state, &lifecycle,
        "profile_check_conflict", "stop Harness before verifying plugins").await { return response; }
    let result = async {
        let catalog = state.profiles.load()?;
        let home = state.snapshots.configured_dsh_home()?;
        let source = compatibility::source_profile(&home, &catalog.active_profile)?;
        if command.profile.as_deref().is_some_and(|profile| profile != source) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "verify the currently selected source profile"));
        }
        compatibility::check_selected(&state, &source).await?;
        profile_list_response(&state, catalog)
    }.await;
    match result {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "profile_compatibility_failed"),
    }
}

async fn profile_plugin_isolation(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let (Some(profile), Some(package)) = (command.profile.as_deref(), command.package.as_deref()) else {
        return data_error_response(io::Error::new(io::ErrorKind::InvalidInput,
            "profile and package are required"), "profile_invalid");
    };
    if command.target.is_some() {
        return data_error_response(io::Error::new(io::ErrorKind::InvalidInput,
            "plugin isolation does not accept a target"), "profile_invalid");
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let _update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(&state, &lifecycle,
        "plugin_isolation_conflict", "stop Harness before changing plugin isolation").await {
        return response;
    }
    let result = (|| -> io::Result<ProfileListResponse> {
        let home = state.snapshots.configured_dsh_home()?;
        let catalog = state.profiles.load()?;
        let source = compatibility::source_profile(&home, profile)?;
        let failed_target = compatibility::latest(&state.paths).is_some_and(|report|
            report.status == "needs_choice" && report.trigger.as_deref() == Some("profile_switch")
                && report.source_profile == source
                && source_context::compatibility_id(&state.paths,&state.releases).ok().flatten().as_deref() == Some(report.release_id.as_str()));
        if source != compatibility::source_profile(&home, &catalog.active_profile)? && !failed_target {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "plugin isolation must belong to the selected profile"));
        }
        compatibility::set_plugin_disabled(&home, profile, package, command.action == ProfileAction::PluginDisable)?;
        profile_list_response(&state, catalog)
    })();
    match result {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => data_error_response(error, "plugin_isolation_failed"),
    }
}

/// Create a new profile from the shipped `web` template. Metadata only:
/// the new profile is never selected and Harness is never restarted.
async fn profile_create(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let _update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate, Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let _snapshot_gate = match state.snapshots.try_acquire_configuration() {
        Ok(gate) => gate, Err(error) => return data_error_response(error, "profile_change_conflict"),
    };
    if let Err(response) = ensure_harness_selection_quiescent(&state, &lifecycle,
        "profile_change_conflict", "Stop Harness before creating a profile").await { return response; }
    let Some(name) = command.profile.as_deref() else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "profile name is required"),
            "profile_name_required",
        );
    };
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    match state.profiles.create(name, &dsh_home) {
        Ok(catalog) => {
            let response = ProfileListResponse::new(
                catalog.active_profile.clone(),
                catalog.profiles.clone(),
            );
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            data_error_response(error, "profile_already_exists")
        }
        Err(error) => data_error_response(error, "profile_create_failed"),
    }
}

// The child cannot use the release until its durable lease has been published.
// An Agent crash before registration therefore closes an inert terminal.
const DSH_TERMINAL_WAIT: &str = r#"$wait = [Diagnostics.Stopwatch]::StartNew(); while (-not (Test-Path -LiteralPath $env:NEXUS_TERMINAL_READY -PathType Leaf)) { if ($wait.Elapsed.TotalSeconds -ge 10) { exit 1 }; Start-Sleep -Milliseconds 100 }; Remove-Item -LiteralPath $env:NEXUS_TERMINAL_READY -ErrorAction Stop; "#;
// Command text is fixed; environment data never becomes shell source.
const DSH_TERMINAL_INIT: &str = r#"function global:dsh { $launch = if ($env:NEXUS_TERMINAL_ARGS) { @(ConvertFrom-Json -InputObject $env:NEXUS_TERMINAL_ARGS) } else { @("--profile", $env:NEXUS_TERMINAL_PROFILE) }; & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_ENTRY @launch @args }; function global:npm { & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_NPM @args }; function global:pnpm { if ($env:NEXUS_TERMINAL_PNPM_SCRIPT -eq '1') { & $env:NEXUS_TERMINAL_NODE $env:NEXUS_TERMINAL_PNPM @args } else { & $env:NEXUS_TERMINAL_PNPM @args } }"#;

/// Open an interactive terminal prepared for working with the selected
/// profile: the shell starts in the profile directory with `DSH_HOME` set,
/// resolved tools on `PATH`, and per-session `dsh`/`pnpm` functions.
async fn profile_open_terminal(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let _update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate, Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    let profiles = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let profile = command
        .profile
        .clone()
        .unwrap_or_else(|| profiles.active_profile.clone());
    // The name becomes a path segment and lands in generated cmd shims;
    // only the validated character set is accepted.
    if let Err(error) = nexus_core::validate_profile_name(&profile) {
        return data_error_response(error, "profile_invalid");
    }
    let profile_dir = dsh_home.join("profiles").join(&profile);
    if !profile_dir.is_dir() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("profile directory not found: {}", profile_dir.display()),
            ),
            "profile_dir_missing",
        );
    }
    let source=match source_context::resolve_async(&state.paths,&state.releases).await {Ok(s)=>s,Err(e)=>return data_error_response(e,"source_invalid")};
    let release_id=source.release_id.unwrap_or_else(||"external-harness".into());
    let Some(release_root)=source.root else {return data_error_response(io::Error::other("Select a Harness source"),"release_none_current");};
    let entry = release_root
        .join("apps")
        .join("cli")
        .join("lib")
        .join("bin.js");
    if !entry.is_file() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("release CLI entry is missing: {}", entry.display()),
            ),
            "release_entry_missing",
        );
    }
    let runtime = match crate::cold::resolved_runtime_config(&state).await {
        Ok(runtime) => runtime,
        Err(error) => return data_error_response(error, "runtime_unavailable"),
    };
    let Some(node) = runtime.node.as_ref().map(|pin| pin.path.clone()) else {
        return data_error_response(
            io::Error::new(io::ErrorKind::NotFound, "no usable node runtime"),
            "node_missing",
        );
    };
    let preferences=match nexus_core::load_harness_preferences(&state.paths){Ok(p)=>p,Err(e)=>return data_error_response(e,"preferences_invalid")};
    if let Err(e)=crate::runtime_patches::validate_for_paths(&state.paths,&preferences){return data_error_response(e,"patch_invalid");}
    let capabilities=match crate::preference_capabilities::resolve(Some(&release_root),&dsh_home,&profile,&preferences){Ok(c)=>c,Err(e)=>return data_error_response(e,"preferences_invalid")};
    let mut terminal_spec=HarnessLaunchSpec::new(node.clone()); terminal_spec.mode=nexus_protocol::HarnessLaunchMode::Node;
    terminal_spec.args=vec![entry.to_string_lossy().into_owned(),"--profile".into(),profile.clone()];
    nexus_core::apply_harness_preferences(&mut terminal_spec,&preferences,&capabilities);
    let terminal_args=serde_json::to_string(&terminal_spec.args[1..]).expect("terminal args serialization");
    let pnpm_pin = runtime.pnpm.as_ref().map(|pin| pin.path.clone());
    let pnpm_is_script = pnpm_pin.as_ref().is_some_and(|path| path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "js" | "cjs" | "mjs")));
    let envs = match nexus_core::build_runtime_child_env(
        &runtime,
        std::env::var_os("PATH").as_deref(),
    ) {
        Ok(envs) => envs,
        Err(error) => return data_error_response(error, "runtime_env_unavailable"),
    };
    #[cfg(windows)]
    {
        let powershell = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
        let ready = state.paths.run_dir.join(format!("terminal-start-{}.ready", match nexus_core::agent_auth::random_hex() {
            Ok(id) => id, Err(error) => return data_error_response(error, "terminal_preparation_failed"),
        }));
        let mut command = std::process::Command::new(powershell);
        command
            .args(["-NoLogo", "-NoProfile", "-NoExit", "-Command"])
            .arg(format!("{DSH_TERMINAL_WAIT}{DSH_TERMINAL_INIT}"))
            .current_dir(&profile_dir);
        for (key,value) in nexus_core::harness_preferences_environment(&preferences,&capabilities) {command.env(key,value);}
        command.env("NEXUS_TERMINAL_ARGS",&terminal_args);
        for (key, value) in &envs {
            command.env(key, value);
        }
        command.env("DSH_HOME", &dsh_home)
            .env("NEXUS_TERMINAL_READY", &ready)
            .env("NEXUS_TERMINAL_NODE", &node).env("NEXUS_TERMINAL_ENTRY", nexus_core::node_script_argument(&entry))
            .env("NEXUS_TERMINAL_NPM", nexus_core::node_script_argument(&node.parent().expect("resolved node has parent").join("node_modules/npm/bin/npm-cli.js")))
            .env("NEXUS_TERMINAL_PROFILE", &profile)
            .env("NEXUS_TERMINAL_PNPM", pnpm_pin.as_deref().unwrap_or(std::path::Path::new("")))
            .env("NEXUS_TERMINAL_PNPM_SCRIPT", if pnpm_is_script { "1" } else { "0" });
        // Give the new console its own handles, not the headless Agent's pipes.
        let mut child = match windows_terminal::spawn(&command, true) {
            Ok(child) => child, Err(error) => return data_error_response(error, "terminal_spawn_failed"),
        };
        let lease = match nexus_core::terminal_lease::register(&state.paths, &release_id, child.id()) {
            Ok(lease) => lease,
            Err(error) => { let _ = child.kill(); let _ = child.wait(); return data_error_response(error, "terminal_registration_failed"); }
        };
        if let Err(error) = nexus_core::write_private_bytes_atomic(&state.paths.root, &ready, b"ready") {
            let _ = child.kill(); let _ = child.wait(); let _ = std::fs::remove_file(&lease);
            return data_error_response(error, "terminal_registration_failed");
        }
        tokio::spawn(async move {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => { let _ = std::fs::remove_file(&lease); let _ = std::fs::remove_file(&ready); break; }
                    Err(_) => break,
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                }
            }
        });
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var_os("SHELL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/bash"));
        let _ = (&shell, &profile_dir, &envs, &dsh_home);
        // Windows is the only shipped platform in this release; opening a
        // visible terminal elsewhere needs a platform terminal emulator, and
        // spawning an invisible shell would silently do nothing.
        return data_error_response(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "DSH terminal is Windows-only in this release",
            ),
            "terminal_unsupported",
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "profile": profile,
            "profile_dir": profile_dir.to_string_lossy(),
            "release_id": release_id,
        })),
    )
        .into_response()
}

/// Open a bounded profile-related file or directory with the system handler.
/// Targets derive only from the DSH home and the validated profile name; no
/// caller-supplied path is accepted.
async fn profile_open_path(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let Some(target) = command.target.as_deref() else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "target is required"),
            "open_path_target_required",
        );
    };
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "dsh_home_unavailable"),
    };
    let profiles = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let profile = command
        .profile
        .clone()
        .unwrap_or_else(|| profiles.active_profile.clone());
    // The name becomes a path segment under the profiles root; reject
    // traversal and other unsafe characters up front.
    if let Err(error) = nexus_core::validate_profile_name(&profile) {
        return data_error_response(error, "profile_invalid");
    }
    let profile_dir = dsh_home.join("profiles").join(&profile);
    if !profile_dir.is_dir() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("profile directory not found: {}", profile_dir.display()),
            ),
            "profile_dir_missing",
        );
    }
    let (path, open_dir) = match target {
        "settings" => (dsh_home.join("settings.yaml"), false),
        "profile_dir" => (profile_dir.clone(), true),
        "profile_patch" => (profile_dir.join("cordis.patch.yml"), false),
        "plugin_manifest" => (profile_dir.join("package.json"), false),
        other => {
            return data_error_response(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown open target: {other}"),
                ),
                "open_path_target_invalid",
            );
        }
    };
    if !open_dir && !path.exists() {
        return data_error_response(
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("path not found: {}", path.display()),
            ),
            "open_path_missing",
        );
    }
    #[cfg(windows)]
    let opened = {
        use std::os::windows::process::CommandExt;
        // `explorer` opens directories in a window; for files it selects them
        // in the parent. `start` opens files with the default association.
        let output = if open_dir {
            std::process::Command::new("explorer")
                .arg(path.as_os_str())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .output()
        } else {
            std::process::Command::new("cmd")
                .args(["/C", "start", ""])
                .arg(path.as_os_str())
                .creation_flags(0x0800_0000)
                .output()
        };
        output.map(|out| out.status.success()).unwrap_or(false)
    };
    #[cfg(not(windows))]
    let opened = {
        std::process::Command::new("xdg-open")
            .arg(path.as_os_str())
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    };
    if !opened {
        return data_error_response(
            io::Error::other("the system handler did not accept the path"),
            "open_path_failed",
        );
    }
    (
        StatusCode::OK,
        Json(ProfileOpenPathResponse::new(
            target.to_owned(),
            path.to_string_lossy().into_owned(),
        )),
    )
        .into_response()
}

async fn profile_plugin_move(state: AppState, command: ProfileCommand) -> axum::response::Response {
    let Some(profile) = command.profile else { return data_error_response(io::Error::other("Profile is required"), "plugin_move_invalid"); };
    let undo = command.action == ProfileAction::PluginUndoMove;
    if (!undo && command.package.is_none()) || (undo && (command.target.is_none() || command.package.is_some())) {
        return data_error_response(io::Error::other("Reorder requires package; undo requires its saved operation ID"), "plugin_move_invalid");
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let update = match state.updater.try_acquire_gate() { Ok(gate) => gate, Err(error) => return update_error_response(error) };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let snapshots = match state.snapshots.try_acquire_configuration() { Ok(gate) => gate, Err(error) => return data_error_response(error, "plugin_move_conflict") };
    if let Err(response) = ensure_harness_selection_quiescent(&state, &lifecycle,
        "plugin_move_conflict", "stop Harness before changing plugin load order").await { return response; }
    let home = match state.snapshots.configured_dsh_home() { Ok(home) => home, Err(error) => return data_error_response(error, "profile_catalog_unavailable") };
    let paths=state.paths.clone();
    let result=tokio::task::spawn_blocking(move || {
        let _owners=(lifecycle,update,snapshots);
        if undo { dsh::undo_profile_order(&paths,&home,&profile,command.target.as_deref().unwrap()) }
        else { dsh::move_profile_plugin(&paths,&home,&profile,command.package.as_deref().unwrap(),command.target.as_deref()) }
    }).await.unwrap_or_else(|error|Err(io::Error::other(error.to_string())));
    match result { Ok(inventory) => (StatusCode::OK, Json(inventory)).into_response(), Err(error) => data_error_response(error, "plugin_move_failed") }
}

async fn profile_plugin_remove(
    state: AppState,
    command: ProfileCommand,
) -> axum::response::Response {
    let Some(profile) = command.profile else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "profile is required"),
            "plugin_remove_invalid",
        );
    };
    let Some(package) = command.package else {
        return data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "package is required"),
            "plugin_remove_invalid",
        );
    };
    let catalog = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    if catalog.active_profile != profile {
        return api_error_response(
            StatusCode::CONFLICT,
            "plugin_profile_conflict",
            "plugins can be removed only from the selected profile",
        );
    }
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "plugin_remove_conflict",
        "cannot remove a plugin until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let release_id = match state.releases.load() {
        Ok(catalog) => match catalog.current_release {
            Some(id) => id,
            None => {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "no verified DSH release is selected",
                    ),
                    "plugin_cli_unavailable",
                )
            }
        },
        Err(error) => return data_error_response(error, "release_catalog_unavailable"),
    };
    let release_root = match state.releases.release_root(&release_id) {
        Ok(root) => root,
        Err(error) => return data_error_response(error, "plugin_cli_unavailable"),
    };
    let paths = state.paths.clone();
    let dsh_home = match state.snapshots.configured_dsh_home() {
        Ok(home) => home.clone(),
        Err(error) => return data_error_response(error, "profile_catalog_unavailable"),
    };
    let owner = tokio::spawn(async move {
        let _lifecycle = lifecycle;
        let _update_gate = update_gate;
        tokio::task::spawn_blocking(move || {
            dsh::remove_profile_plugin(&paths, &dsh_home, &release_root, &profile, &package).map(
                |(outcome, inventory)| {
                    let removed = outcome.exit_code == Some(0)
                        && !inventory.plugins.iter().any(|item| item.package == package);
                    PluginRemoveResponse {
                        api_version: nexus_protocol::API_VERSION.to_owned(),
                        profile,
                        package,
                        removed,
                        exit_code: outcome.exit_code,
                        stdout: outcome.stdout,
                        stderr: outcome.stderr,
                        inventory,
                    }
                },
            )
        })
        .await
        .map_err(|error| io::Error::other(format!("plugin removal task failed: {error}")))?
    });
    match owner.await {
        Ok(Ok(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(Err(error)) => data_error_response(error, "plugin_remove_failed"),
        Err(error) => data_error_response(
            io::Error::other(format!("plugin removal owner failed: {error}")),
            "plugin_remove_failed",
        ),
    }
}

async fn checkpoint_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
    let checkpoints = match state.checkpoints.list() {
        Ok(checkpoints) => checkpoints,
        Err(error) => return data_error_response(error, "checkpoint_list_failed"),
    };
    let profile = match state.profiles.load() {
        Ok(catalog) => catalog.active_profile,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let last_capture = state.snapshots.last_capture();
    let inventory_refresh_pending = last_capture.get("state").and_then(serde_json::Value::as_str) == Some("running");
    let mut diagnostic = state.snapshots.healthy_error();
    let snapshots = if inventory_refresh_pending { Vec::new() } else { match state.snapshots.list_inspections(profile).await {
        Ok(snapshots) => snapshots,
        Err(error) => {
            if diagnostic.is_none() {
                diagnostic = Some(error.to_string());
            }
            Vec::new()
        }
    }};
    let pending_restore = match state.checkpoint_restores.load() {
        Ok(Some(journal)) => snapshots::restore_status(&journal, None),
        Ok(None) => None,
        Err(error) => return data_error_response(error, "checkpoint_restore_journal_failed"),
    };
    (
        StatusCode::OK,
        Json(
            CheckpointListResponse::new(checkpoints).with_snapshot_state(
                snapshots,
                pending_restore,
                diagnostic,
            ).with_last_capture(last_capture),
        ),
    )
        .into_response()
}

async fn ensure_checkpoint_mutation_ready(
    state: &AppState,
) -> Result<(), axum::response::Response> {
    ensure_mutation_ready_for_owner(state, None).await
}

async fn ensure_mutation_ready_for_owner(
    state: &AppState,
    cold_owner: Option<&str>,
) -> Result<(), axum::response::Response> {
    if let Err(error) = canary::ensure_idle(&state.paths) { return Err(data_error_response(error, "canary_pending")); }

    if state.cold.publication_pending() {
        return Err(api_error_response(
            StatusCode::CONFLICT,
            "cold_publication_pending",
            "Cold publication recovery is pending; use Retry recovery or Keep current and end recovery in Updates",
        ));
    }
    let owns_publication = match cold_owner {
        Some(id) => match state.cold.owns_verifying_publication(id).await {
            Ok(true) => true,
            Ok(false) => return Err(api_error_response(StatusCode::CONFLICT, "cold_install_owner_conflict",
                "The cold installation is no longer the active verifying owner; publication was stopped")),
            Err(error) => return Err(data_error_response(error, "cold_operation_unavailable")),
        },
        None => false,
    };
    match if owns_publication { Ok(false) } else { state.cold.cleanup_pending() } {
        Ok(true) => {
            return Err(api_error_response(
                StatusCode::CONFLICT,
                "cold_cleanup_pending",
                "cold cleanup is pending; retry cancel or restart the Agent",
            ));
        }
        Ok(false) => {}
        Err(error) => return Err(data_error_response(error, "cold_operation_unavailable")),
    }
    if let Err(error) = settle_checkpoint_restore(state).await {
        return Err(data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        ));
    }
    let store = state.checkpoint_restores.clone();
    let pending = tokio::task::spawn_blocking(move || store.load())
        .await
        .map_err(|error| {
            data_error_response(
                io::Error::other(format!("checkpoint journal task failed: {error}")),
                "checkpoint_restore_journal_failed",
            )
        })?
        .map_err(|error| data_error_response(error, "checkpoint_restore_journal_failed"))?;
    if pending.is_some_and(|journal| journal.intent.snapshot.is_some()) {
        return Err(api_error_response(
            StatusCode::CONFLICT,
            "checkpoint_restore_pending",
            "a content restore is pending; use checkpoint retry or checkpoint abort",
        ));
    }
    Ok(())
}

async fn checkpoint_control(
    State(state): State<AppState>,
    Json(command): Json<CheckpointCommand>,
) -> axum::response::Response {
    match command.action {
        CheckpointAction::List => checkpoint_list(State(state)).await,
        CheckpointAction::Create => checkpoint_create(state, command.note).await,
        CheckpointAction::Detail | CheckpointAction::Inspect => {
            let Some(id) = command.id else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "snapshot or checkpoint id is required",
                    ),
                    "checkpoint_invalid",
                );
            };
            checkpoint_snapshot_read(state, id, command.action).await
        }
        CheckpointAction::Restore => {
            let Some(id) = command.id else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "checkpoint id is required"),
                    "checkpoint_invalid",
                );
            };
            checkpoint_restore(state, id).await
        }
        CheckpointAction::Retry => checkpoint_restore_retry(state, command.id).await,
        CheckpointAction::Abort => checkpoint_restore_abort(state, command.id).await,
    }
}

async fn checkpoint_create(state: AppState, note: Option<String>) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_create_conflict",
        "cannot create a checkpoint until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let current = state.runtime.read().await.clone();
    let profile = current
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let state_snapshot = NexusStateSnapshot {
        profile: profile.clone(),
        release: current.release.clone(),
    };
    let version = selected_dsh_version(&state.releases, current.release.as_deref());
    let owner_state = state.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = async {
            let (lease, capture_id) = owner_state.snapshots.acquire_capture(profile.clone(), "manual").await?;
            let result = async {
            let manifest = lease.capture_manual(version, note.clone()).await?;
            owner_state.checkpoints.create_with_snapshot(
                &profile,
                current.release,
                note,
                state_snapshot,
                Some(snapshots::snapshot_reference(&manifest)),
            )
            }.await;
            owner_state.snapshots.finish_capture(&capture_id, &result);
            result
        }
        .await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(checkpoint)) => (
            StatusCode::CREATED,
            Json(CheckpointCreateResponse::from_manifest(checkpoint)),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_create_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint capture owner exited without a result"),
            "checkpoint_create_failed",
        ),
    }
}

fn selected_dsh_version(releases: &ReleaseStore, release: Option<&str>) -> String {
    release
        .and_then(|id| releases.get(id).ok())
        .map(|manifest| manifest.version)
        .or_else(|| release.map(ToOwned::to_owned))
        .unwrap_or_else(|| "unmanaged".to_owned())
}

async fn checkpoint_snapshot_read(
    state: AppState,
    id: String,
    action: CheckpointAction,
) -> axum::response::Response {
    let (profile, snapshot_id) = match state.checkpoints.read(&id) {
        Ok(Some(checkpoint)) => match checkpoint.snapshot {
            Some(reference) => (checkpoint.profile, reference.snapshot_id),
            None => {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "checkpoint_legacy_metadata_only",
                    "this legacy checkpoint has no content snapshot",
                )
            }
        },
        Ok(None) => match state.profiles.load() {
            Ok(catalog) => (catalog.active_profile, id),
            Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
        },
        Err(error) => return data_error_response(error, "checkpoint_invalid"),
    };
    match action {
        CheckpointAction::Detail => match state.snapshots.detail(profile, snapshot_id).await {
            Ok(detail) => (StatusCode::OK, Json(detail)).into_response(),
            Err(error) => data_error_response(error, "snapshot_detail_failed"),
        },
        CheckpointAction::Inspect => match state.snapshots.inspect(profile, snapshot_id).await {
            Ok(inspection) => (StatusCode::OK, Json(inspection)).into_response(),
            Err(error) => data_error_response(error, "snapshot_inspect_failed"),
        },
        _ => data_error_response(
            io::Error::new(io::ErrorKind::InvalidInput, "invalid snapshot read action"),
            "checkpoint_invalid",
        ),
    }
}

/// Resolve a restore request whose id names a healthy-start snapshot rather
/// than a checkpoint: synthesize a checkpoint that references the snapshot so
/// the standard two-phase restore and materialization apply unchanged. The
/// snapshot's harness version must still be an installed release slot.
async fn checkpoint_from_snapshot(
    state: &AppState,
    snapshot_id: &str,
) -> Result<CheckpointManifest, axum::response::Response> {
    let profiles = state
        .profiles
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_profile_invalid"))?;
    let profile = profiles.active_profile.clone();
    let detail = state
        .snapshots
        .detail(profile.clone(), snapshot_id.to_owned())
        .await
        .map_err(|error| data_error_response(error, "snapshot_not_found"))?;
    let summary = &detail.summary;
    let snapshot_profile = summary.profile_name.clone();
    if snapshot_profile != profile {
        return Err(data_error_response(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("snapshot belongs to profile {snapshot_profile}"),
            ),
            "snapshot_profile_mismatch",
        ));
    }
    let dsh_version = summary.dsh_version.clone();
    let releases = state
        .releases
        .load()
        .map_err(|error| data_error_response(error, "checkpoint_release_unavailable"))?;
    let release = releases
        .releases
        .iter()
        .find(|item| item.version == dsh_version)
        .map(|item| item.id.clone())
        .ok_or_else(|| {
            data_error_response(
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "the snapshot's harness version {dsh_version} is not installed; cold-switch to it first"
                    ),
                ),
                "snapshot_release_not_installed",
            )
        })?;
    let state_snapshot = NexusStateSnapshot {
        profile: profile.clone(),
        release: Some(release.clone()),
    };
    let reference = SnapshotReference {
        snapshot_id: snapshot_id.to_owned(),
        summary: detail.summary.clone(),
    };
    state
        .checkpoints
        .create_with_snapshot(
            &profile,
            Some(release),
            Some(format!("restored from snapshot {snapshot_id}")),
            state_snapshot,
            Some(reference),
        )
        .map_err(|error| data_error_response(error, "snapshot_checkpoint_failed"))
}

async fn checkpoint_restore(state: AppState, id: String) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
        return response;
    }
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot restore a checkpoint until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let checkpoint = match state.checkpoints.restore(&id) {
        Ok(checkpoint) => checkpoint,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // The id may name a healthy-start snapshot: synthesize a
            // checkpoint referencing it so the standard two-phase restore
            // and materialization apply unchanged.
            match checkpoint_from_snapshot(&state, &id).await {
                Ok(checkpoint) => checkpoint,
                Err(response) => return response,
            }
        }
        Err(error) => {
            let code = if error.kind() == io::ErrorKind::NotFound {
                "checkpoint_not_found"
            } else {
                "checkpoint_restore_failed"
            };
            return data_error_response(error, code);
        }
    };
    let previous_profiles = match state.profiles.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let target_profiles = match ProfileCatalog::new(
        checkpoint.profile.clone(),
        previous_profiles.profiles.clone(),
    ) {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let previous_releases = match state.releases.load() {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "checkpoint_release_unavailable"),
    };
    let target_releases = match state
        .releases
        .plan_checkpoint_release(checkpoint.release.as_deref())
    {
        Ok(catalog) => catalog,
        Err(error) => {
            let code = if error.kind() == io::ErrorKind::NotFound {
                "checkpoint_release_not_found"
            } else {
                "checkpoint_release_unavailable"
            };
            return data_error_response(error, code);
        }
    };
    let mut intent = CheckpointRestoreIntent {
        checkpoint_id: checkpoint.id.clone(),
        previous_profiles,
        previous_current_release: previous_releases.current_release,
        previous_last_known_good: previous_releases.last_known_good,
        target_profiles,
        target_current_release: target_releases.current_release,
        target_last_known_good: target_releases.last_known_good,
        snapshot: None,
    };
    if let Some(reference) = checkpoint.snapshot.as_ref() {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let owner_state = state.clone();
        let checkpoint_for_owner = checkpoint.clone();
        let snapshot_id = reference.snapshot_id.clone();
        tokio::spawn(async move {
            let result = complete_content_checkpoint_restore(
                owner_state,
                &mut intent,
                checkpoint_for_owner,
                snapshot_id,
            )
            .await;
            drop(update_gate);
            drop(lifecycle);
            let _ = result_tx.send(result);
        });
        return match result_rx.await {
            Ok(Ok((checkpoint, None))) => (
                StatusCode::OK,
                Json(CheckpointRestoreResponse::content(
                    checkpoint,
                    true,
                    CheckpointContentState::Committed,
                    None,
                )),
            )
                .into_response(),
            Ok(Ok((checkpoint, Some(status)))) => (
                StatusCode::ACCEPTED,
                Json(CheckpointRestoreResponse::content(
                    checkpoint,
                    false,
                    status.state.clone(),
                    Some(status),
                )),
            )
                .into_response(),
            Ok(Err(error)) => data_error_response(error, "checkpoint_restore_failed"),
            Err(_) => data_error_response(
                io::Error::other("checkpoint restore owner exited without a result"),
                "checkpoint_restore_failed",
            ),
        };
    }
    if let Err(error) = state.checkpoint_restores.begin(intent.clone()) {
        return data_error_response(error, "checkpoint_restore_journal_failed");
    }

    // Once Prepared is durable, an independent owner holds the supervisor
    // lifecycle gate through commit or rollback. Cancelling the HTTP request
    // cannot abandon a mixed Harness selection.
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = complete_checkpoint_restore(owner_state, intent).await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::restored(checkpoint)),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_state_persistence_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint restore owner exited without a result"),
            "checkpoint_state_persistence_failed",
        ),
    }
}

async fn complete_content_checkpoint_restore(
    state: AppState,
    intent: &mut CheckpointRestoreIntent,
    checkpoint: nexus_protocol::CheckpointManifest,
    snapshot_id: String,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let lease = state
        .snapshots
        .acquire(intent.target_profiles.active_profile.clone())
        .await?;
    let ticket = lease.prepare(snapshot_id).await?;
    intent.snapshot = Some(snapshots::binding_for(&lease, ticket.clone()));
    if let Err(error) = state.checkpoint_restores.begin(intent.clone()) {
        let rollback = lease.rollback(ticket).await;
        return Err(checkpoint_transaction_error(error, rollback.map(|_| ())));
    }

    let outcome = match lease.apply(ticket.clone()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return pending_content_restore(&state, intent, checkpoint, error, None);
        }
    };
    let outcome = if outcome.materialization_pending {
        match run_profile_materialization(&state, &lease, &ticket).await {
            Ok(()) => match lease.mark_materialized(ticket.clone()).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    return pending_content_restore(
                        &state,
                        intent,
                        checkpoint,
                        error,
                        Some(&outcome),
                    );
                }
            },
            Err(error) => {
                return pending_content_restore(&state, intent, checkpoint, error, Some(&outcome));
            }
        }
    } else {
        outcome
    };

    if let Err(primary) = apply_checkpoint_target_selection(&state, intent).await {
        let content_rollback = lease.rollback(ticket.clone()).await.map(|_| ());
        let selection_rollback = rollback_checkpoint_selection(&state, intent).await;
        let rollback = combine_results(content_rollback, selection_rollback).and_then(|()| {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, intent)
        });
        return Err(checkpoint_transaction_error(primary, rollback));
    }

    if let Err(primary) = mark_checkpoint_committed(&state, intent) {
        match state.checkpoint_restores.load() {
            Ok(Some(journal))
                if journal.intent == *intent
                    && journal.phase == CheckpointRestorePhase::Committed => {}
            Ok(Some(journal))
                if journal.intent == *intent
                    && journal.phase == CheckpointRestorePhase::Prepared =>
            {
                let content_rollback = lease.rollback(ticket.clone()).await.map(|_| ());
                let selection_rollback = rollback_checkpoint_selection(&state, intent).await;
                let rollback =
                    combine_results(content_rollback, selection_rollback).and_then(|()| {
                        state
                            .checkpoint_restores
                            .clear(CheckpointRestorePhase::Prepared, intent)
                    });
                return Err(checkpoint_transaction_error(primary, rollback));
            }
            Ok(_) => return Err(primary),
            Err(inspection) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }

    match lease.commit(ticket).await {
        Ok(_) => {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Committed, intent)?;
            Ok((checkpoint, None))
        }
        Err(error) => pending_content_restore(&state, intent, checkpoint, error, Some(&outcome)),
    }
}

fn pending_content_restore(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
    checkpoint: nexus_protocol::CheckpointManifest,
    error: io::Error,
    outcome: Option<&nexus_snapshots::RestoreOutcome>,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let journal = state.checkpoint_restores.load()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "content restore failed after its outer journal disappeared",
        )
    })?;
    let diagnostic = bounded_checkpoint_diagnostic(&error);
    state
        .checkpoint_restores
        .record_error(journal.phase, intent, diagnostic)?;
    let journal = state
        .checkpoint_restores
        .load()?
        .ok_or_else(|| io::Error::other("content restore journal disappeared"))?;
    Ok((checkpoint, snapshots::restore_status(&journal, outcome)))
}

async fn run_profile_materialization(
    state: &AppState,
    lease: &snapshots::SnapshotLease,
    ticket: &nexus_snapshots::RestoreTicket,
) -> io::Result<()> {
    let paths = state.paths.clone();
    let dsh_home = lease.store().dsh_home().to_path_buf();
    let profile = ticket.profile_name.clone();
    tokio::task::spawn_blocking(move || dsh::materialize_profile(&paths, &dsh_home, &profile))
        .await
        .map_err(|error| io::Error::other(format!("materialization owner failed: {error}")))?
}

async fn apply_checkpoint_target_selection(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    state.releases.restore_release_pointers(
        intent.target_current_release.as_deref(),
        intent.target_last_known_good.as_deref(),
    )?;
    state.profiles.write(&intent.target_profiles)?;
    let profile = intent.target_profiles.active_profile.clone();
    let release = intent.target_current_release.clone();
    update_agent_state_inner(
        state,
        move |current| {
            current.set_profile(profile);
            current.set_release(release);
        },
        true,
    )
    .await
    .map(|_| ())
    .map_err(|error| io::Error::other(error.to_string()))
}

fn mark_checkpoint_committed(state: &AppState, intent: &CheckpointRestoreIntent) -> io::Result<()> {
    let result = state.checkpoint_restores.mark_committed(intent);
    #[cfg(test)]
    let result = match result {
        Ok(())
            if state
                .checkpoint_commit_result_failure
                .swap(false, std::sync::atomic::Ordering::SeqCst) =>
        {
            Err(io::Error::other(
                "injected error after durable Committed journal publication",
            ))
        }
        result => result,
    };
    result
}

async fn checkpoint_restore_retry(
    state: AppState,
    requested_id: Option<String>,
) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot retry a restore until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let journal = match load_requested_content_restore(&state, requested_id.as_deref()) {
        Ok(journal) => journal,
        Err(error) => return data_error_response(error, "checkpoint_restore_not_pending"),
    };
    let checkpoint = match state.checkpoints.get(&journal.intent.checkpoint_id) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return data_error_response(error, "checkpoint_not_found"),
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = resume_content_checkpoint_restore(owner_state, journal, checkpoint).await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    content_restore_http_result(result_rx.await)
}

async fn resume_content_checkpoint_restore(
    state: AppState,
    journal: CheckpointRestoreJournal,
    checkpoint: nexus_protocol::CheckpointManifest,
) -> io::Result<(
    nexus_protocol::CheckpointManifest,
    Option<nexus_protocol::CheckpointRestoreStatus>,
)> {
    let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy restore has no content ticket",
        )
    })?;
    let lease = state.snapshots.acquire_bound(binding).await?;
    if journal.phase == CheckpointRestorePhase::Committed {
        validate_committed_checkpoint_restore(&journal, &state.profiles, &state.releases)?;
        return match lease.commit(binding.ticket.clone()).await {
            Ok(_) => {
                state
                    .checkpoint_restores
                    .clear(CheckpointRestorePhase::Committed, &journal.intent)?;
                Ok((checkpoint, None))
            }
            Err(error) => pending_content_restore(&state, &journal.intent, checkpoint, error, None),
        };
    }

    let mut outcome = match lease.resume_apply(binding.ticket.clone()).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return pending_content_restore(&state, &journal.intent, checkpoint, error, None);
        }
    };
    if outcome.materialization_pending {
        if let Err(error) = run_profile_materialization(&state, &lease, &binding.ticket).await {
            return pending_content_restore(
                &state,
                &journal.intent,
                checkpoint,
                error,
                Some(&outcome),
            );
        }
        outcome = match lease.mark_materialized(binding.ticket.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return pending_content_restore(
                    &state,
                    &journal.intent,
                    checkpoint,
                    error,
                    Some(&outcome),
                );
            }
        };
    }
    if let Err(primary) = apply_checkpoint_target_selection(&state, &journal.intent).await {
        let content_rollback = lease.rollback(binding.ticket.clone()).await.map(|_| ());
        let selection_rollback = rollback_checkpoint_selection(&state, &journal.intent).await;
        let rollback = combine_results(content_rollback, selection_rollback).and_then(|()| {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, &journal.intent)
        });
        return Err(checkpoint_transaction_error(primary, rollback));
    }
    if let Err(primary) = mark_checkpoint_committed(&state, &journal.intent) {
        match state.checkpoint_restores.load() {
            Ok(Some(current))
                if current.intent == journal.intent
                    && current.phase == CheckpointRestorePhase::Committed => {}
            Ok(Some(current))
                if current.intent == journal.intent
                    && current.phase == CheckpointRestorePhase::Prepared =>
            {
                let content_rollback = lease.rollback(binding.ticket.clone()).await.map(|_| ());
                let selection_rollback =
                    rollback_checkpoint_selection(&state, &journal.intent).await;
                let rollback =
                    combine_results(content_rollback, selection_rollback).and_then(|()| {
                        state
                            .checkpoint_restores
                            .clear(CheckpointRestorePhase::Prepared, &journal.intent)
                    });
                return Err(checkpoint_transaction_error(primary, rollback));
            }
            Ok(_) => return Err(primary),
            Err(inspection) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }
    match lease.commit(binding.ticket.clone()).await {
        Ok(_) => {
            state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Committed, &journal.intent)?;
            Ok((checkpoint, None))
        }
        Err(error) => {
            pending_content_restore(&state, &journal.intent, checkpoint, error, Some(&outcome))
        }
    }
}

async fn checkpoint_restore_abort(
    state: AppState,
    requested_id: Option<String>,
) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let update_gate = match state.updater.try_acquire_gate() {
        Ok(gate) => gate,
        Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_harness_selection_quiescent(
        &state,
        &lifecycle,
        "checkpoint_restore_conflict",
        "cannot abort a restore until Harness is positively stopped and unowned",
    )
    .await
    {
        return response;
    }
    let journal = match load_requested_content_restore(&state, requested_id.as_deref()) {
        Ok(journal) if journal.phase == CheckpointRestorePhase::Prepared => journal,
        Ok(_) => {
            return api_error_response(
                StatusCode::CONFLICT,
                "checkpoint_restore_committed",
                "a committed content restore can only be finished with retry",
            )
        }
        Err(error) => return data_error_response(error, "checkpoint_restore_not_pending"),
    };
    let checkpoint = match state.checkpoints.get(&journal.intent.checkpoint_id) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return data_error_response(error, "checkpoint_not_found"),
    };
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let owner_state = state.clone();
    tokio::spawn(async move {
        let result = async {
            let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "legacy restore has no content ticket",
                )
            })?;
            let lease = owner_state.snapshots.acquire_bound(binding).await?;
            lease.rollback(binding.ticket.clone()).await?;
            rollback_checkpoint_selection(&owner_state, &journal.intent).await?;
            owner_state
                .checkpoint_restores
                .clear(CheckpointRestorePhase::Prepared, &journal.intent)?;
            Ok::<_, io::Error>(checkpoint)
        }
        .await;
        drop(update_gate);
        drop(lifecycle);
        let _ = result_tx.send(result);
    });
    match result_rx.await {
        Ok(Ok(checkpoint)) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                false,
                CheckpointContentState::RolledBack,
                None,
            )),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_restore_abort_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint abort owner exited without a result"),
            "checkpoint_restore_abort_failed",
        ),
    }
}

fn content_restore_http_result(
    result: Result<
        io::Result<(
            nexus_protocol::CheckpointManifest,
            Option<nexus_protocol::CheckpointRestoreStatus>,
        )>,
        tokio::sync::oneshot::error::RecvError,
    >,
) -> axum::response::Response {
    match result {
        Ok(Ok((checkpoint, None))) => (
            StatusCode::OK,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                true,
                CheckpointContentState::Committed,
                None,
            )),
        )
            .into_response(),
        Ok(Ok((checkpoint, Some(status)))) => (
            StatusCode::ACCEPTED,
            Json(CheckpointRestoreResponse::content(
                checkpoint,
                false,
                status.state.clone(),
                Some(status),
            )),
        )
            .into_response(),
        Ok(Err(error)) => data_error_response(error, "checkpoint_restore_retry_failed"),
        Err(_) => data_error_response(
            io::Error::other("checkpoint retry owner exited without a result"),
            "checkpoint_restore_retry_failed",
        ),
    }
}

fn load_requested_content_restore(
    state: &AppState,
    requested_id: Option<&str>,
) -> io::Result<CheckpointRestoreJournal> {
    let journal = state.checkpoint_restores.load()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no checkpoint restore is pending")
    })?;
    let binding = journal.intent.snapshot.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the pending legacy restore has no content retry or abort action",
        )
    })?;
    if requested_id
        .is_some_and(|id| id != journal.intent.checkpoint_id && id != binding.ticket.ticket_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "requested restore does not match the pending transaction",
        ));
    }
    Ok(journal)
}

fn bounded_checkpoint_diagnostic(error: &io::Error) -> String {
    let mut text: String = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(4096)
        .collect();
    if text.is_empty() {
        text = "checkpoint restore failed".to_owned();
    }
    text
}

fn combine_results(first: io::Result<()>, second: io::Result<()>) -> io::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(first), Ok(())) => Err(first),
        (Ok(()), Err(second)) => Err(second),
        (Err(first), Err(second)) => Err(io::Error::new(
            first.kind(),
            format!("{first}; selection rollback also failed: {second}"),
        )),
    }
}

async fn complete_checkpoint_restore(
    state: AppState,
    intent: CheckpointRestoreIntent,
) -> io::Result<()> {
    #[cfg(test)]
    state.wait_for_checkpoint_transition_gate().await;

    let target_result = (|| -> io::Result<()> {
        state.releases.restore_release_pointers(
            intent.target_current_release.as_deref(),
            intent.target_last_known_good.as_deref(),
        )?;
        state.profiles.write(&intent.target_profiles)
    })();
    let target_result = match target_result {
        Ok(()) => {
            let active_profile = intent.target_profiles.active_profile.clone();
            let current_release = intent.target_current_release.clone();
            update_agent_state_inner(
                &state,
                |current| {
                    current.set_profile(active_profile);
                    current.set_release(current_release);
                },
                true,
            )
            .await
            .map(|_| ())
            .map_err(|error| io::Error::other(error.to_string()))
        }
        Err(error) => Err(error),
    };
    if let Err(primary) = target_result {
        return Err(checkpoint_transaction_error(
            primary,
            rollback_prepared_checkpoint(&state, &intent).await,
        ));
    }
    let commit_result = state.checkpoint_restores.mark_committed(&intent);
    #[cfg(test)]
    let commit_result = match commit_result {
        Ok(())
            if state
                .checkpoint_commit_result_failure
                .swap(false, std::sync::atomic::Ordering::SeqCst) =>
        {
            Err(io::Error::other(
                "injected error after durable Committed journal publication",
            ))
        }
        result => result,
    };
    if let Err(primary) = commit_result {
        match state.checkpoint_restores.load() {
            Ok(Some(journal))
                if journal.intent == intent
                    && journal.phase == CheckpointRestorePhase::Committed =>
            {
                tracing::warn!(error = %primary, "checkpoint commit returned an error after durable Committed publication");
            }
            Ok(Some(journal))
                if journal.intent == intent
                    && journal.phase == CheckpointRestorePhase::Prepared =>
            {
                return Err(checkpoint_transaction_error(
                    primary,
                    rollback_prepared_checkpoint(&state, &intent).await,
                ));
            }
            Ok(None) => {
                return Err(checkpoint_transaction_error(
                    primary,
                    rollback_prepared_checkpoint(&state, &intent).await,
                ));
            }
            Ok(Some(_)) => {
                return Err(io::Error::new(
                    primary.kind(),
                    format!(
                        "{primary}; checkpoint journal changed while commit result was uncertain"
                    ),
                ));
            }
            Err(inspection) => {
                // The phase is unknown. Never roll back a target that may
                // already be durably Committed; leave the journal for the
                // next fail-closed startup/control recovery.
                return Err(io::Error::new(
                    primary.kind(),
                    format!("{primary}; cannot inspect checkpoint commit outcome: {inspection}"),
                ));
            }
        }
    }
    if let Err(error) = state
        .checkpoint_restores
        .clear(CheckpointRestorePhase::Committed, &intent)
    {
        tracing::warn!(error = %error, "committed checkpoint restore journal remains for validation on next control transition");
    }
    Ok(())
}

async fn rollback_prepared_checkpoint(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    rollback_checkpoint_selection(state, intent).await?;
    state
        .checkpoint_restores
        .clear(CheckpointRestorePhase::Prepared, intent)
}

async fn rollback_checkpoint_selection(
    state: &AppState,
    intent: &CheckpointRestoreIntent,
) -> io::Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = state.releases.restore_release_pointers(
        intent.previous_current_release.as_deref(),
        intent.previous_last_known_good.as_deref(),
    ) {
        failures.push(format!("release pointer rollback failed: {error}"));
    }
    if let Err(error) = state.profiles.write(&intent.previous_profiles) {
        failures.push(format!("profile rollback failed: {error}"));
    }
    let previous_profile = intent.previous_profiles.active_profile.clone();
    let previous_release = intent.previous_current_release.clone();
    if let Err(error) = update_agent_state(state, |current| {
        // The publication mutex serializes this selection rollback with an
        // Agent shutdown or Harness observation. Never replay the stale full
        // Agent snapshot captured before the restore transaction.
        current.set_profile(previous_profile);
        current.set_release(previous_release);
    })
    .await
    {
        failures.push(format!("Agent metadata rollback failed: {error}"));
    }
    if !failures.is_empty() {
        return Err(io::Error::other(failures.join("; ")));
    }
    Ok(())
}

fn checkpoint_transaction_error(primary: io::Error, rollback: io::Result<()>) -> io::Error {
    match rollback {
        Ok(()) => primary,
        Err(rollback) => io::Error::new(
            primary.kind(),
            format!("{primary}; checkpoint rollback also failed: {rollback}"),
        ),
    }
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
    let _lifecycle = match try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
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
    let external = git_worker::selected_external(&runtime).or_else(|| Some(git_worker::ExternalGit {
        program: spec.git_program.clone(), prefix: Vec::new(),
    }));
    match git_worker::list_tags(&spec.source, &state.paths.run_dir, command_timeout, external).await {
        Ok(tags) => (
            StatusCode::OK,
            Json(TagListResponse::new(spec.source.clone(), tags)),
        )
            .into_response(),
        Err(error) => data_error_response(io::Error::other(error.to_string()), "tag_list_failed"),
    }
}

async fn release_control(state: State<AppState>, command: Json<ReleaseCommand>) -> axum::response::Response {
    // Keep ownership of a potentially long compatibility check when a caller
    // disconnects. Publication and process cleanup still settle under the locks.
    if matches!(command.0.action, ReleaseAction::Promote | ReleaseAction::Rollback) {
        match tokio::spawn(release_control_inner(state, command)).await {
            Ok(response) => response,
            Err(error) => data_error_response(io::Error::other(error.to_string()), "release_owner_failed"),
        }
    } else { release_control_inner(state, command).await }
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
            let external = match state.config.load() { Ok(config) => config.external_harness.is_some(), Err(error) => return data_error_response(error, "config_read_failed") };
            if command.inspect_only {
                return match if external { Ok(None) } else { state.releases.promotion_risk_confirmation(id) } {
                    Ok(confirmation) => Json(serde_json::json!({"rollback_confirmation": confirmation})).into_response(),
                    Err(error) => data_error_response(error, "release_promote_failed"),
                };
            }
            if let Err(error) = compatibility::for_release(&state, id, true, &nexus_core::CancellationToken::default()).await {
                return data_error_response(error, "profile_compatibility_failed");
            }
            let catalog = match if external { state.releases.promote(id) } else { state.releases.promote_confirmed(id, command.rollback_confirmation.as_deref()) } {
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
            }).await.unwrap_or_else(|error| Err(io::Error::other(format!("Release cleanup owner failed: {error}"))));
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
                if let Err(error) = compatibility::for_release(&state, id, true, &nexus_core::CancellationToken::default()).await {
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
        Ok(status) => status, Err(error) => return data_error_response(error, "config_recovery_unavailable"),
    };
    response.publication_recovery = match state.cold.publication_status() {
        Ok(status) => status, Err(error) => return data_error_response(error, "publication_recovery_unavailable"),
    };
    response.install_operation = match state.updater.install_operation() {
        Ok(operation) => operation, Err(error) => return data_error_response(error, "install_operation_unavailable"),
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
            let Some(archive) = command.archive_path.as_deref() else { return api_error_response(StatusCode::BAD_REQUEST, "offline_path_required", "archive_path is required"); };
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
                return api_error_response(StatusCode::BAD_REQUEST,"offline_path_required","archive_path is required");
            };
            if let Err(response)=ensure_checkpoint_mutation_ready(&state).await {return response;}
            match cold::offline::begin(&state,command.action,archive,command.release_id.as_deref(),command.offline_contents).await {
                Ok(operation)=>{
                    tokio::spawn(cold::prepare(state.clone(),operation.operation_id.clone()));
                    (StatusCode::ACCEPTED,Json(UpdateResponse::new(state.updater.status().unwrap_or_else(|_|nexus_protocol::UpdateRuntimeInfo::idle()),None).with_operation(operation))).into_response()
                },
                Err(error)=>data_error_response(error,"offline_operation_rejected"),
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
        UpdateAction::PublicationRetry | UpdateAction::PublicationAbandon | UpdateAction::ConfigurationRetry | UpdateAction::ConfigurationAbandon => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(StatusCode::BAD_REQUEST, "cold_operation_id_required", "operation_id is required");
            };
            let configuration_only = matches!(command.action, UpdateAction::ConfigurationRetry | UpdateAction::ConfigurationAbandon);
            if configuration_only && state.cold.publication_pending() {
                return api_error_response(StatusCode::CONFLICT, "publication_recovery_pending", "Use publication recovery to resolve both pending operations together");
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await { return response; }
            match state.checkpoint_restores.load() {
                Ok(None) => {},
                Ok(Some(_)) => return api_error_response(StatusCode::CONFLICT, "checkpoint_recovery_pending", "Finish checkpoint recovery first"),
                Err(error) => return data_error_response(error, "checkpoint_recovery_unavailable"),
            }
            let update = match state.updater.try_acquire_gate() { Ok(value) => value, Err(error) => return update_error_response(error) };
            // A persisted Running update may be this interrupted cold publication.
            // The exclusive owner gate plus ordinary-install record proves quiescence.
            match state.updater.install_operation() {
                Ok(Some(operation)) if !operation.owner_quiescent || operation.cleanup_pending || operation.phase == "installing" =>
                    return api_error_response(StatusCode::CONFLICT, "install_recovery_pending", "Finish ordinary installation recovery first"),
                Ok(_) => {}, Err(error) => return data_error_response(error, "install_recovery_unavailable"),
            }
            let snapshots = match state.snapshots.try_acquire_configuration() {
                Ok(value) => value, Err(error) => return data_error_response(error, "publication_recovery_conflict"),
            };
            let cold = match state.cold.acquire_recovery().await {
                Ok(value) => value, Err(error) => return data_error_response(error, "publication_recovery_conflict"),
            };
            let coordinator = state.cold.clone();
            let config_store = state.config.clone();
            let preserve = matches!(command.action, UpdateAction::PublicationAbandon | UpdateAction::ConfigurationAbandon);
            let result = tokio::task::spawn_blocking(move || {
                let _guards = (lifecycle, update, snapshots, cold);
                if configuration_only {
                    if preserve { config_store.preserve_current_configuration(&operation_id) } else { config_store.retry_configuration(&operation_id) }
                } else { coordinator.recover_explicit(&operation_id, preserve) }
            }).await.unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
            if let Err(error) = result { return data_error_response(error, "publication_recovery_failed"); }
            let catalog = match state.releases.load() { Ok(value) => value, Err(error) => return data_error_response(error, "release_catalog_unavailable") };
            if let Err(error) = persist_release_catalog_state(&state, &catalog, false).await {
                return data_error_response(io::Error::other(error.to_string()), "release_state_persistence_failed");
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
                let result = tokio::spawn(async move { updater.cancel_install(&operation_id).await }).await
                    .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
                return match result {
                    Ok(operation) => {
                        let mut response = UpdateResponse::new(state.updater.status().unwrap_or_else(|_| nexus_protocol::UpdateRuntimeInfo::idle()), None);
                        response.install_operation = Some(operation);
                        (StatusCode::OK, Json(response)).into_response()
                    },
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
async fn diagnostics_status_with_budget(state: AppState, request: runtime::RuntimeRequestContext) -> axum::response::Response {
    match request.run_blocking_io(runtime::BlockingStage::Diagnostics, state.paths.diagnostics_dir.clone(), move || state.diagnostics.list_with_warnings()).await {
        Ok((bundles,warnings)) => {
            let mut response = serde_json::to_value(DiagnosticsResponse::new(bundles)).unwrap_or_default();
            response["log_retention"] = log_retention::status();
            response["warnings"] = serde_json::json!(warnings);
            (StatusCode::OK, Json(response)).into_response()
        },
        Err(error) => data_error_response(error, "diagnostics_list_failed"),
    }
}

fn collect_current_diagnostics(state: &AppState, note: Option<String>) -> io::Result<nexus_protocol::DiagnosticsBundle> {
    let mut context = match state.config.snapshot().and_then(|config| config_response_for_paths(&state.paths, config)) {
        Ok(config) => serde_json::to_value(config)?,
        Err(error) => serde_json::json!({"config_error": error.to_string()}),
    };
    if let Some(prompt) = context.pointer_mut("/harness_preferences/system_prompt") { *prompt = serde_json::json!("[REDACTED]"); }
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
            let result = runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, state.paths.diagnostics_dir.clone(), move || {
                command.bundle.as_deref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bundle is required"))
                .and_then(|id| state.diagnostics.open_path(id, command.file.as_deref()))
                .and_then(|path| {
                    #[cfg(windows)]
                    let mut opener = {
                        use std::os::windows::process::CommandExt;
                        // Always view collected text in an editor, never execute a log by extension.
                        let mut opener = std::process::Command::new(if command.file.is_some() { "notepad.exe" } else { "explorer.exe" });
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
            }).await;
            match result {
                Ok(path) => (StatusCode::OK, Json(serde_json::json!({ "api_version": "v1", "path": path }))).into_response(),
                Err(error) => data_error_response(error, "diagnostics_open_failed"),
            }
        }
        DiagnosticsAction::Export => {
            if command.bundle.is_some() || command.file.is_some() {
                return data_error_response(io::Error::new(io::ErrorKind::InvalidInput, "Export collects the current diagnostic context; bundle and file are not accepted"), "diagnostics_invalid");
            }
            let collected = runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, state.paths.diagnostics_dir.clone(), move || {
                let bundle = collect_current_diagnostics(&state, command.note)?;
                let path = std::path::PathBuf::from(&bundle.directory).join("export.json");
                let reveal_error = reveal_diagnostic_export(&path).err().map(|error| error.to_string());
                Ok((bundle, path, reveal_error))
            }).await;
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
            let collected = runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Diagnostics, state.paths.diagnostics_dir.clone(), move || collect_current_diagnostics(&state, command.note)).await;
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
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).creation_flags(0x0800_0000).spawn()?;
    }
    #[cfg(target_os = "macos")] {
        std::process::Command::new("open").arg("-R").arg(path).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))] {
        std::process::Command::new("xdg-open").arg(path.parent().ok_or_else(|| io::Error::other("Diagnostic export has no parent"))?).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
static RELEASE_REMOVE_TEST_GATE: std::sync::Mutex<Option<(String, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = std::sync::Mutex::new(None);
#[cfg(test)]
fn wait_release_remove_test_gate(id: &str) {
    let gate = {
        let mut stored = RELEASE_REMOVE_TEST_GATE.lock().unwrap();
        if stored.as_ref().is_some_and(|entry| entry.0 == id) { stored.take() } else { None }
    };
    if let Some((_, entered, release)) = gate {
        entered.send(()).unwrap();
        release.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    }
}

async fn config_status(State(state): State<AppState>) -> axum::response::Response {
    match state.config.snapshot() {
        Ok(document) => match config_response_for_paths(&state.paths, document) {
            Ok(mut response) => {
                let next = crate::launch_inputs::next(&state.paths).ok();
                let current = state.supervisor.launch_input_identity().and_then(|(generation, status, session)|
                    crate::launch_inputs::current(&state.paths, generation, status, &session));
                response.launch_inputs = Some(serde_json::json!({ "next_launch": next, "running_launch": current }));
                (StatusCode::OK, Json(response)).into_response()
            },
            Err(error) => data_error_response(error, "config_unavailable"),
        },
        Err(error) => {
            let code = if nexus_core::is_config_transaction_error(&error) { "config_recovery_pending" } else { "config_unavailable" };
            data_error_response(error, code)
        },
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
    #[serde(default)] retention_days: Option<u32>,
    #[serde(default)] preview_id: Option<String>,
    #[serde(default)] item_ids: Vec<String>,
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum MaintenancePreviewPhase { #[default] Idle, Running, Completed, Failed }

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
        scan.state = if error.is_some() { MaintenancePreviewPhase::Failed } else { MaintenancePreviewPhase::Completed };
        scan.error = error;
        scan.wait_message = None;
    }
}
impl Drop for MaintenancePreviewWorker {
    fn drop(&mut self) {
        if self.1 { return; }
        let mut scan = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(scan.state, MaintenancePreviewPhase::Running) {
            scan.state = MaintenancePreviewPhase::Failed;
            scan.error = Some("Cleanup preview worker stopped before saving a result".into());
        }
    }
}

fn maintenance_preview_snapshot(state: &AppState) -> MaintenancePreviewScan {
    state.maintenance_preview.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn maintenance_response(status: StatusCode, saved: Option<nexus_core::maintenance::MaintenanceStatus>, scan: MaintenancePreviewScan) -> axum::response::Response {
    (status, Json(serde_json::json!({
        "preview": saved.as_ref().and_then(|s| s.preview.as_ref()),
        "result": saved.as_ref().and_then(|s| s.result.as_ref()),
        "preview_scan": scan,
    }))).into_response()
}

async fn maintenance_status(State(state): State<AppState>) -> axum::response::Response {
    let scan = maintenance_preview_snapshot(&state);
    if matches!(scan.state, MaintenancePreviewPhase::Running) {
        return maintenance_response(StatusCode::OK, None, scan);
    }
    let paths = state.paths.clone();
    match runtime::RuntimeRequestContext::production().run_blocking_io(runtime::BlockingStage::Maintenance, paths.run_dir.clone(), move || nexus_core::maintenance::MaintenanceStore::new(paths).status()).await {
        Ok(status) => maintenance_response(StatusCode::OK, Some(status), maintenance_preview_snapshot(&state)),
        Err(error) => data_error_response(error, "maintenance_status_failed"),
    }
}

async fn maintenance_preview_with_budget(state: AppState, retention_days: u32, request: runtime::RuntimeRequestContext) -> axum::response::Response {
    if !(1..=3650).contains(&retention_days) {
        return api_error_response(StatusCode::BAD_REQUEST, "maintenance_invalid_retention", "Retention must be between 1 and 3650 days");
    }
    {
        let mut scan = state.maintenance_preview.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(scan.state, MaintenancePreviewPhase::Running) {
            return maintenance_response(StatusCode::ACCEPTED, None, scan.clone());
        }
        *scan = MaintenancePreviewScan { state: MaintenancePreviewPhase::Running, operation_id: nexus_core::new_instance_id(), retention_days, error: None, wait_message: None };
    }
    let paths = state.paths.clone();
    let mut worker = MaintenancePreviewWorker(state.maintenance_preview.clone(), false);
    let result = request.run_blocking_io(runtime::BlockingStage::Maintenance, paths.root.clone(), move || {
        let result = (|| {
            let protected_logs = nexus_core::HarnessLogSessionStore::new(paths.clone()).read()?
                .map(|session| vec![session.stdout_log_name, session.stderr_log_name]).unwrap_or_default();
            nexus_core::maintenance::MaintenanceStore::new(paths).preview(retention_days, &protected_logs)
        })();
        worker.finish(result.as_ref().err().map(ToString::to_string));
        result
    }).await;
    match result {
        Ok(status) => maintenance_response(StatusCode::OK, Some(status), maintenance_preview_snapshot(&state)),
        Err(error) => {
            let mut scan = state.maintenance_preview.lock().unwrap_or_else(|e| e.into_inner());
            if error.kind() == io::ErrorKind::TimedOut && matches!(scan.state, MaintenancePreviewPhase::Running) {
                scan.wait_message = Some(error.to_string());
                maintenance_response(StatusCode::ACCEPTED, None, scan.clone())
            } else { data_error_response(error, "maintenance_preview_failed") }
        },
    }
}

async fn maintenance_dispatch(State(state): State<AppState>, Json(value): Json<serde_json::Value>) -> axum::response::Response {
    if matches!(value.get("action").and_then(|v| v.as_str()), Some("reset" | "restore_previous")) {
        return match serde_json::from_value(value) {
            Ok(request) => maintenance_control(State(state), Json(request)).await,
            Err(error) => data_error_response(io::Error::new(io::ErrorKind::InvalidInput, error), "maintenance_invalid_request"),
        };
    }
    let request: SpaceRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => return data_error_response(io::Error::new(io::ErrorKind::InvalidInput, error), "maintenance_invalid_request"),
    };
    let paths = state.paths.clone();
    if request.action == "preview" {
        return maintenance_preview_with_budget(state, request.retention_days.unwrap_or(30), runtime::RuntimeRequestContext::production()).await;
    }
    let protected_logs = match nexus_core::HarnessLogSessionStore::new(paths.clone()).read() {
        Ok(session) => session.map(|s| vec![s.stdout_log_name, s.stderr_log_name]).unwrap_or_default(),
        Err(error) => return data_error_response(error, "maintenance_logs_unavailable"),
    };
    if request.action != "cleanup" {
        return api_error_response(StatusCode::BAD_REQUEST, "maintenance_invalid_action", "Choose preview, cleanup, or reset");
    }
    let Some(preview_id) = request.preview_id else {
        return api_error_response(StatusCode::BAD_REQUEST, "maintenance_preview_required", "Create and confirm a cleanup preview first");
    };
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await { return response; }
    let update_guard = match state.updater.try_acquire_gate() {
        Ok(guard) => guard, Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let snapshot_guard = match state.snapshots.try_acquire_configuration() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    let cold_guard = match state.cold.try_acquire_maintenance() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    // The worker owns every gate until the durable result is written, even if
    // the HTTP connection closes. Cleanup never follows a client-supplied path.
    let result = tokio::task::spawn_blocking(move || {
        let _guards = (lifecycle, update_guard, snapshot_guard, cold_guard);
        nexus_core::maintenance::MaintenanceStore::new(paths).cleanup(&preview_id, &request.item_ids, &protected_logs)
    }).await;
    match result {
        Ok(Ok(status)) => (StatusCode::OK, Json(status)).into_response(),
        Ok(Err(error)) => data_error_response(error, "maintenance_cleanup_failed"),
        Err(error) => data_error_response(io::Error::other(error.to_string()), "maintenance_cleanup_failed"),
    }
}

async fn maintenance_control(
    State(state): State<AppState>,
    Json(request): Json<MaintenanceRequest>,
) -> axum::response::Response {
    let Some(expected) = request.expected_revision.filter(|value| !value.is_empty()) else {
        return api_error_response(StatusCode::PRECONDITION_REQUIRED, "config_revision_required", "Refresh configuration before maintenance; expected_revision is required");
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
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
        return response;
    }
    let update_guard = match state.updater.try_acquire_gate() {
        Ok(guard) => guard, Err(error) => return update_error_response(error),
    };
    if let Err(response) = ensure_update_idle(&state) { return response; }
    let snapshot_guard = match state.snapshots.try_acquire_configuration() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    let cold_guard = match state.cold.try_acquire_maintenance() {
        Ok(guard) => guard, Err(error) => return data_error_response(error, "maintenance_conflict"),
    };
    if restore_previous {
        if scope != "config" { return api_error_response(StatusCode::BAD_REQUEST, "maintenance_invalid_scope", "Previous configuration restore does not change the slot registry"); }
        let config = state.config.clone();
        let restored = tokio::task::spawn_blocking(move || {
            let _guards = (lifecycle, update_guard, snapshot_guard, cold_guard);
            config.restore_previous_if_revision(&expected)
        }).await;
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
        if task_scope == "slots" { nexus_core::ReleaseStore::new(paths.clone()).load()?; }
        let (_, (backup_dir, backed_up)) = config.transaction_if_revision(&expected, |document| {
            let backup = backup_reset_targets(&paths, &task_scope)?;
            *document = nexus_core::NexusConfigFile::default();
            Ok(backup)
        })?;
        let removed = clear_reset_targets(&paths, &task_scope)?;
        Ok::<_, io::Error>((backup_dir, backed_up, removed))
    }).await;
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
    let backup_dir = paths
        .diagnostics_dir
        .join(format!("reset-backup-{}", nexus_core::unix_time_nanos_for_update()));
    std::fs::create_dir_all(&backup_dir)?;
    let mut backed_up: Vec<String> = Vec::new();
    backup_file(&paths.config_file, &backup_dir, "config.json", &mut backed_up)?;
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

fn backup_file(source: &std::path::Path, backup_dir: &std::path::Path, name: &str, backed_up: &mut Vec<String>) -> io::Result<()> {
    if let Some(bytes) = nexus_core::read_regular_file_bounded(source, 4 * 1024 * 1024)? {
        nexus_core::write_private_bytes_atomic(backup_dir, &backup_dir.join(name), &bytes)?;
        backed_up.push(name.to_owned());
    }
    Ok(())
}

async fn config_control(
    State(state): State<AppState>,
    Json(command): Json<ConfigCommand>,
) -> axum::response::Response {
    if command.action != ConfigAction::Status && command.expected_revision.as_deref().is_none_or(str::is_empty) {
        return api_error_response(StatusCode::PRECONDITION_REQUIRED, "config_revision_required", "Refresh configuration before saving; expected_revision is required");
    }
    let expected = command.expected_revision.as_deref().unwrap_or("");
    match command.action {
        ConfigAction::Status => config_status(State(state)).await,
        ConfigAction::DiscardHarnessPatchPreview => {
            let id = command.patch_query.as_ref().and_then(|query| query.preview_id.as_deref()).unwrap_or("");
            runtime_patches::discard_preview(&state.paths, id);
            Json(serde_json::json!({"discarded":true})).into_response()
        }
        ConfigAction::ListHarnessPatchRefs => {
            let query = command.patch_query.unwrap_or_default();
            let Some(entry) = query.entry else { return data_error_response(io::Error::new(io::ErrorKind::InvalidInput, "Patch entry is required"), "patch_query_invalid"); };
            match runtime_patches::list_refs(&entry, query.page.unwrap_or(1)).await {
                Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                Err(error) => data_error_response(error, "patch_refs_failed"),
            }
        }
        ConfigAction::SetExternalHarness | ConfigAction::ClearExternalHarness => {
            let lifecycle=state.supervisor.acquire_lifecycle().await;
            if let Err(response)=ensure_checkpoint_mutation_ready(&state).await {return response;}
            if let Err(response)=ensure_harness_stopped(&state,&lifecycle).await {return response;}
            let _update=match state.updater.try_acquire_gate(){Ok(g)=>g,Err(e)=>return update_error_response(e)};
            let _configuration=match state.snapshots.try_acquire_configuration(){Ok(g)=>g,Err(e)=>return data_error_response(e,"source_busy")};
            let external=if command.action==ConfigAction::SetExternalHarness {
                let Some(path)=command.external_harness_path else {return data_error_response(io::Error::other("External Harness path is required"),"source_invalid");};
                let paths=state.paths.clone();
                match tokio::task::spawn_blocking(move || nexus_core::ExternalHarness::inspect(&paths,std::path::Path::new(&path))).await.map_err(io::Error::other).and_then(|r|r) {
                    Ok(source)=>Some(source),Err(e)=>return data_error_response(e,"external_source_invalid")
                }
            }else{None};
            transact_config_response(&state,expected,move|document|{document.external_harness=external;Ok(())})
        }
        ConfigAction::SetHarness => {
            let Some(payload) = command.harness else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "harness configuration is required",
                    ),
                    "config_invalid",
                );
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let preserve = command.preserve_harness_readiness_url;
            let releases = state.releases.clone();
            transact_config_response(&state, expected, move |document| {
                let mut payload = payload;
                if preserve {
                    if let Some(existing) = document
                        .harness
                        .as_ref()
                        .and_then(|harness| harness.readiness_url.clone())
                    {
                        payload.readiness_url = Some(existing);
                    } else if env::var_os(HARNESS_READINESS_URL_ENV)
                        .is_some_and(|value| !value.is_empty())
                    {
                        payload.readiness_url = None;
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "cannot preserve a readiness URL when no existing URL is configured",
                        ));
                    }
                }
                let mut spec = HarnessLaunchSpec::from_payload(payload)?;
                supervisor::normalize_managed_launch(&mut spec, &releases)?;
                document.harness = Some(spec);
                Ok(())
            })
        }
        ConfigAction::ClearHarness => {
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            transact_config_response(&state, expected, |document| {
                document.harness = None;
                Ok(())
            })
        }
        ConfigAction::SetUpdate | ConfigAction::SetUpdateSource => {
            let source_only = command.action == ConfigAction::SetUpdateSource;
            let Some(payload) = command.update else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "update configuration is required",
                    ),
                    "config_invalid",
                );
            };
            if let Err(error) = nexus_core::validate_update_source(&payload.source) { return data_error_response(error, "config_invalid"); }
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, move |document| {
                let update = if source_only {
                    let mut current = document.update.clone().map(Ok).unwrap_or_else(|| serde_json::from_value::<UpdateSpec>(serde_json::json!({"source":payload.source})).map_err(io::Error::other))?;
                    current.source = payload.source;
                    current.validate()?;
                    current
                } else { UpdateSpec::from_payload(payload)? };
                document.set_update(Some(update));
                Ok(())
            })
        }
        ConfigAction::ClearUpdate => {
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, |document| {
                document.set_update(None);
                Ok(())
            })
        }
        ConfigAction::SetHarnessPreferences | ConfigAction::FetchHarnessPatches | ConfigAction::PreviewHarnessPatches | ConfigAction::ApplyHarnessPatchPreview => {
            let preview_id = command.patch_query.as_ref().and_then(|query| query.preview_id.as_deref());
            let payload = if command.action == ConfigAction::ApplyHarnessPatchPreview {
                match preview_id.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Patch preview ID is required"))
                    .and_then(|id| runtime_patches::preview_candidate(&state.paths, expected, id)) {
                    Ok(payload) => Some(payload), Err(error) => return data_error_response(error, "patch_preview_invalid"),
                }
            } else { command.harness_preferences };
            let Some(payload) = payload else {
                return data_error_response(io::Error::new(io::ErrorKind::InvalidInput,
                    "harness_preferences is required; use an empty object to inherit"), "config_invalid");
            };
            let mut preferences = match nexus_core::normalize_harness_preferences(payload) {
                Ok(value) => value,
                Err(error) => return data_error_response(error, "config_invalid"),
            };
            for value in [&preferences.deepseek_base_url, &preferences.search_base_url].into_iter().flatten() {
                if value.parse::<axum::http::Uri>().ok().and_then(|url| url.host().map(str::to_owned)).is_none() {
                    return data_error_response(io::Error::new(io::ErrorKind::InvalidInput, "Invalid API base URL"), "config_invalid");
                }
            }
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await { return response; }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate, Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) { return response; }
            let _snapshot_gate = match state.snapshots.try_acquire_configuration() {
                Ok(gate) => gate, Err(error) => return data_error_response(error, "config_change_conflict"),
            };
            if matches!(command.action, ConfigAction::FetchHarnessPatches | ConfigAction::PreviewHarnessPatches) {
                match state.config.snapshot() {
                    Ok(current) if current.revision == expected => {},
                    Ok(_) => return api_error_response(StatusCode::CONFLICT, "config_revision_conflict", "Configuration changed; keep your draft and reload before downloading"),
                    Err(error) => return data_error_response(error, "config_unavailable"),
                }
                if command.action == ConfigAction::PreviewHarnessPatches {
                    return match runtime_patches::preview_update(&state.paths, expected, preferences).await {
                        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                        Err(error) => data_error_response(error, "patch_preview_failed"),
                    };
                }
                preferences = match runtime_patches::fetch(&state.paths, preferences).await {
                    Ok(value) => value,
                    Err(error) => return data_error_response(error, "patch_download_failed"),
                };
            }
            if command.action == ConfigAction::ApplyHarnessPatchPreview {
                preferences = match runtime_patches::preview_candidate(&state.paths, expected, preview_id.unwrap_or("")) {
                    Ok(preferences) => preferences,
                    Err(error) => return data_error_response(error, "patch_preview_invalid"),
                };
            }
            let acknowledged = preferences.clone();
            let response = transact_config_response(&state, expected, move |document| {
                document.harness_preferences = (preferences != Default::default()).then_some(preferences);
                Ok(())
            });
            if response.status().is_success() {
                if let Some(id) = preview_id { runtime_patches::finish_preview(id); }
                if let Err(error) = runtime_patches::acknowledge_disabled(&state.paths, &acknowledged) {
                    return data_error_response(error, "patch_acknowledgement_failed");
                }
            }
            response
        }
        ConfigAction::SetRuntime | ConfigAction::ClearRuntime => {
            let runtime = if command.action == ConfigAction::SetRuntime {
                let Some(payload) = command.runtime else {
                    return data_error_response(
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "runtime configuration is required",
                        ),
                        "config_invalid",
                    );
                };
                match RuntimeConfig::from_payload(payload) {
                    Ok(runtime) => Some(runtime),
                    Err(error) => return data_error_response(error, "config_invalid"),
                }
            } else {
                None
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            transact_config_response(&state, expected, move |document| {
                document.runtime = runtime;
                Ok(())
            })
        }
        ConfigAction::SetSnapshots | ConfigAction::ClearSnapshots => {
            let snapshots = if command.action == ConfigAction::SetSnapshots {
                let Some(payload) = command.snapshots else {
                    return data_error_response(
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "snapshot configuration is required",
                        ),
                        "config_invalid",
                    );
                };
                Some(SnapshotsConfig::from_payload(payload))
            } else {
                None
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            transact_config_response(&state, expected, move |document| {
                document.snapshots = snapshots;
                Ok(())
            })
        }
    }
}

fn config_response(document: NexusConfigFile) -> ConfigResponse {
    let harness_readiness_url_redacted = document
        .harness
        .as_ref()
        .and_then(|harness| harness.readiness_url.as_ref())
        .is_some_and(|url| redact_config_url(Some(url.clone())).as_deref() != Some(url.as_str()));
    let snapshots = document.snapshots.map(|snapshots| snapshots.to_payload());
    let external_harness=document.external_harness.as_ref().map(|s|serde_json::to_value(s).expect("source serialization"));
    let mut response=ConfigResponse::new(
        document.harness.map(|harness| {
            let mut payload = harness.to_payload();
            payload.args = redact_config_args(payload.args);
            payload.readiness_url = redact_config_url(payload.readiness_url);
            payload
        }),
        document.update.map(|update| {
            let mut payload = update.to_payload();
            payload.build_args = redact_config_args(payload.build_args);
            payload.verify_args = redact_config_args(payload.verify_args);
            payload
        }),
    )
    .with_runtime(document.runtime.map(|runtime| runtime.to_payload()))
    .with_snapshots(snapshots)
    .with_harness_preferences(document.harness_preferences)
    .with_harness_readiness_url_redacted(harness_readiness_url_redacted);
    response.external_harness=external_harness;
    response
}

fn config_response_for_paths(
    _paths: &nexus_core::NexusPaths,
    snapshot: nexus_core::ConfigSnapshot,
) -> io::Result<ConfigResponse> {
    let harness_env_override = [
        HARNESS_PROGRAM_ENV,
        HARNESS_ARGS_ENV,
        HARNESS_WORKING_DIR_ENV,
        HARNESS_READINESS_URL_ENV,
        HARNESS_READINESS_TIMEOUT_ENV,
    ]
    .iter()
    .any(|key| env::var_os(key).is_some_and(|value| !value.is_empty()));
    let update_env_override = [
        UPDATE_SOURCE_ENV,
        UPDATE_REF_ENV,
        UPDATE_GIT_PROGRAM_ENV,
        UPDATE_BUILD_PROGRAM_ENV,
        UPDATE_BUILD_ARGS_ENV,
        UPDATE_VERIFY_PROGRAM_ENV,
        UPDATE_VERIFY_ARGS_ENV,
        UPDATE_TIMEOUT_ENV,
    ]
    .iter()
    .any(|key| env::var_os(key).is_some_and(|value| !value.is_empty()));

    let effective = nexus_core::effective_config_document(snapshot.document)?;
    let mut response = config_response(effective).with_environment_overrides(harness_env_override, update_env_override);
    response.revision = snapshot.revision;
    Ok(response)
}

fn redact_config_args(values: Vec<String>) -> Vec<String> {
    let mut redacted = Vec::with_capacity(values.len());
    let mut redact_next = false;
    for value in values {
        if redact_next {
            redacted.push("[REDACTED]".to_owned());
            redact_next = false;
            continue;
        }

        if let Some((name, _)) = value.split_once('=') {
            if is_sensitive_config_value(name) {
                redacted.push(format!("{name}=[REDACTED]"));
                continue;
            }
        }

        if is_sensitive_config_value(&value) {
            if let Some((name, _)) = value.split_once('=') {
                if is_sensitive_config_value(name) {
                    redacted.push(format!("{name}=[REDACTED]"));
                } else {
                    redacted.push("[REDACTED]".to_owned());
                }
            } else if value.starts_with('-') {
                redacted.push(value);
                redact_next = true;
            } else {
                redacted.push("[REDACTED]".to_owned());
            }
        } else {
            redacted.push(value);
        }
    }
    redacted
}

fn is_sensitive_config_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    // Header-style inline credentials are values even when the option name is
    // innocuous (for example, `--header=Authorization: Bearer ...`).
    if lower.contains("bearer ") || lower.contains("authorization:") || lower.contains("cookie:") {
        return true;
    }
    let key = value
        .split_once('=')
        .map_or(value, |(name, _)| name)
        .trim_start_matches('-')
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_');
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous_is_lower = false;
    for character in key.chars() {
        if character.is_ascii_uppercase() && previous_is_lower {
            normalized.push('_');
        }
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
            previous_is_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        } else {
            normalized.push('_');
            previous_is_lower = false;
        }
    }
    let segments = normalized.split('_').filter(|segment| !segment.is_empty());
    segments.clone().any(|segment| {
        matches!(
            segment,
            "password"
                | "passwd"
                | "secret"
                | "authorization"
                | "token"
                | "cookie"
                | "bearer"
                | "auth"
                | "apikey"
                | "key"
        )
    }) || ["access_token", "refresh_token", "api_key", "private_key"]
        .iter()
        .any(|marker| normalized == *marker)
}

/// Remove query/fragment credentials and userinfo before configuration is
/// returned to a UI. The on-disk config remains unchanged; this is a display
/// boundary only. Dropping the complete query is intentionally conservative:
/// a readiness URL is never a place where Nexus needs to preserve arguments.
fn redact_config_url(value: Option<String>) -> Option<String> {
    let value = value?;
    let query_start = value.find(['?', '#']).unwrap_or(value.len());
    let base = &value[..query_start];
    let Some(scheme_end) = base.find("://") else {
        return Some(base.to_owned());
    };
    let authority_start = scheme_end + 3;
    let authority_end = base[authority_start..]
        .find('/')
        .map_or(base.len(), |offset| authority_start + offset);
    let authority = &base[authority_start..authority_end];
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut sanitized = String::with_capacity(base.len());
    sanitized.push_str(&base[..authority_start]);
    sanitized.push_str(authority);
    sanitized.push_str(&base[authority_end..]);
    Some(sanitized)
}

fn transact_config_response(
    state: &AppState,
    expected: &str,
    update: impl FnOnce(&mut NexusConfigFile) -> io::Result<()>,
) -> axum::response::Response {
    match state.config.transaction_if_revision(expected, update) {
        Ok((document, ())) => match config_response_for_paths(&state.paths, document) {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(error) => data_error_response(error, "config_unavailable"),
        },
        Err(error) => data_error_response(error, "config_write_failed"),
    }
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
    if nexus_core::is_config_revision_conflict(&error) { return api_error_response(StatusCode::CONFLICT, "config_revision_conflict", &error.to_string()); }
    let status = match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy => StatusCode::CONFLICT,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error_response(status, fallback_code, error.to_string())
}

fn harness_error_response(error: HarnessSupervisorError) -> axum::response::Response {
    if let HarnessSupervisorError::Preflight(report)=&error {return (StatusCode::CONFLICT,Json(serde_json::json!({"api_version":nexus_protocol::API_VERSION,"code":"harness_preflight_blocked","message":error.to_string(),"preflight":report}))).into_response();}
    let (status, code) = match &error {
        HarnessSupervisorError::Preflight(_)=>unreachable!(),
        HarnessSupervisorError::Cancelled => (StatusCode::CONFLICT,"harness_start_cancelled"),
        HarnessSupervisorError::Busy => (StatusCode::CONFLICT,"harness_operation_busy"),
        HarnessSupervisorError::NotConfigured => {
            (StatusCode::UNPROCESSABLE_ENTITY, "harness_not_configured")
        }
        HarnessSupervisorError::RecoveryPaused => (StatusCode::CONFLICT, "harness_start_paused"),
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
        LifecycleAction::Shutdown => accept_shutdown(state, command.action).await,
    }
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
mod cors_tests {
    #[tokio::test]
    async fn api_authorization_rejects_anonymous_origin_tampering_and_replay() {
        use super::*;
        use nexus_core::agent_auth as auth;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = std::env::temp_dir().join(format!("nexus-auth-{}", auth::random_hex().unwrap()));
        let paths = nexus_core::NexusPaths::from_root(root.clone());
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        let credential = auth::AgentCredential::publish(&paths, "test-generation").unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let app = Router::new().route("/v1/config", get(move || { let counter = counter.clone(); async move { counter.fetch_add(1, Ordering::SeqCst); "ok" } }))
            .route("/v1/health", get(|| async { "health" }))
            .layer(middleware::from_fn_with_state(ApiAuthorization::new(credential.clone()), enforce_api_authorization))
            .layer(middleware::from_fn(local_console_cors));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        async fn send(address: std::net::SocketAddr, path: &str, headers: &str, body: &[u8]) -> (String, Vec<u8>) {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap(); let mut bytes = Vec::new(); socket.read_to_end(&mut bytes).await.unwrap(); let boundary = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4; (String::from_utf8(bytes[..boundary].to_vec()).unwrap(), bytes[boundary..].to_vec())
        }
        assert!(send(address, "/v1/health", "", b"").await.0.starts_with("HTTP/1.1 200"));
        assert!(send(address, "/v1/health?x=1", "", b"").await.0.starts_with("HTTP/1.1 409"));
        assert!(send(address, "/v1/config", "", b"").await.0.starts_with("HTTP/1.1 409"));
        let nonce = auth::random_hex().unwrap(); let time = auth::unix_seconds().to_string();
        let ciphertext = credential.seal_request("GET", "/v1/config", &nonce, &time, b"").unwrap();
        let signature = credential.request_signature("GET", "/v1/config", &nonce, &time, &ciphertext);
        let headers = format!("x-nexus-auth-version: 2\r\nx-nexus-data-root-id: {}\r\nx-nexus-instance-id: {}\r\n{}: {nonce}\r\n{}: {time}\r\n{}: {signature}\r\n", credential.data_root_id, credential.instance_id, auth::NONCE_HEADER, auth::TIME_HEADER, auth::SIGNATURE_HEADER);
        assert!(send(address, "/v1/config", &(headers.clone()+"Origin: https://evil.invalid\r\n"), &ciphertext).await.0.starts_with("HTTP/1.1 403"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let response = send(address, "/v1/config", &headers, &ciphertext).await;
        assert!(response.0.starts_with("HTTP/1.1 200"));
        let proof = response.0.lines().find_map(|line| line.strip_prefix("x-nexus-auth-response: ")).unwrap();
        assert!(credential.verify_response(&nonce, 200, &response.1, proof));
        assert!(send(address, "/v1/config", &headers, &ciphertext).await.0.starts_with("HTTP/1.1 401"));
        assert_eq!(credential.open_response(&nonce, 200, &response.1).unwrap(), b"ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort(); let _ = server.await;
        std::fs::remove_dir_all(root).unwrap();
    }
    use super::{
        acquire_runtime_lock, are_allowed_cors_headers, harness_ui_process_is_presentable,
        is_allowed_console_origin_for_port, is_allowed_cors_method, proxy_identity_values_match,
        recovery_log_tail, redact_config_args, redact_config_url, redact_recovery_error,
        recovery_log_payload, MAX_RECOVERY_LOG_BYTES, PROXY_DATA_ROOT_HEADER,
        PROXY_INSTANCE_HEADER,
        RecoveryStatusResponse,
    };
    use nexus_core::HarnessLogSession;
    use nexus_core::NexusPaths;
    use nexus_protocol::{HarnessResponse, HarnessRuntimeInfo, HarnessState};

    #[test]
    fn allows_only_the_configured_local_console_port() {
        assert!(is_allowed_console_origin_for_port(
            "http://127.0.0.1:3191",
            3191
        ));
        assert!(is_allowed_console_origin_for_port(
            "http://localhost:3191",
            3191
        ));
        assert!(is_allowed_console_origin_for_port(
            "http://[::1]:3191",
            3191
        ));
        assert!(!is_allowed_console_origin_for_port(
            "http://127.0.0.1:3091",
            3191
        ));
        assert!(!is_allowed_console_origin_for_port(
            "https://127.0.0.1:3191",
            3191
        ));
        assert!(!is_allowed_console_origin_for_port(
            "http://192.168.1.10:3191",
            3191
        ));
        assert!(!is_allowed_console_origin_for_port(
            "http://127.0.0.1:3191/",
            3191
        ));
        assert!(!is_allowed_console_origin_for_port(
            "http://127.0.0.1:3191@localhost",
            3191
        ));
    }

    #[test]
    fn restricts_preflight_methods_and_headers() {
        assert!(is_allowed_cors_method("GET"));
        assert!(is_allowed_cors_method("POST"));
        assert!(!is_allowed_cors_method("DELETE"));
        assert!(are_allowed_cors_headers("content-type"));
        assert!(are_allowed_cors_headers("Content-Type, accept"));
        assert!(!are_allowed_cors_headers("authorization"));
        assert!(!are_allowed_cors_headers("content-type, x-client-secret"));
    }

    #[test]
    fn redacts_token_shaped_config_values_and_readiness_credentials() {
        assert_eq!(
            redact_config_args(vec![
                "--token".to_owned(),
                "secret".to_owned(),
                "token=inline-secret".to_owned(),
                "--api-key=api-secret".to_owned(),
                "--accessToken".to_owned(),
                "camel-secret".to_owned(),
                "--header=Authorization: Bearer header-secret".to_owned(),
                "--tokenize".to_owned(),
                "safe".to_owned(),
            ]),
            vec![
                "--token".to_owned(),
                "[REDACTED]".to_owned(),
                "token=[REDACTED]".to_owned(),
                "--api-key=[REDACTED]".to_owned(),
                "--accessToken".to_owned(),
                "[REDACTED]".to_owned(),
                "[REDACTED]".to_owned(),
                "--tokenize".to_owned(),
                "safe".to_owned(),
            ]
        );
        assert_eq!(
            redact_config_url(Some(
                "http://user:password@127.0.0.1:3080/?token=secret#auth=secret".to_owned()
            )),
            Some("http://127.0.0.1:3080/".to_owned())
        );
        assert_eq!(
            redact_config_url(Some("tcp://127.0.0.1:3080?token=secret".to_owned())),
            Some("tcp://127.0.0.1:3080".to_owned())
        );
    }

    #[test]
    fn runtime_lock_is_owned_for_the_agent_lifetime() {
        let root =
            std::env::temp_dir().join(format!("nexus-agent-runtime-lock-{}", std::process::id()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let first = acquire_runtime_lock(&paths, "first").expect("first Agent owns root");
        let error = match acquire_runtime_lock(&paths, "second") {
            Ok(_) => panic!("second Agent must not own the same root"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        drop(first);
        let recovered = acquire_runtime_lock(&paths, "replacement")
            .expect("replacement owns root after first exits");
        drop(recovered);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn proxy_identity_headers_must_match_as_a_pair() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!proxy_identity_values_match(
            &headers,
            "root-a",
            "instance-a"
        ));
        headers.insert(PROXY_DATA_ROOT_HEADER, "root-a".parse().expect("header"));
        assert!(!proxy_identity_values_match(
            &headers,
            "root-a",
            "instance-a"
        ));
        headers.insert(PROXY_INSTANCE_HEADER, "instance-b".parse().expect("header"));
        assert!(!proxy_identity_values_match(
            &headers,
            "root-a",
            "instance-a"
        ));
        headers.insert(PROXY_INSTANCE_HEADER, "instance-a".parse().expect("header"));
        assert!(proxy_identity_values_match(
            &headers,
            "root-a",
            "instance-a"
        ));
    }

    #[test]
    fn recovered_pidless_ui_requires_a_fresh_log_boundary() {
        let runtime = HarnessRuntimeInfo {
            state: HarnessState::Running,
            pid: None,
            exit_code: None,
            error: None,
            started_at_unix: Some(10),
            updated_at_unix: Some(11),
        };
        let response = HarnessResponse::from_observation(
            runtime,
            2,
            "run-2".to_owned(),
            2,
            10,
            20,
            "stdout-id".to_owned(),
            "stderr-id".to_owned(),
            "stdout.log".to_owned(),
            "stderr.log".to_owned(),
            true,
        );
        let session = HarnessLogSession::new(
            "run-2".to_owned(),
            2,
            10,
            20,
            "stdout-id".to_owned(),
            "stderr-id".to_owned(),
            "stdout.log".to_owned(),
            "stderr.log".to_owned(),
            true,
            11,
        );
        assert!(harness_ui_process_is_presentable(&response, &session));

        let mut unreserved = session.clone();
        unreserved.launch_pending = false;
        assert!(!harness_ui_process_is_presentable(&response, &unreserved));

        let attached = HarnessResponse::from_observation(
            HarnessRuntimeInfo {
                pid: Some(42),
                ..response.harness.clone()
            },
            2,
            "run-2".to_owned(),
            2,
            10,
            20,
            "stdout-id".to_owned(),
            "stderr-id".to_owned(),
            "stdout.log".to_owned(),
            "stderr.log".to_owned(),
            false,
        );
        assert!(harness_ui_process_is_presentable(&attached, &unreserved));
    }

    #[test]
    fn recovery_tail_is_bounded_redacted_and_uses_fatal_prefix_as_metadata() {
        let root = std::env::temp_dir().join(format!("nexus-recovery-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = NexusPaths::from_root(root.clone());
        paths
            .ensure_directories()
            .expect("Nexus directories create");
        std::fs::write(
            paths.logs_dir.join("current.stderr.log"),
            "[fatal] boot failed\nAuthorization: Bearer DUMMY-RECOVERY-SECRET\n",
        )
        .expect("synthetic log writes");
        let (content, truncated, fatal) =
            recovery_log_tail(&paths, "current.stderr.log").expect("bounded recovery tail reads");
        assert!(fatal);
        assert!(!truncated);
        assert!(!content.contains("DUMMY-RECOVERY-SECRET"));
        assert!(content.contains("[REDACTED]"));
        std::fs::write(
            paths.logs_dir.join("current.stderr.log"),
            vec![b'x'; MAX_RECOVERY_LOG_BYTES as usize + 1024],
        )
        .expect("oversized log fixture writes");
        let (content, truncated, _) =
            recovery_log_tail(&paths, "current.stderr.log").expect("oversized tail reads");
        assert!(truncated);
        assert!(content.len() <= MAX_RECOVERY_LOG_BYTES as usize);
        assert!(recovery_log_tail(&paths, "../outside.log").is_err());
        std::fs::remove_dir_all(root).expect("fixture removes");
    }

    #[test]
    fn recovery_payload_drops_sentinel_and_preserves_utf8_bound_after_redaction() {
        let limit = MAX_RECOVERY_LOG_BYTES as usize;
        let (ascii, _) = recovery_log_payload(vec![b'x'; limit + 1], limit, true);
        assert_eq!(ascii.len(), limit);

        let (multibyte, _) = recovery_log_payload("€".repeat(limit).into_bytes(), limit, true);
        assert!(multibyte.len() <= limit);
        assert!(multibyte.is_char_boundary(multibyte.len()));

        let (invalid, _) = recovery_log_payload(vec![0xff; limit + 1], limit, true);
        assert!(invalid.len() <= limit);
        assert!(invalid.contains("binary diagnostics payload omitted"));
    }

    #[test]
    fn mixed_windows_log_keeps_stack_and_redacts_credentials() {
        let mut bytes = vec![0xce, 0xc4, 0xbc, 0xfe, b'\n'];
        bytes.extend_from_slice(b"Error: task-board ledger is already owned by process 62756\n    at HostTaskLedger.acquireLock (plugin/index.js:1985)\ntoken=do-not-expose-this-secret\n");
        let (text, _) = recovery_log_payload(bytes, 4096, false);
        assert!(text.contains("task-board ledger is already owned"));
        assert!(!text.contains("do-not-expose-this-secret"));
        assert!(!text.contains("binary diagnostics"));
    }

    #[test]
    fn recovery_response_redacts_harness_error_and_startup_error_consistently() {
        let secret = "Authorization: Bearer DUMMY-RECOVERY-SECRET";
        let startup_error = redact_recovery_error(Some(secret));
        let response = RecoveryStatusResponse {
            pause_error: None,            paused: false,
            api_version: nexus_protocol::API_VERSION.to_owned(),
            manual_entry_available: true,
            harness_stop_required: false,
            harness: HarnessRuntimeInfo {
                state: HarnessState::Stopped,
                pid: None,
                exit_code: None,
                error: startup_error.clone(),
                started_at_unix: None,
                updated_at_unix: None,
            },
            startup_error,
            fatal_prefix_observed: false,
            log_tail: Vec::new(),
            diagnostic_errors: Vec::new(),
            pending_restore: None,
        };
        let encoded = serde_json::to_string(&response).expect("recovery response serializes");
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("DUMMY-RECOVERY-SECRET"));
        assert_eq!(response.harness.error, response.startup_error);
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::{atomic::AtomicU64, Arc},
        time::Duration,
    };

    use nexus_core::{
        data_root_identity, AgentState, CheckpointRestoreIntent, CheckpointRestoreJournalStore,
        CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec, NexusConfigFile,
        NexusPaths, NexusStateSnapshot, ProfileCatalog, ProfileStore, ReleaseStore, RuntimeConfig,
        RuntimePin,
    };
    use nexus_protocol::{
        AgentLifecycleState, CheckpointCreateResponse, CheckpointRestoreResponse, HarnessState,
        ReleaseAction, ReleaseCommand, RuntimeInstallMode, RuntimeOwnership, RuntimeSource,
    };
    use axum::{extract::State, http::StatusCode, Json};
    use tokio::{
        sync::{oneshot, watch, Mutex, RwLock},
        time::{sleep, timeout},
    };

    use super::{
        checkpoint_create, checkpoint_restore, checkpoint_restore_abort, checkpoint_restore_retry,
        execute_harness_action, recover_checkpoint_restore_startup, snapshots, sync_harness_state,
        release_control, update_agent_state, AppState, CheckpointTransitionGate, HarnessSupervisor,
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
                home: Some(selected.to_string_lossy().into_owned()), port: Some(0),
                open_browser: Some(false), ..Default::default()
            }), ..Default::default()
        };
        let lease = state.snapshots.acquire("demo".into()).await.unwrap();
        let blocked = super::config_control(State(state.clone()), Json(command.clone())).await;
        assert_eq!(blocked.status(), StatusCode::CONFLICT);
        drop(lease);
        let saved = super::config_control(State(state.clone()), Json(command)).await;
        assert!(saved.status().is_success());
        assert_eq!(state.snapshots.configured_dsh_home().unwrap(), selected);
        assert_eq!(crate::dsh::resolve_dsh_home_for_paths(&state.paths).unwrap(), selected);
        assert!(!selected.exists(), "saving must not create, copy or move Harness data");
        assert_eq!(fs::read(original.join("settings.yaml")).unwrap(), original_bytes);
        let cleared = super::config_control(State(state.clone()), Json(ConfigCommand {
            expected_revision: Some(state.config.snapshot().unwrap().revision),
            action: ConfigAction::SetHarnessPreferences,
            harness_preferences: Some(HarnessPreferencesPayload { home: Some("  ".into()), ..Default::default() }),
            ..Default::default()
        })).await;
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
        let request = || Json(super::MaintenanceRequest { expected_revision: Some(state.config.snapshot().unwrap().revision), action: "reset".into(), scope: Some("config".into()) });
        let update = state.updater.try_acquire_gate().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(update);
        let snapshot = state.snapshots.try_acquire_configuration().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(snapshot);
        let cold = state.cold.try_acquire_maintenance().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(cold);
        assert_eq!(fs::read(&state.paths.config_file).unwrap(), before);
        let response = super::maintenance_control(State(state.clone()), request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let backup = std::path::PathBuf::from(value["backup_dir"].as_str().unwrap());
        assert_eq!(fs::read(backup.join("config.json")).unwrap(), before);
        assert_eq!(state.config.load().unwrap(), nexus_core::NexusConfigFile::default());
        assert!(root.join("dsh-home").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn maintenance_restores_previous_only_when_owners_are_idle_without_starting() {
        let (state, root) = content_test_state("maintenance-undo");
        let previous = state.config.load().unwrap();
        let mut current = previous.clone();
        current.harness_preferences.get_or_insert_with(Default::default).telemetry_disabled = Some(true);
        state.config.write(&current).unwrap();
        let request = || Json(super::MaintenanceRequest { expected_revision: Some(state.config.snapshot().unwrap().revision), action: "restore_previous".into(), scope: Some("config".into()) });
        let update = state.updater.try_acquire_gate().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(update);
        let snapshot = state.snapshots.try_acquire_configuration().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(snapshot);
        let cold = state.cold.try_acquire_maintenance().unwrap();
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(cold);
        assert_eq!(state.config.load().unwrap(), current);
        assert_eq!(super::maintenance_control(State(state.clone()), request()).await.status(), StatusCode::OK);
        assert_eq!(state.config.load().unwrap(), previous);
        assert!(state.supervisor.status().await.pid.is_none());
        let invalid_request=request();
        fs::write(&state.paths.config_file, b"{").unwrap();
        assert_ne!(super::maintenance_control(State(state.clone()), invalid_request).await.status(), StatusCode::OK);
        assert_eq!(fs::read(&state.paths.config_file).unwrap(), b"{");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn release_cleanup_owner_survives_http_cancellation() {
        let (state,root)=content_test_state("release-cleanup-owner");
        let id=format!("owner-{}", nexus_core::unix_time_nanos_for_update());
        let (entered_tx,entered_rx)=std::sync::mpsc::channel();
        let (release_tx,release_rx)=std::sync::mpsc::channel();
        *super::RELEASE_REMOVE_TEST_GATE.lock().unwrap()=Some((id.clone(),entered_tx,release_rx));
        let owned=state.clone();
        let task=tokio::spawn(async move {
            super::release_control(State(owned),Json(nexus_protocol::ReleaseCommand {
                action:nexus_protocol::ReleaseAction::Remove,id:Some(id),version:None,source:None,note:None,
                ..nexus_protocol::ReleaseCommand::default()
            })).await
        });
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()).await.unwrap();
        task.abort(); let _=task.await;
        assert!(state.updater.try_acquire_gate().is_err());
        assert!(state.supervisor.try_acquire_lifecycle().is_none());
        release_tx.send(()).unwrap();
        let guard=tokio::time::timeout(std::time::Duration::from_secs(10),state.supervisor.acquire_lifecycle()).await.unwrap();
        drop(guard);
        assert!(state.updater.try_acquire_gate().is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn standalone_configuration_preserve_api_keeps_external_current() {
        let (state,root)=content_test_state("configuration-preserve-api");
        let current=fs::read(&state.paths.config_file).unwrap();
        nexus_core::write_private_json_atomic(&state.paths.root,&state.paths.root.join("config-write.pending.json"),
            &serde_json::json!({"schema":1,"committed":false,"rotate":false,"old_current":null,"old_previous":null,"target":[123,125]})).unwrap();
        let status=state.config.pending_configuration_status().unwrap().unwrap();
        let id=status["operation_id"].as_str().unwrap().to_owned();
        let response=super::update_control(State(state.clone()),Json(nexus_protocol::UpdateCommand {
            action:nexus_protocol::UpdateAction::ConfigurationAbandon,operation_id:Some(id),..Default::default()
        })).await;
        assert_eq!(response.status(),StatusCode::OK);
        assert_eq!(fs::read(&state.paths.config_file).unwrap(),current);
        assert!(state.config.pending_configuration_status().unwrap().is_none());
        assert!(super::ensure_checkpoint_mutation_ready(&state).await.is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn maintenance_cleanup_uses_existing_owner_gates() {
        let (state, root) = content_test_state("cleanup-owner-gates");
        let request = || Json(serde_json::json!({"action":"cleanup", "preview_id":"test", "item_ids":["item-0"]}));
        let update = state.updater.try_acquire_gate().unwrap();
        assert_eq!(super::maintenance_dispatch(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(update);
        let snapshot = state.snapshots.try_acquire_configuration().unwrap();
        assert_eq!(super::maintenance_dispatch(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(snapshot);
        let cold = state.cold.try_acquire_maintenance().unwrap();
        assert_eq!(super::maintenance_dispatch(State(state.clone()), request()).await.status(), StatusCode::CONFLICT);
        drop(cold);
        assert_eq!(super::maintenance_status(State(state)).await.status(), StatusCode::OK);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn harness_preferences_reach_child_without_changing_launch_directory() {
        let (state, root) = content_test_state("preferences-child");
        let selected = root.join("selected-home");
        state.releases.register("verified-preferences", "0.1.2-rc.1", None, None).unwrap();
        state.releases.promote("verified-preferences").unwrap();
        let slot = state.releases.release_root("verified-preferences").unwrap();
        write_profile_file(&slot.join("package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#);
        let entry = slot.join("apps/cli/lib/bin.js");
        let capture = root.join("child-observed.json");
        write_profile_file(&entry, &format!(
            "require('node:fs').writeFileSync({}, JSON.stringify({{home:process.env.DSH_HOME, telemetry:process.env.DSH_TELEMETRY_DISABLED, args:process.argv.slice(2), cwd:process.cwd()}})); setInterval(()=>{{}},1000);",
            serde_json::to_string(&capture).unwrap()));
        let mut launch = HarnessLaunchSpec::new(executable_on_path(if cfg!(windows) { "node.exe" } else { "node" }).expect("Node fixture runtime"));
        launch.mode = nexus_protocol::HarnessLaunchMode::Node;
        launch.args = vec![entry.to_string_lossy().into_owned(), "--profile".into(), "{profile}".into()];
        launch.working_dir = Some(root.clone());
        state.config.write(&NexusConfigFile { external_harness: None,
            harness: Some(launch.clone()),
            harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(selected.to_string_lossy().into_owned()), port: Some(0),
                open_browser: Some(false), telemetry_disabled: Some(false), ..Default::default()
            }), ..Default::default()
        }).unwrap();
        state.supervisor.start_with_profile("web").await.unwrap();
        let observed = timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(bytes) = fs::read(&capture) {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) { break value; }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await;
        state.supervisor.stop().await.unwrap();
        let observed = observed.unwrap();
        assert_eq!(observed["home"], selected.to_string_lossy().as_ref());
        assert_eq!(observed["telemetry"], "");
        assert_eq!(observed["cwd"], root.to_string_lossy().as_ref());
        assert_eq!(observed["args"], serde_json::json!(["--profile", "web", "--port", "0", "--no-open"]));
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
        state.config.write(&NexusConfigFile { external_harness: None,
            harness: Some(spec),
            harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(inaccessible.to_string_lossy().into_owned()), ..Default::default()
            }),
            ..Default::default()
        }).unwrap();
        fs::remove_dir(&inaccessible).unwrap();
        fs::write(&inaccessible, "preserve").unwrap();
        let (checks, _) = crate::preflight::collect(&state, false);
        for id in ["home", "release", "entry"] {
            assert!(checks.iter().any(|check| check["id"] == id && check["status"] == "blocked"), "{id}: {checks:?}");
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
        state.config.write(&NexusConfigFile { external_harness: None,
            harness: Some(spec), ..Default::default()
        }).unwrap();
        let (checks, runtime) = crate::preflight::collect(&state, false);
        assert!(runtime.is_none());
        assert!(!checks.iter().any(|check| check["status"] == "blocked"), "{checks:?}");
        state.config.transaction(|config| {
            config.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload { telemetry_disabled: Some(false), ..Default::default() }); Ok(())
        }).unwrap();
        let (checks, _) = crate::preflight::collect(&state, false);
        assert!(checks.iter().any(|check| check["id"] == "preferences" && check["status"] == "blocked"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_mode_blocks_start_restart_and_leaves_stopped() {
        let (state, root) = content_test_state("recovery-mode");
        fs::write(state.paths.run_dir.join("harness-recovery.json"), b"{").unwrap();
        let response = super::recovery_status(State(state.clone())).await;
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(report["pause_error"].is_string());
        assert_eq!(report["paused"], true);
        let response = super::recovery_control(State(state.clone()), Json(super::RecoveryCommand { action: super::RecoveryAction::Enter })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(super::recovery_mode::paused(&state.paths).unwrap());
        let response = axum::response::IntoResponse::into_response(super::preflight::check(State(state.clone())).await);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let check: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(check["paused"], true);
        assert!(check["checks"].as_array().unwrap().iter().any(|item| item["id"] == "recovery_mode" && item["status"] == "warning"));
        assert!(matches!(state.supervisor.start().await, Err(super::HarnessSupervisorError::RecoveryPaused)));
        assert!(matches!(state.supervisor.restart().await, Err(super::HarnessSupervisorError::RecoveryPaused)));
        let response = super::recovery_control(State(state.clone()), Json(super::RecoveryCommand { action: super::RecoveryAction::Leave })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!super::recovery_mode::paused(&state.paths).unwrap());
        assert!(state.supervisor.selection_change_is_quiescent(&state.supervisor.acquire_lifecycle().await).await);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recovery_mode_selects_valid_profile_without_launch_probe_and_creates_blank_profile() {
        let (state, root) = content_test_state("recovery-select");
        let home = state.snapshots.configured_dsh_home().unwrap().clone();
        write_profile_file(&home.join("profiles/target/package.json"),
            r#"{"name":"target","dsh":{"profile":{"bundles":["plugin-one"]}}}"#);
        let mut config = state.config.load().unwrap();
        config.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload { home: Some(home.to_string_lossy().into_owned()), ..Default::default() });
        let mut harness = HarnessLaunchSpec::new(root.join("missing-runtime").join("node.exe"));
        harness.mode = nexus_protocol::HarnessLaunchMode::Node;
        harness.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
        config.harness = Some(harness);
        state.config.write(&config).unwrap();
        state.releases.register("recovery-version", "test", None, None).unwrap();
        state.releases.promote("recovery-version").unwrap();
        super::recovery_mode::set_paused(&state.paths, true).unwrap();
        for (action, name) in [(nexus_protocol::ProfileAction::Select, "target"), (nexus_protocol::ProfileAction::Create, "new-empty")] {
            let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
                action, profile: Some(name.to_owned()), package: None, target: None,
            })).await;
            assert!(response.status().is_success(), "{}", response.status());
        }
        assert_eq!(state.profiles.load().unwrap().active_profile, "target");
        assert!(!state.paths.root.join("compatibility/latest.json").exists());
        for (profile, target) in [(None, None), (Some("other"), None), (None, Some("invalid"))] {
            let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
                action: nexus_protocol::ProfileAction::CompatibilityCheck,
                profile: profile.map(str::to_owned), package: None, target: target.map(str::to_owned),
            })).await;
            assert!(!response.status().is_success(), "missing runtime/entry and invalid targets must not pass verification");
            assert!(super::recovery_mode::paused(&state.paths).unwrap());
            assert_eq!(state.profiles.load().unwrap().active_profile, "target");
        }
        // Creation must still honor the same mutation exclusion as configuration edits.
        let _snapshot = state.snapshots.try_acquire_configuration().unwrap();
        let check = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::CompatibilityCheck, profile: None, package: None, target: None,
        })).await;
        assert!(!check.status().is_success());
        let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::Create, profile: Some("blocked".to_owned()), package: None, target: None,
        })).await;
        assert!(!response.status().is_success());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn corrupt_install_journal_keeps_control_plane_and_diagnostics_available() {
        let (state, root) = content_test_state("corrupt-install-journal");
        let journal = state.paths.root.join("install-operation.json");
        fs::write(&journal, "{interrupted-invalid-record").unwrap();
        state.updater.state_store().write(&nexus_protocol::UpdateRuntimeInfo::running("interrupted".into(), 1)).unwrap();
        let recovered = state.updater.recover_unattached().unwrap();
        assert_eq!(recovered.state, nexus_protocol::UpdateState::Failed);
        let _ = super::health(State(state.clone())).await;
        assert_eq!(super::current_state(State(state.clone())).await.status(), StatusCode::OK);
        assert_eq!(super::harness_status(State(state.clone())).await.status(), StatusCode::OK);
        assert_eq!(super::recovery_status(State(state.clone())).await.status(), StatusCode::OK);
        let bundle = super::collect_current_diagnostics(&state, None).unwrap();
        assert!(bundle.files.iter().any(|file| file.name == "install-operation.json"));
        let updates = super::update_status(State(state.clone())).await;
        assert!(!updates.status().is_success());
        let body = axum::body::to_bytes(updates.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("install_operation_unavailable"));
        assert!(state.updater.install(Some("blocked".into()), Some("test".into())).await.is_err());
        let reset = super::maintenance_control(State(state.clone()), Json(super::MaintenanceRequest { expected_revision: Some(state.config.snapshot().unwrap().revision), action: "reset".into(), scope: None })).await;
        assert!(!reset.status().is_success());
        assert_eq!(fs::read_to_string(&journal).unwrap(), "{interrupted-invalid-record");
        fs::remove_file(journal).unwrap();
        assert!(state.updater.try_acquire_gate().is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn checkpoint_capture_progress_get_does_not_wait_for_capture_owner() {
        let (state, root) = content_test_state("capture-progress");
        let (lease, id) = state.snapshots.acquire_capture("demo".into(), "manual").await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(1), super::checkpoint_list(State(state.clone()))).await.expect("GET does not wait for capture owner");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["inventory_refresh_pending"], true); assert_eq!(value["last_capture"]["state"], "running");
        assert!(value["checkpoints"].is_array());
        state.snapshots.finish_capture(&id, &Ok::<_, io::Error>(())); drop(lease); drop(state); let _ = fs::remove_dir_all(root);
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
            .write(
                &ProfileCatalog::new("demo", vec!["demo".to_owned()]).expect("profile validates"),
            )
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
        maintenance_preview: Arc::new(std::sync::Mutex::new(crate::MaintenancePreviewScan::default())),
                checkpoint_transition_gate: Arc::new(Mutex::new(None)),
                agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                shutdown,
                data_root_id: data_root_identity(&paths).expect("data root identity reads"),
                instance_id: format!("content-{label}"),                crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())), canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(nexus_launcher_core::HarnessLogObserver::default())),            },
            root,
        )
    }

    #[tokio::test]
    #[ignore = "requires NEXUS_TEST_NODE_BINARY for the real startup choice flow"]
    async fn compatibility_choice_allows_failed_release_to_be_retried() {
        use crate::{compatibility, profile_control};
        use nexus_protocol::{ProfileAction, ProfileCommand};
        let node = PathBuf::from(std::env::var_os("NEXUS_TEST_NODE_BINARY").expect("explicit Node runtime"));
        let (state, root) = content_test_state("compatibility-choice");
        let home = state.snapshots.configured_dsh_home().unwrap();
        let manifest = home.join("profiles/demo/package.json");
        let original = br#"{"name":"demo","dsh":{"profile":{"bundles":["unclassified"]}}}"#;
        fs::write(&manifest, original).unwrap();
        state.releases.register("choice-old", "old", None, None).unwrap();
        state.releases.register("choice-target", "target", None, None).unwrap();
        state.releases.promote("choice-old").unwrap();
        let target = state.releases.release_root("choice-target").unwrap();
        write_profile_file(&target.join("vendor/core/package.json"), r#"{"name":"@deepseek-ai/test-core"}"#);
        write_profile_file(&target.join("apps/cli/lib/bin.js"), r#"
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const manifest = JSON.parse(fs.readFileSync(path.join(process.env.DSH_HOME, 'profiles', process.argv[3], 'package.json')));
if (manifest.dsh.profile.bundles.includes('unclassified')) {
  console.error('failed to apply loader entry fixture (unclassified): unsupported setup'); process.exit(1);
}
const server = http.createServer((req, res) => res.end('<html>ready</html>'));
server.listen(0, '127.0.0.1', () => console.log('dsh web: http://127.0.0.1:' + server.address().port + '/'));
"#);
        let mut config = state.config.load().unwrap();
        let mut spec = HarnessLaunchSpec::new(node);
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
        config.harness = Some(spec);
        state.config.write(&config).unwrap();
        let promote = ReleaseCommand { action: ReleaseAction::Promote, id: Some("choice-target".to_owned()),
            rollback_confirmation: state.releases.promotion_risk_confirmation("choice-target").unwrap(), ..ReleaseCommand::default() };
        let failed = release_control(State(state.clone()), Json(promote.clone())).await;
        assert!(!failed.status().is_success());
        assert_eq!(state.releases.load().unwrap().current_release.as_deref(), Some("choice-old"));
        let report = compatibility::latest(&state.paths).unwrap();
        assert_eq!(report.status, "needs_choice");
        assert_eq!(report.candidates[0].package, "unclassified");
        let saved = profile_control(State(state.clone()), Json(ProfileCommand {
            action: ProfileAction::PluginDisable, profile: Some("demo".to_owned()), package: Some("unclassified".to_owned()), target: None,
        })).await;
        assert!(saved.status().is_success());
        assert_eq!(compatibility::disabled_plugins(&home, "demo").unwrap(), vec!["unclassified"]);
        let retried = release_control(State(state.clone()), Json(promote)).await;
        if !retried.status().is_success() {
            let body = axum::body::to_bytes(retried.into_body(), 64 * 1024).await.unwrap();
            panic!("retry failed: {}", String::from_utf8_lossy(&body));
        }
        assert_eq!(state.releases.load().unwrap().current_release.as_deref(), Some("choice-target"));
        let report = compatibility::latest(&state.paths).unwrap();
        assert_eq!(report.status, "isolated");
        assert_eq!(report.disabled[0].reason, "Disabled by user");
        assert_eq!(fs::read(&manifest).unwrap(), original);
        let restored = profile_control(State(state.clone()), Json(ProfileCommand {
            action: ProfileAction::PluginEnable, profile: Some("demo".to_owned()), package: Some("unclassified".to_owned()), target: None,
        })).await;
        assert!(restored.status().is_success());
        assert!(compatibility::disabled_plugins(&home, "demo").unwrap().is_empty());
        assert_eq!(fs::read(&manifest).unwrap(), original);
        // Manual verification in recovery mode executes only the disposable
        // probe, including failure and a subsequent saved isolation choice.
        let mut config = state.config.load().unwrap();
        config.harness.as_mut().unwrap().args = vec!["{release_root}/apps/cli/lib/bin.js".into(), "--profile".into(), "{profile}".into()];
        state.config.write(&config).unwrap();
        super::recovery_mode::set_paused(&state.paths, true).unwrap();
        let before_profiles = state.profiles.load().unwrap();
        let before_releases = state.releases.load().unwrap();
        let check = ProfileCommand { action: ProfileAction::CompatibilityCheck, profile: None, package: None, target: None };
        assert!(!profile_control(State(state.clone()), Json(check.clone())).await.status().is_success());
        compatibility::set_plugin_disabled(&home, "demo", "unclassified", true).unwrap();
        let response = profile_control(State(state.clone()), Json(check)).await;
        if !response.status().is_success() {
            panic!("manual verification failed: {}", String::from_utf8_lossy(&axum::body::to_bytes(response.into_body(), 65536).await.unwrap()));
        }
        assert!(super::recovery_mode::paused(&state.paths).unwrap());
        assert_eq!(state.profiles.load().unwrap(), before_profiles);
        assert_eq!(state.releases.load().unwrap(), before_releases);
        assert!(state.supervisor.selection_change_is_quiescent(&state.supervisor.acquire_lifecycle().await).await);
        let report = compatibility::latest(&state.paths).unwrap();
        assert_eq!(report.trigger.as_deref(), Some("manual_check"));
        assert_eq!(report.checked_disabled_plugins, Some(vec!["unclassified".to_owned()]));
        assert!(!state.paths.root.join("compatibility/owner-pending.json").exists());
        assert_eq!(fs::read(&manifest).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_routes_fail_fast_without_settling_a_locked_checkpoint() {
        let (state, root) = content_test_state("read-lifecycle-busy");
        state.releases.register("read-old", "old", None, None).unwrap();
        state.releases.register("read-target", "target", None, None).unwrap();
        state.releases.promote("read-old").unwrap();
        let entry = state.paths.releases_dir.join("read-old/health-entry.js"); fs::write(&entry, "fixture").unwrap();
        let mut evidence = state.releases.healthy_launch_candidate("read-old", &entry, "web", "fixture-config".into()).unwrap();
        evidence.run_id = "read-old-run".into(); evidence.generation = 1; state.releases.record_healthy_release(evidence).unwrap();
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
        state.releases.restore_release_pointers(Some("read-target"), Some("read-old")).unwrap();
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
                .await.expect("read route must not queue behind lifecycle owner");
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["code"], "lifecycle_busy");
            assert!(error["message"].as_str().unwrap().starts_with("NEXUS_LIFECYCLE_BUSY:"));
        }
        assert_eq!(state.releases.load().unwrap(), partial_releases);
        assert_eq!(state.profiles.load().unwrap(), target_profiles);
        assert_eq!(serde_json::to_value(state.checkpoint_restores.load().unwrap()).unwrap(), pending);
        assert_eq!(serde_json::to_value(state.runtime.read().await.as_payload()).unwrap(), runtime);
        drop(owner);
        for route in 0..7 {
            let response = timeout(Duration::from_secs(2), read_route(state.clone(), route))
                .await.expect("read route resumes after lifecycle owner releases");
            if route == 0 { assert_eq!(response.status(), StatusCode::OK); }
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
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
        write_profile_file(&path, r#"{"name":"other","dsh":{"profile":{"bundles":["a","b"]}},"dependencies":{"a":"1","b":"1"}}"#);
        let before = state.profiles.load().unwrap();
        let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::PluginMove, profile: Some("other".to_owned()),
            package: Some("b".to_owned()), target: Some("a".to_owned()),
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(super::dsh::native_profile(&home, "other").unwrap().bundles, ["b", "a"]);
        assert_eq!(state.profiles.load().unwrap(), before);
        let id=super::dsh::order_undo_id(&state.paths,&home,"other").unwrap().unwrap();
        let request=|| Json(nexus_protocol::ProfileCommand { action:nexus_protocol::ProfileAction::PluginUndoMove,
            profile:Some("other".into()),package:None,target:Some(id.clone()) });
        let snapshot_owner=state.snapshots.try_acquire_configuration().unwrap();
        assert_eq!(super::profile_control(State(state.clone()),request()).await.status(),StatusCode::CONFLICT);
        drop(snapshot_owner);
        let update_owner=state.updater.try_acquire_gate().unwrap();
        assert_eq!(super::profile_control(State(state.clone()),request()).await.status(),StatusCode::CONFLICT);
        drop(update_owner);
        assert_eq!(super::profile_control(State(state.clone()),request()).await.status(),StatusCode::OK);
        assert_eq!(super::dsh::native_profile(&home,"other").unwrap().bundles,["a","b"]);
        assert_eq!(state.profiles.load().unwrap(),before);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn profile_selection_preflight_failure_does_not_publish_target() {
        let (state, root) = content_test_state("profile-preflight-failure");
        let home = state.snapshots.configured_dsh_home().unwrap().clone();
        write_profile_file(&home.join("profiles/target/package.json"),
            r#"{"name":"target","dsh":{"profile":{"bundles":["plugin-one","plugin-two"]}}}"#);
        state.releases.register("profile-runtime", "test", None, None).unwrap();
        state.releases.promote("profile-runtime").unwrap();
        let before = state.profiles.load().unwrap();
        let runtime_before = state.runtime.read().await.profile.clone();
        let mut config = state.config.load().unwrap();
        let mut harness = HarnessLaunchSpec::new(root.join("unused-runtime/node.exe"));
        harness.mode = nexus_protocol::HarnessLaunchMode::Node;
        harness.args = vec!["{release_root}/apps/cli/lib/bin.js".to_owned()];
        config.harness = Some(harness);
        state.config.write(&config).unwrap();
        let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
            action: nexus_protocol::ProfileAction::Select, profile: Some("target".to_owned()),
            package: None, target: None,
        })).await;
        assert!(!response.status().is_success());
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("profile_compatibility_failed"));
        assert_eq!(state.profiles.load().unwrap(), before);
        assert_eq!(state.runtime.read().await.profile, runtime_before);
        // A profile-switch report grants choices for that unselected target,
        // and consecutive choices must preserve the report and current profile.
        let report = nexus_protocol::CompatibilityReport {
            checker_version: 1, status: "needs_choice".to_owned(), source_profile: "target".to_owned(),
            effective_profile: "nexus-target".to_owned(), release_id: "profile-runtime".to_owned(),
            fingerprint: "fixture".to_owned(), checked_at_unix: 1, checked_disabled_plugins: None, trigger: Some("profile_switch".to_owned()),
            last_trigger: Some("profile_switch".to_owned()), last_used_at_unix: Some(1), cache_reused: false,
            disabled: Vec::new(), candidates: Vec::new(), error: Some("Choose plugin isolation".to_owned()),
        };
        let directory = state.paths.root.join("compatibility");
        nexus_core::write_json_atomic(&directory, &directory.join("latest.json"), &report).unwrap();
        for package in ["plugin-one", "plugin-two"] {
            let response = super::profile_control(State(state.clone()), Json(nexus_protocol::ProfileCommand {
                action: nexus_protocol::ProfileAction::PluginDisable, profile: Some("target".to_owned()),
                package: Some(package.to_owned()), target: None,
            })).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(state.profiles.load().unwrap(), before);
        assert_eq!(super::compatibility::latest(&state.paths), Some(report));
        let policy: serde_json::Value = serde_json::from_slice(&fs::read(home.join("profiles/.nexus-plugin-isolation/target.json")).unwrap()).unwrap();
        assert_eq!(policy, serde_json::json!(["plugin-one", "plugin-two"]));
        let response = super::profile_list_response(&state, before).unwrap();
        assert_eq!(response.disabled_plugins, vec!["plugin-one", "plugin-two"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn compatibility_preflight_failure_preserves_release_selection() {
        let (state, root) = content_test_state("compatibility-preflight");
        state.releases.register("compat-old", "old", None, None).unwrap();
        state.releases.register("compat-target", "target", None, None).unwrap();
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
        let response = release_control(State(state.clone()), Json(ReleaseCommand {
            action: ReleaseAction::Promote,
            id: Some("compat-target".to_owned()),
            version: None, source: None, note: None,
            ..ReleaseCommand::default()
        })).await;
        assert!(!response.status().is_success());
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("profile_compatibility_failed"));
        assert_eq!(state.releases.load().unwrap(), before);
        assert!(!latest.exists(), "a failed recheck must invalidate the previous success");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn manual_promotion_preview_binds_consent_without_changing_the_selection() {
        let (state, root) = content_test_state("promotion-preview");
        for id in ["old", "target"] { state.releases.register(id, "1", None, None).unwrap(); }
        state.releases.promote("old").unwrap();
        let before = fs::read(&state.paths.release_pointers_file).unwrap();
        let response = release_control(State(state.clone()), Json(ReleaseCommand {
            action: ReleaseAction::Promote, id: Some("target".into()), inspect_only: true, ..ReleaseCommand::default()
        })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["rollback_confirmation"].as_str(), state.releases.promotion_risk_confirmation("target").unwrap().as_deref());
        assert!(value["rollback_confirmation"].as_str().unwrap().starts_with("unprotected-promotion-"));
        assert_eq!(fs::read(&state.paths.release_pointers_file).unwrap(), before);
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
        assert!(!state
            .paths
            .releases_dir
            .join("external-slot")
            .exists());
        fs::remove_dir_all(root).expect("fixture removes");
    }

    #[tokio::test]
    async fn materialization_failure_stays_prepared_blocks_mutations_and_abort_rolls_back() {
        let (mut state, root) = content_test_state("pending-abort");
        let selected_home = state.snapshots.configured_dsh_home().unwrap();
        state.config.transaction(|document| {
            document.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(selected_home.to_string_lossy().into_owned()), ..Default::default()
            });
            Ok(())
        }).unwrap();
        state.snapshots = super::snapshots::SnapshotCoordinator::new(
            state.paths.clone(), Ok(root.join("different-default-home")));
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
            let reset = super::maintenance_control(State(state.clone()), Json(super::MaintenanceRequest {
                expected_revision: Some(state.config.snapshot().unwrap().revision),
                action: "reset".into(), scope: Some(scope.into()),
            })).await;
            assert_eq!(reset.status(), StatusCode::CONFLICT);
            assert_eq!(fs::read(&state.paths.config_file).unwrap(), config_before);
            assert_eq!(fs::read(&state.paths.release_pointers_file).ok(), pointers_before);
            assert_eq!(state.snapshots.configured_dsh_home().unwrap(), selected_home);
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
        timeout(Duration::from_secs(3), async {
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
        for id in ["old", "target", "external"] { state.releases.register(id, "1", None, None).unwrap(); }
        let public = state.releases.restore_release_pointers(Some("target"), Some("old")).unwrap();
        assert!(public.last_known_good.is_none());
        let profiles = state.profiles.load().unwrap();
        let intent = CheckpointRestoreIntent {
            checkpoint_id: "legacy-checkpoint".into(), previous_profiles: profiles.clone(), target_profiles: profiles,
            previous_current_release: Some("old".into()), previous_last_known_good: None,
            target_current_release: Some("target".into()), target_last_known_good: Some("old".into()), snapshot: None,
        };
        state.checkpoint_restores.begin(intent.clone()).unwrap();
        state.checkpoint_restores.mark_committed(&intent).unwrap();
        recover_checkpoint_restore_startup(&state.checkpoint_restores, &state.profiles, &state.releases, &state.snapshots).await.unwrap();
        assert!(state.checkpoint_restores.load().unwrap().is_none(), "legacy committed transaction settles");
        assert_eq!(state.releases.stored_release_pointers().unwrap().1.as_deref(), Some("old"));
        assert!(state.releases.load().unwrap().last_known_good.is_none(), "raw recovery evidence is not verified health");
        assert!(state.releases.rollback().is_err());
        state.checkpoint_restores.begin(intent.clone()).unwrap();
        state.checkpoint_restores.mark_committed(&intent).unwrap();
        state.releases.restore_release_pointers(Some("target"), Some("external")).unwrap();
        assert!(recover_checkpoint_restore_startup(&state.checkpoint_restores, &state.profiles, &state.releases, &state.snapshots).await.is_err());
        assert!(state.checkpoint_restores.load().unwrap().is_some(), "different unverified pointer remains an actual conflict");
        assert_recovery_agent_available(&state.paths).await;
        assert!(state.checkpoint_restores.load().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn dated_record_restore_through_real_agent_survives_restart_without_launch() {
        let (state, root) = content_test_state("dated-record-restore");
        let paths=state.paths.clone();
        let points=nexus_core::profile_history::list(&paths).unwrap();
        let point=points[0]["id"].clone();
        let mut previous_instance=String::new();
        for phase in 0..3 {
            if phase==0 { fs::write(&paths.profiles_file,b"{broken").unwrap(); }
            if phase==1 { nexus_core::ProfileStore::new(paths.clone()).select("other").unwrap(); }
            let server=tokio::spawn(super::run(nexus_core::NexusConfig {data_dir:Some(paths.root.clone()),port:0}));
            let discovery=tokio::time::timeout(Duration::from_secs(15),async {
                loop {
                    assert!(!server.is_finished());
                    if let Ok(Some(record))=paths.read_agent_discovery() { if record.instance_id!=previous_instance {break record;} }
                    sleep(Duration::from_millis(20)).await;
                }
            }).await.unwrap();
            previous_instance=discovery.instance_id.clone();
            let client=nexus_launcher_core::AgentClient::new(discovery.port).unwrap().with_expected_identity(nexus_launcher_core::AgentIdentity {data_root_id:discovery.data_root_id,instance_id:discovery.instance_id}).with_credential_paths(paths.clone());
            let health:serde_json::Value=client.get_json("/v1/health").await.unwrap();
            assert_eq!(health["degraded"]==true,phase==0);
            if phase<2 {
                let listed:serde_json::Value=client.get_json("/v1/recovery/records").await.unwrap();
                let restored:serde_json::Value=client.post_json("/v1/recovery/records",&serde_json::json!({"action":"restore","point_id":point,"expected_revision":listed["expected_revision"]})).await.unwrap();
                assert_eq!(restored["restored"],true);
                assert_eq!(restored["active_profile"],"demo");
                if phase==1 {
                    let current:serde_json::Value=client.get_json("/v1/state").await.unwrap();
                    assert_eq!(current["state"]["profile"],"demo");
                }
            }
            if phase>0 {
                assert!(super::recovery_mode::paused(&paths).unwrap());
                assert!(super::recovery_mode::ensure_start_allowed(&paths).is_err());
                assert!(client.post_json::<_,serde_json::Value>("/v1/harness",&serde_json::json!({"action":"start"})).await.is_err());
            }
            let _:serde_json::Value=client.post_empty("/v1/shutdown").await.unwrap();
            tokio::time::timeout(Duration::from_secs(10),server).await.unwrap().unwrap().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    // Exercise the real initialization, discovery, authorization and route
    // chain, not merely the recovery helper's return value.
    async fn assert_recovery_agent_available(paths: &nexus_core::NexusPaths) {
        let config = nexus_core::NexusConfig { data_dir: Some(paths.root.clone()), port: 0 };
        let server = tokio::spawn(super::run(config));
        let discovery = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                assert!(!server.is_finished(), "Agent exited before exposing recovery APIs");
                if let Ok(Some(record)) = paths.read_agent_discovery() { break record; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.expect("Agent publishes discovery");
        let client = nexus_launcher_core::AgentClient::new(discovery.port).unwrap()
            .with_expected_identity(nexus_launcher_core::AgentIdentity {
                data_root_id: discovery.data_root_id, instance_id: discovery.instance_id,
            }).with_credential_paths(paths.clone());
        let health: serde_json::Value = client.get_json("/v1/health").await.unwrap();
        let _: serde_json::Value = client.get_json("/v1/diagnostics").await.unwrap();
        if health["degraded"] == true {
            assert_eq!(health["read_only"], true);
            let recovery: serde_json::Value = client.get_json("/v1/recovery").await.unwrap();
            assert_eq!(recovery["degraded"], true);
            let records: serde_json::Value = client.get_json("/v1/recovery/records").await.unwrap();
            assert_eq!(records["records"].as_array().unwrap().len(), 5);
            let record = records["records"].as_array().unwrap().iter().find(|record| record["can_backup"] == true).unwrap();
            let backup: serde_json::Value = client.post_json("/v1/recovery/records", &serde_json::json!({"action":"backup","record_id":record["id"],"expected_revision":record["revision"]})).await.unwrap();
            assert_eq!(backup["original_unchanged"], true);
            assert!(std::path::Path::new(backup["backup_path"].as_str().unwrap()).is_file());
            for path in ["/v1/config", "/v1/maintenance", "/v1/updates", "/v1/profiles", "/v1/recovery"] {
                assert!(client.post_json::<_, serde_json::Value>(path, &serde_json::json!({"action":"reset"})).await.is_err());
            }
            let exported: serde_json::Value = client.post_json("/v1/diagnostics", &serde_json::json!({"action":"export"})).await.unwrap();
            assert!(std::path::Path::new(exported["export_path"].as_str().unwrap()).is_file());
        }
        let checked: serde_json::Value = client.get_json("/v1/preflight").await.unwrap();
        assert_eq!(checked["ready"], false);
        let error = client.post_json::<_, serde_json::Value>("/v1/harness", &serde_json::json!({"action":"start"})).await.unwrap_err();
        let nexus_launcher_core::AgentClientError::Http { body, .. } = error else { panic!("Expected structured startup rejection: {error}"); };
        let failure: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(matches!(failure["code"].as_str(), Some("agent_recovery_required" | "checkpoint_recovery_failed" | "checkpoint_restore_journal_failed" | "cold_publication_pending" | "cold_operation_unavailable" | "cold_cleanup_pending")), "{failure}");
        let _: serde_json::Value = client.post_empty("/v1/shutdown").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), server).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn startup_recovery_errors_keep_real_agent_api_available() {
        for name in ["checkpoint-restore.json", "cold-publication.json"] {
            let (state, root) = content_test_state("recovery-api");
            let evidence = if name.starts_with("checkpoint") { state.paths.run_dir.join(name) } else { state.paths.root.join(name) };
            if name.starts_with("checkpoint") { fs::write(&evidence, b"{broken").unwrap(); }
            else { fs::create_dir(&evidence).unwrap(); } // Ordinary read I/O failure, not a typed publication conflict.
            assert_recovery_agent_available(&state.paths).await;
            if name.starts_with("checkpoint") { assert_eq!(fs::read(&evidence).unwrap(), b"{broken"); }
            else { assert!(evidence.is_dir()); }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn damaged_initialization_documents_keep_read_only_api_and_original_bytes() {
        for name in ["profiles.json", "update-state.json", "release-pointers.json", "state.json", "run/harness-log-session.json", "releases/bad/manifest.json"] {
            let (state, root) = content_test_state("degraded-api");
            let path = state.paths.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = if name == "profiles.json" { br#"{"schema_version":999,"active_profile":"demo","profiles":["demo"]}"#.as_slice() } else { b"{broken" };
            fs::write(&path, original).unwrap();
            assert_recovery_agent_available(&state.paths).await;
            assert_eq!(fs::read(&path).unwrap(), original, "{name} must remain untouched");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn background_crash_capture_without_http_retries_failed_collection() {
        let (state, root) = content_test_state("background-crash");
        let mut launch = nexus_core::HarnessLaunchSpec::new(std::path::PathBuf::from("C:/Windows/System32/cmd.exe"));
        launch.mode = nexus_protocol::HarnessLaunchMode::Direct;
        launch.args = vec!["/d".into(), "/c".into(), "exit 7".into()];
        state.config.transaction(|config| { config.harness = Some(launch); Ok(()) }).unwrap();
        let _ = state.supervisor.start_with_profile("demo").await;
        // Make only diagnostic collection fail; the supervisor and its logs
        // still work. No status/control HTTP request drives the observer.
        fs::remove_dir(&state.paths.diagnostics_dir).unwrap();
        fs::write(&state.paths.diagnostics_dir, b"temporary obstruction").unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let observer = super::start_crash_observer(state.clone(), receiver);
        timeout(Duration::from_secs(10), async {
            loop {
                if state.crash_capture_run.lock().await.attempts == 1 { break; }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        while state.crash_capture_run.lock().await.in_flight.load(std::sync::atomic::Ordering::SeqCst) { sleep(Duration::from_millis(20)).await; }
        assert!(!state.crash_capture_run.lock().await.completed);
        fs::remove_file(&state.paths.diagnostics_dir).unwrap();
        fs::create_dir(&state.paths.diagnostics_dir).unwrap();
        timeout(Duration::from_secs(12), async {
            loop {
                if state.crash_capture_run.lock().await.completed { break; }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
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
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
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
        maintenance_preview: Arc::new(std::sync::Mutex::new(crate::MaintenancePreviewScan::default())),
            checkpoint_transition_gate: Arc::new(Mutex::new(Some(CheckpointTransitionGate {
                reached: transition_reached,
                release: transition_release_rx,
            }))),
            agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown,
            data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
            instance_id: "checkpoint-test-agent".to_owned(),            crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())), canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(nexus_launcher_core::HarnessLogObserver::default())),        };

        let restore_state = state.clone();
        let checkpoint_id = checkpoint.id.clone();
        let restore =
            tokio::spawn(async move { checkpoint_restore(restore_state, checkpoint_id).await });
        transition_reached_rx
            .await
            .expect("checkpoint restore reaches the serialized transition");
        assert!(matches!(execute_harness_action(&state,nexus_protocol::HarnessAction::Start).await,Err(crate::HarnessSupervisorError::Busy)));
        restore.abort();
        assert!(restore
            .await
            .expect_err("request cancellation aborts handler")
            .is_cancelled());
        transition_release
            .send(())
            .expect("checkpoint transition releases");
        let settled=supervisor.acquire_lifecycle().await;drop(settled);
        execute_harness_action(&state,nexus_protocol::HarnessAction::Start).await.expect("Harness starts after restore owner settles");
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
}

#[cfg(test)]
mod switch_ownership_tests {
    use std::{
        fs, io,
        path::PathBuf,
        sync::{atomic::AtomicU64, Arc},
        time::Duration,
    };

    use axum::{extract::State, Json};
    use nexus_core::{
        data_root_identity, AgentState, CheckpointRestoreJournalStore, CheckpointStore,
        ConfigStore, DiagnosticsStore, HarnessLaunchSpec, NexusConfigFile, NexusPaths,
        ProfileStore, ReleaseStore, UpdateSpec,
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
            .write(&NexusConfigFile { update_attempt_id: None, external_harness: None,
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
        maintenance_preview: Arc::new(std::sync::Mutex::new(crate::MaintenancePreviewScan::default())),
            checkpoint_transition_gate: Arc::new(Mutex::new(None)),
            agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown,
            data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
            instance_id: "switch-test-agent".to_owned(),            crash_capture_run: Arc::new(Mutex::new(super::CrashCapture::default())), canary: Arc::new(Mutex::new(None)),
        harness_logs: Arc::new(Mutex::new(nexus_launcher_core::HarnessLogObserver::default())),        }
    }

    #[tokio::test]
    async fn update_source_only_preserves_latest_private_commands_and_defaults() {
        let state = switch_test_state("source-only");
        let root = state.paths.root.clone();
        state.config.transaction(|document| {
            let update = document.update.as_mut().unwrap();
            update.ref_name = "custom-ref".into(); update.build_program = Some("custom-build".into());
            update.build_args = vec!["--token".into(), "BUILD-SECRET".into()];
            update.verify_program = Some("custom-verify".into()); update.verify_args = vec!["--password=VERIFY-SECRET".into()];
            update.timeout_secs = Some(987); Ok(())
        }).unwrap();
        let mut stale = state.config.load().unwrap().update.unwrap().to_payload();
        stale.source = "https://example.test/updated".into();
        stale.build_args = vec!["[REDACTED]".into()];
        state.config.transaction(|document| { document.update.as_mut().unwrap().ref_name = "newer-concurrent-ref".into(); Ok(()) }).unwrap();
        let response = config_control(State(state.clone()), Json(ConfigCommand { expected_revision: Some(state.config.snapshot().unwrap().revision), action: ConfigAction::SetUpdateSource, update: Some(stale), ..Default::default() })).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("BUILD-SECRET")); assert!(!text.contains("VERIFY-SECRET"));
        let update = state.config.load().unwrap().update.unwrap();
        assert_eq!(update.source, "https://example.test/updated"); assert_eq!(update.ref_name, "newer-concurrent-ref");
        assert_eq!(update.build_args, vec!["--token", "BUILD-SECRET"]); assert_eq!(update.verify_args, vec!["--password=VERIFY-SECRET"]);
        assert_eq!(update.build_program.unwrap().to_str(), Some("custom-build")); assert_eq!(update.verify_program.unwrap().to_str(), Some("custom-verify"));
        assert_eq!(update.timeout_secs, Some(987));
        state.config.transaction(|document| { document.update = None; Ok(()) }).unwrap();
        let command: ConfigCommand = serde_json::from_value(serde_json::json!({"action":"set_update_source","expected_revision":state.config.snapshot().unwrap().revision,"update":{"source":"https://example.test/new"}})).unwrap();
        assert_eq!(config_control(State(state.clone()), Json(command)).await.status(), axum::http::StatusCode::OK);
        let update = state.config.load().unwrap().update.unwrap();
        assert_eq!(update.ref_name, "main"); assert_eq!(update.git_program.to_str(), Some("git"));
        drop(state); let _ = fs::remove_dir_all(root);
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
        let state=switch_test_state("config-cas-api");let root=state.paths.root.clone();
        let first=state.config.snapshot().unwrap();
        let status=super::config_status(State(state.clone())).await;
        assert_eq!(status.status(),axum::http::StatusCode::OK);
        let json:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(status.into_body(),128*1024).await.unwrap()).unwrap();
        assert_eq!(json["revision"],first.revision);
        let absent=config_control(State(state.clone()),Json(ConfigCommand{action:ConfigAction::ClearUpdate,..Default::default()})).await;
        assert_eq!(absent.status(),axum::http::StatusCode::PRECONDITION_REQUIRED);
        assert_eq!(state.config.snapshot().unwrap().revision,first.revision);
        let saved=config_control(State(state.clone()),Json(ConfigCommand{expected_revision:Some(first.revision.clone()),action:ConfigAction::ClearUpdate,..Default::default()})).await;
        assert_eq!(saved.status(),axum::http::StatusCode::OK);
        let saved_json:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(saved.into_body(),128*1024).await.unwrap()).unwrap();
        assert_eq!(saved_json["revision"],state.config.snapshot().unwrap().revision);
        assert_ne!(saved_json["revision"],first.revision);
        let before=fs::read(&state.paths.config_file).unwrap();
        let conflict=config_control(State(state.clone()),Json(ConfigCommand{expected_revision:Some(first.revision.clone()),action:ConfigAction::ClearUpdate,..Default::default()})).await;
        assert_eq!(conflict.status(),axum::http::StatusCode::CONFLICT);
        let error:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(conflict.into_body(),128*1024).await.unwrap()).unwrap();
        assert_eq!(error["code"],"config_revision_conflict");
        let maintenance=super::maintenance_control(State(state.clone()),Json(super::MaintenanceRequest{expected_revision:Some(first.revision),action:"reset".into(),scope:Some("config".into())})).await;
        assert_eq!(maintenance.status(),axum::http::StatusCode::CONFLICT);
        assert_eq!(fs::read(&state.paths.config_file).unwrap(),before);
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
    async fn asynchronous_fast_switch_persists_terminal_failure() {
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
        assert_eq!(terminal.phase, nexus_protocol::ColdOperationPhase::Failed);
        assert!(terminal
            .error
            .as_deref()
            .is_some_and(|message| message.contains("failed to persist Agent current release")));
        assert_eq!(
            state
                .releases
                .load()
                .expect("release selection loads")
                .current_release
                .as_deref(),
            None,
            "failed asynchronous promotion restores the previous pointer"
        );
        assert!(state.updater.try_acquire_gate().is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
