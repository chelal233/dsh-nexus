//! Single-entry host for the headless Nexus Agent and native Launcher API.
//!
//! The launcher owns process bootstrap metadata and the native GUI API boundary
//! under Nexus' `run/` directory. It does not own Harness state, profile data,
//! release pointers, or business logic; those remain in the Agent and are
//! reachable through its loopback v1 API.

use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{self, Command as StdCommand},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    extract::State,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use hmac::{Hmac, Mac};
use nexus_core::{
    data_root_identity, load_harness_launch_spec, new_instance_id, HarnessLogSessionStore,
    NexusConfig, NexusPaths,
};
#[cfg(test)]
use nexus_core::{log_file_identity, HarnessLogSession};
use nexus_launcher_core::{
    harness_observation_matches_session, parse_loopback_harness_url,
    read_harness_ui_info_with_observer, AgentClient, AgentClientError, AgentResponse, AgentRuntime,
    AgentStartResult, HarnessLogObserver, HarnessUiInfo,
};
#[cfg(test)]
use nexus_launcher_core::{read_harness_ui_info, HARNESS_LOG_TAIL_BYTES};
use nexus_protocol::{HarnessAction, HarnessCommand, HarnessResponse, HarnessState, StateResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::{net::TcpListener, sync::Mutex, time::sleep};

mod installer_shutdown;
mod installer_cleanup;

fn unavailable_harness_ui_info(_paths: &NexusPaths, message: String) -> HarnessUiInfo {
    nexus_launcher_core::unavailable_harness_ui_info(message)
}

const DEFAULT_WAIT_SECS: u64 = 20;
const DEFAULT_STOP_WAIT_SECS: u64 = 15;
const DEFAULT_CONSOLE_PORT: u16 = 3091;
const DEFAULT_LAUNCHER_SCHEMA_VERSION: u32 = 1;
const CONSOLE_PORT_ENV: &str = "NEXUS_CONSOLE_PORT";
const LAUNCHER_WAIT_SECS_ENV: &str = "NEXUS_LAUNCHER_WAIT_SECS";
const CONSOLE_OPEN_ENV: &str = "NEXUS_CONSOLE_OPEN";
const LAUNCHER_CONFIG_FILE: &str = "launcher.json";
const LAUNCHER_DATA_ROOT_HEADER: &str = "x-nexus-launcher-data-root-id";
const LAUNCHER_INSTANCE_HEADER: &str = "x-nexus-launcher-instance-id";
const LAUNCHER_CAPABILITY_HEADER: &str = "x-nexus-launcher-capability";
const LAUNCHER_CHALLENGE_HEADER: &str = "x-nexus-launcher-challenge";
const LAUNCHER_PROOF_HEADER: &str = "x-nexus-launcher-proof";
const LAUNCHER_CAPABILITY_ENV: &str = "NEXUS_LAUNCHER_CAPABILITY";
const MAX_AGENT_PROXY_BODY_BYTES: usize = 32 * 1024;

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
    /// Headless loopback API for the native Electron shell.
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
    runtime: AgentRuntime,
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

#[tokio::main]
async fn main() {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("installer-cleanup")) {
        if let Err(message) = installer_cleanup::run(env::args_os().skip(2).collect()) {
            eprintln!("nexus-launcher installer-cleanup: {message}");
            process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("installer-parse-arguments")) {
        if let Err(message) = installer_shutdown::print_arguments() {
            eprintln!("nexus-launcher installer-parse-arguments: {message}");
            process::exit(1);
        }
        return;
    }
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("installer-authenticated-shutdown")) {
        if let Err(message) = installer_shutdown::authenticated_shutdown().await {
            eprintln!("nexus-launcher installer shutdown: {message}");
            process::exit(1);
        }
        return;
    }
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("installer-stop")) {
        if let Err(message) = installer_shutdown::run(env::args_os().skip(2).collect()) {
            eprintln!("nexus-launcher installer-stop: {message}");
            process::exit(1);
        }
        return;
    }
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

    // The native Electron shell owns the visible window. A no-argument launcher
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
    let runtime = AgentRuntime::new(options.config.clone(), options.agent_program.clone())
        .map_err(|error| format!("cannot initialize Nexus Agent runtime: {error}"))?;
    let paths = runtime.paths().clone();

    // Keep the Agent's browser CORS allowlist aligned with the legacy browser
    // port. The variable is inherited by a newly spawned Agent; an already-running
    // Agent must have been started with the same launcher configuration.
    env::set_var(CONSOLE_PORT_ENV, options.console_port.to_string());

    match options.command {
        LauncherCommand::Start => {
            let result = runtime
                .start(options.wait_secs)
                .await
                .map_err(|error| format!("cannot start Agent: {error}"))?;
            if result.started {
                persist_started_agent_or_stop(&runtime, &paths, &result).await?;
            }
            print_started(&options, result.pid, &paths, !result.started);
        }
        LauncherCommand::Run => {
            let result = runtime
                .start(options.wait_secs)
                .await
                .map_err(|error| format!("cannot start Agent: {error}"))?;
            if !result.started {
                return Err(
                    "Agent is already running; use `nexus-launcher status` or `stop`, or run foreground after stopping it"
                        .to_owned(),
                );
            }
            persist_started_agent_or_stop(&runtime, &paths, &result).await?;
            run_foreground(&runtime, &options, &paths).await?;
        }
        LauncherCommand::Api | LauncherCommand::Console => run_api(&options, &runtime).await?,
        LauncherCommand::Stop => stop_agent(&runtime, &options, &paths).await?,
        LauncherCommand::Status => status_agent(&runtime, &options, &paths).await?,
        LauncherCommand::Logs => print_logs(&options, &paths),
    }

    Ok(())
}

