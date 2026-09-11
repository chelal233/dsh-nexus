//! Safe launch inputs, not the upstream configuration after patch composition.
use std::{fs, io::{self, Read}, path::Path};
use nexus_core::{HarnessLogSession, HarnessLaunchSpec, NexusPaths};
use nexus_protocol::{HarnessPreferencesPayload, HarnessState};
use serde::{Deserialize, Serialize};

const RECORD: &str = "harness-effective.json";
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputField { name: String, value: String, source: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchInputs {
    schema_version: u32,
    pub generation: Option<u64>,
    pub run_id: Option<String>,
    profile: String,
    fields: Vec<InputField>,
}

pub(crate) fn describe(profile: &str, home: &Path, program: &Path, cwd: Option<&Path>,
    root: Option<&Path>, p: &HarnessPreferencesPayload, launch_env_override: bool, arguments: &[String]) -> LaunchInputs {
    let mut fields = Vec::new();
    let safe = |value: String| String::from_utf8_lossy(&nexus_core::redact_diagnostics_payload(value.as_bytes()).0).trim().to_owned();
    let mut add = |name: &str, value: String, source: &str| fields.push(InputField { name: name.into(), value: safe(value), source: source.into() });
    let launch_source = if launch_env_override { "Launcher environment or launch configuration" } else { "Resolved launch configuration" };
    add("Program", program.display().to_string(), launch_source);
    add("Working directory", cwd.map(|path| path.display().to_string()).unwrap_or_else(|| "Inherited process directory".into()), launch_source);
    add("Harness data directory", home.display().to_string(), if p.home.is_some() { "Nexus override" }
        else if std::env::var_os("DSH_HOME").is_some_and(|value| !value.is_empty()) { "DSH_HOME environment" } else { "User-home default" });
    add("Profile", profile.into(), "Selected or compatibility profile");
    if let Some(root) = root {
        add("Release directory", root.display().to_string(), "Selected Harness program source");
        if let Ok(evidence) = crate::preference_capabilities::inspect(root, home, profile) {
            add("Verified Harness version", evidence.version, "Installed package manifest");
        } else { add("Verified Harness version", "Not verified".into(), "Installed package manifest"); }
    }
    let mut probe = HarnessLaunchSpec::new(program.to_owned());
    probe.mode = nexus_protocol::HarnessLaunchMode::Node; probe.args = arguments.to_vec(); probe.working_dir = cwd.map(Path::to_owned);
    let managed = crate::preference_capabilities::validate_launch(&probe,
        &HarnessPreferencesPayload { port: Some(0), ..Default::default() }, root).is_ok();
    let (argument_port, argument_no_open) = if managed { argument_inputs(arguments) } else { (None, false) };
    let port = p.port.or(argument_port);
    let browser = p.open_browser.or_else(|| argument_no_open.then_some(false));
    for (name, value, explicit_override, argument_value) in [
        ("Port", port.map(|v| if v == 0 { "Automatic port (0)".into() } else { v.to_string() }), p.port.is_some(), argument_port.is_some()),
        ("Open browser", browser.map(|v| v.to_string()), p.open_browser.is_some(), argument_no_open),
        ("Disable telemetry", p.telemetry_disabled.map(|v| v.to_string()), p.telemetry_disabled.is_some(), false),
        ("Tools mode", p.tools_mode.clone(), p.tools_mode.is_some(), false),
        ("Permission mode", p.permission_mode.clone(), p.permission_mode.is_some(), false),
    ] {
        let source = if explicit_override { "Nexus override" } else if argument_value { "Launch arguments" } else { "Inherited configuration" };
        add(name, value.unwrap_or_else(|| "Resolved by Harness; not inspected".into()), source);
    }
    add("Additional patches", p.patches.as_ref().map_or(0, Vec::len).to_string(), "Nexus override; contents omitted");
    // No raw argv, prompt, endpoint credentials, or process environment is serialized.
    LaunchInputs { schema_version: 1, generation: None, run_id: None, profile: profile.into(), fields }
}

fn argument_inputs(arguments: &[String]) -> (Option<u16>, bool) {
    let mut port = None;
    let mut no_open = false;
    for (index, argument) in arguments.iter().enumerate() {
        if argument == "--" { break; }
        if argument == "--port" { port = arguments.get(index + 1).and_then(|value| value.parse().ok()); }
        else if let Some(value) = argument.strip_prefix("--port=") { port = value.parse().ok(); }
        if argument == "--no-open" { no_open = true; }
    }
    (port, no_open)
}

pub(crate) fn record(paths: &NexusPaths, mut inputs: LaunchInputs, session: &HarnessLogSession) -> io::Result<()> {
    inputs.generation = Some(session.generation); inputs.run_id = Some(session.run_id.clone());
    nexus_core::write_json_atomic(&paths.run_dir, &paths.run_dir.join(RECORD), &inputs)
}

pub(crate) fn current(paths: &NexusPaths, generation: u64, state: HarnessState, session: &HarnessLogSession) -> Option<LaunchInputs> {
    if state != HarnessState::Running || generation != session.generation { return None; }
    let path = paths.run_dir.join(RECORD);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 65536 { return None; }
    let mut bytes = Vec::new(); fs::File::open(path).ok()?.take(65537).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 65536 { return None; }
    let inputs: LaunchInputs = serde_json::from_slice(&bytes).ok()?;
    (inputs.schema_version == 1 && inputs.generation == Some(generation) && inputs.run_id.as_deref() == Some(session.run_id.as_str())).then_some(inputs)
}

pub(crate) fn next(paths: &NexusPaths) -> io::Result<LaunchInputs> {
    let p = nexus_core::load_harness_preferences(paths)?;
    let home = crate::dsh::resolve_dsh_home_for_paths(paths)?;
    let profile = nexus_core::ProfileStore::new(paths.clone()).load()?.active_profile;
    let releases = nexus_core::ReleaseStore::new(paths.clone());
    let external=nexus_core::ConfigStore::new(paths.clone()).load()?.external_harness;
    let catalog=if external.is_none(){Some(releases.load()?)}else{None};
    let id=catalog.as_ref().and_then(|c|c.current_release.as_deref());
    let root = external.map(|s|s.root).or(id.map(|id| releases.release_root(id)).transpose()?);
    let mut spec: HarnessLaunchSpec = nexus_core::load_harness_launch_spec(paths)?.ok_or_else(|| io::Error::other("No launch configuration"))?;
    crate::supervisor::normalize_selected_launch(&mut spec, paths, &releases)?;
    let runtime = nexus_core::ConfigStore::new(paths.clone()).load()?.runtime.unwrap_or_default();
    crate::runtime::runtime_for_launch(&mut spec, runtime, nexus_core::bundled_runtime_dir().as_deref());
    let program = spec.render_path_for_context(&spec.program, &profile, id, root.as_deref())?;
    let cwd = spec.working_dir.as_deref().map(|path| spec.render_path_for_context(path, &profile, id, root.as_deref())).transpose()?;
    let arguments = spec.render_args_for_context(&profile, id, root.as_deref())?;
    Ok(describe(&profile, &home, &program, cwd.as_deref(), root.as_deref(), &p, environment_override(), &arguments))
}

pub(crate) fn environment_override() -> bool {
    ["NEXUS_HARNESS_PROGRAM", "NEXUS_HARNESS_ARGS", "NEXUS_HARNESS_WORKING_DIR"].iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_arguments_explain_explicit_port_and_browser_without_serializing_argv() {
        let root = std::env::temp_dir().join(format!("nexus-input-arguments-{}", nexus_core::unix_time_nanos_for_update()));
        let entry = root.join("apps/cli/lib/bin.js"); fs::create_dir_all(entry.parent().unwrap()).unwrap(); fs::write(&entry, "").unwrap();
        let args = vec![entry.to_string_lossy().into_owned(), "--token".into(), "ARGV_SENTINEL".into(), "--port".into(), "3080".into(), "--no-open".into()];
        let show = |p: HarnessPreferencesPayload, root_option: Option<&Path>| describe("web", &root.join("home"), Path::new("node.exe"), None, root_option, &p, false, &args);
        let inputs = show(HarnessPreferencesPayload::default(), Some(&root));
        assert!(inputs.fields.iter().any(|field| field.name == "Port" && field.value == "3080" && field.source == "Launch arguments"));
        assert!(inputs.fields.iter().any(|field| field.name == "Open browser" && field.value == "false" && field.source == "Launch arguments"));
        assert!(!serde_json::to_string(&inputs).unwrap().contains("ARGV_SENTINEL"));
        let explicit = show(HarnessPreferencesPayload { port: Some(0), ..Default::default() }, Some(&root));
        assert!(explicit.fields.iter().any(|field| field.name == "Port" && field.value == "Automatic port (0)" && field.source == "Nexus override"));
        let custom = show(HarnessPreferencesPayload::default(), None);
        assert!(custom.fields.iter().any(|field| field.name == "Port" && field.value.contains("not inspected")));
        assert_eq!(argument_inputs(&["--port=10".into(), "--port".into(), "20".into()]).0, Some(20));
        assert_eq!(argument_inputs(&["--".into(), "--port=20".into()]).0, None);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn launch_record_is_safe_and_only_presented_for_the_matching_running_session() {
        let root = std::env::temp_dir().join(format!("nexus-launch-inputs-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let p = HarnessPreferencesPayload { system_prompt: Some("PROMPT_SENTINEL".into()),
            deepseek_base_url: Some("https://example.test/?key=URL_SENTINEL".into()),
            patches: Some(vec!["PATCH_SENTINEL".into()]), port: Some(0), telemetry_disabled: Some(false), ..Default::default() };
        let inputs = describe("web", &root.join("home"), Path::new("node.exe"), None, None, &p, false, &[]);
        let session = HarnessLogSession::new("input-run".into(), 3, 0, 0, "out".into(), "err".into(), "stdout.log".into(), "stderr.log".into(), true, 1);
        record(&paths, inputs, &session).unwrap();
        let bytes = fs::read_to_string(paths.run_dir.join(RECORD)).unwrap();
        for secret in ["PROMPT_SENTINEL", "URL_SENTINEL", "PATCH_SENTINEL"] { assert!(!bytes.contains(secret)); }
        assert!(bytes.contains("Automatic port (0)"));
        assert!(bytes.contains("Resolved by Harness; not inspected"));
        assert!(current(&paths, 3, HarnessState::Running, &session).is_some());
        assert!(current(&paths, 4, HarnessState::Running, &session).is_none());
        assert!(current(&paths, 3, HarnessState::Stopped, &session).is_none());
        let mut other = session.clone(); other.run_id = "other".into();
        assert!(current(&paths, 3, HarnessState::Running, &other).is_none());
        fs::write(paths.run_dir.join(RECORD), "invalid").unwrap();
        assert!(current(&paths, 3, HarnessState::Running, &session).is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
