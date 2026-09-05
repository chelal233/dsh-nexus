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
    load_update_spec, new_instance_id, AgentState, CheckpointRestoreIntent,
    CheckpointRestoreJournal, CheckpointRestoreJournalStore, CheckpointRestorePhase,
    CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec, HarnessLogSession,
    HarnessLogSessionStore, NexusConfig, NexusConfigFile, NexusStateSnapshot, ProfileCatalog,
    ProfileStore, ReleaseCatalog, ReleaseStore, UpdateSpec, DEFAULT_MAX_RELEASE_SLOTS,
    DEFAULT_PROFILE, HARNESS_ARGS_ENV,
    HARNESS_PROGRAM_ENV, HARNESS_READINESS_TIMEOUT_ENV, HARNESS_READINESS_URL_ENV,
    HARNESS_WORKING_DIR_ENV, UPDATE_BUILD_ARGS_ENV, UPDATE_BUILD_PROGRAM_ENV,
    UPDATE_GIT_PROGRAM_ENV, UPDATE_REF_ENV, UPDATE_SOURCE_ENV, UPDATE_TIMEOUT_ENV,
    UPDATE_VERIFY_ARGS_ENV, UPDATE_VERIFY_PROGRAM_ENV,
};
use nexus_launcher_core::{
    harness_observation_matches_session, read_harness_ui_info, unavailable_harness_ui_info,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointAction, CheckpointCommand, CheckpointCreateResponse,
    CheckpointListResponse, CheckpointRestoreResponse, ConfigAction, ConfigCommand, ConfigResponse,
    DiagnosticsAction, DiagnosticsCommand, DiagnosticsResponse, ErrorResponse, HarnessAction,
    HarnessCommand, HarnessDiscoveryResponse, HarnessResponse, HarnessRuntimeInfo, HealthResponse,
    LifecycleAccepted, LifecycleAction, LifecycleCommand, ProfileAction, ProfileCommand,
    ProfileListResponse, ProfileSelectResponse, ReleaseAction, ReleaseCommand, ReleaseListResponse,
    StateResponse, TagListResponse, UpdateAction, UpdateCommand, UpdateResponse, UpdateState,
};
use tokio::{
    net::TcpListener,
    sync::{watch, Mutex, RwLock},
};

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
    supervisor: HarnessSupervisor,
    harness_sync: Arc<Mutex<()>>,
    #[cfg(test)]
    checkpoint_transition_gate: Arc<Mutex<Option<CheckpointTransitionGate>>>,
    #[cfg(test)]
    checkpoint_agent_persist_failure: Arc<std::sync::atomic::AtomicBool>,
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
    let checkpoint_restores = CheckpointRestoreJournalStore::new(paths.clone());
    recover_checkpoint_restore_startup(&checkpoint_restores, &profiles, &releases)?;
    let profile_catalog = profiles.load()?;
    let release_catalog = releases.load()?;
    let diagnostics = DiagnosticsStore::new(paths.clone());
    let config_store = ConfigStore::new(paths.clone());
    let updater = UpdateExecutor::new(paths.clone(), releases.clone());
    let _ = updater.recover_unattached()?;
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
        supervisor: supervisor.clone(),
        harness_sync: Arc::new(Mutex::new(())),
        #[cfg(test)]
        checkpoint_transition_gate: Arc::new(Mutex::new(None)),
        #[cfg(test)]
        checkpoint_agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        .route(
            "/v1/checkpoints",
            get(checkpoint_list).post(checkpoint_control),
        )
        .route("/v1/releases", get(release_list).post(release_control))
        .route("/v1/releases/tags", get(release_tags))
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

