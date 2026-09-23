//! Offline repair: the lockfile selects the exact locally available
//! package. An existing entry is never proposed for replacement.
use std::{collections::BTreeSet, fs, io, path::{Component, Path, PathBuf}};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use axum::{extract::State, response::IntoResponse, Json};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairRequest { #[serde(default)] fingerprint: String, #[serde(default)] startup: bool, #[serde(default)] release_id: Option<String>, #[serde(default)] bind_desktop_home: Option<PathBuf> }

pub(crate) async fn repair(State(state): State<super::AppState>, Json(request): Json<RepairRequest>) -> axum::response::Response {
    let Some(lifecycle) = state.supervisor.try_acquire_lifecycle() else {
        return super::data_error_response(io::Error::new(io::ErrorKind::ResourceBusy, "Harness operation in progress"), "dependency_repair_busy");
    };
    if let Err(response) = super::ensure_checkpoint_mutation_ready(&state).await { return response; }
    if let Err(response) = super::ensure_harness_stopped(&state, &lifecycle).await { return response; }
    let update = match state.updater.try_acquire_gate() { Ok(g) => g, Err(error) => return super::update_error_response(error) };
    if let Err(response) = super::ensure_update_idle(&state) { return response; }
    let snapshots = match state.snapshots.try_acquire_configuration() { Ok(g) => g, Err(error) => return super::data_error_response(error, "dependency_repair_busy") };
    let cold = match state.cold.try_acquire_maintenance() { Ok(g) => g, Err(error) => return super::data_error_response(error, "dependency_repair_busy") };
    let result = tokio::task::spawn_blocking(move || {
        let _guards = (lifecycle, update, snapshots, cold);
        nexus_core::terminal_lease::ensure_all_idle(&state.paths)?;
        let source = super::source_context::resolve(&state.paths, &state.releases)?;
        if source.external { return Err(invalid("Dependency repair is only available for managed Harness releases")); }
        if request.release_id.as_deref().is_some_and(|expected| source.release_id.as_deref() != Some(expected)) { return Err(invalid("Selected release changed before dependency repair")); }
        let root = source.root.ok_or_else(|| invalid("No Harness release selected"))?;
        if let Some(expected_home) = request.bind_desktop_home {
            let home = state.snapshots.configured_dsh_home()?;
            if home != expected_home && fs::canonicalize(&home).ok().zip(fs::canonicalize(&expected_home).ok()).is_none_or(|(a,b)| a != b) {
                return Err(invalid("Desktop home changed before module binding"));
            }
            let linked = nexus_core::ReleaseStore::heal_module_farm(&home, &root)?;
            let archived = nexus_core::ReleaseStore::heal_profile_modules(&home, "desktop", &root)?;
            Ok(serde_json::json!({"phase":"bound", "linked":linked,"archived":archived}))
        } else if request.startup { ensure_startup(&root, &state.paths.root) } else { apply(&root, &request.fingerprint, &state.paths.root) }
    }).await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => super::data_error_response(error, "dependency_repair_failed"),
        Err(error) => super::data_error_response(io::Error::other(error), "dependency_repair_failed"),
    }
}

fn apply(root: &Path, expected: &str, data_root: &Path) -> io::Result<Value> {
    apply_scoped(root, expected, data_root, false)
}

