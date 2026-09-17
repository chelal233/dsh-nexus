//! UI-independent runtime contracts for Nexus Launcher clients.
//!
//! The Electron shell and the legacy headless launcher both use this crate for
//! loopback Agent transport and Agent lifecycle ownership.  The Agent HTTP
//! JSON API is the stable boundary; this crate provides the shared Rust
//! transport and lifecycle adapter used by the desktop stdio bridge.

use std::{
    env, fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures_util::StreamExt;
use nexus_core::{data_root_identity, new_instance_id, NexusConfig, NexusPaths};
use nexus_protocol::{ErrorResponse, HealthResponse, HealthStatus, LifecycleAccepted};
use reqwest::{header, Method, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    process::Command,
    sync::Mutex as AsyncMutex,
    time::{sleep, timeout, Instant},
};

pub mod harness_ui;
pub mod startup_diagnostics;
#[cfg(windows)]
mod windows_agent;
#[cfg(windows)]
use windows_agent::Child;
#[cfg(not(windows))]
use tokio::process::Child;

pub use harness_ui::{
    harness_observation_matches_session, parse_loopback_harness_url, read_harness_ui_info,
    read_harness_ui_info_with_observer, unavailable_harness_ui_info, HarnessLogObserver,
    HarnessLogSnapshot, HarnessUiInfo, HarnessUrlCandidate, HARNESS_LOG_TAIL_BYTES,
};

pub const DEFAULT_AGENT_PORT: u16 = nexus_core::DEFAULT_AGENT_PORT;
pub const DEFAULT_START_WAIT_SECS: u64 = 20;
pub const DEFAULT_STOP_WAIT_SECS: u64 = 15;
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024;
pub const MAX_RESPONSE_BODY_BYTES: usize = 512 * 1024;
pub const AGENT_BINARY_ENV: &str = "NEXUS_AGENT_BIN";
pub const AGENT_DATA_ROOT_HEADER: &str = "x-nexus-data-root-id";
pub const AGENT_INSTANCE_HEADER: &str = "x-nexus-instance-id";

// Agent connections are exclusively IPv4 loopback. Bound absent-listener
// retries separately from the request budget; slow business work still gets
// its full response deadline after the TCP connection is established.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const OFFLINE_PREVIEW_TIMEOUT: Duration = Duration::from_secs(1830);
const AGENT_ROUTES: &[&str] = &[
    "/v1/health",
    "/v1/state",
    "/v1/harness",
    "/v1/harness/startup",
    "/v1/harness/ui",
    "/v1/notifications",
    "/v1/market",
    "/v1/desktop/profile",
    "/v1/harness/discover",
    "/v1/profiles",
    "/v1/recovery",
    "/v1/recovery/records",
    "/v1/preflight",
    "/v1/checkpoints",
    "/v1/releases",
    "/v1/releases/tags",
    "/v1/runtime",
    "/v1/runtime/plan",
    "/v1/updates",
    "/v1/requests",
    "/v1/canary",
    "/v1/diagnostics",
    "/v1/config",
    "/v1/maintenance",
    "/v1/lifecycle",
    "/v1/shutdown",
];

/// Identity advertised by the Agent health endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentIdentity {
    pub data_root_id: String,
    pub instance_id: String,
}

impl From<&HealthResponse> for AgentIdentity {
    fn from(response: &HealthResponse) -> Self {
        Self {
            data_root_id: response.data_root_id.clone(),
            instance_id: response.instance_id.clone(),
        }
    }
}

/// Stable JSON shape exposed by the native bridge for startup and status UI.
/// It contains only process metadata and the Agent's public loopback identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStatus {
    pub available: bool,
    pub running: bool,
    pub api_base: String,
    pub data_root: String,
    pub data_root_id: Option<String>,
    pub instance_id: Option<String>,
    pub agent_pid: Option<u32>,
    pub agent_program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum AgentClientError {
    InvalidPort(u16),
    InvalidPath(String),
    InvalidRequest(String),
    Transport(String),
    ResponseTooLarge {
        limit: usize,
    },
    InvalidResponse(String),
    Http {
        status: StatusCode,
        message: String,
        body: Vec<u8>,
    },
}

impl fmt::Display for AgentClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPort(port) => write!(
                formatter,
                "Agent port must be between 1 and 65535; got {port}"
            ),
            Self::InvalidPath(path) => write!(formatter, "Agent route is not allowed: {path}"),
            Self::InvalidRequest(message) => {
                write!(formatter, "Agent request is invalid: {message}")
            }
            Self::Transport(message) => write!(formatter, "Agent request failed: {message}"),
            Self::ResponseTooLarge { limit } => {
                write!(formatter, "Agent response exceeds the {limit}-byte limit")
            }
            Self::InvalidResponse(message) => {
                write!(formatter, "Agent response is invalid: {message}")
            }
            Self::Http {
                status, message, ..
            } => {
                write!(formatter, "Agent returned HTTP {status}: {message}")
            }
        }
    }
}

impl std::error::Error for AgentClientError {}

/// Bounded loopback HTTP client for the versioned Agent API.
#[derive(Clone)]
pub struct AgentClient {
    http: reqwest::Client,
    base_url: Url,
    expected_identity: Option<AgentIdentity>,
    credential_paths: Option<NexusPaths>,
}

/// A bounded Agent response with its HTTP status retained for compatibility
/// bridges that need to preserve non-200 success responses such as 201 or
/// 204. The response body is already capped by `MAX_RESPONSE_BODY_BYTES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResponse<T> {
    pub status: StatusCode,
    pub body: T,
}

impl fmt::Debug for AgentClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentClient")
            .field("base_url", &self.base_url)
            .field("expected_identity", &self.expected_identity)
            .finish_non_exhaustive()
    }
}

