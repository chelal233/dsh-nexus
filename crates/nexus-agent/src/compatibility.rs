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
    if !matches!(report.status.as_str(), "passed" | "isolated" | "needs_choice" | "failed")
        || (!matches!(report.status.as_str(), "needs_choice" | "failed") && Some(report.release_id.as_str()) != release) {
        return None;
    }
    if matches!(report.status.as_str(), "needs_choice" | "failed") && report.trigger.as_deref() == Some("profile_switch") {
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
        let live = home.join("profiles").join(&source).join(".nexus-compatibility.json");
        let marker = if live.try_exists()? { live } else { home.join(".nexus-retired-profiles").join(&source).join(".nexus-compatibility.json") };
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
    serde_json::from_slice(&fs::read(path)?).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display())))
}

fn retire_projections(home: &Path) -> io::Result<()> {
    let profiles = home.join("profiles");
    let retired = home.join(".nexus-retired-profiles");
    ensure_work_directory(&profiles)?;
    for entry in fs::read_dir(&profiles)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = fs::symlink_metadata(entry.path())?;
        if validate_profile_name(&name).is_err() || !metadata.is_dir() || is_link_or_reparse(&metadata) { continue; }
        let marker = entry.path().join(".nexus-compatibility.json");
        let value = match read_plain_json(&marker) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let report: CompatibilityReport = serde_json::from_value(value).map_err(io::Error::other)?;
        validate_profile_name(&report.source_profile)?;
        if report.effective_profile != name || report.source_profile == name || !matches!(report.status.as_str(), "passed" | "isolated") {
            return Err(io::Error::other("Invalid legacy profile identity; retained"));
        }
        ensure_work_directory(&retired)?;
        let destination = retired.join(&name);
        if destination.try_exists()? { return Err(io::Error::other("Retired profile destination already exists; both copies retained")); }
        // Same-volume rename is atomic across interruption. Unknown files and
        // user edits remain intact; archived markers resolve old references.
        fs::rename(entry.path(), destination)?;
    }
    Ok(())
}

pub(crate) fn disabled_plugins(home: &Path, profile: &str) -> io::Result<Vec<String>> {
    let source = source_profile(&home, profile)?;
    let file = home.join("profiles/.nexus-plugin-isolation").join(format!("{source}.json"));
    let mut legacy: Vec<String> = match read_plain_json(&file) {
        Ok(value) => serde_json::from_value(value).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }?;
    let manifest = match read_plain_json(&home.join("profiles").join(&source).join("package.json")) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error),
    };
    if manifest.pointer("/dsh/profile/nexusIsolationPolicyVersion").and_then(|v| v.as_u64()) == Some(1) { legacy.clear(); }
    if let Some(records) = manifest.pointer("/dsh/profile/nexusDisabledBundles") {
        for record in records.as_array().ok_or_else(|| io::Error::other("Invalid disabled bundle metadata"))? {
            let package = record["package"].as_str().ok_or_else(|| io::Error::other("Invalid disabled bundle metadata"))?;
            if !legacy.iter().any(|value| value == package) { legacy.push(package.to_owned()); }
        }
    }
    Ok(legacy)
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
    let mut manifest = read_plain_json(&source_dir.join("package.json"))?;
    let bundles = manifest.pointer("/dsh/profile/bundles").and_then(|value| value.as_array())
        .ok_or_else(|| io::Error::other("Unsupported profile bundle manifest"))?;
    let mut records = manifest.pointer("/dsh/profile/nexusDisabledBundles").cloned().unwrap_or_else(|| serde_json::json!([]))
        .as_array().cloned().ok_or_else(|| io::Error::other("Invalid disabled bundle metadata"))?;
    let mut bundles = bundles.clone();
    if manifest.pointer("/dsh/profile/nexusIsolationPolicyVersion").and_then(|v| v.as_u64()) != Some(1) {
        let legacy_file = profiles.join(".nexus-plugin-isolation").join(format!("{source}.json"));
        let legacy: Vec<String> = match read_plain_json(&legacy_file) {
            Ok(value) => serde_json::from_value(value).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for item in legacy {
            if item.starts_with("@deepseek-ai/") { return Err(io::Error::other("Invalid legacy isolation policy")); }
            if let Some(index) = bundles.iter().position(|value| value.as_str() == Some(&item)) {
                if !records.iter().any(|record| record["package"].as_str() == Some(&item)) {
                    records.push(serde_json::json!({"package":item,"index":index,"following":bundles[index+1..]}));
                }
                bundles.remove(index);
            }
        }
    }
    let saved = records.iter().find(|item| item["package"].as_str() == Some(package)).cloned();
    if package.starts_with("@deepseek-ai/") || (!bundles.iter().any(|value| value.as_str() == Some(package)) && saved.is_none()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Only third-party bundles from the original profile can be isolated"));
    }
    if disabled {
        if let Some(index) = bundles.iter().position(|value| value.as_str() == Some(package)) {
            if saved.is_none() { records.push(serde_json::json!({"package":package,"index":index,"following":bundles[index+1..]})); }
            bundles.remove(index);
        }
    } else if let Some(saved) = saved {
        if !bundles.iter().any(|value| value.as_str() == Some(package)) {
            let index = saved["following"].as_array().and_then(|following| following.iter().find_map(|next| bundles.iter().position(|value| value == next)))
                .unwrap_or_else(|| (saved["index"].as_u64().unwrap_or(bundles.len() as u64) as usize).min(bundles.len()));
            bundles.insert(index, serde_json::json!(package));
        }
        records.retain(|item| item["package"].as_str() != Some(package));
    }
    manifest["dsh"]["profile"]["bundles"] = serde_json::json!(bundles);
    manifest["dsh"]["profile"]["nexusIsolationPolicyVersion"] = serde_json::json!(1);
    if records.is_empty() { manifest["dsh"]["profile"].as_object_mut().unwrap().remove("nexusDisabledBundles"); }
    else { manifest["dsh"]["profile"]["nexusDisabledBundles"] = serde_json::json!(records); }
    // One atomic document owns both the active order and restoration metadata.
    // A process interruption cannot publish only one half of the change.
    write_json_atomic(&source_dir, &source_dir.join("package.json"), &manifest)
}

