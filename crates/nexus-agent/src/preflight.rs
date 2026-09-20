//! On-demand observations only: never run Harness, compile, or materialize profiles.
use std::{fs, io, net::TcpListener, path::{Path, PathBuf}};
use axum::{extract::State, response::IntoResponse, Json};
use nexus_core::{load_harness_launch_spec, HarnessLaunchSpec, RuntimeConfig};
use nexus_protocol::{HarnessLaunchMode, HarnessState};
use serde_json::{json, Value};
use crate::{AppState, runtime::{RuntimeRequestContext, BlockingStage}};

fn item(id: &str, status: &str, reason: impl ToString, next: &str) -> Value {
    json!({"id":id,"status":status,"reason":reason.to_string(),"next":next})
}

pub(crate) async fn check(State(state): State<AppState>) -> impl IntoResponse { Json(evaluate(state).await.0) }

#[derive(Debug)]
pub(crate) struct PreparedStart {
    pub source: crate::source_context::SourceContext,
    revision: String,
    profile: String,
    critical: Option<String>,
}
fn critical(root: &Path) -> io::Result<String> {
    use sha2::{Digest,Sha256};
    let mut digest=Sha256::new();
    digest.update(nexus_core::data_root_identity(&nexus_core::NexusPaths::from_root(root.to_owned()))?);
    for relative in ["package.json","apps/cli/package.json","apps/cli/lib/bin.js"] {
        digest.update(relative);
        if let Some(bytes)=nexus_core::read_regular_file_bounded(&root.join(relative),16*1024*1024)? {digest.update(Sha256::digest(bytes));}
    }
    Ok(format!("{:x}",digest.finalize()))
}
impl PreparedStart {
    pub(crate) fn recheck(&self,paths:&nexus_core::NexusPaths,profile:&str)->io::Result<()> {
        if self.profile!=profile || nexus_core::ConfigStore::new(paths.clone()).snapshot()?.revision!=self.revision || nexus_core::ProfileStore::new(paths.clone()).load()?.active_profile!=self.profile {
            return Err(io::Error::other("Startup inputs changed after verification; retry startup"));
        }
        if self.source.root.as_deref().map(critical).transpose()?!=self.critical {return Err(io::Error::other("Harness identity or entry changed after verification; retry startup"));}
        Ok(())
    }
}
pub(crate) async fn evaluate(state: AppState) -> (Value,Option<PreparedStart>) {
    let request = RuntimeRequestContext::preflight();
    let (generation, harness, session) = state.supervisor.status_observation().await;
    let running = matches!(harness.state, HarnessState::Running | HarnessState::Starting);
    let owned = state.clone();
    let basics = request.run_blocking_io(BlockingStage::ConfigFile, state.paths.config_file.clone(),
        move || {
            let current_listener = if running && generation == session.generation {
                let mut observer = nexus_launcher_core::HarnessLogObserver::default();
                let ui = nexus_launcher_core::read_harness_ui_info_with_observer(&owned.paths, &mut observer, Some(&session));
                if ui.available && ui.generation == Some(generation) && ui.run_id.as_deref() == Some(session.run_id.as_str()) {
                    let mut observed = HarnessLaunchSpec::new("node".into());
                    observed.readiness_url = ui.url;
                    crate::supervisor::preflight_readiness_endpoint(&observed).ok().flatten()
                } else { None }
            } else { None };
            let snapshot=owned.config.snapshot()?;
            let profile=owned.profiles.load()?.active_profile;
            let mut source=None;
            let (checks,runtime)=collect_with_source(&owned,running,current_listener.as_ref(),&mut source);
            let prepared=source.map(|source|->io::Result<PreparedStart>{
                let critical=source.root.as_deref().map(critical).transpose()?;
                let prepared=PreparedStart{source,revision:snapshot.revision,profile,critical};
                prepared.recheck(&owned.paths,&prepared.profile)?;
                Ok(prepared)
            }).transpose()?;
            Ok((checks,runtime,prepared))
        }).await;
    let (mut checks, runtime, prepared) = match basics {
        Ok(value) => value,
        Err(error) => (vec![item("configuration", "blocked", error, "Retry the check; export diagnostics if it fails again.")], None, None),
    };
    if let Some(runtime) = runtime {
        let observed = crate::runtime::observe_runtime_selection_until(&state.paths, Some(&runtime), &request).await;
        for tool in observed.tools.into_iter().filter(|tool| tool.name != "git") {
            checks.push(item(&tool.name, if tool.available { "ok" } else { "blocked" },
                tool.reason.unwrap_or_else(|| format!("{} · {}", tool.version.unwrap_or_default(), tool.source.unwrap_or_default())),
                if tool.available { "" } else { "Select a complete runtime in Settings or reinstall the bundled runtime." }));
        }
    }
    let paused = false;
    let ready = !checks.iter().any(|check| check["status"] == "blocked");
    (json!({"api_version":nexus_protocol::API_VERSION,"checked_at_unix":nexus_core::unix_time_seconds(),
        "ready":ready,"paused":paused,"checks":checks,"note":"This is an observation, not a startup guarantee; startup protection and plugin compatibility checks still apply."}),prepared)
}

