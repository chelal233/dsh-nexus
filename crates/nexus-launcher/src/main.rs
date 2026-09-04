//! Single-entry host for the headless Nexus Agent and native Launcher API.
//!
//! The launcher owns process bootstrap metadata and the native GUI API boundary
//! under Nexus' `run/` directory. It does not own Harness state, profile data,
//! release pointers, or business logic; those remain in the Agent and are
//! reachable through its loopback v1 API.

use std::{
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{self, Command as StdCommand, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use hmac::{Hmac, Mac};
use nexus_core::{
    data_root_identity, load_harness_launch_spec, log_file_identity, new_instance_id,
    HarnessLogSession, HarnessLogSessionStore, NexusConfig, NexusPaths,
};
use nexus_protocol::{
    HarnessAction, HarnessCommand, HarnessResponse, HarnessState, HealthResponse, HealthStatus,
    StateResponse,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::{
    net::TcpListener,
    process::{Child, Command as TokioCommand},
    sync::Mutex,
    time::{sleep, Instant},
};

const DEFAULT_WAIT_SECS: u64 = 20;
const DEFAULT_STOP_WAIT_SECS: u64 = 15;
const DEFAULT_CONSOLE_PORT: u16 = 3091;
const DEFAULT_LAUNCHER_SCHEMA_VERSION: u32 = 1;
const HARNESS_LOG_TAIL_BYTES: u64 = 64 * 1024;
const AGENT_BINARY_ENV: &str = "NEXUS_AGENT_BIN";
const CONSOLE_PORT_ENV: &str = "NEXUS_CONSOLE_PORT";
const LAUNCHER_WAIT_SECS_ENV: &str = "NEXUS_LAUNCHER_WAIT_SECS";
const CONSOLE_OPEN_ENV: &str = "NEXUS_CONSOLE_OPEN";
const LAUNCHER_CONFIG_FILE: &str = "launcher.json";
const PROXY_DATA_ROOT_HEADER: &str = "x-nexus-data-root-id";
const PROXY_INSTANCE_HEADER: &str = "x-nexus-instance-id";
const LAUNCHER_DATA_ROOT_HEADER: &str = "x-nexus-launcher-data-root-id";
const LAUNCHER_INSTANCE_HEADER: &str = "x-nexus-launcher-instance-id";
const LAUNCHER_CAPABILITY_HEADER: &str = "x-nexus-launcher-capability";
const LAUNCHER_CHALLENGE_HEADER: &str = "x-nexus-launcher-challenge";
const LAUNCHER_PROOF_HEADER: &str = "x-nexus-launcher-proof";
const LAUNCHER_CAPABILITY_ENV: &str = "NEXUS_LAUNCHER_CAPABILITY";
const MAX_AGENT_PROXY_BODY_BYTES: usize = 32 * 1024;
const MAX_AGENT_PROXY_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Clone)]
struct Options {
    command: LauncherCommand,
    config: NexusConfig,
    agent_program: Option<PathBuf>,
    console_port: u16,
    wait_secs: u64,
    json: bool,
    launcher_instance_id: String,
    launcher_capability: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherCommand {
    Start,
    Run,
    /// Headless loopback API for the native Tauri shell.
    Api,
    /// Compatibility alias for older scripts. It never serves HTML.
    Console,
    Stop,
    Status,
    Logs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ConsoleStatus {
    running: bool,
    desired_agent_running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_api: Option<String>,
    console_url: String,
    data_root: String,
    data_root_id: String,
    launcher_instance_id: String,
    agent_pid: Option<u32>,
    agent_program: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LauncherConfigFile {
    #[serde(default = "default_launcher_schema_version")]
    schema_version: u32,
    #[serde(default)]
    agent_program: Option<PathBuf>,
    #[serde(default)]
    agent_port: Option<u16>,
    #[serde(default)]
    console_dir: Option<PathBuf>,
    #[serde(default)]
    console_port: Option<u16>,
    #[serde(default)]
    wait_secs: Option<u64>,
    #[serde(default)]
    open_browser: Option<bool>,
}

impl Default for LauncherConfigFile {
    fn default() -> Self {
        Self {
            schema_version: DEFAULT_LAUNCHER_SCHEMA_VERSION,
            agent_program: None,
            agent_port: None,
            console_dir: None,
            console_port: None,
            wait_secs: None,
            open_browser: None,
        }
    }
}

impl LauncherConfigFile {
    fn validate(&self, path: &Path) -> Result<(), String> {
        if self.schema_version != DEFAULT_LAUNCHER_SCHEMA_VERSION {
            return Err(format!(
                "{} has unsupported schema_version {}; expected {}",
                path.display(),
                self.schema_version,
                DEFAULT_LAUNCHER_SCHEMA_VERSION
            ));
        }
        validate_optional_path(self.agent_program.as_deref(), "agent_program")?;
        validate_optional_path(self.console_dir.as_deref(), "console_dir")?;
        validate_optional_port(self.agent_port, "agent_port")?;
        validate_optional_port(self.console_port, "console_port")?;
        if let Some(seconds) = self.wait_secs {
            if !(1..=300).contains(&seconds) {
                return Err(format!(
                    "{} wait_secs must be between 1 and 300",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

fn default_launcher_schema_version() -> u32 {
    DEFAULT_LAUNCHER_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HarnessUiInfo {
    available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleHarnessCommand {
    action: ConsoleHarnessAction,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsoleHarnessAction {
    Open,
    Status,
}

#[derive(Debug, Clone, Deserialize)]
struct ConsoleAgentCommand {
    action: ConsoleAgentAction,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConsoleAgentAction {
    Start,
    Stop,
    Restart,
    Status,
}

#[derive(Clone)]
struct ConsoleController {
    options: Options,
    paths: NexusPaths,
    client: Client,
    state: std::sync::Arc<Mutex<ConsoleRuntimeState>>,
    operation: std::sync::Arc<Mutex<()>>,
    harness_logs: std::sync::Arc<Mutex<HarnessLogObserver>>,
    data_root_id: String,
    launcher_instance_id: String,
    launcher_capability: String,
}

#[derive(Debug, Clone)]
struct ConsoleRuntimeState {
    desired_agent_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchRecord {
    pid: u32,
    port: u16,
    started_at_unix: u64,
    agent_program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_instance_id: Option<String>,
}

struct InstanceLock {
    _file: fs::File,
}

struct OwnedAgent {
    child: Option<Child>,
    pid: u32,
}

struct SpawnedAgent {
    child: Option<Child>,
    pid: u32,
    #[cfg(windows)]
    process_handle: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl Drop for SpawnedAgent {
    fn drop(&mut self) {
        if let Some(handle) = self.process_handle.take() {
            let _ = terminate_windows_process(handle);
        }
    }
}

#[tokio::main]
async fn main() {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(message) => {
            eprintln!("nexus-launcher: {message}");
            eprintln!("use --help for usage");
            process::exit(2);
        }
    };

    if let Err(message) = run(options).await {
        eprintln!("nexus-launcher: {message}");
        process::exit(1);
    }
}

fn parse_args() -> Result<Option<Options>, String> {
    parse_args_from(env::args_os().skip(1))
}

fn parse_args_from<I, S>(arguments: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let raw_args: Vec<std::ffi::OsString> = arguments.into_iter().map(Into::into).collect();
    if raw_args
        .iter()
        .any(|argument| matches!(argument.to_string_lossy().as_ref(), "--help" | "-h"))
    {
        print_help();
        return Ok(None);
    }

    let mut config = NexusConfig::from_env();
    if let Some(data_dir) = cli_option_value(&raw_args, "--data-dir")? {
        config.data_dir = Some(validate_path_value(data_dir, "--data-dir")?);
    }
    let launcher_config = load_launcher_config(&config.paths())?;
    let mut command = None;
    let mut agent_program = launcher_config.agent_program.clone();
    // These options remain accepted for compatibility with older scripts, but
    // the native GUI owns the visible surface and the API never opens HTML.
    let mut _legacy_console_dir = launcher_config.console_dir.clone();
    let mut console_port = launcher_config
        .console_port
        .or_else(|| env_port(CONSOLE_PORT_ENV))
        .unwrap_or(DEFAULT_CONSOLE_PORT);
    let mut wait_secs = launcher_config
        .wait_secs
        .or_else(env_wait_secs)
        .unwrap_or(DEFAULT_WAIT_SECS);
    let mut _legacy_no_open = !launcher_config
        .open_browser
        .or_else(env_open_browser)
        .unwrap_or(true);
    let mut json = false;
    let mut launcher_instance_id = new_instance_id();
    if let Some(port) = launcher_config.agent_port {
        config.port = port;
    }

    let mut index = 0usize;
    while index < raw_args.len() {
        let argument = &raw_args[index];
        match argument.to_string_lossy().as_ref() {
            "start" if command.is_none() => command = Some(LauncherCommand::Start),
            "run" | "foreground" if command.is_none() => command = Some(LauncherCommand::Run),
            "api" if command.is_none() => command = Some(LauncherCommand::Api),
            "console" if command.is_none() => command = Some(LauncherCommand::Console),
            "stop" if command.is_none() => command = Some(LauncherCommand::Stop),
            "status" if command.is_none() => command = Some(LauncherCommand::Status),
            "logs" if command.is_none() => command = Some(LauncherCommand::Logs),
            "--data-dir" => {
                let value = next_cli_value(&raw_args, &mut index, "--data-dir")?;
                config.data_dir = Some(validate_path_value(value, "--data-dir")?);
            }
            "--port" => {
                let value = next_cli_value(&raw_args, &mut index, "--port")?;
                config.port = parse_port(&value.to_string_lossy())?;
            }
            "--agent" => {
                let value = next_cli_value(&raw_args, &mut index, "--agent")?;
                agent_program = Some(validate_path_value(value, "--agent")?);
            }
            "--console-dir" => {
                let value = next_cli_value(&raw_args, &mut index, "--console-dir")?;
                _legacy_console_dir = Some(validate_path_value(value, "--console-dir")?);
            }
            "--console-port" => {
                let value = next_cli_value(&raw_args, &mut index, "--console-port")?;
                console_port = parse_port(&value.to_string_lossy())?;
            }
            "--wait-secs" => {
                let value = next_cli_value(&raw_args, &mut index, "--wait-secs")?;
                wait_secs = value
                    .to_string_lossy()
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=300).contains(seconds))
                    .ok_or_else(|| "--wait-secs must be between 1 and 300".to_owned())?;
            }
            "--no-open" => _legacy_no_open = true,
            "--open" => _legacy_no_open = false,
            "--json" => json = true,
            "--launcher-instance-id" => {
                let value = next_cli_value(&raw_args, &mut index, "--launcher-instance-id")?
                    .into_string()
                    .map_err(|_| "--launcher-instance-id must be valid Unicode".to_owned())?;
                if value.is_empty() || value.len() > 192 || value.chars().any(char::is_control) {
                    return Err("--launcher-instance-id is invalid".to_owned());
                }
                launcher_instance_id = value;
            }
            value => return Err(format!("unknown argument: {value}")),
        }
        index += 1;
    }

    // The native Tauri shell owns the visible window. A no-argument launcher
    // invocation therefore starts only the headless API and never serves HTML.
    let command = command.unwrap_or(LauncherCommand::Api);
    let launcher_capability_value = env::var(LAUNCHER_CAPABILITY_ENV);
    // Consume the native-only secret before Agent/Harness child processes are
    // spawned so it is never propagated beyond this helper.
    env::remove_var(LAUNCHER_CAPABILITY_ENV);
    let launcher_capability = match launcher_capability_value {
        Ok(value) if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Some(value)
        }
        Ok(_) => {
            return Err(format!(
                "{LAUNCHER_CAPABILITY_ENV} must be a 64-character hexadecimal secret"
            ))
        }
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{LAUNCHER_CAPABILITY_ENV} is not valid Unicode"))
        }
    };

    Ok(Some(Options {
        command,
        config,
        agent_program,
        console_port,
        wait_secs,
        json,
        launcher_instance_id,
        launcher_capability,
    }))
}

fn cli_option_value(
    arguments: &[std::ffi::OsString],
    option: &str,
) -> Result<Option<std::ffi::OsString>, String> {
    let mut value = None;
    let mut index = 0usize;
    while index < arguments.len() {
        if arguments[index].to_string_lossy() == option {
            value = Some(
                arguments
                    .get(index + 1)
                    .cloned()
                    .ok_or_else(|| format!("{option} requires a value"))?,
            );
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(value)
}

fn next_cli_value(
    arguments: &[std::ffi::OsString],
    index: &mut usize,
    option: &str,
) -> Result<std::ffi::OsString, String> {
    let value = arguments
        .get(*index + 1)
        .cloned()
        .ok_or_else(|| format!("{option} requires a value"))?;
    *index += 1;
    Ok(value)
}

fn validate_path_value(value: std::ffi::OsString, option: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
        return Err(format!(
            "{option} must be a non-empty path without control characters"
        ));
    }
    Ok(path)
}

fn validate_optional_path(path: Option<&Path>, field: &str) -> Result<(), String> {
    if let Some(path) = path {
        if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
            return Err(format!(
                "launcher.json {field} must be a non-empty path without control characters"
            ));
        }
    }
    Ok(())
}

fn validate_optional_port(port: Option<u16>, field: &str) -> Result<(), String> {
    if matches!(port, Some(0)) {
        return Err(format!("launcher.json {field} must be between 1 and 65535"));
    }
    Ok(())
}

fn env_port(name: &str) -> Option<u16> {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
}

fn env_wait_secs() -> Option<u64> {
    env::var(LAUNCHER_WAIT_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=300).contains(seconds))
}

fn env_open_browser() -> Option<bool> {
    env::var(CONSOLE_OPEN_ENV).ok().and_then(|value| {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    })
}

fn launcher_config_path(paths: &NexusPaths) -> PathBuf {
    paths.root.join(LAUNCHER_CONFIG_FILE)
}

fn load_launcher_config(paths: &NexusPaths) -> Result<LauncherConfigFile, String> {
    let path = launcher_config_path(paths);
    if !path.is_file() {
        return Ok(LauncherConfigFile::default());
    }
    let bytes = fs::read(&path)
        .map_err(|error| format!("cannot read launcher config {}: {error}", path.display()))?;
    let config: LauncherConfigFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse launcher config {}: {error}", path.display()))?;
    config.validate(&path)?;
    Ok(config)
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("invalid port: {value}"))
}

async fn run(options: Options) -> Result<(), String> {
    let paths = options.config.paths();
    paths
        .ensure_directories()
        .map_err(|error| format!("cannot initialize Nexus directories: {error}"))?;
    let client = Client::builder()
        // Keep the initial probe alive long enough for a just-spawned Agent to
        // bind its loopback listener. A one-second connect timeout can leave
        // the launcher in a retry storm when another local client is polling
        // the same port during startup.
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|error| format!("cannot initialize HTTP client: {error}"))?;

    // Keep the Agent's browser CORS allowlist aligned with the legacy browser
    // port. The variable is inherited by a newly spawned Agent; an already-running
    // Agent must have been started with the same launcher configuration.
    env::set_var(CONSOLE_PORT_ENV, options.console_port.to_string());

    match options.command {
        LauncherCommand::Start => {
            let result = ensure_agent_started(&options, &paths, &client).await?;
            match result {
                EnsureResult::AlreadyRunning => print_started(&options, None, &paths, true),
                EnsureResult::Owned(owned) => {
                    let pid = owned.pid;
                    drop(owned.child);
                    print_started(&options, Some(pid), &paths, false);
                }
            }
        }
        LauncherCommand::Run => {
            let EnsureResult::Owned(owned) =
                ensure_agent_started(&options, &paths, &client).await?
            else {
                return Err(
                    "Agent is already running; use `nexus-launcher status` or `stop`, or run foreground after stopping it"
                        .to_owned(),
                );
            };
            run_foreground(owned, &options, &paths, &client).await?;
        }
        LauncherCommand::Api | LauncherCommand::Console => {
            run_api(&options, &paths, &client).await?
        }
        LauncherCommand::Stop => stop_agent(&options, &paths, &client).await?,
        LauncherCommand::Status => status_agent(&options, &paths, &client).await?,
        LauncherCommand::Logs => print_logs(&options, &paths),
    }

    Ok(())
}

enum EnsureResult {
    AlreadyRunning,
    Owned(OwnedAgent),
}

impl ConsoleController {
    fn new(options: Options, paths: NexusPaths, client: Client) -> Result<Self, String> {
        let data_root_id = data_root_identity(&paths)
            .map_err(|error| format!("cannot identify Launcher data root: {error}"))?;
        let launcher_instance_id = options.launcher_instance_id.clone();
        let launcher_capability = options.launcher_capability.clone().ok_or_else(|| {
            "Launcher API requires a private capability supplied by the native owner".to_owned()
        })?;
        Ok(Self {
            options,
            paths,
            client,
            state: std::sync::Arc::new(Mutex::new(ConsoleRuntimeState {
                desired_agent_running: true,
            })),
            operation: std::sync::Arc::new(Mutex::new(())),
            harness_logs: std::sync::Arc::new(Mutex::new(HarnessLogObserver::default())),
            data_root_id,
            launcher_instance_id,
            launcher_capability,
        })
    }

    async fn start_agent(&self) -> Result<ConsoleStatus, String> {
        let _operation = self.operation.lock().await;
        self.start_agent_locked().await?;
        Ok(self.status().await)
    }

    async fn start_agent_locked(&self) -> Result<(), String> {
        let mut options = self.options.clone();
        options.command = LauncherCommand::Start;
        let result = ensure_agent_started(&options, &self.paths, &self.client).await?;
        match result {
            EnsureResult::AlreadyRunning => {}
            EnsureResult::Owned(owned) => {
                drop(owned.child);
            }
        }
        {
            let mut state = self.state.lock().await;
            state.desired_agent_running = true;
        }

        // A console launch is the user-facing one-shot path. A missing Harness
        // configuration is intentionally non-fatal: the control plane remains
        // available so the user can configure it from the Console.
        self.start_harness_if_configured().await;
        Ok(())
    }

    async fn stop_agent(&self) -> Result<ConsoleStatus, String> {
        let _operation = self.operation.lock().await;
        {
            let mut state = self.state.lock().await;
            state.desired_agent_running = false;
        }
        let result = stop_agent(&self.options, &self.paths, &self.client).await;
        result?;
        Ok(self.status().await)
    }

    async fn restart_agent(&self) -> Result<ConsoleStatus, String> {
        let _operation = self.operation.lock().await;
        {
            let mut state = self.state.lock().await;
            state.desired_agent_running = true;
        }
        stop_agent(&self.options, &self.paths, &self.client).await?;
        self.start_agent_locked().await?;
        Ok(self.status().await)
    }

    async fn status(&self) -> ConsoleStatus {
        let health = probe_health(&self.client, self.options.config.port, &self.paths)
            .await
            .ok()
            .flatten()
            .filter(|health| health.status == HealthStatus::Ok);
        let record = read_launch_record(&self.paths);
        let state = self.state.lock().await.clone();
        let (agent_pid, agent_program) = if let Some(health) = health.as_ref() {
            record
                .filter(|record| {
                    record.agent_instance_id.as_deref() == Some(health.instance_id.as_str())
                })
                .map(|record| (Some(record.pid), Some(record.agent_program)))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        ConsoleStatus {
            running: health.is_some(),
            desired_agent_running: state.desired_agent_running,
            agent_api: health
                .as_ref()
                .map(|_| format!("http://127.0.0.1:{}", self.options.config.port)),
            console_url: format!("http://127.0.0.1:{}/", self.options.console_port),
            data_root: self.paths.root.display().to_string(),
            data_root_id: self.data_root_id.clone(),
            launcher_instance_id: self.launcher_instance_id.clone(),
            agent_pid,
            agent_program,
        }
    }

    async fn harness_ui(&self) -> HarnessUiInfo {
        let first = match self.fetch_harness_observation().await {
            Ok(response)
                if response.harness.state == HarnessState::Running
                    && response.harness.pid.is_some() =>
            {
                response
            }
            Ok(response) if response.harness.state == HarnessState::Running => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(
                    &self.paths,
                    "Harness is running without an Agent-owned process identity; token-session continuity cannot be proven"
                        .to_owned(),
                );
            }
            Ok(response) => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(
                    &self.paths,
                    format!(
                        "Harness is {:?}; a current authentication token is not available",
                        response.harness.state
                    ),
                );
            }
            Err(error) => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(&self.paths, error);
            }
        };
        let session = match HarnessLogSessionStore::new(self.paths.clone()).read() {
            Ok(Some(session)) => session,
            Ok(None) => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(
                    &self.paths,
                    "Harness log session marker is not available; restart Harness to establish a safe token boundary"
                        .to_owned(),
                );
            }
            Err(error) => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(
                    &self.paths,
                    format!("Harness log session marker is invalid: {error}"),
                );
            }
        };
        if !harness_observation_matches_session(&first, &session) {
            let mut observer = self.harness_logs.lock().await;
            observer.invalidate();
            return unavailable_harness_ui_info(
                &self.paths,
                "Agent Harness observation does not match the durable log session marker"
                    .to_owned(),
            );
        }
        let second = match self.fetch_harness_observation().await {
            Ok(response) => response,
            Err(error) => {
                let mut observer = self.harness_logs.lock().await;
                observer.invalidate();
                return unavailable_harness_ui_info(&self.paths, error);
            }
        };
        if first != second
            || second.harness.state != HarnessState::Running
            || second.harness.pid.is_none()
            || !harness_observation_matches_session(&second, &session)
        {
            let mut observer = self.harness_logs.lock().await;
            observer.invalidate();
            return unavailable_harness_ui_info(
                &self.paths,
                "Harness changed state while its token was being observed; refresh after it is running"
                    .to_owned(),
            );
        }
        let info = {
            let mut observer = self.harness_logs.lock().await;
            read_harness_ui_info_with_observer(&self.paths, &mut observer, Some(&session))
        };
        let final_session = HarnessLogSessionStore::new(self.paths.clone()).read();
        let final_observation = self.fetch_harness_observation().await;
        if !matches!(final_session, Ok(Some(ref current)) if current == &session)
            || !matches!(final_observation, Ok(ref current) if current == &second && current.harness.pid.is_some())
        {
            let mut observer = self.harness_logs.lock().await;
            observer.invalidate();
            return unavailable_harness_ui_info(
                &self.paths,
                "Harness changed state while its token was being observed; refresh after it is running"
                    .to_owned(),
            );
        }
        info
    }

    async fn fetch_harness_observation(&self) -> Result<HarnessResponse, String> {
        let response = verified_agent_request(
            &self.client,
            self.options.config.port,
            &self.paths,
            Method::GET,
            "/v1/harness",
        )
        .await?
        .send()
        .await
        .map_err(|error| format!("Agent Harness status is unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Agent Harness status returned HTTP {}",
                response.status()
            ));
        }
        response
            .json::<HarnessResponse>()
            .await
            .map_err(|error| format!("Agent Harness status is invalid: {error}"))
    }

    async fn open_harness(&self) -> Result<HarnessUiInfo, String> {
        let info = self.harness_ui().await;
        let Some(url) = info.url.as_deref() else {
            return Err(info.message.unwrap_or_else(|| {
                "Harness authentication URL was not found in the recent Harness log".to_owned()
            }));
        };
        open_browser_url(url)?;
        Ok(info)
    }

    async fn start_harness_if_configured(&self) {
        if !matches!(load_harness_launch_spec(&self.paths), Ok(Some(_))) {
            return;
        }
        let response = match verified_agent_request(
            &self.client,
            self.options.config.port,
            &self.paths,
            Method::POST,
            "/v1/harness",
        )
        .await
        {
            Ok(request) => {
                request
                    .json(&HarnessCommand {
                        action: HarnessAction::Start,
                    })
                    .send()
                    .await
            }
            Err(error) => {
                eprintln!("nexus-launcher: Console auto-start Harness skipped: {error}");
                return;
            }
        };
        if let Ok(response) = response {
            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::CONFLICT {
                eprintln!("nexus-launcher: Console auto-start Harness returned HTTP {status}");
            }
        }
    }

    async fn watchdog(self) {
        loop {
            sleep(Duration::from_secs(1)).await;
            let desired = self.state.lock().await.desired_agent_running;
            if desired
                && !is_agent_healthy(&self.client, self.options.config.port, &self.paths)
                    .await
                    .unwrap_or(false)
            {
                let _ = self.start_agent().await;
            }
        }
    }
}

async fn run_api(options: &Options, paths: &NexusPaths, client: &Client) -> Result<(), String> {
    let controller = ConsoleController::new(options.clone(), paths.clone(), client.clone())?;
    let listener = TcpListener::bind(console_bind_addr(options.console_port))
        .await
        .map_err(|error| {
            format!(
                "cannot bind Launcher API at 127.0.0.1:{}: {error}",
                options.console_port
            )
        })?;
    controller.start_agent().await?;
    let app = build_api_router(controller.clone());
    println!(
        "nexus launcher api: http://127.0.0.1:{}/ (Agent {})",
        options.console_port,
        controller
            .status()
            .await
            .agent_api
            .as_deref()
            .unwrap_or("unavailable")
    );

    let watchdog = tokio::spawn(controller.clone().watchdog());
    let server = axum::serve(listener, app);
    let server_result = tokio::select! {
        result = server => result.map_err(|error| format!("Launcher API server failed: {error}")),
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| format!("cannot listen for Ctrl+C: {error}"))?;
            println!("nexus launcher api stopped; Agent remains running (use `nexus-launcher stop` to stop it)");
            Ok(())
        }
    };
    watchdog.abort();
    server_result
}

fn console_bind_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn open_browser_url(url: &str) -> Result<(), String> {
    let Some((safe_url, _)) = parse_loopback_harness_url(url) else {
        return Err("refusing to open a non-loopback HTTP Harness URL".to_owned());
    };
    if safe_url != url {
        return Err("refusing to open a non-canonical Harness URL".to_owned());
    }
    #[cfg(windows)]
    {
        // Pass the canonical URL as one argument to Explorer. This avoids the
        // cmd.exe command parser used by the legacy `start` opener.
        let status = StdCommand::new("explorer.exe")
            .arg(url)
            .status()
            .map_err(|error| format!("failed to invoke the system browser: {error}"))?;
        if !status.success() {
            return Err(format!("system browser exited with {status}"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let status = StdCommand::new("open")
            .arg(url)
            .status()
            .map_err(|error| format!("failed to invoke the system browser: {error}"))?;
        if !status.success() {
            return Err(format!("system browser exited with {status}"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = StdCommand::new("xdg-open")
            .arg(url)
            .status()
            .map_err(|error| format!("failed to invoke the system browser: {error}"))?;
        if !status.success() {
            return Err(format!("system browser exited with {status}"));
        }
    }
    Ok(())
}

fn build_api_router(controller: ConsoleController) -> Router {
    let identity = controller.clone();
    Router::new()
        .route("/launcher/status", get(console_status))
        .route("/launcher/handshake", get(console_handshake))
        .route(
            "/launcher/agent",
            get(console_agent_status).post(console_agent_control),
        )
        .route(
            "/launcher/harness",
            get(console_harness_status).post(console_harness_control),
        )
        .route("/launcher/logs", get(console_logs))
        .route("/launcher/agent-api/v1/health", get(proxy_agent_request))
        .route("/launcher/agent-api/v1/state", get(proxy_agent_request))
        .route(
            "/launcher/agent-api/v1/harness",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/profiles",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/checkpoints",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/releases",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/updates",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/diagnostics",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .route(
            "/launcher/agent-api/v1/config",
            get(proxy_agent_request).post(proxy_agent_request),
        )
        .layer(middleware::from_fn_with_state(
            identity,
            enforce_launcher_identity,
        ))
        .with_state(controller)
}

async fn console_handshake(
    State(controller): State<ConsoleController>,
    request: Request,
) -> Response {
    let challenge = match request
        .headers()
        .get(LAUNCHER_CHALLENGE_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            value
        }
        _ => {
            return launcher_error_response(
                StatusCode::BAD_REQUEST,
                "Launcher handshake requires a 256-bit hexadecimal challenge".to_owned(),
            )
        }
    };
    let proof = launcher_handshake_proof(
        &controller.launcher_capability,
        challenge,
        &controller.data_root_id,
        &controller.launcher_instance_id,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(LAUNCHER_PROOF_HEADER, proof)
        .body(Body::empty())
        .expect("fixed Launcher handshake response is valid")
}

fn launcher_handshake_proof(
    capability: &str,
    challenge: &str,
    data_root_id: &str,
    launcher_instance_id: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(capability.as_bytes())
        .expect("HMAC accepts a capability of any length");
    mac.update(b"nexus-launcher-handshake-v1");
    for value in [challenge, data_root_id, launcher_instance_id] {
        mac.update(&(value.len() as u64).to_be_bytes());
        mac.update(value.as_bytes());
    }
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn launcher_identity_matches(
    headers: &axum::http::HeaderMap,
    controller: &ConsoleController,
) -> bool {
    launcher_identity_values_match(
        headers,
        &controller.data_root_id,
        &controller.launcher_instance_id,
    )
}

fn launcher_capability_matches(
    headers: &axum::http::HeaderMap,
    controller: &ConsoleController,
) -> bool {
    launcher_capability_values_match(headers, &controller.launcher_capability)
}

fn launcher_capability_values_match(
    headers: &axum::http::HeaderMap,
    launcher_capability: &str,
) -> bool {
    headers
        .get(LAUNCHER_CAPABILITY_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some(launcher_capability)
}

fn launcher_identity_values_match(
    headers: &axum::http::HeaderMap,
    data_root_id: &str,
    launcher_instance_id: &str,
) -> bool {
    headers
        .get(LAUNCHER_DATA_ROOT_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some(data_root_id)
        && headers
            .get(LAUNCHER_INSTANCE_HEADER)
            .and_then(|value| value.to_str().ok())
            == Some(launcher_instance_id)
}

async fn enforce_launcher_identity(
    State(controller): State<ConsoleController>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path().starts_with("/v1/") {
        return launcher_error_response(
            StatusCode::NOT_FOUND,
            "Top-level Agent routes are not exposed by the Launcher API".to_owned(),
        );
    }
    // Status is the one bootstrap endpoint. It exposes the pair generated for
    // this helper process; every Launcher control/read route must echo the
    // verified pair.
    if launcher_route_requires_identity(request.uri().path()) {
        if !launcher_capability_matches(request.headers(), &controller) {
            return launcher_error_response(
                StatusCode::FORBIDDEN,
                "Launcher API capability is missing or invalid".to_owned(),
            );
        }
        if !launcher_identity_matches(request.headers(), &controller) {
            return launcher_error_response(
                StatusCode::CONFLICT,
                "Launcher helper identity does not match this API process".to_owned(),
            );
        }
    }
    next.run(request).await
}

fn launcher_route_requires_identity(path: &str) -> bool {
    !matches!(path, "/launcher/status" | "/launcher/handshake")
}

async fn proxy_agent_request(
    State(controller): State<ConsoleController>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let path = match agent_proxy_target_path(request.uri().path()) {
        Some(path) => path,
        None => {
            return launcher_error_response(
                StatusCode::NOT_FOUND,
                "Agent proxy route is not available".to_owned(),
            )
        }
    };
    let body = match to_bytes(request.into_body(), MAX_AGENT_PROXY_BODY_BYTES).await {
        Ok(body) => body,
        Err(error) => return launcher_error_response(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let request = match verified_agent_request(
        &controller.client,
        controller.options.config.port,
        &controller.paths,
        method,
        path,
    )
    .await
    {
        Ok(request) => request,
        Err(error) => return launcher_error_response(StatusCode::BAD_GATEWAY, error),
    };
    let response = match request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return launcher_error_response(
                StatusCode::BAD_GATEWAY,
                format!("Agent request failed: {error}"),
            )
        }
    };
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AGENT_PROXY_RESPONSE_BYTES as u64)
    {
        return launcher_error_response(
            StatusCode::BAD_GATEWAY,
            "Agent response exceeded the Launcher proxy limit".to_owned(),
        );
    }
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= MAX_AGENT_PROXY_RESPONSE_BYTES => bytes,
        Ok(_) => {
            return launcher_error_response(
                StatusCode::BAD_GATEWAY,
                "Agent response exceeded the Launcher proxy limit".to_owned(),
            )
        }
        Err(error) => {
            return launcher_error_response(
                StatusCode::BAD_GATEWAY,
                format!("Agent response could not be read: {error}"),
            )
        }
    };
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(Body::from(bytes))
        .unwrap_or_else(|error| {
            launcher_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Launcher proxy response failed: {error}"),
            )
        })
}

fn agent_proxy_target_path(path: &str) -> Option<&'static str> {
    match path {
        "/launcher/agent-api/v1/health" => Some("/v1/health"),
        "/launcher/agent-api/v1/state" => Some("/v1/state"),
        "/launcher/agent-api/v1/harness" => Some("/v1/harness"),
        "/launcher/agent-api/v1/profiles" => Some("/v1/profiles"),
        "/launcher/agent-api/v1/checkpoints" => Some("/v1/checkpoints"),
        "/launcher/agent-api/v1/releases" => Some("/v1/releases"),
        "/launcher/agent-api/v1/updates" => Some("/v1/updates"),
        "/launcher/agent-api/v1/diagnostics" => Some("/v1/diagnostics"),
        "/launcher/agent-api/v1/config" => Some("/v1/config"),
        _ => None,
    }
}

fn launcher_error_response(status: StatusCode, message: String) -> Response {
    (status, Json(json!({"ok": false, "message": message}))).into_response()
}

async fn console_status(State(controller): State<ConsoleController>) -> Json<ConsoleStatus> {
    Json(controller.status().await)
}

async fn console_agent_status(State(controller): State<ConsoleController>) -> Json<ConsoleStatus> {
    Json(controller.status().await)
}

async fn console_agent_control(
    State(controller): State<ConsoleController>,
    Json(command): Json<ConsoleAgentCommand>,
) -> Response {
    let result = match command.action {
        ConsoleAgentAction::Start => controller.start_agent().await,
        ConsoleAgentAction::Stop => controller.stop_agent().await,
        ConsoleAgentAction::Restart => controller.restart_agent().await,
        ConsoleAgentAction::Status => Ok(controller.status().await),
    };
    match result {
        Ok(status) => (StatusCode::OK, Json(json!(status))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "message": error})),
        )
            .into_response(),
    }
}

async fn console_harness_status(
    State(controller): State<ConsoleController>,
) -> Json<HarnessUiInfo> {
    Json(controller.harness_ui().await)
}

async fn console_harness_control(
    State(controller): State<ConsoleController>,
    Json(command): Json<ConsoleHarnessCommand>,
) -> Response {
    let result = match command.action {
        ConsoleHarnessAction::Open => controller.open_harness().await,
        ConsoleHarnessAction::Status => Ok(controller.harness_ui().await),
    };
    match result {
        Ok(info) => (StatusCode::OK, Json(info)).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "message": error})),
        )
            .into_response(),
    }
}