pub(crate) async fn for_release(
    state: &crate::AppState,
    id: &str,
    force: bool,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    let Some(mut spec) = nexus_core::load_harness_launch_spec(&state.paths)? else { return Ok(()); };
    if spec.mode != HarnessLaunchMode::Node { return Ok(()); }
    let home = state.snapshots.configured_dsh_home()?;
    let profile = state.profiles.load()?.active_profile;
    let slot = state.releases.release_root(id)?;
    crate::supervisor::normalize_selected_launch(&mut spec, &state.paths, &state.releases)?;
    let runtime = crate::runtime::runtime_for_launch(&mut spec, state.config.load()?.runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref());
    let runtime_env = nexus_core::build_runtime_child_env(&runtime, std::env::var_os("PATH").as_deref())?;
    let node = spec.render_path_for_context(&spec.program, &profile, Some(id), Some(&slot))?;
    prepare(&state.paths, &home, &profile, id, &slot, &node, force, cancellation,
        "version_switch", &runtime_env).await?;
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
    prepare(&state.paths, &state.snapshots.configured_dsh_home()?, profile,
        &release, &slot, &node, true,
        &CancellationToken::default(), "manual_check", &runtime_env).await?
        .ok_or_else(|| io::Error::other("Selected profile has no native manifest to verify"))?;
    Ok(())
}

