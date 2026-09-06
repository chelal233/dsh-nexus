use std::{env, net::SocketAddr, process};

use nexus_core::{NexusConfig, DEFAULT_AGENT_PORT};
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
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Status,
    Harness(HarnessAction),
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
    let mut config = NexusConfig::from_env();
    let mut command = None;
    let mut json = false;
    let mut args = env::args_os().skip(1).peekable();

    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "status" if command.is_none() => command = Some(Command::Status),
            "harness" if command.is_none() => {
                let action = args
                    .next()
                    .ok_or_else(|| "harness requires status, start, stop, or restart".to_owned())?;
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
                let (third, source, mode) = match action {
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
                        (Some(tag), Some(source), Some(mode))
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
                        (Some(format!("{operation}\n{token}")), None, None)
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
                    ),
                    _ => (None, None, None),
                };
                command = Some(Command::Update(
                    action, release_id, version, third, source, mode,
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
        port: if config.port == 0 {
            DEFAULT_AGENT_PORT
        } else {
            config.port
        },
    }))
}

async fn run(options: Options) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], options.port));
    let client = reqwest::Client::new();
    let response = match &options.command {
        Command::Status => client
            .get(format!("http://{address}/v1/state"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Harness(HarnessAction::Status) => client
            .get(format!("http://{address}/v1/harness"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Harness(action) => client
            .post(format!("http://{address}/v1/harness"))
            .json(&HarnessCommand { action: *action })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Profile(ProfileAction::List | ProfileAction::Status, _) => client
            .get(format!("http://{address}/v1/profiles"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Profile(ProfileAction::Select, profile) => client
            .post(format!("http://{address}/v1/profiles"))
            .json(&ProfileCommand {
                action: ProfileAction::Select,
                profile: profile.clone(),
                package: None,
            
                target: None,})
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Profile(
            ProfileAction::PluginInventory
            | ProfileAction::PluginMove
            | ProfileAction::PluginRemove
            | ProfileAction::PluginDisable
            | ProfileAction::PluginEnable
            | ProfileAction::OpenPath
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
            
                target: None,})
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Recovery => client
            .get(format!("http://{address}/v1/recovery"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Checkpoint(CheckpointAction::List, _, _) => client
            .get(format!("http://{address}/v1/checkpoints"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Checkpoint(action, id, note) => client
            .post(format!("http://{address}/v1/checkpoints"))
            .json(&CheckpointCommand {
                action: *action,
                id: id.clone(),
                note: note.clone(),
            })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Release(ReleaseAction::List | ReleaseAction::Current, _, _, _, _) => client
            .get(format!("http://{address}/v1/releases"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Release(action, id, version, source, note) => client
            .post(format!("http://{address}/v1/releases"))
            .json(&ReleaseCommand {
                action: *action,
                id: id.clone(),
                version: version.clone(),
                source: source.clone(),
                note: note.clone(),
            })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Update(UpdateAction::Status, _, _, _, _, _) => client
            .get(format!("http://{address}/v1/updates"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Update(action, release_id, version, third, source, mode) => client
            .post(format!("http://{address}/v1/updates"))
            .json(&UpdateCommand {
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
                    UpdateAction::Confirm => third
                        .as_deref()
                        .and_then(|value| value.split_once('\n'))
                        .map(|(id, _)| id.to_owned()),
                    UpdateAction::Cancel => third.clone(),
                    _ => None,
                },
                confirmation: if *action == UpdateAction::Confirm {
                    third
                        .as_deref()
                        .and_then(|value| value.split_once('\n'))
                        .map(|(_, token)| token.to_owned())
                } else {
                    None
                },
            })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Diagnostics(DiagnosticsAction::Status, _) => client
            .get(format!("http://{address}/v1/diagnostics"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Diagnostics(action, note) => client
            .post(format!("http://{address}/v1/diagnostics"))
            .json(&DiagnosticsCommand {
                action: *action,
                note: note.clone(),
            })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Config(ConfigAction::Status, _) => client
            .get(format!("http://{address}/v1/config"))
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
        Command::Config(action, runtime) => client
            .post(format!("http://{address}/v1/config"))
            .json(&ConfigCommand {
                action: *action,
                harness: None,
                update: None,
                runtime: runtime.clone(),
                snapshots: None,
                preserve_harness_readiness_url: false,
            })
            .send()
            .await
            .map_err(|error| format!("agent is unavailable: {error}"))?,
    };
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read agent response: {error}"))?;

    if !status.is_success() {
        if let Ok(error) = serde_json::from_str::<ErrorResponse>(&body) {
            return Err(format!(
                "agent returned HTTP {status}: {} ({})",
                error.message, error.code
            ));
        }
        return Err(format!("agent returned HTTP {status}: {body}"));
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
        Command::Update(_, _, _, _, _, _) => {
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
