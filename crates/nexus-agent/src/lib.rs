//! Headless Nexus control-plane process.

use std::{io, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use nexus_core::{AgentState, NexusConfig};
use nexus_protocol::{
    AgentLifecycleState, HealthResponse, LifecycleAccepted, LifecycleAction, LifecycleCommand,
    StateResponse,
};
use tokio::{
    net::TcpListener,
    sync::{watch, RwLock},
};

#[derive(Clone)]
struct AppState {
    runtime: Arc<RwLock<AgentState>>,
    shutdown: watch::Sender<bool>,
}

/// Run the Agent in the foreground until the lifecycle API or Ctrl+C requests shutdown.
pub async fn run(config: NexusConfig) -> io::Result<()> {
    let paths = config.paths();
    paths.ensure_directories()?;

    let runtime = Arc::new(RwLock::new(AgentState::starting()));
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let state = AppState {
        runtime: Arc::clone(&runtime),
        shutdown,
    };

    let listener = TcpListener::bind(config.bind_addr()).await?;
    {
        let mut current = runtime.write().await;
        current.mark_running();
    }

    tracing::info!(
        address = %config.bind_addr(),
        data_root = %paths.root.display(),
        "nexus agent listening"
    );

    let server = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(wait_for_shutdown(shutdown_receiver));
    let result = server.await;

    let mut current = runtime.write().await;
    current.mark_stopped();
    result
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/state", get(current_state))
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
    let current = state.runtime.read().await;
    Json(StateResponse::from_state(current.as_payload()))
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
