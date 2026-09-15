#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use nexus_core::NexusConfig;
use nexus_launcher_core::{
    validate_agent_request, AgentAction, AgentIdentity, AgentRuntime, AgentStatus,
    MAX_REQUEST_BODY_BYTES,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};

#[tauri::command]
async fn choose_local_path(window: tauri::WebviewWindow, directory: bool, save: Option<bool>, archive: Option<bool>) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let owner = window.hwnd().map_err(|error| error.to_string())?.0 as usize;
        let (send, receive) = std::sync::mpsc::channel();
        window.run_on_main_thread(move || {
            let result = unsafe { choose_windows_path(owner, directory, save.unwrap_or(false), archive.unwrap_or(false)) };
            let _ = send.send(result);
        }).map_err(|error| error.to_string())?;
        tauri::async_runtime::spawn_blocking(move || receive.recv().map_err(|error| error.to_string()))
            .await.map_err(|error| error.to_string())??
    }
    #[cfg(not(windows))]
    { let _ = (window, directory, save, archive); Err("Path selection is available in the Windows launcher".into()) }
}

#[cfg(windows)]
fn folder_browse_info(owner: usize, display_name: &mut [u16; windows_sys::Win32::Foundation::MAX_PATH as usize]) -> windows_sys::Win32::UI::Shell::BROWSEINFOW {
    use windows_sys::Win32::UI::Shell::*;
    BROWSEINFOW { hwndOwner: owner as _, pszDisplayName: display_name.as_mut_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX, ..unsafe { std::mem::zeroed() } }
}

#[cfg(windows)]
unsafe fn choose_windows_path(owner: usize, directory: bool, save: bool, archive: bool) -> Result<Option<String>, String> {
    use windows_sys::Win32::{System::Com::CoTaskMemFree, UI::{Shell::*, Controls::Dialogs::*}};
    let mut buffer = vec![0u16; 32768];
    if directory {
        let mut display_name = [0u16; windows_sys::Win32::Foundation::MAX_PATH as usize];
        let info = folder_browse_info(owner, &mut display_name);
        let selected = SHBrowseForFolderW(&info);
        if selected.is_null() { return Ok(None); }
        let ok = SHGetPathFromIDListEx(selected, buffer.as_mut_ptr(), buffer.len() as u32, 0) != 0;
        CoTaskMemFree(selected.cast());
        if !ok { return Err("The selected folder path could not be read".into()); }
    } else {
        let filter: Vec<u16> = "Nexus offline package (*.tar.gz)\0*.tar.gz\0\0".encode_utf16().collect();
        let extension: Vec<u16> = "tar.gz\0".encode_utf16().collect();
        if save && archive {
            for (index, value) in "harness-export.tar.gz".encode_utf16().enumerate() { buffer[index] = value; }
        }
        let mut info = OPENFILENAMEW { lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: owner as _, lpstrFile: buffer.as_mut_ptr(), nMaxFile: buffer.len() as u32,
            lpstrFilter: if archive { filter.as_ptr() } else { std::ptr::null() },
            lpstrDefExt: if archive { extension.as_ptr() } else { std::ptr::null() },
            Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR
                | if save { OFN_OVERWRITEPROMPT } else { OFN_FILEMUSTEXIST },
            ..std::mem::zeroed() };
        let selected = if save { GetSaveFileNameW(&mut info) } else { GetOpenFileNameW(&mut info) };
        if selected == 0 {
            let code = CommDlgExtendedError();
            return if code == 0 { Ok(None) } else { Err(format!("File picker failed (Windows error {code:#x})")) };
        }
    }
    let end = buffer.iter().position(|value| *value == 0).ok_or("The selected path exceeds the supported length")?;
    let path = String::from_utf16(&buffer[..end]).map_err(|error| error.to_string())?;
    Ok((!path.is_empty()).then_some(path))
}

mod native_i18n;
use native_i18n::{text as native_text, NativeText};

