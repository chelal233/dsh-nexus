//! Version-bound, startup-only plugin compatibility checks.
use std::{fs, io, path::Path, process::Stdio, time::Duration};
use nexus_core::{validate_profile_name, write_json_atomic, NexusPaths};
use nexus_protocol::{CompatibilityReport, HarnessLaunchMode};
use nexus_core::CancellationToken;

pub(crate) fn latest(paths: &NexusPaths) -> Option<CompatibilityReport> {
    let bytes = fs::read(paths.root.join("compatibility/latest.json")).ok()?;
    if bytes.len() > 64 * 1024 { return None; }
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn latest_for_selection(
    paths: &NexusPaths, home: &Path, selected: &str, release: Option<&str>,
) -> Option<CompatibilityReport> {
    let report = latest(paths)?;
    if !matches!(report.status.as_str(), "passed" | "isolated" | "needs_choice")
        || (report.status != "needs_choice" && Some(report.release_id.as_str()) != release) {
        return None;
    }
    if report.status == "needs_choice" && report.trigger.as_deref() == Some("profile_switch") {
        return (Some(report.release_id.as_str()) == release).then_some(report);
    }
    let source = source_profile(&home, selected).ok()?;
    (report.source_profile == source).then_some(report)
}

pub(crate) fn source_profile(home: &Path, selected: &str) -> io::Result<String> {
    let mut source = selected.to_owned();
    let mut seen = std::collections::HashSet::new();
    loop {
        validate_profile_name(&source)?;
        if !seen.insert(source.clone()) || seen.len() > 5 {
            return Err(io::Error::other("Compatibility profile source cycle"));
        }
        let marker = home.join("profiles").join(&source).join(".nexus-compatibility.json");
        let metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(source),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || is_link_or_reparse(&metadata) || metadata.len() > 64 * 1024 {
            return Err(io::Error::other("Invalid compatibility profile marker"));
        }
        let value: serde_json::Value = serde_json::from_slice(&fs::read(marker)?).map_err(io::Error::other)?;
        source = value.get("source_profile").and_then(|value| value.as_str())
            .ok_or_else(|| io::Error::other("Compatibility profile source is missing"))?.to_owned();
    }
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    { metadata.file_type().is_symlink() }
}

fn ensure_work_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => Ok(()),
        Ok(_) => Err(io::Error::other("Compatibility work directory cannot be a link or non-directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
        Err(error) => Err(error),
    }
}

fn read_plain_json(path: &Path) -> io::Result<serde_json::Value> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) || metadata.len() > 64 * 1024 {
        return Err(io::Error::other("Invalid compatibility policy or profile manifest"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

pub(crate) fn disabled_plugins(home: &Path, profile: &str) -> io::Result<Vec<String>> {
    let source = source_profile(&home, profile)?;
    let file = home.join("profiles/.nexus-plugin-isolation").join(format!("{source}.json"));
    match read_plain_json(&file) {
        Ok(value) => serde_json::from_value(value).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(crate) fn set_plugin_disabled(home: &Path, profile: &str, package: &str, disabled: bool) -> io::Result<()> {
    let source = source_profile(&home, profile)?;
    let profiles = home.join("profiles");
    ensure_work_directory(&profiles)?;
    let source_dir = profiles.join(&source);
    let metadata = fs::symlink_metadata(&source_dir)?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(io::Error::other("Compatibility source profile cannot be a link"));
    }
    let manifest = read_plain_json(&source_dir.join("package.json"))?;
    let bundles = manifest.pointer("/dsh/profile/bundles").and_then(|value| value.as_array())
        .ok_or_else(|| io::Error::other("Unsupported profile bundle manifest"))?;
    if package.starts_with("@deepseek-ai/") || !bundles.iter().any(|value| value.as_str() == Some(package)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Only third-party bundles from the original profile can be isolated"));
    }
    let policy_dir = profiles.join(".nexus-plugin-isolation");
    ensure_work_directory(&policy_dir)?;
    let policy_file = policy_dir.join(format!("{source}.json"));
    let mut policy: Vec<String> = match read_plain_json(&policy_file) {
        Ok(value) => serde_json::from_value(value).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    policy.retain(|item| item != package);
    if disabled { policy.push(package.to_owned()); }
    policy.sort();
    policy.dedup();
    write_json_atomic(&policy_dir, &policy_file, &policy)
}

pub(crate) async fn for_release(
    state: &crate::AppState,
    id: &str,
    force: bool,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    let Some(spec) = state.config.load()?.harness else { return Ok(()); };
    if spec.mode != HarnessLaunchMode::Node { return Ok(()); }
    let home = state.snapshots.configured_dsh_home()?;
    let profile = state.profiles.load()?.active_profile;
    let slot = state.releases.release_root(id)?;
    prepare(&state.paths, &home, &profile, id, &slot, &spec.program, force, cancellation).await?;
    Ok(())
}

pub(crate) async fn for_profile_selection(state: &crate::AppState, profile: &str) -> io::Result<()> {
    let Some(mut spec)=nexus_core::load_harness_launch_spec(&state.paths)? else{return Ok(())};
    if spec.mode!=HarnessLaunchMode::Node{return Ok(())}
    let source=crate::source_context::resolve_async(&state.paths,&state.releases).await?;
    let Some(slot)=source.root else{return Ok(())};
    let release=crate::source_context::compatibility_id(&state.paths,&state.releases)?.ok_or_else(||io::Error::other("Select a Harness source"))?;
    crate::supervisor::normalize_selected_launch(&mut spec,&state.paths,&state.releases)?;
    let runtime=crate::runtime::runtime_for_launch(&mut spec,state.config.load()?.runtime.unwrap_or_default(),nexus_core::bundled_runtime_dir().as_deref());
    let runtime_env=nexus_core::build_runtime_child_env(&runtime,std::env::var_os("PATH").as_deref())?;
    let node=spec.render_path_for_context(&spec.program,profile,Some(&release),Some(&slot))?;
    prepare_with_trigger(&state.paths, &state.snapshots.configured_dsh_home()?, profile,
        &release, &slot, &node, true,
        &CancellationToken::default(), "profile_switch", &runtime_env).await?;
    Ok(())
}

pub(crate) async fn check_selected(state: &crate::AppState, profile: &str) -> io::Result<()> {
    let mut spec = nexus_core::load_harness_launch_spec(&state.paths)?.ok_or_else(|| io::Error::other("Configure a Node Harness before verifying plugins"))?;
    if spec.mode != HarnessLaunchMode::Node { return Err(io::Error::other("Plugin verification requires a Node Harness")); }
    let source=crate::source_context::resolve_async(&state.paths,&state.releases).await?;
    let release=crate::source_context::compatibility_id(&state.paths,&state.releases)?.ok_or_else(||io::Error::other("Select a Harness source"))?;
    let slot=source.root.ok_or_else(||io::Error::other("Select a Harness source before verifying plugins"))?;
    crate::supervisor::normalize_selected_launch(&mut spec, &state.paths, &state.releases)?;
    let runtime = crate::runtime::runtime_for_launch(&mut spec, state.config.load()?.runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref());
    let runtime_env = nexus_core::build_runtime_child_env(&runtime, std::env::var_os("PATH").as_deref())?;
    let node = spec.render_path_for_context(&spec.program, profile, Some(&release), Some(&slot))?;
    if node.is_relative() && node.components().count() > 1 {
        return Err(io::Error::other("Independent plugin verification requires an absolute Node path or a runtime selected in settings"));
    }
    let args = spec.render_args_for_context(profile, Some(&release), Some(&slot))?;
    let expected_entry = fs::canonicalize(slot.join("apps/cli/lib/bin.js"))?;
    let managed_entry = args.first().and_then(|path| fs::canonicalize(path).ok()).as_ref() == Some(&expected_entry);
    if args.is_empty() || !managed_entry || !(args.len() == 3 && args[1] == "--profile" && args[2] == profile) {
        return Err(io::Error::other("Independent plugin verification requires the managed Harness entry and selected profile arguments; custom launch commands are not supported"));
    }
    prepare_with_trigger(&state.paths, &state.snapshots.configured_dsh_home()?, profile,
        &release, &slot, &node, true,
        &CancellationToken::default(), "manual_check", &runtime_env).await?
        .ok_or_else(|| io::Error::other("Selected profile has no native manifest to verify"))?;
    Ok(())
}

pub(crate) async fn prepare(
    paths: &NexusPaths, home: &Path, profile: &str, release: &str,
    slot: &Path, node: &Path, force: bool, cancellation: &CancellationToken,
) -> io::Result<Option<CompatibilityReport>> {
    prepare_with_trigger(paths, home, profile, release, slot, node, force, cancellation,
        if force { "version_switch" } else { "startup" }, &[]).await
}

async fn prepare_with_trigger(
    paths: &NexusPaths, home: &Path, profile: &str, release: &str,
    slot: &Path, node: &Path, force: bool, cancellation: &CancellationToken, trigger: &str,
    runtime_env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> io::Result<Option<CompatibilityReport>> {
    validate_profile_name(profile)?;
    let root = paths.root.join("compatibility");
    ensure_work_directory(&root)?;
    let lock_path = root.join("operation.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file() || is_link_or_reparse(&m)) {
        return Err(io::Error::other("Invalid compatibility operation lock"));
    }
    let lease = fs::File::options().read(true).write(true).create(true).truncate(false).open(lock_path)?;
    lease.try_lock().map_err(|_| io::Error::new(io::ErrorKind::ResourceBusy, "Compatibility check is still running"))?;
    recover_pending(&root, home)?;
    match fs::remove_file(root.join("latest.json")) {
        Ok(()) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error),
    }
    let pending = root.join("owner-pending.json");
    let preferences = nexus_core::load_harness_preferences(paths)?;
    crate::runtime_patches::validate_for_paths(paths, &preferences)?;
    let capabilities = crate::preference_capabilities::resolve(Some(slot), home, profile, &preferences)?;
    let preferences_env = nexus_core::harness_preferences_environment(&preferences, &capabilities);
    // A first-run installation may not have created a native profile yet.
    if !home.join("profiles").join(profile).join("package.json").is_file() { return Ok(None); }
    if !slot.join("apps/cli/lib/bin.js").is_file() {
        return Err(io::Error::other("Target release does not support the Node profile compatibility check"));
    }
    ensure_work_directory(&home.join("profiles"))?;
    let work_root = home.join("profiles/.nexus-compatibility-work");
    ensure_work_directory(&work_root)?;
    let nonce = nexus_core::unix_time_nanos_for_update();
    let work = work_root.join(format!("run-{nonce}"));
    nexus_core::create_new_private_directory(&work)?;
    let input = work.join("request.json");
    let output = work.join("result.json");
    let script = root.join("checker.mjs");
    fs::write(&script, include_bytes!("compatibility.mjs"))?;
    let desktop_patch = crate::desktop_plugins::stage(paths, home, profile)?;
    use sha2::Digest;
    nexus_core::write_private_json_atomic(&work, &input, &serde_json::json!({
        "home":home,"selected":profile,"release_id":release,"slot":slot,
        "node":node,"work":work,"output":output,"force":force,
        "trigger": trigger, "owned_round": true,
        "preference_capabilities": { "adapter_version": crate::preference_capabilities::VERIFIED_VERSION, "capabilities": capabilities },
        "patches": preferences.patches.as_deref().unwrap_or(&[]),
        "builtin_patches": [desktop_patch],
        "builtin_fingerprint": format!("{:x}", sha2::Sha256::digest(include_bytes!("../../../plugins/nexus-desktop-compat/index.mjs"))),
        "preferences_env": preferences_env.iter().filter(|(key, _)| key != "DSH_HOME")
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
            .collect::<std::collections::BTreeMap<_, _>>(),
    }))?;
    let mut command = std::process::Command::new(node);
    command.envs(runtime_env.iter().cloned());
    command.envs(preferences_env);
    let pnpm = nexus_core::ConfigStore::new(paths.clone()).load()?.runtime.and_then(|runtime| runtime.pnpm).map(|pin| pin.path);
    command.env("NEXUS_DESKTOP_CONTEXT", crate::desktop_plugins::context(paths, home, profile, node, slot, pnpm.as_deref())?);
    command.env("NEXUS_DESKTOP_PROBE", "1");
    command.arg(&script).arg(&input).current_dir(&root).stdin(Stdio::null()).stdout(Stdio::null());
    tracing::info!(release, profile, "checking target release plugin startup compatibility");
    write_json_atomic(&root, &pending, &serde_json::json!({"format_version":2,"work":work,"release":release,"profile":profile}))?;
    let mut result = crate::cold::run_owned_command(command, "plugin compatibility check", Duration::from_secs(600), &work, cancellation).await;
    if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) {
        // Keep the durable marker and all paths while process ownership is
        // unresolved. A subsequent launch must not reuse or delete this tree.
        return result.map(|_| None);
    }
    if result.is_ok() && work.join("publication.json").is_file() {
        let mut request: serde_json::Value = serde_json::from_slice(&fs::read(&input)?).map_err(io::Error::other)?;
        request["finalize"] = serde_json::json!(true);
        nexus_core::write_private_json_atomic(&work, &input, &request)?;
        let mut command = std::process::Command::new(node);
        command.arg(&script).arg(&input).current_dir(&root).stdin(Stdio::null()).stdout(Stdio::null());
        result = crate::cold::run_owned_command(command, "publish verified compatibility profile", Duration::from_secs(60), &work, cancellation).await;
        if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) { return result.map(|_| None); }
    }
    let bytes = fs::read(&output);
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    crate::cold::remove_owned_directory(&work_root, &work)?;
    fs::remove_file(&pending)?;
    if cancellation.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted,"Harness startup was cancelled")); }
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => { result?; return Err(error); }
    };
    if bytes.len() > 64 * 1024 { return Err(io::Error::other("Compatibility report exceeds limit")); }
    let (bytes, _) = nexus_core::redact_diagnostics_payload(&bytes);
    if result.is_err() && serde_json::from_slice::<serde_json::Value>(&bytes).ok().is_some_and(|v| v["publication_pending"] == true) { return result.map(|_| None); }
    let report: CompatibilityReport = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_profile_name(&report.source_profile)?;
    validate_profile_name(&report.effective_profile)?;
    if report.release_id != release || report.source_profile != source_profile(&home, profile)?
        || !matches!(report.status.as_str(), "passed" | "isolated" | "needs_choice") {
        return Err(io::Error::other("Compatibility report does not match target release"));
    }
    if report.status == "needs_choice" {
        if preferences.patches.as_ref().is_some_and(|patches| !patches.is_empty()) {
            crate::runtime_patches::record_failure(paths, &preferences, "compatibility_combination")?;
        }
        write_json_atomic(&root, &root.join("latest.json"), &report)?;
        return Err(io::Error::other(format!("Plugin compatibility needs a choice; disable selected third-party plugins and retry the target release. {}", report.error.as_deref().unwrap_or(""))));
    }
    result?;
    write_json_atomic(&root, &root.join("latest.json"), &report)?;
    tracing::info!(release, profile, isolated = report.disabled.len(), "plugin compatibility check passed");
    Ok(Some(report))
}