impl AgentClient {
    pub fn new(port: u16) -> Result<Self, AgentClientError> {
        if port == 0 {
            return Err(AgentClientError::InvalidPort(port));
        }
        let base_url = Url::parse(&format!("http://127.0.0.1:{port}"))
            .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            http,
            base_url,
            expected_identity: None,
            credential_paths: None,
        })
    }

    /// Construct a client around an existing bounded reqwest client.
    ///
    /// Compatibility callers can preserve their own short test or recovery
    /// timeout while still using the shared route, body, and identity checks.
    pub fn from_reqwest(port: u16, http: reqwest::Client) -> Result<Self, AgentClientError> {
        if port == 0 {
            return Err(AgentClientError::InvalidPort(port));
        }
        let base_url = Url::parse(&format!("http://127.0.0.1:{port}"))
            .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            http,
            base_url,
            expected_identity: None,
            credential_paths: None,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn with_expected_identity(mut self, identity: AgentIdentity) -> Self {
        self.expected_identity = Some(identity);
        self
    }

    pub fn expected_identity(&self) -> Option<&AgentIdentity> {
        self.expected_identity.as_ref()
    }

    pub fn with_credential_paths(mut self, paths: NexusPaths) -> Self {
        self.credential_paths = Some(paths);
        self
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, AgentClientError> {
        self.request_json(Method::GET, path, None).await
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, AgentClientError> {
        let encoded = serde_json::to_vec(body)
            .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        if encoded.len() > MAX_REQUEST_BODY_BYTES {
            return Err(AgentClientError::InvalidRequest(format!(
                "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
            )));
        }
        self.request_json(Method::POST, path, Some(encoded)).await
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T, AgentClientError> {
        self.request_json(Method::POST, path, Some(Vec::new()))
            .await
    }

    /// Send a pre-encoded JSON value for bridge and compatibility clients.
    pub async fn request_value(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, AgentClientError> {
        self.request_value_with_status(method, path, body)
            .await
            .map(|response| response.body)
    }

    /// Send a bounded JSON request while retaining the Agent's success status.
    /// An empty successful body is represented as JSON `null`, allowing a
    /// caller to return a genuine 204 without inventing a response payload.
    pub async fn request_value_with_status(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<AgentResponse<Value>, AgentClientError> {
        let encoded = match body {
            Some(value) => {
                let encoded = serde_json::to_vec(value)
                    .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
                if encoded.len() > MAX_REQUEST_BODY_BYTES {
                    return Err(AgentClientError::InvalidRequest(format!(
                        "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
                    )));
                }
                Some(encoded)
            }
            None => None,
        };
        let response = self.request_raw(method, path, encoded).await?;
        let body = if response.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&response.body)
                .map_err(|error| AgentClientError::InvalidResponse(error.to_string()))?
        };
        Ok(AgentResponse {
            status: response.status,
            body,
        })
    }

    /// Send a bounded JSON request and retain the raw JSON bytes and HTTP
    /// status. This is used by the legacy compatibility proxy so it can
    /// forward 201/204 and Agent error statuses without rewriting them.
    pub async fn request_raw_value(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<AgentResponse<Vec<u8>>, AgentClientError> {
        let encoded = match body {
            Some(value) => {
                let encoded = serde_json::to_vec(value)
                    .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
                if encoded.len() > MAX_REQUEST_BODY_BYTES {
                    return Err(AgentClientError::InvalidRequest(format!(
                        "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
                    )));
                }
                Some(encoded)
            }
            None => None,
        };
        self.request_raw(method, path, encoded).await
    }

    pub async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, AgentClientError> {
        let response = self.request_raw(method, path, body).await?;
        serde_json::from_slice(&response.body)
            .map_err(|error| AgentClientError::InvalidResponse(error.to_string()))
    }

    async fn request_raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<AgentResponse<Vec<u8>>, AgentClientError> {
        validate_agent_request(path, &method, body.as_deref())?;
        let mut body = body;
        let url = self
            .base_url
            .join(path)
            .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        let compatibility_mutation = method == Method::POST && (matches!(path, "/v1/releases" | "/v1/harness" | "/v1/market")
            || (path == "/v1/profiles" && body.as_ref().and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok()).is_some_and(|body| matches!(body.get("action").and_then(Value::as_str), Some("select" | "compatibility_check")))));
        let patch_download = method == Method::POST && path == "/v1/config" && body.as_ref().and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok()).is_some_and(|body| matches!(body.get("action").and_then(Value::as_str), Some("fetch_harness_patches" | "preview_harness_patches" | "list_harness_patch_refs" | "set_external_harness")));
        let offline_preview = method == Method::POST && path == "/v1/updates" && body.as_ref().and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok()).is_some_and(|body| body.get("action").and_then(Value::as_str) == Some("offline_inspect"));
        let mut request = self.http.request(method.clone(), url);
        let authentication = if path != "/v1/health" {
            if let Some(paths) = &self.credential_paths {
                use nexus_core::agent_auth as auth;
                let identity = self.expected_identity.as_ref().ok_or_else(|| AgentClientError::InvalidRequest("Agent identity must be verified before authorization".into()))?;
                let credential = auth::AgentCredential::read(paths, &identity.data_root_id, &identity.instance_id)
                    .map_err(|_| AgentClientError::InvalidRequest("Agent credential is unavailable or invalid; restart Nexus Agent".into()))?;
                let nonce = auth::random_hex().map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
                let time = auth::unix_seconds().to_string();
                body = Some(credential.seal_request(method.as_str(), path, &nonce, &time, body.as_deref().unwrap_or_default()).map_err(|_| AgentClientError::InvalidRequest("Agent request encryption failed".into()))?);
                let signature = credential.request_signature(method.as_str(), path, &nonce, &time, body.as_deref().unwrap_or_default());
                request = request.header(auth::VERSION_HEADER, "2").header(auth::NONCE_HEADER, &nonce).header(auth::TIME_HEADER, &time).header(auth::SIGNATURE_HEADER, signature);
                Some((credential, nonce))
            } else { None }
        } else { None };
        if method == Method::GET && path == "/v1/preflight" { request=request.timeout(Duration::from_secs(45)); }
        if compatibility_mutation { request = request.timeout(Duration::from_secs(660)); }
        if offline_preview { request = request.timeout(OFFLINE_PREVIEW_TIMEOUT); }
        if patch_download {
            request = request.timeout(Duration::from_secs(75));
        }
        if let Some(identity) = &self.expected_identity {
            request = request
                .header(AGENT_DATA_ROOT_HEADER, &identity.data_root_id)
                .header(AGENT_INSTANCE_HEADER, &identity.instance_id);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| AgentClientError::Transport(error.to_string()))?;
        let status = response.status();
        let encrypted = response.headers().get(nexus_core::agent_auth::VERSION_HEADER).and_then(|v| v.to_str().ok()) == Some("2");
        let response_signature = response.headers().get(nexus_core::agent_auth::RESPONSE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        let mut bytes = read_bounded_body(response).await?;
        if authentication.as_ref().is_some_and(|(credential, nonce)| !credential.verify_response(nonce, status.as_u16(), &bytes, &response_signature)) {
            return Err(AgentClientError::InvalidResponse("Agent response authentication failed; response was not trusted".into()));
        }
        if let Some((credential, nonce)) = authentication {
            if !encrypted { return Err(AgentClientError::InvalidResponse("Agent response encryption is required".into())); }
            bytes = credential.open_response(&nonce, status.as_u16(), &bytes).map_err(|_| AgentClientError::InvalidResponse("Agent response decryption failed".into()))?;
        }
        if !status.is_success() {
            return Err(AgentClientError::Http {
                status,
                message: response_message(&bytes, status),
                body: bytes,
            });
        }
        Ok(AgentResponse {
            status,
            body: bytes,
        })
    }
}

async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AgentClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > (MAX_RESPONSE_BODY_BYTES + nexus_core::agent_auth::TAG_BYTES) as u64)
    {
        return Err(AgentClientError::ResponseTooLarge {
            limit: MAX_RESPONSE_BODY_BYTES,
        });
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AgentClientError::Transport(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES + nexus_core::agent_auth::TAG_BYTES {
            return Err(AgentClientError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn response_message(bytes: &[u8], status: StatusCode) -> String {
    serde_json::from_slice::<ErrorResponse>(bytes)
        .map(|body| body.message)
        .or_else(|_| serde_json::from_slice::<Value>(bytes).map(|value| value.to_string()))
        .unwrap_or_else(|_| {
            let detail = String::from_utf8_lossy(bytes);
            let detail = detail.trim();
            if detail.is_empty() { format!("HTTP {status}") }
            else { format!("HTTP {status}: {}", detail.chars().take(4096).collect::<String>()) }
        })
}

pub fn is_allowed_agent_route(path: &str) -> bool {
    AGENT_ROUTES.iter().any(|route| *route == path)
}

pub fn validate_agent_request(
    path: &str,
    method: &Method,
    body: Option<&[u8]>,
) -> Result<(), AgentClientError> {
    if !is_allowed_agent_route(path)
        || !path.starts_with('/')
        || path.contains(['?', '#'])
        || path.chars().any(char::is_control)
    {
        return Err(AgentClientError::InvalidPath(path.to_owned()));
    }
    let method_allowed = match path {
        "/v1/health"
        | "/v1/state"
        | "/v1/harness/ui"
        | "/v1/harness/discover"
        | "/v1/releases/tags"
        | "/v1/runtime"
        | "/v1/requests"
        | "/v1/preflight" => *method == Method::GET,
        "/v1/runtime/plan" => *method == Method::POST,
        "/v1/harness/startup" | "/v1/canary" | "/v1/recovery" | "/v1/recovery/records" | "/v1/harness" | "/v1/profiles" | "/v1/checkpoints" | "/v1/releases" | "/v1/updates"
        | "/v1/diagnostics" | "/v1/config" | "/v1/maintenance" | "/v1/notifications" | "/v1/market" | "/v1/desktop/profile" => {
            *method == Method::GET || *method == Method::POST
        }
        "/v1/lifecycle" | "/v1/shutdown" => *method == Method::POST,
        _ => false,
    };
    if !method_allowed {
        return Err(AgentClientError::InvalidRequest(format!(
            "HTTP method {method} is not allowed for Agent route {path}"
        )));
    }
    if *method == Method::GET && body.is_some_and(|body| !body.is_empty()) {
        return Err(AgentClientError::InvalidRequest(
            "GET Agent requests cannot include a body".to_owned(),
        ));
    }
    if let Some(body) = body {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(AgentClientError::InvalidRequest(format!(
                "request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte limit"
            )));
        }
        if *method == Method::POST && !body.is_empty() {
            serde_json::from_slice::<Value>(body)
                .map_err(|error| AgentClientError::InvalidRequest(error.to_string()))?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum AgentRuntimeError {
    Io(String),
    Client(AgentClientError),
    IdentityMismatch { expected: String, observed: String },
    NotReady(String),
    Timeout(String),
    Configuration(String),
}

impl fmt::Display for AgentRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "Agent process error: {message}"),
            Self::Client(error) => error.fmt(formatter),
            Self::IdentityMismatch { expected, observed } => write!(
                formatter,
                "Agent data-root identity mismatch: expected {expected}, observed {observed}"
            ),
            Self::NotReady(message) => write!(formatter, "Agent is not ready: {message}"),
            Self::Timeout(message) => write!(formatter, "Agent operation timed out: {message}"),
            Self::Configuration(message) => {
                write!(formatter, "Agent configuration error: {message}")
            }
        }
    }
}

impl std::error::Error for AgentRuntimeError {}

impl From<AgentClientError> for AgentRuntimeError {
    fn from(error: AgentClientError) -> Self {
        Self::Client(error)
    }
}

/// Actions exposed by a native shell for the independent Agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    Start,
    Stop,
    Restart,
    Status,
}

/// Result of a shared Agent start request.
///
/// `started` distinguishes an already-running compatible Agent from a process
/// created by this runtime. The legacy launcher uses that distinction only for
/// its compatibility output; process creation and readiness remain centralized
/// here for every Rust entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStartResult {
    pub health: HealthResponse,
    pub started: bool,
    pub port: u16,
    pub pid: Option<u32>,
    pub program: Option<PathBuf>,
}

/// Shared Agent process resolver and lifecycle coordinator.
#[derive(Clone)]
pub struct AgentRuntime {
    config: NexusConfig,
    paths: NexusPaths,
    data_root_id: String,
    base_client: AgentClient,
    effective_port: Arc<AtomicU16>,
    program: Option<PathBuf>,
    resolved_program: Arc<Mutex<Option<PathBuf>>>,
    resource_dir: Option<PathBuf>,
    expected_build_id: Option<String>,
    child: Arc<Mutex<Option<Child>>>,
    startup_error: Arc<Mutex<Option<String>>>,
    operation: Arc<AsyncMutex<()>>,
}

impl fmt::Debug for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("config", &self.config)
            .field("paths", &self.paths)
            .field("data_root_id", &self.data_root_id)
            .field("client", &self.client())
            .field("program", &self.program)
            .field("resolved_program", &self.resolved_program())
            .finish_non_exhaustive()
    }
}

