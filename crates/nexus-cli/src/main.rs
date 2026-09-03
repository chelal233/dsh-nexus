use std::{env, net::SocketAddr, process};

use nexus_core::{NexusConfig, DEFAULT_AGENT_PORT};
use nexus_protocol::{
    ErrorResponse, HarnessAction, HarnessCommand, HarnessResponse, StateResponse,
};

#[derive(Debug)]
struct Options {
    command: Command,
    json: bool,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Status,
    Harness(HarnessAction),
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
                    .ok_or_else(|| "harness requires status, start, or stop".to_owned())?;
                let action = match action.to_string_lossy().as_ref() {
                    "status" => HarnessAction::Status,
                    "start" => HarnessAction::Start,
                    "stop" => HarnessAction::Stop,
                    value => return Err(format!("unknown harness action: {value}")),
                };
                command = Some(Command::Harness(action));
            }
            "--json" => json = true,
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
                print_help();
                return Ok(None);
            }
            value => return Err(format!("unknown argument: {value}")),
        }
    }

    let Some(command) = command else {
        return Err(
            "a command is required (supported: status, harness status|start|stop)".to_owned(),
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
    let response = match options.command {
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
            .json(&HarnessCommand { action })
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

    match options.command {
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

fn print_help() {
    println!(
        "nexusctl\n\nUsage:\n  nexusctl status [--json] [--port PORT]\n  nexusctl harness status [--json] [--port PORT]\n  nexusctl harness start [--json] [--port PORT]\n  nexusctl harness stop [--json] [--port PORT]\n\n\
         Queries and controls the loopback Nexus Agent API."
    );
}
