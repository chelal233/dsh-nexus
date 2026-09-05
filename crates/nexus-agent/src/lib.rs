//! Headless Nexus control-plane process.

use std::{
    env, fs, io,
    io::{Seek, SeekFrom, Write},
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
    data_root_identity, discover_harness_candidates_with_paths, load_harness_launch_spec,
    load_update_spec, new_instance_id, redact_diagnostics_payload, AgentState,
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
    harness_observation_matches_session, read_harness_ui_info, unavailable_harness_ui_info,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointAction, CheckpointCommand, CheckpointContentState,
    CheckpointCreateResponse, CheckpointListResponse, CheckpointRestoreResponse, ConfigAction,
    ConfigCommand, ConfigResponse, DiagnosticsAction, DiagnosticsCommand, DiagnosticsResponse,
    ErrorResponse, HarnessAction, HarnessCommand, HarnessDiscoveryResponse, HarnessResponse,
    HarnessRuntimeInfo, HealthResponse, LifecycleAccepted, LifecycleAction, LifecycleCommand,
    PluginRemoveResponse, ProfileAction, ProfileCommand, ProfileListResponse,
    ProfileSelectResponse, RecoveryLogTail, RecoveryStatusResponse, ReleaseAction, ReleaseCommand,
    ReleaseListResponse, RuntimeInstallMode, RuntimePlanRequest, RuntimeSource, StateResponse,
    TagListResponse, UpdateAction, UpdateCommand, UpdateResponse, UpdateState,
};
use tokio::{
    net::TcpListener,
    sync::{watch, Mutex, RwLock},
};

mod cold;
mod dsh;
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
    recover_checkpoint_restore_startup(&checkpoint_restores, &profiles, &releases, &snapshots)
        .await?;
    let profile_catalog = profiles.load()?;
    let release_catalog = releases.load()?;
    let diagnostics = DiagnosticsStore::new(paths.clone());
    let updater = UpdateExecutor::new(paths.clone(), releases.clone());
    let _ = updater.recover_unattached()?;
    let cold = cold::ColdCoordinator::new(paths.clone());
    cold.recover()?;
    let supervisor = HarnessSupervisor::new(paths.clone())?;
    let metadata = supervisor.metadata_store();
    // A restart can only recover a persisted Harness state by proving the
    // configured loopback readiness endpoint; it never reattaches a stale PID.
    let initial_harness = supervisor.recover_unattached().await;
    let mut initial_runtime = AgentState::starting();
    initial_runtime.profile = Some(profile_catalog.active_profile.clone());
    initial_runtime.release = release_catalog.current_release.clone();
    initial_runtime.harness = initial_harness.state;
    metadata.write_snapshot(&initial_runtime, initial_harness.clone())?;
    let runtime = Arc::new(RwLock::new(initial_runtime));
    let agent_revision = Arc::new(AtomicU64::new(0));
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
        #[cfg(test)]
        checkpoint_transition_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        #[cfg(test)]
        checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shutdown,
        data_root_id,
        instance_id,
    };

    let listener = TcpListener::bind(config.bind_addr()).await?;
    {
        let mut current = runtime.write().await;
        current.mark_running();
        agent_revision.store(1, Ordering::SeqCst);
        metadata.write_snapshot(&current, initial_harness.clone())?;
    }

    tracing::info!(
        address = %config.bind_addr(),
        data_root = %paths.root.display(),
        "nexus agent listening"
    );

    let server = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(wait_for_shutdown(shutdown_receiver));
    let result = server.await;

    let _ = supervisor.stop().await;
    let harness = supervisor.status().await;
    let mut current = runtime.write().await;
    current.mark_stopped();
    current.harness = harness.state;
    metadata.write_snapshot(&current, harness)?;
    result
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/state", get(current_state))
        .route("/v1/harness", get(harness_status).post(harness_control))
        .route("/v1/harness/discover", get(harness_discover))
        .route("/v1/harness/ui", get(harness_ui))
        .route("/v1/profiles", get(profile_list).post(profile_control))
        .route("/v1/recovery", get(recovery_status))
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
        .route("/v1/lifecycle", post(lifecycle))
        .route("/v1/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_proxy_identity,
        ))
        .layer(middleware::from_fn(local_console_cors))
        .with_state(state)
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
    let releases = releases.load()?;
    if profiles != journal.intent.target_profiles
        || releases.current_release != journal.intent.target_current_release
        || releases.last_known_good != journal.intent.target_last_known_good
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

fn proxy_identity_matches(headers: &axum::http::HeaderMap, state: &AppState) -> bool {
    proxy_identity_values_match(headers, &state.data_root_id, &state.instance_id)
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
        (None, None) => true,
        (Some(root), Some(instance)) => root == data_root_id && instance == instance_id,
        _ => false,
    }
}