const START_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_START_WAIT_SECS;
#[derive(Debug, serde::Serialize)]
struct BridgeError {
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
        Self { preflight: None, code: "launcher_error".into(), message, retryable: false, actions: vec![], status: None }
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
            return Self { preflight: document.get("preflight").cloned(), code, message: document.get("message").and_then(Value::as_str).unwrap_or(&message).to_owned(),
                retryable: matches!(status.as_u16(), 408 | 429 | 502 | 503 | 504), actions, status: Some(status.as_u16()) };
        }
        let retryable = matches!(&error, nexus_launcher_core::AgentClientError::Transport(_));
        Self { preflight: None, code: if retryable { "agent_transport_error" } else { "agent_protocol_error" }.into(), message: error.to_string(), retryable,
            actions: vec!["check_agent".into()], status: None }
    }
}
const STOP_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_STOP_WAIT_SECS;
static NATIVE_NOTIFICATIONS_ENABLED: AtomicBool = AtomicBool::new(false);
static MINIMIZE_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);
static EXIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static DIAGNOSTIC_EXPORT_ACTIVE: AtomicBool = AtomicBool::new(false);

#[tauri::command]
async fn export_startup_diagnostics(state: tauri::State<'_, AppState>, observed_error: Option<String>) -> Result<Value, String> {
    if DIAGNOSTIC_EXPORT_ACTIVE.swap(true, Ordering::AcqRel) {
        return Err("Diagnostic export is already in progress".into());
    }
    struct Guard;
    impl Drop for Guard { fn drop(&mut self) { DIAGNOSTIC_EXPORT_ACTIVE.store(false, Ordering::Release); } }
    let guard = Guard;
    let paths = state.runtime.paths().clone();
    let context = json!({"build":build_identity().ok(),"agent_startup_error":state.runtime.startup_error(),
        "observed_agent_error":observed_error.map(|error| error.chars().take(4096).collect::<String>()),
        "harness_startup_error":state.harness_startup_error.lock().ok().and_then(|value| value.clone()),
        "data_root":paths.root,"collection":"Launcher only; Agent availability is not required"});
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let path = nexus_launcher_core::startup_diagnostics::export(&paths, context).map_err(|error| error.to_string())?;
        #[cfg(windows)]
        let reveal_error = Command::new("explorer.exe").arg(format!("/select,{}",path.display())).spawn().err().map(|error| error.to_string());
        #[cfg(not(windows))]
        let reveal_error = Some("Open the exported file path manually".to_owned());
        Ok(json!({"export_path":path,"reveal_error":reveal_error,"source":"launcher"}))
    }).await.map_err(|error| error.to_string())?
}

#[tauri::command]
fn set_native_notifications(enabled: bool) {
    NATIVE_NOTIFICATIONS_ENABLED.store(enabled, Ordering::Release);
}

#[tauri::command]
fn build_identity() -> Result<Value, String> {
    serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/release-identity.json")))
        .map_err(|error| error.to_string())
}

/// The native shell talks to the independent Agent only through its versioned
/// loopback API. `/v1/agent` is the small native lifecycle adapter because an
/// Agent process cannot start itself; all state and Harness operations are
/// forwarded directly to the Agent's `/v1/*` routes.
const ALLOWED_ROUTES: &[&str] = &[
    "/v1/agent",
    "/v1/health",
    "/v1/state",
    "/v1/harness",
    "/v1/harness/startup",
    "/v1/harness/ui",
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
    "/v1/diagnostics",
    "/v1/requests",
    "/v1/canary",
    "/v1/config",
    "/v1/maintenance",
    "/v1/lifecycle",
    "/v1/shutdown",
];

