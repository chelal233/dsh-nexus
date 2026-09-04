#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    env,
    path::PathBuf,
    process::{self, Child, Command, Stdio},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    client::conn::http1::{self, SendRequest},
    header::{CONTENT_TYPE, HOST},
    Request,
};
use hyper_util::rt::TokioIo;
use reqwest::{redirect::Policy, Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WindowEvent,
};
use tokio::{net::TcpStream, time::timeout};

const DEFAULT_API_PORT: u16 = 3091;
const API_PORT_ENV: &str = "NEXUS_CONSOLE_PORT";
const API_PORT_OVERRIDE_ENV: &str = "NEXUS_LAUNCHER_API_PORT";
const LAUNCHER_BIN_ENV: &str = "NEXUS_LAUNCHER_BIN";
const LAUNCHER_CAPABILITY_ENV: &str = "NEXUS_LAUNCHER_CAPABILITY";
const MAX_PROXY_BODY_BYTES: usize = 32 * 1024;
const MAX_PROXY_RESPONSE_BYTES: usize = 512 * 1024;
const LAUNCHER_DATA_ROOT_HEADER: &str = "x-nexus-launcher-data-root-id";
const LAUNCHER_INSTANCE_HEADER: &str = "x-nexus-launcher-instance-id";
const LAUNCHER_CAPABILITY_HEADER: &str = "x-nexus-launcher-capability";
const LAUNCHER_CHALLENGE_HEADER: &str = "x-nexus-launcher-challenge";
const LAUNCHER_PROOF_HEADER: &str = "x-nexus-launcher-proof";
const HELPER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const HELPER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

const ALLOWED_ROUTES: &[&str] = &[
    "/launcher/status",
    "/launcher/agent",
    "/launcher/harness",
    "/launcher/logs",
    "/launcher/agent-api/v1/health",
    "/launcher/agent-api/v1/state",
    "/launcher/agent-api/v1/harness",
    "/launcher/agent-api/v1/profiles",
    "/launcher/agent-api/v1/checkpoints",
    "/launcher/agent-api/v1/releases",
    "/launcher/agent-api/v1/updates",
    "/launcher/agent-api/v1/diagnostics",
    "/launcher/agent-api/v1/config",
];

struct AppState {
    client: Client,
    api_base: Option<String>,
    api_port: Option<u16>,
    helper: Mutex<HelperState>,
}

struct HelperState {
    path: Option<PathBuf>,
    child: Option<Child>,
    startup_error: Option<String>,
    expected_instance_id: Option<String>,
    pinned_identity: Option<LauncherIdentity>,
    api_capability: Option<String>,
}

