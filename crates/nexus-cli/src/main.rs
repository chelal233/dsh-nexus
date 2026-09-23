use std::{env, net::SocketAddr, process};

use nexus_core::{NexusConfig, DEFAULT_AGENT_PORT, PORT_ENV, AgentDiscoveryRecord, data_root_identity};
use nexus_protocol::{
    CheckpointAction, CheckpointCommand, CheckpointCreateResponse, CheckpointListResponse,
    CheckpointRestoreResponse, ConfigAction, ConfigCommand, ConfigResponse, DiagnosticsAction,
    DiagnosticsCommand, DiagnosticsResponse, ErrorResponse, HarnessAction, HarnessCommand,
    HarnessResponse, PluginRemoveResponse, ProfileAction, ProfileCommand, ProfileListResponse,
    ProfileSelectResponse, RecoveryStatusResponse, ReleaseAction, ReleaseCommand,
    ReleaseListResponse, RuntimeConfigPayload, RuntimeInstallMode, RuntimeOwnership,
    RuntimePinPayload, RuntimeSource, SnapshotDetailResponse, SnapshotInspectionPayload,
    StateResponse, UpdateAction, UpdateCommand, UpdateResponse,
};

#[derive(Debug)]
struct Options {
    command: Command,
    json: bool,
    config: NexusConfig,
    explicit_port: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Status,
    Harness(HarnessAction),
    HarnessStartup(Option<String>),
    Profile(ProfileAction, Option<String>),
    ProfileRemove(String, String),
    Recovery,
    Checkpoint(CheckpointAction, Option<String>, Option<String>),
    Release(
        ReleaseAction,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    Update(
        UpdateAction,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<RuntimeSource>,
        Option<RuntimeInstallMode>,
        // `update confirm OPERATION_ID TOKEN` keeps the two values in
        // separate slots instead of a packed string.
        Option<String>,
    ),
    Diagnostics(DiagnosticsAction, Option<String>),
    Config(ConfigAction, Option<RuntimeConfigPayload>),
}

#[tokio::main]
async fn main() {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(message) => {
            eprintln!("nexusctl: {message}");
            eprintln!("use --help for usage");
            process::exit(2);
        }
    };

    if let Err(message) = run(options).await {
        eprintln!("nexusctl: {message}");
        process::exit(1);
    }
}

fn parse_args() -> Result<Option<Options>, String> {
    parse_args_from(NexusConfig::from_env(), valid_environment_port(env::var(PORT_ENV).ok().as_deref()).is_some(), env::args_os().skip(1))
}

fn valid_environment_port(value: Option<&str>) -> Option<u16> {
    value.and_then(|value| value.parse::<u16>().ok()).filter(|port| *port != 0)
}