async fn console_logs(State(controller): State<ConsoleController>) -> Json<Value> {
    let session = HarnessLogSessionStore::new(controller.paths.clone())
        .read()
        .ok()
        .flatten();
    Json(json!({
        "data_root": controller.paths.root.display().to_string(),
        "agent_stdout": controller.paths.logs_dir.join("agent.stdout.log"),
        "agent_stderr": controller.paths.logs_dir.join("agent.stderr.log"),
        "harness_stdout": session.as_ref().map(|value| controller.paths.logs_dir.join(&value.stdout_log_name)),
        "harness_stderr": session.as_ref().map(|value| controller.paths.logs_dir.join(&value.stderr_log_name)),
        "launcher_config": launcher_config_path(&controller.paths),
        "launch_record": launch_record_path(&controller.paths),
    }))
}

/// Return the newest loopback Harness URL from a bounded log tail.
///
/// Harness remains an opaque upstream process. The launcher only observes the
/// text it already redirected to its own logs; it does not inspect `$HOME/.dsh`
/// or infer a URL from a process command line. A URL is considered usable only
/// when it is plain HTTP and loopback-bound. Token-bearing URLs are preferred
/// because readiness/health messages may also contain a loopback URL. The
/// observer keeps only a bounded in-memory cursor. An unchanged byte snapshot
/// reuses its candidate; every content/length change rescans the bounded tail
/// so a rotation cannot masquerade as an append merely because its sliding
/// overlap happens to match.
#[cfg(test)]
fn read_harness_ui_info(paths: &NexusPaths) -> HarnessUiInfo {
    let mut observer = HarnessLogObserver::default();
    match HarnessLogSessionStore::new(paths.clone()).read() {
        Ok(Some(session)) => {
            read_harness_ui_info_with_observer(paths, &mut observer, Some(&session))
        }
        Ok(None) => unavailable_harness_ui_info(
            paths,
            "Harness log session marker is not available".to_owned(),
        ),
        Err(error) => unavailable_harness_ui_info(
            paths,
            format!("Harness log session marker is invalid: {error}"),
        ),
    }
}