fn apply_scoped(root: &Path, expected: &str, data_root: &Path, startup: bool) -> io::Result<Value> {
    let plan = preview_scoped(root, startup)?;
    if expected != plan.fingerprint { return Err(io::Error::new(io::ErrorKind::WouldBlock, "Dependency preview changed; inspect again before repairing")); }
    let candidates: Vec<_> = plan.entries.iter().filter(|entry| entry.target.is_some()).collect();
    if candidates.is_empty() { return Err(invalid("No verified missing links to repair")); }
    let history = data_root.join("dependency-repair-history");
    ordinary_ancestors(data_root, &history)?;
    fs::create_dir_all(&history)?;
    let round = history.join(nexus_core::agent_auth::random_hex()?);
    fs::create_dir(&round)?;
    let write = |name: &str, bytes: &[u8]| nexus_core::write_private_bytes_atomic(data_root, &round.join(name), bytes);
    // Preserve the exact lockfile, importer manifests and absent-entry plan
    // before the first write. Existing entries are never removed or replaced.
    write("plan.json", &serde_json::to_vec(&plan)?)?;
    write("pnpm-lock.yaml", &bounded(&plan.root.join("pnpm-lock.yaml"), 16 * 1024 * 1024)?)?;
    let modules = plan.root.join("node_modules/.modules.yaml");
    if modules.exists() { write("modules.yaml", &bounded(&modules, 16 * 1024 * 1024)?)?; }
    let mut manifests = serde_json::Map::new();
    for entry in &candidates {
        let base = if entry.importer == "." { plan.root.clone() } else { plan.root.join(&entry.importer) };
        manifests.insert(entry.importer.clone(), serde_json::from_slice(&bounded(&base.join("package.json"), 2 * 1024 * 1024)?)?);
    }
    write("manifests.json", &serde_json::to_vec(&manifests)?)?;
    if preview_scoped(&plan.root, startup)?.fingerprint != plan.fingerprint {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "Dependency inputs changed while saving the repair record; inspect again"));
    }
    let mut repaired = Vec::new();
    let result = (|| -> io::Result<()> {
        for entry in candidates {
            let parent = entry.destination.parent().unwrap();
            ordinary_ancestors(&plan.root, parent)?;
            fs::create_dir_all(parent)?;
            ordinary_ancestors(&plan.root, parent)?;
            nexus_core::ReleaseStore::create_missing_module_link(&entry.destination, entry.target.as_ref().unwrap())?;
            repaired.push(entry.destination.clone());
            write("result.json", &serde_json::to_vec(&serde_json::json!({"phase":"repairing","repaired":repaired}))?)?;
        }
        Ok(())
    })();
    let after = preview_scoped(&plan.root, startup);
    let error = result.err().map(|error| error.to_string());
    let verified = error.is_none() && after.is_ok() && repaired.iter().all(|destination| {
        plan.entries.iter().find(|entry| &entry.destination == destination).is_some_and(|entry|
            fs::canonicalize(destination).ok().as_ref() == entry.target.as_ref())
    });
    let value = serde_json::json!({"phase":if verified {"repaired"} else {"incomplete"}, "record":round,
        "repaired":repaired,"error":error,"verification_error":after.as_ref().err().map(ToString::to_string),
        "preview":after.ok(),"startup_verified":false});
    write("result.json", &serde_json::to_vec(&value)?)?;
    Ok(value)
}

pub(crate) async fn status(State(state): State<super::AppState>) -> axum::response::Response {
    let _lifecycle = match super::try_read_lifecycle(&state) { Ok(guard) => guard, Err(response) => return response };
    let result = async {
        let source = super::source_context::resolve_async(&state.paths, &state.releases).await?;
        if source.external { return Err(invalid("Dependency repair is only available for managed Harness releases")); }
        let root = source.root.ok_or_else(|| invalid("No Harness release selected"))?;
        tokio::task::spawn_blocking(move || preview(&root)).await.map_err(io::Error::other)?
    }.await;
    match result { Ok(value) => Json(value).into_response(), Err(error) => super::data_error_response(error, "dependency_preview_unavailable") }
}

