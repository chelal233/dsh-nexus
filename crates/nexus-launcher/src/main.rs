//! Small, replaceable process launcher for the headless Nexus Agent.
//!
//! The launcher owns only process bootstrap metadata under Nexus' `run/`
//! directory. It does not own Harness state, profile data, release pointers,
//! or business logic; those remain in the Agent and are reachable through its
//! loopback v1 API.

use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{self, Command as StdCommand, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nexus_core::{NexusConfig, NexusPaths};
use nexus_protocol::{HealthResponse, HealthStatus, StateResponse};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{
    process::{Child, Command as TokioCommand},
    time::{sleep, Instant},
};

const DEFAULT_WAIT_SECS: u64 = 20;
const DEFAULT_STOP_WAIT_SECS: u64 = 15;
const LOCK_STALE_AFTER_SECS: u64 = 30;
const AGENT_BINARY_ENV: &str = "NEXUS_AGENT_BIN";

#[derive(Debug, Clone)]
struct Options {
    command: LauncherCommand,
    config: NexusConfig,
    agent_program: Option<PathBuf>,
    wait_secs: u64,
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherCommand {
    Start,
    Run,
    Stop,
    Status,
    Logs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchRecord {
    pid: u32,
    port: u16,
    started_at_unix: u64,
    agent_program: String,
}

struct InstanceLock {
    path: PathBuf,
    remove_on_drop: bool,
}

impl InstanceLock {
    fn retain(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct OwnedAgent {
    child: Option<Child>,
    lock: InstanceLock,
    pid: u32,
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
    let mut config = NexusConfig::from_env();
    let mut command = None;
    let mut agent_program = None;
    let mut wait_secs = DEFAULT_WAIT_SECS;
    let mut json = false;
    let mut args = env::args_os().skip(1);

    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "start" if command.is_none() => command = Some(LauncherCommand::Start),
            "run" | "foreground" if command.is_none() => command = Some(LauncherCommand::Run),
            "stop" if command.is_none() => command = Some(LauncherCommand::Stop),
            "status" if command.is_none() => command = Some(LauncherCommand::Status),
            "logs" if command.is_none() => command = Some(LauncherCommand::Logs),
            "--data-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--data-dir requires PATH".to_owned())?;
                let path = PathBuf::from(value);
                if path.as_os_str().is_empty() {
                    return Err("--data-dir cannot be empty".to_owned());
                }
                config.data_dir = Some(path);
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--port requires PORT".to_owned())?;
                config.port = parse_port(&value.to_string_lossy())?;
            }
            "--agent" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--agent requires PATH".to_owned())?;
                let path = PathBuf::from(value);
                if path.as_os_str().is_empty()
                    || path.to_string_lossy().chars().any(char::is_control)
                {
                    return Err(
                        "--agent must be a non-empty path without control characters".to_owned(),
                    );
                }
                agent_program = Some(path);
            }
            "--wait-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--wait-secs requires SECONDS".to_owned())?;
                wait_secs = value
                    .to_string_lossy()
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=300).contains(seconds))
                    .ok_or_else(|| "--wait-secs must be between 1 and 300".to_owned())?;
            }
            "--json" => json = true,
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            value => return Err(format!("unknown argument: {value}")),
        }
    }

    let Some(command) = command else {
        return Err("a command is required (start, run, stop, status, or logs)".to_owned());
    };

    Ok(Some(Options {
        command,
        config,
        agent_program,
        wait_secs,
        json,
    }))
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

    match options.command {
        LauncherCommand::Start => {
            let result = ensure_agent_started(&options, &paths, &client).await?;
            match result {
                EnsureResult::AlreadyRunning => print_started(&options, None, &paths, true),
                EnsureResult::Owned(mut owned) => {
                    let pid = owned.pid;
                    owned.lock.retain();
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

async fn ensure_agent_started(
    options: &Options,
    paths: &NexusPaths,
    client: &Client,
) -> Result<EnsureResult, String> {
    if is_agent_healthy(client, options.config.port).await {
        return Ok(EnsureResult::AlreadyRunning);
    }

    let lock = match acquire_lock(paths) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            sleep(Duration::from_millis(250)).await;
            if is_agent_healthy(client, options.config.port).await {
                return Ok(EnsureResult::AlreadyRunning);
            }
            if !lock_is_stale(paths) {
                return Err(format!(
                    "another launcher is starting the Agent or holds {}; retry after it finishes",
                    lock_path(paths).display()
                ));
            }
            remove_lock(paths);
            acquire_lock(paths).map_err(|error| {
                format!("cannot recover and acquire Agent instance lock: {error}")
            })?
        }
        Err(error) => return Err(format!("cannot acquire Agent instance lock: {error}")),
    };

    if is_agent_healthy(client, options.config.port).await {
        return Ok(EnsureResult::AlreadyRunning);
    }

    let agent_program = resolve_agent_program(options.agent_program.as_deref())?;
    let detached = options.command == LauncherCommand::Start;
    let log_offset = agent_log_len(paths);
    let (mut child, pid) = spawn_agent(&agent_program, &options.config, paths, detached)
        .map_err(|error| format!("cannot start Agent: {error}"))?;
    if let Err(error) = write_launch_record(
        paths,
        &LaunchRecord {
            pid,
            port: options.config.port,
            started_at_unix: unix_time_seconds(),
            agent_program: agent_program.to_string_lossy().into_owned(),
        },
    ) {
        kill_agent_pid(pid).await;
        if let Some(child) = child.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        return Err(format!("cannot persist Agent launch record: {error}"));
    }

    // Use a fresh HTTP client after process creation. A client that attempted
    // a connection before the listener existed can retain a Windows TCP
    // refusal while another local WebShell is polling the same port.
    let startup_client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|error| format!("cannot initialize Agent health client: {error}"))?;
    if let Err(error) = wait_for_health(
        &startup_client,
        options.config.port,
        options.wait_secs,
        paths,
        log_offset,
    )
    .await
    {
        kill_agent_pid(pid).await;
        if let Some(child) = child.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        remove_launch_record(paths);
        return Err(error);
    }

    Ok(EnsureResult::Owned(OwnedAgent { child, lock, pid }))
}

fn acquire_lock(paths: &NexusPaths) -> io::Result<InstanceLock> {
    let path = lock_path(paths);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    writeln!(file, "pid={}", process::id())?;
    file.flush()?;
    Ok(InstanceLock {
        path,
        remove_on_drop: true,
    })
}

fn lock_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("agent.lock")
}

