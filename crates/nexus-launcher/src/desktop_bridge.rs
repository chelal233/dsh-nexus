//! Private stdio adapter for desktop hosts; all business state remains in Agent.
use std::{io::{self, BufRead, Write}, path::PathBuf, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};
use nexus_core::NexusConfig;
use nexus_launcher_core::{validate_agent_request, AgentAction, AgentIdentity, AgentRuntime, AgentStatus, MAX_REQUEST_BODY_BYTES};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
const START_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_START_WAIT_SECS;
#[derive(Debug, serde::Serialize)]
struct BridgeError {
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preflight: Option<Value>,
    code: String,
    message: String,
    retryable: bool,
    actions: Vec<String>,
    status: Option<u16>,
}
impl From<String> for BridgeError {
    fn from(message: String) -> Self {
        Self { kind: None, preflight: None, code: "launcher_error".into(), message, retryable: false, actions: vec![], status: None }
    }
}
impl From<nexus_launcher_core::AgentClientError> for BridgeError {
    fn from(error: nexus_launcher_core::AgentClientError) -> Self {
        if let nexus_launcher_core::AgentClientError::Http { status, message, body } = error {
            let document: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let code = document.get("code").and_then(Value::as_str).unwrap_or("agent_http_error").to_owned();
            let actions = match code.as_str() {
                "config_revision_conflict" | "config_revision_required" => vec!["reload_config".into()],
                "harness_not_configured" | "harness_configuration_error" => vec!["open_settings".into()],
                "harness_start_paused" => vec!["open_recovery".into()],
                _ => vec![],
            };
            return Self { kind: document.get("kind").and_then(Value::as_str).map(str::to_owned), preflight: document.get("preflight").cloned(), code, message: document.get("message").and_then(Value::as_str).unwrap_or(&message).to_owned(),
                retryable: document.get("retryable").and_then(Value::as_bool).unwrap_or(false) || matches!(status.as_u16(), 408 | 429 | 502 | 503 | 504), actions, status: Some(status.as_u16()) };
        }
        let retryable = matches!(&error, nexus_launcher_core::AgentClientError::Transport(_));
        Self { kind: None, preflight: None, code: if retryable { "agent_transport_error" } else { "agent_protocol_error" }.into(), message: error.to_string(), retryable,
            actions: vec!["check_agent".into()], status: None }
    }
}
const STOP_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_STOP_WAIT_SECS;
#[derive(Clone)]
struct AppState {
    update_lock: Arc<Mutex<Option<std::fs::File>>>,
    runtime: Arc<AgentRuntime>,
    startup_attempted: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
    harness_startup_error: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Deserialize)]
struct AgentCommand {
    action: String,
}

impl AppState {
    fn new(resource_dir: Option<PathBuf>) -> Result<Self, String> {
        let config = NexusConfig::from_env();
        let mut runtime = AgentRuntime::new_with_resource_dir(config, None, resource_dir)
            .map_err(|error| error.to_string())?;
        if !cfg!(debug_assertions) {
            let identity = build_identity()?;
            let id = identity.get("buildId").and_then(Value::as_str).ok_or("Packaged build identity is missing")?;
            runtime.bind_build_identity(id).map_err(|error| error.to_string())?;
        }
        Ok(Self {
            update_lock: Arc::new(Mutex::new(None)),
            runtime: Arc::new(runtime),
            startup_attempted: Arc::new(AtomicBool::new(false)),
            desired_running: Arc::new(AtomicBool::new(true)),
            harness_startup_error: Arc::new(Mutex::new(None)),
        })
    }

