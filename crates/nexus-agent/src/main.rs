use std::{env, path::PathBuf, process};

use nexus_core::NexusConfig;

fn main() {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(message) => {
            eprintln!("nexus-agent: {message}");
            eprintln!("use --help for usage");
            process::exit(2);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter("nexus_agent=info")
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("nexus-agent: failed to initialize runtime: {error}");
            process::exit(1);
        }
    };

    if let Err(error) = runtime.block_on(nexus_agent::run(config)) {
        eprintln!("nexus-agent: {error}");
        process::exit(1);
    }
}

fn parse_args() -> Result<Option<NexusConfig>, String> {
    let mut config = NexusConfig::from_env();
    let mut args = env::args_os().skip(1);

    while let Some(argument) = args.next() {
        match argument.to_string_lossy().as_ref() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--port requires a value".to_owned())?;
                config.port = parse_port(&value.to_string_lossy())?;
            }
            "--data-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--data-dir requires a value".to_owned())?;
                let path = PathBuf::from(value);
                if path.as_os_str().is_empty() {
                    return Err("--data-dir cannot be empty".to_owned());
                }
                config.data_dir = Some(path);
            }
            value => return Err(format!("unknown argument: {value}")),
        }
    }

    Ok(Some(config))
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| format!("invalid port: {value}"))
}

fn print_help() {
    println!(
        "nexus-agent\n\nUsage: nexus-agent [--port PORT] [--data-dir PATH]\n\n\
         The Agent binds to loopback only. Environment overrides: NEXUS_AGENT_PORT, NEXUS_DATA_DIR."
    );
}
