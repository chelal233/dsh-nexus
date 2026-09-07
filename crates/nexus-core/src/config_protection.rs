//! Durable location protection shared by configuration saves and uninstall.
use std::{fs, io, path::{Component, Path, PathBuf}, sync::Mutex};
use serde::{Deserialize, Serialize};
use crate::{data_root_identity, NexusPaths, write_json_atomic};

pub const PROTECTED_HARNESS_HOMES_FILE: &str = "protected-harness-homes.json";
pub const PREVIOUS_CONFIG_FILE: &str = "config.previous.json";
static PROTECTION_GATE: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedHomes { schema_version: u32, root: PathBuf, homes: Vec<PathBuf> }

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

pub fn path_is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))] { metadata.file_type().is_symlink() }
}

fn identity(path: &Path) -> io::Result<String> { data_root_identity(&NexusPaths::from_root(path.to_path_buf())) }

fn validate_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(invalid("Harness protection requires an absolute path without traversal"));
    }
    Ok(())
}

/// Read only an ordinary bounded configuration file. Invalid home metadata must
/// not be overwritten before its data location can be protected.
pub fn configured_harness_home(root: &Path) -> io::Result<Option<PathBuf>> {
    let path = root.join("config.json");
    let Some(bytes) = crate::read_regular_file_bounded(&path, 4 * 1024 * 1024)? else { return Ok(None); };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let preferences = value.get("harness_preferences").filter(|value| !value.is_null());
    if preferences.is_some_and(|value| !value.is_object()) { return Err(invalid("Invalid Harness preferences")); }
    match preferences.and_then(|value| value.get("home")) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.trim().into())),
        _ => Err(invalid("Invalid Harness home")),
    }
}

/// Read protection metadata without changing configuration or creating paths.
pub fn read_protected_harness_homes(root: &Path) -> io::Result<Vec<PathBuf>> {
    validate_path(root)?;
    let marker = root.join(PROTECTED_HARNESS_HOMES_FILE);
    let Some(bytes) = crate::read_regular_file_bounded(&marker, 1024 * 1024)? else { return Ok(Vec::new()); };
    let saved: ProtectedHomes = serde_json::from_slice(&bytes)?;
    validate_path(&saved.root)?;
    if saved.schema_version != 1 || saved.homes.len() > 256 || identity(&saved.root)? != identity(root)? {
        return Err(invalid("Harness protection record does not match this data root"));
    }
    for home in &saved.homes { validate_path(home)?; }
    Ok(saved.homes)
}

/// Refuse to remove a directory containing, contained in, or identical to a
/// current or historical Harness home. Compare filesystem identities across
/// Windows namespaces, and inspect the original access path as well as its
/// resolved target so removing an ancestor of a home junction is protected.
pub fn ensure_harness_homes_preserved(root: &Path, target: &Path) -> io::Result<()> {
    validate_path(target)?;
    let mut homes = read_protected_harness_homes(root)?;
    homes.extend(configured_harness_home(root)?);
    if let Some(home) = std::env::var_os("DSH_HOME").filter(|value| !value.is_empty()) {
        homes.push(home.into());
    }
    homes.push(root.join(".dsh"));
    #[cfg(windows)]
    let user_home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let user_home = std::env::var_os("HOME");
    if let Some(user_home) = user_home.filter(|value| !value.is_empty()) {
        homes.push(PathBuf::from(user_home).join(".dsh"));
    }
    for home in homes {
        validate_path(&home)?;
        if paths_overlap_by_identity(target, &home)? {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy,
                "This directory contains or overlaps protected Harness data"));
        }
    }
    Ok(())
}