#[cfg(test)]
pub(super) fn collect(state: &AppState, running: bool) -> (Vec<Value>, Option<RuntimeConfig>) {
    collect_with_listener(state, running, None)
}

#[cfg(test)]
fn collect_with_listener(state: &AppState, running: bool, current_listener: Option<&(String, u16)>) -> (Vec<Value>, Option<RuntimeConfig>) {
    collect_with_source(state,running,current_listener,&mut None)
}
fn collect_with_source(state:&AppState,running:bool,current_listener:Option<&(String,u16)>,source_out:&mut Option<crate::source_context::SourceContext>)->(Vec<Value>,Option<RuntimeConfig>){
    let mut checks = Vec::new();
    match state.checkpoint_restores.load() {
        Ok(Some(_)) => checks.push(item("recovery", "blocked", "A checkpoint restore transaction is pending.", "Retry or abort the pending restore in Checkpoints.")),
        Ok(None) => checks.push(item("recovery", "ok", "No checkpoint restore is pending.", "")),
        Err(error) => checks.push(item("recovery", "blocked", error, "Export diagnostics and repair the restore record.")),
    }
    match state.cold.cleanup_pending() {
        Ok(false) if !state.cold.publication_pending() => {}
        Ok(_) => checks.push(item("installation", "blocked", "Installation publication or cleanup is pending.", "Retry cancellation or restart Agent to reconcile installation.")),
        Err(error) => checks.push(item("installation", "blocked", error, "Export diagnostics before retrying installation.")),
    }
    let configuration = match state.config.load() {
        Ok(config) => config,
        Err(error) => { checks.push(item("configuration", "blocked", error, "Repair Nexus settings.")); return (checks, None); }
    };
    let profile = match state.profiles.load() {
        Ok(catalog) => catalog.active_profile,
        Err(error) => { checks.push(item("profile", "blocked", error, "Repair the profile selection.")); return (checks, None); }
    };
    let home = crate::dsh::resolve_dsh_home_for_paths(&state.paths);
    if let Ok(home) = &home {
        match check_home_access(home) {
            Ok(existing) => checks.push(item("home", "ok", format!("{} · {}", home.display(), if existing { "read/write access verified" } else { "parent access verified; directories will be created at startup" }), "")),
            Err(error) => checks.push(item("home", "blocked", error, "Choose an accessible ordinary directory in Settings; no data is moved.")),
        }
    } else if let Err(error) = &home {
        checks.push(item("home", "blocked", error, "Choose a valid Harness data directory in Settings."));
    }
    let mut spec = match load_harness_launch_spec(&state.paths) {
        Ok(Some(spec)) if !spec.program.as_os_str().is_empty() => spec,
        Ok(_) => { checks.push(item("launch", "blocked", "No Harness launch command is configured.", "Install or select a Harness version.")); return (checks, None); }
        Err(error) => { checks.push(item("launch", "blocked", error, "Repair the launch configuration in Settings.")); return (checks, None); }
    };
    let managed = spec.program.to_string_lossy().contains("{release_root}") || spec.args.iter().any(|arg| arg.contains("{release_root}") || arg.replace('\\', "/").ends_with("/apps/cli/lib/bin.js"));
    let effective_runtime = crate::runtime::runtime_for_launch(&mut spec, configuration.runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref());
    let source=match crate::source_context::resolve(&state.paths,&state.releases) {
        Ok(source)=>source,
        Err(error)=>{checks.push(item("release","blocked",error,"Confirm the external Harness source again or select an installed version."));return (checks,None);}
    };
    let id=source.release_id.as_deref();
    let root=source.root.clone();
    checks.push(item("release", if managed && root.is_none() { "blocked" } else { "ok" },
        id.unwrap_or(if source.external { "External Harness directory verified." } else if managed { "No usable version slot is selected." } else { "Custom command; no version slot required." }),
        if managed && root.is_none() { "Install or select a Harness version." } else { "" }));
    if let Err(error) = crate::supervisor::normalize_selected_launch(&mut spec, &state.paths, &state.releases) {
        checks.push(item("launch", "blocked", error, "Repair the selected version or launch paths."));
    }
    let preferences = nexus_core::load_harness_preferences(&state.paths);
    if let Err(error) = &preferences { checks.push(item("preferences", "blocked", error, "Correct the invalid Harness setting.")); }
    if let (Ok(home), Ok(preferences)) = (&home, preferences) {
        match crate::runtime_patches::validate_for_paths(&state.paths, &preferences)
            .and_then(|()| crate::preference_capabilities::validate_launch(&spec, &preferences, root.as_deref()))
            .and_then(|()| crate::preference_capabilities::resolve(root.as_deref(), home, &profile, &preferences)) {
            Ok(capabilities) => nexus_core::apply_harness_preferences(&mut spec, &preferences, &capabilities),
            Err(error) => checks.push(item("preferences", "blocked", error, "Clear unsupported overrides in Settings or select a verified Harness version and profile.")),
        }
    }
    let rendered = render_entry(&spec, &profile, id, root.as_deref());
    match rendered {
        Ok(entry) if entry.is_file() => checks.push(item("entry", "ok", entry.display(), "")),
        Ok(entry) => checks.push(item("entry", "blocked", format!("Entry file is missing: {}", entry.display()), "Reinstall the selected version or correct the launch path.")),
        Err(error) => checks.push(item("entry", "blocked", error, "Select a usable version or correct the launch path.")),
    }
    if let Some(cwd) = &spec.working_dir {
        match spec.render_path_for_context(cwd, &profile, id, root.as_deref()) {
            Ok(path) if path.is_dir() => checks.push(item("working_directory", "ok", path.display(), "")),
            Ok(path) => checks.push(item("working_directory", "blocked", format!("Launch directory is missing: {}", path.display()), "Repair the selected version or launch working directory.")),
            Err(error) => checks.push(item("working_directory", "blocked", error, "Correct the launch working directory.")),
        }
    }
    if managed {
        if let Ok(home) = &home {
            let profile_path = home.join("profiles").join(&profile);
            match crate::dsh::profile_is_initialized(home, &profile) {
                Ok(true) => checks.push(item("profile", "ok", format!("{profile}: initialized"), "")),
                Ok(false) if !profile_path.exists() => checks.push(item("profile", "warning", format!("{profile}: not initialized; the selected Harness must provide its built-in profile."), "First startup initializes supported built-in profiles. For a custom profile, create it in Profiles first.")),
                Ok(false) => checks.push(item("profile", "blocked", format!("{profile}: package.json is missing or invalid"), "Repair the profile manifest or select another profile.")),
                Err(error) => checks.push(item("profile", "blocked", error, "Repair the selected profile directory.")),
            }
        }
    } else { checks.push(item("profile", "ok", "Custom command: Harness profile checks are not applied.", "")); }
    checks.push(if managed { check_port(&spec, running, current_listener) } else { item("port", "warning", "Custom command: its readiness endpoint may belong to an external service.", "Verify custom listener ownership separately.") });
    let runtime = if managed && spec.mode == HarnessLaunchMode::Node {
        let mut runtime = effective_runtime;
        let mut program_spec = spec.clone();
        program_spec.mode = HarnessLaunchMode::Direct;
        if spec.program == Path::new("node") || spec.program == Path::new("node.exe") {
            if let Some(node) = &runtime.node { program_spec.program = node.path.clone(); }
        }
        match render_entry(&program_spec, &profile, id, root.as_deref()) {
            Ok(program) if program.is_file() => {
                let path = fs::canonicalize(&program).unwrap_or(program);
                let ownership = runtime.node.as_ref().filter(|pin|
                    fs::canonicalize(&pin.path).unwrap_or_else(|_| pin.path.clone()) == path
                ).map(|pin| pin.ownership.clone()).unwrap_or(nexus_protocol::RuntimeOwnership::System);
                runtime.node = Some(nexus_core::RuntimePin { path, ownership });
            },
            Ok(_) => checks.push(item("node_program", "blocked", "The configured Node launch program is missing.", "Select a complete Node runtime or reinstall Harness.")),
            Err(error) => checks.push(item("node_program", "blocked", error, "Correct the Node launch program.")),
        }
        Some(runtime)
    } else { checks.push(item("runtime", "ok", "Custom command: package-manager checks are not required.", "")); None };
    *source_out=Some(source);
    (checks, runtime)
}

