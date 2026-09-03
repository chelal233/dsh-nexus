use std::{env, net::SocketAddr, process};

use nexus_core::{NexusConfig, DEFAULT_AGENT_PORT};
use nexus_protocol::{
    CheckpointAction, CheckpointCommand, CheckpointCreateResponse, CheckpointListResponse,
    CheckpointRestoreResponse, ErrorResponse, HarnessAction, HarnessCommand, HarnessResponse,
    ProfileAction, ProfileCommand, ProfileListResponse, ProfileSelectResponse, ReleaseAction,
    ReleaseCommand, ReleaseListResponse, StateResponse,
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
    Checkpoint(CheckpointAction, Option<String>, Option<String>),
    Release(
        ReleaseAction,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
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
    let mut args = env::args_os().skip(1);

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
                let action = args
                    .next()
                    .ok_or_else(|| "profile requires status, list, or select NAME".to_owned())?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => ProfileAction::Status,
                    "list" => ProfileAction::List,
                    "select" => ProfileAction::Select,
                    value => return Err(format!("unknown profile action: {value}")),
                };
                let name = if action == ProfileAction::Select {
                    Some(
                        args.next()
                            .ok_or_else(|| "profile select requires NAME".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    )
                } else {
                    None
                };
                command = Some(Command::Profile(action, name));
            }
            "checkpoint" if command.is_none() => {
                let action = args
                    .next()
                    .ok_or_else(|| "checkpoint requires list, create, or restore ID".to_owned())?;
                let action = match action.to_string_lossy().as_ref() {
                    "list" => CheckpointAction::List,
                    "create" => CheckpointAction::Create,
                    "restore" => CheckpointAction::Restore,
                    value => return Err(format!("unknown checkpoint action: {value}")),
                };
                let id = if action == CheckpointAction::Restore {
                    Some(
                        args.next()
                            .ok_or_else(|| "checkpoint restore requires ID".to_owned())?
                            .to_string_lossy()
                            .into_owned(),
                    )
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
                    _ => {
                        return Err(
                            "--note is only valid for checkpoint create or release register"
                                .to_owned(),
                        )
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
            "a command is required (supported: status, harness, profile, checkpoint)".to_owned(),
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
            })
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
                        println!(
                            "{} profile={} created_at_unix={}",
                            checkpoint.id, checkpoint.profile, checkpoint.created_at_unix
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
            CheckpointAction::Restore => {
                let checkpoint: CheckpointRestoreResponse = serde_json::from_str(&body)
                    .map_err(|error| format!("invalid agent response: {error}"))?;
                if options.json {
                    print_json_value(
                        &serde_json::to_value(&checkpoint).map_err(|error| error.to_string())?,
                    )?;
                } else {
                    println!("restored checkpoint: {}", checkpoint.checkpoint.id);
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
  nexusctl checkpoint list [--json] [--port PORT]
  nexusctl checkpoint create [--note TEXT] [--json] [--port PORT]
  nexusctl checkpoint restore ID [--json] [--port PORT]
  nexusctl release list|current [--json] [--port PORT]
  nexusctl release register ID VERSION [--source TEXT] [--note TEXT] [--json] [--port PORT]
  nexusctl release promote ID [--json] [--port PORT]
  nexusctl release rollback [--json] [--port PORT]

Queries and controls the loopback Nexus Agent API."#
    );
}