fn recover_checkpoint_restore_startup(
    journal_store: &CheckpointRestoreJournalStore,
    profiles: &ProfileStore,
    releases: &ReleaseStore,
) -> io::Result<()> {
    let Some(journal) = journal_store.load()? else {
        return Ok(());
    };
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
    settle_checkpoint_restore(state).await?;
    if action == HarnessAction::Status {
        return Ok(state.supervisor.status().await);
    }
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
    match update_agent_state(state, |_| {}).await {
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
    }
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
                .checkpoint_agent_persist_failure
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

fn profile_list_response(catalog: ProfileCatalog) -> ProfileListResponse {
    ProfileListResponse::new(catalog.active_profile, catalog.profiles)
}

async fn profile_list(State(state): State<AppState>) -> axum::response::Response {
    let _lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
    match state.profiles.load() {
        Ok(catalog) => (StatusCode::OK, Json(profile_list_response(catalog))).into_response(),
        Err(error) => data_error_response(error, "profile_catalog_unavailable"),
    }
}

async fn profile_control(
    State(state): State<AppState>,
    Json(command): Json<ProfileCommand>,
) -> axum::response::Response {
    match command.action {
        ProfileAction::List | ProfileAction::Status => profile_list(State(state)).await,
        ProfileAction::Select => {
            let Some(profile) = command.profile.as_deref() else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "profile is required"),
                    "profile_invalid",
                );
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
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
            let catalog = match state.profiles.select(profile) {
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
    }
}

async fn checkpoint_list(State(state): State<AppState>) -> axum::response::Response {
    match state.checkpoints.list() {
        Ok(checkpoints) => (
            StatusCode::OK,
            Json(CheckpointListResponse::new(checkpoints)),
        )
            .into_response(),
        Err(error) => data_error_response(error, "checkpoint_list_failed"),
    }
}

async fn checkpoint_control(
    State(state): State<AppState>,
    Json(command): Json<CheckpointCommand>,
) -> axum::response::Response {
    match command.action {
        CheckpointAction::List => checkpoint_list(State(state)).await,
        CheckpointAction::Create => checkpoint_create(state, command.note).await,
        CheckpointAction::Restore => {
            let Some(id) = command.id else {
                return data_error_response(
                    io::Error::new(io::ErrorKind::InvalidInput, "checkpoint id is required"),
                    "checkpoint_invalid",
                );
            };
            checkpoint_restore(state, id).await
        }
    }
}

async fn checkpoint_create(state: AppState, note: Option<String>) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
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
    let snapshot = NexusStateSnapshot {
        profile: profile.clone(),
        release: current.release.clone(),
    };
    match state
        .checkpoints
        .create(&profile, current.release, note, snapshot)
    {
        Ok(checkpoint) => (
            StatusCode::CREATED,
            Json(CheckpointCreateResponse::from_manifest(checkpoint)),
        )
            .into_response(),
        Err(error) => data_error_response(error, "checkpoint_create_failed"),
    }
}

async fn checkpoint_restore(state: AppState, id: String) -> axum::response::Response {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    if let Err(error) = settle_checkpoint_restore(&state).await {
        return data_error_response(
            io::Error::other(error.to_string()),
            "checkpoint_recovery_failed",
        );
    }
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
    let intent = CheckpointRestoreIntent {
        checkpoint_id: checkpoint.id.clone(),
        previous_profiles,
        previous_current_release: previous_releases.current_release,
        previous_last_known_good: previous_releases.last_known_good,
        target_profiles,
        target_current_release: target_releases.current_release,
        target_last_known_good: target_releases.last_known_good,
    };
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
    state
        .checkpoint_restores
        .clear(CheckpointRestorePhase::Prepared, intent)
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

async fn release_tags(State(state): State<AppState>) -> axum::response::Response {
    let spec = match load_update_spec(&state.paths) {
        Ok(Some(spec)) => spec,
        Ok(None) => {
            return data_error_response(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update source is not configured; set it in config.json first",
                ),
                "update_source_not_configured",
            );
        }
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
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
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
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
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
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
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
    let current_release = catalog.current_release.clone();
    if let Err(error) = update_agent_state(state, |current| {
        current.set_release(current_release);
    })
    .await
    {
        return data_error_response(
            io::Error::other(error.to_string()),
            "release_state_persistence_failed",
        );
    }
    (StatusCode::OK, Json(release_list_response(catalog))).into_response()
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
    (StatusCode::OK, Json(UpdateResponse::new(update, release))).into_response()
}

async fn update_control(
    State(state): State<AppState>,
    Json(command): Json<UpdateCommand>,
) -> axum::response::Response {
    match command.action {
        UpdateAction::Status => update_status(State(state)).await,
        UpdateAction::Install => match state
            .updater
            .install(command.release_id, command.version)
            .await
        {
            Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
            Err(error) => update_error_response(error),
        },
    }
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
        DiagnosticsAction::Collect => match state.diagnostics.collect(command.note) {
            Ok(bundle) => (
                StatusCode::CREATED,
                Json(DiagnosticsResponse::new(vec![bundle])),
            )
                .into_response(),
            Err(error) => data_error_response(error, "diagnostics_collect_failed"),
        },
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
            let Some(mut payload) = command.harness else {
                return data_error_response(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "harness configuration is required",
                    ),
                    "config_invalid",
                );
            };
            if command.preserve_harness_readiness_url {
                let existing = match state.config.load() {
                    Ok(document) => document.harness.and_then(|harness| harness.readiness_url),
                    Err(error) => return data_error_response(error, "config_unavailable"),
                };
                if let Some(existing) = existing {
                    payload.readiness_url = Some(existing);
                } else if env::var_os(HARNESS_READINESS_URL_ENV)
                    .is_some_and(|value| !value.is_empty())
                {
                    // An environment-only URL is intentionally never copied
                    // into Nexus config. Keep the persisted field empty while
                    // the effective response continues to report the env
                    // override in a redacted form.
                    payload.readiness_url = None;
                } else {
                    return data_error_response(
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "cannot preserve a readiness URL when no existing URL is configured",
                        ),
                        "config_invalid",
                    );
                }
            }
            let harness = match HarnessLaunchSpec::from_payload(payload) {
                Ok(harness) => harness,
                Err(error) => return data_error_response(error, "config_invalid"),
            };
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let mut document = match state.config.load() {
                Ok(document) => document,
                Err(error) => return data_error_response(error, "config_unavailable"),
            };
            document.harness = Some(harness);
            write_config_response(&state, document)
        }
        ConfigAction::ClearHarness => {
            let lifecycle = state.supervisor.acquire_lifecycle().await;
            if let Err(error) = settle_checkpoint_restore(&state).await {
                return data_error_response(
                    io::Error::other(error.to_string()),
                    "checkpoint_recovery_failed",
                );
            }
            if let Err(response) = ensure_harness_stopped(&state, &lifecycle).await {
                return response;
            }
            let mut document = match state.config.load() {
                Ok(document) => document,
                Err(error) => return data_error_response(error, "config_unavailable"),
            };
            document.harness = None;
            write_config_response(&state, document)
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
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            let mut document = match state.config.load() {
                Ok(document) => document,
                Err(error) => return data_error_response(error, "config_unavailable"),
            };
            document.update = Some(update);
            write_config_response(&state, document)
        }
        ConfigAction::ClearUpdate => {
            let _update_gate = match state.updater.try_acquire_gate() {
                Ok(gate) => gate,
                Err(error) => return update_error_response(error),
            };
            if let Err(response) = ensure_update_idle(&state) {
                return response;
            }
            let mut document = match state.config.load() {
                Ok(document) => document,
                Err(error) => return data_error_response(error, "config_unavailable"),
            };
            document.update = None;
            write_config_response(&state, document)
        }
    }
}