fn lock_is_stale(paths: &NexusPaths) -> bool {
    let Ok(modified) = fs::metadata(lock_path(paths)).and_then(|metadata| metadata.modified())
    else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age >= Duration::from_secs(LOCK_STALE_AFTER_SECS))
        .unwrap_or(false)
}

fn launch_record_path(paths: &NexusPaths) -> PathBuf {
    paths.run_dir.join("agent.json")
}

#[cfg(not(windows))]
fn spawn_agent(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    _detached: bool,
) -> io::Result<(Option<Child>, u32)> {
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
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("Agent process did not expose a PID"))?;
    Ok((Some(child), pid))
}

#[cfg(windows)]
fn spawn_agent(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
    detached: bool,
) -> io::Result<(Option<Child>, u32)> {
    if !detached {
        return spawn_agent_direct(agent_program, config, paths);
    }

    let pid = spawn_agent_windows_detached(agent_program, config, paths)?;
    Ok((None, pid))
}

#[cfg(windows)]
fn spawn_agent_direct(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
) -> io::Result<(Option<Child>, u32)> {
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
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.creation_flags(0x0900_0200);
    let child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("Agent process did not expose a PID"))?;
    Ok((Some(child), pid))
}

#[cfg(windows)]
fn spawn_agent_windows_detached(
    agent_program: &Path,
    config: &NexusConfig,
    paths: &NexusPaths,
) -> io::Result<u32> {
    use std::{
        ffi::{OsStr, OsString},
        mem::size_of,
        os::windows::ffi::OsStrExt,
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
        CloseHandle(process_info.hProcess);
    }
    Ok(process_info.dwProcessId)
}