fn read_harness_ui_info_with_observer(
    paths: &NexusPaths,
    observer: &mut HarnessLogObserver,
    session: Option<&HarnessLogSession>,
) -> HarnessUiInfo {
    observer.select_session(session);
    let Some(session) = session else {
        return unavailable_harness_ui_info(
            paths,
            "Harness log session marker is not available".to_owned(),
        );
    };
    let log_paths = harness_log_paths(paths, session);
    let boundaries = [
        (
            session.stdout_watermark,
            Some(session.stdout_file_identity.as_str()),
        ),
        (
            session.stderr_watermark,
            Some(session.stderr_file_identity.as_str()),
        ),
    ];
    let mut candidates = Vec::new();
    for (path, (watermark, file_identity)) in log_paths.into_iter().zip(boundaries) {
        let Ok(Some(candidate)) = observer.observe_file(&path, watermark, file_identity) else {
            continue;
        };
        if candidate.token.is_some() {
            candidates.push(candidate);
        }
    }

    candidates.sort_by(|left, right| {
        (
            left.token.is_some(),
            left.observed_at_nanos,
            left.offset,
            left.source.as_str(),
            left.sequence,
        )
            .cmp(&(
                right.token.is_some(),
                right.observed_at_nanos,
                right.offset,
                right.source.as_str(),
                right.sequence,
            ))
    });
    if let Some(candidate) = candidates.pop() {
        return HarnessUiInfo {
            available: true,
            generation: Some(session.generation),
            run_id: Some(session.run_id.clone()),
            url: Some(candidate.url),
            token: candidate.token,
            source: Some(candidate.source),
            observed_at_unix: Some(candidate.observed_at_unix),
            message: None,
        };
    }

    unavailable_harness_ui_info(
        paths,
        "Current Harness authentication token not found after this run's log boundary".to_owned(),
    )
}

fn unavailable_harness_ui_info(_paths: &NexusPaths, message: String) -> HarnessUiInfo {
    HarnessUiInfo {
        available: false,
        generation: None,
        run_id: None,
        url: None,
        token: None,
        source: None,
        observed_at_unix: None,
        message: Some(message),
    }
}

fn harness_observation_matches_session(
    response: &HarnessResponse,
    session: &HarnessLogSession,
) -> bool {
    response.log_session_run_id.as_deref() == Some(session.run_id.as_str())
        && response.generation == Some(session.generation)
        && response.log_session_generation == Some(session.generation)
        && response.log_stdout_watermark == Some(session.stdout_watermark)
        && response.log_stderr_watermark == Some(session.stderr_watermark)
        && response.log_stdout_file_identity.as_deref()
            == Some(session.stdout_file_identity.as_str())
        && response.log_stderr_file_identity.as_deref()
            == Some(session.stderr_file_identity.as_str())
        && response.log_stdout_name.as_deref() == Some(session.stdout_log_name.as_str())
        && response.log_stderr_name.as_deref() == Some(session.stderr_log_name.as_str())
        && response.log_session_launch_pending == Some(session.launch_pending)
}

#[derive(Debug, Default)]
struct HarnessLogObserver {
    files: HashMap<PathBuf, HarnessLogCursor>,
    sequence: u64,
    session: Option<(String, u64, u64, u64, String, String, String, String, bool)>,
    session_initialized: bool,
}

#[derive(Debug, Clone)]
struct HarnessLogCursor {
    offset: u64,
    fingerprint: u64,
    candidate: Option<HarnessUrlCandidate>,
}

#[derive(Debug)]
struct HarnessLogSnapshot {
    length: u64,
    start: u64,
    file_identity: String,
    modified_at_nanos: u128,
    fingerprint: u64,
    bytes: Vec<u8>,
    left_delimited: bool,
    right_delimited: bool,
}

#[derive(Debug, Clone)]
struct HarnessUrlCandidate {
    url: String,
    token: Option<String>,
    source: String,
    observed_at_unix: u64,
    observed_at_nanos: u128,
    offset: u64,
    sequence: u64,
}

impl HarnessLogObserver {
    fn invalidate(&mut self) {
        self.files.clear();
        self.session = None;
        self.session_initialized = false;
    }

    fn select_session(&mut self, session: Option<&HarnessLogSession>) {
        let selected = session.map(|session| {
            (
                session.run_id.clone(),
                session.generation,
                session.stdout_watermark,
                session.stderr_watermark,
                session.stdout_file_identity.clone(),
                session.stderr_file_identity.clone(),
                session.stdout_log_name.clone(),
                session.stderr_log_name.clone(),
                session.launch_pending,
            )
        });
        if !self.session_initialized || self.session != selected {
            self.files.clear();
            self.sequence = 0;
            self.session = selected;
            self.session_initialized = true;
        }
    }