pub(crate) async fn prepare(
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
    recover_orphan_work(&root.join("work"))?;
    schedule_retired_cleanup(&root.join("work"));
    let source = source_profile(home, profile)?;
    if source != profile {
        return Err(io::Error::other(format!("Legacy generated profile {profile} is selected. Select source profile {source} explicitly before starting; all legacy files are retained for recovery.")));
    }
    let legacy_policy = home.join("profiles/.nexus-plugin-isolation").join(format!("{source}.json"));
    let mut legacy: Vec<String> = match read_plain_json(&legacy_policy) {
        Ok(value) => serde_json::from_value(value).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if read_plain_json(&home.join("profiles").join(profile).join("package.json")).ok()
        .and_then(|value| value.pointer("/dsh/profile/nexusIsolationPolicyVersion").and_then(|v| v.as_u64())) == Some(1) { legacy.clear(); }
    // The first atomic manifest edit imports the entire legacy selection.
    if let Some(package) = legacy.first() {
        set_plugin_disabled(home, profile, &package, true)?;
    }
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
    if trigger == "startup_direct" && !force && crate::desktop_plugins::single_start_supported(slot, home, profile) {
        tracing::info!(release, profile, "Checking startup in the actual Harness instance");
        return Ok(None);
    }
    let work_root = root.join("work");
    ensure_work_directory(&work_root)?;
    let nonce = nexus_core::unix_time_nanos_for_update();
    let work = work_root.join(format!("run-{nonce}"));
    nexus_core::create_new_private_directory(&work)?;
    let input = work.join("request.json");
    let output = work.join("result.json");
    let script = root.join("checker.mjs");
    fs::write(&script, include_bytes!("compatibility.mjs"))?;
    let vendor = script.parent().unwrap().join("vendor");
    fs::create_dir_all(&vendor)?;
    fs::write(vendor.join("semver.cjs"), include_bytes!("vendor/semver.cjs"))?;
    fs::write(vendor.join("semver.LICENSE"), include_bytes!("vendor/semver.LICENSE"))?;
    let desktop_patch = crate::desktop_plugins::stage(paths, home, profile)?;
    use sha2::Digest;
    nexus_core::write_private_json_atomic(&work, &input, &serde_json::json!({
        "home":home,"selected":profile,"release_id":release,"slot":slot,"cache":root.join("verified.json"),
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
    let result = crate::cold::run_owned_command(command, "plugin compatibility check", Duration::from_secs(600), &work, cancellation).await;
    if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) {
        // Keep the durable marker and all paths while process ownership is
        // unresolved. A subsequent launch must not reuse or delete this tree.
        return result.map(|_| None);
    }
    let bytes = fs::read(&output);
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    // The owned probe has exited. Retire its private copy atomically; deleting
    // thousands of isolated dependency files need not delay the real launch.
    retire_completed_work(&work_root, &work)?;
    fs::remove_file(&pending)?;
    schedule_retired_cleanup(&work_root);
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
        || !matches!(report.status.as_str(), "passed" | "isolated" | "needs_choice" | "failed") {
        return Err(io::Error::other("Compatibility report does not match target release"));
    }
    if report.status == "failed" {
        write_json_atomic(&root, &root.join("latest.json"), &report)?;
        return Err(io::Error::other(report.error.as_deref().unwrap_or("Compatibility preparation failed")));
    }
    if report.status == "needs_choice" {
        if preferences.patches.as_ref().is_some_and(|patches| !patches.is_empty()) {
            crate::runtime_patches::record_failure(paths, &preferences, "compatibility_combination")?;
        }
        write_json_atomic(&root, &root.join("latest.json"), &report)?;
        return Err(io::Error::other(format!("Plugin compatibility needs a choice; disable selected third-party plugins and retry the target release. {}", report.error.as_deref().unwrap_or(""))));
    }
    result?;
    // Probe ownership has settled. Archive old copies intact, never merge them
    // into the source or delete potentially edited configuration.
    retire_projections(home)?;
    let profiles = nexus_core::ProfileStore::new(paths.clone());
    let mut catalog = profiles.load()?;
    let before = catalog.profiles.clone();
    catalog.profiles.retain(|name| !home.join(".nexus-retired-profiles").join(name).join(".nexus-compatibility.json").is_file());
    if catalog.profiles != before { profiles.write(&catalog)?; }
    write_json_atomic(&root, &root.join("latest.json"), &report)?;
    tracing::info!(release, profile, isolated = report.disabled.len(), "plugin compatibility check passed");
    Ok(Some(report))
}

fn retire_completed_work(root: &Path, work: &Path) -> io::Result<()> {
    ensure_work_directory(root)?;
    ensure_work_directory(work)?;
    if work.parent() != Some(root) { return Err(io::Error::other("Invalid completed compatibility work path")); }
    let name = work.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_prefix("run-"))
        .filter(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| io::Error::other("Invalid completed compatibility work name"))?;
    crate::process_recovery::reconcile(&work.join("owned-processes"))?;
    fs::rename(work, root.join(format!("retired-{name}")))
}

fn schedule_retired_cleanup(root: &Path) {
    use std::sync::{Mutex, OnceLock};
    static QUEUED: OnceLock<Mutex<std::collections::HashSet<std::path::PathBuf>>> = OnceLock::new();
    let queued = QUEUED.get_or_init(Default::default);
    let Ok(entries) = fs::read_dir(root) else { return; };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().and_then(|name| name.strip_prefix("retired-"))
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())) { continue; }
        let target = entry.path();
        if !queued.lock().unwrap().insert(target.clone()) { continue; }
        let root = root.to_owned();
        tokio::task::spawn_blocking(move || {
            let result = ensure_work_directory(&root).and_then(|_| ensure_work_directory(&target))
                .and_then(|_| crate::process_recovery::reconcile(&target.join("owned-processes")))
                .and_then(|_| crate::cold::remove_owned_directory(&root, &target));
            if let Err(error) = result { tracing::warn!(%error, "Completed startup check cleanup deferred until next launch"); }
            queued.lock().unwrap().remove(&target);
        });
    }
}