fn render_entry(spec: &HarnessLaunchSpec, profile: &str, id: Option<&str>, root: Option<&Path>) -> io::Result<PathBuf> {
    let path = if spec.mode == HarnessLaunchMode::Node {
        PathBuf::from(spec.render_args_for_context(profile, id, root)?.first().ok_or_else(|| io::Error::other("Node entry is missing"))?)
    } else { spec.render_path_for_context(&spec.program, profile, id, root)? };
    if path.is_absolute() { return Ok(path); }
    if spec.mode == HarnessLaunchMode::Direct && path.components().count() == 1 {
        // A generic PATH command is valid; verify lookup without executing it.
        for parent in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            for suffix in if cfg!(windows) { vec!["", ".exe", ".cmd", ".bat"] } else { vec![""] } {
                let candidate = parent.join(format!("{}{suffix}", path.display()));
                if candidate.is_file() { return Ok(candidate); }
            }
        }
    }
    if let Some(cwd) = &spec.working_dir {
        return Ok(spec.render_path_for_context(cwd, profile, id, root)?.join(path));
    }
    Ok(path)
}

fn check_port(spec: &HarnessLaunchSpec, running: bool, current_listener: Option<&(String, u16)>) -> Value {
    let (host, port) = match crate::supervisor::preflight_readiness_endpoint(spec) {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => return item("port", "warning", "No fixed listener is declared; Harness chooses it at startup.", "Check the emitted Web UI address after startup."),
        Err(error) => return item("port", "blocked", error, "Correct the launch readiness address in Settings."),
    };
    let normalized_host = |host: &str| if host.eq_ignore_ascii_case("localhost") { "127.0.0.1".to_owned() } else { host.to_ascii_lowercase() };
    if running && current_listener.is_some_and(|(current_host, current_port)| *current_port == port && normalized_host(current_host) == normalized_host(&host)) {
        return item("port", "ok", "The current Harness session owns this same listener.", "");
    }
    let address = if host.contains(':') { format!("[::1]:{port}") } else { format!("127.0.0.1:{port}") };
    match TcpListener::bind(&address) {
        Ok(_) => item("port", "ok", format!("{address} is currently available"), ""),
        Err(error) if running && current_listener.is_none() => item("port", "warning", format!("{address}: listener ownership could not be verified ({error})"), "Confirm this is the current Harness listener before restarting."),
        Err(error) => item("port", "blocked", format!("{address}: {error}"), "Choose a different port or stop the application using this port."),
    }
}