impl ConsoleController {
    fn new(options: Options, paths: NexusPaths, runtime: AgentRuntime) -> Result<Self, String> {
        let data_root_id = data_root_identity(&paths)
            .map_err(|error| format!("cannot identify Launcher data root: {error}"))?;
        let launcher_instance_id = options.launcher_instance_id.clone();
        let launcher_capability = options.launcher_capability.clone().ok_or_else(|| {
            "Launcher API requires a private capability supplied by the native owner".to_owned()
        })?;
        Ok(Self {
            options,
            paths,
            runtime,
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
        let result = self
            .runtime
            .start(self.options.wait_secs)
            .await
            .map_err(|error| format!("cannot start Agent: {error}"))?;
        if result.started {
            persist_started_agent_or_stop(&self.runtime, &self.paths, &result).await?;
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
        self.runtime
            .stop(self.options.wait_secs)
            .await
            .map_err(|error| format!("cannot stop Agent: {error}"))?;
        remove_launch_record(&self.paths);
        print_stop(&self.options, true, &self.paths);
        Ok(self.status().await)
    }

    async fn restart_agent(&self) -> Result<ConsoleStatus, String> {
        let _operation = self.operation.lock().await;
        {
            let mut state = self.state.lock().await;
            state.desired_agent_running = true;
        }
        self.runtime
            .stop(self.options.wait_secs)
            .await
            .map_err(|error| format!("cannot stop Agent: {error}"))?;
        remove_launch_record(&self.paths);
        self.start_agent_locked().await?;
        Ok(self.status().await)
    }

    async fn status(&self) -> ConsoleStatus {
        let health = self.runtime.probe().await.ok();
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
        verified_agent_client(&self.runtime)
            .await?
            .get_json::<HarnessResponse>("/v1/harness")
            .await
            .map_err(|error| format!("Agent Harness status is unavailable: {error}"))
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
        let response = match verified_agent_client(&self.runtime).await {
            Ok(client) => {
                client
                    .post_json::<_, HarnessResponse>(
                        "/v1/harness",
                        &HarnessCommand {
                            action: HarnessAction::Start,
                        },
                    )
                    .await
            }
            Err(error) => {
                eprintln!("nexus-launcher: Console auto-start Harness skipped: {error}");
                return;
            }
        };
        if let Err(error) = response {
            if !error.to_string().contains("HTTP 409") {
                eprintln!("nexus-launcher: Console auto-start Harness returned {error}");
            }
        }
    }

    async fn watchdog(self) {
        loop {
            sleep(Duration::from_secs(1)).await;
            let desired = self.state.lock().await.desired_agent_running;
            if desired && self.runtime.probe().await.is_err() {
                let _ = self.start_agent().await;
            }
        }
    }
}

async fn run_api(options: &Options, runtime: &AgentRuntime) -> Result<(), String> {
    let paths = runtime.paths().clone();
    let controller = ConsoleController::new(options.clone(), paths, runtime.clone())?;
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

async fn stop_agent(
    runtime: &AgentRuntime,
    options: &Options,
    paths: &NexusPaths,
) -> Result<(), String> {
    let was_running = runtime.probe().await.is_ok();
    runtime
        .stop(options.wait_secs)
        .await
        .map_err(|error| format!("cannot stop Agent: {error}"))?;
    remove_launch_record(paths);
    print_stop(options, was_running, paths);
    Ok(())
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
    let body = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(value) => Some(value),
            Err(error) => {
                return launcher_error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Agent request body is not valid JSON: {error}"),
                )
            }
        }
    };
    let client = match verified_agent_client(&controller.runtime).await {
        Ok(client) => client,
        Err(error) => return launcher_error_response(StatusCode::BAD_GATEWAY, error),
    };
    match client.request_raw_value(method, path, body.as_ref()).await {
        Ok(response) => agent_proxy_success_response(response),
        Err(error) => agent_proxy_error_response(error),
    }
}

fn agent_proxy_success_response(response: AgentResponse<Vec<u8>>) -> Response {
    agent_proxy_raw_response(response.status, response.body)
}

fn agent_proxy_error_response(error: AgentClientError) -> Response {
    match error {
        AgentClientError::Http { status, body, .. } => agent_proxy_raw_response(status, body),
        error => launcher_error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

fn agent_proxy_raw_response(status: reqwest::StatusCode, body: Vec<u8>) -> Response {
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(Body::from(body)).unwrap_or_else(|error| {
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

fn persist_started_agent(paths: &NexusPaths, result: &AgentStartResult) -> io::Result<()> {
    let pid = result
        .pid
        .ok_or_else(|| io::Error::other("started Agent did not expose a PID"))?;
    let program = result
        .program
        .as_ref()
        .ok_or_else(|| io::Error::other("started Agent program was not resolved"))?;
    write_launch_record(
        paths,
        &LaunchRecord {
            pid,
            port: result.port,
            started_at_unix: unix_time_seconds(),
            agent_program: program.to_string_lossy().into_owned(),
            agent_instance_id: Some(result.health.instance_id.clone()),
        },
    )
}

async fn persist_started_agent_or_stop(
    runtime: &AgentRuntime,
    paths: &NexusPaths,
    result: &AgentStartResult,
) -> Result<(), String> {
    if let Err(error) = persist_started_agent(paths, result) {
        remove_launch_record(paths);
        let cleanup = runtime.stop(DEFAULT_STOP_WAIT_SECS).await;
        return match cleanup {
            Ok(()) => Err(format!("cannot persist Agent launch record: {error}")),
            Err(cleanup_error) => Err(format!(
                "cannot persist Agent launch record: {error}; Agent cleanup failed: {cleanup_error}"
            )),
        };
    }
    Ok(())
}

async fn run_foreground(
    runtime: &AgentRuntime,
    options: &Options,
    paths: &NexusPaths,
) -> Result<(), String> {
    print_started(options, runtime.child_pid(), paths, false);
    let mut down_polls = 0u8;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.map_err(|error| format!("cannot listen for Ctrl+C: {error}"))?;
                runtime
                    .stop(DEFAULT_STOP_WAIT_SECS)
                    .await
                    .map_err(|error| format!("cannot stop Agent: {error}"))?;
                remove_launch_record(paths);
                println!("agent stopped");
                return Ok(());
            }
            _ = sleep(Duration::from_secs(1)) => {
                if runtime.probe().await.is_ok() {
                    down_polls = 0;
                    continue;
                }
                down_polls = down_polls.saturating_add(1);
                if down_polls < 3 {
                    continue;
                }
                let _ = runtime.stop(DEFAULT_STOP_WAIT_SECS).await;
                remove_launch_record(paths);
                println!("agent stopped");
                return Ok(());
            }
        }
    }
}

async fn verified_agent_client(runtime: &AgentRuntime) -> Result<AgentClient, String> {
    let health = runtime
        .probe()
        .await
        .map_err(|error| format!("Nexus Agent is not healthy: {error}"))?;
    Ok(runtime
        .client()
        .with_expected_identity(nexus_launcher_core::AgentIdentity::from(&health)))
}

async fn status_agent(
    runtime: &AgentRuntime,
    options: &Options,
    paths: &NexusPaths,
) -> Result<(), String> {
    let health = runtime.probe().await.ok();
    let record = read_launch_record(paths).filter(|record| {
        health.as_ref().is_some_and(|health| {
            record.agent_instance_id.as_deref() == Some(health.instance_id.as_str())
        })
    });
    let state = if health.is_some() {
        verified_agent_client(runtime)
            .await?
            .get_json::<StateResponse>("/v1/state")
            .await
            .map_err(|error| format!("cannot read Agent state: {error}"))?
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

fn launch_record_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("launcher-agent.json")
}

fn write_launch_record(paths: &NexusPaths, record: &LaunchRecord) -> io::Result<()> {
    let path = launch_record_path(paths);
    let temporary = paths.run_dir.join(format!(
        ".launcher-agent.json.tmp-{}-{}",
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
serves HTML or opens a browser. The native Electron shell can open the latest
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
    use nexus_protocol::{HarnessRuntimeInfo, HealthResponse};
    use reqwest::Client;

    async fn authenticated_mock_agent(
        State(credential): State<nexus_core::agent_auth::AgentCredential>, request: Request, next: Next,
    ) -> Response {
        use nexus_core::agent_auth as auth;
        if request.method() == axum::http::Method::GET && request.uri().path() == "/v1/health" { return next.run(request).await; }
        let (mut parts, body) = request.into_parts();
        let nonce = parts.headers.get(auth::NONCE_HEADER).and_then(|v|v.to_str().ok()).unwrap().to_owned();
        let time = parts.headers.get(auth::TIME_HEADER).and_then(|v|v.to_str().ok()).unwrap();
        let signature = parts.headers.get(auth::SIGNATURE_HEADER).and_then(|v|v.to_str().ok()).unwrap();
        assert_eq!(parts.headers.get(auth::VERSION_HEADER).unwrap(), "2");
        let ciphertext = to_bytes(body, 1024 * 1024).await.unwrap();
        assert!(credential.verify_request(parts.method.as_str(), parts.uri.path(), &nonce, time, &ciphertext, signature));
        let plaintext = credential.open_request(parts.method.as_str(), parts.uri.path(), &nonce, time, &ciphertext).unwrap();
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        let response = next.run(Request::from_parts(parts, Body::from(plaintext))).await;
        let (mut parts, body) = response.into_parts();
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
        let ciphertext = credential.seal_response(&nonce, parts.status.as_u16(), &bytes).unwrap();
        let signature = credential.response_signature(&nonce, parts.status.as_u16(), &ciphertext);
        parts.headers.insert(auth::VERSION_HEADER, "2".parse().unwrap());
        parts.headers.insert(auth::RESPONSE_HEADER, signature.parse().unwrap());
        parts.headers.insert(axum::http::header::CONTENT_LENGTH, ciphertext.len().to_string().parse().unwrap());
        Response::from_parts(parts, Body::from(ciphertext))
    }

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
    fn launch_record_is_json_safe_and_round_trips() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-test-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("directories create");
        let discovery = nexus_core::AgentDiscoveryRecord {
            port: 54321, instance_id: "discovery-owner".into(),
            data_root_id: "discovery-root".into(), pid: 123, updated_at_unix: 10,
        };
        paths.publish_agent_discovery(&discovery).unwrap();
        let discovery_bytes = fs::read(paths.run_dir.join("agent.json")).unwrap();
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
                    .starts_with(".launcher-agent.json.tmp-")),
            "an atomic launch-record publication must not leave a temporary file"
        );
        remove_launch_record(&paths);
        assert_eq!(fs::read(paths.run_dir.join("agent.json")).unwrap(), discovery_bytes);
        assert_eq!(paths.read_agent_discovery().unwrap().unwrap().instance_id, "discovery-owner");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_probe_rejects_agent_for_a_different_data_root() {
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
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(expected.root.clone()),
                port,
            },
            None,
        )
        .expect("runtime creates");
        let error = runtime
            .probe()
            .await
            .expect_err("foreign data root is never adopted");
        assert!(error.to_string().contains("identity mismatch"));
        server.abort();
        let _ = server.await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn verified_agent_client_binds_the_probed_root_and_instance() {
        let root = std::env::temp_dir().join(format!(
            "nexus-launcher-verified-proxy-{}-{}",
            process::id(),
            unix_time_seconds()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().expect("root creates");
        let data_root_id = data_root_identity(&paths).expect("root identity reads");
        let instance_id = "expected-instance".to_owned();
        let credential = nexus_core::agent_auth::AgentCredential::publish(&paths, &instance_id).unwrap();
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
                            .get(nexus_launcher_core::AGENT_DATA_ROOT_HEADER)
                            .and_then(|value| value.to_str().ok())
                            == Some(expected_root.as_str())
                            && headers
                                .get(nexus_launcher_core::AGENT_INSTANCE_HEADER)
                                .and_then(|value| value.to_str().ok())
                                == Some(expected_instance.as_str())
                        {
                            Json(json!({ "ok": true }))
                        } else {
                            Json(json!({ "ok": false }))
                        }
                    }
                }),
            );
        let app = app.layer(middleware::from_fn_with_state(credential, authenticated_mock_agent));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("Agent mock binds");
        let port = listener.local_addr().expect("Agent mock address").port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serves") });
        let runtime = AgentRuntime::new(
            NexusConfig {
                data_dir: Some(root.clone()),
                port,
            },
            None,
        )
        .expect("runtime creates");
        let response = verified_agent_client(&runtime)
            .await
            .expect("identity-bound client builds")
            .get_json::<Value>("/v1/state")
            .await
            .expect("identity-bound request sends");
        assert_eq!(response.get("ok"), Some(&Value::Bool(true)));
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
        let credential = nexus_core::agent_auth::AgentCredential::publish(&paths, "runtime-token-agent").unwrap();
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
        let app = app.layer(middleware::from_fn_with_state(credential, authenticated_mock_agent));
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
            AgentRuntime::new(
                NexusConfig {
                    data_dir: Some(root.clone()),
                    port,
                },
                None,
            )
            .expect("Agent runtime creates"),
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
    fn harness_log_cursor_does_not_refresh_old_words_across_streams() {
        let root = std::env::temp_dir().join(format!("nexus-token-age-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let stdout = paths.logs_dir.join("harness.stdout.log");
        let stderr = paths.logs_dir.join("harness.stderr.log");
        let timestamp = |path: &std::path::Path, seconds| {
            fs::OpenOptions::new().write(true).open(path).unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)).unwrap();
        };
        fs::write(&stdout, "http://127.0.0.1:3080/?token=first\nhttp://127.0.0.1:3080/?token=second\n").unwrap();
        timestamp(&stdout, 100);
        let session = test_harness_log_session(&paths, "age", 1, 0, 0);
        let mut observer = HarnessLogObserver::default();
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("second"));
        fs::write(&stderr, "http://127.0.0.1:3080/?token=new-stderr\n").unwrap();
        timestamp(&stderr, 200);
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("new-stderr"));
        fs::OpenOptions::new().append(true).open(&stdout).unwrap().write_all(b"ordinary output\n").unwrap();
        timestamp(&stdout, 300);
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("new-stderr"));
        fs::OpenOptions::new().append(true).open(&stdout).unwrap().write_all(b"http://127.0.0.1:3080/?token=new-stdout\n").unwrap();
        timestamp(&stdout, 400);
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("new-stdout"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn harness_log_cursor_retains_only_verified_original_bytes_beyond_tail() {
        use std::io::{Seek, SeekFrom};
        for mutation in ["token", "left", "right", "session"] {
            let root = std::env::temp_dir().join(format!("nexus-token-proof-{mutation}-{}", nexus_core::unix_time_nanos_for_update()));
            let paths = NexusPaths::from_root(root.clone());
            paths.ensure_directories().unwrap();
            let stdout = paths.logs_dir.join("harness.stdout.log");
            let url = "http://127.0.0.1:3080/?token=current";
            fs::write(&stdout, format!("x {url}\n")).unwrap();
            let session = test_harness_log_session(&paths, "proof", 1, 0, 0);
            let mut observer = HarnessLogObserver::default();
            assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("current"));
            fs::OpenOptions::new().append(true).open(&stdout).unwrap().write_all(&vec![b'x'; HARNESS_LOG_TAIL_BYTES as usize + 20]).unwrap();
            assert_eq!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).token.as_deref(), Some("current"));
            if mutation == "session" {
                assert!(!read_harness_ui_info_with_observer(&paths, &mut observer, None).available);
                assert!(!read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).available);
            } else {
                let offset = match mutation { "left" => 1, "right" => 2 + url.len(), _ => 2 + url.len() - 1 };
                let mut file = fs::OpenOptions::new().write(true).open(&stdout).unwrap();
                file.seek(SeekFrom::Start(offset as u64)).unwrap();
                file.write_all(b"Z").unwrap();
                drop(file);
                assert!(!read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).available, "{mutation}");
            }
            fs::remove_dir_all(root).unwrap();
        }
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
    async fn agent_proxy_preserves_legacy_agent_http_status_and_error_body() {
        let body = br#"{"api_version":"v1","code":"busy","message":"Agent lifecycle is busy"}"#;
        let response = agent_proxy_error_response(AgentClientError::Http {
            status: reqwest::StatusCode::CONFLICT,
            message: "Agent lifecycle is busy".to_owned(),
            body: body.to_vec(),
        });
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), MAX_AGENT_PROXY_BODY_BYTES)
            .await
            .expect("proxy error body is readable");
        assert_eq!(bytes.as_ref(), body);
        let value: Value = serde_json::from_slice(&bytes).expect("proxy error body is JSON");
        assert_eq!(value["api_version"], "v1");
        assert_eq!(value["code"], "busy");
    }

    #[test]
    fn agent_proxy_preserves_legacy_agent_success_status_codes() {
        let created = agent_proxy_success_response(AgentResponse {
            status: StatusCode::CREATED,
            body: br#"{"accepted":true}"#.to_vec(),
        });
        assert_eq!(created.status(), StatusCode::CREATED);

        let no_content = agent_proxy_success_response(AgentResponse {
            status: StatusCode::NO_CONTENT,
            body: Vec::new(),
        });
        assert_eq!(no_content.status(), StatusCode::NO_CONTENT);
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
            // The shared AgentRuntime deliberately uses the same bounded
            // transport as the native entry points. This route test is about
            // namespace/auth behavior, so allow that bounded probe to finish
            // when the selected port is intentionally unused.
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(3))
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
            AgentRuntime::new(
                NexusConfig {
                    data_dir: Some(root.clone()),
                    port: agent_port,
                },
                None,
            )
            .expect("Agent runtime creates"),
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
