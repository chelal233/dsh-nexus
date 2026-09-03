#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    env,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
};

use reqwest::{redirect::Policy, Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WindowEvent,
};

const DEFAULT_API_PORT: u16 = 3091;
const API_PORT_ENV: &str = "NEXUS_CONSOLE_PORT";
const API_PORT_OVERRIDE_ENV: &str = "NEXUS_LAUNCHER_API_PORT";
const LAUNCHER_BIN_ENV: &str = "NEXUS_LAUNCHER_BIN";
const MAX_PROXY_BODY_BYTES: usize = 32 * 1024;
const MAX_PROXY_RESPONSE_BYTES: usize = 512 * 1024;

const ALLOWED_ROUTES: &[&str] = &[
    "/launcher/status",
    "/launcher/agent",
    "/launcher/harness",
    "/launcher/logs",
    "/v1/health",
    "/v1/state",
    "/v1/harness",
    "/v1/profiles",
    "/v1/checkpoints",
    "/v1/releases",
    "/v1/updates",
    "/v1/diagnostics",
    "/v1/config",
];

struct AppState {
    client: Client,
    api_base: String,
    helper: Mutex<HelperState>,
}

struct HelperState {
    path: Option<PathBuf>,
    child: Option<Child>,
    startup_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StartupStatus {
    available: bool,
    api_base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    helper_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyErrorBody {
    message: Option<String>,
}

impl AppState {
    fn new() -> Self {
        let port = api_port();
        let api_base = format!("http://127.0.0.1:{port}");
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(2))
                .timeout(std::time::Duration::from_secs(8))
                .redirect(Policy::none())
                .build()
                .expect("native launcher HTTP client configuration is valid"),
            api_base,
            helper: Mutex::new(HelperState {
                path: None,
                child: None,
                startup_error: None,
            }),
        }
    }

    fn start_helper(&self) {
        let mut helper = self.helper.lock().expect("helper state lock");
        if let Some(child) = helper.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(_)) | Err(_) => helper.child = None,
            }
        }

        let path = match resolve_launcher_helper() {
            Ok(path) => path,
            Err(error) => {
                helper.startup_error = Some(error);
                return;
            }
        };
        helper.path = Some(path.clone());
        helper.startup_error = None;

        let mut command = Command::new(&path);
        command
            .arg("api")
            .arg("--no-open")
            .arg("--console-port")
            .arg(api_port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(child) => helper.child = Some(child),
            Err(error) => {
                helper.startup_error = Some(format!(
                    "Could not start the Launcher API helper at {}: {error}. Set {LAUNCHER_BIN_ENV} to a valid nexus-launcher executable.",
                    path.display()
                ));
            }
        }
    }

    fn startup_snapshot(&self, available: bool) -> StartupStatus {
        let helper = self.helper.lock().expect("helper state lock");
        StartupStatus {
            available,
            api_base: self.api_base.clone(),
            helper_path: helper.path.as_ref().map(|path| path.display().to_string()),
            message: if available {
                None
            } else {
                helper.startup_error.clone().or_else(|| {
                    Some(format!(
                        "The Launcher API is not responding at {}. Build nexus-launcher or set {LAUNCHER_BIN_ENV}.",
                        self.api_base
                    ))
                })
            },
        }
    }

    fn stop_helper(&self) {
        let mut helper = self.helper.lock().expect("helper state lock");
        if let Some(mut child) = helper.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn api_port() -> u16 {
    env::var(API_PORT_OVERRIDE_ENV)
        .ok()
        .or_else(|| env::var(API_PORT_ENV).ok())
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(DEFAULT_API_PORT)
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
        "/launcher/status" | "/launcher/logs" | "/v1/health" | "/v1/state" => method == Method::GET,
        "/launcher/agent" | "/launcher/harness" | "/v1/harness" | "/v1/checkpoints"
        | "/v1/diagnostics" => *method == Method::GET || *method == Method::POST,
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
            "/v1/harness" => matches!(action, "start" | "stop" | "restart" | "status"),
            "/v1/checkpoints" => action == "create",
            "/v1/diagnostics" => action == "collect",
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
    let available = state
        .client
        .get(format!("{}/launcher/status", state.api_base))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false);
    if !available {
        state.start_helper();
    }
    let available_after_start = state
        .client
        .get(format!("{}/launcher/status", state.api_base))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(available);
    Ok(state.startup_snapshot(available || available_after_start))
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
    let url = format!("{}{path}", state.api_base);
    let mut request = state.client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Launcher API request failed: {error}"))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROXY_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "Launcher API response exceeds the {MAX_PROXY_RESPONSE_BYTES}-byte limit"
        ));
    }
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Launcher API response could not be read: {error}"))?
    {
        if bytes.len() + chunk.len() > MAX_PROXY_RESPONSE_BYTES {
            return Err(format!(
                "Launcher API response exceeds the {MAX_PROXY_RESPONSE_BYTES}-byte limit"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
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

    #[test]
    fn proxy_allowlist_rejects_external_and_query_routes() {
        assert!(validate_proxy_path("/launcher/status").is_ok());
        assert!(validate_proxy_path("https://example.com").is_err());
        assert!(validate_proxy_path("/v1/health?url=https://example.com").is_err());
        assert!(validate_proxy_path("/v1/health/../config").is_err());
    }

    #[test]
    fn proxy_methods_and_body_size_are_bounded() {
        assert!(validate_proxy_request("/v1/health", &Method::GET, None).is_ok());
        assert!(validate_proxy_request(
            "/v1/health",
            &Method::POST,
            Some(&serde_json::json!({
                "action": "status"
            }))
        )
        .is_err());
        assert!(
            validate_proxy_request("/v1/state", &Method::GET, Some(&serde_json::json!({})))
                .is_err()
        );
        assert!(validate_proxy_request(
            "/v1/harness",
            &Method::POST,
            Some(&serde_json::json!({ "action": "status" }))
        )
        .is_ok());
        assert!(validate_proxy_request(
            "/v1/config",
            &Method::POST,
            Some(&serde_json::json!({ "action": "set_harness" }))
        )
        .is_err());
        assert!(validate_proxy_request(
            "/v1/harness",
            &Method::POST,
            Some(&serde_json::json!({ "action": "set_config" }))
        )
        .is_err());
        let oversized = Value::String("x".repeat(MAX_PROXY_BODY_BYTES));
        assert!(validate_proxy_request("/v1/harness", &Method::POST, Some(&oversized)).is_err());
    }

    #[test]
    fn only_loopback_api_base_is_constructed() {
        let base = format!("http://127.0.0.1:{}", DEFAULT_API_PORT);
        let url = Url::parse(&base).expect("loopback API URL parses");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(DEFAULT_API_PORT));
        assert_eq!(StatusCode::OK.as_u16(), 200);
    }
}
