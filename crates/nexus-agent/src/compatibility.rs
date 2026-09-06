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
    if Some(report.release_id.as_str()) != release
        || !matches!(report.status.as_str(), "passed" | "isolated") {
        return None;
    }
    let source = source_profile(home, selected).ok()?;
    (report.source_profile == source).then_some(report)
}

fn source_profile(home: &Path, selected: &str) -> io::Result<String> {
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
    let bytes = if result.is_ok() { fs::read(&output) } else { Ok(Vec::new()) };
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    crate::cold::remove_owned_directory(&work_root, &work)?;
    fs::remove_file(&pending)?;
    result?;
    let bytes = bytes?;
    if bytes.len() > 64 * 1024 { return Err(io::Error::other("Compatibility report exceeds limit")); }
    let report: CompatibilityReport = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    validate_profile_name(&report.source_profile)?;
    validate_profile_name(&report.effective_profile)?;
    if report.release_id != release || report.source_profile != source_profile(home, profile)?
        || !matches!(report.status.as_str(), "passed" | "isolated") {
        return Err(io::Error::other("Compatibility report does not match target release"));
    }
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
            fingerprint: "fixture".to_owned(), checked_at_unix: 1, disabled: Vec::new(),
        };
        write_json_atomic(&root.join("compatibility"), &root.join("compatibility/latest.json"), &report).unwrap();
        assert_eq!(latest_for_selection(&paths, &home, "original", Some("release-a")), Some(report.clone()));
        assert_eq!(latest_for_selection(&paths, &home, "nexus-projection", Some("release-a")), Some(report));
        assert!(latest_for_selection(&paths, &home, "other", Some("release-a")).is_none());
        assert!(latest_for_selection(&paths, &home, "original", Some("release-b")).is_none());
        assert!(latest_for_selection(&paths, &home, "original", None).is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