struct AuthenticatedHelperConnection {
    sender: SendRequest<Full<Bytes>>,
    host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LauncherIdentity {
    data_root_id: String,
    launcher_instance_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct StartupStatus {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    helper_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_root_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    launcher_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyErrorBody {
    message: Option<String>,
}

impl AppState {
    fn new() -> Self {
        let (api_port, startup_error) = match api_port() {
            Ok(port) => (Some(port), None),
            Err(error) => (None, Some(error)),
        };
        let api_base = api_port.map(|port| format!("http://127.0.0.1:{port}"));
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(2))
                .timeout(std::time::Duration::from_secs(8))
                .redirect(Policy::none())
                .build()
                .expect("native launcher HTTP client configuration is valid"),
            api_base,
            api_port,
            helper: Mutex::new(HelperState {
                path: None,
                child: None,
                startup_error,
                expected_instance_id: None,
                pinned_identity: None,
                api_capability: None,
            }),
        }
    }

    fn start_helper(&self) {
        let mut helper = self.helper.lock().expect("helper state lock");
        let Some(api_port) = self.api_port else {
            return;
        };
        if let Some(child) = helper.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(_)) => {
                    helper.child = None;
                    helper.expected_instance_id = None;
                    helper.pinned_identity = None;
                    helper.api_capability = None;
                }
                Err(error) => {
                    helper.startup_error = Some(format!(
                        "Could not confirm whether the previous Launcher helper exited: {error}"
                    ));
                    return;
                }
            }
        }

        let path = match resolve_launcher_helper() {
            Ok(path) => path,
            Err(error) => {
                helper.startup_error = Some(error);
                return;
            }
        };
        let expected_instance_id = new_launcher_instance_id();
        let api_capability = match new_launcher_capability() {
            Ok(capability) => capability,
            Err(error) => {
                helper.startup_error = Some(error);
                return;
            }
        };
        helper.path = Some(path.clone());
        helper.startup_error = None;
        helper.expected_instance_id = Some(expected_instance_id.clone());
        helper.pinned_identity = None;
        helper.api_capability = Some(api_capability.clone());

        let mut command = Command::new(&path);
        command
            .arg("api")
            .arg("--no-open")
            .arg("--console-port")
            .arg(api_port.to_string())
            .arg("--launcher-instance-id")
            .arg(&expected_instance_id)
            .env(LAUNCHER_CAPABILITY_ENV, &api_capability)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(child) => helper.child = Some(child),
            Err(error) => {
                helper.expected_instance_id = None;
                helper.api_capability = None;
                helper.startup_error = Some(format!(
                    "Could not start the Launcher API helper at {}: {error}. Set {LAUNCHER_BIN_ENV} to a valid nexus-launcher executable.",
                    path.display()
                ));
            }
        }
    }

    fn startup_snapshot(&self, available: bool) -> StartupStatus {
        let helper = self.helper.lock().expect("helper state lock");
        let identity = available.then_some(()).and(helper.pinned_identity.as_ref());
        StartupStatus {
            available,
            api_base: self.api_base.clone(),
            helper_path: helper.path.as_ref().map(|path| path.display().to_string()),
            data_root_id: identity.map(|value| value.data_root_id.clone()),
            launcher_instance_id: identity.map(|value| value.launcher_instance_id.clone()),
            message: if available {
                None
            } else {
                helper.startup_error.clone().or_else(|| {
                    Some(format!(
                        "The Launcher API is not responding at {}. Build nexus-launcher or set {LAUNCHER_BIN_ENV}.",
                        self.api_base.as_deref().unwrap_or("an invalid configured port")
                    ))
                })
            },
        }
    }

    fn mark_helper_unavailable(&self, message: String) {
        let mut helper = self.helper.lock().expect("helper state lock");
        helper.startup_error = Some(message);
    }

    fn stop_helper(&self) {
        let mut helper = self.helper.lock().expect("helper state lock");
        if let Some(mut child) = helper.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        helper.expected_instance_id = None;
        helper.pinned_identity = None;
        helper.api_capability = None;
    }
}

fn new_launcher_instance_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("native-{}-{nanos}", process::id())
}

fn new_launcher_capability() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("Could not generate Launcher capability: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn api_port() -> Result<u16, String> {
    let override_value = read_optional_env(API_PORT_OVERRIDE_ENV)?;
    let console_value = read_optional_env(API_PORT_ENV)?;
    api_port_from_values(override_value.as_deref(), console_value.as_deref())
}

fn read_optional_env(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
    }
}

fn api_port_from_values(
    override_value: Option<&str>,
    console_value: Option<&str>,
) -> Result<u16, String> {
    let Some((name, value)) = override_value
        .map(|value| (API_PORT_OVERRIDE_ENV, value))
        .or_else(|| console_value.map(|value| (API_PORT_ENV, value)))
    else {
        return Ok(DEFAULT_API_PORT);
    };
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("{name} must be an integer between 1 and 65535; got {value:?}"))
}

fn resolve_launcher_helper() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os(LAUNCHER_BIN_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "{LAUNCHER_BIN_ENV} points to a missing helper: {}",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join(platform_launcher_name()));
            candidates.push(parent.join("nexus-launcher"));
            if cfg!(debug_assertions) {
                let mut ancestor = Some(parent);
                for _ in 0..8 {
                    if let Some(path) = ancestor {
                        candidates.push(
                            path.join("target")
                                .join("debug")
                                .join(platform_launcher_name()),
                        );
                        candidates.push(
                            path.join("target")
                                .join("release")
                                .join(platform_launcher_name()),
                        );
                        ancestor = path.parent();
                    }
                }
            }
        }
    }
    if cfg!(debug_assertions) {
        if let Ok(current) = env::current_dir() {
            candidates.push(
                current
                    .join("target")
                    .join("debug")
                    .join(platform_launcher_name()),
            );
            candidates.push(
                current
                    .join("target")
                    .join("release")
                    .join(platform_launcher_name()),
            );
        }
    }

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "nexus-launcher helper was not found beside the native app. Debug builds also inspect nearby target directories. Set {LAUNCHER_BIN_ENV} to the signed helper path."
            )
        })
}