#[derive(Clone)]
struct AppState {
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

#[tauri::command]
async fn startup_status(state: tauri::State<'_, AppState>) -> Result<StartupResponse, String> {
    if state.should_auto_start() {
        if let Err(error) = state.runtime.ensure_started(START_WAIT_SECS).await {
            // `status` retains a bounded, user-visible startup error and never
            // hides a failed Agent resolution behind a fallback port or process.
            let mut status = state.runtime.status().await;
            status.message = Some(error.to_string());
            return Ok(startup_response(&state, status));
        }
        schedule_configured_harness(state.inner().clone());
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
    tauri::async_runtime::spawn(async move {
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

#[tauri::command]
async fn retry_startup(state: tauri::State<'_, AppState>) -> Result<StartupResponse, String> {
    state.set_desired_running(true);
    state.startup_attempted.store(false, Ordering::Release);
    startup_status(state).await
}

#[tauri::command]
async fn proxy_request(
    state: tauri::State<'_, AppState>,
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

    // Opening the validated Harness URL is a native side effect. The URL and
    // token still come from the Agent endpoint; the shell never reads logs or
    // derives credentials itself.
    if path == "/v1/harness/ui" && method == Method::POST {
        let command = parse_command(body.as_ref())?;
        if command.action != "open" {
            return Err("Only the open action is supported for Harness UI metadata".to_owned().into());
        }
        let health = state
            .runtime
            .probe_ready()
            .await
            .map_err(|error| error.to_string())?;
        let client = state
            .runtime
            .client()
            .with_expected_identity(AgentIdentity::from(&health));
        let info = client
            .get_json::<Value>("/v1/harness/ui")
            .await
            .map_err(BridgeError::from)?;
        let url = info
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "Agent did not report a validated Harness URL".to_owned())?;
        open_loopback_url(url)?;
        return Ok(info);
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
    ALLOWED_ROUTES.iter().any(|route| *route == path)
}

fn open_loopback_url(raw: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw).map_err(|error| format!("Harness URL is invalid: {error}"))?;
    if url.scheme() != "http"
        || url.username() != ""
        || url.password().is_some()
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        || url.port_or_known_default().is_none()
        || raw.chars().any(char::is_control)
    {
        return Err("refusing to open a non-loopback HTTP Harness URL".to_owned());
    }
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.args(["url.dll,FileProtocolHandler", raw]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(raw);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(raw);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open Harness URL in the system browser: {error}"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TrayControls {
    state: String,
    start: bool,
    stop: bool,
    web: bool,
    terminal: bool,
}
static TRAY_CONTROLS: Mutex<Option<(TrayControls, std::time::Instant)>> = Mutex::new(None);
// Hidden Chromium pages may deliver timers only once per minute. Keep a
// bounded cache across those batches; explicit unavailable updates still win.
const TRAY_FRESH_SECS: u64 = 180;
fn tray_controls_at(value: Option<&(TrayControls, std::time::Instant)>, now: std::time::Instant) -> TrayControls {
    value.filter(|(_, time)| now.saturating_duration_since(*time).as_secs() < TRAY_FRESH_SECS)
        .map(|(controls, _)| controls.clone()).unwrap_or_default()
}
fn current_tray_controls() -> TrayControls {
    TRAY_CONTROLS.lock().ok().map(|value| tray_controls_at(value.as_ref(), std::time::Instant::now())).unwrap_or_default()
}
#[tauri::command]
fn update_tray(app: AppHandle, controls: TrayControls) -> Result<(), String> {
    let changed = current_tray_controls() != controls;
    *TRAY_CONTROLS.lock().map_err(|_| "Tray state lock unavailable")? =
        Some((controls, std::time::Instant::now()));
    #[cfg(desktop)]
    if changed { rebuild_tray(&app)?; }
    Ok(())
}
#[cfg(desktop)]
fn rebuild_tray(app: &AppHandle) -> Result<(), String> {
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(tray_menu(app, native_i18n::active_locale()).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(desktop)]
fn tray_menu(
    app: &impl Manager<tauri::Wry>,
    locale: native_i18n::NativeLocale,
) -> tauri::Result<Menu<tauri::Wry>> {
    let show = MenuItem::with_id(
        app,
        "show",
        native_text(locale, NativeText::TrayShow),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        native_text(locale, NativeText::TrayQuit),
        true,
        None::<&str>,
    )?;
    let stop_quit = MenuItem::with_id(app, "stop-quit", native_text(locale, NativeText::TrayStopQuit), true, None::<&str>)?;
    let controls = current_tray_controls();
    let status_key = match controls.state.as_str() {
        "running" => NativeText::HarnessRunning, "stopped" | "detached" => NativeText::HarnessStopped,
        "starting" => NativeText::HarnessStarting, "stopping" => NativeText::HarnessStopping,
        "failed" => NativeText::HarnessFailed, _ => NativeText::HarnessUnknown,
    };
    let status = MenuItem::with_id(app, "status", native_text(locale, status_key), false, None::<&str>)?;
    let action = if controls.stop { "stop" } else { "start" };
    let lifecycle = MenuItem::with_id(app, action, native_text(locale,
        if controls.stop { NativeText::HarnessStop } else { NativeText::HarnessStart }),
        controls.start || controls.stop, None::<&str>)?;
    let web = MenuItem::with_id(app, "web", native_text(locale, NativeText::HarnessWeb), controls.web, None::<&str>)?;
    let terminal = MenuItem::with_id(app, "terminal", native_text(locale, NativeText::DshTerminal), controls.terminal, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&status, &show, &lifecycle, &web, &terminal, &separator, &quit, &stop_quit])
}

#[cfg(desktop)]
fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let locale = native_i18n::active_locale();
    let menu = tray_menu(app, locale)?;
    TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon()
                .cloned()
                .expect("Nexus Launcher config must provide a default icon"),
        )
        .menu(&menu)
        .tooltip(native_text(locale, NativeText::TrayTooltip))
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            action @ ("start" | "stop" | "web" | "terminal") => {
                let controls = current_tray_controls();
                let allowed = match action { "start" => controls.start, "stop" => controls.stop,
                    "web" => controls.web, "terminal" => controls.terminal, _ => false };
                if allowed { let _ = app.emit("nexus-tray-action", action); }
                else { show_window(app); }
            },
            "quit" => app.exit(0),
            "stop-quit" => {
                if EXIT_IN_PROGRESS.swap(true, Ordering::AcqRel) { return; }
                let app = app.clone();
                let state = app.state::<AppState>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    match execute_agent_action(&state, AgentAction::Stop).await {
                        Ok(_) => app.exit(0),
                        Err(error) => {
                            EXIT_IN_PROGRESS.store(false, Ordering::Release);
                            show_window(&app);
                            let _ = app.emit("nexus-native-error", error);
                        }
                    }
                });
            }
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
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let expired = TRAY_CONTROLS.lock().ok().is_some_and(|mut value| {
                if value.as_ref().is_some_and(|(_, time)| time.elapsed().as_secs() >= TRAY_FRESH_SECS) {
                    *value = None; true
                } else { false }
            });
            if expired { let _ = rebuild_tray(&handle); }
        }
    });
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn set_native_locale(app: AppHandle, locale: String) -> Result<(), String> {
    let locale = native_i18n::NativeLocale::from_code(&locale)
        .ok_or_else(|| format!("unsupported locale: {locale}"))?;
    native_i18n::set_active_locale(locale);
    let menu = tray_menu(&app, locale).map_err(|error| error.to_string())?;
    let tray = app
        .tray_by_id("main")
        .ok_or_else(|| "native tray is not available".to_owned())?;
    tray.set_menu(Some(menu))
        .map_err(|error| error.to_string())?;
    tray.set_tooltip(Some(native_text(locale, NativeText::TrayTooltip)))
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(not(desktop))]
#[tauri::command]
fn set_native_locale(_app: AppHandle, _locale: String) -> Result<(), String> {
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn autostart_status(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())
}

