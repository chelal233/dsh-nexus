//! Offline packaging owns one ordinary cold operation; imports use its durable
//! publication tuple. Package integrity is not publisher authentication.
use super::*;
use nexus_protocol::{RuntimeOwnership, UpdateAction};
const HELPER: &str = include_str!("../scripts/offline-package.mjs");
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(1800);

fn archive_path(paths: &NexusPaths, value: &str, exporting: bool) -> io::Result<PathBuf> {
    if value.len() > 4096 || !value.to_ascii_lowercase().ends_with(".tar.gz") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Select a complete .tar.gz path"));
    }
    let supplied = PathBuf::from(value);
    if !supplied.is_absolute() { return Err(io::Error::other("Offline archive path must be absolute")); }
    let canonical = if exporting {
        let parent = fs::canonicalize(supplied.parent().ok_or_else(|| io::Error::other("Missing archive parent"))?)?;
        let target = parent.join(supplied.file_name().ok_or_else(|| io::Error::other("Missing archive filename"))?);
        if fs::symlink_metadata(&target).is_ok() { return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Export destination already exists")); }
        target
    } else {
        let metadata = fs::symlink_metadata(&supplied)?;
        if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 12 * 1024 * 1024 * 1024 {
            return Err(io::Error::other("Import requires an ordinary bounded archive file"));
        }
        fs::canonicalize(supplied)?
    };
    if exporting && canonical.starts_with(fs::canonicalize(&paths.root)?) {
        return Err(io::Error::other("Export outside the managed Nexus data directory so cleanup cannot remove the package"));
    }
    Ok(canonical)
}

fn complete_runtime(state: &AppState) -> io::Result<PathBuf> {
    let config = state.config.load()?;
    let pinned = config.runtime.as_ref().and_then(|runtime| runtime.node.as_ref());
    let root = if let Some(pin) = pinned {
        pin.path.parent().and_then(Path::parent).map(Path::to_path_buf)
    } else { nexus_core::bundled_runtime_dir() }.ok_or_else(|| io::Error::other("No complete portable runtime is available"))?;
    validate_runtime_root(&root)?;
    if let Some(pin) = config.runtime.as_ref().and_then(|runtime| runtime.pnpm.as_ref()) {
        if fs::canonicalize(&pin.path)? != fs::canonicalize(root.join("pnpm/bin/pnpm.cjs"))? {
            return Err(io::Error::other("Offline export requires Node and pnpm from the same complete portable runtime"));
        }
    }
    fs::canonicalize(root)
}

fn validate_runtime_root(root: &Path) -> io::Result<PathBuf> {
    for relative in ["node/node.exe", "node/npm.cmd", "node/node_modules/npm/bin/npm-cli.js", "pnpm/bin/pnpm.cjs"] {
        let metadata = fs::symlink_metadata(root.join(relative))?;
        if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Offline packaging requires a complete portable Node/npm/pnpm runtime")); }
    }
    fs::canonicalize(root)
}

fn import_tools() -> io::Result<PathBuf> {
    let root = nexus_core::bundled_runtime_dir().ok_or_else(|| io::Error::other("Offline import needs the complete runtime included with Nexus; reinstall Nexus to restore it"))?;
    validate_runtime_root(&root)
}

pub(crate) async fn begin(state: &AppState, action: UpdateAction, archive: &str, release: Option<&str>) -> io::Result<ColdOperation> {
    if !cfg!(all(windows, target_arch = "x86_64")) { return Err(io::Error::other("Offline packages currently support Windows x64")); }
    let exporting = action == UpdateAction::OfflineExport;
    let archive = archive_path(&state.paths, archive, exporting)?;
    let runtime = if exporting { complete_runtime(state)? } else { import_tools()? };
    if exporting && archive.starts_with(&runtime) { return Err(io::Error::other("Export destination cannot be inside the source runtime")); }
    let release = if exporting {
        Some(state.releases.get(release.ok_or_else(|| io::Error::other("Select an installed release to export"))?)?)
    } else { None };
    let tag = release.as_ref().map(|item| item.version.clone()).unwrap_or_else(|| "offline-import".into());
    state.cold.begin_with_details(tag, nexus_protocol::RuntimeSource::Official, nexus_protocol::RuntimeInstallMode::Portable,
        if exporting { "offline_export" } else { "offline_import" }, Some(archive.to_string_lossy().into_owned()), release.map(|item| item.id)).await
}