#[derive(Debug, Serialize)]
pub(crate) struct DependencyPreview {
    pub root: PathBuf,
    pub fingerprint: String,
    pub entries: Vec<DependencyEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DependencyEntry {
    pub importer: String,
    pub package: String,
    pub destination: PathBuf,
    pub target: Option<PathBuf>,
    pub reason: &'static str,
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn bounded(file: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use io::Read;
    let metadata = fs::symlink_metadata(file)?;
    if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) {
        return Err(invalid("Dependency metadata must be an ordinary file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(file)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(invalid("Dependency metadata is too large")); }
    Ok(bytes)
}

fn relative(value: &str) -> io::Result<PathBuf> {
    if value.is_empty() || value.contains(['\\', ':']) ||
        value.split('/').any(|part| part.is_empty() || part.ends_with(['.', ' '])) {
        return Err(invalid("Invalid dependency path"));
    }
    let path = PathBuf::from(value);
    if path.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(invalid("Dependency path escapes the release"));
    }
    Ok(path)
}

fn package_path(name: &str) -> io::Result<PathBuf> {
    let parts: Vec<_> = name.split('/').collect();
    if !((parts.len() == 1 && !name.starts_with('@')) ||
        (parts.len() == 2 && parts[0].starts_with('@') && parts[0].len() > 1)) ||
        !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"@/-_.".contains(&b)) {
        return Err(invalid("Invalid dependency package name"));
    }
    relative(name)
}

// No linked parent may redirect a proposed destination outside its importer.
fn ordinary_ancestors(root: &Path, path: &Path) -> io::Result<()> {
    let suffix = path.strip_prefix(root).map_err(|_| invalid("Path outside release"))?;
    let mut cursor = root.to_path_buf();
    for part in suffix.components() {
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.is_dir() && !nexus_core::path_is_reparse(&meta) => (),
            Ok(_) => return Err(invalid("Dependency parent is not an ordinary directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn preview(root: &Path) -> io::Result<DependencyPreview> {
    preview_scoped(root, false)
}

fn preview_scoped(root: &Path, startup: bool) -> io::Result<DependencyPreview> {
    let root = fs::canonicalize(root)?;
    let lock_bytes = bounded(&root.join("pnpm-lock.yaml"), 16 * 1024 * 1024)?;
    let lock: Value = serde_yaml_ng::from_slice(&lock_bytes).map_err(io::Error::other)?;
    let importers = lock["importers"].as_object().ok_or_else(|| invalid("Missing lockfile importers"))?;
    if importers.len() > 4096 { return Err(invalid("Too many workspace importers")); }
    let mut identity = Sha256::new();
    identity.update(&lock_bytes);
    // pnpm 11 records its virtual-store filename limit. Long peer/patch IDs
    // use a SHA-256 suffix, so guessing the unshortened directory misses valid
    // local packages. Only apply this mapping for the verified package-manager family.
    let modules = root.join("node_modules/.modules.yaml");
    let mut store_limit = None;
    if modules.exists() {
        let bytes = bounded(&modules, 16 * 1024 * 1024)?;
        identity.update(&bytes);
        let value: Value = serde_yaml_ng::from_slice(&bytes).map_err(io::Error::other)?;
        if value["packageManager"].as_str().is_some_and(|value| value.starts_with("pnpm@11.")) {
            store_limit = value["virtualStoreDirMaxLength"].as_u64().filter(|limit| (34..=255).contains(limit));
        }
    }
    let mut entries = Vec::new();
    for (importer, spec) in importers {
        if startup && !(importer.starts_with("packages/") || importer.starts_with("vendor/") || importer == "apps/cli" || importer == "apps/desktop") { continue; }
        let base = if importer == "." { root.clone() } else { root.join(relative(importer)?) };
        ordinary_ancestors(&root, &base)?;
        let manifest_bytes = bounded(&base.join("package.json"), 2 * 1024 * 1024)?;
        identity.update(importer.as_bytes()); identity.update(&manifest_bytes);
        let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(io::Error::other)?;
        let mut required = BTreeSet::new();
        for section in ["dependencies", "peerDependencies"] {
            if let Some(deps) = manifest[section].as_object() { required.extend(deps.keys()); }
        }
        for name in required {
            if startup && manifest["peerDependenciesMeta"][name]["optional"].as_bool() == Some(true) && manifest["dependencies"].get(name).is_none() { continue; }
            if entries.len() >= 10000 { return Err(invalid("Too many missing dependencies")); }
            let package = package_path(name)?;
            let destination = base.join("node_modules").join(&package);
            ordinary_ancestors(&root, destination.parent().unwrap())?;
            match fs::symlink_metadata(&destination) {
                Ok(metadata) => {
                    if nexus_core::path_is_reparse(&metadata) && fs::canonicalize(&destination).is_err() {
                        entries.push(DependencyEntry { importer: importer.clone(), package: name.clone(), destination,
                            target: None, reason: "existing_broken_link" });
                    }
                    if startup && !metadata.is_dir() && !nexus_core::path_is_reparse(&metadata) {
                        return Err(invalid(&format!("Startup dependency conflicts with an existing file: {importer}: {name}")));
                    }
                    continue; // Existing entries are never replaced, including broken links.
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
            let version = ["dependencies", "devDependencies", "optionalDependencies"].iter()
                .find_map(|section| spec[*section][name]["version"].as_str());
            let mut entry = DependencyEntry { importer: importer.clone(), package: name.clone(), destination,
                target: None, reason: "lock_entry_missing" };
            if let Some(version) = version {
                let candidate = if let Some(link) = version.strip_prefix("link:") {
                    // Workspace links may contain '..'; canonical containment below is mandatory.
                    base.join(link)
                } else {
                    let mut store = format!("{}@{}", name.replace('/', "+"), version.strip_suffix(')').unwrap_or(version).replace(")(", "_").replace(['(', ')'], "_"));
                    if let Some(limit) = store_limit {
                        if store.len() > limit as usize || store != store.to_lowercase() {
                            let hash = format!("{:x}", Sha256::digest(store.as_bytes()));
                            let prefix: String = store.chars().take(limit as usize - 33).collect();
                            store = format!("{prefix}_{}", &hash[..32]);
                        }
                    }
                    root.join("node_modules/.pnpm").join(relative(&store)?).join("node_modules").join(&package)
                };
                entry.reason = "local_package_missing";
                if let Ok(target) = fs::canonicalize(candidate) {
                    if !target.starts_with(&root) { return Err(invalid("Dependency target escapes release")); }
                    let target_bytes = bounded(&target.join("package.json"), 2 * 1024 * 1024)?;
                    let metadata: Value = serde_json::from_slice(&target_bytes).map_err(io::Error::other)?;
                    identity.update(&target_bytes);
                    let expected = version.split('(').next().unwrap_or(version);
                    if metadata["name"].as_str() == Some(name.as_str()) &&
                        (version.starts_with("link:") || metadata["version"].as_str() == Some(expected)) {
                        entry.target = Some(target); entry.reason = "missing_link";
                    } else { entry.reason = "local_package_identity_mismatch"; }
                }
            }
            entries.push(entry);
        }
    }
    identity.update(serde_json::to_vec(&entries).map_err(io::Error::other)?);
    Ok(DependencyPreview { root, entries, fingerprint: format!("{:x}", identity.finalize()) })
}

// Run under the caller's lifecycle ownership, before spawning Harness. Only
// inspect official runtime importers: no registry access, plugin execution or full tree walk.
pub(crate) fn ensure_startup(root: &Path, data_root: &Path) -> io::Result<Value> {
    if !root.join("packages/boot/app-boot/package.json").is_file() {
        return Ok(serde_json::json!({"phase":"unsupported_layout"}));
    }
    let plan = preview_scoped(root, true)?;
    if let Some(entry) = plan.entries.iter().find(|entry| entry.target.is_none()) {
        return Err(invalid(&format!("Startup dependency unavailable: {}: {} ({}). Repair the selected Harness version before starting.", entry.importer, entry.package, entry.reason)));
    }
    if plan.entries.is_empty() { return Ok(serde_json::json!({"phase":"ready"})); }
    let result = apply_scoped(root, &plan.fingerprint, data_root, true)?;
    if result["phase"] != "repaired" || !preview_scoped(root, true)?.entries.is_empty() {
        return Err(invalid("Startup dependency repair incomplete; inspect dependency-repair-history before retrying"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "read-only installed-release probe; requires NEXUS_TEST_DEPENDENCY_ROOT"]
    fn installed_release_preview_is_read_only() {
        let root = PathBuf::from(std::env::var_os("NEXUS_TEST_DEPENDENCY_ROOT").expect("explicit release root required"));
        let started = std::time::Instant::now();
        let plan = preview_scoped(&root, true).expect("installed startup preview");
        let mut reasons = std::collections::BTreeMap::new();
        for entry in &plan.entries { *reasons.entry(entry.reason).or_insert(0usize) += 1; }
        println!("preview_ms={} entries={} reasons={reasons:?}", started.elapsed().as_millis(), plan.entries.len());
        for entry in &plan.entries { println!("{}: {} ({})", entry.importer, entry.package, entry.reason); }
        assert_eq!(preview_scoped(&root, true).unwrap().fingerprint, plan.fingerprint, "unchanged installed inputs must produce a stable plan");
    }
    #[test]
    fn startup_recovers_repeated_missing_entries_and_blocks_unavailable_packages() {
        let base = std::env::temp_dir().join(format!("nexus-startup-repair-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let root = base.join("release"); let data = base.join("data");
        let importer = root.join("packages/boot/app-boot");
        let target = root.join("vendor/example");
        fs::create_dir_all(&importer).unwrap(); fs::create_dir_all(&target).unwrap(); fs::create_dir_all(&data).unwrap();
        fs::write(importer.join("package.json"), r#"{"peerDependencies":{"example":"*"}}"#).unwrap();
        fs::write(target.join("package.json"), r#"{"name":"example","version":"1.0.0"}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  packages/boot/app-boot:\n    devDependencies:\n      example:\n        version: link:../../../vendor/example\n").unwrap();
        let link = importer.join("node_modules/example");
        for _ in 0..2 {
            assert_eq!(ensure_startup(&root, &data).unwrap()["phase"], "repaired");
            assert_eq!(fs::canonicalize(&link).unwrap(), fs::canonicalize(&target).unwrap());
            assert_eq!(ensure_startup(&root, &data).unwrap()["phase"], "ready");
            #[cfg(windows)] fs::remove_dir(&link).unwrap();
            #[cfg(unix)] fs::remove_file(&link).unwrap();
        }
        fs::write(&link, "preserve user file").unwrap();
        assert!(ensure_startup(&root, &data).is_err());
        assert_eq!(fs::read_to_string(&link).unwrap(), "preserve user file");
        fs::remove_file(&link).unwrap(); fs::remove_dir_all(&target).unwrap();
        assert!(ensure_startup(&root, &data).unwrap_err().to_string().contains("example"));
        assert!(!link.exists()); fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn startup_recovers_official_plugin_dependencies_outside_bootstrap() {
        let base = std::env::temp_dir().join(format!("nexus-startup-repair-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let root = base.join("release"); let data = base.join("data");
        let importer = root.join("packages/settings/settings-file");
        fs::create_dir_all(root.join("packages/boot/app-boot")).unwrap();
        fs::write(root.join("packages/boot/app-boot/package.json"), "{}").unwrap();
        let target = root.join("vendor/example");
        fs::create_dir_all(&importer).unwrap(); fs::create_dir_all(&target).unwrap(); fs::create_dir_all(&data).unwrap();
        fs::write(importer.join("package.json"), r#"{"peerDependencies":{"example":"*"}}"#).unwrap();
        fs::write(target.join("package.json"), r#"{"name":"example","version":"1.0.0"}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  packages/settings/settings-file:\n    devDependencies:\n      example:\n        version: link:../../../vendor/example\n").unwrap();
        let link = importer.join("node_modules/example");
        for _ in 0..2 {
            assert_eq!(ensure_startup(&root, &data).unwrap()["phase"], "repaired");
            assert_eq!(fs::canonicalize(&link).unwrap(), fs::canonicalize(&target).unwrap());
            assert_eq!(ensure_startup(&root, &data).unwrap()["phase"], "ready");
            #[cfg(windows)] fs::remove_dir(&link).unwrap();
            #[cfg(unix)] fs::remove_file(&link).unwrap();
        }
        fs::write(&link, "preserve user file").unwrap();
        assert!(ensure_startup(&root, &data).is_err());
        assert_eq!(fs::read_to_string(&link).unwrap(), "preserve user file");
        fs::remove_file(&link).unwrap(); fs::remove_dir_all(&target).unwrap();
        assert!(ensure_startup(&root, &data).unwrap_err().to_string().contains("example"));
        assert!(!link.exists()); fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn preview_uses_exact_local_package_and_never_writes() {
        let root = std::env::temp_dir().join(format!("nexus-dependency-preview-{}", nexus_core::agent_auth::random_hex().unwrap()));
        fs::create_dir_all(root.join("node_modules/.pnpm/example@1.2.3/node_modules/example")).unwrap();
        fs::write(root.join("package.json"), r#"{"dependencies":{"example":"^1"}}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  .:\n    dependencies:\n      example:\n        version: 1.2.3\n").unwrap();
        let target = root.join("node_modules/.pnpm/example@1.2.3/node_modules/example/package.json");
        fs::write(&target, r#"{"name":"example","version":"1.2.3"}"#).unwrap();
        let before = preview(&root).unwrap();
        assert_eq!(before.entries[0].reason, "missing_link");
        assert!(!root.join("node_modules/example").exists());
        fs::write(&target, r#"{"name":"example","version":"9.0.0"}"#).unwrap();
        let changed = preview(&root).unwrap();
        assert_eq!(changed.entries[0].reason, "local_package_identity_mismatch");
        assert_ne!(before.fingerprint, changed.fingerprint);
        fs::write(root.join("node_modules/example"), "user file").unwrap();
        assert!(preview(&root).unwrap().entries.is_empty());
        assert_eq!(fs::read_to_string(root.join("node_modules/example")).unwrap(), "user file");
        fs::remove_file(root.join("node_modules/example")).unwrap();
        let absent = root.join("removed-target");
        fs::create_dir(&absent).unwrap();
        nexus_core::ReleaseStore::create_missing_module_link(&root.join("node_modules/example"), &absent).unwrap();
        fs::remove_dir(&absent).unwrap();
        let broken = preview(&root).unwrap();
        assert_eq!(broken.entries[0].reason, "existing_broken_link");
        assert!(broken.entries[0].target.is_none());
        #[cfg(windows)] fs::remove_dir(root.join("node_modules/example")).unwrap();
        #[cfg(unix)] fs::remove_file(root.join("node_modules/example")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_paths_that_could_escape_the_release() {
        for value in ["../evil", "C:/evil", "/evil", "a/../../evil", "a\\evil", "a/.. ", "a./b", "a//b"] { assert!(relative(value).is_err()); }
        for value in ["../evil", "@scope/../../evil", "@/evil", "a/b"] { assert!(package_path(value).is_err()); }
        assert!(package_path("@scope/package").is_ok());
    }

    #[test]
    fn pnpm_11_patched_package_uses_recorded_store_limit() {
        let root = std::env::temp_dir().join(format!("nexus-patched-preview-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let package = root.join("node_modules/.pnpm/node-pty@1.2.0-beta.15_patc_04ea68a78398ae52b35b6f6b1ec3bdf9/node_modules/node-pty");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"node-pty","version":"1.2.0-beta.15"}"#).unwrap();
        fs::write(root.join("package.json"), r#"{"dependencies":{"node-pty":"1.2.0-beta.15"}}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  .:\n    dependencies:\n      node-pty:\n        version: 1.2.0-beta.15(patch_hash=b40ae545608897914bd25fb009c97eeac478c34e8a910298ddcb01b746534bb0)\n").unwrap();
        fs::write(root.join("node_modules/.modules.yaml"), "packageManager: pnpm@11.7.0\nvirtualStoreDirMaxLength: 60\n").unwrap();
        let plan = preview(&root).unwrap();
        assert_eq!(plan.entries[0].reason, "missing_link");
        assert_eq!(plan.entries[0].target, Some(fs::canonicalize(&package).unwrap()));
        fs::write(root.join("node_modules/.modules.yaml"), "packageManager: pnpm@11.7.0\nvirtualStoreDirMaxLength: 120\n").unwrap();
        let changed = preview(&root).unwrap();
        assert_ne!(plan.fingerprint, changed.fingerprint);
        assert!(changed.entries[0].target.is_none(), "different layout must not reuse a guessed package");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires NEXUS_TEST_NODE and permission to launch an owned Node fixture"]
    fn startup_repair_restores_real_node_module_loading() {
        let node = PathBuf::from(std::env::var_os("NEXUS_TEST_NODE").expect("explicit Node executable required"));
        let base = std::env::temp_dir().join(format!("nexus-repair-load-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let root = base.join("release"); let data = base.join("data");
        let package = root.join("node_modules/.pnpm/example@1.2.3/node_modules/example");
        fs::create_dir_all(&package).unwrap(); fs::create_dir(&data).unwrap();
        fs::write(package.join("package.json"), r#"{"name":"example","version":"1.2.3","type":"module","exports":"./index.js"}"#).unwrap();
        fs::write(package.join("index.js"), "export default 'module-loaded';").unwrap();
        let importer = root.join("packages/boot/app-boot");
        fs::create_dir_all(&importer).unwrap();
        fs::write(importer.join("package.json"), r#"{"dependencies":{"example":"1.2.3"}}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  packages/boot/app-boot:\n    dependencies:\n      example:\n        version: 1.2.3\n").unwrap();
        let run = || {
            let mut command = std::process::Command::new(&node);
            command.current_dir(&importer).env_remove("NODE_OPTIONS").env_remove("NODE_PATH")
                .args(["--input-type=module", "-e", "import value from 'example'; console.log(value)"]);
            #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
            command.output().unwrap()
        };
        let before = run();
        assert!(!before.status.success());
        assert!(String::from_utf8_lossy(&before.stderr).contains("ERR_MODULE_NOT_FOUND"));
        assert_eq!(ensure_startup(&root, &data).unwrap()["phase"], "repaired");
        let after = run();
        assert!(after.status.success(), "{}", String::from_utf8_lossy(&after.stderr));
        assert_eq!(String::from_utf8_lossy(&after.stdout).trim(), "module-loaded");
        #[cfg(windows)] fs::remove_dir(importer.join("node_modules/example")).unwrap();
        #[cfg(unix)] fs::remove_file(importer.join("node_modules/example")).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn repair_rejects_stale_preview_and_preserves_a_durable_record() {
        let base = std::env::temp_dir().join(format!("nexus-dependency-repair-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let root = base.join("release");
        let data = base.join("data");
        let package = root.join("node_modules/.pnpm/example@1.2.3/node_modules/example");
        fs::create_dir_all(&package).unwrap(); fs::create_dir(&data).unwrap();
        fs::write(root.join("package.json"), r#"{"dependencies":{"example":"^1"}}"#).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "importers:\n  .:\n    dependencies:\n      example:\n        version: 1.2.3\n").unwrap();
        fs::write(package.join("package.json"), r#"{"name":"example","version":"1.2.3"}"#).unwrap();
        let plan = preview(&root).unwrap();
        assert!(apply(&root, "stale", &data).is_err());
        assert!(!data.join("dependency-repair-history").exists());
        let result = apply(&root, &plan.fingerprint, &data).unwrap();
        assert_eq!(result["phase"], "repaired");
        assert_eq!(result["startup_verified"], false);
        assert_eq!(fs::canonicalize(root.join("node_modules/example")).unwrap(), fs::canonicalize(&package).unwrap());
        assert!(preview(&root).unwrap().entries.is_empty());
        let record = PathBuf::from(result["record"].as_str().unwrap());
        for file in ["plan.json", "manifests.json", "pnpm-lock.yaml", "result.json"] { assert!(record.join(file).is_file()); }
        assert!(apply(&root, &plan.fingerprint, &data).is_err());
        // Remove only the fixture link, never recurse through its target.
        #[cfg(windows)] fs::remove_dir(root.join("node_modules/example")).unwrap();
        #[cfg(unix)] fs::remove_file(root.join("node_modules/example")).unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