fn recover_pending(root: &Path, home: &Path) -> io::Result<()> {
    let pending = root.join("owner-pending.json");
    let Some(bytes) = nexus_core::read_regular_file_bounded(&pending, 64 * 1024)? else { return Ok(()); };
    let record: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.get("format_version").is_some_and(|version| version != 2) {
        return Err(io::Error::other("Unsupported compatibility recovery record; files retained"));
    }
    let work = std::path::PathBuf::from(record["work"].as_str().ok_or_else(|| io::Error::other("Compatibility recovery has no work identity"))?);
    let work_root = home.join("profiles/.nexus-compatibility-work");
    if work.parent() != Some(work_root.as_path()) || work.file_name().and_then(|s| s.to_str()).is_none_or(|s|
        !s.strip_prefix("run-").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))) {
        return Err(io::Error::other("Compatibility recovery path does not match the configured home; files retained"));
    }
    if work.try_exists()? {
        ensure_work_directory(&work_root)?;
        ensure_work_directory(&work)?;
        if record["format_version"] == 2 {
            crate::process_recovery::reconcile(&work.join("owned-processes"))?;
        } else if record.get("format_version").is_none() {
            crate::process_recovery::require_legacy_reboot(&pending)?;
        } else { return Err(io::Error::other("Unsupported compatibility recovery record; files retained")); }
        // Publication touches only atomic, fingerprinted generated projections.
        // Never replay an old publication against current user configuration.
        crate::cold::remove_owned_directory(&work_root, &work)?;
    }
    fs::remove_file(pending)?;
    tracing::info!("Recovered interrupted compatibility check; a fresh check will run");
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_check_recovers_each_cut_without_replaying_publication() {
        for cut in ["reserved", "command-prepared", "publication-prepared", "work-removed"] {
            let root = std::env::temp_dir().join(format!("nexus-compat-recovery-{}-{cut}", nexus_core::unix_time_nanos_for_update()));
            let home = root.join("home"); let control = root.join("compatibility");
            let work = home.join("profiles/.nexus-compatibility-work/run-123");
            fs::create_dir_all(&work).unwrap(); fs::create_dir(&control).unwrap();
            let original = home.join("profiles/source.json"); fs::write(&original, b"original").unwrap();
            write_json_atomic(&control, &control.join("owner-pending.json"), &serde_json::json!({"format_version":2,"work":work})).unwrap();
            if cut == "command-prepared" {
                let owner = crate::process_recovery::Owner::create(&work.join("owned-processes"), None).unwrap();
                assert!(recover_pending(&control, &home).is_err());
                assert!(work.exists() && control.join("owner-pending.json").exists());
                drop(owner);
            }
            if cut == "publication-prepared" { fs::write(work.join("publication.json"), b"do not replay").unwrap(); }
            if cut == "work-removed" { fs::remove_dir(&work).unwrap(); }
            recover_pending(&control, &home).unwrap(); recover_pending(&control, &home).unwrap();
            assert!(!work.exists() && !control.join("owner-pending.json").exists());
            assert_eq!(fs::read(&original).unwrap(), b"original");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn legacy_check_requires_positive_reboot_evidence_and_rejects_foreign_paths() {
        let root = std::env::temp_dir().join(format!("nexus-compat-legacy-{}", nexus_core::unix_time_nanos_for_update()));
        let home = root.join("home"); let control = root.join("compatibility");
        let work = home.join("profiles/.nexus-compatibility-work/run-123");
        fs::create_dir_all(&work).unwrap(); fs::create_dir(&control).unwrap();
        let request = work.join("request.json"); fs::write(&request, b"{}").unwrap();
        let pending = control.join("owner-pending.json");
        write_json_atomic(&control, &pending, &serde_json::json!({"work":work})).unwrap();
        assert!(recover_pending(&control, &home).unwrap_err().to_string().contains("Restart the computer once"));
        assert!(work.exists() && pending.exists());
        fs::File::options().write(true).open(&pending).unwrap().set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(86400))).unwrap();
        recover_pending(&control, &home).unwrap();
        let foreign = root.join("foreign"); fs::create_dir(&foreign).unwrap();
        write_json_atomic(&control, &pending, &serde_json::json!({"format_version":2,"work":foreign})).unwrap();
        assert!(recover_pending(&control, &home).is_err()); assert!(foreign.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn latest_report_is_bound_to_release_and_logical_source() {
        let root = std::env::temp_dir().join(format!("nexus-compat-selection-{}-{}",
            std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let home = root.join("home");
        let projection = home.join("profiles/nexus-projection");
        fs::create_dir_all(&projection).unwrap();
        fs::create_dir_all(root.join("compatibility")).unwrap();
        fs::write(projection.join(".nexus-compatibility.json"),
            br#"{"source_profile":"original"}"#).unwrap();
        let report = CompatibilityReport {
            checker_version: 1, status: "passed".to_owned(), source_profile: "original".to_owned(),
            effective_profile: "nexus-projection".to_owned(), release_id: "release-a".to_owned(),
            fingerprint: "fixture".to_owned(), checked_at_unix: 1, checked_disabled_plugins: None, disabled: Vec::new(), error: None, candidates: Vec::new(),
            trigger: None, last_trigger: None, last_used_at_unix: None, cache_reused: false,
        };
        write_json_atomic(&root.join("compatibility"), &root.join("compatibility/latest.json"), &report).unwrap();
        assert_eq!(latest_for_selection(&paths, &home, "original", Some("release-a")), Some(report.clone()));
        assert_eq!(latest_for_selection(&paths, &home, "nexus-projection", Some("release-a")), Some(report));
        assert!(latest_for_selection(&paths, &home, "other", Some("release-a")).is_none());
        assert!(latest_for_selection(&paths, &home, "original", Some("release-b")).is_none());
        assert!(latest_for_selection(&paths, &home, "original", None).is_none());
        let mut failed = latest(&paths).unwrap();
        failed.status = "needs_choice".to_owned();
        failed.release_id = "not-yet-selected".to_owned();
        failed.error = Some("Third-party initialization requires a choice".to_owned());
        write_json_atomic(&root.join("compatibility"), &root.join("compatibility/latest.json"), &failed).unwrap();
        assert_eq!(latest_for_selection(&paths, &home, "original", Some("release-a")), Some(failed));
        assert!(latest_for_selection(&paths, &home, "other", Some("release-a")).is_none());
        let mut target_profile = latest(&paths).unwrap();
        target_profile.trigger = Some("profile_switch".to_owned());
        target_profile.last_trigger = Some("profile_switch".to_owned());
        target_profile.release_id = "release-a".to_owned();
        write_json_atomic(&root.join("compatibility"), &root.join("compatibility/latest.json"), &target_profile).unwrap();
        assert_eq!(latest_for_selection(&paths, &home, "other", Some("release-a")), Some(target_profile));
        assert!(latest_for_selection(&paths, &home, "other", Some("release-b")).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_isolation_preserves_original_and_rejects_official_bundles() {
        let home = std::env::temp_dir().join(format!("nexus-compat-policy-{}-{}",
            std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let profile = home.join("profiles/original");
        fs::create_dir_all(&profile).unwrap();
        let manifest = br#"{"dsh":{"profile":{"bundles":["third-party","@deepseek-ai/core"]}}}"#;
        fs::write(profile.join("package.json"), manifest).unwrap();
        set_plugin_disabled(&home, "original", "third-party", true).unwrap();
        let policy = home.join("profiles/.nexus-plugin-isolation/original.json");
        assert_eq!(read_plain_json(&policy).unwrap(), serde_json::json!(["third-party"]));
        assert!(set_plugin_disabled(&home, "original", "@deepseek-ai/core", true).is_err());
        assert!(set_plugin_disabled(&home, "original", "not-in-manifest", true).is_err());
        assert_eq!(read_plain_json(&policy).unwrap(), serde_json::json!(["third-party"]));
        assert_eq!(fs::read(profile.join("package.json")).unwrap(), manifest);
        set_plugin_disabled(&home, "original", "third-party", false).unwrap();
        assert_eq!(read_plain_json(&policy).unwrap(), serde_json::json!([]));
        assert_eq!(fs::read(profile.join("package.json")).unwrap(), manifest);
        fs::remove_dir_all(home).unwrap();
    }
}