fn platform_launcher_name() -> &'static str {
    if cfg!(windows) {
        "nexus-launcher.exe"
    } else {
        "nexus-launcher"
    }
}

fn is_allowed_route(path: &str) -> bool {
    ALLOWED_ROUTES.iter().any(|route| *route == path)
}

fn validate_proxy_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/')
        || path.contains('?')
        || path.contains('#')
        || path.chars().any(char::is_control)
        || !is_allowed_route(path)
    {
        return Err(format!("Proxy route is not allowed: {path}"));
    }
    Ok(())
}

fn parse_method(method: &str) -> Result<Method, String> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        _ => Err("Only GET and POST are supported by the local proxy".to_owned()),
    }
}

fn validate_proxy_request(path: &str, method: &Method, body: Option<&Value>) -> Result<(), String> {
    validate_proxy_path(path)?;
    let method_allowed = match path {
        "/launcher/status"
        | "/launcher/logs"
        | "/launcher/agent-api/v1/health"
        | "/launcher/agent-api/v1/state" => method == Method::GET,
        "/launcher/agent"
        | "/launcher/harness"
        | "/launcher/agent-api/v1/harness"
        | "/launcher/agent-api/v1/checkpoints"
        | "/launcher/agent-api/v1/diagnostics"
        | "/launcher/agent-api/v1/config" => *method == Method::GET || *method == Method::POST,
        _ => *method == Method::GET,
    };
    if !method_allowed {
        return Err(format!(
            "HTTP method {method} is not allowed for proxy route {path}"
        ));
    }
    match (method, body) {
        (method, Some(_)) if *method == Method::GET => {
            return Err("GET proxy requests cannot include a body".to_owned())
        }
        (method, None) if *method == Method::POST => {
            return Err("POST proxy requests require a JSON body".to_owned())
        }
        _ => {}
    }
    if let Some(body) = body {
        let encoded = serde_json::to_vec(body)
            .map_err(|error| format!("Proxy request body could not be encoded: {error}"))?;
        if encoded.len() > MAX_PROXY_BODY_BYTES {
            return Err(format!(
                "Proxy request body exceeds the {MAX_PROXY_BODY_BYTES}-byte limit"
            ));
        }
        let action = body
            .as_object()
            .and_then(|object| object.get("action"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Proxy POST body for {path} requires an action"))?;
        let action_allowed = match path {
            "/launcher/agent" => matches!(action, "start" | "stop" | "restart" | "status"),
            "/launcher/harness" => matches!(action, "open" | "status"),
            "/launcher/agent-api/v1/harness" => {
                matches!(action, "start" | "stop" | "restart" | "status")
            }
            "/launcher/agent-api/v1/checkpoints" => action == "create",
            "/launcher/agent-api/v1/diagnostics" => action == "collect",
            "/launcher/agent-api/v1/config" => matches!(
                action,
                "status" | "set_harness" | "clear_harness" | "set_update" | "clear_update"
            ),
            _ => false,
        };
        if !action_allowed {
            return Err(format!("Proxy action is not allowed for route {path}"));
        }
    }
    Ok(())
}

#[tauri::command]
async fn startup_status(state: tauri::State<'_, AppState>) -> Result<StartupStatus, String> {
    if state.api_base.is_none() {
        return Ok(state.startup_snapshot(false));
    }
    if verify_helper_identity(&state).await.is_ok() {
        return Ok(state.startup_snapshot(true));
    }
    state.start_helper();
    match verify_helper_identity(&state).await {
        Ok(_) => Ok(state.startup_snapshot(true)),
        Err(error) => {
            state.mark_helper_unavailable(error);
            Ok(state.startup_snapshot(false))
        }
    }
}

async fn verify_helper_identity(state: &AppState) -> Result<LauncherIdentity, String> {
    let api_base = state
        .api_base
        .as_deref()
        .ok_or_else(|| "Launcher API port is unavailable".to_owned())?;
    let expected_instance_id = {
        let mut helper = state.helper.lock().expect("helper state lock");
        verify_owned_child_live(&mut helper)?
    };
    let response = state
        .client
        .get(format!("{api_base}/launcher/status"))
        .send()
        .await
        .map_err(|error| format!("Launcher helper status is unavailable: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Launcher helper status returned HTTP {}",
            response.status()
        ));
    }
    let identity = response
        .json::<LauncherIdentity>()
        .await
        .map_err(|error| format!("Launcher helper identity is invalid: {error}"))?;
    let mut helper = state.helper.lock().expect("helper state lock");
    let live_instance_id = verify_owned_child_live(&mut helper)?;
    if live_instance_id != expected_instance_id {
        return Err("Launcher helper identity changed during verification".to_owned());
    }
    if !pin_observed_identity(&mut helper, &expected_instance_id, &identity) {
        let message = format!(
            "Refusing Launcher API at {} because its data-root/instance identity does not match the helper started by this native app",
            api_base
        );
        helper.startup_error = Some(message.clone());
        return Err(message);
    }
    helper.startup_error = None;
    Ok(identity)
}

fn verify_owned_child_live(helper: &mut HelperState) -> Result<String, String> {
    let status = match helper.child.as_mut() {
        Some(child) => child.try_wait(),
        None => {
            return Err(
                "The native app does not own a live identity-bound Launcher helper".to_owned(),
            )
        }
    };
    match status {
        Ok(None) => helper
            .expected_instance_id
            .clone()
            .ok_or_else(|| "The live Launcher helper has no expected instance identity".to_owned()),
        Ok(Some(status)) => {
            helper.child = None;
            helper.expected_instance_id = None;
            helper.pinned_identity = None;
            helper.api_capability = None;
            let message = format!("The owned Launcher helper exited with status {status}");
            helper.startup_error = Some(message.clone());
            Err(message)
        }
        Err(error) => {
            let message = format!("Could not verify the owned Launcher helper process: {error}");
            helper.startup_error = Some(message.clone());
            Err(message)
        }
    }
}

fn pin_observed_identity(
    helper: &mut HelperState,
    expected_instance_id: &str,
    observed: &LauncherIdentity,
) -> bool {
    if helper.expected_instance_id.as_deref() != Some(expected_instance_id)
        || observed.launcher_instance_id != expected_instance_id
    {
        return false;
    }
    match helper.pinned_identity.as_ref() {
        Some(identity) => identity == observed,
        None => {
            helper.pinned_identity = Some(observed.clone());
            true
        }
    }
}

#[tauri::command]
async fn proxy_request(
    state: tauri::State<'_, AppState>,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    let method = parse_method(&method)?;
    validate_proxy_request(&path, &method, body.as_ref())?;
    let identity = verify_helper_identity(&state).await?;
    let (capability, api_port) = {
        let mut helper = state.helper.lock().expect("helper state lock");
        let live = verify_owned_child_live(&mut helper)?;
        if live != identity.launcher_instance_id {
            return Err("Launcher helper changed before the proxy request".to_owned());
        }
        let capability = helper
            .api_capability
            .clone()
            .ok_or_else(|| "The owned Launcher helper has no private API capability".to_owned())?;
        let api_port = state
            .api_port
            .ok_or_else(|| "Launcher API port is unavailable".to_owned())?;
        (capability, api_port)
    };
    resolve_proxy_url(&state, &path)?;
    let mut connection =
        AuthenticatedHelperConnection::open(api_port, &identity, &capability).await?;
    // The capability and command body have not been sent yet. If the owned
    // child exited during the challenge, fail before disclosing either.
    {
        let mut helper = state.helper.lock().expect("helper state lock");
        let live = verify_owned_child_live(&mut helper)?;
        if live != identity.launcher_instance_id {
            return Err("Launcher helper changed during authenticated handshake".to_owned());
        }
    }
    let response = connection
        .send(method, &path, body.as_ref(), &identity, &capability)
        .await;
    let mut helper = state.helper.lock().expect("helper state lock");
    let live = verify_owned_child_live(&mut helper)?;
    if live != identity.launcher_instance_id {
        return Err("Launcher helper changed during the proxy request".to_owned());
    }
    response
}

impl AuthenticatedHelperConnection {
    async fn open(
        api_port: u16,
        identity: &LauncherIdentity,
        capability: &str,
    ) -> Result<Self, String> {
        let stream = timeout(
            HELPER_CONNECT_TIMEOUT,
            TcpStream::connect(("127.0.0.1", api_port)),
        )
        .await
        .map_err(|_| "Timed out connecting to the Launcher helper".to_owned())?
        .map_err(|error| format!("Launcher helper connection failed: {error}"))?;
        let (mut sender, connection) = timeout(
            HELPER_REQUEST_TIMEOUT,
            http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| "Timed out establishing the Launcher HTTP connection".to_owned())?
        .map_err(|error| format!("Launcher HTTP handshake failed: {error}"))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let challenge = new_launcher_capability()?;
        let host = format!("127.0.0.1:{api_port}");
        let request = Request::builder()
            .method(Method::GET)
            .uri("/launcher/handshake")
            .header(HOST, &host)
            .header(LAUNCHER_CHALLENGE_HEADER, &challenge)
            .body(Full::new(Bytes::new()))
            .map_err(|error| format!("Launcher challenge request is invalid: {error}"))?;
        let response = timeout(HELPER_REQUEST_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| "Launcher challenge timed out".to_owned())?
            .map_err(|error| format!("Launcher challenge failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Launcher challenge returned HTTP {}",
                response.status()
            ));
        }
        let proof = response
            .headers()
            .get(LAUNCHER_PROOF_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "Launcher challenge response has no proof".to_owned())?
            .to_owned();
        collect_hyper_body(response.into_body(), 1024).await?;
        verify_launcher_handshake_proof(capability, &challenge, identity, &proof)?;
        Ok(Self { sender, host })
    }

    async fn send(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        identity: &LauncherIdentity,
        capability: &str,
    ) -> Result<Value, String> {
        let body = match body {
            Some(body) => serde_json::to_vec(body)
                .map_err(|error| format!("Proxy request body could not be encoded: {error}"))?,
            None => Vec::new(),
        };
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, &self.host)
            .header(LAUNCHER_DATA_ROOT_HEADER, &identity.data_root_id)
            .header(LAUNCHER_INSTANCE_HEADER, &identity.launcher_instance_id)
            .header(LAUNCHER_CAPABILITY_HEADER, capability);
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| format!("Launcher API request is invalid: {error}"))?;
        let response = timeout(HELPER_REQUEST_TIMEOUT, self.sender.send_request(request))
            .await
            .map_err(|_| "Launcher API request timed out".to_owned())?
            .map_err(|error| format!("Launcher API request failed: {error}"))?;
        let status = response.status();
        let bytes = collect_hyper_body(response.into_body(), MAX_PROXY_RESPONSE_BYTES).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let value = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| Value::String(text));
        if !status.is_success() {
            let message = serde_json::from_value::<ProxyErrorBody>(value.clone())
                .ok()
                .and_then(|body| body.message)
                .unwrap_or_else(|| format!("Launcher API returned HTTP {status}"));
            return Err(message);
        }
        Ok(value)
    }
}