fn parse_args_from(mut config: NexusConfig, mut explicit_port: bool, args: impl Iterator<Item = std::ffi::OsString>) -> Result<Option<Options>, String> {
    let mut command = None;
    let mut json = false;
    let mut args = args.peekable();

    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "status" if command.is_none() => command = Some(Command::Status),
            "harness" if command.is_none() => {
                let action = args
                    .next()
                    .ok_or_else(|| "harness requires status, start, stop, or restart".to_owned())?;
                if action=="startup-status" {command=Some(Command::HarnessStartup(None));continue;}
                if action=="cancel-start" {command=Some(Command::HarnessStartup(Some(args.next().ok_or("cancel-start requires OPERATION_ID")?.to_string_lossy().into_owned())));continue;}
                let action = match action.to_string_lossy().as_ref() {
                    "status" => HarnessAction::Status,
                    "start" => HarnessAction::Start,
                    "stop" => HarnessAction::Stop,
                    "restart" => HarnessAction::Restart,
                    value => return Err(format!("unknown harness action: {value}")),
                };
                command = Some(Command::Harness(action));
            }
            "profile" if command.is_none() => {
                let action = args.next().ok_or_else(|| {
                    "profile requires status, list, select NAME, or remove NAME PACKAGE".to_owned()
                })?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => ProfileAction::Status,
                    "list" => ProfileAction::List,
                    "select" => ProfileAction::Select,
                    "remove" => ProfileAction::PluginRemove,
                    value => return Err(format!("unknown profile action: {value}")),
                };
                let name = if matches!(action, ProfileAction::Select | ProfileAction::PluginRemove)
                {
                    Some(
                        args.next()
                            .ok_or_else(|| "profile action requires NAME".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else {
                    None
                };
                if action == ProfileAction::PluginRemove {
                    let package = args
                        .next()
                        .ok_or_else(|| "profile remove requires PACKAGE".to_owned())?
                        .to_string_lossy()
                        .into_owned();
                    command = Some(Command::ProfileRemove(
                        name.expect("profile name parsed"),
                        package,
                    ));
                } else {
                    command = Some(Command::Profile(action, name));
                }
            }
            "recovery" if command.is_none() => command = Some(Command::Recovery),
            "checkpoint" if command.is_none() => {
                let action = args
                    .next()
                    .ok_or_else(|| "checkpoint requires list, create, detail ID, inspect ID, restore ID, retry [ID], or abort [ID]".to_owned())?;
                let action = match action.to_string_lossy().as_ref() {
                    "list" => CheckpointAction::List,
                    "create" => CheckpointAction::Create,
                    "detail" => CheckpointAction::Detail,
                    "inspect" => CheckpointAction::Inspect,
                    "restore" => CheckpointAction::Restore,
                    "retry" => CheckpointAction::Retry,
                    "abort" => CheckpointAction::Abort,
                    value => return Err(format!("unknown checkpoint action: {value}")),
                };
                let id = if matches!(
                    action,
                    CheckpointAction::Detail
                        | CheckpointAction::Inspect
                        | CheckpointAction::Restore
                ) {
                    Some(
                        args.next()
                            .ok_or_else(|| "checkpoint action requires ID".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else if matches!(action, CheckpointAction::Retry | CheckpointAction::Abort) {
                    match args.peek() {
                        Some(value) if !value.to_string_lossy().starts_with('-') => Some(
                            args.next()
                                .expect("peeked checkpoint id")
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        _ => None,
                    }
                } else {
                    None
                };
                command = Some(Command::Checkpoint(action, id, None));
            }
            "release" if command.is_none() => {
                let action = args.next().ok_or_else(|| {
                    "release requires list, current, register ID VERSION, promote ID, or rollback"
                        .to_owned()
                })?;
                let action = match action.to_string_lossy().as_ref() {
                    "list" => ReleaseAction::List,
                    "current" => ReleaseAction::Current,
                    "register" => ReleaseAction::Register,
                    "promote" => ReleaseAction::Promote,
                    "rollback" => ReleaseAction::Rollback,
                    value => return Err(format!("unknown release action: {value}")),
                };
                let id = match action {
                    ReleaseAction::Register | ReleaseAction::Promote => Some(
                        args.next()
                            .ok_or_else(|| "release action requires ID".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    _ => None,
                };
                let version = if action == ReleaseAction::Register {
                    Some(
                        args.next()
                            .ok_or_else(|| "release register requires VERSION".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else {
                    None
                };
                command = Some(Command::Release(action, id, version, None, None));
            }
            "update" if command.is_none() => {
                let action = args.next().ok_or_else(|| {
                    "update requires status, install, switch, confirm, or cancel".to_owned()
                })?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => UpdateAction::Status,
                    "install" => UpdateAction::Install,
                    "switch" => UpdateAction::Switch,
                    "confirm" => UpdateAction::Confirm,
                    "cancel" => UpdateAction::Cancel,
                    value => return Err(format!("unknown update action: {value}")),
                };
                let (release_id, version) = if action == UpdateAction::Install {
                    let id = match args.peek() {
                        Some(value) if !value.to_string_lossy().starts_with('-') => args.next(),
                        _ => None,
                    };
                    let version = match id {
                        Some(id) => {
                            let version = match args.peek() {
                                Some(value) if !value.to_string_lossy().starts_with('-') => {
                                    args.next()
                                }
                                _ => None,
                            };
                            let Some(version) = version else {
                                return Err("update install accepts either no positional values or ID VERSION".to_owned());
                            };
                            (
                                Some(id.to_string_lossy().into_owned()),
                                Some(version.to_string_lossy().into_owned()),
                            )
                        }
                        None => (None, None),
                    };
                    version
                } else {
                    (None, None)
                };
                let (third, source, mode, confirm_token) = match action {
                    UpdateAction::Switch => {
                        let tag = args
                            .next()
                            .ok_or_else(|| "update switch requires TAG".to_owned())?
                            .to_string_lossy()
                            .into_owned();
                        let source = match args.peek().map(|value| value.to_string_lossy()) {
                            Some(value) if value == "npmmirror" => {
                                args.next();
                                RuntimeSource::Npmmirror
                            }
                            Some(value) if value == "official" => {
                                args.next();
                                RuntimeSource::Official
                            }
                            _ => RuntimeSource::Official,
                        };
                        let mode = match args.peek().map(|value| value.to_string_lossy()) {
                            Some(value) if value == "system" => {
                                args.next();
                                RuntimeInstallMode::System
                            }
                            Some(value) if value == "portable" => {
                                args.next();
                                RuntimeInstallMode::Portable
                            }
                            _ => RuntimeInstallMode::Portable,
                        };
                        (Some(tag), Some(source), Some(mode), None)
                    }
                    UpdateAction::Confirm => {
                        let operation = args
                            .next()
                            .ok_or_else(|| "update confirm requires OPERATION_ID TOKEN".to_owned())?
                            .to_string_lossy()
                            .into_owned();
                        let token = args
                            .next()
                            .ok_or_else(|| "update confirm requires OPERATION_ID TOKEN".to_owned())?
                            .to_string_lossy()
                            .into_owned();
                        (Some(operation), None, None, Some(token))
                    }
                    UpdateAction::Cancel => (
                        Some(
                            args.next()
                                .ok_or_else(|| "update cancel requires OPERATION_ID".to_owned())?
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        None,
                        None,
                        None,
                    ),
                    _ => (None, None, None, None),
                };
                command = Some(Command::Update(
                    action, release_id, version, third, source, mode, confirm_token,
                ));
            }
            "diagnostics" if command.is_none() => {
                let action = args
                    .next()
                    .ok_or_else(|| "diagnostics requires status or collect".to_owned())?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => DiagnosticsAction::Status,
                    "collect" => DiagnosticsAction::Collect,
                    value => return Err(format!("unknown diagnostics action: {value}")),
                };
                command = Some(Command::Diagnostics(action, None));
            }
            "config" if command.is_none() => {
                let action = args.next().ok_or_else(|| {
                    "config requires status, set-runtime, clear-runtime, clear-harness, or clear-update".to_owned()
                })?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => ConfigAction::Status,
                    "clear-harness" => ConfigAction::ClearHarness,
                    "clear-update" => ConfigAction::ClearUpdate,
                    "set-runtime" => ConfigAction::SetRuntime,
                    "clear-runtime" => ConfigAction::ClearRuntime,
                    value => return Err(format!("unknown config action: {value}")),
                };
                let runtime =
                    (action == ConfigAction::SetRuntime).then(RuntimeConfigPayload::default);
                command = Some(Command::Config(action, runtime));
            }
            "--node" | "--pnpm" | "--git" => {
                let tool = argument
                    .to_string_lossy()
                    .trim_start_matches("--")
                    .to_owned();
                let ownership = args
                    .next()
                    .ok_or_else(|| format!("--{tool} requires system|nexus PATH"))?;
                let ownership = match ownership.to_string_lossy().as_ref() {
                    "system" => RuntimeOwnership::System,
                    "nexus" => RuntimeOwnership::Nexus,
                    value => return Err(format!("invalid --{tool} ownership: {value}")),
                };
                let path = args
                    .next()
                    .ok_or_else(|| format!("--{tool} requires system|nexus PATH"))?
                    .to_string_lossy()
                    .into_owned();
                let pin = RuntimePinPayload { path, ownership };
                match command.as_mut() {
                    Some(Command::Config(ConfigAction::SetRuntime, Some(runtime))) => {
                        match tool.as_str() {
                            "node" => runtime.node = Some(pin),
                            "pnpm" => runtime.pnpm = Some(pin),
                            "git" => runtime.git = Some(pin),
                            _ => unreachable!(),
                        }
                    }
                    _ => return Err(format!("--{tool} is valid only after config set-runtime")),
                }
            }
            "--runtime-source" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--runtime-source requires official|npmmirror".to_owned())?;
                let source = match value.to_string_lossy().as_ref() {
                    "official" => RuntimeSource::Official,
                    "npmmirror" => RuntimeSource::Npmmirror,
                    value => return Err(format!("invalid runtime source: {value}")),
                };
                match command.as_mut() {
                    Some(Command::Config(ConfigAction::SetRuntime, Some(runtime))) => {
                        runtime.source = source
                    }
                    _ => {
                        return Err(
                            "--runtime-source is valid only after config set-runtime".to_owned()
                        )
                    }
                }
            }
            "--runtime-mode" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--runtime-mode requires portable|system".to_owned())?;
                let mode = match value.to_string_lossy().as_ref() {
                    "portable" => RuntimeInstallMode::Portable,
                    "system" => RuntimeInstallMode::System,
                    value => return Err(format!("invalid runtime mode: {value}")),
                };
                match command.as_mut() {
                    Some(Command::Config(ConfigAction::SetRuntime, Some(runtime))) => {
                        runtime.mode = mode
                    }
                    _ => {
                        return Err(
                            "--runtime-mode is valid only after config set-runtime".to_owned()
                        )
                    }
                }
            }
            "--json" => json = true,
            "--note" => {
                let note = args
                    .next()
                    .ok_or_else(|| "--note requires TEXT".to_owned())?
                    .to_string_lossy()
                    .into_owned();
                match command.as_mut() {
                    Some(Command::Checkpoint(CheckpointAction::Create, _, current)) => {
                        *current = Some(note);
                    }
                    Some(Command::Release(ReleaseAction::Register, _, _, _, current)) => {
                        *current = Some(note);
                    }
                    Some(Command::Diagnostics(DiagnosticsAction::Collect, current)) => {
                        *current = Some(note);
                    }
                    _ => {
                        return Err("--note is only valid for checkpoint create, release register, or diagnostics collect".to_owned())
                    }
                }
            }
            "--source" => {
                let source = args
                    .next()
                    .ok_or_else(|| "--source requires TEXT".to_owned())?
                    .to_string_lossy()
                    .into_owned();
                match command.as_mut() {
                    Some(Command::Release(ReleaseAction::Register, _, _, current, _)) => {
                        *current = Some(source);
                    }
                    _ => return Err("--source is only valid for release register".to_owned()),
                }
            }
            "--port" => {
                explicit_port = true;
                let value = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_owned())?;
                config.port = value
                    .to_string_lossy()
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| format!("invalid port: {}", value.to_string_lossy()))?;
            }
            "--help" | "-h" => {
                print_help_v2();
                return Ok(None);
            }
            value => return Err(format!("unknown argument: {value}")),
        }
    }

    let Some(command) = command else {
        return Err(
            "a command is required (supported: status, harness, profile, checkpoint, release, update, diagnostics, config)".to_owned(),
        );
    };

    Ok(Some(Options {
        command,
        json,
        config,
        explicit_port,
    }))
}

fn local_client(headers: reqwest::header::HeaderMap) -> Result<reqwest::Client, String> {
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
        .default_headers(headers).build().map_err(|error| format!("cannot create the local Agent client: {error}"))
}

async fn resolve_agent_client(config: &NexusConfig, explicit_port: bool) -> Result<(reqwest::Client, SocketAddr, nexus_protocol::HealthResponse), String> {
    let paths = config.paths();
    // An explicit port chooses an address, never a different data owner.
    let root_identity = Some(data_root_identity(&paths)
        .map_err(|error| format!("cannot identify Nexus data directory: {error}"))?);
    let discovery = if explicit_port { None } else {
        let path = paths.agent_discovery_file();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 64 * 1024 {
                    return Err("Agent discovery record is unsafe or too large".into());
                }
                use std::io::Read;
                let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
                let mut bytes = Vec::new();
                file.take(64 * 1024 + 1).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
                if bytes.len() > 64 * 1024 { return Err("Agent discovery record is too large".into()); }
                let record: AgentDiscoveryRecord = serde_json::from_slice(&bytes).map_err(|error| format!("invalid Agent discovery record: {error}"))?;
                if record.port == 0 || record.instance_id.is_empty() || Some(&record.data_root_id) != root_identity.as_ref() {
                    return Err("Agent discovery record does not belong to this data directory".into());
                }
                Some(record)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("cannot read Agent discovery: {error}")),
        }
    };
    let port = discovery.as_ref().map(|record| record.port).unwrap_or(if config.port == 0 { DEFAULT_AGENT_PORT } else { config.port });
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let client = local_client(reqwest::header::HeaderMap::new())?;
    let mut response = client.get(format!("http://{address}/v1/health")).timeout(std::time::Duration::from_secs(5))
        .send().await.map_err(|error| format!("Agent health is unavailable: {error}"))?;
    if !response.status().is_success() { return Err(format!("Agent health returned HTTP {}", response.status())); }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len() + chunk.len() > 64 * 1024 { return Err("Agent health response is too large".into()); }
        bytes.extend_from_slice(&chunk);
    }
    let health: nexus_protocol::HealthResponse = serde_json::from_slice(&bytes).map_err(|error| format!("invalid Agent health: {error}"))?;
    if health.api_version != nexus_protocol::API_VERSION || health.service != "nexus-agent"
        || health.status != nexus_protocol::HealthStatus::Ok || health.instance_id.is_empty() || health.data_root_id.is_empty()
        || root_identity.as_ref().is_some_and(|root| root != &health.data_root_id)
        || discovery.as_ref().is_some_and(|record| record.instance_id != health.instance_id) {
        return Err("Agent health identity does not match the selected Nexus instance".into());
    }
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-nexus-data-root-id", health.data_root_id.clone().parse().map_err(|_| "Invalid Agent data-root identity")?);
    headers.insert("x-nexus-instance-id", health.instance_id.clone().parse().map_err(|_| "Invalid Agent instance identity")?);
    Ok((local_client(headers)?, address, health))
}