fn config_response(document: NexusConfigFile) -> ConfigResponse {
    let harness_readiness_url_redacted = document
        .harness
        .as_ref()
        .and_then(|harness| harness.readiness_url.as_ref())
        .is_some_and(|url| redact_config_url(Some(url.clone())).as_deref() != Some(url.as_str()));
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

fn write_config_response(state: &AppState, document: NexusConfigFile) -> axum::response::Response {
    match state.config.write(&document) {
        Ok(()) => match config_response_for_paths(&state.paths, document) {
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
    use super::{DEFAULT_MAX_RELEASE_SLOTS, 
        acquire_runtime_lock, are_allowed_cors_headers, harness_ui_process_is_presentable,
        is_allowed_console_origin_for_port, is_allowed_cors_method, proxy_identity_values_match,
        redact_config_args, redact_config_url, PROXY_DATA_ROOT_HEADER, PROXY_INSTANCE_HEADER,
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
}

#[cfg(test)]
mod checkpoint_tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{atomic::AtomicU64, Arc},
        time::Duration,
    };

    use nexus_core::{
        data_root_identity, AgentState, CheckpointRestoreIntent, CheckpointRestoreJournalStore,
        CheckpointStore, ConfigStore, DiagnosticsStore, HarnessLaunchSpec, NexusConfigFile,
        NexusPaths, NexusStateSnapshot, ProfileCatalog, ProfileStore, ReleaseStore,
    };
    use nexus_protocol::{AgentLifecycleState, HarnessState};
    use tokio::{
        sync::{oneshot, watch, Mutex, RwLock},
        time::{sleep, timeout},
    };

    use super::{
        checkpoint_create, checkpoint_restore, execute_harness_action, DEFAULT_MAX_RELEASE_SLOTS,
        recover_checkpoint_restore_startup, sync_harness_state, update_agent_state, AppState,
        CheckpointTransitionGate, HarnessSupervisor, UpdateExecutor,
    };

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

    #[test]
    fn startup_recovers_prepared_and_validates_committed_checkpoint_restore() {
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
        recover_checkpoint_restore_startup(&journals, &profiles, &releases)
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
        assert!(recover_checkpoint_restore_startup(&journals, &profiles, &releases).is_err());
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
        recover_checkpoint_restore_startup(&journals, &profiles, &releases)
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
            
                releases: None,})
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
            supervisor: supervisor.clone(),
            harness_sync: Arc::new(Mutex::new(())),
            checkpoint_transition_gate: Arc::new(Mutex::new(Some(CheckpointTransitionGate {
                reached: transition_reached,
                release: transition_release_rx,
            }))),
            checkpoint_agent_persist_failure: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            .checkpoint_agent_persist_failure
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
    }
}
