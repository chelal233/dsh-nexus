//! Bounded, validated history of Nexus profile indexes, never Harness files.
use crate::{NexusPaths, ProfileCatalog, PROFILE_SCHEMA_VERSION};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, io, path::PathBuf};
const LIMIT: u64 = 4 * 1024 * 1024;
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }
pub fn validate(bytes: &[u8]) -> io::Result<ProfileCatalog> {
    let value: Value = serde_json::from_slice(bytes)?;
    let object = value.as_object().ok_or_else(|| invalid("Invalid profile history"))?;
    if object.get("schema_version") != Some(&json!(PROFILE_SCHEMA_VERSION)) || object.keys().any(|key| !matches!(key.as_str(), "schema_version" | "active_profile" | "profiles")) {
        return Err(invalid("Unsupported profile history format"));
    }
    let catalog: ProfileCatalog = serde_json::from_value(value)?;
    let normalized = ProfileCatalog::new(catalog.active_profile.clone(), catalog.profiles.clone())?;
    if normalized != catalog { return Err(invalid("Profile history is not a complete normalized catalog")); }
    Ok(catalog)
}
fn directory(paths: &NexusPaths) -> io::Result<PathBuf> {
    let directory = paths.root.join("profile-history");
    for path in [&paths.root, &directory] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !crate::path_is_reparse(&metadata) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound && path == &directory => (),
            _ => return Err(invalid("Profile history must use ordinary directories")),
        }
    }
    Ok(directory)
}
pub fn list(paths: &NexusPaths) -> io::Result<Vec<Value>> {
    let directory = directory(paths)?;
    if !directory.exists() { return Ok(vec![]); }
    let mut result = vec![];
    for (index, entry) in fs::read_dir(directory)?.take(129).enumerate() {
        if index == 128 { return Err(invalid("Recovery history listing exceeded its limit; some recovery points were not inspected")); }
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if let Ok((_, manifest)) = load(paths, &id) { result.push(manifest); }
    }
    result.sort_by(|a,b| b["created_at_unix_nanos"].as_u64().cmp(&a["created_at_unix_nanos"].as_u64()));
    result.truncate(64);
    Ok(result)
}
pub fn load(paths: &NexusPaths, id: &str) -> io::Result<(Vec<u8>, Value)> {
    if id.is_empty() || id.len()>128 || !id.bytes().all(|c|c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c==b'-') { return Err(invalid("Invalid recovery point")); }
    let bytes = crate::read_regular_file_bounded(&directory(paths)?.join(id), LIMIT)?.ok_or_else(||invalid("Recovery point missing"))?;
    let manifest: Value = serde_json::from_slice(&bytes)?;
    if manifest["format_version"] != 1 || manifest["id"] != id { return Err(invalid("Invalid recovery point identity")); }
    let nanos = manifest["created_at_unix_nanos"].as_u64().ok_or_else(||invalid("Recovery time is missing"))?;
    if manifest["created_at_unix"].as_u64() != Some(nanos / 1_000_000_000) || !id.starts_with(&format!("{nanos}-")) { return Err(invalid("Recovery time is inconsistent")); }
    let catalog_bytes = serde_json::to_vec(&manifest["catalog"])?;
    validate(&catalog_bytes)?;
    if manifest["sha256"] != format!("{:x}",Sha256::digest(&catalog_bytes)) { return Err(invalid("Recovery point integrity check failed")); }
    Ok((catalog_bytes, manifest))
}
pub fn capture(paths: &NexusPaths, catalog: &ProfileCatalog) -> io::Result<()> {
    let current = crate::read_regular_file_bounded(&paths.profiles_file, LIMIT)?.ok_or_else(||invalid("No saved profile catalog to back up"))?;
    if validate(&current)? != *catalog { return Err(invalid("Profile catalog changed before history capture")); }
    let value = serde_json::to_value(catalog)?;
    let bytes = serde_json::to_vec(&value)?;
    validate(&bytes)?;
    let hash = format!("{:x}",Sha256::digest(&bytes));
    let history = list(paths)?;
    if history.first().is_some_and(|point|point["sha256"]==hash) { return Ok(()); }
    let directory = directory(paths)?;
    fs::create_dir_all(&directory)?;
    let now=crate::unix_time_nanos();
    let id=format!("{now}-{}",&hash[..16]);
    crate::write_private_json_atomic(&directory,&directory.join(&id),&json!({"format_version":1,"id":id,"created_at_unix_nanos":now,"created_at_unix":now/1_000_000_000,"sha256":hash,"catalog":value}))?;
    for old in history.iter().skip(63) {
        if let Some(id)=old["id"].as_str() { let _=fs::remove_file(directory.join(id)); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_saved_catalogs_are_history_and_retention_is_bounded() {
        let paths=NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-profile-history-{}",crate::new_instance_id())));
        paths.ensure_directories().unwrap();
        let store=crate::ProfileStore::new(paths.clone());
        assert!(capture(&paths,&ProfileCatalog::default()).is_err());
        store.load().unwrap();
        assert_eq!(list(&paths).unwrap().len(),1);
        store.load().unwrap();
        assert_eq!(list(&paths).unwrap().len(),1);
        for index in 0..66 { store.select(&format!("test{index}")).unwrap(); }
        assert_eq!(list(&paths).unwrap().len(),64);
        assert_eq!(fs::read_dir(paths.root.join("profile-history")).unwrap().count(),64);
        let history=list(&paths).unwrap();
        assert_eq!(history[0]["catalog"]["active_profile"],"test65");
        assert!(capture(&paths,&ProfileCatalog::default()).is_err());
        assert_eq!(list(&paths).unwrap().len(),64);
        assert!(load(&paths,"../profiles.json").is_err());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn unavailable_history_does_not_fail_committed_profile_changes_or_loads() {
        let paths=NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-history-failure-{}",crate::new_instance_id())));
        paths.ensure_directories().unwrap();
        fs::write(paths.root.join("profile-history"),b"blocked").unwrap();
        let store=crate::ProfileStore::new(paths.clone());
        store.load().unwrap();
        assert_eq!(store.select("work").unwrap().active_profile,"work");
        assert_eq!(store.read().unwrap().unwrap().active_profile,"work");
        assert_eq!(crate::ProfileStore::new(paths.clone()).load().unwrap().active_profile,"work");
        assert!(list(&paths).is_err());
        fs::remove_dir_all(paths.root).unwrap();
    }
}