    fn observe_file(
        &mut self,
        path: &Path,
        session_watermark: u64,
        expected_file_identity: Option<&str>,
    ) -> io::Result<Option<HarnessUrlCandidate>> {
        let snapshot = read_log_snapshot(path)?;
        if expected_file_identity.is_some_and(|expected| expected != snapshot.file_identity) {
            self.files.remove(path);
            return Ok(None);
        }
        let previous = self.files.get(path).cloned();
        if previous.as_ref().is_some_and(|previous| {
            snapshot.length == previous.offset && snapshot.fingerprint == previous.fingerprint
        }) {
            let previous = previous.expect("same-file cursor is present");
            self.files.insert(
                path.to_owned(),
                HarnessLogCursor {
                    offset: snapshot.length,
                    fingerprint: snapshot.fingerprint,
                    candidate: previous.candidate.clone(),
                },
            );
            return Ok(previous.candidate);
        }

        // A bounded overlap cannot prove that a file was appended: a rotated
        // file may preserve the overlap while replacing bytes before it. Parse
        // the complete current tail and discard candidates no longer present.
        // A session watermark belongs to the append-only file that existed
        // when the Agent started Harness. A shorter replacement cannot prove
        // where this run begins, so fail closed instead of treating its first
        // byte as current output and potentially reviving an old token.
        if snapshot.length < session_watermark {
            self.files.remove(path);
            return Ok(None);
        }
        let mut candidate = None;
        for (word_start, word_end) in log_word_ranges(&snapshot.bytes) {
            if (word_start == 0 && !snapshot.left_delimited)
                || (word_end == snapshot.bytes.len() && !snapshot.right_delimited)
            {
                continue;
            }
            let absolute_start = snapshot.start + word_start as u64;
            let absolute_end = snapshot.start + word_end as u64;
            // The word must begin at or after the durable EOF boundary and
            // end after it. This rejects a URL whose bytes straddle the
            // previous and current run while allowing output in a new file to
            // begin at offset zero.
            if absolute_start < session_watermark || absolute_end <= session_watermark {
                continue;
            }
            let Ok(word) = std::str::from_utf8(&snapshot.bytes[word_start..word_end]) else {
                continue;
            };
            let cleaned = trim_log_url(word);
            let Some((url, token)) = parse_loopback_harness_url(cleaned) else {
                continue;
            };
            let next = HarnessUrlCandidate {
                url,
                token,
                source: path.display().to_string(),
                observed_at_unix: (snapshot.modified_at_nanos / 1_000_000_000) as u64,
                observed_at_nanos: snapshot.modified_at_nanos,
                offset: absolute_end,
                sequence: self.sequence,
            };
            self.sequence = self.sequence.wrapping_add(1);
            if candidate.as_ref().map_or(true, |current| {
                harness_candidate_cmp(current, &next).is_lt()
            }) {
                candidate = Some(next);
            }
        }

        self.files.insert(
            path.to_owned(),
            HarnessLogCursor {
                offset: snapshot.length,
                fingerprint: snapshot.fingerprint,
                candidate: candidate.clone(),
            },
        );
        Ok(candidate)
    }
}

fn harness_log_paths(paths: &NexusPaths, session: &HarnessLogSession) -> [PathBuf; 2] {
    [
        paths.logs_dir.join(&session.stdout_log_name),
        paths.logs_dir.join(&session.stderr_log_name),
    ]
}

fn harness_candidate_cmp(
    left: &HarnessUrlCandidate,
    right: &HarnessUrlCandidate,
) -> std::cmp::Ordering {
    (
        left.token.is_some(),
        left.observed_at_nanos,
        left.offset,
        left.source.as_str(),
        left.sequence,
    )
        .cmp(&(
            right.token.is_some(),
            right.observed_at_nanos,
            right.offset,
            right.source.as_str(),
            right.sequence,
        ))
}

fn read_log_snapshot(path: &Path) -> io::Result<HarnessLogSnapshot> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let modified_at_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let length = metadata.len();
    let file_identity = log_file_identity(&file)?;
    let start = length.saturating_sub(HARNESS_LOG_TAIL_BYTES);
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = Vec::new();
    let expected_len = length.saturating_sub(read_start);
    (&mut file).take(expected_len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Harness log changed while its bounded snapshot was read",
        ));
    }
    let left_delimited = if start == 0 {
        true
    } else {
        let delimiter = bytes
            .first()
            .copied()
            .is_some_and(|byte| byte.is_ascii_whitespace());
        if !bytes.is_empty() {
            bytes.remove(0);
        }
        delimiter
    };
    let right_delimited = bytes
        .last()
        .copied()
        .is_none_or(|byte| byte.is_ascii_whitespace());
    let fingerprint = fingerprint_bytes(&bytes, length);
    Ok(HarnessLogSnapshot {
        length,
        start,
        file_identity,
        modified_at_nanos,
        fingerprint,
        bytes,
        left_delimited,
        right_delimited,
    })
}

fn fingerprint_bytes(bytes: &[u8], length: u64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    length.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn log_word_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_whitespace() {
            if let Some(start) = start.take() {
                ranges.push((start, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        ranges.push((start, bytes.len()));
    }
    ranges
}

fn trim_log_url(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character.is_control()
            || matches!(
                character,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '.'
            )
    })
}

fn parse_loopback_harness_url(raw: &str) -> Option<(String, Option<String>)> {
    if raw.is_empty() {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "http"
        || url.username() != ""
        || url.password().is_some()
        || !is_loopback_host(url.host_str()?)
        || url.port_or_known_default().is_none()
        || url.as_str().chars().any(char::is_control)
    {
        return None;
    }
    if url
        .query_pairs()
        .any(|(key, value)| key.is_empty() || value.is_empty())
    {
        return None;
    }
    if let Some(fragment) = url.fragment() {
        if fragment.is_empty()
            || fragment.split('&').any(|part| {
                let Some((key, value)) = part.split_once('=') else {
                    return true;
                };
                key.is_empty() || value.is_empty()
            })
        {
            return None;
        }
    }
    let token = url
        .query_pairs()
        .find_map(|(key, value)| is_token_key(&key).then(|| value.into_owned()))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            url.fragment().and_then(|fragment| {
                fragment.split('&').find_map(|part| {
                    let (key, value) = part.split_once('=')?;
                    is_token_key(key).then(|| value.to_owned())
                })
            })
        });
    Some((url.as_str().to_owned(), token))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn is_token_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "token" | "access_token" | "auth_token" | "session_token" | "authorization"
    )
}

async fn ensure_agent_started(
    options: &Options,
    paths: &NexusPaths,
    client: &Client,
) -> Result<EnsureResult, String> {
    if is_agent_healthy(client, options.config.port, paths).await? {
        return Ok(EnsureResult::AlreadyRunning);
    }

    let lock = acquire_lock(paths).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            format!(
                "another launcher is starting the Agent or holds {}; retry after it finishes",
                lock_path(paths).display()
            )
        } else {
            format!("cannot acquire Agent instance lock: {error}")
        }
    })?;

    if is_agent_healthy(client, options.config.port, paths).await? {
        return Ok(EnsureResult::AlreadyRunning);
    }

    ensure_runtime_lock_available(paths).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            "another Nexus Agent already owns this data root, possibly on a different port"
                .to_owned()
        } else {
            format!("cannot inspect Agent runtime lock: {error}")
        }
    })?;
    let agent_program = resolve_agent_program(options.agent_program.as_deref())?;
    let expected_instance_id = new_instance_id();
    let detached = should_detach_agent(options.command);
    let mut spawned = spawn_agent(
        &agent_program,
        &options.config,
        paths,
        &expected_instance_id,
        detached,
    )
    .map_err(|error| format!("cannot start Agent: {error}"))?;
    let pid = spawned.pid;
    if let Err(error) = write_launch_record(
        paths,
        &LaunchRecord {
            pid,
            port: options.config.port,
            started_at_unix: unix_time_seconds(),
            agent_program: agent_program.to_string_lossy().into_owned(),
            agent_instance_id: Some(expected_instance_id.clone()),
        },
    ) {
        let cleanup = terminate_spawned_agent(&mut spawned).await;
        return Err(cleanup_error(
            format!("cannot persist Agent launch record: {error}"),
            cleanup,
        ));
    }

    // Use a fresh HTTP client after process creation. A client that attempted
    // a connection before the listener existed can retain a Windows TCP
    // refusal while another local WebShell is polling the same port.
    let startup_client = match Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(6))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let cleanup = terminate_spawned_agent(&mut spawned).await;
            remove_launch_record(paths);
            return Err(cleanup_error(
                format!("cannot initialize Agent health client: {error}"),
                cleanup,
            ));
        }
    };
    let health = match wait_for_health(
        &startup_client,
        options.config.port,
        options.wait_secs,
        paths,
        Some(&expected_instance_id),
    )
    .await
    {
        Ok(health) => health,
        Err(error) => {
            let cleanup = terminate_spawned_agent(&mut spawned).await;
            remove_launch_record(paths);
            return Err(cleanup_error(error, cleanup));
        }
    };
    if let Err(error) = write_launch_record(
        paths,
        &LaunchRecord {
            pid,
            port: options.config.port,
            started_at_unix: unix_time_seconds(),
            agent_program: agent_program.to_string_lossy().into_owned(),
            agent_instance_id: Some(health.instance_id),
        },
    ) {
        let cleanup = terminate_spawned_agent(&mut spawned).await;
        remove_launch_record(paths);
        return Err(cleanup_error(
            format!("cannot persist Agent instance identity: {error}"),
            cleanup,
        ));
    }

    // Exact health proves that the spawned Agent has acquired its own
    // lifetime runtime lock. The bootstrap lock must not remain coupled to a
    // foreground Launcher's lifetime.
    drop(lock);
    #[cfg(windows)]
    drop(spawned.process_handle.take());
    Ok(EnsureResult::Owned(OwnedAgent {
        child: spawned.child.take(),
        pid,
    }))
}

fn should_detach_agent(command: LauncherCommand) -> bool {
    matches!(
        command,
        LauncherCommand::Start | LauncherCommand::Api | LauncherCommand::Console
    )
}

fn acquire_lock(paths: &NexusPaths) -> io::Result<InstanceLock> {
    let path = lock_path(paths);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;
    file.try_lock()?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "pid={}", process::id())?;
    file.sync_all()?;
    Ok(InstanceLock { _file: file })
}

fn lock_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("agent-bootstrap.lock")
}

fn ensure_runtime_lock_available(paths: &NexusPaths) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(paths.run_dir.join("agent.lock"))?;
    file.try_lock()?;
    drop(file);
    Ok(())
}

fn launch_record_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("agent.json")
}

#[cfg(not(windows))]
fn spawn_agent(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    instance_id: &str,
    detached: bool,
) -> io::Result<SpawnedAgent> {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stdout.log"))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stderr.log"))?;
    let mut command = TokioCommand::new(agent_program);
    command
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--instance-id")
        .arg(instance_id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_agent_process_group(&mut command, detached);
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("Agent process did not expose a PID"))?;
    Ok(SpawnedAgent {
        child: Some(child),
        pid,
    })
}

#[cfg(not(windows))]
fn configure_agent_process_group(command: &mut TokioCommand, detached: bool) {
    #[cfg(unix)]
    if detached {
        // `start` and the headless API transfer Agent lifetime to the Agent's
        // data-root runtime lock. Put that process in its own group before
        // spawn so a terminal SIGINT delivered to the Launcher's foreground
        // group cannot contradict that ownership contract.
        command.process_group(0);
    }

    #[cfg(not(unix))]
    let _ = (command, detached);
}

#[cfg(windows)]
fn spawn_agent(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    instance_id: &str,
    detached: bool,
) -> io::Result<SpawnedAgent> {
    if !detached {
        return spawn_agent_direct(agent_program, config, paths, instance_id);
    }

    let (process_handle, pid) =
        spawn_agent_windows_detached(agent_program, config, paths, instance_id)?;
    Ok(SpawnedAgent {
        child: None,
        pid,
        process_handle: Some(process_handle),
    })
}

#[cfg(windows)]
fn spawn_agent_direct(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    instance_id: &str,
) -> io::Result<SpawnedAgent> {
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stdout.log"))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("agent.stderr.log"))?;
    let mut command = TokioCommand::new(agent_program);
    command
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--instance-id")
        .arg(instance_id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.kill_on_drop(true);
    command.creation_flags(0x0900_0200);
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("Agent process did not expose a PID"))?;
    Ok(SpawnedAgent {
        child: Some(child),
        pid,
        process_handle: None,
    })
}