fn recover_orphan_work(root: &Path) -> io::Result<()> {
    if !root.try_exists()? { return Ok(()); }
    ensure_work_directory(root)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.strip_prefix("run-").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())) { continue; }
        ensure_work_directory(&entry.path())?;
        crate::process_recovery::reconcile(&entry.path().join("owned-processes"))?;
        crate::cold::remove_owned_directory(root, &entry.path())?;
    }
    Ok(())
}

fn recover_pending(root: &Path, home: &Path) -> io::Result<()> {
    let pending = root.join("owner-pending.json");
    let Some(bytes) = nexus_core::read_regular_file_bounded(&pending, 64 * 1024)? else { return Ok(()); };
    let record: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if record.get("format_version").is_some_and(|version| version != 2) {
        return Err(io::Error::other("Unsupported compatibility recovery record; files retained"));
    }
    let work = std::path::PathBuf::from(record["work"].as_str().ok_or_else(|| io::Error::other("Compatibility recovery has no work identity"))?);
    let current_root = root.join("work");
    let legacy_root = home.join("profiles/.nexus-compatibility-work");
    let work_root = if work.parent() == Some(current_root.as_path()) { current_root } else { legacy_root };
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

    #[tokio::test]
    async fn completed_probe_is_retired_only_after_ownership_settles_and_reclaimed_in_background() {
        let root = std::env::temp_dir().join(format!("nexus-retired-work-{}", nexus_core::unix_time_nanos_for_update()));
        let work = root.join("run-123");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("isolated-copy"), "fixture").unwrap();
        let owner = crate::process_recovery::Owner::create(&work.join("owned-processes"), None).unwrap();
        assert!(retire_completed_work(&root, &work).is_err());
        assert!(work.exists());
        drop(owner);
        retire_completed_work(&root, &work).unwrap();
        assert!(!work.exists());
        assert!(root.join("retired-123/isolated-copy").exists());
        // The normal orphan recovery must not block on retired copies.
        recover_orphan_work(&root).unwrap();
        assert!(root.join("retired-123").exists());
        schedule_retired_cleanup(&root);
        schedule_retired_cleanup(&root);
        tokio::time::timeout(Duration::from_secs(5), async {
            while root.join("retired-123").exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

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
        diagnosis: None,        failure_stage: None,        dependency_origins: Vec::new(),
            declarations: Vec::new(), declarations_omitted: 0, checker_version: 1, status: "passed".to_owned(), source_profile: "original".to_owned(),
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
    fn manual_isolation_edits_one_native_manifest_and_restores_order() {
        let home = std::env::temp_dir().join(format!("nexus-compat-policy-{}-{}",
            std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let profile = home.join("profiles/original");
        fs::create_dir_all(&profile).unwrap();
        let manifest = br#"{"dependencies":{"third-party":"1"},"custom":"keep","dsh":{"profile":{"bundles":["third-party","second","@deepseek-ai/core"]}}}"#;
        fs::write(profile.join("package.json"), manifest).unwrap();
        set_plugin_disabled(&home, "original", "third-party", true).unwrap();
        assert_eq!(disabled_plugins(&home, "original").unwrap(), vec!["third-party"]);
        assert_eq!(read_plain_json(&profile.join("package.json")).unwrap()["dsh"]["profile"]["bundles"], serde_json::json!(["second","@deepseek-ai/core"]));
        assert!(set_plugin_disabled(&home, "original", "@deepseek-ai/core", true).is_err());
        assert!(set_plugin_disabled(&home, "original", "not-in-manifest", true).is_err());
        set_plugin_disabled(&home, "original", "second", true).unwrap();
        set_plugin_disabled(&home, "original", "third-party", false).unwrap();
        set_plugin_disabled(&home, "original", "second", false).unwrap();
        assert!(disabled_plugins(&home, "original").unwrap().is_empty());
        let mut restored = read_plain_json(&profile.join("package.json")).unwrap();
        restored["dsh"]["profile"].as_object_mut().unwrap().remove("nexusIsolationPolicyVersion");
        assert_eq!(restored, serde_json::from_slice::<serde_json::Value>(manifest).unwrap());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn legacy_policy_import_is_atomic_and_cannot_re_disable_after_enable() {
        let home = std::env::temp_dir().join(format!("nexus-policy-upgrade-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir_all(home.join("profiles/web")).unwrap();
        fs::create_dir(home.join("profiles/.nexus-plugin-isolation")).unwrap();
        write_json_atomic(&home, &home.join("profiles/web/package.json"), &serde_json::json!({"dsh":{"profile":{"bundles":["a","b","c"]}}})).unwrap();
        write_json_atomic(&home, &home.join("profiles/.nexus-plugin-isolation/web.json"), &serde_json::json!(["a","b"])).unwrap();
        set_plugin_disabled(&home,"web","a",true).unwrap();
        assert_eq!(disabled_plugins(&home,"web").unwrap(), vec!["a","b"]);
        set_plugin_disabled(&home,"web","a",false).unwrap();
        assert_eq!(disabled_plugins(&home,"web").unwrap(), vec!["b"]);
        set_plugin_disabled(&home,"web","b",false).unwrap();
        assert_eq!(read_plain_json(&home.join("profiles/web/package.json")).unwrap()["dsh"]["profile"]["bundles"], serde_json::json!(["a","b","c"]));
        assert!(disabled_plugins(&home,"web").unwrap().is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn legacy_profiles_are_archived_intact_and_old_references_still_resolve() {
        let home = std::env::temp_dir().join(format!("nexus-retire-{}", nexus_core::unix_time_nanos_for_update()));
        for name in ["web","nexus-user","nexus-old"] { fs::create_dir_all(home.join("profiles").join(name)).unwrap(); }
        let legacy = home.join("profiles/nexus-old");
        fs::write(legacy.join("user-edit.txt"), "preserve every byte").unwrap();
        write_json_atomic(&home, &legacy.join(".nexus-compatibility.json"), &serde_json::json!({
            "checker_version":2,"status":"passed","source_profile":"web","effective_profile":"nexus-old",
            "release_id":"old","fingerprint":"fixture","checked_at_unix":1,"disabled":[]
        })).unwrap();
        retire_projections(&home).unwrap(); retire_projections(&home).unwrap();
        assert!(!legacy.exists()); assert!(home.join("profiles/nexus-user").is_dir());
        assert_eq!(fs::read_to_string(home.join(".nexus-retired-profiles/nexus-old/user-edit.txt")).unwrap(), "preserve every byte");
        assert_eq!(source_profile(&home,"nexus-old").unwrap(), "web");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn interrupted_new_work_is_reclaimed_only_after_process_ownership_settles() {
        let root = std::env::temp_dir().join(format!("nexus-work-recovery-{}", nexus_core::unix_time_nanos_for_update()));
        let work = root.join("run-123"); fs::create_dir_all(&work).unwrap();
        let owner = crate::process_recovery::Owner::create(&work.join("owned-processes"), None).unwrap();
        assert!(recover_orphan_work(&root).is_err()); assert!(work.exists());
        drop(owner); recover_orphan_work(&root).unwrap(); assert!(!work.exists());
        fs::create_dir(&work).unwrap(); fs::write(work.join("request.json"), b"{}").unwrap();
        recover_orphan_work(&root).unwrap(); assert!(!work.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
