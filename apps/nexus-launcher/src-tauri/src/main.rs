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
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WindowEvent,
};

mod native_i18n;
use native_i18n::{text as native_text, NativeText};

const START_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_START_WAIT_SECS;
const STOP_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_STOP_WAIT_SECS;
static NATIVE_NOTIFICATIONS_ENABLED: AtomicBool = AtomicBool::new(false);
static MINIMIZE_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);
static EXIT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

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
    "/v1/harness/ui",
    "/v1/harness/discover",
    "/v1/profiles",
    "/v1/recovery",
    "/v1/preflight",
    "/v1/checkpoints",
    "/v1/releases",
    "/v1/releases/tags",
    "/v1/runtime",
    "/v1/runtime/plan",
    "/v1/updates",
    "/v1/diagnostics",
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
) -> Result<Value, String> {
    let method = method
        .parse::<Method>()
        .map_err(|_| "Only GET and POST are supported by the local Agent bridge".to_owned())?;

    if !is_allowed_route(&path) {
        return Err(format!("Agent route is not allowed: {path}"));
    }

    if method == Method::POST && path == "/v1/harness" {
        if let Ok(mut error) = state.harness_startup_error.lock() { *error = None; }
    }
    if path == "/v1/agent" {
        validate_native_agent_request(&method, body.as_ref())?;
        return execute_agent_action(&state, body.as_ref()).await;
    }

    // Opening the validated Harness URL is a native side effect. The URL and
    // token still come from the Agent endpoint; the shell never reads logs or
    // derives credentials itself.
    if path == "/v1/harness/ui" && method == Method::POST {
        let command = parse_command(body.as_ref())?;
        if command.action != "open" {
            return Err("Only the open action is supported for Harness UI metadata".to_owned());
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
            .map_err(|error| error.to_string())?;
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
        .map_err(|error| error.to_string())
}

fn validate_native_agent_request(method: &Method, body: Option<&Value>) -> Result<(), String> {
    if *method != Method::POST {
        return Err("Agent lifecycle adapter accepts POST only".to_owned());
    }
    let command = parse_command(body)?;
    if !matches!(
        command.action.as_str(),
        "start" | "stop" | "restart" | "status"
    ) {
        return Err(format!("Agent action is not allowed: {}", command.action));
    }
    Ok(())
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

async fn execute_agent_action(state: &AppState, body: Option<&Value>) -> Result<Value, String> {
    let command = parse_command(body)?;
    let action = match command.action.as_str() {
        "start" => AgentAction::Start,
        "stop" => AgentAction::Stop,
        "restart" => AgentAction::Restart,
        "status" => AgentAction::Status,
        _ => unreachable!("validate_native_agent_request checks Agent actions"),
    };
    match action {
        AgentAction::Status => Ok(serde_json::to_value(state.runtime.status().await)
            .map_err(|error| format!("Agent status could not be encoded: {error}"))?),
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
            Ok(json!({ "accepted": true, "action": command.action }))
        }
        AgentAction::Stop => {
            state.set_desired_running(false);
            state
                .runtime
                .action(action, STOP_WAIT_SECS)
                .await
                .map_err(|error| error.to_string())?;
            Ok(json!({ "accepted": true, "action": command.action }))
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
    Menu::with_items(app, &[&show, &quit, &stop_quit])
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
            "quit" => app.exit(0),
            "stop-quit" => {
                if EXIT_IN_PROGRESS.swap(true, Ordering::AcqRel) { return; }
                let app = app.clone();
                let state = app.state::<AppState>().inner().clone();
                tauri::async_runtime::spawn(async move {
                    match execute_agent_action(&state, Some(&json!({"action":"stop"}))).await {
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
    result.map_err(|error| error.to_string())
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
            build_identity,
            startup_status,
            retry_startup,
            proxy_request,
            set_native_locale,
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
    use super::*;

    #[test]
    fn background_bootstrap_returns_before_harness_and_retains_failure() {
        use std::{io::{Read, Write}, net::TcpListener, time::{Duration, Instant}};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = std::env::temp_dir().join(format!("nexus-bootstrap-dialog-{}-{}", std::process::id(), listener.local_addr().unwrap().port()));
        std::fs::create_dir_all(&root).unwrap();
        let program = root.join("fixture-agent.exe");
        std::fs::write(&program, "fixture; never executed").unwrap();
        let runtime = Arc::new(AgentRuntime::new(NexusConfig { data_dir: Some(root.clone()), port: listener.local_addr().unwrap().port() }, Some(program)).unwrap());
        let identity = runtime.data_root_id().to_owned();
        let state = AppState { runtime, startup_attempted: Arc::new(AtomicBool::new(true)), desired_running: Arc::new(AtomicBool::new(true)), harness_startup_error: Arc::new(Mutex::new(None)) };
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for route in ["GET /v1/health", "GET /v1/recovery", "GET /v1/config", "POST /v1/harness"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut bytes = [0u8; 8192];
                let length = stream.read(&mut bytes).unwrap();
                assert!(String::from_utf8_lossy(&bytes[..length]).starts_with(route));
                let (status, body) = if route.ends_with("health") {
                    ("200 OK", json!({"api_version":"v1","service":"nexus-agent","status":"ok","data_root_id":identity,"instance_id":"fixture"}).to_string())
                } else if route.ends_with("recovery") {
                    ("200 OK", json!({"api_version":"v1","paused":false}).to_string())
                } else if route.ends_with("config") {
                    ("200 OK", json!({"harness":{"mode":"node"}}).to_string())
                } else {
                    reached_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    ("500 Internal Server Error", json!({"api_version":"v1","code":"fixture","message":"Node entry missing"}).to_string())
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
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
        assert!(
            validate_native_agent_request(&Method::POST, Some(&json!({ "action": "start" })),)
                .is_ok()
        );
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