#[cfg(not(desktop))]
#[tauri::command]
fn autostart_status(_app: AppHandle) -> Result<bool, String> {
    Ok(false)
}

#[cfg(desktop)]
#[tauri::command]
fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autostart = app.autolaunch();
    let result = if enabled {
        autostart.enable()
    } else {
        autostart.disable()
    };
    result.map_err(|error| error.to_string())?;
    #[cfg(windows)]
    if enabled {
        // auto-launch 0.5 writes an unquoted executable path. The default
        // installation directory contains spaces, so bind the Run command to
        // the exact executable instead of Windows' ambiguous path parsing.
        if let Err(error) = quote_windows_autostart(&app.package_info().name) {
            let rollback = autostart.disable();
            return Err(format!("Cannot save login startup command: {error}; rollback: {rollback:?}"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn quote_windows_autostart(name: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ};
    let wide = |value: &std::ffi::OsStr| value.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let key = wide(std::ffi::OsStr::new("Software\\Microsoft\\Windows\\CurrentVersion\\Run"));
    let name = wide(std::ffi::OsStr::new(name));
    let executable = std::env::current_exe()?;
    let mut command = vec![34u16];
    command.extend(executable.as_os_str().encode_wide());
    command.extend([34, 0]);
    let mut handle = std::ptr::null_mut();
    let opened = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut handle) };
    if opened != 0 { return Err(std::io::Error::from_raw_os_error(opened as i32)); }
    let written = unsafe { RegSetValueExW(handle, name.as_ptr(), 0, REG_SZ, command.as_ptr().cast(), (command.len() * 2) as u32) };
    unsafe { RegCloseKey(handle); }
    if written != 0 { return Err(std::io::Error::from_raw_os_error(written as i32)); }
    Ok(())
}

#[cfg(not(desktop))]
#[tauri::command]
fn autostart_set(_app: AppHandle, _enabled: bool) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn agent_log_set(level: String) -> Result<(), String> {
    nexus_launcher_core::set_agent_log_level(&level);
    Ok(())
}

fn main() {
    let builder = tauri::Builder::default()
        // The single-instance plugin must be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            choose_local_path,
            build_identity,
            startup_status,
            retry_startup,
            proxy_request,
            set_native_locale,
            update_tray,
            export_startup_diagnostics,
            set_native_notifications,
            autostart_status,
            autostart_set,
            agent_log_set
        ])
        .setup(|app| {
            let resource_dir = app.path().resource_dir().map_err(|error| {
                tauri::Error::Io(std::io::Error::other(format!(
                    "cannot resolve the installed Agent resource directory: {error}"
                )))
            })?;
            let state = AppState::new(Some(resource_dir))
                .map_err(|error| tauri::Error::Io(std::io::Error::other(error)))?;
            app.manage(state);
            #[cfg(desktop)]
            {
                #[cfg(windows)]
                {
                    use tauri_plugin_autostart::ManagerExt;
                    if app.autolaunch().is_enabled().unwrap_or(false) {
                        if let Err(error) = quote_windows_autostart(&app.package_info().name) {
                            eprintln!("nexus-launcher: cannot repair login startup command: {error}");
                        }
                    }
                }
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
                if !NATIVE_NOTIFICATIONS_ENABLED.load(Ordering::Acquire) || MINIMIZE_NOTICE_SHOWN.swap(true, Ordering::AcqRel) { return; }
                use tauri_plugin_notification::NotificationExt;
                let _ = app
                    .notification()
                    .builder()
                    .title(native_text(
                        native_i18n::active_locale(),
                        NativeText::MinimizedTitle,
                    ))
                    .body(native_text(
                        native_i18n::active_locale(),
                        NativeText::MinimizedBody,
                    ))
                    .show();
            }
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building Nexus Launcher");
    app.run(|_, _| {});
}

#[cfg(test)]
mod tests {
    #[test]
    fn tray_cache_tolerates_hidden_timer_batches_but_expires_or_accepts_unavailable() {
        let now=std::time::Instant::now();
        let controls=super::TrayControls { terminal:true, ..Default::default() };
        let cached=(controls.clone(),now);
        for seconds in [0,65,125,179] { assert_eq!(super::tray_controls_at(Some(&cached),now+std::time::Duration::from_secs(seconds)),controls); }
        assert_eq!(super::tray_controls_at(Some(&cached),now+std::time::Duration::from_secs(180)),super::TrayControls::default());
        assert_eq!(super::tray_controls_at(Some(&(super::TrayControls::default(),now)),now),super::TrayControls::default());
    }
    #[cfg(windows)]
    #[test]
    fn folder_picker_provides_the_shell_a_max_path_output_buffer() {
        let mut display_name = [0u16; windows_sys::Win32::Foundation::MAX_PATH as usize];
        let info = super::folder_browse_info(0, &mut display_name);
        assert_eq!(info.pszDisplayName, display_name.as_mut_ptr());
        // The shell may write all MAX_PATH units, including the terminator.
        unsafe { for index in 0..display_name.len() { *info.pszDisplayName.add(index) = if index + 1 == display_name.len() { 0 } else { b'x' as u16 }; } }
        assert_eq!(display_name[display_name.len() - 2], b'x' as u16);
        assert_eq!(display_name[display_name.len() - 1], 0);
    }
    use super::*;

    #[test]
    fn background_bootstrap_returns_before_harness_and_retains_failure() {
        use std::{io::{Read, Write}, net::TcpListener, time::{Duration, Instant}};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = std::env::temp_dir().join(format!("nexus-bootstrap-dialog-{}-{}", std::process::id(), listener.local_addr().unwrap().port()));
        std::fs::create_dir_all(&root).unwrap();
        let program = root.join("fixture-agent.exe");
        std::fs::write(&program, "fixture; never executed").unwrap();
        let build_id = option_env!("NEXUS_BUILD_ID").unwrap_or("native-bootstrap-fixture");
        std::fs::write(root.join("release-identity.json"), json!({"schemaVersion":1,"buildId":build_id}).to_string()).unwrap();
        let runtime = Arc::new(AgentRuntime::new(NexusConfig { data_dir: Some(root.clone()), port: listener.local_addr().unwrap().port() }, Some(program.clone())).unwrap());
        let paths = nexus_core::NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let credential = nexus_core::agent_auth::AgentCredential::publish(&paths,"fixture").unwrap();
        let identity = runtime.data_root_id().to_owned();
        let state = AppState { runtime, startup_attempted: Arc::new(AtomicBool::new(true)), desired_running: Arc::new(AtomicBool::new(true)), harness_startup_error: Arc::new(Mutex::new(None)) };
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for route in ["GET /v1/health", "GET /v1/recovery", "GET /v1/config", "POST /v1/harness"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                // Read the whole framed request; a TCP read can split headers
                // and encrypted body at any byte boundary.
                let mut bytes = Vec::new(); let mut chunk = [0u8; 8192];
                let boundary = loop {
                    let length = stream.read(&mut chunk).unwrap(); assert_ne!(length,0);
                    bytes.extend_from_slice(&chunk[..length]); assert!(bytes.len()<=64*1024);
                    if let Some(index)=bytes.windows(4).position(|v|v==b"\r\n\r\n") {break index+4;}
                };
                let headers = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
                assert!(headers.starts_with(route));
                let header = |name:&str| headers.lines().find_map(|line|line.split_once(':').filter(|(key,_)|key.eq_ignore_ascii_case(name)).map(|(_,value)|value.trim()));
                let length:usize=header("content-length").unwrap_or("0").parse().unwrap(); assert!(length<=64*1024);
                while bytes.len()<boundary+length {let count=stream.read(&mut chunk).unwrap();assert_ne!(count,0);bytes.extend_from_slice(&chunk[..count]);}
                let nonce = if route.ends_with("health") { None } else {
                    use nexus_core::agent_auth as auth;
                    assert_eq!(header(auth::VERSION_HEADER),Some("2"));
                    assert_eq!(header("x-nexus-data-root-id"),Some(identity.as_str()));
                    assert_eq!(header("x-nexus-instance-id"),Some("fixture"));
                    let nonce=header(auth::NONCE_HEADER).unwrap(); let time=header(auth::TIME_HEADER).unwrap();
                    let (method,path)=route.split_once(' ').unwrap(); let body=&bytes[boundary..boundary+length];
                    assert!(credential.verify_request(method,path,nonce,time,body,header(auth::SIGNATURE_HEADER).unwrap()));
                    let plaintext=credential.open_request(method,path,nonce,time,body).unwrap();
                    if method=="POST" {assert_eq!(serde_json::from_slice::<Value>(&plaintext).unwrap(),json!({"action":"start"}));} else {assert!(plaintext.is_empty());}
                    Some(nonce)
                };
                let (status, body) = if route.ends_with("health") {
                    ("200 OK", json!({"api_version":"v1","service":"nexus-agent","status":"ok","data_root_id":identity,"instance_id":"fixture","build_id":build_id,"binary_path":program,"auth_version":2,"harness_config_wire_version":2}).to_string())
                } else if route.ends_with("recovery") {
                    ("200 OK", json!({"api_version":"v1","paused":false}).to_string())
                } else if route.ends_with("config") {
                    ("200 OK", json!({"harness":{"mode":"node"}}).to_string())
                } else {
                    reached_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    ("500 Internal Server Error", json!({"api_version":"v1","code":"fixture","message":"Node entry missing"}).to_string())
                };
                if let Some(nonce)=nonce {
                    use nexus_core::agent_auth as auth;
                    let code=status.split_whitespace().next().unwrap().parse().unwrap();
                    let ciphertext=credential.seal_response(nonce,code,body.as_bytes()).unwrap();
                    let signature=credential.response_signature(nonce,code,&ciphertext);
                    write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}: 2\r\n{}: {signature}\r\nConnection: close\r\n\r\n",ciphertext.len(),auth::VERSION_HEADER,auth::RESPONSE_HEADER).unwrap();
                    stream.write_all(&ciphertext).unwrap();
                } else {
                    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        let begin = Instant::now();
        schedule_configured_harness(state.clone());
        assert!(begin.elapsed() < Duration::from_secs(1));
        reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(state.harness_startup_error.lock().unwrap().is_none());
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.harness_startup_error.lock().unwrap().is_none() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        assert!(state.harness_startup_error.lock().unwrap().as_deref().unwrap().contains("Node entry missing"));
        server.join().unwrap();
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_routes_are_direct_agent_routes() {
        assert!(is_allowed_route("/v1/health"));
        assert!(is_allowed_route("/v1/harness/ui"));
        assert!(is_allowed_route("/v1/harness/discover"));
        assert!(is_allowed_route("/v1/releases/tags"));
        assert!(is_allowed_route("/v1/runtime"));
        assert!(is_allowed_route("/v1/runtime/plan"));
        assert!(is_allowed_route("/v1/recovery"));
        assert!(is_allowed_route("/v1/agent"));
        assert!(!is_allowed_route("/v1/health?url=https://example.com"));
        assert!(validate_agent_request("/v1/runtime", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/runtime", &Method::POST, None).is_err());
        assert!(validate_agent_request("/v1/runtime", &Method::GET, Some(b"{}")).is_err());
        assert!(validate_agent_request("/v1/runtime/plan", &Method::GET, None).is_err());
        assert!(validate_agent_request("/v1/runtime/plan", &Method::POST, Some(b"{}")).is_ok());
        assert!(validate_agent_request("/v1/releases/tags", &Method::GET, None).is_ok());
        assert!(validate_agent_request("/v1/releases/tags", &Method::POST, None).is_err());
        assert!(validate_agent_request("/v1/releases/tags", &Method::GET, Some(b"{}")).is_err());
    }

    #[test]
    fn lifecycle_adapter_actions_are_bounded() {
        for (name, action) in [
            ("start", AgentAction::Start),
            ("stop", AgentAction::Stop),
            ("restart", AgentAction::Restart),
            ("status", AgentAction::Status),
        ] {
            assert_eq!(
                validate_native_agent_request(&Method::POST, Some(&json!({ "action": name })))
                    .unwrap(),
                action
            );
        }
        assert!(
            validate_native_agent_request(&Method::GET, Some(&json!({ "action": "status" })),)
                .is_err()
        );
        assert!(
            validate_native_agent_request(&Method::POST, Some(&json!({ "action": "exec" })),)
                .is_err()
        );
        assert!(parse_command(Some(&json!({
            "action": "x".repeat(MAX_REQUEST_BODY_BYTES)
        })))
        .is_err());
    }

    #[test]
    fn external_open_rejects_non_loopback_urls() {
        assert!(open_loopback_url("https://example.com/?token=secret").is_err());
        assert!(open_loopback_url("http://127.0.0.1:3080/?token=secret\n").is_err());
    }

    #[test]
    fn startup_gate_runs_once_and_stop_can_disable_future_auto_start() {
        let attempted = AtomicBool::new(false);
        assert!(attempted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        assert!(attempted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());

        let desired = AtomicBool::new(true);
        desired.store(false, Ordering::Release);
        assert!(!desired.load(Ordering::Acquire));
    }
}
