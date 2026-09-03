//! Headless Nexus control-plane process.

use std::{io, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use nexus_core::{
    AgentState, CheckpointStore, NexusConfig, NexusStateSnapshot, ProfileCatalog, ProfileStore,
    ReleaseCatalog, ReleaseStore, RuntimeMetadataStore, DEFAULT_PROFILE,
};
use nexus_protocol::{
    AgentLifecycleState, CheckpointAction, CheckpointCommand, CheckpointCreateResponse,
    CheckpointListResponse, CheckpointRestoreResponse, ErrorResponse, HarnessAction,
    HarnessCommand, HarnessResponse, HarnessRuntimeInfo, HealthResponse, LifecycleAccepted,
    LifecycleAction, LifecycleCommand, ProfileAction, ProfileCommand, ProfileListResponse,
    ProfileSelectResponse, ReleaseAction, ReleaseCommand, ReleaseListResponse, StateResponse,
};
use tokio::{
    net::TcpListener,
    sync::{watch, RwLock},
};

mod supervisor;

pub use supervisor::{HarnessSupervisor, HarnessSupervisorError};

#[derive(Clone)]
struct AppState {
    runtime: Arc<RwLock<AgentState>>,
    metadata: RuntimeMetadataStore,
    profiles: ProfileStore,
    checkpoints: CheckpointStore,
    releases: ReleaseStore,
    supervisor: HarnessSupervisor,
    shutdown: watch::Sender<bool>,
}

/// Run the Agent in the foreground until the lifecycle API or Ctrl+C requests shutdown.
pub async fn run(config: NexusConfig) -> io::Result<()> {
    let paths = config.paths();
    paths.ensure_directories()?;

    let profiles = ProfileStore::new(paths.clone());
    let profile_catalog = profiles.load()?;
    let checkpoints = CheckpointStore::new(paths.clone());
    let releases = ReleaseStore::new(paths.clone());
    let release_catalog = releases.load()?;
    let supervisor = HarnessSupervisor::new(paths.clone())?;
    let metadata = supervisor.metadata_store();
    let initial_harness = supervisor.recover_unattached().await;
    let mut initial_runtime = AgentState::starting();
    initial_runtime.profile = Some(profile_catalog.active_profile.clone());
    initial_runtime.release = release_catalog.current_release.clone();
    initial_runtime.harness = initial_harness.state;
    metadata.write_snapshot(&initial_runtime, initial_harness.clone())?;
    let runtime = Arc::new(RwLock::new(initial_runtime));
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let state = AppState {
        runtime: Arc::clone(&runtime),
        metadata: metadata.clone(),
        profiles,
        checkpoints,
        releases,
        supervisor: supervisor.clone(),
        shutdown,
    };

    let listener = TcpListener::bind(config.bind_addr()).await?;
    {
        let mut current = runtime.write().await;
        current.mark_running();
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
        .route("/v1/profiles", get(profile_list).post(profile_control))
        .route(
            "/v1/checkpoints",
            get(checkpoint_list).post(checkpoint_control),
        )
        .route("/v1/releases", get(release_list).post(release_control))
        .route("/v1/lifecycle", post(lifecycle))
        .route("/v1/shutdown", post(shutdown))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let current = state.runtime.read().await;
    let response = if current.lifecycle == AgentLifecycleState::ShuttingDown {
        HealthResponse::shutting_down()
    } else {
        HealthResponse::healthy()
    };
    Json(response)
}

async fn current_state(State(state): State<AppState>) -> Json<StateResponse> {
    let _ = sync_harness_state(&state).await;
    let current = state.runtime.read().await;
    Json(StateResponse::from_state(current.as_payload()))
}

async fn harness_status(State(state): State<AppState>) -> Json<HarnessResponse> {
    let harness = sync_harness_state(&state).await;
    Json(HarnessResponse::from_runtime(harness))
}

async fn harness_control(
    State(state): State<AppState>,
    Json(command): Json<HarnessCommand>,
) -> impl IntoResponse {
    let profile = state
        .runtime
        .read()
        .await
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let result = match command.action {
        HarnessAction::Start => state.supervisor.start_with_profile(&profile).await,
        HarnessAction::Stop => state.supervisor.stop().await,
        HarnessAction::Restart => state.supervisor.restart_with_profile(&profile).await,
        HarnessAction::Status => Ok(state.supervisor.status().await),
    };

    match result {
        Ok(harness) => match set_harness_state(&state, harness.clone()).await {
            Ok(()) => {
                (StatusCode::OK, Json(HarnessResponse::from_runtime(harness))).into_response()
            }
            Err(error) => harness_error_response(error),
        },
        Err(error) => harness_error_response(error),
    }
}

async fn sync_harness_state(state: &AppState) -> HarnessRuntimeInfo {
    let harness = state.supervisor.status().await;
    if let Err(error) = set_harness_state(state, harness.clone()).await {
        tracing::warn!(error = %error, "failed to persist refreshed Harness state");
    }
    harness
}

async fn set_harness_state(
    state: &AppState,
    harness: HarnessRuntimeInfo,
) -> Result<(), HarnessSupervisorError> {
    let current = {
        let mut current = state.runtime.write().await;
        if current.harness != harness.state {
            current.set_harness(harness.state);
        }
        current.clone()
    };
    state
        .metadata
        .write_snapshot(&current, harness)
        .map_err(HarnessSupervisorError::Persistence)
}

fn profile_list_response(catalog: ProfileCatalog) -> ProfileListResponse {
    ProfileListResponse::new(catalog.active_profile, catalog.profiles)
}

async fn profile_list(State(state): State<AppState>) -> axum::response::Response {
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
            let harness = sync_harness_state(&state).await;
            if matches!(
                harness.state,
                nexus_protocol::HarnessState::Starting | nexus_protocol::HarnessState::Running
            ) {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "profile_change_conflict",
                    "cannot switch profile while Harness is running; stop Harness first",
                );
            }
            let catalog = match state.profiles.select(profile) {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "profile_invalid"),
            };
            let current = {
                let mut current = state.runtime.write().await;
                current.set_profile(catalog.active_profile.clone());
                current.clone()
            };
            if let Err(error) = state.metadata.write_snapshot(&current, harness) {
                return data_error_response(error, "profile_state_persistence_failed");
            }
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
    let _harness = sync_harness_state(&state).await;
    let current = state.runtime.read().await.clone();
    let profile = current
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let snapshot = NexusStateSnapshot {
        lifecycle: current.lifecycle,
        harness: current.harness,
        profile: profile.clone(),
        release: current.release.clone(),
        updated_at_unix: current.updated_at_unix,
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
    let harness = sync_harness_state(&state).await;
    if matches!(
        harness.state,
        nexus_protocol::HarnessState::Starting | nexus_protocol::HarnessState::Running
    ) {
        return api_error_response(
            StatusCode::CONFLICT,
            "checkpoint_restore_conflict",
            "cannot restore a checkpoint while Harness is running; stop Harness first",
        );
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
    if let Some(release) = checkpoint.release.as_deref() {
        if let Err(error) = state.releases.get(release) {
            let code = if error.kind() == io::ErrorKind::NotFound {
                "checkpoint_release_not_found"
            } else {
                "checkpoint_release_unavailable"
            };
            return data_error_response(error, code);
        }
    }
    let catalog = match state.profiles.select(&checkpoint.profile) {
        Ok(catalog) => catalog,
        Err(error) => return data_error_response(error, "checkpoint_profile_invalid"),
    };
    let current = {
        let mut current = state.runtime.write().await;
        current.set_profile(catalog.active_profile);
        current.set_release(checkpoint.release.clone());
        current.clone()
    };
    if let Err(error) = state.metadata.write_snapshot(&current, harness) {
        return data_error_response(error, "checkpoint_state_persistence_failed");
    }
    (
        StatusCode::OK,
        Json(CheckpointRestoreResponse::restored(checkpoint)),
    )
        .into_response()
}

fn release_list_response(catalog: ReleaseCatalog) -> ReleaseListResponse {
    ReleaseListResponse::new(
        catalog.current_release,
        catalog.last_known_good,
        catalog.releases,
    )
}

async fn release_list(State(state): State<AppState>) -> axum::response::Response {
    match state.releases.load() {
        Ok(catalog) => (StatusCode::OK, Json(release_list_response(catalog))).into_response(),
        Err(error) => data_error_response(error, "release_catalog_unavailable"),
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
            let harness = sync_harness_state(&state).await;
            if matches!(
                harness.state,
                nexus_protocol::HarnessState::Starting | nexus_protocol::HarnessState::Running
            ) {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "release_change_conflict",
                    "cannot promote a release while Harness is running; stop Harness first",
                );
            }
            let catalog = match state.releases.promote(id) {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "release_promote_failed"),
            };
            apply_release_catalog(&state, catalog, harness).await
        }
        ReleaseAction::Rollback => {
            let harness = sync_harness_state(&state).await;
            if matches!(
                harness.state,
                nexus_protocol::HarnessState::Starting | nexus_protocol::HarnessState::Running
            ) {
                return api_error_response(
                    StatusCode::CONFLICT,
                    "release_change_conflict",
                    "cannot roll back a release while Harness is running; stop Harness first",
                );
            }
            let catalog = match state.releases.rollback() {
                Ok(catalog) => catalog,
                Err(error) => return data_error_response(error, "release_rollback_failed"),
            };
            apply_release_catalog(&state, catalog, harness).await
        }
    }
}

async fn apply_release_catalog(
    state: &AppState,
    catalog: ReleaseCatalog,
    harness: HarnessRuntimeInfo,
) -> axum::response::Response {
    let current = {
        let mut current = state.runtime.write().await;
        current.set_release(catalog.current_release.clone());
        current.clone()
    };
    if let Err(error) = state.metadata.write_snapshot(&current, harness) {
        return data_error_response(error, "release_state_persistence_failed");
    }
    (StatusCode::OK, Json(release_list_response(catalog))).into_response()
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
        io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
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
    {
        let mut current = state.runtime.write().await;
        current.request_shutdown();
        let harness = state.supervisor.status().await;
        if let Err(error) = state.metadata.write_snapshot(&current, harness) {
            tracing::warn!(error = %error, "failed to persist Agent shutdown state");
        }
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