async fn enforce_proxy_identity(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !proxy_identity_matches(request.headers(), &state) {
        return StatusCode::CONFLICT.into_response();
    }
    next.run(request).await
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
    let response = if current.lifecycle == AgentLifecycleState::ShuttingDown {
        HealthResponse::shutting_down(state.data_root_id.clone(), state.instance_id.clone())
    } else {
        HealthResponse::healthy(state.data_root_id.clone(), state.instance_id.clone())
    };
    Json(response)
}

async fn current_state(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
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
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
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

async fn recovery_status(State(state): State<AppState>) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    let harness = sync_harness_state(&state).await.into_response().harness;
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
    let startup_error = harness.error.as_deref().map(|error| {
        let bytes = error.as_bytes();
        let bounded = &bytes[..bytes.len().min(4096)];
        String::from_utf8_lossy(&redact_diagnostics_payload(bounded).0).into_owned()
    });
    (
        StatusCode::OK,
        Json(RecoveryStatusResponse {
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
    let truncated = metadata.len() > MAX_RECOVERY_LOG_BYTES;
    let start = metadata.len().saturating_sub(MAX_RECOVERY_LOG_BYTES);
    let mut file = fs::File::open(canonical)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    if truncated {
        if let Some(position) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=position);
        }
    }
    let raw = String::from_utf8_lossy(&bytes);
    let fatal = raw.lines().any(|line| {
        let line = line.trim_start().to_ascii_lowercase();
        line.starts_with("fatal:") || line.starts_with("[fatal]") || line.starts_with("fatal ")
    });
    let redacted = redact_diagnostics_payload(&bytes).0;
    Ok((
        String::from_utf8_lossy(&redacted).into_owned(),
        truncated,
        fatal,
    ))
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
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }

    fn unavailable_harness_ui_response(message: impl Into<String>) -> axum::response::Response {
        (StatusCode::OK, Json(unavailable_harness_ui_info(message))).into_response()
    }

    let session = match HarnessLogSessionStore::new(state.paths.clone()).read() {
        Ok(Some(session)) => session,
        Ok(None) => {
            return unavailable_harness_ui_response(
                "Harness log session marker is not available; restart Harness to establish a safe token boundary",
            )
        }
        Err(error) => {
            return unavailable_harness_ui_response(format!(
                "Harness log session marker is invalid: {error}"
            ))
        }
    };
    let first = sync_harness_state(&state).await.into_response();
    if !harness_ui_process_is_presentable(&first, &session) {
        return unavailable_harness_ui_response(format!(
            "Harness is {:?}; a current authentication token is not available",
            first.harness.state
        ));
    }
    if !harness_observation_matches_session(&first, &session) {
        return unavailable_harness_ui_response(
            "Agent Harness observation does not match the durable log session marker",
        );
    }

    let second = sync_harness_state(&state).await.into_response();
    if first != second
        || !harness_ui_process_is_presentable(&second, &session)
        || !harness_observation_matches_session(&second, &session)
    {
        return unavailable_harness_ui_response(
            "Harness changed state while its token was being observed; refresh after it is running",
        );
    }

    let info = read_harness_ui_info(&state.paths);
    let final_session = HarnessLogSessionStore::new(state.paths.clone()).read();
    let final_observation = sync_harness_state(&state).await.into_response();
    if !matches!(final_session, Ok(Some(ref current)) if current == &session)
        || final_observation != second
    {
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

async fn harness_control(
    State(state): State<AppState>,
    Json(command): Json<HarnessCommand>,
) -> impl IntoResponse {
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
    let lifecycle = state.supervisor.acquire_lifecycle().await;
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
    let manifests = dsh::native_profiles(state.snapshots.configured_dsh_home()?)?;
    let names = manifests
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    Ok(ProfileListResponse::new(catalog.active_profile, names).with_manifests(manifests))
}

async fn profile_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
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
    State(state): State<AppState>,
    Json(command): Json<ProfileCommand>,
) -> axum::response::Response {
    match command.action {
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
                match dsh::native_profiles(match state.snapshots.configured_dsh_home() {
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
    }
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
    let checkpoints = match state.checkpoints.list() {
        Ok(checkpoints) => checkpoints,
        Err(error) => return data_error_response(error, "checkpoint_list_failed"),
    };
    let profile = match state.profiles.load() {
        Ok(catalog) => catalog.active_profile,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let mut diagnostic = state.snapshots.healthy_error();
    let snapshots = match state.snapshots.list_inspections(profile).await {
        Ok(snapshots) => snapshots,
        Err(error) => {
            if diagnostic.is_none() {
                diagnostic = Some(error.to_string());
            }
            Vec::new()
        }
    };
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
            ),
        ),
    )
        .into_response()
}

async fn ensure_checkpoint_mutation_ready(
    state: &AppState,
) -> Result<(), axum::response::Response> {
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
            let lease = owner_state.snapshots.acquire(profile.clone()).await?;
            let manifest = lease.capture_manual(version, note.clone()).await?;
            owner_state.checkpoints.create_with_snapshot(
                &profile,
                current.release,
                note,
                state_snapshot,
                Some(snapshots::snapshot_reference(&manifest)),
            )
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
    ReleaseListResponse::new(
        catalog.current_release,
        catalog.last_known_good,
        catalog.releases,
    )
}

async fn release_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
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
    match updater::list_remote_tags(&spec.source, &spec.git_program, command_timeout).await {
        Ok(tags) => (
            StatusCode::OK,
            Json(TagListResponse::new(spec.source.clone(), tags)),
        )
            .into_response(),
        Err(error) => data_error_response(io::Error::other(error.to_string()), "tag_list_failed"),
    }
}

