//! Offline packaging owns one ordinary cold operation; imports use its durable
//! publication tuple. Package integrity is not publisher authentication.
use super::*;
use nexus_protocol::{RuntimeOwnership, UpdateAction};
const HELPER: &str = include_str!("../scripts/offline-package.mjs");
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(1800);

pub(super) fn environment_root(paths: &NexusPaths, operation_id: &str) -> io::Result<PathBuf> {
    if !operation_id.starts_with("cold-") || !operation_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') { return Err(io::Error::other("Invalid imported environment identity")); }
    let root = fs::canonicalize(&paths.root)?;
    let name = root.file_name().ok_or_else(|| io::Error::other("Nexus data root must have a directory name"))?.to_string_lossy();
    let parent = root.parent().ok_or_else(|| io::Error::other("Nexus data parent missing"))?.join(format!("{name}-environments"));
    fs::create_dir_all(&parent)?;
    let metadata = fs::symlink_metadata(&parent)?;
    if !metadata.is_dir() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Imported environment parent must be an ordinary directory")); }
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(&root) || root.starts_with(&parent) { return Err(io::Error::other("Imported environment must be outside Nexus data")); }
    Ok(parent.join(operation_id))
}

pub(crate) fn progress(operation: &ColdOperation) -> Option<serde_json::Value> {
    if !operation.kind.starts_with("offline_") || operation.phase.is_terminal() { return None; }
    let bytes = nexus_core::read_regular_file_bounded(&Path::new(&operation.candidate).join("progress.json"), 16384).ok()??;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    (value["operation_id"].as_str() == Some(operation.operation_id.as_str())).then_some(value)
}

pub(crate) async fn inspect(state: &AppState, archive: &str) -> io::Result<serde_json::Value> {
    let archive = archive_path(&state.paths, archive, false)?;
    let tools = import_tools()?;
    let work = state.paths.downloads_dir.join(format!("offline-preview-{}", unix_time_nanos_for_update()));
    fs::create_dir(&work)?;
    let result = async {
        fs::write(work.join("offline-package.mjs"), HELPER)?;
        write_json_atomic(&work, &work.join("job.json"), &serde_json::json!({"action":"inspect","work":work,"archive":archive,"tools":tools,"id":"preview"}))?;
        let selected = resolved_runtime_config(state).await?;
        let node = nexus_core::resolve_runtime_command(&selected, "node")?.ok_or_else(|| io::Error::other("Selected Node runtime is unavailable"))?;
        let mut command = std::process::Command::new(node.program);
        command.envs(nexus_core::checked_runtime_child_env(&selected, std::env::var_os("PATH").as_deref())?);
        command.arg(work.join("offline-package.mjs")).arg(work.join("job.json"))
            .env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
        run_owned_command(command, "offline preview", OFFLINE_TIMEOUT, &state.paths.run_dir, &CancellationToken::default()).await?;
        let bytes = nexus_core::read_regular_file_bounded(&work.join("result.json"), 65536)?.ok_or_else(|| io::Error::other("Package preview missing"))?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }.await;
    // Keep evidence if an exceptional child-ownership failure prevented cleanup.
    if result.as_ref().err().is_none_or(command_owner_quiescent) { remove_owned_directory(&state.paths.downloads_dir, &work)?; }
    result
}

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
    let runtime = crate::runtime::prefer_bundled_runtime(config.runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref());
    let pinned = runtime.node.as_ref();
    let root = if let Some(pin) = pinned {
        pin.path.parent().and_then(Path::parent).map(Path::to_path_buf)
    } else { nexus_core::bundled_runtime_dir() }.ok_or_else(|| io::Error::other("No complete portable runtime is available"))?;
    validate_runtime_root(&root)?;
    if let Some(pin) = runtime.pnpm.as_ref() {
        if fs::canonicalize(&pin.path)? != fs::canonicalize(root.join("pnpm/bin/pnpm.cjs"))? {
            return Err(io::Error::other("Offline export requires Node and pnpm from the same complete portable runtime"));
        }
    }
    fs::canonicalize(root)
}

