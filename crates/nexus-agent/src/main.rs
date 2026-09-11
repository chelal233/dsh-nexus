use std::{env, path::PathBuf, process};

use nexus_core::NexusConfig;

fn main() {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--parse-offline-yaml")) {
        use std::io::{Read, Write};
        let result = (|| -> std::io::Result<()> {
            let mut input=Vec::new();std::io::stdin().lock().take(1024*1024+1).read_to_end(&mut input)?;
            let output=parse_offline_yaml(&input)?;
            std::io::stdout().lock().write_all(&output)
        })();
        if result.is_err() { eprintln!("Offline configuration is not supported YAML");process::exit(1); }
        return;
    }
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--write-private-archive")) {
        let result = env::args_os().nth(2).map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other("Archive target is required"))
            .and_then(|path| nexus_private_file::write_new_private_stream(&path, &mut std::io::stdin().lock()));
        if let Err(error) = result { eprintln!("{error}"); process::exit(1); }
        return;
    }
    #[cfg(windows)]
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--harness-command")) {
        match nexus_agent::windows_harness::command_helper(&env::args_os().skip(2).collect::<Vec<_>>()) {
            Ok(code) => process::exit(code),
            Err(error) => { eprintln!("Harness command failed: {error}"); process::exit(1); }
        }
    }
    #[cfg(windows)]
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--signal-console")) {
        let result = nexus_agent::windows_harness::signal_helper(&env::args().skip(2).collect::<Vec<_>>());
        if let Err(error) = result { eprintln!("{error}"); process::exit(1); }
        return;
    }
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-identity")) {
        println!("{}", serde_json::json!({ "buildId": option_env!("NEXUS_BUILD_ID").unwrap_or("development"), "version": env!("CARGO_PKG_VERSION"), "authVersion": 2 }));
        return;
    }
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--git-worker")) {
        let result = env::args_os().nth(2).map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other("Git worker request is required"))
            .and_then(|path| nexus_agent::git_worker::execute_request(&path));
        if let Err(error) = result { eprintln!("{error}"); process::exit(1); }
        return;
    }
    let (config, instance_id) = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(message) => {
            eprintln!("nexus-agent: {message}");
            eprintln!("use --help for usage");
            process::exit(2);
        }
    };

    // The launcher injects NEXUS_AGENT_LOG when the user picks a log level;
    // unknown or absent values keep the default.
    let level = std::env::var("NEXUS_AGENT_LOG")
        .ok()
        .filter(|value| {
            matches!(
                value.as_str(),
                "error" | "warn" | "info" | "debug" | "trace"
            )
        })
        .unwrap_or_else(|| "info".to_owned());
    tracing_subscriber::fmt()
        .with_env_filter(format!("nexus_agent={level}"))
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

    if let Err(error) = runtime.block_on(nexus_agent::run_with_instance_id(config, instance_id)) {
        eprintln!("nexus-agent: {error}");
        process::exit(1);
    }
}

fn parse_offline_yaml(input: &[u8]) -> std::io::Result<Vec<u8>> {
    use serde::de::DeserializeSeed;
    let invalid=||std::io::Error::other("Offline configuration is not supported YAML");
    if input.len()>1024*1024 {return Err(invalid());}
    let mut budget=YamlBudget{nodes:100_000,bytes:4*1024*1024};
    let mut documents=serde_yaml_ng::Deserializer::from_slice(input);
    let document=documents.next().ok_or_else(invalid)?;
    let value=BoundedYaml{budget:&mut budget,depth:0}.deserialize(document).map_err(|_|invalid())?;
    if documents.next().is_some(){return Err(invalid());}
    let result=serde_json::to_vec(&value).map_err(|_|invalid())?;
    if result.len()>4*1024*1024 {return Err(invalid());} Ok(result)
}

