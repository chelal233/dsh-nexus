//! Headless Nexus control-plane process.

use std::{io, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use nexus_core::{AgentState, NexusConfig, RuntimeMetadataStore};
use nexus_protocol::{
    AgentLifecycleState, ErrorResponse, HarnessAction, HarnessCommand, HarnessResponse,
    HarnessRuntimeInfo, HealthResponse, LifecycleAccepted, LifecycleAction, LifecycleCommand,
    StateResponse,
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
    supervisor: HarnessSupervisor,
    shutdown: watch::Sender<bool>,
}

/// Run the Agent in the foreground until the lifecycle API or Ctrl+C requests shutdown.
pub async fn run(config: NexusConfig) -> io::Result<()> {
    let paths = config.paths();
    paths.ensure_directories()?;

    let supervisor = HarnessSupervisor::new(paths.clone())?;
    let metadata = supervisor.metadata_store();
    let initial_harness = supervisor.recover_unattached().await;
    let mut initial_runtime = AgentState::starting();
    initial_runtime.harness = initial_harness.state;
    metadata.write_snapshot(&initial_runtime, initial_harness.clone())?;
    let runtime = Arc::new(RwLock::new(initial_runtime));
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let state = AppState {
        runtime: Arc::clone(&runtime),
        metadata: metadata.clone(),
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
    let result = match command.action {
        HarnessAction::Start => state.supervisor.start().await,
        HarnessAction::Stop => state.supervisor.stop().await,
        HarnessAction::Restart => state.supervisor.restart().await,
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

fn harness_error_response(error: HarnessSupervisorError) -> axum::response::Response {
    let (status, code) = match &error {
        HarnessSupervisorError::NotConfigured => {
            (StatusCode::UNPROCESSABLE_ENTITY, "harness_not_configured")
        }
        HarnessSupervisorError::AlreadyRunning => (StatusCode::CONFLICT, "harness_already_running"),
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