fn attach_receipt_id(path: &str, value: &mut serde_json::Value, nonce: &str) -> Option<String> {
    nexus_protocol::request_receipt_kind(path, value.get("action")?.as_str()?)?;
    let id = env::var("NEXUS_REQUEST_ID").unwrap_or_else(|_| format!("{}-{nonce}", nexus_core::agent_auth::unix_seconds()));
    value["request_id"] = serde_json::Value::String(id.clone());
    Some(id)
}

async fn authenticated_response(client: &reqwest::Client, credential: &nexus_core::agent_auth::AgentCredential, request: reqwest::RequestBuilder) -> Result<(reqwest::StatusCode, String), String> {
    use nexus_core::agent_auth as auth;
    let nonce = auth::random_hex().map_err(|error| error.to_string())?;
    let time = auth::unix_seconds().to_string();
    let mut request = request.build().map_err(|error| error.to_string())?;
    if request.method() == reqwest::Method::POST {
        if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(request.body().and_then(|b| b.as_bytes()).unwrap_or_default()) {
            if let Some(id) = attach_receipt_id(request.url().path(), &mut value, &nonce) {
                eprintln!("Request ID: {id} (reuse with NEXUS_REQUEST_ID after a timeout)");
                *request.body_mut() = Some(serde_json::to_vec(&value).map_err(|e|e.to_string())?.into());
            }
        }
    }
    let ciphertext = credential.seal_request(request.method().as_str(), request.url().path(), &nonce, &time, request.body().and_then(|body| body.as_bytes()).unwrap_or_default()).map_err(|_| "Agent request encryption failed")?;
    *request.body_mut() = Some(ciphertext.into());
    request.headers_mut().remove(reqwest::header::CONTENT_LENGTH);
    request.headers_mut().insert(auth::VERSION_HEADER, "2".parse().unwrap());
    let signature = credential.request_signature(request.method().as_str(), request.url().path(), &nonce, &time, request.body().and_then(|body| body.as_bytes()).unwrap_or_default());
    request.headers_mut().insert(auth::NONCE_HEADER, nonce.parse().unwrap());
    request.headers_mut().insert(auth::TIME_HEADER, time.parse().unwrap());
    request.headers_mut().insert(auth::SIGNATURE_HEADER, signature.parse().unwrap());
    let mut response = client.execute(request).await.map_err(|error| format!("Agent is unavailable: {error}"))?;
    let status = response.status();
    let encrypted = response.headers().get(auth::VERSION_HEADER).and_then(|v| v.to_str().ok()) == Some("2");
    let signature = response.headers().get(auth::RESPONSE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len() + chunk.len() > 512 * 1024 + auth::TAG_BYTES { return Err("Agent response is too large".into()); }
        bytes.extend_from_slice(&chunk);
    }
    if !credential.verify_response(&nonce, status.as_u16(), &bytes, &signature) { return Err("Agent response authentication failed".into()); }
    if !encrypted { return Err("Agent response encryption is required".into()); }
    let bytes = credential.open_response(&nonce, status.as_u16(), &bytes).map_err(|_| "Agent response decryption failed")?;
    let body = String::from_utf8(bytes).map_err(|_| "Agent response is not UTF-8")?;
    Ok((status, body))

}