fn validate_runtime_root(root: &Path) -> io::Result<PathBuf> {
    for relative in [if cfg!(windows) { "node/node.exe" } else { "node/node" }, if cfg!(windows) { "node/npm.cmd" } else { "node/npm" }, "node/node_modules/npm/bin/npm-cli.js", "pnpm/bin/pnpm.cjs"] {
        let metadata = fs::symlink_metadata(root.join(relative))?;
        if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Offline packaging requires a complete portable Node/npm/pnpm runtime")); }
    }
    fs::canonicalize(root)
}

fn import_tools() -> io::Result<PathBuf> {
    let root = nexus_core::bundled_runtime_dir().ok_or_else(|| io::Error::other("Offline import needs the complete runtime included with Nexus; reinstall Nexus to restore it"))?;
    validate_runtime_root(&root)
}

pub(crate) async fn begin(state: &AppState, action: UpdateAction, archive: &str, release: Option<&str>, contents: Option<nexus_protocol::OfflineContents>) -> io::Result<ColdOperation> {
    if !cfg!(all(any(windows, target_os = "macos", target_os = "linux"), any(target_arch = "x86_64", target_arch = "aarch64"))) { return Err(io::Error::other("Offline packages require a supported 64-bit desktop platform")); }
    let exporting = action == UpdateAction::OfflineExport;
    if let Some(contents) = &contents {
        if contents.profiles.len() > 32 { return Err(io::Error::other("Select at most 32 profiles")); }
        let home = crate::dsh::resolve_dsh_home_for_paths(&state.paths)?;
        let mut seen = std::collections::HashSet::new();
        for name in &contents.profiles {
            nexus_core::validate_profile_name(name)?;
            if !seen.insert(name.to_lowercase()) { return Err(io::Error::other("Duplicate exported profile")); }
            if exporting { crate::dsh::profile_directory(&home, name)?; }
        }
        if !contents.runtime && contents.profiles.is_empty() && !contents.environment.unwrap_or(contents.configuration) && !contents.sessions && !contents.credentials { return Err(io::Error::other("Select at least one export component")); }
    }
    let archive = archive_path(&state.paths, archive, exporting)?;
    let runtime = if exporting && contents.as_ref().is_none_or(|value| value.runtime) { complete_runtime(state)? } else { import_tools()? };
    if exporting && archive.starts_with(&runtime) { return Err(io::Error::other("Export destination cannot be inside the source runtime")); }
    let release = if exporting {
        if let Some(id) = release.filter(|id| !id.is_empty()) { Some(state.releases.get(id)?) }
        else if contents.as_ref().is_some_and(|value| !value.runtime) { None }
        else { return Err(io::Error::other("Select an installed release to export")); }
    } else { None };
    let tag = release.as_ref().map(|item| item.version.clone()).unwrap_or_else(|| "offline-import".into());
    let mut operation = state.cold.begin_with_details(tag, nexus_protocol::RuntimeSource::Official, nexus_protocol::RuntimeInstallMode::Portable,
        if exporting { "offline_export" } else { "offline_import" }, Some(archive.to_string_lossy().into_owned()), release.map(|item| item.id)).await?;
    operation.offline_contents = contents;
    state.cold.write(&operation)?;
    Ok(operation)
}

async fn helper(state: &AppState, operation: &ColdOperation, tools: &Path, action: &str, slot: Option<&Path>, runtime: Option<&Path>) -> io::Result<()> {
    let work = PathBuf::from(&operation.candidate);
    let script = work.join("offline-package.mjs");
    let job = work.join("offline-job.json");
    let content = serde_json::json!({ "action":action,"id":operation.operation_id,"work":work,
        "private_writer":std::env::current_exe()?,
        "tools":tools,"slot":slot,"runtime":runtime,"archive":operation.archive_path,
        "desktop_runtime":nexus_core::bundled_runtime_dir().map(|root|root.join("desktop")),
        "git_runtime":nexus_core::bundled_runtime_dir().map(|root|root.join("git")),
        "version":operation.tag,"contents":operation.offline_contents,
        "home":if action == "export" || action == "merge_environment" { Some(crate::dsh::resolve_dsh_home_for_paths(&state.paths)?) } else { None },
        "environment":environment_root(&state.paths, &operation.operation_id)?,
        "active_profile":state.profiles.load()?.active_profile,
        "preferences":if action == "export" { state.config.load()?.harness_preferences } else { None },
        "nexus":{"version":env!("CARGO_PKG_VERSION"),"build_id":option_env!("NEXUS_BUILD_ID").unwrap_or("development")} });
    write_json_atomic(&work, &job, &content)?;
    fs::write(&script, HELPER)?;
    let selected = resolved_runtime_config(state).await?;
        let node = nexus_core::resolve_runtime_command(&selected, "node")?.ok_or_else(|| io::Error::other("Selected Node runtime is unavailable"))?;
        let mut command = std::process::Command::new(node.program);
        command.envs(nexus_core::checked_runtime_child_env(&selected, std::env::var_os("PATH").as_deref())?);
    command.arg(script).arg(job).current_dir(&work)
        .env_remove("NODE_OPTIONS").env_remove("NODE_PATH").env("npm_config_offline", "true");
    let cancellation = state.cold.token(&operation.operation_id).await;
    run_owned_command(command, "offline package", OFFLINE_TIMEOUT, &state.paths.run_dir, &cancellation).await
}