// Charge aliases while they are visited, before copying strings/containers.
// A post-parse size check alone allows a small YAML document to expand to GiB.
struct YamlBudget { nodes:usize, bytes:usize }
struct BoundedYaml<'a> { budget:&'a mut YamlBudget, depth:usize }
impl<'de> serde::de::DeserializeSeed<'de> for BoundedYaml<'_> {
    type Value=serde_json::Value;
    fn deserialize<D:serde::Deserializer<'de>>(self,de:D)->Result<Self::Value,D::Error> {
        if self.depth>64 || self.budget.nodes==0 {return Err(serde::de::Error::custom("YAML budget exceeded"));}
        self.budget.nodes-=1;de.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for BoundedYaml<'_> {
    type Value=serde_json::Value;
    fn expecting(&self,f:&mut std::fmt::Formatter)->std::fmt::Result {f.write_str("bounded JSON-compatible YAML")}
    fn visit_unit<E:serde::de::Error>(self)->Result<Self::Value,E>{Ok(serde_json::Value::Null)}
    fn visit_bool<E:serde::de::Error>(self,v:bool)->Result<Self::Value,E>{Ok(v.into())}
    fn visit_i64<E:serde::de::Error>(self,v:i64)->Result<Self::Value,E>{Ok(v.into())}
    fn visit_u64<E:serde::de::Error>(self,v:u64)->Result<Self::Value,E>{Ok(v.into())}
    fn visit_f64<E:serde::de::Error>(self,v:f64)->Result<Self::Value,E>{serde_json::Number::from_f64(v).map(serde_json::Value::Number).ok_or_else(||E::custom("Non-finite YAML number"))}
    fn visit_str<E:serde::de::Error>(self,v:&str)->Result<Self::Value,E>{
        self.budget.bytes=self.budget.bytes.checked_sub(v.len()).ok_or_else(||E::custom("YAML string budget exceeded"))?;
        Ok(v.into())
    }
    fn visit_string<E:serde::de::Error>(self,v:String)->Result<Self::Value,E>{self.visit_str(&v)}
    fn visit_seq<A:serde::de::SeqAccess<'de>>(self,mut seq:A)->Result<Self::Value,A::Error>{
        let mut values=Vec::new();
        while let Some(value)=seq.next_element_seed(BoundedYaml{budget:self.budget,depth:self.depth+1})? {values.push(value);}
        Ok(values.into())
    }
    fn visit_map<A:serde::de::MapAccess<'de>>(self,mut map:A)->Result<Self::Value,A::Error>{
        use serde::de::Error;
        let mut values=serde_json::Map::new();
        while let Some(key)=map.next_key_seed(BoundedYaml{budget:self.budget,depth:self.depth+1})? {
            let serde_json::Value::String(key)=key else {return Err(A::Error::custom("YAML keys must be strings"));};
            if key=="<<" || values.contains_key(&key) {return Err(A::Error::custom("YAML merge or duplicate key"));}
            let value=map.next_value_seed(BoundedYaml{budget:self.budget,depth:self.depth+1})?;values.insert(key,value);
        }
        Ok(values.into())
    }
}

#[cfg(test)]
mod offline_yaml_tests {
    use super::*;
    #[test]
    fn bounded_yaml_parser_handles_real_configuration_and_rejects_ambiguous_formats() {
        let parsed:serde_json::Value=serde_json::from_slice(&parse_offline_yaml(b"model: local\ncredentials:\n  apiKey: keep\ntokenLimit: 4096\n").unwrap()).unwrap();
        assert_eq!(parsed["credentials"]["apiKey"],"keep");assert_eq!(parsed["tokenLimit"],4096);
        for input in ["a: 1\na: 2", "key: !unsafe value", "? [a,b]\n: value", "a: .nan", "base: &base {a: 1}\nother: {<<: *base}"] {assert!(parse_offline_yaml(input.as_bytes()).is_err());}
        assert!(parse_offline_yaml(&vec![b' ';1024*1024+1]).is_err());
        // Only ~5MiB could be expanded even without the fix; no stress input.
        // With the visitor the string budget fails before materialization.
        let repeated=format!("large: &large {}\nvalues: [{}]\n","x".repeat(64*1024),vec!["*large";80].join(","));
        assert!(parse_offline_yaml(repeated.as_bytes()).is_err());
        assert!(parse_offline_yaml(b"a: 1\n---\nb: 2").is_err());
    }
}

fn parse_args() -> Result<Option<(NexusConfig, Option<String>)>, String> {
    let mut config = NexusConfig::from_env();
    let mut instance_id = None;
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
            "--instance-id" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--instance-id requires a value".to_owned())?
                    .to_string_lossy()
                    .into_owned();
                if value.is_empty() || value.len() > 192 || value.chars().any(char::is_control) {
                    return Err("--instance-id is invalid".to_owned());
                }
                instance_id = Some(value);
            }
            value => return Err(format!("unknown argument: {value}")),
        }
    }

    Ok(Some((config, instance_id)))
}

fn parse_port(value: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .ok()
        // 0 selects an OS-assigned (ephemeral) port; the Agent publishes the
        // actual port to run/agent.json for discovery.
        .ok_or_else(|| format!("invalid port: {value}"))
}

fn print_help() {
    println!(
        "nexus-agent\n\nUsage: nexus-agent [--port PORT] [--data-dir PATH] [--instance-id ID]\n\n\
         The Agent binds to loopback only. Environment overrides: NEXUS_AGENT_PORT, NEXUS_DATA_DIR."
    );
}
