#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
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
    AppHandle, Manager, WindowEvent,
};

mod native_i18n;
use native_i18n::{text as native_text, NativeText};

const START_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_START_WAIT_SECS;
const STOP_WAIT_SECS: u64 = nexus_launcher_core::DEFAULT_STOP_WAIT_SECS;

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
    "/v1/checkpoints",
    "/v1/releases",
    "/v1/releases/tags",
    "/v1/runtime",
    "/v1/runtime/plan",
    "/v1/updates",
    "/v1/diagnostics",
    "/v1/config",
    "/v1/lifecycle",
    "/v1/shutdown",
];

#[derive(Clone)]
struct AppState {
    runtime: Arc<AgentRuntime>,
    startup_attempted: Arc<AtomicBool>,
    desired_running: Arc<AtomicBool>,
}

#[derive(Debug, Deserialize)]
struct AgentCommand {
    action: String,
}

impl AppState {
    fn new(resource_dir: Option<PathBuf>) -> Result<Self, String> {
        let config = NexusConfig::from_env();
        let runtime = AgentRuntime::new_with_resource_dir(config, None, resource_dir)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            runtime: Arc::new(runtime),
            startup_attempted: Arc::new(AtomicBool::new(false)),
            desired_running: Arc::new(AtomicBool::new(true)),
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

#[tauri::command]
async fn startup_status(state: tauri::State<'_, AppState>) -> Result<AgentStatus, String> {
    if state.should_auto_start() {
        if let Err(error) = state.runtime.ensure_started(START_WAIT_SECS).await {
            // `status` retains a bounded, user-visible startup error and never
            // hides a failed Agent resolution behind a fallback port or process.
            let mut status = state.runtime.status().await;
            status.message = Some(error.to_string());
            return Ok(status);
        }
        start_configured_harness(&state).await;
    }
    Ok(state.runtime.status().await)
}

/// The GUI is the user-facing launcher, so its first successful Agent
/// handshake also performs the configured Harness bootstrap. Harness remains
/// an independent child of Agent; this is only orchestration and a 409 means
/// another owner already has it running.
async fn start_configured_harness(state: &AppState) {
    let health = match state.runtime.probe().await {
        Ok(health) => health,
        Err(error) => {
            eprintln!("nexus-launcher-app: Harness auto-start skipped: {error}");
            return;
        }
    };
    let client = state
        .runtime
        .client()
        .with_expected_identity(AgentIdentity::from(&health));
    let config = match client.get_json::<Value>("/v1/config").await {
        Ok(config) => config,
        Err(error) => {
            eprintln!("nexus-launcher-app: Harness auto-start config check failed: {error}");
            return;
        }
    };
    if !config.get("harness").is_some_and(|value| !value.is_null()) {
        return;
    }
    if let Err(error) = client
        .post_json::<_, Value>("/v1/harness", &json!({ "action": "start" }))
        .await
    {
        let message = error.to_string();
        if !message.contains("HTTP 409") {
            eprintln!("nexus-launcher-app: Harness auto-start failed: {message}");
        }
    }
}

#[tauri::command]
async fn retry_startup(state: tauri::State<'_, AppState>) -> Result<AgentStatus, String> {
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
            .probe()
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
        .probe()
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
            start_configured_harness(state).await;
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
    Menu::with_items(app, &[&show, &quit])
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

fn main() {
    let builder = tauri::Builder::default()
        // The single-instance plugin must be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            startup_status,
            retry_startup,
            proxy_request,
            set_native_locale
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