pub(crate) fn paths_overlap_by_identity(left: &Path, right: &Path) -> io::Result<bool> {
    fn existing_identity(path: &Path) -> io::Result<Option<String>> {
        match identity(path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn contains(parent: &Path, child: &Path) -> io::Result<bool> {
        let Some(parent_id) = existing_identity(parent)? else { return Ok(false); };
        for ancestor in child.ancestors() {
            if existing_identity(ancestor)?.as_ref() == Some(&parent_id) { return Ok(true); }
        }
        if let Ok(resolved) = fs::canonicalize(child) {
            for ancestor in resolved.ancestors() {
                if existing_identity(ancestor)?.as_ref() == Some(&parent_id) { return Ok(true); }
            }
        }
        Ok(false)
    }
    Ok(contains(left, right)? || contains(right, left)?)
}

/// Persist protection before a config overwrite or deletion. Homes are mapped
/// into the root's namespace using actual directory identities, including UNC
/// aliases. No Harness directory is created, migrated or removed here.
pub fn protect_harness_homes(root: &Path, candidates: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let _guard = PROTECTION_GATE.lock().map_err(|_| invalid("Harness protection lock is poisoned"))?;
    validate_path(root)?;
    for ancestor in root.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if path_is_reparse(&metadata) { return Err(invalid("Nexus data path contains a link; data was preserved")); }
    }
    let root = fs::canonicalize(root)?;
    let root_id = identity(&root)?;
    let marker = root.join(PROTECTED_HARNESS_HOMES_FILE);
    let mut saved = match crate::read_regular_file_bounded(&marker, 1024 * 1024)? {
        Some(bytes) => {
            let saved: ProtectedHomes = serde_json::from_slice(&bytes)?;
            if saved.schema_version != 1 || saved.homes.len() > 256 || identity(&saved.root)? != root_id {
                return Err(invalid("Harness protection record does not match this data root"));
            }
            saved
        }
        None => ProtectedHomes { schema_version: 1, root: root.clone(), homes: Vec::new() },
    };
    let all: Vec<_> = saved.homes.iter().chain(candidates).cloned().collect();
    for home in all {
        validate_path(&home)?;
        let resolved = match fs::canonicalize(&home) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !resolved.is_dir() { return Err(invalid("Harness home is not a directory")); }
        let home_id = identity(&resolved)?;
        for ancestor in root.ancestors() {
            if identity(ancestor)? == home_id { return Err(invalid("Nexus data root is inside DSH_HOME; data was preserved")); }
        }
        for ancestor in resolved.ancestors() {
            if identity(ancestor)? == root_id {
                let relative = resolved.strip_prefix(ancestor).map_err(|_| invalid("Cannot bind Harness home to Nexus data root"))?;
                let local = root.join(relative);
                if identity(&local)? != home_id { return Err(invalid("Harness home namespace identity is ambiguous; data was preserved")); }
                if !saved.homes.contains(&local) { saved.homes.push(local); }
                break;
            }
        }
    }
    if saved.homes.len() > 256 { return Err(invalid("Too many protected Harness locations; data was preserved")); }
    saved.root = root.clone();
    if !saved.homes.is_empty() { write_json_atomic(&root, &marker, &saved)?; }
    Ok(saved.homes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigStore, NexusConfigFile};

    fn fixture() -> (PathBuf, ConfigStore) {
        let root = std::env::temp_dir().join(format!("nexus-config-protection-{}", crate::new_instance_id()));
        fs::create_dir_all(&root).unwrap();
        (root.clone(), ConfigStore::new(NexusPaths::from_root(root)))
    }
    fn config(home: &Path) -> NexusConfigFile {
        NexusConfigFile { harness_preferences: Some(nexus_protocol::HarnessPreferencesPayload {
            home: Some(home.to_string_lossy().into_owned()), ..Default::default()
        }), ..Default::default() }
    }

    #[test]
    fn marker_failure_preserves_config_and_previous_valid_backup() {
        let (root, store) = fixture();
        let first = config(&root.join("missing-home"));
        store.write(&first).unwrap();
        let before = fs::read(root.join("config.json")).unwrap();
        fs::write(root.join(PROTECTED_HARNESS_HOMES_FILE), "invalid").unwrap();
        assert!(store.write(&NexusConfigFile::default()).is_err());
        assert_eq!(fs::read(root.join("config.json")).unwrap(), before);
        assert!(!root.join(PREVIOUS_CONFIG_FILE).exists());
        assert!(!root.join("missing-home").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn previous_config_is_valid_and_same_value_saves_do_not_replace_it() {
        let (root, store) = fixture();
        let first = config(&root.join("first-missing"));
        let second = config(&root.join("second-missing"));
        store.write(&first).unwrap();
        store.write(&second).unwrap();
        let backup = fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap();
        assert_eq!(serde_json::from_slice::<NexusConfigFile>(&backup).unwrap(), first);
        store.transaction(|_| Ok(())).unwrap();
        assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(), backup);
        // Valid JSON but invalid config is never adopted as the valid backup.
        fs::write(root.join("config.json"), r#"{"snapshots":{"healthy_slots":0,"max_manual_snapshots":1}}"#).unwrap();
        store.write(&second).unwrap();
        assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(), backup);
        fs::write(root.join("config.json"), "broken json").unwrap();
        assert!(store.write(&second).is_err());
        assert_eq!(fs::read(root.join(PREVIOUS_CONFIG_FILE)).unwrap(), backup);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inherited_and_selected_homes_are_unioned_without_creating_new_home() {
        let (root, _) = fixture();
        let inherited = root.join("downloads/inherited");
        let selected = root.join("runtimes/selected");
        fs::create_dir_all(&inherited).unwrap();
        fs::create_dir_all(&selected).unwrap();
        let missing = root.join("new/home");
        let saved = protect_harness_homes(&root, &[inherited.clone(), selected.clone(), missing.clone()]).unwrap();
        assert_eq!(saved.len(), 2);
        assert!(!missing.exists());
        assert_eq!(protect_harness_homes(&root, &[]).unwrap(), saved);
        assert!(saved.contains(&fs::canonicalize(inherited).unwrap()));
        assert!(saved.contains(&fs::canonicalize(selected).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_removal_preserves_current_and_redirected_homes() {
        for redirect in 0..3 {
            let (root, config_store) = fixture();
            let paths = NexusPaths::from_root(root.clone());
            let releases = crate::ReleaseStore::new(paths.clone());
            releases.register("active", "1", None, None).unwrap();
            releases.register("old", "2", None, None).unwrap();
            releases.register("free", "3", None, None).unwrap();
            releases.promote("active").unwrap();
            let home = paths.releases_dir.join("old").join("user-home");
            fs::create_dir_all(&home).unwrap();
            fs::write(home.join("sentinel"), "user data").unwrap();
            config_store.write(&config(&home)).unwrap();
            if redirect == 1 { config_store.write(&NexusConfigFile::default()).unwrap(); }
            if redirect == 2 { config_store.write(&config(&root.with_extension("outside"))).unwrap(); }
            assert_eq!(releases.remove("old").unwrap_err().kind(), io::ErrorKind::ResourceBusy);
            assert_eq!(fs::read_to_string(home.join("sentinel")).unwrap(), "user data");
            releases.remove("free").unwrap();
            assert!(releases.load().unwrap().find("old").is_some());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn release_removal_refuses_damaged_protection_records() {
        let (root, _) = fixture();
        let paths = NexusPaths::from_root(root.clone());
        let releases = crate::ReleaseStore::new(paths.clone());
        releases.register("free", "1", None, None).unwrap();
        fs::write(root.join(PROTECTED_HARNESS_HOMES_FILE), "broken").unwrap();
        assert!(releases.remove("free").is_err());
        assert!(paths.releases_dir.join("free").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_home_ancestor_overlap_is_protected_without_changing_environment() {
        let (root, _) = fixture();
        let default_home = root.join("user").join(".dsh");
        let slot = default_home.join("Nexus").join("releases").join("old");
        fs::create_dir_all(&slot).unwrap();
        assert!(paths_overlap_by_identity(&slot, &default_home).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