async fn admit_verification(state: &AppState, operation_id: &str) -> io::Result<()> {
    // The accepted offline operation is already busy. Admit only its live owner,
    // while retaining the publication, checkpoint and Canary recovery checks.
    state.cold.update(operation_id, ColdOperationPhase::Verifying, 10, None).await?;
    ensure_publication_admission(state, operation_id).await
}

pub(crate) async fn run(state: &AppState, operation_id: &str) -> io::Result<()> {
    let lifecycle = state.supervisor.acquire_lifecycle().await;
    admit_verification(state, operation_id).await?;
    let _update = state.updater.try_acquire_gate().map_err(|error| io::Error::other(error.to_string()))?;
    let _snapshot = state.snapshots.try_acquire_configuration()?;
    super::super::ensure_harness_selection_quiescent(state, &lifecycle, "offline_conflict", "Stop Harness before offline packaging")
        .await.map_err(|_| io::Error::other("Harness must be positively stopped before offline packaging"))?;
    let mut operation = state.cold.load()?.ok_or_else(|| io::Error::other("Offline operation disappeared"))?;
    if operation.operation_id != operation_id { return Err(io::Error::other("Offline operation identity changed")); }
    let tools = if operation.kind == "offline_import" || operation.offline_contents.as_ref().is_some_and(|value| !value.runtime) { import_tools()? } else { complete_runtime(state)? };
    let candidate = PathBuf::from(&operation.candidate);
    nexus_private_file::create_new_private_directory(&candidate)?;
    let cancellation = state.cold.token(operation_id).await;
    ensure_not_cancelled(&cancellation)?;
    if operation.kind == "offline_export" {
        let slot = state.releases.get(&operation.release_id).ok().map(|_| state.releases.release_root(&operation.release_id)).transpose()?;
        helper(state, &operation, &tools, "export", slot.as_deref(), Some(&tools)).await?;
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
    helper(state, &operation, &tools, "import", None, None).await?;
    publish_verified_import(state, operation, &tools, &cancellation).await
}

async fn publish_verified_import(state: &AppState, mut operation: ColdOperation, tools: &Path,
    cancellation: &nexus_core::CancellationToken) -> io::Result<()> {
    let candidate = PathBuf::from(&operation.candidate);
    ensure_not_cancelled(&cancellation)?;
    let result = nexus_core::read_regular_file_bounded(&candidate.join("result.json"), 65536)?.ok_or_else(|| io::Error::other("Offline verification result missing"))?;
    let result: serde_json::Value = serde_json::from_slice(&result).map_err(io::Error::other)?;
    let contents: Option<nexus_protocol::OfflineContents> = result.get("contents").filter(|value| !value.is_null()).map(|value| serde_json::from_value(value.clone()).map_err(io::Error::other)).transpose()?;
    let base = contents.as_ref().is_none_or(|value| value.runtime);
    operation.offline_contents = contents.clone();
    if base {
        // Admit before cloning the receiver's home or creating recovery records.
        // Publication/promote repeats this check under its own gate.
        state.releases.ensure_rollback_protection(&operation.release_id)?;
        state.releases.ensure_capacity_for_new()?;
    }
    operation.tag = result["version"].as_str().ok_or_else(|| io::Error::other("Offline package version missing"))?.to_owned();
    validate_update_ref(&operation.tag)?;
    operation.phase = ColdOperationPhase::Registering;
    operation.progress_percent = 85;
    state.cold.write(&operation)?;
    let runtime_id = format!("offline-{}", operation.operation_id);
    let runtime_root = state.paths.runtimes_dir.join(&runtime_id);
    if fs::symlink_metadata(&runtime_root).is_ok() { return Err(io::Error::other("Offline runtime destination already exists")); }
    let mut config = state.config.load()?;
    if base { config.runtime = Some(RuntimeConfig {
        node: Some(RuntimePin { path: runtime_root.join(if cfg!(windows) { "node/node.exe" } else { "node/node" }), ownership: RuntimeOwnership::Nexus }),
        pnpm: Some(RuntimePin { path: runtime_root.join("pnpm/bin/pnpm.cjs"), ownership: RuntimeOwnership::Nexus }),
        git: candidate.join("payload/runtime").join(if cfg!(windows) { "git/cmd/git.exe" } else { "git/bin/git" }).is_file().then(|| RuntimePin {
            path: runtime_root.join(if cfg!(windows) { "git/cmd/git.exe" } else { "git/bin/git" }), ownership: RuntimeOwnership::Nexus,
        }),
        ..RuntimeConfig::default()
    });
    config.harness = Some(HarnessLaunchSpec {
        mode: HarnessLaunchMode::Node, program: runtime_root.join(if cfg!(windows) { "node/node.exe" } else { "node/node" }),
        args: vec!["{release_root}\\apps/cli/lib/bin.js".into(), "--profile".into(), "{profile}".into()],
        working_dir: Some("{release_root}".into()), readiness_url: None, readiness_timeout_secs: None, readiness_token_required: false,
    });
    config.external_harness = None;
    }
    let environment = environment_root(&state.paths, &operation.operation_id)?;
    let has_environment = candidate.join("payload/environment").is_dir();
    if has_environment {
        if fs::symlink_metadata(&environment).is_ok() { return Err(io::Error::other("Imported environment destination already exists")); }
        helper(state, &operation, &tools, "merge_environment", None, None).await?;
        operation.warning=merge_warning(&candidate)?;
        state.cold.write(&operation)?;
        if let Some(bytes) = nexus_core::read_regular_file_bounded(&candidate.join("credential-recovery.json"), 65536)? {
            let recovery = state.paths.run_dir.join(format!("credential-recovery-{}.json", operation.operation_id));
            nexus_core::write_private_bytes_atomic(&state.paths.root, &recovery, &bytes)?;
            operation.credential_recovery_path = Some(recovery.to_string_lossy().into_owned());
            state.cold.write(&operation)?;
        }
        let mut preferences = if contents.as_ref().is_some_and(|value| value.environment.unwrap_or(value.configuration)) {
            serde_json::from_value(result.get("preferences").filter(|value| !value.is_null()).cloned().unwrap_or_else(|| serde_json::json!({}))).map_err(io::Error::other)?
        } else { config.harness_preferences.clone().unwrap_or_default() };
        preferences.home = Some(environment.to_string_lossy().into_owned());
        config.harness_preferences = Some(nexus_core::normalize_harness_preferences(preferences)?);
    }
    let imported_profiles = if let Some(contents) = &contents {
        if contents.profiles.is_empty() { None } else {
            let active = result["active_profile"].as_str().filter(|name| contents.profiles.iter().any(|item| item == name))
                .unwrap_or(&contents.profiles[0]);
            let mut names = state.profiles.load()?.profiles;
            for name in &contents.profiles { if !names.contains(name) { names.push(name.clone()); } }
            let catalog = nexus_core::ProfileCatalog::new(active, names)?;
            Some(catalog)
        }
    } else { None };
    let _commit = state.cold.gate.lock().await;
    ensure_not_cancelled(&cancellation)?;
    if base { state.releases.ensure_rollback_protection(&operation.release_id)?; }
    let mut intent = state.cold.prepare_publication(&operation, base, config.clone())?;
    intent.owned_runtime = base.then_some(runtime_id);
    intent.owned_environment = has_environment;
    intent.preserve_release = !base;
    if let Some(catalog) = imported_profiles {
        intent.previous_profiles = Some(state.profiles.load()?);
        intent.target_profiles = Some(catalog);
    }
    state.cold.write_intent(&intent)?;
    if has_environment { fs::rename(candidate.join("payload/merged-environment"), &environment)?; }
    let slot = if base {
    fs::rename(candidate.join("payload/runtime"), &runtime_root)?;
    state.releases.register_prepared(&candidate.join("payload/slot"), &operation.release_id, &operation.tag,
        Some("offline package".into()), Some("Verified offline package; integrity is not a publisher signature".into()))?;
    state.releases.release_root(&operation.release_id)?
    } else { state.releases.load()?.current_release.map(|id| state.releases.release_root(&id)).transpose()?.unwrap_or_else(|| candidate.join("unused-slot")) };
    helper(state, &operation, &tools, "finalize", Some(&slot), Some(&runtime_root)).await?;
    ensure_not_cancelled(&cancellation)?;
    if !state.config.write_if_current(&intent.previous_config, &config)? {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, PublicationConflict));
    }
    if base {
        let catalog = state.releases.promote_with_rollback(&operation.release_id)?;
        super::super::persist_release_catalog_state(state, &catalog, false).await.map_err(|error| io::Error::other(error.to_string()))?;
    }
    intent.committed = true;
    state.cold.write_intent(&intent)?;
    // The common replay path finalizes the same durable config/pointer tuple.
    state.cold.recover_publication()?;
    if let Some(catalog) = intent.target_profiles {
        super::super::update_agent_state(state, |current| current.set_profile(catalog.active_profile.clone()))
            .await.map_err(|error| io::Error::other(error.to_string()))?;
    }
    Ok(())
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