impl AgentRuntime {
    /// Bind the GUI's embedded package identity; missing disk metadata then fails closed.
    pub fn bind_build_identity(&mut self, build_id: &str) -> Result<(), AgentRuntimeError> {
        if build_id.is_empty() || build_id == "development" || build_id.len() > 128 || build_id.chars().any(char::is_control) {
            return Err(AgentRuntimeError::NotReady("Invalid packaged build identity".to_owned()));
        }
        self.expected_build_id = Some(build_id.to_owned());
        Ok(())
    }

    pub fn new(config: NexusConfig, program: Option<PathBuf>) -> Result<Self, AgentRuntimeError> {
        Self::new_with_resource_dir(config, program, None)
    }

    /// Construct a runtime with an optional Electron/resource directory.
    ///
    /// Resolution still gives an explicit program and `NEXUS_AGENT_BIN`
    /// precedence. The resource directory is checked before development
    /// `target/` candidates, so an installed app does not depend on a source
    /// checkout or a developer environment.
    pub fn new_with_resource_dir(
        config: NexusConfig,
        program: Option<PathBuf>,
        resource_dir: Option<PathBuf>,
    ) -> Result<Self, AgentRuntimeError> {
        let paths = config.paths();
        paths
            .ensure_directories()
            .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
        let data_root_id =
            data_root_identity(&paths).map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
        let client = AgentClient::new(config.port)?;
        if let Some(resource_dir) = resource_dir.as_deref() {
            validate_resource_dir(resource_dir)?;
        }
        let effective_port = Arc::new(AtomicU16::new(config.port));
        Ok(Self {
            config,
            paths,
            data_root_id,
            base_client: client,
            effective_port,
            resolved_program: Arc::new(Mutex::new(program.clone())),
            program,
            resource_dir,
            expected_build_id: option_env!("NEXUS_BUILD_ID").map(str::to_owned),
            child: Arc::new(Mutex::new(None)),
            startup_error: Arc::new(Mutex::new(None)),
            operation: Arc::new(AsyncMutex::new(())),
        })
    }

    pub fn client(&self) -> AgentClient {
        let port = self.effective_port.load(Ordering::Acquire);
        if port == self.config.port {
            return self.base_client.clone().with_credential_paths(self.paths.clone());
        }
        AgentClient::from_reqwest(port, self.base_client.http.clone())
            .expect("effective port is validated before adoption")
            .with_credential_paths(self.paths.clone())
    }

    fn adopt_discovered_port(&self, port: u16) {
        self.effective_port.store(port, Ordering::Release);
    }

    pub fn config(&self) -> &NexusConfig {
        &self.config
    }

    pub fn paths(&self) -> &NexusPaths {
        &self.paths
    }

    pub fn data_root_id(&self) -> &str {
        &self.data_root_id
    }

    pub fn agent_program(&self) -> Option<&Path> {
        self.program.as_deref()
    }

    /// Return the executable selected by the last successful resolution, or
    /// the explicit constructor override before the first start.
    pub fn resolved_program(&self) -> Option<PathBuf> {
        self.resolved_program
            .lock()
            .ok()
            .and_then(|program| program.clone())
            .or_else(|| self.program.clone())
    }

    pub fn resource_dir(&self) -> Option<&Path> {
        self.resource_dir.as_deref()
    }