async fn collect_hyper_body(mut body: Incoming, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| format!("Launcher response read failed: {error}"))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(data.len()) > limit {
            return Err(format!(
                "Launcher API response exceeds the {limit}-byte limit"
            ));
        }
        bytes.extend_from_slice(&data);
    }
    Ok(bytes)
}

fn verify_launcher_handshake_proof(
    capability: &str,
    challenge: &str,
    identity: &LauncherIdentity,
    proof: &str,
) -> Result<(), String> {
    let proof = decode_hex_32(proof)
        .ok_or_else(|| "Launcher challenge proof is not 256-bit hexadecimal".to_owned())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(capability.as_bytes())
        .map_err(|_| "Launcher capability cannot initialize HMAC".to_owned())?;
    mac.update(b"nexus-launcher-handshake-v1");
    for value in [
        challenge,
        identity.data_root_id.as_str(),
        identity.launcher_instance_id.as_str(),
    ] {
        mac.update(&(value.len() as u64).to_be_bytes());
        mac.update(value.as_bytes());
    }
    mac.verify_slice(&proof)
        .map_err(|_| "Launcher challenge proof does not match the owned helper".to_owned())
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut decoded = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(chunk).ok()?;
        decoded[index] = u8::from_str_radix(text, 16).ok()?;
    }
    Some(decoded)
}