fn merge_warning(candidate:&Path)->io::Result<Option<String>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Summary {schema_version:u32,preserved_configuration_values:u64}
    let Some(bytes)=nexus_core::read_regular_file_bounded(&candidate.join("merge-summary.json"),4096)? else {return Ok(None)};
    let summary:Summary=serde_json::from_slice(&bytes).map_err(|_|io::Error::other("Invalid offline merge summary"))?;
    if summary.schema_version!=1 {return Err(io::Error::other("Invalid offline merge summary"));}
    Ok((summary.preserved_configuration_values>0).then(||"Some incoming configuration values were not applied to preserve your local credentials. To use the package values, review the credential conflict option before importing again.".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn merge_notices_are_bounded_and_do_not_echo_configuration_values() {
        let state=crate::switch_ownership_tests::switch_test_state("offline-merge-notice");
        let file=state.paths.root.join("merge-summary.json");
        for (count,notice) in [(0,false),(2,true)] {
            fs::write(&file,serde_json::to_vec(&serde_json::json!({"schema_version":1,"preserved_configuration_values":count})).unwrap()).unwrap();
            assert_eq!(merge_warning(&state.paths.root).unwrap().is_some(),notice);
        }
        let secret=b"{\"schema_version\":1,\"preserved_configuration_values\":1,\"secret\":\"do-not-echo\"}";
        fs::write(&file,secret).unwrap();
        let error=merge_warning(&state.paths.root).unwrap_err();assert!(!error.to_string().contains("do-not-echo"));
        assert_eq!(fs::read(&file).unwrap(),secret);
        fs::remove_dir_all(state.paths.root).unwrap();
    }
    #[test]
    fn preview_and_import_share_a_thirty_minute_archive_budget() {
        assert!(super::OFFLINE_TIMEOUT >= std::time::Duration::from_secs(1800));
    }
    use super::*;

    #[tokio::test]
    async fn runtime_import_requires_rollback_before_environment_merge_but_data_only_does_not() {
        for runtime in [true, false] {
            let state = crate::switch_ownership_tests::switch_test_state(if runtime { "offline-early-rollback" } else { "offline-data-admission" });
            let pointers = br#"{"schema_version":1,"current_release":"legacy-unverified","last_known_good":null}"#;
            fs::write(&state.paths.release_pointers_file, pointers).unwrap();
            let original_config = state.config.load().unwrap();
            let op = state.cold.begin_with_details("v-test".into(), nexus_protocol::RuntimeSource::Official,
                nexus_protocol::RuntimeInstallMode::Portable, "offline_import", None, None).await.unwrap();
            let candidate = PathBuf::from(&op.candidate);
            fs::create_dir_all(&candidate).unwrap();
            fs::write(candidate.join("result.json"), serde_json::to_vec(&serde_json::json!({
                "version":"v-test", "contents":{"runtime":runtime,"environment":true}
            })).unwrap()).unwrap();
            fs::create_dir_all(candidate.join("payload/environment")).unwrap();
            // The real merge helper writes its job before launching Node. An
            // unavailable helper makes accidental late admission observable.
            let error = publish_verified_import(&state, op.clone(), &candidate.join("no-tools"),
                &nexus_core::CancellationToken::default()).await.unwrap_err();
            if runtime {
                assert!(error.to_string().contains("rollback_health_required"), "{error}");
                assert!(!candidate.join("offline-job.json").exists(), "merge must not start");
            } else {
                let job: serde_json::Value = serde_json::from_slice(&fs::read(candidate.join("offline-job.json")).unwrap()).unwrap();
                assert_eq!(job["action"], "merge_environment");
                assert!(!error.to_string().contains("rollback_health_required"));
            }
            assert_eq!(fs::read(&state.paths.release_pointers_file).unwrap(), pointers);
            assert_eq!(state.config.load().unwrap(), original_config);
            assert!(!state.cold.intent_path().exists());
            assert!(!state.paths.run_dir.join(format!("credential-recovery-{}.json", op.operation_id)).exists());
            cleanup_candidate(&state.paths, &op).unwrap();
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }

    #[test]
    fn imported_home_allows_healthy_snapshots_without_weakening_root_isolation() {
        let state = crate::switch_ownership_tests::switch_test_state("offline-home-isolation");
        let old = state.paths.runtimes_dir.join("offline-old/environment");
        fs::create_dir_all(old.join("profiles/web")).unwrap();
        assert!(nexus_snapshots::SnapshotStore::new(&state.paths.root, &old, "web").is_err());
        let home = environment_root(&state.paths, "cold-isolation-test").unwrap();
        fs::create_dir_all(home.join("profiles/web")).unwrap();
        fs::write(home.join("profiles/web/package.json"), r#"{"name":"web","dependencies":{}}"#).unwrap();
        let store = nexus_snapshots::SnapshotStore::new(&state.paths.root, &home, "web").unwrap();
        store.capture_healthy(nexus_snapshots::CaptureRequest { dsh_version: "fixture".into() }).unwrap();
        assert!(!home.starts_with(fs::canonicalize(&state.paths.root).unwrap()));
        remove_owned_directory(home.parent().unwrap(), &home).unwrap();
        fs::remove_dir_all(&state.paths.root).unwrap();
    }

    #[tokio::test]
    async fn offline_verification_admits_its_owner_and_preserves_recovery_guards() {
        for kind in ["offline_export", "offline_import"] {
            let state = crate::switch_ownership_tests::switch_test_state(kind);
            let operation = state.cold.begin_with_details("v-test".into(),
                nexus_protocol::RuntimeSource::Official, nexus_protocol::RuntimeInstallMode::Portable,
                kind, None, None).await.unwrap();
            let id = &operation.operation_id;
            assert!(crate::ensure_checkpoint_mutation_ready(&state).await.is_err());
            admit_verification(&state, id).await.unwrap();
            assert!(admit_verification(&state, "wrong-owner").await.is_err());
            fs::write(state.cold.intent_path(), "pending").unwrap();
            let error = admit_verification(&state, id).await.unwrap_err().to_string();
            assert!(error.contains("cold_publication_pending"), "{error}");
            fs::remove_file(state.cold.intent_path()).unwrap();
            let journal = state.paths.run_dir.join("checkpoint-restore.json");
            fs::write(&journal, "broken recovery record").unwrap();
            let error = admit_verification(&state, id).await.unwrap_err().to_string();
            assert!(error.contains("checkpoint_recovery_failed"), "{error}");
            fs::remove_file(journal).unwrap();
            state.cold.cancellation.lock().await.as_ref().unwrap().1.cancel();
            assert!(admit_verification(&state, id).await.unwrap_err().to_string().contains("cold_install_owner_conflict"));
            fs::remove_dir_all(&state.paths.root).unwrap();
        }
    }
}