    pub fn startup_error(&self) -> Option<String> {
        self.startup_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    pub fn child_pid(&self) -> Option<u32> {
        let mut child = self.child.lock().ok()?;
        let process = child.as_mut()?;
        match process.try_wait() {
            Ok(None) => process.id(),
            Ok(Some(_)) => {
                *child = None;
                None
            }
            Err(_) => process.id(),
        }
    }

    pub async fn probe(&self) -> Result<HealthResponse, AgentRuntimeError> {
        let primary = self.client().get_json::<HealthResponse>("/v1/health").await
            .map_err(AgentRuntimeError::Client).and_then(|health| self.validate_health(health));
        match primary {
            Ok(health) => Ok(health),
            Err(primary_error) => {
                // A live unrelated HTTP listener is just as unavailable as a
                // closed port. Only adopt a discovery target after identity validation.
                let Some(record) = self
                    .paths
                    .read_agent_discovery()
                    .ok()
                    .flatten()
                    .filter(|record| record.data_root_id == self.data_root_id)
                else {
                    return Err(primary_error);
                };
                let candidate =
                    AgentClient::from_reqwest(record.port, self.base_client.http.clone())
                        .map_err(AgentRuntimeError::Client)?;
                let health = candidate
                    .get_json::<HealthResponse>("/v1/health")
                    .await
                    .map_err(AgentRuntimeError::Client)?;
                if health.instance_id != record.instance_id {
                    return Err(AgentRuntimeError::NotReady(
                        "discovery record belongs to a different Agent instance".to_owned(),
                    ));
                }
                let health = self.validate_health(health)?;
                self.adopt_discovered_port(record.port);
                Ok(health)
            }
        }
    }

    /// Application requests require both root identity and the packaged build.
    /// `probe` remains available for inspecting and stopping a stale Agent.
    pub async fn probe_ready(&self) -> Result<HealthResponse, AgentRuntimeError> {
        let health = self.probe().await?;
        self.require_fresh_binary(&health)?;
        Ok(health)
    }

    fn validate_health(&self, health: HealthResponse) -> Result<HealthResponse, AgentRuntimeError> {
        if health.api_version != nexus_protocol::API_VERSION
            || health.service != "nexus-agent"
            || health.instance_id.is_empty()
        {
            return Err(AgentRuntimeError::NotReady(
                "loopback listener did not return the Nexus Agent health contract".to_owned(),
            ));
        }
        if health.data_root_id != self.data_root_id {
            return Err(AgentRuntimeError::IdentityMismatch {
                expected: self.data_root_id.clone(),
                observed: health.data_root_id,
            });
        }
        if health.status != HealthStatus::Ok {
            return Err(AgentRuntimeError::NotReady(format!(
                "health status is {:?}",
                health.status
            )));
        }
        Ok(health)
    }

    /// Whether a probed Agent was started from the same binary this launcher
    /// would spawn. A packaged build requires the compiled Agent ID to match
    /// its resource identity, including when an old process uses the same path.
    fn binary_is_fresh(&self, health: &HealthResponse) -> bool {
        let Ok(resolved) = resolve_agent_program_with_resource_dir(
            self.program.as_deref(),
            self.resource_dir.as_deref(),
        ) else {
            return self.resource_dir.is_none() && self.expected_build_id.is_none();
        };
        if self.expected_build_id.as_deref().is_some_and(|id| health.build_id.as_deref() != Some(id)) { return false; }
        let identity = resolved.parent().unwrap_or_else(|| Path::new(".")).join("release-identity.json");
        match fs::symlink_metadata(&identity) {
            Ok(metadata) => {
                if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 64 * 1024 { return false; }
                let Some(identity) = fs::read(identity).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()) else { return false; };
                let Some(expected) = identity.get("buildId").and_then(serde_json::Value::as_str).filter(|id| !id.is_empty() && id.len() <= 128 && *id != "development") else { return false; };
                if health.build_id.as_deref() != Some(expected) || self.expected_build_id.as_deref().is_some_and(|id| id != expected) { return false; }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.expected_build_id.is_none() => {},
            Err(_) => return false,
        }
        let Some(running) = health.binary_path.as_deref() else { return true; };
        let normalize = |value: &str| -> String { value.replace('/', "\\").to_lowercase() };
        if normalize(running) == normalize(&resolved.to_string_lossy()) {
            return true;
        }
        match (fs::canonicalize(running), fs::canonicalize(&resolved)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }

    fn require_fresh_binary(&self, health: &HealthResponse) -> Result<(), AgentRuntimeError> {
        if self.binary_is_fresh(health) { Ok(()) } else {
            Err(AgentRuntimeError::NotReady("The running Agent does not match this installation. Stop the old Agent successfully and verify the installed package before retrying.".to_owned()))
        }
    }

    pub async fn start(&self, wait_secs: u64) -> Result<AgentStartResult, AgentRuntimeError> {
        if let Ok(health) = self.probe().await {
            if self.binary_is_fresh(&health) {
                self.remember_resolved_program();
                return Ok(AgentStartResult {
                    health,
                    started: false,
                    port: self.effective_port.load(Ordering::Acquire),
                    pid: self.child_pid(),
                    program: self.resolved_program(),
                });
            }
            // A running Agent from a different binary generation: stop it
            // gracefully through its own endpoint so the fresh binary can
            // take over, then fall through to a normal spawn.
            self.stop(wait_secs).await?;
        }

        let _operation = self.operation.lock().await;
        if let Ok(health) = self.probe().await {
            self.require_fresh_binary(&health)?;
            self.remember_resolved_program();
            return Ok(AgentStartResult {
                health,
                started: false,
                port: self.effective_port.load(Ordering::Acquire),
                pid: self.child_pid(),
                program: self.resolved_program(),
            });
        }

        // This OS-owned lock is shared by every Rust entry point. It closes the
        // cross-process check/spawn race between the Electron shell and legacy
        // `nexus-launcher`, while the Agent's own `agent.lock` remains the
        // lifetime owner after readiness.
        let _bootstrap_lock = acquire_bootstrap_lock(&self.paths)?;
        if let Ok(health) = self.probe().await {
            self.require_fresh_binary(&health)?;
            self.remember_resolved_program();
            return Ok(AgentStartResult {
                health,
                started: false,
                port: self.effective_port.load(Ordering::Acquire),
                pid: self.child_pid(),
                program: self.resolved_program(),
            });
        }
        if self.child_pid().is_some() {
            let health = self.wait_for_health(wait_secs).await?;
            return Ok(AgentStartResult {
                health,
                started: true,
                port: self.effective_port.load(Ordering::Acquire),
                pid: self.child_pid(),
                program: self.resolved_program(),
            });
        }
        // An unowned Agent can close its listener before its supervisor has
        // released the lifetime lock. Wait for that bounded handoff, then
        // probe once more so a restarting external Agent is adopted instead
        // of being mistaken for a missing process. This path never kills an
        // unowned process.
        self.wait_for_runtime_lock_available(wait_secs).await?;
        if let Ok(health) = self.probe().await {
            self.require_fresh_binary(&health)?;
            self.remember_resolved_program();
            return Ok(AgentStartResult {
                health,
                started: false,
                port: self.effective_port.load(Ordering::Acquire),
                pid: self.child_pid(),
                program: self.resolved_program(),
            });
        }

        self.paths
            .ensure_directories()
            .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
        let program = resolve_agent_program_with_resource_dir(
            self.program.as_deref(),
            self.resource_dir.as_deref(),
        )?;
        self.set_resolved_program(program.clone());
        let expected_instance_id = new_instance_id();
        let child = spawn_agent(&program, &self.config, &self.paths, &expected_instance_id)
            .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
        let pid = child.id().ok_or_else(|| {
            AgentRuntimeError::Io("Agent process did not expose a PID".to_owned())
        })?;
        {
            let mut current = self
                .child
                .lock()
                .map_err(|_| AgentRuntimeError::Io("Agent child lock is poisoned".to_owned()))?;
            *current = Some(child);
        }
        self.set_startup_error(None);
        match self
            .wait_for_health_with_instance(wait_secs, &expected_instance_id)
            .await
        {
            Ok(health) => Ok(AgentStartResult {
                health,
                started: true,
                port: self.effective_port.load(Ordering::Acquire),
                pid: Some(pid),
                program: Some(program),
            }),
            Err(error) => {
                self.terminate_owned_child().await;
                self.set_startup_error(Some(error.to_string()));
                Err(error)
            }
        }
    }

    pub async fn ensure_started(
        &self,
        wait_secs: u64,
    ) -> Result<HealthResponse, AgentRuntimeError> {
        self.start(wait_secs).await.map(|result| result.health)
    }

    async fn wait_for_health(&self, wait_secs: u64) -> Result<HealthResponse, AgentRuntimeError> {
        self.wait_for_health_with_instance(wait_secs, "").await
    }

    async fn wait_for_health_with_instance(
        &self,
        wait_secs: u64,
        expected_instance_id: &str,
    ) -> Result<HealthResponse, AgentRuntimeError> {
        let timeout_secs = wait_secs.clamp(1, 300);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut last_error = None;
        while Instant::now() <= deadline {
            match self.probe().await {
                Ok(health)
                    if expected_instance_id.is_empty()
                        || health.instance_id == expected_instance_id =>
                {
                    match self.require_fresh_binary(&health) {
                        Ok(()) => return Ok(health),
                        Err(error) => last_error = Some(error),
                    }
                }
                Ok(health) => {
                    last_error = Some(AgentRuntimeError::NotReady(format!(
                        "instance identity {} does not match the spawned Agent",
                        health.instance_id
                    )));
                }
                Err(error) => last_error = Some(error),
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(AgentRuntimeError::Timeout(last_error.map_or_else(
            || format!("Agent did not become healthy within {timeout_secs} seconds"),
            |error| format!("Agent did not become healthy within {timeout_secs} seconds: {error}"),
        )))
    }

    pub async fn stop(&self, wait_secs: u64) -> Result<(), AgentRuntimeError> {
        let _operation = self.operation.lock().await;
        let owned_child = self.child_pid().is_some();
        let health = match self.probe().await {
            Ok(health) => health,
            Err(AgentRuntimeError::Client(AgentClientError::Transport(_)))
            | Err(AgentRuntimeError::Client(AgentClientError::InvalidResponse(_))) => {
                if self.child_pid().is_none() {
                    return Ok(());
                }
                return Err(AgentRuntimeError::NotReady(
                    "Agent health is unavailable; refusing to terminate a live owned process "
                        .to_owned(),
                ));
            }
            Err(error) => return Err(error),
        };
        let identity = AgentIdentity::from(&health);
        let client = self.client().with_expected_identity(identity);
        let _: LifecycleAccepted = client.post_empty("/v1/shutdown").await?;
        let timeout_secs = wait_secs.clamp(1, 300);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut last_error = None;
        loop {
            if owned_child && self.child_pid().is_none() {
                // The child exited naturally.  A disappearing listener alone
                // is not enough: the Agent may still be draining Harness
                // supervision after its HTTP server has stopped accepting.
                return Ok(());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let poll_window = remaining.min(Duration::from_secs(1));
            match timeout(
                poll_window,
                self.client().get_json::<HealthResponse>("/v1/health"),
            )
            .await
            {
                Ok(Ok(next)) if next.data_root_id != self.data_root_id => {
                    return Err(AgentRuntimeError::IdentityMismatch {
                        expected: self.data_root_id.clone(),
                        observed: next.data_root_id,
                    });
                }
                Ok(Ok(next)) if next.instance_id != health.instance_id => {
                    // Never kill an owned child after the port has been
                    // rebound to a different instance, even if it advertises
                    // the same data root.
                    return Err(AgentRuntimeError::NotReady(format!(
                        "Agent instance changed while stopping: expected {}, observed {}",
                        health.instance_id, next.instance_id
                    )));
                }
                Ok(Ok(_)) => {}
                Ok(Err(_error)) if !owned_child => return Ok(()),
                Err(_) if !owned_child => return Ok(()),
                Ok(Err(error)) => last_error = Some(error.to_string()),
                Err(_) => last_error = Some("health probe timed out".to_owned()),
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                sleep(remaining.min(Duration::from_millis(100))).await;
            }
        }

        if owned_child {
            // Only the child originally owned by this runtime may be cleaned
            // up, and only after the bounded graceful wait has expired.
            self.terminate_owned_child().await;
        }
        Err(AgentRuntimeError::Timeout(last_error.map_or_else(
            || format!("Agent did not stop within {timeout_secs} seconds"),
            |error| format!("Agent did not stop within {timeout_secs} seconds: {error}"),
        )))
    }

    pub async fn restart(&self, wait_secs: u64) -> Result<HealthResponse, AgentRuntimeError> {
        self.stop(wait_secs).await?;
        self.ensure_started(wait_secs).await
    }

    pub async fn action(
        &self,
        action: AgentAction,
        wait_secs: u64,
    ) -> Result<Option<HealthResponse>, AgentRuntimeError> {
        match action {
            AgentAction::Start => self.ensure_started(wait_secs).await.map(Some),
            AgentAction::Stop => {
                self.stop(wait_secs).await?;
                Ok(None)
            }
            AgentAction::Restart => self.restart(wait_secs).await.map(Some),
            AgentAction::Status => self.probe().await.map(Some),
        }
    }

    pub async fn status(&self) -> AgentStatus {
        self.remember_resolved_program();
        let health = self.probe().await.ok();
        let running = health.is_some();
        let available = health.as_ref().is_some_and(|health| self.binary_is_fresh(health));
        AgentStatus {
            available,
            running,
            api_base: self.base_url_string(),
            data_root: self.paths.root.display().to_string(),
            data_root_id: health.as_ref().map(|health| health.data_root_id.clone()),
            instance_id: health.as_ref().map(|health| health.instance_id.clone()),
            agent_pid: self.child_pid(),
            agent_program: if running { health.as_ref().and_then(|health| health.binary_path.clone()) } else { self.resolved_program().map(|path| path.display().to_string()) },
            message: if available {
                None
            } else if running {
                Some("The running Agent does not match this installation. Stop it successfully before starting the installed build.".to_owned())
            } else {
                self.startup_error().or_else(|| {
                    Some(format!(
                        "Agent is not responding at {}",
                        self.base_url_string()
                    ))
                })
            },
        }
    }

    fn base_url_string(&self) -> String {
        self.client()
            .base_url()
            .to_string()
            .trim_end_matches('/')
            .to_owned()
    }

    fn set_startup_error(&self, value: Option<String>) {
        if let Ok(mut error) = self.startup_error.lock() {
            *error = value;
        }
    }

    fn set_resolved_program(&self, program: PathBuf) {
        if let Ok(mut current) = self.resolved_program.lock() {
            *current = Some(program);
        }
    }

    fn remember_resolved_program(&self) {
        if self.resolved_program().is_none() {
            if let Ok(program) = resolve_agent_program_with_resource_dir(
                self.program.as_deref(),
                self.resource_dir.as_deref(),
            ) {
                self.set_resolved_program(program);
            }
        }
    }

    async fn wait_for_runtime_lock_available(
        &self,
        wait_secs: u64,
    ) -> Result<(), AgentRuntimeError> {
        let timeout_secs = wait_secs.clamp(1, 300);
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            match ensure_runtime_lock_available(&self.paths) {
                Ok(()) => return Ok(()),
                Err(AgentRuntimeError::NotReady(message)) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(AgentRuntimeError::Timeout(format!(
                            "Agent runtime lock did not become available within {timeout_secs} seconds: Agent is not ready: {message}"
                        )));
                    }
                    sleep(remaining.min(Duration::from_millis(100))).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn terminate_owned_child(&self) {
        let child = self
            .child
            .lock()
            .ok()
            .and_then(|mut current| current.take());
        let Some(mut child) = child else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill().await;
        }
        let _ = child.wait().await;
    }
}

pub fn resolve_agent_program(explicit: Option<&Path>) -> Result<PathBuf, AgentRuntimeError> {
    resolve_agent_program_with_resource_dir(explicit, None)
}

pub fn resolve_agent_program_with_resource_dir(
    explicit: Option<&Path>,
    resource_dir: Option<&Path>,
) -> Result<PathBuf, AgentRuntimeError> {
    if let Some(path) = explicit {
        validate_program_path(path, "explicit Agent program")?;
        return Ok(path.to_owned());
    }
    if let Some(value) = env::var_os(AGENT_BINARY_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        validate_program_path(&path, AGENT_BINARY_ENV)?;
        return Ok(path);
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            // A packaged Electron app keeps its Agent beside the launcher EXE
            // whenever the bundle resource map permits it. Prefer this
            // same-directory layout before all resource/development fallbacks.
            push_agent_candidates(&mut candidates, parent);
        }
    }
    if let Some(resource_dir) = resource_dir {
        validate_resource_dir(resource_dir)?;
        // Electron versions/platforms expose either the bundle's resource root
        // or its parent to `resource_dir`. Check both bounded locations so a
        // package containing `<exe>/resources/nexus-agent.exe` is usable too.
        push_agent_candidates(&mut candidates, resource_dir);
        push_agent_candidates(&mut candidates, &resource_dir.join("resources"));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            add_target_candidates(&mut candidates, parent);
        }
    }
    if let Ok(current) = env::current_dir() {
        add_target_candidates(&mut candidates, &current);
    }
    candidates.retain(|path| path.is_file());
    candidates.into_iter().next().ok_or_else(|| {
        AgentRuntimeError::Configuration(format!(
            "nexus-agent was not found beside the native app or in a nearby Cargo target directory; set {AGENT_BINARY_ENV} to a signed Agent executable"
        ))
    })
}

fn push_agent_candidates(candidates: &mut Vec<PathBuf>, directory: &Path) {
    // A resource staged by a cross-platform build can retain the Windows
    // suffix even when the resolver is exercised from a compatibility shell.
    // Keep all names bounded to the selected directory.
    candidates.push(directory.join(platform_agent_name()));
    candidates.push(directory.join("nexus-agent"));
    candidates.push(directory.join("nexus-agent.exe"));
}

fn validate_program_path(path: &Path, label: &str) -> Result<(), AgentRuntimeError> {
    if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
        return Err(AgentRuntimeError::Configuration(format!(
            "{label} must be a non-empty path without control characters"
        )));
    }
    if !path.is_file() {
        return Err(AgentRuntimeError::Configuration(format!(
            "{label} does not name an existing file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_resource_dir(path: &Path) -> Result<(), AgentRuntimeError> {
    if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
        return Err(AgentRuntimeError::Configuration(
            "Agent resource directory must be a non-empty path without control characters"
                .to_owned(),
        ));
    }
    Ok(())
}

fn add_target_candidates(candidates: &mut Vec<PathBuf>, start: &Path) {
    // Walking ancestors for a Cargo target directory is a source-tree
    // development convenience. A packaged app must not silently adopt an old
    // Agent from a stray target directory, so release builds skip the walk
    // and rely on the package's own resource locations.
    if !cfg!(debug_assertions) {
        return;
    }
    let mut ancestor = Some(start);
    for _ in 0..8 {
        let Some(path) = ancestor else {
            break;
        };
        candidates.push(
            path.join("target")
                .join("debug")
                .join(platform_agent_name()),
        );
        candidates.push(
            path.join("target")
                .join("release")
                .join(platform_agent_name()),
        );
        ancestor = path.parent();
    }
}

fn platform_agent_name() -> &'static str {
    if cfg!(windows) {
        "nexus-agent.exe"
    } else {
        "nexus-agent"
    }
}

static AGENT_LOG_LEVEL: std::sync::RwLock<Option<&'static str>> = std::sync::RwLock::new(None);

/// Set the log level for subsequently spawned Agent processes. Accepts the
/// standard level names; anything else clears the override back to the
/// Agent default ("info"). Applies on the next spawn, not to a live Agent.
pub fn set_agent_log_level(level: &str) {
    let parsed = match level {
        "error" => Some("error"),
        "warn" => Some("warn"),
        "info" => Some("info"),
        "debug" => Some("debug"),
        "trace" => Some("trace"),
        _ => None,
    };
    *AGENT_LOG_LEVEL
        .write()
        .expect("agent log level lock is not poisoned") = parsed;
}

pub fn agent_log_level() -> Option<&'static str> {
    *AGENT_LOG_LEVEL
        .read()
        .expect("agent log level lock is not poisoned")
}

fn spawn_agent(
    program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    instance_id: &str,
) -> io::Result<Child> {
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stdout.log"))?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stderr.log"))?;
    let mut command = Command::new(program);
    command
        .arg("--data-dir")
        .arg(&paths.root)
        // The default port is only a compatibility pin: an ephemeral
        // OS-assigned port is the default posture, discovered through
        // run/agent.json. An explicitly configured non-default port passes
        // through unchanged.
        .arg("--port")
        .arg(if config.port == nexus_core::DEFAULT_AGENT_PORT {
            "0".to_owned()
        } else {
            config.port.to_string()
        })
        .arg("--instance-id")
        .arg(instance_id)
        .stdin(Stdio::null());
    if let Some(level) = agent_log_level() {
        command.env("NEXUS_AGENT_LOG", level);
    }
    configure_agent_process(&mut command);
    #[cfg(windows)]
    {
        windows_agent::spawn(command.as_std(), &stdout, &stderr, agent_creation_flags())
    }
    #[cfg(not(windows))]
    {
        command.stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr)).spawn()
    }
}