async fn helper(state: &AppState, operation: &ColdOperation, tools: &Path, action: &str, slot: Option<&Path>, runtime: Option<&Path>) -> io::Result<()> {
    let work = PathBuf::from(&operation.candidate);
    let script = work.join("offline-package.mjs");
    let job = work.join("offline-job.json");
    let content = serde_json::json!({ "action":action,"id":operation.operation_id,"work":work,
        "tools":tools,"slot":slot,"runtime":runtime,"archive":operation.archive_path,
        "version":operation.tag,"nexus":{"version":env!("CARGO_PKG_VERSION"),"build_id":option_env!("NEXUS_BUILD_ID").unwrap_or("development")} });
    write_json_atomic(&work, &job, &content)?;
    fs::write(&script, HELPER)?;
    let mut command = std::process::Command::new(tools.join("node/node.exe"));
    command.arg(script).arg(job).current_dir(&work)
        .env_remove("NODE_OPTIONS").env_remove("NODE_PATH").env("npm_config_offline", "true");
    let cancellation = state.cold.token(&operation.operation_id).await;
    run_owned_command(command, "offline package", OFFLINE_TIMEOUT, &state.paths.run_dir, &cancellation).await
}

pub(crate) async fn run(state: &AppState, operation_id: &str) -> io::Result<()> {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    super::super::ensure_checkpoint_mutation_ready(state).await.map_err(|_| io::Error::other("Finish pending recovery before packaging"))?;
    let _update = state.updater.try_acquire_gate().map_err(|error| io::Error::other(error.to_string()))?;
    let _snapshot = state.snapshots.try_acquire_configuration()?;
    super::super::ensure_harness_selection_quiescent(state, &lifecycle, "offline_conflict", "Stop Harness before offline packaging")
        .await.map_err(|_| io::Error::other("Harness must be positively stopped before offline packaging"))?;
    let mut operation = state.cold.load()?.ok_or_else(|| io::Error::other("Offline operation disappeared"))?;
    if operation.operation_id != operation_id { return Err(io::Error::other("Offline operation identity changed")); }
    let tools = if operation.kind == "offline_import" { import_tools()? } else { complete_runtime(state)? };
    let candidate = PathBuf::from(&operation.candidate);
    fs::create_dir(&candidate)?;
    let cancellation = state.cold.token(operation_id).await;
    ensure_not_cancelled(&cancellation)?;
    state.cold.update(operation_id, ColdOperationPhase::Verifying, 10, None).await?;
    if operation.kind == "offline_export" {
        let slot = state.releases.release_root(&operation.release_id)?;
        helper(state, &operation, &tools, "export", Some(&slot), Some(&tools)).await?;
        // The archive is already atomically published; cancellation cannot undo that result.
        operation.phase = ColdOperationPhase::Succeeded;
        operation.progress_percent = 100;
        operation.owner_quiescent = true;
        operation.updated_at_unix = Some(unix_time_seconds());
        state.cold.write(&operation)?;
        let parent = state.paths.downloads_dir.clone();
        let cleanup = tokio::task::spawn_blocking(move || remove_owned_directory(&parent, &candidate)).await.map_err(io::Error::other).and_then(|result| result);
        if let Err(error) = cleanup {
            operation.cleanup_pending = true;
            operation.cleanup_error = Some(format!("Package exported successfully; temporary files could not be removed: {error}"));
            state.cold.write(&operation)?;
        }
        return Ok(());
    }
    if operation.kind != "offline_import" { return Err(io::Error::other("Unknown offline operation kind")); }
    state.releases.ensure_capacity_for_new()?;
    helper(state, &operation, &tools, "import", None, None).await?;
    ensure_not_cancelled(&cancellation)?;
    let result = nexus_core::read_regular_file_bounded(&candidate.join("result.json"), 65536)?.ok_or_else(|| io::Error::other("Offline verification result missing"))?;
    let result: serde_json::Value = serde_json::from_slice(&result).map_err(io::Error::other)?;
    operation.tag = result["version"].as_str().ok_or_else(|| io::Error::other("Offline package version missing"))?.to_owned();
    validate_update_ref(&operation.tag)?;
    operation.phase = ColdOperationPhase::Registering;
    operation.progress_percent = 85;
    state.cold.write(&operation)?;
    let runtime_id = format!("offline-{}", operation.operation_id);
    let runtime_root = state.paths.runtimes_dir.join(&runtime_id);
    if fs::symlink_metadata(&runtime_root).is_ok() { return Err(io::Error::other("Offline runtime destination already exists")); }
    let mut config = state.config.load()?;
    config.runtime = Some(RuntimeConfig {
        node: Some(RuntimePin { path: runtime_root.join("node/node.exe"), ownership: RuntimeOwnership::Nexus }),
        pnpm: Some(RuntimePin { path: runtime_root.join("pnpm/bin/pnpm.cjs"), ownership: RuntimeOwnership::Nexus }),
        ..RuntimeConfig::default()
    });
    config.harness = Some(HarnessLaunchSpec {
        mode: HarnessLaunchMode::Node, program: runtime_root.join("node/node.exe"),
        args: vec!["{release_root}\\apps/cli/lib/bin.js".into(), "--profile".into(), "{profile}".into()],
        working_dir: Some("{release_root}".into()), readiness_url: None, readiness_timeout_secs: None, readiness_token_required: false,
    });
    let _commit = state.cold.gate.lock().await;
    ensure_not_cancelled(&cancellation)?;
    let mut intent = state.cold.prepare_publication(&operation, true, config.clone())?;
    intent.owned_runtime = Some(runtime_id);
    state.cold.write_intent(&intent)?;
    fs::rename(candidate.join("payload/runtime"), &runtime_root)?;
    state.releases.register_prepared(&candidate.join("payload/slot"), &operation.release_id, &operation.tag,
        Some("offline package".into()), Some("Verified offline package; integrity is not a publisher signature".into()))?;
    let slot = state.releases.release_root(&operation.release_id)?;
    helper(state, &operation, &tools, "finalize", Some(&slot), Some(&runtime_root)).await?;
    ensure_not_cancelled(&cancellation)?;
    if !state.config.write_if_current(&intent.previous_config, &config)? {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, PublicationConflict));
    }
    let catalog = state.releases.promote(&operation.release_id)?;
    super::super::persist_release_catalog_state(state, &catalog, false).await.map_err(|error| io::Error::other(error.to_string()))?;
    intent.committed = true;
    state.cold.write_intent(&intent)?;
    // The common replay path finalizes the same durable config/pointer tuple.
    state.cold.recover_publication()
}

/// Only called after the operation's child owner is confirmed quiescent.
pub(super) fn cleanup_candidate(paths: &NexusPaths, operation: &ColdOperation) -> io::Result<()> {
    if operation.kind == "offline_export" {
        let archive = operation.archive_path.as_deref().ok_or_else(|| io::Error::other("Offline export path missing"))?;
        if !Path::new(archive).is_absolute() || !operation.operation_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
            return Err(io::Error::other("Invalid offline cleanup identity"));
        }
        let temp = PathBuf::from(format!("{archive}.nexus-{}.tmp", operation.operation_id));
        match fs::symlink_metadata(&temp) {
            Ok(metadata) if metadata.is_file() && !nexus_core::path_is_reparse(&metadata) => fs::remove_file(temp)?,
            Ok(_) => return Err(io::Error::other("Offline archive temporary file is not ordinary")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
    }
    remove_owned_directory(&paths.downloads_dir, Path::new(&operation.candidate))
}