fn check_home_access(home: &Path) -> io::Result<bool> {
    let mut nearest = None;
    for ancestor in home.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                let linked = { use std::os::windows::fs::MetadataExt; metadata.file_attributes() & 0x400 != 0 };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if !metadata.is_dir() || linked { return Err(io::Error::other("Harness home has a non-directory or linked ancestor")); }
                nearest = Some(ancestor);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    let parent = nearest.ok_or_else(|| io::Error::other("No accessible Harness home parent"))?;
    fs::read_dir(parent)?;
    let probe = parent.join(format!(".nexus-access-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
    let file = fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    drop(file);
    fs::remove_file(probe)?;
    Ok(parent == home)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn home_check_does_not_create_or_migrate_the_selected_home() {
        let root = std::env::temp_dir().join(format!("nexus-preflight-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir(&root).unwrap();
        assert!(!check_home_access(&root.join("missing/home")).unwrap());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn occupied_port_blocks_stopped_harness_but_not_running_harness() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut spec = HarnessLaunchSpec::new("custom.exe".into());
        spec.readiness_url = Some(format!("tcp://{}", listener.local_addr().unwrap()));
        assert_eq!(check_port(&spec, false, None)["status"], "blocked");
        assert_eq!(check_port(&spec, true, Some(&("127.0.0.1".into(), listener.local_addr().unwrap().port())))["status"], "ok");
        assert_eq!(check_port(&spec, true, Some(&("127.0.0.1".into(), 1)))["status"], "blocked");
        assert_eq!(check_port(&spec, true, None)["status"], "warning");
        spec.readiness_url = None; // Preferences port=0 removes the static readiness endpoint.
        assert_eq!(check_port(&spec, false, None)["status"], "warning");
    }
}
#[cfg(test)]
mod prepared_tests {
    use super::*;
    #[test]
    fn prepared_start_rechecks_critical_identity_and_revision_without_rescanning_tree() {
        let base=std::env::temp_dir().join(format!("prepared-start-{}",nexus_core::new_instance_id()));
        let paths=nexus_core::NexusPaths::from_root(base.join("nexus"));paths.ensure_directories().unwrap();
        let root=base.join("source");fs::create_dir_all(root.join("apps/cli/lib")).unwrap();fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("package.json"),br#"{"name":"@deepseek-ai/dsh-root"}"#).unwrap();fs::write(root.join("apps/cli/lib/bin.js"),b"entry").unwrap();
        let source=nexus_core::ExternalHarness::inspect(&paths,&root).unwrap();
        let store=nexus_core::ConfigStore::new(paths.clone());store.write(&nexus_core::NexusConfigFile{external_harness:Some(source.clone()),..Default::default()}).unwrap();
        let profile=nexus_core::ProfileStore::new(paths.clone()).load().unwrap().active_profile;
        let prepared=PreparedStart{source:crate::source_context::SourceContext{root:Some(source.root.clone()),release_id:None,external:true},revision:store.snapshot().unwrap().revision,profile:profile.clone(),critical:Some(critical(&source.root).unwrap())};
        prepared.recheck(&paths,&profile).unwrap();
        fs::write(root.join("node_modules/later.js"),b"ordinary dependency").unwrap();
        assert!(source.verify(&paths).is_err());
        prepared.recheck(&paths,&profile).unwrap(); // no second full-tree read
        fs::write(root.join("apps/cli/lib/bin.js"),b"changed entry").unwrap();assert!(prepared.recheck(&paths,&profile).is_err());
        fs::write(root.join("apps/cli/lib/bin.js"),b"entry").unwrap();
        store.transaction(|d|{d.external_harness=None;Ok(())}).unwrap();assert!(prepared.recheck(&paths,&profile).is_err());
        fs::remove_dir_all(base).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn blocked_prepared_restart_keeps_existing_child_running() {
        let state=crate::switch_ownership_tests::switch_test_state("prepared-restart");
        let mut spec=HarnessLaunchSpec::new(PathBuf::from(std::env::var_os("COMSPEC").unwrap_or_else(||"C:/Windows/System32/cmd.exe".into())));
        spec.args=vec!["/C".into(),"ping -n 60 127.0.0.1 >NUL".into()];
        state.config.transaction(|d|{d.harness=Some(spec);Ok(())}).unwrap();
        let lifecycle=state.supervisor.acquire_lifecycle().await;
        let started=state.supervisor.start_with_profile_locked("web",&lifecycle).await.unwrap();
        drop(lifecycle);
        let missing=state.paths.root.join("home-parent-file");fs::write(&missing,b"not a directory").unwrap();
        state.config.transaction(|d|{d.harness_preferences=Some(nexus_protocol::HarnessPreferencesPayload{home:Some(missing.join("home").to_string_lossy().into_owned()),..Default::default()});Ok(())}).unwrap();
        let error=crate::execute_harness_action(&state,nexus_protocol::HarnessAction::Restart).await.unwrap_err();
        assert!(matches!(error,crate::supervisor::HarnessSupervisorError::Preflight(_)));
        let current=state.supervisor.status().await;assert_eq!(current.pid,started.pid);assert_eq!(current.state,HarnessState::Running);
        state.supervisor.stop().await.unwrap();fs::remove_dir_all(&state.paths.root).unwrap();
    }
}