fn configure_agent_process(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);

    #[cfg(windows)]
    {
        command.creation_flags(agent_creation_flags());
    }
}

#[cfg(windows)]
const fn agent_creation_flags() -> u32 {
    // CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW.
    // This keeps a packaged console-subsystem Agent independent of the GUI
    // process tree without opening an extra console window.
    0x0100_0000 | 0x0000_0200 | 0x0800_0000
}

#[derive(Debug)]
struct BootstrapLock {
    _file: fs::File,
}

fn bootstrap_lock_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("agent-bootstrap.lock")
}

fn acquire_bootstrap_lock(paths: &NexusPaths) -> Result<BootstrapLock, AgentRuntimeError> {
    let path = bootstrap_lock_path(paths);
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => AgentRuntimeError::NotReady(format!(
            "another launcher is starting the Agent or holds {}",
            path.display()
        )),
        fs::TryLockError::Error(error) => AgentRuntimeError::Io(error.to_string()),
    })?;
    file.set_len(0)
        .and_then(|()| writeln!(file, "pid={}", std::process::id()))
        .and_then(|()| file.sync_all())
        .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
    Ok(BootstrapLock { _file: file })
}

fn ensure_runtime_lock_available(paths: &NexusPaths) -> Result<(), AgentRuntimeError> {
    let path = paths.run_dir.join("agent.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|error| AgentRuntimeError::Io(error.to_string()))?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => AgentRuntimeError::NotReady(
            "another Nexus Agent already owns this data root, possibly on a different port"
                .to_owned(),
        ),
        fs::TryLockError::Error(error) => AgentRuntimeError::Io(error.to_string()),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_text_rejections_keep_bounded_parser_details() {
        let message = response_message(b"Failed to deserialize JSON: unknown field `request_id`", StatusCode::UNPROCESSABLE_ENTITY);
        assert!(message.contains("unknown field `request_id`"));
        assert_eq!(response_message(b"", StatusCode::BAD_REQUEST), "HTTP 400 Bad Request");
        assert!(response_message(&vec![b'x'; 8000], StatusCode::BAD_REQUEST).len() < 4200);
    }
    use nexus_protocol::HealthResponse;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn patch_download_waits_beyond_normal_request_deadline() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client = AgentClient::new(listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            socket.read(&mut bytes).await.unwrap();
            sleep(DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
            write_json_response(&mut socket, "200 OK", &serde_json::json!({"saved":true})).await;
        });
        let response: Value = client.request_json(Method::POST, "/v1/config", Some(br#"{"action":"fetch_harness_patches"}"#.to_vec())).await.unwrap();
        assert_eq!(response["saved"], true); server.await.unwrap();
    }
    #[tokio::test]
    async fn offline_preview_uses_the_full_archive_budget_across_transport() {
        assert!(OFFLINE_PREVIEW_TIMEOUT >= Duration::from_secs(1830));
        let listener=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await.unwrap();
        let client=AgentClient::new(listener.local_addr().unwrap().port()).unwrap();
        let server=tokio::spawn(async move {
            let (mut socket,_)=listener.accept().await.unwrap();let mut bytes=[0;4096];socket.read(&mut bytes).await.unwrap();
            sleep(DEFAULT_REQUEST_TIMEOUT+Duration::from_secs(1)).await;
            write_json_response(&mut socket,"200 OK",&serde_json::json!({"preview":true})).await;
        });
        let response:Value=client.request_json(Method::POST,"/v1/updates",Some(br#"{"action":"offline_inspect"}"#.to_vec())).await.unwrap();
        assert_eq!(response["preview"],true);server.await.unwrap();
    }

    #[tokio::test]
    async fn manual_plugin_check_waits_beyond_normal_request_deadline() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client = AgentClient::new(listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let count = socket.read(&mut bytes).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("POST /v1/profiles "));
            sleep(DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
            write_json_response(&mut socket, "200 OK", &serde_json::json!({"verified":true})).await;
        });
        let response: Value = client.request_json(Method::POST, "/v1/profiles",
            Some(br#"{"action":"compatibility_check"}"#.to_vec())).await.unwrap();
        assert_eq!(response["verified"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn agent_client_bypasses_download_proxy_environment() {
        // Isolate environment changes from every other concurrent test.
        if env::var_os("NEXUS_TEST_LOOPBACK_PROXY").is_none() {
            let output = std::process::Command::new(env::current_exe().unwrap())
                .args(["--exact", "tests::agent_client_bypasses_download_proxy_environment", "--nocapture"])
                .env("NEXUS_TEST_LOOPBACK_PROXY", "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1").env("http_proxy", "http://127.0.0.1:1")
                .env("HTTPS_PROXY", "http://127.0.0.1:1").env("https_proxy", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1").env("all_proxy", "http://127.0.0.1:1")
                .env("NO_PROXY", "").env("no_proxy", "")
                .output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let client = AgentClient::new(listener.local_addr().unwrap().port()).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096]; let _ = socket.read(&mut bytes).await.unwrap();
            write_json_response(&mut socket, "200 OK", &serde_json::json!({"direct":true})).await;
        });
        let response: Value = client.get_json("/v1/health").await.unwrap();
        assert_eq!(response["direct"], true);
        server.await.unwrap();
    }

    async fn packaged_agent_fixture(label: &str) -> (AgentRuntime, TcpListener, PathBuf, HealthResponse) {
        let root = env::temp_dir().join(format!("nexus-build-identity-{label}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let resources = root.join("resources");
        fs::create_dir_all(&resources).unwrap();
        let program = resources.join(if cfg!(windows) { "nexus-agent.exe" } else { "nexus-agent" });
        fs::write(&program, "test binary; never executed").unwrap();
        fs::write(resources.join("release-identity.json"), r#"{"schemaVersion":1,"buildId":"package-build"}"#).unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut runtime = AgentRuntime::new_with_resource_dir(NexusConfig { data_dir: Some(root.join("data")), port: listener.local_addr().unwrap().port() }, Some(program.clone()), Some(resources)).unwrap();
        runtime.bind_build_identity("package-build").unwrap();
        let mut health = HealthResponse::healthy(runtime.data_root_id().to_owned(), "old-instance".to_owned());
        health.binary_path = Some(program.to_string_lossy().into_owned());
        health.build_id = Some("old-build".to_owned());
        (runtime, listener, root, health)
    }

    #[tokio::test]
    async fn packaged_identity_rejects_same_path_old_or_missing_build() {
        let (mut runtime, listener, root, mut health) = packaged_agent_fixture("match").await;
        assert!(!runtime.binary_is_fresh(&health));
        health.build_id = None;
        assert!(!runtime.binary_is_fresh(&health));
        health.build_id = Some("package-build".to_owned());
        assert!(runtime.binary_is_fresh(&health));
        runtime.bind_build_identity("package-build").unwrap();
        fs::remove_file(root.join("resources/release-identity.json")).unwrap();
        assert!(!runtime.binary_is_fresh(&health), "a packaged identity cannot silently fall back when its manifest is missing");
        fs::write(root.join("resources/release-identity.json"), "broken").unwrap();
        assert!(!runtime.binary_is_fresh(&health));
        drop(listener); fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stale_agent_shutdown_failure_cannot_be_reported_as_successful_start() {
        let (runtime, listener, root, health) = packaged_agent_fixture("stop-failure").await;
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                if String::from_utf8_lossy(&bytes[..count]).starts_with("POST /v1/shutdown ") {
                    write_json_response(&mut socket, "500 Internal Server Error", &serde_json::json!({"error":"stop failed"})).await;
                } else { write_json_response(&mut socket, "200 OK", &health).await; }
            }
        });
        assert!(runtime.start(1).await.is_err());
        assert!(runtime.child_pid().is_none());
        let status = runtime.status().await;
        assert!(status.running && !status.available);
        assert!(status.message.unwrap().contains("does not match"));
        assert!(runtime.probe_ready().await.is_err());
        assert!(runtime.probe().await.is_ok(), "old Agent remains observable for explicit shutdown");
        server.abort(); let _ = server.await;
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stale_agent_appearing_after_initial_probe_is_not_adopted() {
        let (runtime, listener, root, health) = packaged_agent_fixture("late").await;
        let server = tokio::spawn(async move {
            for first in [true, false] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096]; let _ = socket.read(&mut bytes).await.unwrap();
                if first { write_json_response(&mut socket, "503 Service Unavailable", &serde_json::json!({"error":"starting"})).await; }
                else { write_json_response(&mut socket, "200 OK", &health).await; }
            }
        });
        assert!(runtime.start(1).await.is_err());
        assert!(runtime.child_pid().is_none());
        server.await.unwrap(); fs::remove_dir_all(root).unwrap();
    }

    async fn write_json_response<T: Serialize>(
        socket: &mut tokio::net::TcpStream,
        status: &str,
        value: &T,
    ) {
        let body = serde_json::to_vec(value).expect("test response serializes");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("test response headers write");
        socket
            .write_all(&body)
            .await
            .expect("test response body write");
    }

    async fn write_raw_response(socket: &mut tokio::net::TcpStream, status: &str, body: &[u8]) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("test response headers write");
        socket
            .write_all(body)
            .await
            .expect("test response body write");
    }

    #[tokio::test]
    async fn discovery_survives_unrelated_http_on_configured_port() {
        for collision in ["404", "json", "identity"] {
            let root = env::temp_dir().join(format!("nexus-discovery-collision-{}-{}-{collision}",
                std::process::id(), nexus_core::unix_time_nanos_for_update()));
            let wrong = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let correct = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let actual = correct.local_addr().unwrap().port();
            let runtime = AgentRuntime::new(NexusConfig {
                data_dir: Some(root.clone()), port: wrong.local_addr().unwrap().port(),
            }, None).unwrap();
            runtime.paths.ensure_directories().unwrap();
            runtime.paths.publish_agent_discovery(&nexus_core::AgentDiscoveryRecord {
                port: actual, instance_id: "owned".into(), data_root_id: runtime.data_root_id.clone(),
                pid: std::process::id(), updated_at_unix: 0,
            }).unwrap();
            let health = HealthResponse::healthy(runtime.data_root_id.clone(), "owned".into());
            let good_server = tokio::spawn(serve_health_once(correct, health));
            let wrong_server = tokio::spawn(async move {
                let (mut socket, _) = wrong.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                if collision == "identity" {
                    write_json_response(&mut socket, "200 OK", &HealthResponse::healthy("other-root".into(), "other".into())).await;
                } else {
                    write_raw_response(&mut socket, if collision == "404" { "404 Not Found" } else { "200 OK" }, b"{}").await;
                }
            });
            assert_eq!(runtime.probe().await.unwrap().instance_id, "owned");
            assert_eq!(runtime.client().base_url().port(), Some(actual));
            wrong_server.await.unwrap();
            good_server.await.unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    async fn serve_health_once(listener: TcpListener, health: HealthResponse) {
        let (mut socket, _) = listener.accept().await.expect("health request accepts");
        let mut request = [0_u8; 4096];
        let _ = socket
            .read(&mut request)
            .await
            .expect("health request reads");
        write_json_response(&mut socket, "200 OK", &health).await;
    }

    #[tokio::test]
    async fn discovered_port_routes_config_and_start_and_follows_rediscovery() {
        let root = env::temp_dir().join(format!(
            "nexus-port-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let unused = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let configured = unused.local_addr().unwrap().port();
        drop(unused);
        let build_id = option_env!("NEXUS_BUILD_ID").unwrap_or("discovery-fixture-build");
        let resources = root.join("resources"); fs::create_dir_all(&resources).unwrap();
        let program = resources.join(if cfg!(windows) { "nexus-agent.exe" } else { "nexus-agent" });
        fs::write(&program, b"mock Agent identity; never executed").unwrap();
        fs::write(resources.join("release-identity.json"), serde_json::to_vec(&serde_json::json!({"schemaVersion":1,"buildId":build_id})).unwrap()).unwrap();
        let runtime = AgentRuntime::new_with_resource_dir(
            NexusConfig {
                data_dir: Some(root.clone()),
                port: configured,
            },
            Some(program.clone()),
            Some(resources),
        )
        .unwrap();
        let shared = runtime.clone();
        for generation in 0..2 {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let listener = if listener.local_addr().unwrap().port() == configured {
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap()
            } else {
                listener
            };
            let port = listener.local_addr().unwrap().port();
            let instance = format!("instance-{generation}");
            runtime
                .paths
                .publish_agent_discovery(&nexus_core::AgentDiscoveryRecord {
                    port,
                    instance_id: instance.clone(),
                    data_root_id: runtime.data_root_id.clone(),
                    pid: std::process::id(),
                    updated_at_unix: 0,
                })
                .unwrap();
            let credential = nexus_core::agent_auth::AgentCredential::publish(&runtime.paths, &instance).unwrap();
            let identity = AgentIdentity { data_root_id: runtime.data_root_id.clone(), instance_id: instance.clone() };
            let mut health = HealthResponse::healthy(runtime.data_root_id.clone(), instance);
            health.build_id = Some(build_id.into());
            health.binary_path = Some(program.to_string_lossy().into_owned());
            let server = tokio::spawn(async move {
                for route in [
                    "GET /v1/health ",
                    "GET /v1/health ",
                    "GET /v1/config ",
                    "POST /v1/harness ",
                ] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0; 4096];
                    let len = socket.read(&mut request).await.unwrap();
                    assert!(String::from_utf8_lossy(&request[..len]).starts_with(route));
                    if route.contains("health") {
                        write_json_response(&mut socket, "200 OK", &health).await;
                    } else {
                        let raw = String::from_utf8_lossy(&request[..len]);
                        let nonce = raw.lines().find_map(|line| line.strip_prefix("x-nexus-auth-nonce: ")).unwrap();
                        let body = br#"{"ok":true}"#;
                        let body = credential.seal_response(nonce, 200, body).unwrap(); let proof = credential.response_signature(nonce, 200, &body);
                        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nx-nexus-auth-version: 2\r\nx-nexus-auth-response: {proof}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                        socket.write_all(&body).await.unwrap();
                    }
                }
            });
            runtime.probe().await.unwrap();
            assert_eq!(runtime.start(1).await.unwrap().port, port);
            assert_eq!(shared.client().base_url().port(), Some(port));
            shared
                .client().with_expected_identity(identity.clone())
                .get_json::<serde_json::Value>("/v1/config")
                .await
                .unwrap();
            shared
                .client().with_expected_identity(identity.clone())
                .post_json::<_, serde_json::Value>(
                    "/v1/harness",
                    &serde_json::json!({"action":"start"}),
                )
                .await
                .unwrap();
            server.await.unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn invalid_discovery_health_does_not_change_client() {
        let root = env::temp_dir().join(format!(
            "nexus-port-invalid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let unused = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let configured = unused.local_addr().unwrap().port();
        drop(unused);
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(root.clone()),
                port: configured,
            },
            None,
        )
        .unwrap();
        for invalid in ["root", "instance", "service", "api_version"] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let listener = if listener.local_addr().unwrap().port() == configured {
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap()
            } else {
                listener
            };
            runtime
                .paths
                .publish_agent_discovery(&nexus_core::AgentDiscoveryRecord {
                    port: listener.local_addr().unwrap().port(),
                    instance_id: "instance".to_owned(),
                    data_root_id: runtime.data_root_id.clone(),
                    pid: std::process::id(),
                    updated_at_unix: 0,
                })
                .unwrap();
            let mut health =
                HealthResponse::healthy(runtime.data_root_id.clone(), "instance".to_owned());
            match invalid {
                "root" => health.data_root_id = "other".to_owned(),
                "instance" => health.instance_id = "other".to_owned(),
                "service" => health.service = "other".to_owned(),
                _ => health.api_version = "invalid".to_owned(),
            }
            let server = tokio::spawn(serve_health_once(listener, health));
            assert!(runtime.probe().await.is_err());
            assert_eq!(runtime.client().base_url().port(), Some(configured));
            server.await.unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires NEXUS_TEST_AGENT_BINARY pointing to a built Agent"]
    async fn real_agent_dynamic_port_config_and_shutdown() {
        let binary =
            PathBuf::from(env::var_os("NEXUS_TEST_AGENT_BINARY").expect("built Agent path"));
        let root = env::temp_dir().join(format!(
            "nexus-real-port-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let unused = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let configured = unused.local_addr().unwrap().port();
        drop(unused);
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(root.clone()),
                port: configured,
            },
            Some(binary.clone()),
        )
        .unwrap();
        for generation in 0..2 {
            let mut command = Command::new(&binary);
            command
                .args(["--port", "0", "--data-dir"])
                .arg(&root)
                .arg("--instance-id")
                .arg(format!("real-instance-{generation}"))
                .env_remove("NEXUS_HARNESS_PROGRAM")
                .env_remove("NEXUS_HARNESS_ARGS")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            configure_agent_process(&mut command);
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if runtime.probe().await.is_ok() {
                    break;
                }
                assert!(Instant::now() < deadline, "Agent did not become ready");
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "Agent exited before readiness"
                );
                sleep(Duration::from_millis(100)).await;
            }
            let record = runtime.paths.read_agent_discovery().unwrap().unwrap();
            let started = runtime.start(2).await.unwrap();
            assert!(!started.started);
            assert_eq!(started.port, record.port);
            let client = runtime
                .client()
                .with_expected_identity(AgentIdentity::from(&started.health));
            let config: serde_json::Value = client.get_json("/v1/config").await.unwrap();
            assert!(config.is_object());
            runtime.stop(10).await.unwrap();
            assert!(timeout(Duration::from_secs(10), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn agent_routes_are_loopback_versioned_and_bounded() {
        assert!(is_allowed_agent_route("/v1/health"));
        assert!(is_allowed_agent_route("/v1/harness/ui"));
        assert!(is_allowed_agent_route("/v1/harness/discover"));
        assert!(is_allowed_agent_route("/v1/releases/tags"));
        assert!(is_allowed_agent_route("/v1/runtime"));
        assert!(is_allowed_agent_route("/v1/runtime/plan"));
        assert!(!is_allowed_agent_route("/launcher/status"));
        assert!(validate_agent_request("/v1/health", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/harness/discover", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/runtime", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/recovery", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/preflight", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/preflight", &Method::POST, Some(b"{}")).is_err());
        assert!(validate_agent_request("/v1/recovery", &Method::POST, Some(b"{\"action\":\"enter\"}")).is_ok());
        assert!(validate_agent_request("/v1/runtime", &Method::POST, None).is_err());
        assert!(validate_agent_request("/v1/runtime", &Method::GET, Some(b"{}")).is_err());
        assert!(validate_agent_request("/v1/runtime/plan", &Method::GET, None).is_err());
        assert!(validate_agent_request("/v1/runtime/plan", &Method::POST, Some(b"{}")).is_ok());
        assert!(validate_agent_request("/v1/releases/tags", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/releases/tags", &Method::POST, None).is_err());
        assert!(validate_agent_request("/v1/releases/tags", &Method::GET, Some(b"{}")).is_err());
        assert!(
            validate_agent_request("/v1/health?url=http://example", &Method::GET, None).is_err()
        );
        assert!(validate_agent_request("/v1/state", &Method::POST, Some(b"{}")).is_err());
        assert!(validate_agent_request("/v1/config", &Method::POST, Some(b"{}")).is_ok());
        assert!(validate_agent_request(
            "/v1/config",
            &Method::POST,
            Some(&vec![b'x'; MAX_REQUEST_BODY_BYTES + 1]),
        )
        .is_err());
    }

    #[test]
    fn health_identity_is_copied_without_secrets() {
        let health = HealthResponse::healthy("root".to_owned(), "instance".to_owned());
        assert_eq!(
            AgentIdentity::from(&health),
            AgentIdentity {
                data_root_id: "root".to_owned(),
                instance_id: "instance".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn agent_client_retains_non_success_status_and_body_bytes() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("Agent error listener binds");
        let port = listener.local_addr().expect("Agent error address").port();
        let body = br#"{"api_version":"v1","code":"busy","message":"Agent lifecycle is busy"}"#;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("Agent error request accepts");
            let mut request = [0_u8; 4096];
            let _ = socket
                .read(&mut request)
                .await
                .expect("Agent error request reads");
            write_raw_response(&mut socket, "409 Conflict", body).await;
        });

        let client = AgentClient::new(port).expect("Agent client creates");
        let error = client
            .request_raw_value(Method::GET, "/v1/state", None)
            .await
            .expect_err("Agent HTTP error is retained");
        match error {
            AgentClientError::Http {
                status,
                body: response_body,
                ..
            } => {
                assert_eq!(status, StatusCode::CONFLICT);
                assert_eq!(response_body.as_slice(), body);
                let value: Value =
                    serde_json::from_slice(&response_body).expect("Agent error body is JSON");
                assert_eq!(value["api_version"], "v1");
                assert_eq!(value["code"], "busy");
            }
            other => panic!("expected HTTP error, got {other}"),
        }
        server.await.expect("Agent error server completes");
    }

    #[test]
    fn program_resolution_rejects_missing_explicit_path() {
        let error = resolve_agent_program(Some(Path::new("C:/does-not-exist/nexus-agent.exe")))
            .expect_err("missing executable must fail closed");
        assert!(error.to_string().contains("does not name an existing file"));
    }

    #[test]
    fn resource_directory_is_a_bounded_agent_resolution_source() {
        if env::var_os(AGENT_BINARY_ENV).is_some() {
            // A caller-provided override intentionally wins over packaged
            // resources; do not mutate process-global environment in a test.
            return;
        }
        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-resource-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        let resource_dir = root.join("resources");
        fs::create_dir_all(&resource_dir).expect("resource directory creates");
        let resource_name = if cfg!(windows) {
            "nexus-agent.exe"
        } else {
            "nexus-agent"
        };
        let resource_program = resource_dir.join(resource_name);
        fs::write(&resource_program, b"packaged-agent").expect("resource marker writes");
        let resolved = resolve_agent_program_with_resource_dir(None, Some(&resource_dir))
            .expect("packaged Agent resolves");
        assert_eq!(resolved, resource_program);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installed_resource_resolution_prefers_same_directory_and_supports_nested_fallback() {
        if env::var_os(AGENT_BINARY_ENV).is_some() {
            return;
        }
        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-installed-resource-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        let resource_dir = root.join("resources");
        fs::create_dir_all(&resource_dir).expect("resource directory creates");
        let resource_name = if cfg!(windows) {
            "nexus-agent.exe"
        } else {
            "nexus-agent"
        };
        let same_directory = root.join(resource_name);
        let nested = resource_dir.join(resource_name);
        fs::write(&same_directory, b"same-directory-agent").expect("same-directory marker writes");
        fs::write(&nested, b"nested-agent").expect("nested marker writes");
        assert_eq!(
            resolve_agent_program_with_resource_dir(None, Some(&root))
                .expect("same-directory Agent resolves"),
            same_directory
        );
        fs::remove_file(&same_directory).expect("same-directory marker removes");
        assert_eq!(
            resolve_agent_program_with_resource_dir(None, Some(&root))
                .expect("nested resource Agent resolves"),
            nested
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn already_running_agent_start_reports_resolved_program() {
        if env::var_os(AGENT_BINARY_ENV).is_some() {
            return;
        }
        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-running-agent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        let resource_dir = root.join("resources");
        fs::create_dir_all(&resource_dir).expect("resource directory creates");
        let resource_name = if cfg!(windows) {
            "nexus-agent.exe"
        } else {
            "nexus-agent"
        };
        let resource_program = resource_dir.join(resource_name);
        fs::write(&resource_program, b"packaged-agent").expect("resource marker writes");

        let build_id = option_env!("NEXUS_BUILD_ID").unwrap_or("running-fixture-build");
        fs::write(resource_dir.join("release-identity.json"), serde_json::to_vec(&serde_json::json!({"schemaVersion":1,"buildId":build_id})).unwrap()).unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("existing Agent listener binds");
        let port = listener
            .local_addr()
            .expect("existing Agent address")
            .port();
        let runtime = AgentRuntime::new_with_resource_dir(
            NexusConfig {
                data_dir: Some(root.clone()),
                port,
            },
            None,
            Some(resource_dir),
        )
        .expect("runtime creates");
        let mut health = HealthResponse::healthy(
            runtime.data_root_id().to_owned(),
            "existing-agent-instance".to_owned(),
        );
        health.build_id = Some(build_id.into());
        health.binary_path = Some(resource_program.to_string_lossy().into_owned());
        let server = tokio::spawn(serve_health_once(listener, health));
        let result = runtime.start(1).await.expect("existing Agent is accepted");
        assert!(!result.started);
        assert_eq!(result.program, Some(resource_program));
        server.await.expect("health server completes");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn stop_waits_for_owned_child_after_listener_disappears() {
        use nexus_protocol::{LifecycleAccepted, LifecycleAction};

        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-slow-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("runtime root creates");
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("slow Agent listener binds");
        let port = listener.local_addr().expect("slow Agent address").port();
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(root.clone()),
                port,
            },
            None,
        )
        .expect("runtime creates");
        let health = HealthResponse::healthy(
            runtime.data_root_id().to_owned(),
            "slow-stop-instance".to_owned(),
        );
        fs::create_dir_all(&runtime.paths.run_dir).unwrap();
        let credential = nexus_core::agent_auth::AgentCredential::publish(&runtime.paths, "slow-stop-instance").unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request = [0_u8; 4096];
                let size = socket.read(&mut request).await.expect("slow request reads");
                let request = String::from_utf8_lossy(&request[..size]);
                if request.starts_with("POST /v1/shutdown ") {
                    let nonce = request.lines().find_map(|line| line.strip_prefix("x-nexus-auth-nonce: ")).unwrap();
                    let body = serde_json::to_vec(&LifecycleAccepted::accepted(LifecycleAction::Shutdown)).unwrap();
                    let body = credential.seal_response(nonce, 202, &body).unwrap(); let proof = credential.response_signature(nonce, 202, &body);
                    socket.write_all(format!("HTTP/1.1 202 Accepted\r\nContent-Length: {}\r\nx-nexus-auth-version: 2\r\nx-nexus-auth-response: {proof}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                    // The HTTP listener disappears before the child process
                    // exits, modelling Harness supervisor drain time.
                    sleep(Duration::from_millis(300)).await;
                    break;
                }
                write_json_response(&mut socket, "200 OK", &health).await;
            }
        });
        let child = Command::new("cmd.exe")
            .args(["/C", "ping -n 4 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("slow child spawns");
        runtime
            .child
            .lock()
            .expect("child lock is healthy")
            .replace(child.into());

        let started = std::time::Instant::now();
        runtime
            .stop(5)
            .await
            .expect("graceful stop waits for child");
        assert!(started.elapsed() >= Duration::from_millis(500));
        server.await.expect("slow Agent server completes");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_port_impostor_never_receives_plaintext_configuration() {
        use nexus_core::agent_auth::{self, AgentCredential};
        let root = env::temp_dir().join(format!("nexus-impostor-{}", agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone());
        fs::create_dir_all(&paths.run_dir).unwrap();
        let credential = AgentCredential::publish(&paths, "old-generation").unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut captured = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count != 0); captured.extend_from_slice(&chunk[..count]);
                if let Some(index) = captured.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&captured[..index]);
                    let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if captured.len() >= index + 4 + length { break; }
                }
            }
            assert!(!captured.windows(b"SECRET_SENTINEL".len()).any(|window| window == b"SECRET_SENTINEL"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let client = AgentClient::new(port).unwrap().with_credential_paths(paths)
            .with_expected_identity(AgentIdentity { data_root_id: credential.data_root_id, instance_id: credential.instance_id });
        let result = client.post_json::<_, serde_json::Value>("/v1/config", &serde_json::json!({"action":"set_harness", "harness":{"args":["SECRET_SENTINEL"],"readiness_url":"http://localhost/?token=SECRET_SENTINEL"}})).await;
        assert!(matches!(result, Err(AgentClientError::InvalidResponse(_))));
        server.await.unwrap(); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bootstrap_lock_is_shared_across_runtime_instances() {
        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("run directories create");
        let first = acquire_bootstrap_lock(&paths).expect("first runtime acquires bootstrap lock");
        let second = acquire_bootstrap_lock(&paths).expect_err("second runtime is serialized");
        assert!(second.to_string().contains("another launcher is starting"));
        drop(first);
        let recovered = acquire_bootstrap_lock(&paths).expect("released lock can be reused");
        drop(recovered);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_lock_wait_retries_after_previous_agent_releases_lock() {
        let root = env::temp_dir().join(format!(
            "nexus-launcher-core-runtime-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after epoch")
                .as_nanos()
        ));
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(root.clone()),
                port: 1,
            },
            None,
        )
        .expect("runtime creates");
        let lock_path = runtime.paths.run_dir.join("agent.lock");
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .expect("Agent lock opens");
        held.try_lock().expect("test owns Agent lock");
        let releaser = tokio::spawn(async move {
            sleep(Duration::from_millis(200)).await;
            drop(held);
        });

        let started = std::time::Instant::now();
        runtime
            .wait_for_runtime_lock_available(2)
            .await
            .expect("runtime waits for the previous Agent lock to release");
        assert!(started.elapsed() >= Duration::from_millis(100));
        releaser.await.expect("lock releaser completes");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_agent_process_flags_hide_console_and_break_away() {
        assert_ne!(agent_creation_flags() & 0x0800_0000, 0);
        assert_ne!(agent_creation_flags() & 0x0000_0200, 0);
        assert_ne!(agent_creation_flags() & 0x0100_0000, 0);
    }
}