    fn should_auto_start(&self) -> bool {
        self.desired_running.load(Ordering::Acquire)
            && self
                .startup_attempted
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    fn set_desired_running(&self, desired: bool) {
        self.desired_running.store(desired, Ordering::Release);
    }
}

#[derive(serde::Serialize)]
struct StartupResponse {
    #[serde(flatten)]
    agent: AgentStatus,
    harness_startup_error: Option<String>,
}

fn startup_response(state: &AppState, agent: AgentStatus) -> StartupResponse {
    StartupResponse { agent, harness_startup_error: state.harness_startup_error.lock().ok().and_then(|value| value.clone()) }
}

async fn startup_status(state: &AppState) -> Result<StartupResponse, String> {
    if state.should_auto_start() {
        if let Err(error) = state.runtime.ensure_started(START_WAIT_SECS).await {
            // `status` retains a bounded, user-visible startup error and never
            // hides a failed Agent resolution behind a fallback port or process.
            let mut status = state.runtime.status().await;
            status.message = Some(error.to_string());
            return Ok(startup_response(&state, status));
        }
        schedule_configured_harness(state.clone());
    }
    Ok(startup_response(&state, state.runtime.status().await))
}

/// The GUI is the user-facing launcher, so its first successful Agent
/// handshake also performs the configured Harness bootstrap. Harness remains
/// an independent child of Agent; this is only orchestration and a 409 means
/// another owner already has it running.
fn schedule_configured_harness(state: AppState) {
    // Agent readiness must not wait for a potentially long Harness preflight.
    if let Ok(mut error) = state.harness_startup_error.lock() { *error = None; }
    tokio::spawn(async move {
        if let Err(error) = start_configured_harness(&state).await {
            let bounded: String = error.chars().take(2048).collect();
            let redacted = nexus_core::redact_diagnostics_payload(bounded.as_bytes()).0;
            if let Ok(mut slot) = state.harness_startup_error.lock() { *slot = Some(String::from_utf8_lossy(&redacted).into_owned()); }
        }
    });
}

async fn start_configured_harness(state: &AppState) -> Result<(), String> {
    let health = match state.runtime.probe_ready().await {
        Ok(health) => health,
        Err(error) => {
            eprintln!("nexus-launcher-app: Harness auto-start skipped: {error}");
            return Err(error.to_string());
        }
    };
    let client = state
        .runtime
        .client()
        .with_expected_identity(AgentIdentity::from(&health));
    let recovery = client.get_json::<Value>("/v1/recovery").await.map_err(|error| error.to_string())?;
    match recovery.get("paused").and_then(Value::as_bool) {
        Some(true) => return Ok(()),
        Some(false) => {},
        None => return Err("Cannot determine Harness recovery mode; retry or export diagnostics".to_owned()),
    }
    let config = match client.get_json::<Value>("/v1/config").await {
        Ok(config) => config,
        Err(error) => {
            eprintln!("nexus-launcher-app: Harness auto-start config check failed: {error}");
            return Err(error.to_string());
        }
    };
    if !config.get("harness").is_some_and(|value| !value.is_null()) {
        return Ok(());
    }
    if !state.desired_running.load(Ordering::Acquire) { return Ok(()); }
    if let Err(error) = client
        .post_json::<_, Value>("/v1/harness", &json!({ "action": "start" }))
        .await
    {
        let message = error.to_string();
        if !message.contains("HTTP 409") {
            return Err(message);
        }
    }
    Ok(())
}

async fn retry_startup(state: &AppState) -> Result<StartupResponse, String> {
    state.set_desired_running(true);
    state.startup_attempted.store(false, Ordering::Release);
    startup_status(state).await
}

async fn proxy_request(
    state: &AppState,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, BridgeError> {
    let method = method
        .parse::<Method>()
        .map_err(|_| "Only GET and POST are supported by the local Agent bridge".to_owned())?;

    if !is_allowed_route(&path) {
        return Err(format!("Agent route is not allowed: {path}").into());
    }

    if method == Method::POST && path == "/v1/harness" {
        if let Ok(mut error) = state.harness_startup_error.lock() { *error = None; }
    }
    if path == "/v1/agent" {
        let action = validate_native_agent_request(&method, body.as_ref())?;
        return execute_agent_action(&state, action)
            .await
            .map_err(BridgeError::from);
    }

    let encoded_body = body
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|error| format!("Agent request body could not be encoded: {error}"))?;
    validate_agent_request(&path, &method, encoded_body.as_deref())
        .map_err(|error| error.to_string())?;
    let health = state
        .runtime
        .probe_ready()
        .await
        .map_err(|error| error.to_string())?;
    let client = state
        .runtime
        .client()
        .with_expected_identity(AgentIdentity::from(&health));
    client
        .request_value(method, &path, body.as_ref())
        .await
        .map_err(BridgeError::from)
}

fn validate_native_agent_request(
    method: &Method,
    body: Option<&Value>,
) -> Result<AgentAction, String> {
    if *method != Method::POST {
        return Err("Agent lifecycle adapter accepts POST only".to_owned());
    }
    let command = parse_command(body)?;
    match command.action.as_str() {
        "start" => Ok(AgentAction::Start),
        "stop" => Ok(AgentAction::Stop),
        "restart" => Ok(AgentAction::Restart),
        "status" => Ok(AgentAction::Status),
        _ => Err(format!("Agent action is not allowed: {}", command.action)),
    }
}

fn parse_command(body: Option<&Value>) -> Result<AgentCommand, String> {
    let body = body.ok_or_else(|| "POST requests require a JSON body".to_owned())?;
    let encoded = serde_json::to_vec(body)
        .map_err(|error| format!("request body could not be encoded: {error}"))?;
    if encoded.len() > MAX_REQUEST_BODY_BYTES {
        return Err(format!(
            "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
        ));
    }
    serde_json::from_value(body.clone())
        .map_err(|error| format!("request body must contain an action: {error}"))
}

async fn execute_agent_action(state: &AppState, action: AgentAction) -> Result<Value, String> {
    match action {
        AgentAction::Status => serde_json::to_value(state.runtime.status().await)
            .map_err(|error| format!("Agent status could not be encoded: {error}")),
        AgentAction::Start | AgentAction::Restart => {
            state.set_desired_running(true);
            state
                .runtime
                .action(action, START_WAIT_SECS)
                .await
                .map_err(|error| error.to_string())?;
            // A restarted Agent starts with a fresh Harness supervisor.  Run
            // the same best-effort configured-Harness bootstrap used during
            // the first GUI handshake so an explicit Agent restart does not
            // leave the saved Harness idle.
            schedule_configured_harness(state.clone());
            let name = if action == AgentAction::Start {
                "start"
            } else {
                "restart"
            };
            Ok(json!({ "accepted": true, "action": name }))
        }
        AgentAction::Stop => {
            state.set_desired_running(false);
            state
                .runtime
                .action(action, STOP_WAIT_SECS)
                .await
                .map_err(|error| error.to_string())?;
            Ok(json!({ "accepted": true, "action": "stop" }))
        }
    }
}

fn is_allowed_route(path: &str) -> bool {
    nexus_launcher_core::is_allowed_agent_route(path) || path == "/v1/agent"
}


fn build_identity() -> Result<Value, String> {
    let root = std::env::var_os("NEXUS_DESKTOP_RESOURCES").ok_or("Desktop resources are missing")?;
    let path = PathBuf::from(root).join("release-identity.json");
    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { id: u64, command: String, #[serde(default)] args: Value }
async fn dispatch(state: &AppState, request: &Request) -> Result<Value, BridgeError> {
    let args = &request.args;
    match request.command.as_str() {
        "cancel_update" => {
            *state.update_lock.lock().map_err(|e| e.to_string())? = None;
            Ok(Value::Null)
        },
        "prepare_update" => {
            state.set_desired_running(false);
            match state.runtime.probe_ready().await {
                Ok(health) => {
                    let client = state.runtime.client().with_expected_identity(AgentIdentity::from(&health));
                    client.post_json::<_, Value>("/v1/lifecycle", &json!({"action":"shutdown_if_idle"})).await?;
                },
                Err(nexus_launcher_core::AgentRuntimeError::Client(nexus_launcher_core::AgentClientError::Transport(_))) => {},
                Err(error) => return Err(error.to_string().into()),
            }
            // Never call AgentRuntime::stop here: its explicit-user-stop timeout
            // can terminate an owned child. An updater must only defer on timeout.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                match state.runtime.probe().await {
                    Err(nexus_launcher_core::AgentRuntimeError::Client(nexus_launcher_core::AgentClientError::Transport(_))) => {
                        // Positive ownership proof, not merely an unreachable port.
                        let file = std::fs::OpenOptions::new().read(true).write(true)
                            .open(state.runtime.paths().run_dir.join("agent.lock")).map_err(|e| e.to_string())?;
                        if file.try_lock().is_ok() && state.runtime.child_pid().is_none() {
                            *state.update_lock.lock().map_err(|e| e.to_string())? = Some(file);
                            break;
                        }
                    },
                    Err(error) => return Err(error.to_string().into()),
                    Ok(_) => {},
                }
                if tokio::time::Instant::now() >= deadline { return Err("Agent has not exited; update deferred".to_owned().into()); }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(json!({"ready":true}))
        },
        "startup_status" => Ok(serde_json::to_value(startup_status(state).await?).unwrap()),
        "retry_startup" => Ok(serde_json::to_value(retry_startup(state).await?).unwrap()),
        "proxy_request" => proxy_request(state,
            args["method"].as_str().ok_or_else(|| "Missing method".to_owned())?.into(),
            args["path"].as_str().ok_or_else(|| "Missing path".to_owned())?.into(),
            args.get("body").filter(|v| !v.is_null()).cloned()).await,
        "agent_log_set" => {
            let level = args["level"].as_str().ok_or_else(|| "Missing log level".to_owned())?;
            if !["error", "warn", "info", "debug", "trace"].contains(&level) { return Err("Invalid log level".to_owned().into()); }
            nexus_launcher_core::set_agent_log_level(level); Ok(Value::Null)
        },
        "export_startup_diagnostics" => {
            let context = json!({"build": build_identity().ok(), "agent_startup_error": state.runtime.startup_error(),
                "observed_agent_error": args["observedError"].as_str().map(|v| v.chars().take(4096).collect::<String>())});
            let path = nexus_launcher_core::startup_diagnostics::export(state.runtime.paths(), context).map_err(|e| e.to_string())?;
            Ok(json!({"export_path":path, "source":"launcher"}))
        },
        _ => Err("Unknown desktop command".to_owned().into()),
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let resource = std::env::var_os("NEXUS_DESKTOP_RESOURCES").map(PathBuf::from);
    let state = AppState::new(resource)?;
    let mut input = io::stdin().lock();
    loop {
        // Bound input before parsing; a damaged host cannot allocate an unbounded line.
        let mut bytes = Vec::new();
        use std::io::Read;
        let count = input.by_ref().take((MAX_REQUEST_BODY_BYTES + 4096) as u64).read_until(b'\n', &mut bytes)?;
        if count == 0 { break; }
        if bytes.last() != Some(&b'\n') { return Err("Desktop request exceeds limit".into()); }
        let request: Request = serde_json::from_slice(&bytes)?;
        let response = match dispatch(&state, &request).await {
            Ok(value) => json!({"id":request.id,"value":value}),
            Err(error) => json!({"id":request.id,"error":error}),
        };
        let mut output = io::stdout().lock();
        serde_json::to_writer(&mut output, &response)?;
        writeln!(output)?;
        output.flush()?;
    }
    // EOF ends only the adapter. Closing Launcher must not terminate Agent or Harness.
    Ok(())
}