#[cfg(windows)]
fn spawn_agent_windows_detached(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    instance_id: &str,
) -> io::Result<(std::os::windows::io::OwnedHandle, u32)> {
    use std::{
        ffi::{OsStr, OsString},
        mem::size_of,
        os::windows::ffi::OsStrExt,
        os::windows::io::FromRawHandle,
        ptr::{null, null_mut},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
        },
        System::Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
            UpdateProcThreadAttribute, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP,
            CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        },
    };

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn quote(value: &OsStr) -> Vec<u16> {
        let units: Vec<u16> = value.encode_wide().collect();
        let needs_quotes = units.is_empty()
            || units
                .iter()
                .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
        if !needs_quotes {
            return units;
        }
        let mut result = Vec::with_capacity(units.len() + 2);
        result.push(b'"' as u16);
        let mut backslashes = 0usize;
        for unit in units {
            if unit == b'\\' as u16 {
                backslashes += 1;
            } else if unit == b'"' as u16 {
                result.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
                result.push(unit);
                backslashes = 0;
            } else {
                result.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                result.push(unit);
                backslashes = 0;
            }
        }
        result.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        result.push(b'"' as u16);
        result
    }

    fn open_output(path: &Path, security: &SECURITY_ATTRIBUTES) -> io::Result<HANDLE> {
        let path = wide(path.as_os_str());
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                security,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let stdout = open_output(&paths.logs_dir.join("agent.stdout.log"), &security)?;
    let stderr = match open_output(&paths.logs_dir.join("agent.stderr.log"), &security) {
        Ok(handle) => handle,
        Err(error) => {
            unsafe { CloseHandle(stdout) };
            return Err(error);
        }
    };
    let nul_name = wide(OsStr::new("NUL"));
    let stdin = unsafe {
        CreateFileW(
            nul_name.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &security,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if stdin == INVALID_HANDLE_VALUE {
        unsafe {
            CloseHandle(stdout);
            CloseHandle(stderr);
        }
        return Err(io::Error::last_os_error());
    }

    let arguments: Vec<OsString> = vec![
        agent_program.as_os_str().to_owned(),
        OsString::from("--data-dir"),
        paths.root.as_os_str().to_owned(),
        OsString::from("--port"),
        OsString::from(config.port.to_string()),
        OsString::from("--instance-id"),
        OsString::from(instance_id),
    ];
    let mut command_line: Vec<u16> = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            command_line.push(b' ' as u16);
        }
        command_line.extend(quote(argument));
    }
    command_line.push(0);

    let mut attribute_size = 0usize;
    unsafe {
        let _ = InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size);
    }
    if attribute_size == 0 {
        unsafe {
            CloseHandle(stdin);
            CloseHandle(stdout);
            CloseHandle(stderr);
        }
        return Err(io::Error::last_os_error());
    }
    let words = (attribute_size + size_of::<usize>() - 1) / size_of::<usize>();
    let mut attribute_storage = vec![0usize; words];
    let attribute_list = attribute_storage.as_mut_ptr() as *mut core::ffi::c_void;
    let initialized =
        unsafe { InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) };
    if initialized == 0 {
        unsafe {
            CloseHandle(stdin);
            CloseHandle(stdout);
            CloseHandle(stderr);
        }
        return Err(io::Error::last_os_error());
    }

    let handles = [stdin, stdout, stderr];
    let updated = unsafe {
        UpdateProcThreadAttribute(
            attribute_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of::<HANDLE>() * handles.len(),
            null_mut(),
            null(),
        )
    };
    if updated == 0 {
        unsafe {
            DeleteProcThreadAttributeList(attribute_list);
            CloseHandle(stdin);
            CloseHandle(stdout);
            CloseHandle(stderr);
        }
        return Err(io::Error::last_os_error());
    }

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin;
    startup.StartupInfo.hStdOutput = stdout;
    startup.StartupInfo.hStdError = stderr;
    startup.lpAttributeList = attribute_list;
    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_BREAKAWAY_FROM_JOB
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_NO_WINDOW;
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            null(),
            null(),
            &startup.StartupInfo,
            &mut process_info,
        )
    };
    let error = if created == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        DeleteProcThreadAttributeList(attribute_list);
        CloseHandle(stdin);
        CloseHandle(stdout);
        CloseHandle(stderr);
    }
    if let Some(error) = error {
        return Err(error);
    }
    unsafe {
        CloseHandle(process_info.hThread);
    }
    let process_handle =
        unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(process_info.hProcess) };
    Ok((process_handle, process_info.dwProcessId))
}

fn cleanup_error(message: String, cleanup: io::Result<()>) -> String {
    match cleanup {
        Ok(()) => message,
        Err(error) => format!("{message}; spawned Agent handle cleanup failed: {error}"),
    }
}

async fn terminate_spawned_agent(spawned: &mut SpawnedAgent) -> io::Result<()> {
    if let Some(mut child) = spawned.child.take() {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None => {
                child.kill().await?;
                child.wait().await?;
                return Ok(());
            }
        }
    }
    #[cfg(windows)]
    if let Some(handle) = spawned.process_handle.take() {
        return tokio::task::spawn_blocking(move || terminate_windows_process(handle))
            .await
            .map_err(|error| io::Error::other(format!("process cleanup task failed: {error}")))?;
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_windows_process(handle: std::os::windows::io::OwnedHandle) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0},
        System::Threading::{TerminateProcess, WaitForSingleObject},
    };

    let handle = handle.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => return Ok(()),
        WAIT_FAILED => return Err(io::Error::last_os_error()),
        _ => {}
    }
    if unsafe { TerminateProcess(handle, 1) } == 0 {
        // The process can exit naturally between the zero-time probe and the
        // termination call. Accept only an observed signal on the same owned
        // handle; never fall back to a reusable PID.
        if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        return Err(io::Error::last_os_error());
    }
    match unsafe { WaitForSingleObject(handle, 5_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for the spawned Agent process handle",
        )),
    }
}

fn resolve_agent_program(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Some(path) = env::var_os(AGENT_BINARY_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate launcher executable: {error}"))?;
    let parent = executable
        .parent()
        .ok_or_else(|| "launcher executable has no parent directory".to_owned())?;
    let candidate = if cfg!(windows) {
        parent.join("nexus-agent.exe")
    } else {
        parent.join("nexus-agent")
    };
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(format!(
            "nexus-agent was not found beside the launcher at {}; pass --agent PATH or set {AGENT_BINARY_ENV}",
            candidate.display()
        ))
    }
}

async fn is_agent_healthy(client: &Client, port: u16, paths: &NexusPaths) -> Result<bool, String> {
    Ok(matches!(
        probe_health(client, port, paths).await?,
        Some(response) if response.status == HealthStatus::Ok
    ))
}

async fn probe_health(
    client: &Client,
    port: u16,
    paths: &NexusPaths,
) -> Result<Option<HealthResponse>, String> {
    let response = match client
        .get(format!("http://127.0.0.1:{port}/v1/health"))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    let health = response
        .json::<HealthResponse>()
        .await
        .map_err(|error| format!("invalid Agent health response: {error}"))?;
    let expected = data_root_identity(paths)
        .map_err(|error| format!("cannot identify Nexus data root: {error}"))?;
    if health.api_version != nexus_protocol::API_VERSION
        || health.service != "nexus-agent"
        || health.data_root_id != expected
        || health.instance_id.is_empty()
    {
        return Err(format!(
            "port {port} is occupied by a Nexus Agent for a different data root or instance contract"
        ));
    }
    Ok(Some(health))
}

async fn verified_agent_request(
    client: &Client,
    port: u16,
    paths: &NexusPaths,
    method: Method,
    path: &str,
) -> Result<reqwest::RequestBuilder, String> {
    let health = probe_health(client, port, paths)
        .await?
        .filter(|health| health.status == HealthStatus::Ok)
        .ok_or_else(|| format!("Nexus Agent is not healthy on port {port}"))?;
    Ok(client
        .request(method, format!("http://127.0.0.1:{port}{path}"))
        .header(PROXY_DATA_ROOT_HEADER, health.data_root_id)
        .header(PROXY_INSTANCE_HEADER, health.instance_id))
}

async fn wait_for_health(
    client: &Client,
    port: u16,
    wait_secs: u64,
    paths: &NexusPaths,
    expected_instance_id: Option<&str>,
) -> Result<HealthResponse, String> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut last_error = None;
    loop {
        match probe_health(client, port, paths).await {
            Ok(Some(response))
                if response.status == HealthStatus::Ok
                    && expected_instance_id
                        .is_none_or(|expected| response.instance_id == expected) =>
            {
                return Ok(response)
            }
            Ok(Some(response)) if response.status == HealthStatus::Ok => {
                return Err(format!(
                    "port {port} answered with Agent instance {} instead of the spawned instance",
                    response.instance_id
                ))
            }
            Ok(_) => {}
            Err(error) if error.contains("is occupied by a Nexus Agent") => return Err(error),
            Err(error) => last_error = Some(error),
        }
        if Instant::now() >= deadline {
            return Err(match last_error {
                Some(error) => format!(
                    "Agent did not become healthy within {wait_secs} seconds (last probe error: {error})"
                ),
                None => format!("Agent did not become healthy within {wait_secs} seconds"),
            });
        }
        sleep(Duration::from_millis(150)).await;
    }
}