async fn run(options: Options) -> Result<(), String> {
    let (client, address, health) = resolve_agent_client(&options.config, options.explicit_port).await?;
    let credential = nexus_core::agent_auth::AgentCredential::read(&options.config.paths(), &health.data_root_id, &health.instance_id)
        .map_err(|_| "Agent credential is unavailable or invalid; restart Nexus Agent".to_owned())?;
    let expected_revision = if matches!(&options.command, Command::Config(action, _) if *action != ConfigAction::Status) {
        let (status, body) = authenticated_response(&client, &credential, client.get(format!("http://{address}/v1/config"))).await?;
        if !status.is_success() { return Err(format!("Configuration refresh failed: HTTP {status}")); }
        let snapshot: ConfigResponse = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        if snapshot.revision.is_empty() { return Err("Agent did not provide a configuration revision".into()); }
        Some(snapshot.revision)
    } else { None };
    let request = match &options.command {
        Command::Status => client
            .get(format!("http://{address}/v1/state")),
        Command::HarnessStartup(None)=>client.get(format!("http://{address}/v1/harness/startup")),
        Command::HarnessStartup(Some(id))=>client.post(format!("http://{address}/v1/harness/startup")).json(&serde_json::json!({"action":"cancel","operation_id":id})),
        Command::Harness(HarnessAction::Status) => client
            .get(format!("http://{address}/v1/harness")),
        Command::Harness(action) => client
            .post(format!("http://{address}/v1/harness"))
            .json(&HarnessCommand { action: *action }),
        Command::Profile(ProfileAction::List | ProfileAction::Status, _) => client
            .get(format!("http://{address}/v1/profiles")),
        Command::Profile(ProfileAction::Select, profile) => client
            .post(format!("http://{address}/v1/profiles"))
            .json(&ProfileCommand {
                action: ProfileAction::Select,
                profile: profile.clone(),
                package: None,
            
                target: None,}),
        Command::Profile(
            ProfileAction::PluginInventory
            | ProfileAction::PluginMove
            | ProfileAction::PluginUndoMove
            | ProfileAction::PluginRemove
            | ProfileAction::PluginDisable
            | ProfileAction::PluginEnable
            | ProfileAction::CompatibilityCheck
            | ProfileAction::OpenPath
            | ProfileAction::OpenTerminal
            | ProfileAction::Delete
            | ProfileAction::DeletedList
            | ProfileAction::RestoreDeleted
            | ProfileAction::PurgeDeleted
            | ProfileAction::Create,
            _,
        ) => {
            return Err("invalid internal profile command".to_owned())
        }
        Command::ProfileRemove(profile, package) => client
            .post(format!("http://{address}/v1/profiles"))
            .json(&ProfileCommand {
                action: ProfileAction::PluginRemove,
                profile: Some(profile.clone()),
                package: Some(package.clone()),
            
                target: None,}),
        Command::Recovery => client
            .get(format!("http://{address}/v1/recovery")),
        Command::Checkpoint(CheckpointAction::List, _, _) => client
            .get(format!("http://{address}/v1/checkpoints")),
        Command::Checkpoint(action, id, note) => client
            .post(format!("http://{address}/v1/checkpoints"))
            .json(&CheckpointCommand {
                action: *action,
                id: id.clone(),
                note: note.clone(),
            }),
        Command::Release(ReleaseAction::List | ReleaseAction::Current, _, _, _, _) => client
            .get(format!("http://{address}/v1/releases")),
        Command::Release(action, id, version, source, note) => client
            .post(format!("http://{address}/v1/releases"))
            .json(&ReleaseCommand {
                action: *action,
                id: id.clone(),
                version: version.clone(),
                source: source.clone(),
                note: note.clone(),
                ..ReleaseCommand::default()
            }),
        Command::Update(UpdateAction::Status, _, _, _, _, _, _) => client
            .get(format!("http://{address}/v1/updates")),
        Command::Update(action, release_id, version, third, source, mode, confirm_token) => client
            .post(format!("http://{address}/v1/updates"))
            .json(&UpdateCommand {
                offline_contents: None,
                archive_path: None,
                action: *action,
                release_id: release_id.clone(),
                version: version.clone(),

                tag: if *action == UpdateAction::Switch {
                    third.clone()
                } else {
                    None
                },
                source: *source,
                mode: *mode,
                operation_id: match action {
                    UpdateAction::Confirm | UpdateAction::Cancel => third.clone(),
                    _ => None,
                },
                confirmation: if *action == UpdateAction::Confirm {
                    confirm_token.clone()
                } else {
                    None
                },
            }),
        Command::Diagnostics(DiagnosticsAction::Status, _) => client
            .get(format!("http://{address}/v1/diagnostics")),
        Command::Diagnostics(action, note) => client
            .post(format!("http://{address}/v1/diagnostics"))
            .json(&DiagnosticsCommand {
                action: *action,
                note: note.clone(),
                ..Default::default()
            }),
        Command::Config(ConfigAction::Status, _) => client
            .get(format!("http://{address}/v1/config")),
        Command::Config(action, runtime) => client
            .post(format!("http://{address}/v1/config"))
            .json(&ConfigCommand {
                external_harness_path: None,
                patch_query: None,
                expected_revision,
                harness_preferences: None,
                action: *action,
                harness: None,
                update: None,
                runtime: runtime.clone(),
                snapshots: None,
                preserve_harness_readiness_url: false,
            }),
    };
    let (status, body) = authenticated_response(&client, &credential, request).await?;

    if !status.is_success() {
        if let Ok(error) = serde_json::from_str::<ErrorResponse>(&body) {
            return Err(format!(
                "agent returned HTTP {status}: {} ({})",
                error.message, error.code
            ));
        }
        return Err(format!("agent returned HTTP {status}: {body}"));
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
        if value.get("request").is_some() { println!("{}", serde_json::to_string_pretty(&value).map_err(|e|e.to_string())?); return Ok(()); }
    }
    match &options.command {
        Command::Status => {
            let state: StateResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_state_json(&state)?;
            } else {
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
        }
        Command::HarnessStartup(_) => { println!("{body}"); },
        Command::Harness(_) => {
            let harness: HarnessResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_harness_json(&harness)?;
            } else {
                print_harness(&harness);
            }
        }
        Command::Profile(action, _) => {
            if *action == ProfileAction::Select {
                let profile: ProfileSelectResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&profile).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    println!("profile selected: {}", profile.active_profile);
                }
            } else {
                let profiles: ProfileListResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&profiles).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    println!("active_profile: {}", profiles.active_profile);
                    for name in &profiles.profiles {
                        let marker = if name == &profiles.active_profile {
                            "*"
                        } else {
                            " "
                        };
                        println!("{marker} {name}");
                    }
                }
            }
        }
        Command::ProfileRemove(_, _) => {
            let result: PluginRemoveResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&result).map_err(|error| error.to_string())?,
                )?;
            } else {
                println!(
                    "plugin {} removed={} exit_code={}",
                    result.package,
                    result.removed,
                    result
                        .exit_code
                        .map_or_else(|| "<none>".to_owned(), |code| code.to_string())
                );
                if !result.stderr.is_empty() {
                    eprintln!("{}", result.stderr);
                }
            }
        }
        Command::Recovery => {
            let recovery: RecoveryStatusResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&recovery).map_err(|error| error.to_string())?,
                )?;
            } else {
                println!(
                    "manual_entry_available: {}",
                    recovery.manual_entry_available
                );
                println!("harness_stop_required: {}", recovery.harness_stop_required);
                println!("fatal_prefix_observed: {}", recovery.fatal_prefix_observed);
            }
        }
        Command::Checkpoint(action, _, _) => match action {
            CheckpointAction::List => {
                let checkpoints: CheckpointListResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&checkpoints).map_err(|error| error.to_string())?,
                    )?;
                } else if checkpoints.checkpoints.is_empty() {
                    println!("no checkpoints");
                } else {
                    for checkpoint in &checkpoints.checkpoints {
                        let content = checkpoint
                            .snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.snapshot_id.as_str())
                            .unwrap_or("legacy-metadata-only");
                        println!(
                            "{} profile={} created_at_unix={} content={}",
                            checkpoint.id, checkpoint.profile, checkpoint.created_at_unix, content
                        );
                    }
                    if let Some(pending) = &checkpoints.pending_restore {
                        println!(
                            "pending_restore={} state={:?} retryable={} abortable={}",
                            pending.checkpoint_id,
                            pending.state,
                            pending.retryable,
                            pending.abortable
                        );
                    }
                }
            }
            CheckpointAction::Create => {
                let checkpoint: CheckpointCreateResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&checkpoint).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    println!("created checkpoint: {}", checkpoint.checkpoint.id);
                }
            }
            CheckpointAction::Detail => {
                let detail: SnapshotDetailResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&detail).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    println!(
                        "snapshot {} profile={} files={} bytes={}",
                        detail.summary.snapshot_id,
                        detail.summary.profile_name,
                        detail.summary.file_count,
                        detail.summary.total_bytes
                    );
                    for file in detail.files {
                        println!(
                            "{:?} {} stored_bytes={}",
                            file.state, file.path, file.stored_size
                        );
                    }
                }
            }
            CheckpointAction::Inspect => {
                let inspection: SnapshotInspectionPayload = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&inspection).map_err(|error| error.to_string())?,
                    )?;
                } else if inspection.valid {
                    println!("snapshot valid: {}", inspection.snapshot_id);
                } else {
                    println!("snapshot invalid: {}", inspection.snapshot_id);
                    for error in inspection.errors {
                        println!("  {error}");
                    }
                }
            }
            CheckpointAction::Restore | CheckpointAction::Retry | CheckpointAction::Abort => {
                let checkpoint: CheckpointRestoreResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&checkpoint).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    if checkpoint.restored {
                        println!("restored checkpoint: {}", checkpoint.checkpoint.id);
                    } else {
                        println!(
                            "checkpoint {} content state: {:?}",
                            checkpoint.checkpoint.id, checkpoint.content_state
                        );
                    }
                }
            }
        },
        Command::Release(_, _, _, _, _) => {
            let releases: ReleaseListResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&releases).map_err(|error| error.to_string())?,
                )?;
            } else {
                println!(
                    "current_release: {}",
                    releases.current_release.as_deref().unwrap_or("<none>")
                );
                println!(
                    "last_known_good: {}",
                    releases.last_known_good.as_deref().unwrap_or("<none>")
                );
                if releases.releases.is_empty() {
                    println!("no releases");
                } else {
                    for release in &releases.releases {
                        let mut markers = String::new();
                        if releases.current_release.as_deref() == Some(release.id.as_str()) {
                            markers.push('*');
                        }
                        if releases.last_known_good.as_deref() == Some(release.id.as_str()) {
                            markers.push('L');
                        }
                        if markers.is_empty() {
                            markers.push(' ');
                        }
                        println!(
                            "{markers} {} version={} installed_at_unix={}",
                            release.id, release.version, release.installed_at_unix
                        );
                    }
                }
            }
        }
        Command::Update(_, _, _, _, _, _, _) => {
            let update: UpdateResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&update).map_err(|error| error.to_string())?,
                )?;
            } else {
                println!("update_state: {:?}", update.update.state);
                println!(
                    "release_id: {}",
                    update.update.release_id.as_deref().unwrap_or("<none>")
                );
                if let Some(release) = update.release {
                    println!("release_version: {}", release.version);
                }
                if let Some(error) = update.update.error {
                    println!("error: {error}");
                }
                if let Some(operation) = update.operation {
                    println!("operation_id: {}", operation.operation_id);
                    println!("operation_phase: {:?}", operation.phase);
                    println!("tag: {}", operation.tag);
                    println!("progress_percent: {}", operation.progress_percent);
                    if let Some(confirmation) = operation.confirmation {
                        println!("confirmation: {confirmation}");
                    }
                    if let Some(error) = operation.error {
                        println!("operation_error: {error}");
                    }
                }
            }
        }
        Command::Diagnostics(_, _) => {
            let diagnostics: DiagnosticsResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&diagnostics).map_err(|error| error.to_string())?,
                )?;
            } else if diagnostics.bundles.is_empty() {
                println!("no diagnostics bundles");
            } else {
                for bundle in &diagnostics.bundles {
                    println!(
                        "{} created_at_unix={} files={} directory={}",
                        bundle.id,
                        bundle.created_at_unix,
                        bundle.files.len(),
                        bundle.directory
                    );
                    for file in &bundle.files {
                        let mut flags = String::new();
                        if file.redacted {
                            flags.push('R');
                        }
                        if file.truncated {
                            flags.push('T');
                        }
                        println!("  {} bytes={} {}", file.name, file.bytes, flags);
                    }
                }
            }
        }
        Command::Config(_, _) => {
            let config: ConfigResponse = serde_json::from_str(&body)
                .map_err(|error| format!("invalid agent response: {error}"))?;
            if options.json {
                print_json_value(
                    &serde_json::to_value(&config).map_err(|error| error.to_string())?,
                )?;
            } else {
                match config.harness {
                    Some(harness) => {
                        println!("harness_configured: yes");
                        println!("harness_program: {}", harness.program);
                        println!("harness_args: {}", harness.args.len());
                        println!(
                            "harness_working_dir: {}",
                            harness.working_dir.as_deref().unwrap_or("<default>")
                        );
                    }
                    None => println!("harness_configured: no"),
                }
                match config.update {
                    Some(update) => {
                        println!("update_configured: yes");
                        println!("update_source: {}", update.source);
                        println!("update_ref: {}", update.ref_name);
                    }
                    None => println!("update_configured: no"),
                }
                match config.runtime {
                    Some(runtime) => {
                        println!("runtime_configured: yes");
                        println!("runtime_source: {:?}", runtime.source);
                        println!("runtime_mode: {:?}", runtime.mode);
                        for (name, pin) in [
                            ("node", runtime.node),
                            ("pnpm", runtime.pnpm),
                            ("git", runtime.git),
                        ] {
                            if let Some(pin) = pin {
                                println!("runtime_{name}: {:?} {}", pin.ownership, pin.path);
                            }
                        }
                    }
                    None => println!("runtime_configured: no"),
                }
            }
        }
    }

    Ok(())
}

