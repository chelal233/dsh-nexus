use std::{env, net::SocketAddr, process};

use nexus_core::{NexusConfig, DEFAULT_AGENT_PORT};
use nexus_protocol::StateResponse;

#[derive(Debug)]
struct Options {
    json: bool,
    port: u16,
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

    if let Err(message) = run_status(options).await {
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
            "status" if command.is_none() => command = Some("status"),
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

    if command != Some("status") {
        return Err("a command is required (supported: status)".to_owned());
    }

    Ok(Some(Options {
        json,
        port: if config.port == 0 {
            DEFAULT_AGENT_PORT
        } else {
            config.port
        },
    }))
}

async fn run_status(options: Options) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], options.port));
    let url = format!("http://{address}/v1/state");
    let response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|error| format!("agent is unavailable: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read agent response: {error}"))?;

    if !status.is_success() {
        return Err(format!("agent returned HTTP {status}"));
    }

    let state: StateResponse =
        serde_json::from_str(&body).map_err(|error| format!("invalid agent response: {error}"))?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&state)
                .map_err(|error| format!("failed to encode JSON: {error}"))?
        );
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

    Ok(())
}

fn print_help() {
    println!(
        "nexusctl\n\nUsage: nexusctl status [--json] [--port PORT]\n\n\
         Queries the loopback Nexus Agent control API."
    );
}