async fn stop_agent(options: &Options, paths: &NexusPaths, client: &Client) -> Result<(), String> {
    let health = probe_health(client, options.config.port, paths).await?;
    if health.is_none() {
        remove_launch_record(paths);
        print_stop(options, false, paths);
        return Ok(());
    }

    let response = verified_agent_request(
        client,
        options.config.port,
        paths,
        Method::POST,
        "/v1/shutdown",
    )
    .await?
    .json(&serde_json::json!({}))
    .send()
    .await
    .map_err(|error| format!("cannot request Agent shutdown: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Agent rejected shutdown with HTTP {}",
            response.status()
        ));
    }

    let deadline = Instant::now() + Duration::from_secs(DEFAULT_STOP_WAIT_SECS);
    loop {
        if probe_health(client, options.config.port, paths)
            .await?
            .is_none()
        {
            remove_launch_record(paths);
            print_stop(options, true, paths);
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Agent did not exit within {DEFAULT_STOP_WAIT_SECS} seconds; launch metadata was retained"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn run_foreground(
    mut owned: OwnedAgent,
    options: &Options,
    paths: &NexusPaths,
    client: &Client,
) -> Result<(), String> {
    let mut child = owned
        .child
        .take()
        .ok_or_else(|| "foreground Agent start did not return a child handle".to_owned())?;
    print_started(options, Some(owned.pid), paths, false);
    let mut down_polls = 0u8;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.map_err(|error| format!("cannot listen for Ctrl+C: {error}"))?;
                request_shutdown(client, options.config.port, paths).await?;
                let deadline = Instant::now() + Duration::from_secs(DEFAULT_STOP_WAIT_SECS);
                loop {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.start_kill();
                        break;
                    }
                    sleep(Duration::from_millis(200)).await;
                }
                remove_launch_record(paths);
                println!("agent stopped");
                return Ok(());
            }
            _ = sleep(Duration::from_secs(1)) => {
                if let Ok(Some(status)) = child.try_wait() {
                    remove_launch_record(paths);
                    println!("agent exited: {status}");
                    return Ok(());
                }

                // `stop` removes the launch metadata after the loopback
                // listener is gone. Observe that local signal first so a
                // foreground launcher is not held hostage by a stale TCP
                // connection while the Agent is already shutting down.
                if !launch_record_path(paths).exists() {
                    if let Ok(Some(status)) = child.try_wait() {
                        println!("agent exited: {status}");
                    } else {
                        let _ = child.start_kill();
                        println!("agent stopped");
                    }
                    remove_launch_record(paths);
                    return Ok(());
                }
                match probe_health(client, options.config.port, paths).await {
                    Ok(Some(_)) | Err(_) => down_polls = 0,
                    Ok(None) => {
                        down_polls = down_polls.saturating_add(1);
                        if down_polls < 3 {
                            continue;
                        }
                        if let Ok(Some(status)) = child.try_wait() {
                            remove_launch_record(paths);
                            println!("agent exited: {status}");
                            return Ok(());
                        }

                        // Once the owned Agent has kept its listener down for
                        // several consecutive polls, do not wait forever for
                        // a Windows process notification that may be missed.
                        // Terminate only the exact child handle and return.
                        let _ = child.start_kill();
                        remove_launch_record(paths);
                        println!("agent stopped");
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn request_shutdown(client: &Client, port: u16, paths: &NexusPaths) -> Result<(), String> {
    let Some(_) = probe_health(client, port, paths).await? else {
        return Ok(());
    };
    let response = verified_agent_request(client, port, paths, Method::POST, "/v1/shutdown")
        .await?
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|error| format!("cannot request Agent shutdown: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "Agent rejected shutdown with HTTP {}",
            response.status()
        ))
    }
}

async fn status_agent(
    options: &Options,
    paths: &NexusPaths,
    client: &Client,
) -> Result<(), String> {
    let health = probe_health(client, options.config.port, paths).await?;
    let record = read_launch_record(paths).filter(|record| {
        health.as_ref().is_some_and(|health| {
            record.agent_instance_id.as_deref() == Some(health.instance_id.as_str())
        })
    });
    let state = if health.is_some() {
        verified_agent_request(client, options.config.port, paths, Method::GET, "/v1/state")
            .await?
            .send()
            .await
            .map_err(|error| format!("cannot read Agent state: {error}"))?
            .json::<StateResponse>()
            .await
            .map_err(|error| format!("invalid Agent state response: {error}"))?
    } else {
        StateResponse {
            api_version: nexus_protocol::API_VERSION.to_owned(),
            state: nexus_protocol::AgentStatePayload {
                lifecycle: nexus_protocol::AgentLifecycleState::Stopped,
                harness: nexus_protocol::HarnessState::Detached,
                profile: None,
                release: None,
                started_at_unix: 0,
                updated_at_unix: 0,
            },
        }
    };

    if options.json {
        let value = serde_json::json!({
            "api_version": nexus_protocol::API_VERSION,
            "agent": if health.is_some() { "running" } else { "stopped" },
            "port": options.config.port,
            "data_root": paths.root.display().to_string(),
            "record": record,
            "state": state.state,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| format!("failed to encode JSON: {error}"))?
        );
    } else {
        println!(
            "agent: {}",
            if health.is_some() {
                "running"
            } else {
                "stopped"
            }
        );
        println!("data_root: {}", paths.root.display());
        println!("port: {}", options.config.port);
        if let Some(record) = record {
            println!("pid: {}", record.pid);
        } else {
            println!("pid: <none>");
        }
        println!("lifecycle: {:?}", state.state.lifecycle);
        println!("harness: {:?}", state.state.harness);
        println!(
            "profile: {}",
            state.state.profile.as_deref().unwrap_or("<none>")
        );
        println!(
            "release: {}",
            state.state.release.as_deref().unwrap_or("<none>")
        );
    }
    Ok(())
}

fn print_started(options: &Options, pid: Option<u32>, paths: &NexusPaths, already_running: bool) {
    if options.json {
        let value = serde_json::json!({
            "api_version": nexus_protocol::API_VERSION,
            "status": if already_running { "already_running" } else { "started" },
            "pid": pid,
            "port": options.config.port,
            "data_root": paths.root.display().to_string(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("launcher JSON encodes")
        );
    } else if already_running {
        println!("agent already running on 127.0.0.1:{}", options.config.port);
    } else {
        println!(
            "agent started: pid={} address=127.0.0.1:{} data_root={}",
            pid.map_or_else(|| "<none>".to_owned(), |pid| pid.to_string()),
            options.config.port,
            paths.root.display()
        );
    }
}

fn print_stop(options: &Options, stopped: bool, paths: &NexusPaths) {
    if options.json {
        let value = serde_json::json!({
            "api_version": nexus_protocol::API_VERSION,
            "status": if stopped { "stopped" } else { "not_running" },
            "data_root": paths.root.display().to_string(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("launcher JSON encodes")
        );
    } else if stopped {
        println!("agent stopped");
    } else {
        println!("agent not running");
    }
}

fn print_logs(options: &Options, paths: &NexusPaths) {
    let session = HarnessLogSessionStore::new(paths.clone())
        .read()
        .ok()
        .flatten();
    let harness_stdout = session
        .as_ref()
        .map(|value| paths.logs_dir.join(&value.stdout_log_name));
    let harness_stderr = session
        .as_ref()
        .map(|value| paths.logs_dir.join(&value.stderr_log_name));
    if options.json {
        let value = serde_json::json!({
            "api_version": nexus_protocol::API_VERSION,
            "data_root": paths.root.display().to_string(),
            "agent_stdout": paths.logs_dir.join("agent.stdout.log"),
            "agent_stderr": paths.logs_dir.join("agent.stderr.log"),
            "harness_stdout": harness_stdout,
            "harness_stderr": harness_stderr,
            "launcher_config": launcher_config_path(paths),
            "launch_record": launch_record_path(paths),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("launcher JSON encodes")
        );
    } else {
        println!("data_root: {}", paths.root.display());
        println!(
            "agent_stdout: {}",
            paths.logs_dir.join("agent.stdout.log").display()
        );
        println!(
            "agent_stderr: {}",
            paths.logs_dir.join("agent.stderr.log").display()
        );
        println!(
            "harness_stdout: {}",
            harness_stdout.as_ref().map_or_else(
                || "<unavailable>".to_owned(),
                |path| path.display().to_string()
            )
        );
        println!(
            "harness_stderr: {}",
            harness_stderr.as_ref().map_or_else(
                || "<unavailable>".to_owned(),
                |path| path.display().to_string()
            )
        );
        println!("launcher_config: {}", launcher_config_path(paths).display());
        println!("launch_record: {}", launch_record_path(paths).display());
    }
}

fn write_launch_record(paths: &NexusPaths, record: &LaunchRecord) -> io::Result<()> {
    let path = launch_record_path(paths);
    let temporary = paths.run_dir.join(format!(
        ".agent.json.tmp-{}-{}",
        process::id(),
        unix_time_nanos()
    ));
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace_launch_record(&temporary, &path)?;
        #[cfg(unix)]
        fs::File::open(&paths.run_dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn atomic_replace_launch_record(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn atomic_replace_launch_record(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace_launch_record(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

fn read_launch_record(paths: &NexusPaths) -> Option<LaunchRecord> {
    let bytes = fs::read(launch_record_path(paths)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove_launch_record(paths: &NexusPaths) {
    let _ = fs::remove_file(launch_record_path(paths));
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_time_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn print_help() {
    println!(
        r#"nexus-launcher

Usage:
  nexus-launcher start [--data-dir PATH] [--port PORT] [--agent PATH] [--wait-secs SECONDS] [--json]
  nexus-launcher run|foreground [--data-dir PATH] [--port PORT] [--agent PATH] [--wait-secs SECONDS]
  nexus-launcher api [--data-dir PATH] [--port PORT] [--agent PATH] [--console-port PORT] [--wait-secs SECONDS] [--launcher-instance-id ID]
  nexus-launcher console [same options as api; compatibility alias, no HTML]
  nexus-launcher stop [--data-dir PATH] [--port PORT] [--json]
  nexus-launcher status [--data-dir PATH] [--port PORT] [--json]
  nexus-launcher logs [--data-dir PATH] [--json]

With no command, the launcher enters `api`. The headless Launcher API starts or
reconnects to the loopback Agent, starts a configured Harness, binds the
configured loopback API port, and supervises Agent availability. It never
serves HTML or opens a browser. The native Tauri shell can open the latest
loopback Harness authentication URL observed in the bounded Harness log tail
and display its token without reading `$HOME/.dsh` or changing Harness source.
`start`/`run`/`stop`/`status`/`logs` remain script and recovery fallbacks. The
Agent remains the owner of Harness, profile, checkpoint, release, update, and
diagnostic business behavior.

Launcher configuration is read from `<data-root>/launcher.json`. Precedence is
CLI > launcher.json > environment > built-in defaults. `--data-dir` selects
the data root before that file is loaded, so it is intentionally not a field
inside launcher.json. The file is separate from the Agent-owned `config.json`.

Environment: NEXUS_DATA_DIR, NEXUS_AGENT_PORT, NEXUS_AGENT_BIN,
NEXUS_CONSOLE_PORT, NEXUS_LAUNCHER_WAIT_SECS. Legacy console directory/browser
options are accepted for script compatibility but ignored. `GET /launcher/harness`
returns the latest safe Harness URL/token;
`POST /launcher/harness` with `{{"action":"open"}}` opens it in the system
browser.

"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_protocol::HarnessRuntimeInfo;

    fn test_harness_log_session(
        paths: &NexusPaths,
        run_id: &str,
        generation: u64,
        stdout_watermark: u64,
        stderr_watermark: u64,
    ) -> HarnessLogSession {
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.logs_dir.join("harness.stdout.log"))
            .expect("Harness stdout opens");
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.logs_dir.join("harness.stderr.log"))
            .expect("Harness stderr opens");
        HarnessLogSession::new(
            run_id.to_owned(),
            generation,
            stdout_watermark,
            stderr_watermark,
            log_file_identity(&stdout).expect("Harness stdout identity reads"),
            log_file_identity(&stderr).expect("Harness stderr identity reads"),
            "harness.stdout.log".to_owned(),
            "harness.stderr.log".to_owned(),
            true,
            unix_time_seconds(),
        )
    }

    #[test]
    fn default_agent_path_is_sibling_name_for_current_platform() {
        let name = if cfg!(windows) {
            "nexus-agent.exe"
        } else {
            "nexus-agent"
        };
        assert!(!name.is_empty());
    }

    #[test]
    fn background_launcher_modes_detach_agent_but_foreground_run_does_not() {
        assert!(should_detach_agent(LauncherCommand::Start));
        assert!(should_detach_agent(LauncherCommand::Api));
        assert!(should_detach_agent(LauncherCommand::Console));
        assert!(!should_detach_agent(LauncherCommand::Run));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_agent_spawn_gets_an_independent_process_group() {
        let mut command = TokioCommand::new("sh");
        command.args([
            "-c",
            "pid=$$; pgid=$(ps -o pgid= -p $$ | tr -d ' '); test \"$pid\" = \"$pgid\"",
        ]);
        configure_agent_process_group(&mut command, true);
        let status = command
            .status()
            .await
            .expect("detached process-group probe runs");
        assert!(
            status.success(),
            "the detached child PID must be its process-group ID"
        );
    }

    #[test]
    fn launch_record_is_json_safe_and_round_trips() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-test-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let record = LaunchRecord {
            pid: 42,
            port: nexus_core::DEFAULT_AGENT_PORT,
            started_at_unix: 10,
            agent_program: "nexus-agent".to_owned(),
            agent_instance_id: Some("instance-42".to_owned()),
        };
        write_launch_record(&paths, &record).expect("record writes");
        let replacement = LaunchRecord {
            pid: 43,
            agent_instance_id: Some("instance-43".to_owned()),
            ..record
        };
        write_launch_record(&paths, &replacement).expect("record atomically replaces");
        assert_eq!(read_launch_record(&paths), Some(replacement));
        assert!(
            fs::read_dir(&paths.run_dir)
                .expect("run directory reads")
                .all(|entry| !entry
                    .expect("directory entry reads")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".agent.json.tmp-")),
            "an atomic launch-record publication must not leave a temporary file"
        );
        remove_launch_record(&paths);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn instance_lock_is_os_owned_and_cannot_be_stale_deleted() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-lock-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let first = acquire_lock(&paths).expect("first launcher acquires lock");
        let second_error = match acquire_lock(&paths) {
            Ok(_) => panic!("second launcher must not acquire an OS-owned lock"),
            Err(error) => error,
        };
        assert_eq!(second_error.kind(), io::ErrorKind::WouldBlock);
        assert!(lock_path(&paths).exists(), "lock file remains while owned");
        drop(first);
        let recovered = acquire_lock(&paths).expect("released OS lock can be acquired");
        assert!(
            lock_path(&paths).exists(),
            "persistent lock inode is reused instead of stale-delete/create"
        );
        drop(recovered);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn startup_failure_cleanup_terminates_the_owned_process_handle() {
        use std::{
            os::windows::{io::AsRawHandle, io::FromRawHandle, process::CommandExt},
            process::Command,
        };
        use windows_sys::Win32::{
            Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE},
            System::Threading::{GetCurrentProcess, CREATE_NO_WINDOW},
        };

        let mut child = Command::new("cmd.exe")
            .args(["/C", "ping 127.0.0.1 -n 30 >NUL"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("test process spawns");
        let mut duplicate: HANDLE = std::ptr::null_mut();
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                child.as_raw_handle() as HANDLE,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        assert_ne!(duplicated, 0, "process handle duplicates");
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(duplicate) };
        if let Err(error) = terminate_windows_process(handle) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("owned-handle cleanup failed: {error}");
        }
        let status = child.wait().expect("terminated process reaps");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn health_probe_rejects_agent_for_a_different_data_root() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-health-root-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let expected = NexusPaths::from_root(root.join("expected"));
        let foreign = NexusPaths::from_root(root.join("foreign"));
        expected
            .ensure_directories()
            .expect("expected root creates");
        foreign.ensure_directories().expect("foreign root creates");
        let response = HealthResponse::healthy(
            data_root_identity(&foreign).expect("foreign root canonicalizes"),
            "foreign-instance".to_owned(),
        );
        let app = Router::new().route(
            "/v1/health",
            get(move || {
                let response = response.clone();
                async move { Json(response) }
            }),
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("foreign Agent binds");
        let port = listener.local_addr().expect("foreign address").port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("foreign Agent serves")
        });
        let client = Client::new();
        let error = probe_health(&client, port, &expected)
            .await
            .expect_err("foreign data root is never adopted");
        assert!(error.contains("different data root"));
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn verified_agent_request_binds_the_probed_root_and_instance() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-verified-proxy-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("root creates");
        let data_root_id = data_root_identity(&paths).expect("root identity reads");
        let instance_id = "expected-instance".to_owned();
        let health = HealthResponse::healthy(data_root_id.clone(), instance_id.clone());
        let expected_root = data_root_id.clone();
        let expected_instance = instance_id.clone();
        let app = Router::new()
            .route(
                "/v1/health",
                get(move || {
                    let health = health.clone();
                    async move { Json(health) }
                }),
            )
            .route(
                "/v1/state",
                get(move |headers: axum::http::HeaderMap| {
                    let expected_root = expected_root.clone();
                    let expected_instance = expected_instance.clone();
                    async move {
                        if headers
                            .get(PROXY_DATA_ROOT_HEADER)
                            .and_then(|value| value.to_str().ok())
                            == Some(expected_root.as_str())
                            && headers
                                .get(PROXY_INSTANCE_HEADER)
                                .and_then(|value| value.to_str().ok())
                                == Some(expected_instance.as_str())
                        {
                            StatusCode::OK
                        } else {
                            StatusCode::CONFLICT
                        }
                    }
                }),
            );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("Agent mock binds");
        let port = listener.local_addr().expect("Agent mock address").port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serves") });
        let client = Client::new();
        let response = verified_agent_request(&client, port, &paths, Method::GET, "/v1/state")
            .await
            .expect("identity-bound request builds")
            .send()
            .await
            .expect("identity-bound request sends");
        assert_eq!(response.status(), StatusCode::OK);
        let error = wait_for_health(&client, port, 1, &paths, Some("different-instance"))
            .await
            .expect_err("startup nonce mismatch is rejected");
        assert!(error.contains("instead of the spawned instance"));
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parser_requires_a_known_command() {
        // Keep this test independent of the process command line. The parser
        // is exercised by the release smoke and --help output is intentionally
        // stable for scripts.
        assert_eq!(DEFAULT_WAIT_SECS, 20);
        assert_eq!(DEFAULT_STOP_WAIT_SECS, 15);
    }

    #[test]
    fn parser_preserves_the_native_helper_instance_nonce() {
        let options =
            parse_args_from(["api", "--launcher-instance-id", "native-helper-instance-a"])
                .expect("arguments parse")
                .expect("options remain");
        assert_eq!(options.launcher_instance_id, "native-helper-instance-a");
        assert!(parse_args_from(["api", "--launcher-instance-id", ""]).is_err());
    }

    #[test]
    fn console_command_accepts_console_directory_and_disable_open() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-parser-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let options = parse_args_from([
            "console",
            "--data-dir",
            root.to_str().expect("temporary path is UTF-8"),
            "--console-dir",
            "E:\\git\\dsh-nexus\\apps\\nexus-console",
            "--no-open",
        ])
        .expect("console arguments parse")
        .expect("console options are present");
        assert_eq!(options.command, LauncherCommand::Console);
        assert_eq!(options.console_port, DEFAULT_CONSOLE_PORT);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_command_defaults_to_headless_api() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-default-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let options = parse_args_from([
            "--data-dir",
            root.to_str().expect("temporary path is UTF-8"),
            "--open",
        ])
        .expect("empty arguments parse")
        .expect("console options are present");
        assert_eq!(options.command, LauncherCommand::Api);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launcher_config_is_loaded_before_cli_overrides() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-config-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        fs::create_dir_all(&root).expect("temporary root creates");
        let launcher_config = LauncherConfigFile {
            schema_version: DEFAULT_LAUNCHER_SCHEMA_VERSION,
            agent_program: Some(PathBuf::from("configured-agent")),
            agent_port: Some(3190),
            console_dir: Some(PathBuf::from("configured-console")),
            console_port: Some(3191),
            wait_secs: Some(9),
            open_browser: Some(false),
        };
        fs::write(
            root.join(LAUNCHER_CONFIG_FILE),
            serde_json::to_vec_pretty(&launcher_config).expect("config encodes"),
        )
        .expect("config writes");

        let root_arg = root.to_str().expect("temporary path is UTF-8");
        let configured = parse_args_from(["console", "--data-dir", root_arg])
            .expect("configured arguments parse")
            .expect("configured options are present");
        assert_eq!(configured.config.port, 3190);
        assert_eq!(
            configured.agent_program,
            Some(PathBuf::from("configured-agent"))
        );
        assert_eq!(configured.console_port, 3191);
        assert_eq!(configured.wait_secs, 9);

        let overridden = parse_args_from([
            "console",
            "--data-dir",
            root_arg,
            "--port",
            "3290",
            "--agent",
            "cli-agent",
            "--console-dir",
            "cli-console",
            "--console-port",
            "3291",
            "--wait-secs",
            "11",
            "--open",
        ])
        .expect("override arguments parse")
        .expect("override options are present");
        assert_eq!(overridden.config.port, 3290);
        assert_eq!(overridden.agent_program, Some(PathBuf::from("cli-agent")));
        assert_eq!(overridden.console_port, 3291);
        assert_eq!(overridden.wait_secs, 11);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_ui_info_prefers_a_loopback_token_url() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-harness-url-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        HarnessLogSessionStore::new(paths.clone())
            .write(&test_harness_log_session(&paths, "run-current", 1, 0, 0))
            .expect("log session writes");
        fs::write(
            paths.logs_dir.join("harness.stdout.log"),
            "ready at http://127.0.0.1:3080/health\nOpen this URL: http://127.0.0.1:3080/?token=abc123.\n",
        )
        .expect("Harness log writes");
        fs::write(
            paths.logs_dir.join("harness.stderr.log"),
            "ignore https://example.com/?token=remote\n",
        )
        .expect("Harness stderr writes");

        let info = read_harness_ui_info(&paths);
        assert!(info.available);
        assert_eq!(
            info.url.as_deref(),
            Some("http://127.0.0.1:3080/?token=abc123")
        );
        assert_eq!(info.token.as_deref(), Some("abc123"));
        assert!(info
            .source
            .as_deref()
            .is_some_and(|source| source.ends_with("harness.stdout.log")));
        assert!(parse_loopback_harness_url("https://127.0.0.1:3080/?token=x").is_none());
        assert!(parse_loopback_harness_url("http://example.com/?token=x").is_none());
        assert!(parse_loopback_harness_url("http://127.0.0.1:3080/?token=x&calc.exe").is_none());
        assert!(open_browser_url("http://127.0.0.1:3080/?token=x&calc.exe").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_ui_fails_closed_without_a_valid_log_session_marker() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-missing-log-session-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        fs::write(
            paths.logs_dir.join("harness.stdout.log"),
            "http://127.0.0.1:3080/?token=stale\n",
        )
        .expect("old token writes");

        let missing = read_harness_ui_info(&paths);
        assert!(!missing.available);
        assert_eq!(missing.url, None);
        assert_eq!(missing.token, None);

        fs::write(
            HarnessLogSessionStore::new(paths.clone()).path(),
            b"not-json",
        )
        .expect("corrupt marker writes");
        let corrupt = read_harness_ui_info(&paths);
        assert!(!corrupt.available);
        assert_eq!(corrupt.url, None);
        assert_eq!(corrupt.token, None);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn harness_ui_requires_running_agent_state_and_a_durable_marker() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-runtime-token-gate-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        fs::write(
            paths.logs_dir.join("harness.stdout.log"),
            "http://127.0.0.1:3080/?token=stale\n",
        )
        .expect("old token writes");
        let stdout_watermark = fs::metadata(paths.logs_dir.join("harness.stdout.log"))
            .expect("Harness stdout metadata reads")
            .len();
        let current_session = test_harness_log_session(&paths, "run-gated", 1, stdout_watermark, 0);
        let runtime = std::sync::Arc::new(std::sync::Mutex::new(HarnessRuntimeInfo::running(
            42,
            unix_time_seconds(),
            unix_time_seconds(),
        )));
        let health = HealthResponse::healthy(
            data_root_identity(&paths).expect("data-root identity reads"),
            "runtime-token-agent".to_owned(),
        );
        let app = Router::new()
            .route(
                "/v1/health",
                get(move || {
                    let health = health.clone();
                    async move { Json(health) }
                }),
            )
            .route(
                "/v1/harness",
                get({
                    let runtime = std::sync::Arc::clone(&runtime);
                    let session = current_session.clone();
                    move || {
                        let runtime = std::sync::Arc::clone(&runtime);
                        let session = session.clone();
                        async move {
                            let runtime = runtime.lock().expect("runtime locks").clone();
                            Json(HarnessResponse::from_observation(
                                runtime,
                                1,
                                session.run_id,
                                session.generation,
                                session.stdout_watermark,
                                session.stderr_watermark,
                                session.stdout_file_identity,
                                session.stderr_file_identity,
                                session.stdout_log_name,
                                session.stderr_log_name,
                                session.launch_pending,
                            ))
                        }
                    }
                }),
            );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("mock Agent binds");
        let port = listener.local_addr().expect("mock Agent address").port();
        let server =
            tokio::spawn(
                async move { axum::serve(listener, app).await.expect("mock Agent serves") },
            );
        let controller = ConsoleController::new(
            Options {
                command: LauncherCommand::Api,
                config: NexusConfig {
                    data_dir: Some(root.clone()),
                    port,
                },
                agent_program: None,
                console_port: DEFAULT_CONSOLE_PORT,
                wait_secs: DEFAULT_WAIT_SECS,
                json: false,
                launcher_instance_id: "test-launcher".to_owned(),
                launcher_capability: Some("a".repeat(64)),
            },
            paths.clone(),
            Client::new(),
        )
        .expect("controller creates");

        let missing_marker = controller.harness_ui().await;
        assert!(!missing_marker.available);
        assert_eq!(missing_marker.token, None);
        let mismatched_session = HarnessLogSession {
            run_id: "another-agent-run".to_owned(),
            ..current_session.clone()
        };
        HarnessLogSessionStore::new(paths.clone())
            .write(&mismatched_session)
            .expect("mismatched log session writes");
        let mismatched_writer = controller.harness_ui().await;
        assert!(!mismatched_writer.available);
        assert_eq!(mismatched_writer.token, None);
        HarnessLogSessionStore::new(paths.clone())
            .write(&current_session)
            .expect("log session writes");
        *runtime.lock().expect("runtime locks") =
            HarnessRuntimeInfo::starting(43, unix_time_seconds());
        let starting = controller.harness_ui().await;
        assert!(!starting.available);
        assert_eq!(starting.url, None);
        assert_eq!(starting.token, None);

        *runtime.lock().expect("runtime locks") =
            HarnessRuntimeInfo::running(43, unix_time_seconds(), unix_time_seconds());
        let no_current_token = controller.harness_ui().await;
        assert!(!no_current_token.available);
        assert_eq!(no_current_token.token, None);
        OpenOptions::new()
            .append(true)
            .open(paths.logs_dir.join("harness.stdout.log"))
            .expect("Harness stdout opens")
            .write_all(b"http://127.0.0.1:3080/?token=current\n")
            .expect("current token appends");
        let current = controller.harness_ui().await;
        assert!(current.available);
        assert_eq!(current.token.as_deref(), Some("current"));
        assert_eq!(current.generation, Some(1));
        assert_eq!(current.run_id.as_deref(), Some("run-gated"));

        let mut unattached =
            HarnessRuntimeInfo::running(43, unix_time_seconds(), unix_time_seconds());
        unattached.pid = None;
        *runtime.lock().expect("runtime locks") = unattached;
        let pidless = controller.harness_ui().await;
        assert!(
            !pidless.available,
            "an unattached process has no provable token-session continuity"
        );
        assert_eq!(pidless.url, None);
        assert_eq!(pidless.token, None);
        server.abort();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_cursor_ignores_touched_old_file_and_accepts_append() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-harness-cursor-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let stderr = paths.logs_dir.join("harness.stderr.log");
        fs::write(&stdout, "old http://127.0.0.1:3080/?token=old\n").expect("old stdout writes");
        fs::write(&stderr, "old http://127.0.0.1:3080/?token=old-stderr\n")
            .expect("old stderr writes");
        let session = test_harness_log_session(&paths, "run-cursor", 1, 0, 0);

        let mut observer = HarnessLogObserver::default();
        let initial = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(initial.available);
        fs::write(&stderr, "old http://127.0.0.1:3080/?token=old-stderr\n")
            .expect("old stderr is touched");
        fs::OpenOptions::new()
            .append(true)
            .open(&stdout)
            .expect("stdout opens")
            .write_all(b"new http://127.0.0.1:3080/?token=new\n")
            .expect("new stdout appends");

        let info = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(info.token.as_deref(), Some("new"));
        assert_eq!(
            info.url.as_deref(),
            Some("http://127.0.0.1:3080/?token=new")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_tail_requires_left_and_right_token_boundaries() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-token-boundaries-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let partial = "prefix-http://127.0.0.1:3080/?token=partial\n";
        let complete = "http://127.0.0.1:3080/?token=complete\n";
        let mut bytes = format!("{partial}{complete}").into_bytes();
        bytes.extend(std::iter::repeat_n(
            b'x',
            HARNESS_LOG_TAIL_BYTES as usize + 10 - bytes.len(),
        ));
        fs::write(&stdout, bytes).expect("long log writes");
        let session = test_harness_log_session(&paths, "run-boundary", 1, 0, 0);
        let mut observer = HarnessLogObserver::default();
        let info = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(info.token.as_deref(), Some("complete"));

        fs::write(&stdout, "http://127.0.0.1:3080/?token=fragment").expect("fragment writes");
        let fragment_session = test_harness_log_session(&paths, "run-fragment", 2, 0, 0);
        observer.invalidate();
        assert_eq!(
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&fragment_session))
                .token,
            None,
            "an EOF token is not published before a delimiter arrives"
        );
        OpenOptions::new()
            .append(true)
            .open(&stdout)
            .expect("fragment log opens")
            .write_all(b"\n")
            .expect("delimiter appends");
        assert_eq!(
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&fragment_session))
                .token
                .as_deref(),
            Some("fragment")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn per_run_log_path_ignores_longer_copy_truncate_of_legacy_log() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-per-run-log-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let legacy = paths.logs_dir.join("harness.stdout.log");
        fs::write(&legacy, "http://127.0.0.1:3080/?token=stale\n").expect("legacy token writes");
        let run_stdout = "harness-current.stdout.log";
        let run_stderr = "harness-current.stderr.log";
        fs::write(
            paths.logs_dir.join(run_stdout),
            "http://127.0.0.1:3080/?token=current\n",
        )
        .expect("per-run stdout writes");
        fs::write(paths.logs_dir.join(run_stderr), b"").expect("per-run stderr writes");
        let stdout_file = OpenOptions::new()
            .append(true)
            .open(paths.logs_dir.join(run_stdout))
            .expect("per-run stdout opens");
        let stderr_file = OpenOptions::new()
            .append(true)
            .open(paths.logs_dir.join(run_stderr))
            .expect("per-run stderr opens");
        let session = HarnessLogSession::new(
            "run-current".to_owned(),
            9,
            0,
            0,
            log_file_identity(&stdout_file).expect("stdout identity reads"),
            log_file_identity(&stderr_file).expect("stderr identity reads"),
            run_stdout.to_owned(),
            run_stderr.to_owned(),
            true,
            unix_time_seconds(),
        );
        let mut observer = HarnessLogObserver::default();
        assert_eq!(
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session))
                .token
                .as_deref(),
            Some("current")
        );
        fs::write(
            &legacy,
            format!(
                "http://127.0.0.1:3080/?token=stale\n{}",
                "x".repeat(HARNESS_LOG_TAIL_BYTES as usize)
            ),
        )
        .expect("legacy log is copy-truncated to a longer replacement");
        assert_eq!(
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session))
                .token
                .as_deref(),
            Some("current"),
            "the current run never reads the reusable legacy log path"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_cursor_accepts_the_tail_after_truncation() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-harness-rotation-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        fs::write(&stdout, "old http://127.0.0.1:3080/?token=old\n").expect("old log writes");
        let session = test_harness_log_session(&paths, "run-rotation", 1, 0, 0);

        let mut observer = HarnessLogObserver::default();
        let old = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(old.token.as_deref(), Some("old"));
        fs::write(&stdout, "replacement http://127.0.0.1:3080/?token=new\n")
            .expect("rotated log writes");

        let replacement = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(replacement.token.as_deref(), Some("new"));
        assert_eq!(
            replacement.url.as_deref(),
            Some("http://127.0.0.1:3080/?token=new")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_cursor_drops_old_token_after_sliding_window_rotation() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-harness-long-rotation-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let old_url = "http://127.0.0.1:3080/?token=old\n";
        let new_url = "http://127.0.0.1:3080/?token=new\n";
        let old_head = format!("{}{}", old_url, "o".repeat(100 - old_url.len()));
        let new_head = format!("{}{}", new_url, "n".repeat(100 - new_url.len()));
        let shared_tail = "x".repeat(HARNESS_LOG_TAIL_BYTES as usize - 100);
        let old = format!("{old_head}{shared_tail}");
        assert_eq!(old.len(), HARNESS_LOG_TAIL_BYTES as usize);
        fs::write(&stdout, old).expect("old log writes");
        let session = test_harness_log_session(&paths, "run-long-rotation", 1, 0, 0);

        let mut observer = HarnessLogObserver::default();
        let initial = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(initial.token.as_deref(), Some("old"));

        let replacement = format!("{new_head}{shared_tail}{}", "r".repeat(100));
        assert_eq!(replacement.len(), HARNESS_LOG_TAIL_BYTES as usize + 100);
        assert!(replacement.len() > observer.files[&stdout].offset as usize);
        fs::write(&stdout, replacement).expect("longer replacement writes");

        let info = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(!info.available, "the old token must not survive rotation");
        assert_eq!(info.token, None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_session_rejects_old_token_until_new_token_is_appended() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-log-session-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        fs::write(&stdout, "old http://127.0.0.1:3080/?token=old\n").expect("old token writes");
        let mut observer = HarnessLogObserver::default();
        assert!(
            !read_harness_ui_info_with_observer(&paths, &mut observer, None).available,
            "missing session is always fail-closed"
        );
        let watermark = fs::metadata(&stdout).expect("stdout metadata").len();
        let session = test_harness_log_session(&paths, "run-new", 2, watermark, 0);
        observer.invalidate();

        let stopped_then_starting =
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(!stopped_then_starting.available);
        assert_eq!(stopped_then_starting.token, None);
        OpenOptions::new()
            .append(true)
            .open(&stdout)
            .expect("stdout opens")
            .write_all(b"ordinary startup output without a URL\n")
            .expect("ordinary output appends");
        let ordinary_append =
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(
            !ordinary_append.available,
            "ordinary output must not resurrect the pre-session token"
        );
        assert_eq!(ordinary_append.token, None);

        OpenOptions::new()
            .append(true)
            .open(&stdout)
            .expect("stdout opens")
            .write_all(b"open http://127.0.0.1:3080/?token=current\n")
            .expect("current token appends");
        let current = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(current.available);
        assert_eq!(current.token.as_deref(), Some("current"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_session_rejects_cross_boundary_and_short_replacement_tokens() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-log-session-boundary-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let crossing = "prefix http://127.0.0.1:3080/?token=crossing\n";
        fs::write(&stdout, crossing).expect("cross-boundary token writes");
        let watermark = "prefix http://127".len() as u64;
        let session = test_harness_log_session(&paths, "run-boundary", 4, watermark, 0);
        let mut observer = HarnessLogObserver::default();

        let crossed = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(!crossed.available, "a URL straddling the boundary is stale");
        assert_eq!(crossed.token, None);

        let replacement_session =
            test_harness_log_session(&paths, "run-replacement", 5, crossing.len() as u64, 0);
        fs::write(&stdout, "http://127.0.0.1:3080/?token=replacement\n")
            .expect("short replacement writes");
        let replacement =
            read_harness_ui_info_with_observer(&paths, &mut observer, Some(&replacement_session));
        assert!(
            !replacement.available,
            "a file shorter than its session watermark has unknown identity"
        );
        assert_eq!(replacement.token, None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_session_rejects_same_path_replacement_with_a_new_file_identity() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-log-session-identity-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        fs::write(&stdout, "http://127.0.0.1:3080/?token=current\n").expect("current token writes");
        let session = test_harness_log_session(&paths, "run-identity", 6, 0, 0);
        let mut observer = HarnessLogObserver::default();
        let current = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert_eq!(current.token.as_deref(), Some("current"));

        fs::remove_file(&stdout).expect("old log removes");
        fs::write(
            &stdout,
            format!(
                "http://127.0.0.1:3080/?token=replacement\n{}",
                "x".repeat(1024)
            ),
        )
        .expect("longer replacement writes");
        let replacement = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(
            !replacement.available,
            "a same-path file replacement must not inherit the active session"
        );
        assert_eq!(replacement.token, None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_session_keeps_byte_offsets_with_invalid_utf8() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-log-session-bytes-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let prefix = [0xff, b' '];
        let stale = b"http://127.0.0.1:3080/?token=stale\n";
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(stale);
        fs::write(&stdout, bytes).expect("binary Harness output writes");
        let watermark = (prefix.len() + 8) as u64;
        let session = test_harness_log_session(&paths, "run-bytes", 7, watermark, 0);
        let mut observer = HarnessLogObserver::default();

        let info = read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        assert!(
            !info.available,
            "invalid UTF-8 must not shift a stale URL across its byte watermark"
        );
        assert_eq!(info.token, None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn harness_log_session_survives_launcher_observer_restart() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-log-session-restart-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let stdout = paths.logs_dir.join("harness.stdout.log");
        fs::write(&stdout, "old http://127.0.0.1:3080/?token=old\n").expect("old token writes");
        let watermark = fs::metadata(&stdout).expect("stdout metadata").len();
        let session = test_harness_log_session(&paths, "run-persisted", 9, watermark, 0);
        HarnessLogSessionStore::new(paths.clone())
            .write(&session)
            .expect("session marker writes");
        OpenOptions::new()
            .append(true)
            .open(&stdout)
            .expect("stdout opens")
            .write_all(b"open http://127.0.0.1:3080/?token=current\n")
            .expect("current token appends");

        let first_process = read_harness_ui_info(&paths);
        let restarted_process = read_harness_ui_info(&paths);
        assert_eq!(first_process.token.as_deref(), Some("current"));
        assert_eq!(restarted_process.token.as_deref(), Some("current"));
        assert_ne!(restarted_process.token.as_deref(), Some("old"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn console_status_is_json_safe() {
        let status = ConsoleStatus {
            running: true,
            desired_agent_running: true,
            agent_api: Some("http://127.0.0.1:3090".to_owned()),
            console_url: "http://127.0.0.1:3091/".to_owned(),
            data_root: "D:\\dsh-local\\nexus-data".to_owned(),
            data_root_id: "root-a".to_owned(),
            launcher_instance_id: "launcher-a".to_owned(),
            agent_pid: Some(42),
            agent_program: Some("nexus-agent.exe".to_owned()),
        };
        let encoded = serde_json::to_value(status).expect("console status serializes");
        assert_eq!(encoded["running"], true);
        assert_eq!(encoded["agent_pid"], 42);
        assert_eq!(encoded["console_url"], "http://127.0.0.1:3091/");
        assert_eq!(encoded["data_root_id"], "root-a");
        assert_eq!(encoded["launcher_instance_id"], "launcher-a");
        assert!(encoded.get("launcher_capability").is_none());
        assert!(encoded.get("capability").is_none());
    }

    #[test]
    fn launcher_control_routes_require_the_exact_helper_identity_pair() {
        assert!(!launcher_route_requires_identity("/launcher/status"));
        assert!(!launcher_route_requires_identity("/launcher/handshake"));
        assert!(launcher_route_requires_identity("/launcher/agent"));
        assert!(launcher_route_requires_identity(
            "/launcher/agent-api/v1/health"
        ));
        assert!(launcher_route_requires_identity(
            "/launcher/agent-api/v1/harness"
        ));
        let mut headers = axum::http::HeaderMap::new();
        assert!(!launcher_identity_values_match(
            &headers,
            "root-a",
            "launcher-a"
        ));
        headers.insert(
            LAUNCHER_DATA_ROOT_HEADER,
            "root-a".parse().expect("root header parses"),
        );
        assert!(!launcher_identity_values_match(
            &headers,
            "root-a",
            "launcher-a"
        ));
        headers.insert(
            LAUNCHER_INSTANCE_HEADER,
            "launcher-a".parse().expect("instance header parses"),
        );
        assert!(launcher_identity_values_match(
            &headers,
            "root-a",
            "launcher-a"
        ));
        assert!(!launcher_identity_values_match(
            &headers,
            "root-b",
            "launcher-a"
        ));
        assert!(!launcher_capability_values_match(&headers, &"a".repeat(64)));
        headers.insert(
            LAUNCHER_CAPABILITY_HEADER,
            "a".repeat(64).parse().expect("capability header parses"),
        );
        assert!(launcher_capability_values_match(&headers, &"a".repeat(64)));
        assert!(!launcher_capability_values_match(&headers, &"b".repeat(64)));
        assert_eq!(
            launcher_handshake_proof(&"a".repeat(64), &"b".repeat(64), "root-a", "launcher-a",),
            "b0a92a24f6aa2ea5e3352d3646293a69210182905e0b3b0e10206cb395325e7f"
        );
    }

    #[test]
    fn agent_proxy_namespace_maps_only_the_exact_allowlist() {
        let mappings = [
            ("/launcher/agent-api/v1/health", "/v1/health"),
            ("/launcher/agent-api/v1/state", "/v1/state"),
            ("/launcher/agent-api/v1/harness", "/v1/harness"),
            ("/launcher/agent-api/v1/profiles", "/v1/profiles"),
            ("/launcher/agent-api/v1/checkpoints", "/v1/checkpoints"),
            ("/launcher/agent-api/v1/releases", "/v1/releases"),
            ("/launcher/agent-api/v1/updates", "/v1/updates"),
            ("/launcher/agent-api/v1/diagnostics", "/v1/diagnostics"),
            ("/launcher/agent-api/v1/config", "/v1/config"),
        ];
        for (launcher_path, agent_path) in mappings {
            assert_eq!(agent_proxy_target_path(launcher_path), Some(agent_path));
        }
        assert_eq!(agent_proxy_target_path("/v1/state"), None);
        assert_eq!(
            agent_proxy_target_path("/launcher/agent-api/v1/shutdown"),
            None
        );
        assert_eq!(
            agent_proxy_target_path("/launcher/agent-api/v1/state/extra"),
            None
        );
    }

    #[tokio::test]
    async fn headless_api_exposes_agent_proxy_only_in_the_identity_bound_namespace() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-helper-identity-{}-{}",
            process::id(),
            unix_time_nanos()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("data root creates");
        let unused_agent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("unused Agent port reserves");
        let agent_port = unused_agent.local_addr().expect("Agent address").port();
        drop(unused_agent);
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(100))
            .timeout(Duration::from_millis(250))
            .build()
            .expect("client builds");
        let controller = ConsoleController::new(
            Options {
                command: LauncherCommand::Api,
                config: NexusConfig {
                    data_dir: Some(root.clone()),
                    port: agent_port,
                },
                agent_program: None,
                console_port: DEFAULT_CONSOLE_PORT,
                wait_secs: DEFAULT_WAIT_SECS,
                json: false,
                launcher_instance_id: "launcher-api-test".to_owned(),
                launcher_capability: Some("a".repeat(64)),
            },
            paths,
            client.clone(),
        )
        .expect("controller creates");
        let expected_root = controller.data_root_id.clone();
        let app = build_api_router(controller);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("Launcher API binds");
        let port = listener.local_addr().expect("Launcher address").port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serves") });

        let status = client
            .get(format!("http://127.0.0.1:{port}/launcher/status"))
            .send()
            .await
            .expect("bootstrap status responds")
            .json::<ConsoleStatus>()
            .await
            .expect("bootstrap status parses");
        assert_eq!(status.data_root_id, expected_root);
        assert_eq!(status.launcher_instance_id, "launcher-api-test");
        let challenge = "b".repeat(64);
        let handshake = client
            .get(format!("http://127.0.0.1:{port}/launcher/handshake"))
            .header(LAUNCHER_CHALLENGE_HEADER, &challenge)
            .send()
            .await
            .expect("bootstrap handshake responds");
        assert_eq!(handshake.status(), StatusCode::OK);
        assert_eq!(
            handshake
                .headers()
                .get(LAUNCHER_PROOF_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(
                launcher_handshake_proof(
                    &"a".repeat(64),
                    &challenge,
                    &status.data_root_id,
                    &status.launcher_instance_id,
                )
                .as_str()
            )
        );
        let rejected = client
            .get(format!(
                "http://127.0.0.1:{port}/launcher/agent-api/v1/health"
            ))
            .send()
            .await
            .expect("rejection responds");
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        let wrong_capability = client
            .get(format!(
                "http://127.0.0.1:{port}/launcher/agent-api/v1/health"
            ))
            .header(LAUNCHER_DATA_ROOT_HEADER, &status.data_root_id)
            .header(LAUNCHER_INSTANCE_HEADER, &status.launcher_instance_id)
            .header(LAUNCHER_CAPABILITY_HEADER, "b".repeat(64))
            .send()
            .await
            .expect("wrong capability rejection responds");
        assert_eq!(wrong_capability.status(), StatusCode::FORBIDDEN);
        let removed_top_level_route_without_identity = client
            .get(format!("http://127.0.0.1:{port}/v1/health"))
            .send()
            .await
            .expect("removed raw route responds");
        assert_eq!(
            removed_top_level_route_without_identity.status(),
            StatusCode::NOT_FOUND
        );
        let identity_bound = client
            .get(format!(
                "http://127.0.0.1:{port}/launcher/agent-api/v1/health"
            ))
            .header(LAUNCHER_DATA_ROOT_HEADER, &status.data_root_id)
            .header(LAUNCHER_INSTANCE_HEADER, &status.launcher_instance_id)
            .header(LAUNCHER_CAPABILITY_HEADER, "a".repeat(64))
            .send()
            .await
            .expect("identity-bound request responds");
        assert_eq!(identity_bound.status(), StatusCode::BAD_GATEWAY);
        let removed_top_level_route = client
            .get(format!("http://127.0.0.1:{port}/v1/health"))
            .header(LAUNCHER_DATA_ROOT_HEADER, &status.data_root_id)
            .header(LAUNCHER_INSTANCE_HEADER, &status.launcher_instance_id)
            .send()
            .await
            .expect("removed route responds");
        assert_eq!(removed_top_level_route.status(), StatusCode::NOT_FOUND);

        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }
}