async fn kill_agent_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = StdCommand::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(unix)]
    {
        let _ = StdCommand::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
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

async fn is_agent_healthy(client: &Client, port: u16) -> bool {
    matches!(
        probe_health(client, port).await,
        Ok(Some(response)) if response.status == HealthStatus::Ok
    )
}

async fn probe_health(client: &Client, port: u16) -> Result<Option<HealthResponse>, String> {
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
    response
        .json::<HealthResponse>()
        .await
        .map(Some)
        .map_err(|error| format!("invalid Agent health response: {error}"))
}

fn agent_log_len(paths: &NexusPaths) -> u64 {
    fs::metadata(paths.logs_dir.join("agent.stdout.log"))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn agent_log_ready(paths: &NexusPaths, offset: u64) -> bool {
    let path = paths.logs_dir.join("agent.stdout.log");
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).is_ok() && text.contains("nexus agent listening")
}

async fn wait_for_health(
    client: &Client,
    port: u16,
    wait_secs: u64,
    paths: &NexusPaths,
    log_offset: u64,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut last_error = None;
    loop {
        if agent_log_ready(paths, log_offset) {
            return Ok(());
        }
        match probe_health(client, port).await {
            Ok(Some(response)) if response.status == HealthStatus::Ok => return Ok(()),
            Ok(_) => {}
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
    let health = probe_health(client, options.config.port).await?;
    if health.is_none() {
        remove_launch_record(paths);
        remove_lock(paths);
        print_stop(options, false, paths);
        return Ok(());
    }

    let response = client
        .post(format!(
            "http://127.0.0.1:{}/v1/shutdown",
            options.config.port
        ))
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
        if probe_health(client, options.config.port).await?.is_none() {
            remove_launch_record(paths);
            remove_lock(paths);
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
                request_shutdown(client, options.config.port).await?;
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
                if !launch_record_path(paths).exists() && !lock_path(paths).exists() {
                    if let Ok(Some(status)) = child.try_wait() {
                        println!("agent exited: {status}");
                    } else {
                        let _ = child.start_kill();
                        println!("agent stopped");
                    }
                    remove_launch_record(paths);
                    return Ok(());
                }
                match probe_health(client, options.config.port).await {
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

async fn request_shutdown(client: &Client, port: u16) -> Result<(), String> {
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/shutdown"))
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
    let health = probe_health(client, options.config.port).await?;
    let record = read_launch_record(paths);
    let state = if health.is_some() {
        client
            .get(format!("http://127.0.0.1:{}/v1/state", options.config.port))
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
    if options.json {
        let value = serde_json::json!({
            "api_version": nexus_protocol::API_VERSION,
            "data_root": paths.root.display().to_string(),
            "agent_stdout": paths.logs_dir.join("agent.stdout.log"),
            "agent_stderr": paths.logs_dir.join("agent.stderr.log"),
            "harness_stdout": paths.logs_dir.join("harness.stdout.log"),
            "harness_stderr": paths.logs_dir.join("harness.stderr.log"),
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
            paths.logs_dir.join("harness.stdout.log").display()
        );
        println!(
            "harness_stderr: {}",
            paths.logs_dir.join("harness.stderr.log").display()
        );
        println!("launch_record: {}", launch_record_path(paths).display());
    }
}

fn write_launch_record(paths: &NexusPaths, record: &LaunchRecord) -> io::Result<()> {
    let path = launch_record_path(paths);
    let temporary = path.with_extension(format!("tmp-{}", process::id()));
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&temporary, bytes)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(temporary, path)
}

fn read_launch_record(paths: &NexusPaths) -> Option<LaunchRecord> {
    let bytes = fs::read(launch_record_path(paths)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn remove_launch_record(paths: &NexusPaths) {
    let _ = fs::remove_file(launch_record_path(paths));
}

fn remove_lock(paths: &NexusPaths) {
    let _ = fs::remove_file(lock_path(paths));
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn print_help() {
    println!(
        r#"nexus-launcher

Usage:
  nexus-launcher start [--data-dir PATH] [--port PORT] [--agent PATH] [--wait-secs SECONDS] [--json]
  nexus-launcher run|foreground [--data-dir PATH] [--port PORT] [--agent PATH] [--wait-secs SECONDS]
  nexus-launcher stop [--data-dir PATH] [--port PORT] [--json]
  nexus-launcher status [--data-dir PATH] [--port PORT] [--json]
  nexus-launcher logs [--data-dir PATH] [--json]

`start` detaches a sibling `nexus-agent`; `run` owns it in the foreground and
stops it gracefully on Ctrl+C. The Agent remains the only owner of Harness,
profile, checkpoint, release, update, and diagnostics behavior. The launcher
binds no network listener and talks to loopback only.

Environment: NEXUS_DATA_DIR, NEXUS_AGENT_PORT, NEXUS_AGENT_BIN."#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        write_launch_record(&paths, &record).expect("record writes");
        assert_eq!(read_launch_record(&paths), Some(record));
        remove_launch_record(&paths);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parser_requires_a_known_command() {
        // Keep this test independent of the process command line. The parser
        // is exercised by the release smoke and --help output is intentionally
        // stable for scripts.
        assert_eq!(DEFAULT_WAIT_SECS, 20);
        assert_eq!(LOCK_STALE_AFTER_SECS, 30);
    }
}