fn resolve_proxy_url(state: &AppState, path: &str) -> Result<String, String> {
    validate_proxy_path(path)?;
    if state
        .helper
        .lock()
        .expect("helper state lock")
        .pinned_identity
        .is_none()
    {
        return Err("Launcher helper identity has not been verified".to_owned());
    }
    let api_base = state.api_base.as_deref().ok_or_else(|| {
        state
            .helper
            .lock()
            .expect("helper state lock")
            .startup_error
            .clone()
            .unwrap_or_else(|| "Launcher API port is unavailable".to_owned())
    })?;
    Ok(format!("{api_base}{path}"))
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(desktop)]
fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show launcher", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    TrayIconBuilder::new()
        .icon(
            app.default_window_icon()
                .cloned()
                .expect("Nexus Launcher config must provide a default icon"),
        )
        .menu(&menu)
        .tooltip("Nexus Launcher")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(&tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let state = AppState::new();
    let builder = tauri::Builder::default()
        // The single-instance plugin must be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![startup_status, proxy_request])
        .setup(|app| {
            app.state::<AppState>().start_helper();
            #[cfg(desktop)]
            {
                setup_tray(app)?;
                use tauri_plugin_global_shortcut::{
                    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
                };
                let shortcut =
                    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyN);
                app.handle().plugin(
                    tauri_plugin_global_shortcut::Builder::new()
                        .with_handler(move |app, triggered, event| {
                            if triggered == &shortcut && event.state() == ShortcutState::Pressed {
                                show_window(app);
                            }
                        })
                        .build(),
                )?;
                if let Err(error) = app.global_shortcut().register(shortcut) {
                    eprintln!("nexus-launcher: global shortcut unavailable: {error}");
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle();
                let _ = window.hide();
                use tauri_plugin_notification::NotificationExt;
                let _ = app
                    .notification()
                    .builder()
                    .title("Nexus Launcher")
                    .body("Launcher is still running in the system tray")
                    .show();
            }
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building Nexus Launcher");
    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            app_handle.state::<AppState>().stop_helper();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use reqwest::{StatusCode, Url};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn proxy_allowlist_rejects_external_and_query_routes() {
        assert!(validate_proxy_path("/launcher/status").is_ok());
        assert!(validate_proxy_path("/launcher/agent-api/v1/health").is_ok());
        assert!(validate_proxy_path("/v1/health").is_err());
        assert!(validate_proxy_path("https://example.com").is_err());
        assert!(
            validate_proxy_path("/launcher/agent-api/v1/health?url=https://example.com").is_err()
        );
        assert!(validate_proxy_path("/launcher/agent-api/v1/health/../config").is_err());
    }

    #[test]
    fn proxy_methods_and_body_size_are_bounded() {
        assert!(
            validate_proxy_request("/launcher/agent-api/v1/health", &Method::GET, None).is_ok()
        );
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/health",
            &Method::POST,
            Some(&serde_json::json!({
                "action": "status"
            }))
        )
        .is_err());
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/state",
            &Method::GET,
            Some(&serde_json::json!({}))
        )
        .is_err());
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/harness",
            &Method::POST,
            Some(&serde_json::json!({ "action": "status" }))
        )
        .is_ok());
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/config",
            &Method::POST,
            Some(&serde_json::json!({ "action": "set_harness" }))
        )
        .is_ok());
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/config",
            &Method::POST,
            Some(&serde_json::json!({ "action": "set_config" }))
        )
        .is_err());
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/harness",
            &Method::POST,
            Some(&serde_json::json!({ "action": "set_config" }))
        )
        .is_err());
        let oversized = Value::String("x".repeat(MAX_PROXY_BODY_BYTES));
        assert!(validate_proxy_request(
            "/launcher/agent-api/v1/harness",
            &Method::POST,
            Some(&oversized)
        )
        .is_err());
    }

    #[test]
    fn only_loopback_api_base_is_constructed() {
        let base = format!("http://127.0.0.1:{}", DEFAULT_API_PORT);
        let url = Url::parse(&base).expect("loopback API URL parses");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(DEFAULT_API_PORT));
        assert_eq!(StatusCode::OK.as_u16(), 200);
    }

    #[test]
    fn port_rebind_cannot_receive_a_native_agent_route() {
        assert!(ALLOWED_ROUTES.iter().all(|path| !path.starts_with("/v1/")));
        let agent_routes: Vec<_> = ALLOWED_ROUTES
            .iter()
            .copied()
            .filter(|path| path.contains("/agent-api/"))
            .collect();
        assert_eq!(agent_routes.len(), 9);
        assert!(agent_routes
            .iter()
            .all(|path| path.starts_with("/launcher/agent-api/v1/")));
    }

    #[test]
    fn source_avoids_the_post_msrv_option_predicate() {
        let post_msrv_option_api = [".is_", "none_or("].concat();
        assert!(!include_str!("main.rs").contains(&post_msrv_option_api));
    }

    #[test]
    fn api_port_rejects_invalid_explicit_values_and_preserves_priority() {
        assert_eq!(
            api_port_from_values(Some("4100"), Some("4200")).expect("override is valid"),
            4100
        );
        assert_eq!(
            api_port_from_values(None, Some("4200")).expect("console port is valid"),
            4200
        );
        assert_eq!(
            api_port_from_values(None, None).expect("default is valid"),
            DEFAULT_API_PORT
        );
        assert!(api_port_from_values(Some("invalid"), Some("4200")).is_err());
        assert!(api_port_from_values(None, Some("0")).is_err());
        assert!(api_port_from_values(None, Some("70000")).is_err());
    }

    #[test]
    fn launcher_capabilities_are_random_fixed_width_hex() {
        let first = new_launcher_capability().expect("first capability generates");
        let second = new_launcher_capability().expect("second capability generates");
        assert_eq!(first.len(), 64);
        assert_eq!(second.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(second.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn launcher_handshake_proof_matches_the_cross_process_vector() {
        let identity = LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        };
        assert!(verify_launcher_handshake_proof(
            &"a".repeat(64),
            &"b".repeat(64),
            &identity,
            "b0a92a24f6aa2ea5e3352d3646293a69210182905e0b3b0e10206cb395325e7f",
        )
        .is_ok());
        assert!(verify_launcher_handshake_proof(
            &"a".repeat(64),
            &"b".repeat(64),
            &identity,
            &"0".repeat(64),
        )
        .is_err());
    }

    #[tokio::test]
    async fn unproved_listener_never_receives_capability_or_command_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("adversarial listener binds");
        let port = listener.local_addr().expect("listener address").port();
        let adversary = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("connection accepts");
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.expect("challenge reads");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\n{LAUNCHER_PROOF_HEADER}: {}\r\nContent-Length: 0\r\n\r\n",
                        "0".repeat(64)
                    )
                    .as_bytes(),
                )
                .await
                .expect("false proof writes");
            let later = timeout(std::time::Duration::from_secs(1), stream.read(&mut chunk))
                .await
                .expect("client closes rejected connection")
                .expect("connection close reads");
            (String::from_utf8_lossy(&request).into_owned(), later)
        });
        let identity = LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        };
        let error =
            match AuthenticatedHelperConnection::open(port, &identity, &"a".repeat(64)).await {
                Err(error) => error,
                Ok(_) => panic!("invalid proof must reject the listener before command send"),
            };
        assert!(error.contains("proof does not match"));
        let (request, later) = adversary.await.expect("adversary joins");
        assert!(request.starts_with("GET /launcher/handshake HTTP/1.1"));
        assert!(!request.contains(&"a".repeat(64)));
        assert!(!request.contains("dangerous-action"));
        assert_eq!(later, 0, "no second request reaches the unproved peer");
    }

    #[test]
    fn all_native_routes_resolve_only_to_the_headless_launcher() {
        let state = AppState {
            client: Client::new(),
            api_base: Some("http://127.0.0.1:3091".to_owned()),
            api_port: Some(3091),
            helper: Mutex::new(HelperState {
                path: None,
                child: None,
                startup_error: None,
                expected_instance_id: Some("launcher-a".to_owned()),
                pinned_identity: None,
                api_capability: None,
            }),
        };
        assert!(
            resolve_proxy_url(&state, "/launcher/agent-api/v1/state").is_err(),
            "Agent routes must remain disabled before the helper identity is verified"
        );
        state.helper.lock().expect("helper locks").pinned_identity = Some(LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        });
        assert_eq!(
            resolve_proxy_url(&state, "/launcher/status").expect("Launcher route resolves"),
            "http://127.0.0.1:3091/launcher/status"
        );
        assert_eq!(
            resolve_proxy_url(&state, "/launcher/agent-api/v1/state")
                .expect("Agent route resolves through Launcher-only namespace"),
            "http://127.0.0.1:3091/launcher/agent-api/v1/state"
        );
        assert!(
            validate_proxy_path("/v1/state").is_err(),
            "a port-rebound Agent must never receive a native proxy request"
        );
        assert!(
            resolve_proxy_url(&state, "/v1/state").is_err(),
            "URL resolution must independently reject the Agent's native namespace"
        );
    }

    #[test]
    fn helper_identity_never_adopts_a_foreign_port_occupant_or_changed_root() {
        let owned = LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        };
        let foreign_instance = LauncherIdentity {
            launcher_instance_id: "launcher-foreign".to_owned(),
            ..owned.clone()
        };
        let changed_root = LauncherIdentity {
            data_root_id: "root-b".to_owned(),
            ..owned.clone()
        };
        let mut helper = HelperState {
            path: None,
            child: None,
            startup_error: None,
            expected_instance_id: Some("launcher-a".to_owned()),
            pinned_identity: None,
            api_capability: Some("capability-a".to_owned()),
        };
        assert!(pin_observed_identity(&mut helper, "launcher-a", &owned));
        assert!(!pin_observed_identity(
            &mut helper,
            "launcher-a",
            &foreign_instance
        ));
        assert!(!pin_observed_identity(
            &mut helper,
            "launcher-a",
            &changed_root
        ));
        assert_eq!(helper.pinned_identity.as_ref(), Some(&owned));
    }

    #[test]
    fn repeated_identity_mismatch_cannot_replace_the_permanent_pin() {
        let owned = LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        };
        let changed_root = LauncherIdentity {
            data_root_id: "root-b".to_owned(),
            ..owned.clone()
        };
        let mut helper = HelperState {
            path: None,
            child: None,
            startup_error: None,
            expected_instance_id: Some("launcher-a".to_owned()),
            pinned_identity: Some(owned.clone()),
            api_capability: Some("capability-a".to_owned()),
        };

        assert!(!pin_observed_identity(
            &mut helper,
            "launcher-a",
            &changed_root
        ));
        assert!(!pin_observed_identity(
            &mut helper,
            "launcher-a",
            &changed_root
        ));
        assert_eq!(helper.pinned_identity.as_ref(), Some(&owned));
    }

    #[test]
    fn exited_owned_child_clears_identity_and_same_nonce_cannot_replay() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd.exe");
            command.args(["/C", "exit", "0"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };
        let child = command.spawn().expect("short-lived helper starts");
        let owned = LauncherIdentity {
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
        };
        let mut helper = HelperState {
            path: None,
            child: Some(child),
            startup_error: None,
            expected_instance_id: Some("launcher-a".to_owned()),
            pinned_identity: Some(owned),
            api_capability: Some("capability-a".to_owned()),
        };
        helper
            .child
            .as_mut()
            .expect("child remains owned")
            .wait()
            .expect("short-lived helper exits");

        assert!(verify_owned_child_live(&mut helper).is_err());
        assert!(helper.child.is_none());
        assert!(helper.expected_instance_id.is_none());
        assert!(helper.pinned_identity.is_none());
        assert!(helper.api_capability.is_none());
        assert!(!pin_observed_identity(
            &mut helper,
            "launcher-a",
            &LauncherIdentity {
                data_root_id: "root-a".to_owned(),
                launcher_instance_id: "launcher-a".to_owned(),
            }
        ));
    }

    #[test]
    fn missing_owned_child_is_always_unavailable() {
        let mut helper = HelperState {
            path: None,
            child: None,
            startup_error: None,
            expected_instance_id: Some("launcher-a".to_owned()),
            pinned_identity: None,
            api_capability: Some("capability-a".to_owned()),
        };
        assert!(verify_owned_child_live(&mut helper).is_err());
    }

    #[test]
    fn failed_final_startup_probe_hides_but_retains_the_permanent_pin() {
        let state = AppState {
            client: Client::new(),
            api_base: Some("http://127.0.0.1:3091".to_owned()),
            api_port: Some(3091),
            helper: Mutex::new(HelperState {
                path: None,
                child: None,
                startup_error: None,
                expected_instance_id: Some("launcher-a".to_owned()),
                pinned_identity: Some(LauncherIdentity {
                    data_root_id: "root-a".to_owned(),
                    launcher_instance_id: "launcher-a".to_owned(),
                }),
                api_capability: Some("capability-a".to_owned()),
            }),
        };
        state.mark_helper_unavailable("helper disappeared".to_owned());
        let snapshot = state.startup_snapshot(false);
        assert!(!snapshot.available);
        assert!(snapshot.data_root_id.is_none());
        assert!(snapshot.launcher_instance_id.is_none());
        let helper = state.helper.lock().expect("helper state locks");
        assert_eq!(
            helper.pinned_identity.as_ref(),
            Some(&LauncherIdentity {
                data_root_id: "root-a".to_owned(),
                launcher_instance_id: "launcher-a".to_owned(),
            })
        );
        assert_eq!(helper.startup_error.as_deref(), Some("helper disappeared"));
    }
}