fn print_state_json(value: &StateResponse) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("failed to encode JSON: {error}"))?
    );
    Ok(())
}

fn print_harness_json(value: &HarnessResponse) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("failed to encode JSON: {error}"))?
    );
    Ok(())
}

fn print_harness(response: &HarnessResponse) {
    println!("state: {:?}", response.harness.state);
    println!(
        "pid: {}",
        response
            .harness
            .pid
            .map_or_else(|| "<none>".to_owned(), |pid| pid.to_string())
    );
    println!(
        "exit_code: {}",
        response
            .harness
            .exit_code
            .map_or_else(|| "<none>".to_owned(), |code| code.to_string())
    );
    println!(
        "error: {}",
        response.harness.error.as_deref().unwrap_or("<none>")
    );
}

fn print_json_value(value: &serde_json::Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("failed to encode JSON: {error}"))?
    );
    Ok(())
}

fn print_help_v2() {
    println!(
        r#"nexusctl

Usage:
  nexusctl status [--json] [--port PORT]
  nexusctl harness status|start|stop|restart [--json] [--port PORT]
  nexusctl profile status|list [--json] [--port PORT]
  nexusctl profile select NAME [--json] [--port PORT]
  nexusctl profile remove NAME PACKAGE [--json] [--port PORT]
  nexusctl recovery [--json] [--port PORT]
  nexusctl checkpoint list [--json] [--port PORT]
  nexusctl checkpoint create [--note TEXT] [--json] [--port PORT]
  nexusctl checkpoint detail ID [--json] [--port PORT]
  nexusctl checkpoint inspect ID [--json] [--port PORT]
  nexusctl checkpoint restore ID [--json] [--port PORT]
  nexusctl checkpoint retry [ID] [--json] [--port PORT]
  nexusctl checkpoint abort [ID] [--json] [--port PORT]
  nexusctl release list|current [--json] [--port PORT]
  nexusctl release register ID VERSION [--source TEXT] [--note TEXT] [--json] [--port PORT]
  nexusctl release promote ID [--json] [--port PORT]
  nexusctl harness startup-status [--json] [--port PORT]
  nexusctl harness cancel-start OPERATION_ID [--json] [--port PORT]
  nexusctl release rollback [--json] [--port PORT]
  nexusctl update status [--json] [--port PORT]
  nexusctl update install [ID VERSION] [--json] [--port PORT]
  nexusctl update switch TAG [official|npmmirror] [portable|system] [--json] [--port PORT]
  nexusctl update confirm OPERATION_ID TOKEN [--json] [--port PORT]
  nexusctl update cancel OPERATION_ID [--json] [--port PORT]
  nexusctl diagnostics status [--json] [--port PORT]
  nexusctl diagnostics collect [--note TEXT] [--json] [--port PORT]
  nexusctl config status [--json] [--port PORT]
  nexusctl config clear-harness|clear-update|clear-runtime [--json] [--port PORT]
  nexusctl config set-runtime [--node OWNER PATH] [--pnpm OWNER PATH] [--git OWNER PATH]
      [--runtime-source official|npmmirror] [--runtime-mode portable|system]
      [--json] [--port PORT]

Queries and controls the loopback Nexus Agent API."#
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read, Write}, net::TcpListener, fs};

    #[test]
    fn cli_injection_matches_the_agent_receipt_contract() {
        let paths = ["/v1/harness", "/v1/releases", "/v1/updates", "/v1/checkpoints", "/v1/profiles", "/v1/config"];
        let actions = ["restart", "rollback", "switch", "offline_import", "restore", "delete", "restore_deleted", "status"];
        let mut controlled = 0;
        for path in paths { for action in actions {
            let mut body = serde_json::json!({"action":action,"profile":"web"});
            let required = nexus_protocol::request_receipt_kind(path, action).is_some();
            let id = attach_receipt_id(path, &mut body, "0123456789abcdef0123456789abcdef");
            assert_eq!(id.is_some(), required, "{path} {action}");
            assert_eq!(body.get("request_id").and_then(|v| v.as_str()), id.as_deref());
            controlled += usize::from(required);
        } }
        assert_eq!(controlled, 7);
        for action in ["delete", "restore_deleted"] {
            assert!(parse_args_from(NexusConfig::default(), false, ["profile", action, "web"].into_iter().map(std::ffi::OsString::from)).is_err());
        }
    }

    fn fixture(label: &str) -> (NexusConfig, TcpListener) {
        let root = env::temp_dir().join(format!("nexus-cli-{label}-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        (NexusConfig { data_dir: Some(root), port: listener.local_addr().unwrap().port() }, listener)
    }
    fn respond(listener: TcpListener, body: String, expect_command: bool) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut bytes = [0; 8192];
            let n = stream.read(&mut bytes).unwrap();
            assert!(String::from_utf8_lossy(&bytes[..n]).starts_with("GET /v1/health"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            if expect_command {
                let (mut stream, _) = listener.accept().unwrap();
                let n = stream.read(&mut bytes).unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                assert!(request.starts_with("POST /v1/harness"));
                assert!(request.contains("x-nexus-instance-id: fixture"));
                assert!(request.contains("x-nexus-data-root-id:"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
            }
        })
    }
    fn health(root: &str, instance: &str) -> String {
        serde_json::json!({"api_version":"v1","service":"nexus-agent","status":"ok","data_root_id":root,"instance_id":instance}).to_string()
    }
    #[test]
    fn explicit_ports_include_valid_environment_and_default_port() {
        assert_eq!(valid_environment_port(Some("0")), None);
        assert_eq!(valid_environment_port(Some("bad")), None);
        assert_eq!(valid_environment_port(Some("9800")), Some(9800));
        let parse = |args: &[&str], explicit| parse_args_from(NexusConfig::default(), explicit, args.iter().map(std::ffi::OsString::from)).unwrap().unwrap();
        assert!(parse(&["status", "--port", "9800"], false).explicit_port);
        assert!(parse(&["status"], true).explicit_port);
        assert!(!parse(&["status"], false).explicit_port);
    }
    #[tokio::test]
    async fn discovers_current_root_and_binds_mutations() {
        let (mut config, listener) = fixture("discovery");
        let port = config.port;
        config.port = 1;
        let paths = config.paths();
        let root = data_root_identity(&paths).unwrap();
        paths.publish_agent_discovery(&AgentDiscoveryRecord { port, data_root_id: root.clone(), instance_id: "fixture".into(), pid: 1, updated_at_unix: 1 }).unwrap();
        let server = respond(listener, health(&root, "fixture"), true);
        let (client, address, _health) = resolve_agent_client(&config, false).await.unwrap();
        assert_eq!(address.port(), port);
        client.post(format!("http://{address}/v1/harness")).json(&serde_json::json!({"action":"stop"})).send().await.unwrap();
        server.join().unwrap();
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[tokio::test]
    async fn explicit_port_ignores_discovery_but_automatic_rejects_stale_identity() {
        for explicit in [false, true] {
            let (config, listener) = fixture("explicit");
            let paths = config.paths();
            let root = data_root_identity(&paths).unwrap();
            paths.publish_agent_discovery(&AgentDiscoveryRecord { port: if explicit { 1 } else { config.port }, data_root_id: root.clone(), instance_id: "old".into(), pid: 1, updated_at_unix: 1 }).unwrap();
            let server = respond(listener, health(&root, "fixture"), false);
            assert_eq!(resolve_agent_client(&config, explicit).await.is_ok(), explicit);
            server.join().unwrap();
            fs::remove_dir_all(paths.root).unwrap();
        }
    }
    #[tokio::test]
    async fn explicit_port_rejects_another_data_root_before_mutation() {
        let (config, listener) = fixture("explicit-wrong-root");
        let server = respond(listener, health("other-root", "fixture"), false);
        assert!(resolve_agent_client(&config, true).await.is_err());
        server.join().unwrap();
        fs::remove_dir_all(config.paths().root).unwrap();
    }

    #[tokio::test]
    async fn local_health_redirect_is_never_followed() {
        let (config, listener) = fixture("redirect");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            stream.read(&mut bytes).unwrap();
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let error = resolve_agent_client(&config, true).await.unwrap_err();
        assert!(error.contains("HTTP 302"), "{error}");
        server.join().unwrap();
        fs::remove_dir_all(config.paths().root).unwrap();
    }

    #[tokio::test]
    async fn default_fallback_rejects_wrong_root_and_invalid_discovery_never_connects() {
        let (config, listener) = fixture("wrong-root");
        let paths = config.paths();
        let server = respond(listener, health("other-root", "fixture"), false);
        assert!(resolve_agent_client(&config, false).await.is_err());
        server.join().unwrap();
        fs::create_dir_all(&paths.run_dir).unwrap();
        for bytes in [b"{".to_vec(), vec![b'x'; 65537]] {
            fs::write(paths.agent_discovery_file(), bytes).unwrap();
            assert!(resolve_agent_client(&config, false).await.is_err());
        }
        fs::remove_dir_all(paths.root).unwrap();
    }
}