async fn release_control(
    State(state): State<AppState>,
    Json(command): Json<ReleaseCommand>,
) -> axum::response::Response {
    match command.action {
        ReleaseAction::List | ReleaseAction::Current => release_list(State(state)).await,
        ReleaseAction::Register => {
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
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
            let catalog = match state.releases.promote(id) {
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
            let catalog = match state.releases.remove(id) {
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
        response = response.with_operation(operation);
    }
    (StatusCode::OK, Json(response)).into_response()
}

async fn update_control(
    State(state): State<AppState>,
    Json(command): Json<UpdateCommand>,
) -> axum::response::Response {
    match command.action {
        UpdateAction::Status => update_status(State(state)).await,
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
            let operation = match state
                .cold
                .claim_confirmation(&operation_id, &confirmation)
                .await
            {
                Ok(operation) => operation,
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    return api_error_response(
                        StatusCode::CONFLICT,
                        "cold_confirmation_stale",
                        "cold-install confirmation is stale or mismatched",
                    )
                }
                Err(error) => return data_error_response(error, "cold_operation_unavailable"),
            };
            let owner_state = state.clone();
            tokio::spawn(async move {
                cold::confirm(owner_state, operation_id, confirmation).await;
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
        UpdateAction::Cancel => {
            let Some(operation_id) = command.operation_id else {
                return api_error_response(
                    StatusCode::BAD_REQUEST,
                    "cold_operation_id_required",
                    "operation_id is required",
                );
            };
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
    match state.diagnostics.list() {
        Ok(bundles) => (StatusCode::OK, Json(DiagnosticsResponse::new(bundles))).into_response(),
        Err(error) => data_error_response(error, "diagnostics_list_failed"),
    }
}

async fn diagnostics_control(
    State(state): State<AppState>,
    Json(command): Json<DiagnosticsCommand>,
) -> axum::response::Response {
    match command.action {
        DiagnosticsAction::Status => diagnostics_status(State(state)).await,
        DiagnosticsAction::Collect => {
            let _lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(response) = ensure_checkpoint_mutation_ready(&state).await {
                return response;
            }
            match state.diagnostics.collect(command.note) {
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

async fn config_status(State(state): State<AppState>) -> axum::response::Response {
    match state.config.load() {
        Ok(document) => match config_response_for_paths(&state.paths, document) {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(error) => data_error_response(error, "config_unavailable"),
        },
        Err(error) => data_error_response(error, "config_unavailable"),
    }
}

async fn config_control(
    State(state): State<AppState>,
    Json(command): Json<ConfigCommand>,
) -> axum::response::Response {
    match command.action {
        ConfigAction::Status => config_status(State(state)).await,
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
            transact_config_response(&state, move |document| {
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
                document.harness = Some(HarnessLaunchSpec::from_payload(payload)?);
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
            transact_config_response(&state, |document| {
                document.harness = None;
                Ok(())
            })
        }
        ConfigAction::SetUpdate => {
            let Some(payload) = command.update else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "update configuration is required",
                    ),
                    "config_invalid",
                );
            };
            let update = match UpdateSpec::from_payload(payload) {
                Ok(update) => update,
                Err(error) => return data_error_response(error, "config_invalid"),
            };
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
            transact_config_response(&state, move |document| {
                document.update = Some(update);
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
            transact_config_response(&state, |document| {
                document.update = None;
                Ok(())
            })
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
            transact_config_response(&state, move |document| {
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
            transact_config_response(&state, move |document| {
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
    ConfigResponse::new(
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
    .with_harness_readiness_url_redacted(harness_readiness_url_redacted)
}

fn config_response_for_paths(
    paths: &nexus_core::NexusPaths,
    document: NexusConfigFile,
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

    // Always go through the same effective loaders used by the supervisor so
    // legacy official DSH configs receive inferred readiness fields in the
    // GUI as well. The fallback keeps an in-memory document usable when the
    // config file has not been written yet.
    let effective = NexusConfigFile {
        harness: load_harness_launch_spec(paths)?.or(document.harness),
        update: load_update_spec(paths)?.or(document.update),
        releases: None,
        runtime: document.runtime,
        snapshots: document.snapshots,
    };
    Ok(config_response(effective)
        .with_environment_overrides(harness_env_override, update_env_override))
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
    update: impl FnOnce(&mut NexusConfigFile) -> io::Result<()>,
) -> axum::response::Response {
    match state.config.transaction(update) {
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
    let status = match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::AlreadyExists | io::ErrorKind::ResourceBusy => StatusCode::CONFLICT,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error_response(status, fallback_code, error.to_string())
}

fn harness_error_response(error: HarnessSupervisorError) -> axum::response::Response {
    let (status, code) = match &error {
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
    use super::{
        acquire_runtime_lock, are_allowed_cors_headers, harness_ui_process_is_presentable,
        is_allowed_console_origin_for_port, is_allowed_cors_method, proxy_identity_values_match,
        recovery_log_tail, redact_config_args, redact_config_url, PROXY_DATA_ROOT_HEADER,
        PROXY_INSTANCE_HEADER,
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
        assert!(proxy_identity_values_match(
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
        assert!(recovery_log_tail(&paths, "../outside.log").is_err());
        std::fs::remove_dir_all(root).expect("fixture removes");
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
        RuntimeInstallMode, RuntimeOwnership, RuntimeSource,
    };
    use tokio::{
        sync::{oneshot, watch, Mutex, RwLock},
        time::{sleep, timeout},
    };

    use super::{
        checkpoint_create, checkpoint_restore, checkpoint_restore_abort, checkpoint_restore_retry,
        execute_harness_action, recover_checkpoint_restore_startup, snapshots, sync_harness_state,
        update_agent_state, AppState, CheckpointTransitionGate, HarnessSupervisor, UpdateExecutor,
        DEFAULT_MAX_RELEASE_SLOTS,
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
                checkpoint_transition_gate: Arc::new(Mutex::new(None)),
                agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                shutdown,
                data_root_id: data_root_identity(&paths).expect("data root identity reads"),
                instance_id: format!("content-{label}"),
            },
            root,
        )
    }

    #[tokio::test]
    async fn materialization_failure_stays_prepared_blocks_mutations_and_abort_rolls_back() {
        let (state, root) = content_test_state("pending-abort");
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
            checkpoint_transition_gate: Arc::new(Mutex::new(Some(CheckpointTransitionGate {
                reached: transition_reached,
                release: transition_release_rx,
            }))),
            agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown,
            data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
            instance_id: "checkpoint-test-agent".to_owned(),
        };

        let restore_state = state.clone();
        let checkpoint_id = checkpoint.id.clone();
        let restore =
            tokio::spawn(async move { checkpoint_restore(restore_state, checkpoint_id).await });
        transition_reached_rx
            .await
            .expect("checkpoint restore reaches the serialized transition");
        let (start_attempt, start_attempt_rx) = oneshot::channel();
        supervisor.observe_next_lifecycle_wait(start_attempt).await;
        let start_state = state.clone();
        let start = tokio::spawn(async move {
            execute_harness_action(&start_state, nexus_protocol::HarnessAction::Start).await
        });
        start_attempt_rx
            .await
            .expect("Harness start polls the occupied shared lifecycle gate");
        assert!(
            !start.is_finished(),
            "Harness start must wait until checkpoint release publication completes"
        );
        restore.abort();
        assert!(restore
            .await
            .expect_err("request cancellation aborts handler")
            .is_cancelled());
        transition_release
            .send(())
            .expect("checkpoint transition releases");
        start
            .await
            .expect("Harness start task joins")
            .expect("Harness starts from restored release");
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

    fn switch_test_state(label: &str) -> AppState {
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
            checkpoint_transition_gate: Arc::new(Mutex::new(None)),
            agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            checkpoint_commit_result_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown,
            data_root_id: data_root_identity(&paths).expect("data-root identity reads"),
            instance_id: "switch-test-agent".to_owned(),
        }
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
            nexus_protocol::ColdOperationPhase::Cancelled
        );
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
        let set_runtime = tokio::spawn(async move {
            config_control(
                State(task_state),
                Json(ConfigCommand {
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
