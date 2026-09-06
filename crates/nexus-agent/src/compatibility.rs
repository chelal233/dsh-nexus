//! Version-bound, startup-only plugin compatibility checks.
use std::{fs, io, path::Path, process::Stdio, time::Duration};
use nexus_core::{validate_profile_name, write_json_atomic, NexusPaths};
use nexus_protocol::{CompatibilityReport, HarnessLaunchMode};
use nexus_runtime_supply::CancellationToken;

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
    let source = source_profile(home, selected).ok()?;
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
    let source = source_profile(home, profile)?;
    let file = home.join("profiles/.nexus-plugin-isolation").join(format!("{source}.json"));
    match read_plain_json(&file) {
        Ok(value) => serde_json::from_value(value).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(crate) fn set_plugin_disabled(home: &Path, profile: &str, package: &str, disabled: bool) -> io::Result<()> {
    let source = source_profile(home, profile)?;
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
    prepare(&state.paths, home, &profile, id, &slot, &spec.program, force, cancellation).await?;
    Ok(())
}

pub(crate) async fn prepare(
    paths: &NexusPaths, home: &Path, profile: &str, release: &str,
    slot: &Path, node: &Path, force: bool, cancellation: &CancellationToken,
) -> io::Result<Option<CompatibilityReport>> {
    validate_profile_name(profile)?;
    let root = paths.root.join("compatibility");
    ensure_work_directory(&root)?;
    match fs::remove_file(root.join("latest.json")) {
        Ok(()) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Err(error) => return Err(error),
    }
    let pending = root.join("owner-pending.json");
    if fs::symlink_metadata(&pending).is_ok() {
        return Err(io::Error::other("Compatibility process cleanup is pending; reconcile the owned probe before retrying"));
    }
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
    fs::create_dir(&work)?;
    let input = root.join(format!("request-{nonce}.json"));
    let output = root.join(format!("result-{nonce}.json"));
    let script = root.join("checker.mjs");
    fs::write(&script, include_bytes!("compatibility.mjs"))?;
    write_json_atomic(&root, &input, &serde_json::json!({
        "home":home,"selected":profile,"release_id":release,"slot":slot,
        "node":node,"work":work,"output":output,"force":force,
    }))?;
    let mut command = std::process::Command::new(node);
    command.arg(&script).arg(&input).current_dir(&root).stdin(Stdio::null()).stdout(Stdio::null());
    tracing::info!(release, profile, "checking target release plugin startup compatibility");
    write_json_atomic(&root, &pending, &serde_json::json!({"work":work,"release":release,"profile":profile}))?;
    let result = crate::cold::run_owned_command(command, "plugin compatibility check", Duration::from_secs(600), &root, cancellation).await;
    if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) {
        // Keep the durable marker and all paths while process ownership is
        // unresolved. A subsequent launch must not reuse or delete this tree.
        return result.map(|_| None);
    }
    let bytes = fs::read(&output);
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    crate::cold::remove_owned_directory(&work_root, &work)?;
    fs::remove_file(&pending)?;
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(error) => { result?; return Err(error); }
    };
    if bytes.len() > 64 * 1024 { return Err(io::Error::other("Compatibility report exceeds limit")); }
    let report: CompatibilityReport = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_profile_name(&report.source_profile)?;
    validate_profile_name(&report.effective_profile)?;
    if report.release_id != release || report.source_profile != source_profile(home, profile)?
        || !matches!(report.status.as_str(), "passed" | "isolated" | "needs_choice") {
        return Err(io::Error::other("Compatibility report does not match target release"));
    }
    if report.status == "needs_choice" {
        write_json_atomic(&root, &root.join("latest.json"), &report)?;
        return Err(io::Error::other("Plugin compatibility needs a choice; disable selected third-party plugins and retry the target release"));
    }
    result?;
    write_json_atomic(&root, &root.join("latest.json"), &report)?;
    tracing::info!(release, profile, isolated = report.disabled.len(), "plugin compatibility check passed");
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            fingerprint: "fixture".to_owned(), checked_at_unix: 1, disabled: Vec::new(), error: None, candidates: Vec::new(),
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
