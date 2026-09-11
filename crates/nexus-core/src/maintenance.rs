//! Bounded local space inspection and explicitly selected cleanup.
use std::{collections::BTreeSet, fs, io::{self, Read}, path::{Path, PathBuf}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::{NexusPaths, ReleaseStore, CheckpointRestoreJournalStore, data_root_identity, log_file_identity,
    path_is_reparse, ensure_harness_homes_preserved, read_protected_harness_homes,
    configured_harness_home, write_json_atomic, new_instance_id};

const PREVIEW_FILE: &str = "cleanup-preview.json";
const RESULT_FILE: &str = "cleanup-result.json";
const MAX_ENTRIES: usize = 250_000;
const TARGET_SCAN_SECONDS: u64 = 300;
const PREVIEW_SCAN_SECONDS: u64 = 360;
const MAX_OWNERSHIP_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RECORD: u64 = 8 * 1024 * 1024;
const MAX_CONTENT_SCAN_BYTES: u64 = 8 * 1024 * 1024 * 1024;
static ACTIVE: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
static PATCH_ACCESS: std::sync::Mutex<std::collections::BTreeMap<String,(usize, bool)>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
#[derive(Debug)]
pub struct PatchReadLease(String);
impl Drop for PatchReadLease { fn drop(&mut self) { if let Ok(mut map)=PATCH_ACCESS.lock() {if let Some(access)=map.get_mut(&self.0){access.0-=1;if access.0==0 && !access.1{map.remove(&self.0);}}} } }
pub fn protect_patch_cache(paths:&NexusPaths) -> io::Result<std::sync::Arc<PatchReadLease>> {
    let key=data_root_identity(paths)?;let mut map=PATCH_ACCESS.lock().map_err(|_| invalid("Patch cache lock failed"))?;let access=map.entry(key.clone()).or_default();
    if access.1 { return Err(invalid("Patch cache cleanup is running")); } access.0+=1; Ok(std::sync::Arc::new(PatchReadLease(key)))
}
struct PatchDeleteLease(String);
impl Drop for PatchDeleteLease { fn drop(&mut self) { if let Ok(mut map)=PATCH_ACCESS.lock() {map.remove(&self.0);} } }
fn patch_delete_lease(paths:&NexusPaths) -> io::Result<PatchDeleteLease> {let key=data_root_identity(paths)?;let mut map=PATCH_ACCESS.lock().map_err(|_| invalid("Patch cache lock failed"))?;let access=map.entry(key.clone()).or_default();
    if access.0>0 || access.1 {return Err(invalid("Patch preview or download is active; cache is protected"));} access.1=true; Ok(PatchDeleteLease(key)) }
fn patch_protection(paths: &NexusPaths, path: &Path) -> io::Result<Option<String>> {
    if PATCH_ACCESS.lock().map_err(|_| invalid("Patch cache lock failed"))?.get(&data_root_identity(paths)?).is_some_and(|access|access.0>0) { return Ok(Some("Patch preview or download is active; cache is protected".into())); }
    for name in ["config-write.pending.json","config-preserve.pending.json","cold-publication.json"] { if paths.root.join(name).try_exists()? { return Ok(Some("Configuration transaction protects patch cache".into())); } }
    for name in ["snapshots","snapshot-transactions","checkpoints"] { let path=paths.root.join(name);if path.try_exists()? && !entries(&path)?.is_empty() { return Ok(Some("Snapshots may reference patch cache; retained conservatively".into())); } }
    let name=path.file_stem().and_then(|n|n.to_str()).unwrap_or("");
    if name.len()!=64 || !name.bytes().all(|b|b.is_ascii_hexdigit()) || path.extension().and_then(|e|e.to_str())!=Some("yml") {return Ok(Some("Unrecognized patch cache file is preserved".into()));}
    for file in ["config.json", crate::PREVIOUS_CONFIG_FILE] {
        if let Some(bytes)=crate::read_regular_file_bounded(&paths.root.join(file),4*1024*1024)? {
            let config:crate::NexusConfigFile=crate::decode_config_document(&bytes)?;
            // Include disabled entries and legacy literal cache paths, not only effective settings.
            if serde_json::to_string(&config)?.contains(name) { return Ok(Some("Current or previous configuration references this patch".into())); }
        }
    }
    let bytes=crate::read_regular_file_bounded(path,1024*1024)?.ok_or_else(|| invalid("Patch cache disappeared"))?;
    crate::verify_private_file(&fs::File::open(path)?)?;
    if format!("{:x}",Sha256::digest(&bytes))!=name {return Ok(Some("Patch cache content identity does not match".into()));}
    Ok(None)
}
struct ActiveCleanup(String);
impl Drop for ActiveCleanup { fn drop(&mut self) { if let Ok(mut active) = ACTIVE.lock() { active.remove(&self.0); } } }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpaceArea { pub kind: String, pub path: PathBuf, pub bytes: Option<u64>, pub error: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupItem {
    pub id: String, pub kind: String, pub name: String, pub path: PathBuf,
    pub bytes: Option<u64>, pub eligible: bool, pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupPreview {
    pub preview_id: String, pub created_at_unix: u64, pub retention_days: u32,
    pub areas: Vec<SpaceArea>, pub items: Vec<CleanupItem>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupItemResult { pub id: String, pub name: String, pub state: String, pub error: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupResult {
    pub preview_id: String, pub state: String, pub started_at_unix: u64,
    pub items: Vec<CleanupItemResult>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaintenanceStatus { pub preview: Option<CleanupPreview>, pub result: Option<CleanupResult> }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EntryIdentity { relative: PathBuf, identity: String, bytes: u64, modified_nanos: u128, directory: bool,
    #[serde(default)] link_target: Option<PathBuf>,
    #[serde(default)] content_sha256: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PreviewRecord { root_identity: String, preview: CleanupPreview, fingerprints: Vec<Option<String>> }

/// Kept outside the target until the directory has actually disappeared. This
/// is ownership evidence, not a second operation state machine. Later previews
/// may be replaced without losing an interrupted deletion's authorization.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupOwnership {
    schema_version: u32, root_identity: String, kind: String, name: String,
    target: PathBuf, target_identity: String, pub(crate) manifest: serde_json::Value,
    entries: Vec<EntryIdentity>,
}
impl CleanupOwnership {
    pub(crate) fn record_path(paths: &NexusPaths, kind: &str, name: &str) -> io::Result<PathBuf> {
        if !matches!(kind, "release" | "diagnostic" | "terminal_quarantine") || !crate::is_valid_release_id(name)
            || kind=="terminal_quarantine" && !terminal_quarantine_name(name) {
            return Err(invalid("Invalid cleanup ownership target"));
        }
        Ok(paths.run_dir.join(format!("cleanup-owned-{kind}-{name}.json")))
    }
    fn target_path(paths: &NexusPaths, kind: &str, name: &str) -> io::Result<PathBuf> {
        Self::record_path(paths, kind, name)?;
        Ok(match kind { "release" => paths.releases_dir.join(name), "terminal_quarantine" => paths.run_dir.join(name), _ => paths.diagnostics_dir.join(name) })
    }
    pub(crate) fn load(paths: &NexusPaths, kind: &str, name: &str) -> io::Result<Option<Self>> {
        let Some(bytes) = crate::read_regular_file_bounded(&Self::record_path(paths, kind, name)?, MAX_OWNERSHIP_BYTES)? else { return Ok(None); };
        let record: Self = serde_json::from_slice(&bytes)?;
        if record.schema_version != 1 || record.kind != kind || record.name != name
            || record.target != Self::target_path(paths, kind, name)? || record.root_identity != data_root_identity(paths)?
            || record.entries.is_empty() || record.entries.len() > MAX_ENTRIES {
            return Err(invalid("Cleanup ownership does not match this target"));
        }
        if !record.entries.iter().any(|e| e.relative.as_os_str().is_empty() && e.directory && e.identity == record.target_identity) {
            return Err(invalid("Cleanup ownership is missing its directory identity"));
        }
        for entry in &record.entries {
            if entry.relative.is_absolute() || entry.relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(invalid("Cleanup ownership contains an unsafe relative path"));
            }
        }
        record.validate_content_evidence()?;
        Ok(Some(record))
    }
    #[cfg(test)]
    pub(crate) fn begin(paths: &NexusPaths, kind: &str, name: &str, manifest: serde_json::Value) -> io::Result<Self> {
        Self::begin_bound(paths, kind, name, manifest, None)
    }
    pub(crate) fn begin_bound(paths: &NexusPaths, kind: &str, name: &str, manifest: serde_json::Value, expected: Option<&str>) -> io::Result<Self> {
        if let Some(record) = Self::load(paths, kind, name)? {
            let remaining = record.remaining()?.unwrap_or_default();
            if let Some(expected) = expected { if fingerprint_digest(&remaining)? != expected { return Err(invalid("Cleanup target changed after confirmation")); } }
            return Ok(record);
        }
        let target = Self::target_path(paths, kind, name)?;
        let entries = fingerprint(&target)?;
        if let Some(expected) = expected { if fingerprint_digest(&entries)? != expected { return Err(invalid("Cleanup target changed after confirmation")); } }
        let record = Self { schema_version: 1, root_identity: data_root_identity(paths)?, kind: kind.into(), name: name.into(),
            target_identity: data_root_identity(&NexusPaths::from_root(target.clone()))?, target, manifest, entries };
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() as u64 > MAX_OWNERSHIP_BYTES { return Err(invalid("Cleanup ownership exceeds the supported bound")); }
        crate::write_private_bytes_atomic(&paths.run_dir, &Self::record_path(paths, kind, name)?, &bytes)?;
        Ok(record)
    }
    fn list(paths: &NexusPaths, kind: &str) -> io::Result<Vec<PathBuf>> {
        let prefix = format!("cleanup-owned-{kind}-");
        let mut records = Vec::new();
        for path in entries(&paths.run_dir)? {
            let Some(name) = path.file_name().and_then(|v| v.to_str()).and_then(|v| v.strip_prefix(&prefix)).and_then(|v| v.strip_suffix(".json")) else { continue; };
            // Derive only the target name here. Invalid/legacy evidence must
            // protect that target, not disable inspection of every other one.
            if crate::is_valid_release_id(name) { records.push(Self::target_path(paths, kind, name)?); }
        }
        Ok(records)
    }
    fn validate_content_evidence(&self) -> io::Result<()> {
        for entry in &self.entries {
            if !entry.directory && entry.link_target.is_none()
                && !entry.content_sha256.as_ref().is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())) {
                return Err(invalid("Cleanup ownership lacks verified file contents; original files and record are preserved"));
            }
        }
        Ok(())
    }
    fn remaining(&self) -> io::Result<Option<Vec<EntryIdentity>>> {
        self.remaining_until(Instant::now() + Duration::from_secs(TARGET_SCAN_SECONDS))
    }
    fn remaining_until(&self, deadline: Instant) -> io::Result<Option<Vec<EntryIdentity>>> {
        self.validate_content_evidence()?;
        let current = match fingerprint_until(&self.target, deadline) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !self.target.try_exists()? => return Ok(None),
            Err(error) => return Err(error),
        };
        let original_entries: std::collections::BTreeMap<_, _> = self.entries.iter().map(|e| (&e.relative, e)).collect();
        for entry in &current {
            let Some(original) = original_entries.get(&entry.relative) else {
                return Err(invalid("New contents appeared in a cleanup target; they were preserved"));
            };
            if entry.identity != original.identity || entry.directory != original.directory || entry.link_target != original.link_target || entry.content_sha256 != original.content_sha256
                || ((!entry.directory || entry.link_target.is_some()) && (entry.bytes != original.bytes || entry.modified_nanos != original.modified_nanos)) {
                return Err(invalid("A cleanup target changed identity or content; it was preserved"));
            }
        }
        Ok(Some(current))
    }
    pub(crate) fn delete(&self, paths: &NexusPaths) -> io::Result<()> { self.delete_inner(paths, 0) }
    // Fault 1 is the real final-directory boundary; fault 2 is the boundary
    // after removing that directory but before retiring external ownership.
    fn delete_inner(&self, paths: &NexusPaths, fault: u8) -> io::Result<()> {
        if self.root_identity != data_root_identity(paths)? { return Err(invalid("Cleanup data root changed")); }
        ensure_harness_homes_preserved(&paths.root, &self.target)?;
        if let Some(mut current) = self.remaining()? {
            let mut budget = ContentScanBudget::new(Instant::now() + Duration::from_secs(TARGET_SCAN_SECONDS));
            current.sort_by_key(|e| std::cmp::Reverse(e.relative.components().count()));
            for entry in current {
                if entry.relative.as_os_str().is_empty() && fault == 1 { return Err(io::Error::other("Injected interruption before final directory removal")); }
                let path = self.target.join(&entry.relative);
                let actual = entry_identity(&path, entry.relative.clone(), &mut budget)?;
                if actual.identity != entry.identity || actual.link_target != entry.link_target || actual.directory != entry.directory {
                    return Err(invalid("Cleanup object changed before deletion"));
                }
                if entry.directory { fs::remove_dir(&path)?; } else {
                    if actual.bytes != entry.bytes || actual.modified_nanos != entry.modified_nanos || actual.content_sha256 != entry.content_sha256 {
                        return Err(invalid("Cleanup file changed before deletion"));
                    }
                    fs::remove_file(&path)?;
                }
            }
        }
        if fault == 2 { return Err(io::Error::other("Injected interruption after final directory removal")); }
        fs::remove_file(Self::record_path(paths, &self.kind, &self.name)?)
    }
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
fn bounded_record<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value, Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None), Err(error) => return Err(error),
    };
    if path_is_reparse(&metadata) || !metadata.is_file() { return Err(invalid("Maintenance record is not an ordinary file")); }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD { return Err(invalid("Maintenance record exceeds its size limit")); }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

struct ContentScanBudget { deadline: Instant, remaining: u64 }
impl ContentScanBudget {
    fn new(deadline: Instant) -> Self { Self { deadline, remaining: MAX_CONTENT_SCAN_BYTES } }
    fn check(&self) -> io::Result<()> {
        if Instant::now() > self.deadline { Err(invalid("Cleanup content inspection exceeded its time limit; files are preserved")) } else { Ok(()) }
    }
}

fn content_sha256(path: &Path, expected: &EntryIdentity, budget: &mut ContentScanBudget) -> io::Result<String> {
    budget.check()?;
    if expected.bytes > budget.remaining { return Err(invalid("Cleanup content inspection exceeded its byte limit; files are preserved")); }
    let mut options = fs::OpenOptions::new(); options.read(true);
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the object itself and deny concurrent writes/replacement while
        // reading. Junctions and symlinks never reach this content reader.
        options.custom_flags(0x00200000).share_mode(1);
    }
    let mut file = options.open(path)?;
    let verify = |file: &fs::File| -> io::Result<()> {
        let metadata = file.metadata()?;
        let identity = log_file_identity(file)?;
        #[cfg(unix)] let identity = identity.strip_prefix("unix:").unwrap_or(&identity);
        if !metadata.is_file() || path_is_reparse(&metadata) || identity != expected.identity
            || metadata.len() != expected.bytes || metadata.modified()?.duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_nanos() != expected.modified_nanos {
            return Err(invalid("Cleanup file changed while reading its contents"));
        }
        Ok(())
    };
    verify(&file)?;
    let mut hasher = Sha256::new(); let mut total = 0u64; let mut buffer = [0u8; 64 * 1024];
    loop {
        budget.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        total = total.checked_add(count as u64).ok_or_else(|| invalid("Cleanup content size overflow"))?;
        if total > expected.bytes || count as u64 > budget.remaining { return Err(invalid("Cleanup file grew while reading its contents")); }
        budget.remaining -= count as u64;
        hasher.update(&buffer[..count]);
    }
    budget.check()?; verify(&file)?;
    if total != expected.bytes { return Err(invalid("Cleanup file size changed while reading its contents")); }
    Ok(format!("{:x}", hasher.finalize()))
}

fn entry_identity(path: &Path, relative: PathBuf, budget: &mut ContentScanBudget) -> io::Result<EntryIdentity> {
    budget.check()?;
    let metadata = fs::symlink_metadata(path)?;
    let linked = path_is_reparse(&metadata);
    // read_link accepts Windows symlinks/mount points; unsupported reparse
    // types fail closed. It reads link data without visiting its target.
    let link_target = if linked { Some(fs::read_link(path)?) } else { None };
    #[cfg(windows)]
    let (identity, directory) = {
        use std::os::windows::fs::{OpenOptionsExt, MetadataExt};
        let file = fs::OpenOptions::new().access_mode(0x80)
            .custom_flags(0x00200000 | 0x02000000).open(path)?;
        let observed = file.metadata()?;
        if path_is_reparse(&observed) != linked { return Err(invalid("Filesystem object changed while inspecting it")); }
        (log_file_identity(&file)?, metadata.file_attributes() & 0x10 != 0)
    };
    #[cfg(unix)]
    let (identity, directory) = {
        use std::os::unix::fs::MetadataExt;
        (format!("{}:{}", metadata.dev(), metadata.ino()), metadata.is_dir())
    };
    #[cfg(not(any(windows, unix)))]
    let (identity, directory) = (log_file_identity(&fs::File::open(path)?)?, metadata.is_dir());
    if !linked && !directory && !metadata.is_file() { return Err(invalid("Unrecognized filesystem object is preserved")); }
    let mut entry = EntryIdentity { relative, identity, directory, link_target, content_sha256: None,
        bytes: if directory || linked { 0 } else { metadata.len() },
        modified_nanos: metadata.modified()?.duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_nanos() };
    if !linked && !directory { entry.content_sha256 = Some(content_sha256(path, &entry, budget)?); }
    Ok(entry)
}

/// Versioned SHA256 of ordered typed entries; never depends on read_dir order.
/// Preview records hold only this digest, not every slot's full file tree.
fn fingerprint_digest(entries: &[EntryIdentity]) -> io::Result<String> {
    struct HashWriter(Sha256);
    impl io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.0.update(bytes); Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut writer = HashWriter(Sha256::new());
    writer.0.update(b"nexus-cleanup-fingerprint-v2\0");
    // Inputs are generated sorted; sort references as well to make the digest
    // contract explicit for callers reusing an allowed-entry record.
    let mut sorted: Vec<_> = entries.iter().collect(); sorted.sort_by(|a, b| a.relative.cmp(&b.relative));
    serde_json::to_writer(&mut writer, &sorted)?;
    Ok(format!("{:x}", writer.0.finalize()))
}

/// Never follows links. A normal pnpm slot can have tens of thousands of files
/// and thousands of junctions; bounded enumeration covers those real trees.
fn fingerprint(path: &Path) -> io::Result<Vec<EntryIdentity>> {
    fingerprint_until(path, Instant::now() + Duration::from_secs(TARGET_SCAN_SECONDS))
}
fn fingerprint_until(path: &Path, deadline: Instant) -> io::Result<Vec<EntryIdentity>> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<EntryIdentity>, budget: &mut ContentScanBudget) -> io::Result<()> {
        if output.len() >= MAX_ENTRIES { return Err(invalid("Capacity inspection limit reached; size is unknown")); }
        let entry = entry_identity(path, path.strip_prefix(root).map_err(io::Error::other)?.into(), budget)?;
        if path == root && entry.link_target.is_some() { return Err(invalid("The cleanup root must not be a link")); }
        let recurse = entry.directory && entry.link_target.is_none();
        output.push(entry);
        if recurse {
            for entry in fs::read_dir(path)? { visit(root, &entry?.path(), output, budget)?; }
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(path, path, &mut result, &mut ContentScanBudget::new(deadline))?;
    result.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(result)
}
fn size(entries: &[EntryIdentity]) -> io::Result<u64> {
    entries.iter().try_fold(0u64, |sum, entry| sum.checked_add(entry.bytes).ok_or_else(|| invalid("Capacity exceeds supported size")))
}
fn capacity(path: &Path, known: &std::collections::BTreeMap<PathBuf, Option<u64>>, deadline: Instant) -> io::Result<u64> {
    fn visit(path: &Path, known: &std::collections::BTreeMap<PathBuf, Option<u64>>, deadline: Instant, count: &mut usize) -> io::Result<u64> {
        if let Some(value) = known.get(path) { return value.ok_or_else(|| invalid("A child directory has an incomplete size")); }
        *count += 1;
        if *count > MAX_ENTRIES || Instant::now() > deadline { return Err(invalid("Capacity inspection is incomplete; refresh to inspect again")); }
        let metadata = fs::symlink_metadata(path)?;
        if path_is_reparse(&metadata) { return Ok(0); } // No linked target is traversed or counted.
        if metadata.is_file() { return Ok(metadata.len()); }
        if !metadata.is_dir() { return Err(invalid("Unrecognized filesystem object")); }
        let mut total = 0u64;
        for entry in fs::read_dir(path)? {
            total = total.checked_add(visit(&entry?.path(), known, deadline, count)?).ok_or_else(|| invalid("Capacity overflow"))?;
        }
        Ok(total)
    }
    visit(path, known, deadline, &mut 0)
}
fn simple_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}
fn known_log(name: &str) -> bool {
    simple_name(name) && (name.starts_with("harness-") || name.starts_with("update-"))
        && (name.ends_with(".stdout.log") || name.ends_with(".stderr.log"))
}
fn entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let metadata = fs::symlink_metadata(path)?;
    if path_is_reparse(&metadata) || !metadata.is_dir() { return Err(invalid("Maintenance directory is not an ordinary directory")); }
    let mut result = Vec::new();
    for entry in fs::read_dir(path)? {
        if result.len() >= 1000 { return Err(invalid("Too many entries; this directory is preserved")); }
        result.push(entry?.path());
    }
    result.sort();
    Ok(result)
}
fn modified(path: &Path) -> u64 {
    fs::symlink_metadata(path).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(u64::MAX, |m| m.as_secs())
}

fn terminal_quarantine_name(name: &str) -> bool {
    name == "terminal-leases-corrupt" || name.strip_prefix("terminal-leases-corrupt-")
        .is_some_and(|suffix| suffix.len() == 64 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn credential_record(paths: &NexusPaths, path: &Path) -> io::Result<serde_json::Value> {
    let name = path.file_name().and_then(|v| v.to_str()).ok_or_else(|| invalid("Invalid recovery record name"))?;
    let id = name.strip_prefix("credential-recovery-").and_then(|v|v.strip_suffix(".json"))
        .filter(|v| crate::is_valid_release_id(v)).ok_or_else(|| invalid("Invalid recovery record identity"))?;
    let value: serde_json::Value = bounded_record(path)?.ok_or_else(|| invalid("Recovery record disappeared"))?;
    if path.parent() != Some(paths.run_dir.as_path()) || value["schema_version"] != 1 || value["operation_id"] != id
        || value["created_at_unix"].as_u64().is_none()
        || !["previous_home","imported_home"].iter().all(|key| value[key].as_str().is_some_and(|v| Path::new(v).is_absolute()))
        || !value["files"].as_array().is_some_and(|files| !files.is_empty() && files.iter().all(|file| file.as_str().is_some_and(|v|
            !v.is_empty() && Path::new(v).components().all(|c| matches!(c,std::path::Component::Normal(_)))))) {
        return Err(invalid("Unrecognized recovery record is preserved"));
    }
    Ok(value)
}

#[derive(Clone)]
pub struct MaintenanceStore { paths: NexusPaths, default_home: Option<PathBuf> }
impl MaintenanceStore {
    pub fn new(paths: NexusPaths) -> Self {
        #[cfg(windows)] let home = std::env::var_os("USERPROFILE");
        #[cfg(not(windows))] let home = std::env::var_os("HOME");
        Self { paths, default_home: home.filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join(".dsh")) }
    }
    pub fn status(&self) -> io::Result<MaintenanceStatus> {
        let preview: Option<PreviewRecord> = bounded_record(&self.paths.run_dir.join(PREVIEW_FILE))?;
        let mut result: Option<CleanupResult> = bounded_record(&self.paths.run_dir.join(RESULT_FILE))?;
        if let Some(value) = result.as_mut() {
            if value.state == "running" && !ACTIVE.lock().map_err(|_| invalid("Maintenance state lock failed"))?.contains(&value.preview_id) {
                value.state = "interrupted".into();
            }
        }
        Ok(MaintenanceStatus { preview: preview.map(|v| v.preview), result })
    }
    pub fn preview(&self, retention_days: u32, protected_logs: &[String]) -> io::Result<MaintenanceStatus> {
        if !(1..=3650).contains(&retention_days) { return Err(invalid("Retention must be between 1 and 3650 days")); }
        let preview = self.inspect(retention_days, protected_logs)?;
        write_json_atomic(&self.paths.run_dir, &self.paths.run_dir.join(PREVIEW_FILE), &preview)?;
        let mut status = self.status()?;
        status.preview = Some(preview.preview);
        Ok(status)
    }
    fn inspect(&self, retention_days: u32, protected_logs: &[String]) -> io::Result<PreviewRecord> {
        // Do not infer disposable slots from a physically half-published config.
        // No CONFIG_WRITE_GATE acquisition here: callers hold maintenance gates.
        match fs::symlink_metadata(self.paths.root.join("config-write.pending.json")) {
            Ok(_) => return Err(io::Error::new(io::ErrorKind::WouldBlock, "Configuration recovery is pending; refresh Settings before previewing or cleaning data")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
        let deadline = Instant::now() + Duration::from_secs(PREVIEW_SCAN_SECONDS);
        let mut areas = Vec::new();
        let mut locations = vec![("Nexus data", self.paths.root.clone()), ("Release slots", self.paths.releases_dir.clone()),
            ("Bundled runtimes", self.paths.runtimes_dir.clone()), ("Downloads (protected)", self.paths.downloads_dir.clone()),
            ("Logs", self.paths.logs_dir.clone()), ("Diagnostics and recovery backups", self.paths.diagnostics_dir.clone()),
            ("Operation and recovery records", self.paths.run_dir.clone()),
            ("Checkpoints (protected)", self.paths.checkpoints_dir.clone()),
            ("Patch cache", self.paths.root.join("patches")), ("Private recovery backups (protected)", self.paths.root.join("recovery-records"))];
        if let Ok(exe) = std::env::current_exe() { locations.push(("Agent program file", exe)); }
        let mut homes = read_protected_harness_homes(&self.paths.root)?;
        homes.extend(configured_harness_home(&self.paths.root)?);
        if let Some(home) = std::env::var_os("DSH_HOME").filter(|v| !v.is_empty()) { homes.push(home.into()); }
        homes.extend(self.default_home.clone());
        homes.sort(); homes.dedup();
        for home in homes { locations.push(("Harness data (protected)", home)); }
        let mut items = Vec::new();
        let mut fingerprints = Vec::new();
        let cutoff = now().saturating_sub(u64::from(retention_days) * 86400);
        let release_store = ReleaseStore::new(self.paths.clone());
        let mut releases = release_store.load()?;
        let mut invalid_targets = std::collections::BTreeMap::new();
        for path in entries(&self.paths.releases_dir)? {
            let Some(name) = path.file_name().and_then(|v| v.to_str()) else { continue; };
            if !crate::is_valid_release_id(name) { continue; }
            match release_store.pending_cleanup(name) {
                Ok(Some(pending)) => releases.releases.push(pending),
                Ok(None) => {},
                Err(error) => { invalid_targets.insert(path.clone(), ("release".to_owned(), error.to_string())); }
            }
        }
        for target in CleanupOwnership::list(&self.paths, "release")? {
            let name = target.file_name().and_then(|v| v.to_str()).ok_or_else(|| invalid("Invalid cleanup target name"))?;
            if !releases.releases.iter().any(|v| v.id == name) {
                match release_store.pending_cleanup(name) {
                    Ok(Some(pending)) => releases.releases.push(pending),
                    Ok(None) => {},
                    Err(error) => { invalid_targets.insert(target.clone(), ("release".to_owned(), error.to_string())); }
                }
            }
        }
        let launch_paths = crate::configuration_launch_paths(&self.paths)?;
        let restore = CheckpointRestoreJournalStore::new(self.paths.clone()).load()?;
        let mut protected = BTreeSet::new();
        protected.extend(releases.current_release.iter().cloned()); protected.extend(releases.last_known_good.iter().cloned());
        if let Some(restore) = restore {
            protected.extend([restore.intent.previous_current_release, restore.intent.previous_last_known_good,
                restore.intent.target_current_release, restore.intent.target_last_known_good].into_iter().flatten());
        }
        let mut protected_logs: BTreeSet<String> = protected_logs.iter().cloned().collect();
        if let Some(session) = crate::HarnessLogSessionStore::new(self.paths.clone()).read()? {
            protected_logs.insert(session.stdout_log_name); protected_logs.insert(session.stderr_log_name);
        }
        // Retain all logs represented by the latest failure bundle, not only
        // recent filenames. A diagnostic copy is also always preserved.
        let mut diagnostics = entries(&self.paths.diagnostics_dir).unwrap_or_default();
        diagnostics.extend(CleanupOwnership::list(&self.paths, "diagnostic")?);
        diagnostics.sort(); diagnostics.dedup();
        diagnostics.sort_by_key(|path| std::cmp::Reverse(modified(path)));
        let mut diagnostic_info = Vec::new();
        let mut kept_failure = false;
        for path in diagnostics {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("diag-") || !simple_name(name) { continue; }
            let ownership = match CleanupOwnership::load(&self.paths, "diagnostic", name) {
                Ok(value) => value,
                Err(error) => {
                    invalid_targets.insert(path.clone(), ("diagnostic".to_owned(), error.to_string()));
                    continue;
                }
            };
            let document=(||->io::Result<crate::DiagnosticsDocument>{
                let document:crate::DiagnosticsDocument=if let Some(record)=&ownership {serde_json::from_value(record.manifest.clone())?}
                    else {bounded_record(&path.join("diagnostics.json"))?.ok_or_else(||invalid("Diagnostic manifest missing"))?};
                crate::validate_diagnostics_bundle(&document.bundle)?;
                if document.schema_version!=crate::DIAGNOSTICS_SCHEMA_VERSION || document.bundle.id!=name {return Err(invalid("Unsupported diagnostic identity or version"));}Ok(document)
            })();
            let document=match document{Ok(value)=>value,Err(error)=>{invalid_targets.insert(path.clone(),("diagnostic".to_owned(),error.to_string()));continue;}};
            let failure = document.bundle.note.as_deref().is_some_and(|note| {
                let note = note.to_ascii_lowercase(); note.contains("fail") || note.contains("crash") || note.contains("error")
            });
            let keep_failure = failure && !kept_failure && ownership.is_none();
            if keep_failure {
                kept_failure = true;
                for file in &document.bundle.files {
                    let name = file.name.strip_prefix("logs/").unwrap_or(&file.name);
                    if known_log(name) { protected_logs.insert(name.to_owned()); }
                }
            }
            diagnostic_info.push((path, keep_failure, document.bundle.files, ownership.is_some()));
        }
        let mut candidates: Vec<(String, PathBuf, Option<String>)> = Vec::new();
        for (path, (kind, reason)) in invalid_targets { candidates.push((kind, path, Some(reason))); }
        for release in &releases.releases {
            let mut reason = protected.contains(&release.id).then(|| "Current, rollback, or recovery version".into());
            if crate::configuration_paths_overlap(&self.paths.releases_dir.join(&release.id), &launch_paths)? {
                reason = Some("Referenced by the configured runtime or Harness launch path".into());
            }
            candidates.push(("release".into(), self.paths.releases_dir.join(&release.id), reason));
        }
        let mut logs = entries(&self.paths.logs_dir).unwrap_or_default();
        logs.sort_by_key(|path| std::cmp::Reverse(modified(path)));
        let latest_update = crate::UpdateStateStore::new(self.paths.clone()).load()?.release_id;
        for (index, path) in logs.into_iter().filter(|path| path.file_name().and_then(|v| v.to_str()).is_some_and(known_log)).enumerate() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let latest_update_log = latest_update.as_ref().is_some_and(|id| name.starts_with(&format!("update-{id}-")));
            let reason = if index < 10 || protected_logs.contains(&name) || latest_update_log { Some("Current or recent failure logs are retained".into()) }
                else if modified(&path) >= cutoff { Some("Within the log retention period".into()) } else { None };
            candidates.push(("log".into(), path, reason));
        }
        let mut complete_diagnostics = 0;
        for (path, failure, files, pending) in diagnostic_info {
            let ownership = if pending { CleanupOwnership::load(&self.paths, "diagnostic", path.file_name().unwrap().to_str().unwrap())? } else { None };
            let index = complete_diagnostics;
            if ownership.is_none() { complete_diagnostics += 1; }
            let mut allowed: BTreeSet<PathBuf> = files.iter().map(|f| PathBuf::from("files").join(&f.name)).collect();
            allowed.insert("diagnostics.json".into());
            allowed.insert("export.json".into());
            let allowed_directories: BTreeSet<PathBuf> = allowed.iter().flat_map(|file| file.ancestors().skip(1).map(Path::to_path_buf)).collect();
            let retained = ownership.is_none() && (index < 3 || failure || modified(&path) >= cutoff);
            let unknown = if retained { false } else if let Some(ownership) = &ownership { ownership.remaining_until(deadline).is_err() } else { fingerprint_until(&path, deadline).map(|values| values.iter().any(|v| {
                if v.directory { !allowed_directories.contains(&v.relative) } else { !allowed.contains(&v.relative) }
            })).unwrap_or(true) };
            let reason = if ownership.is_none() && (index < 3 || failure) { Some("Recent diagnostics and the last failure are retained".into()) }
                else if unknown { Some("Unrecognized diagnostic contents are preserved".into()) }
                else if ownership.is_none() && modified(&path) >= cutoff { Some("Within the retention period".into()) } else { None };
            candidates.push(("diagnostic".into(), path, reason));
        }
        let mut recovery_records = Vec::new();
        let mut operation_paths=entries(&self.paths.run_dir)?;
        operation_paths.extend(CleanupOwnership::list(&self.paths,"terminal_quarantine")?);
        operation_paths.sort();operation_paths.dedup();
        for path in operation_paths {
            let name = path.file_name().and_then(|v|v.to_str()).unwrap_or("");
            if terminal_quarantine_name(name) {
                let owned=CleanupOwnership::load(&self.paths,"terminal_quarantine",name);
                let metadata = fs::symlink_metadata(&path);
                // Ownership can resume an absent target, but never bless a
                // replacement file or reparse point as the former directory.
                let ordinary=metadata.as_ref().is_ok_and(|m|m.is_dir() && !path_is_reparse(m));
                let missing=metadata.as_ref().is_err_and(|e|e.kind()==io::ErrorKind::NotFound);
                let reason = if owned.is_err() || (!ordinary && !(missing && matches!(owned,Ok(Some(_))))) {Some("Unrecognized filesystem object".into())}
                    else if matches!(owned,Ok(Some(_))) {None}
                    else if modified(&path) >= cutoff {Some("Within the retention period".into())} else {None};
                candidates.push(("terminal_quarantine".into(),path,reason));
            } else if name.starts_with("credential-recovery-") && name.ends_with(".json") {
                recovery_records.push(path);
            }
        }
        recovery_records.sort_by_key(|path|std::cmp::Reverse(modified(path)));
        let operation = crate::read_regular_file_bounded(&self.paths.root.join("cold-operation.json"), MAX_RECORD)
            .and_then(|bytes| bytes.map(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).map_err(io::Error::other)).transpose());
        for (index,path) in recovery_records.into_iter().enumerate() {
            let reason = if credential_record(&self.paths,&path).is_err() {Some("Unrecognized recovery record is preserved".into())}
                else if operation.as_ref().map_or(true, |value| value.as_ref().is_some_and(|value|
                    value["credential_recovery_path"].as_str().is_some_and(|v|Path::new(v)==path))) {Some("Current operation recovery record is protected".into())}
                else if index < 3 || modified(&path) >= cutoff {Some("Recent recovery records and records within retention are preserved".into())} else {None};
            candidates.push(("credential_record".into(),path,reason));
        }
        let cache=self.paths.root.join("patches");
        for path in if cache.try_exists()? {entries(&cache)?}else{vec![]} {
            let reason=match patch_protection(&self.paths,&path) {Ok(reason)=>reason,Err(_)=>Some("Patch references could not be verified; cache is protected".into())};
            candidates.push(("patch".into(),path,reason));
        }
        // Spend the bounded inspection time on actionable targets first.
        // Large current/rollback slots must not prevent previewing old slots.
        for (_, path, reason) in &mut candidates {
            if let Err(error) = ensure_harness_homes_preserved(&self.paths.root, path) {
                *reason = Some(error.to_string());
            }
        }
        candidates.sort_by_key(|(_, _, reason)| reason.is_some());
        let mut known_sizes = std::collections::BTreeMap::new();
        for (kind, path, mut reason) in candidates {
            // Protected targets cannot be selected, so content hashes provide
            // no deletion guarantee here. Count metadata without reading GBs
            // of the current runtime's dependencies or following junctions.
            if reason.is_some() {
                let bytes = capacity(&path, &known_sizes, deadline).ok();
                known_sizes.insert(path.clone(), bytes);
                items.push(CleanupItem { id: format!("item-{}", items.len()), kind,
                    name: path.file_name().unwrap().to_string_lossy().into(),
                    path, bytes, eligible: false, reason });
                fingerprints.push(None);
                continue;
            }
            let measured = match CleanupOwnership::load(&self.paths, &kind, path.file_name().unwrap().to_str().unwrap_or("")) {
                Ok(Some(ownership)) => ownership.remaining_until(deadline).map(|v| v.unwrap_or_default()),
                Ok(None) => fingerprint_until(&path, deadline.min(Instant::now() + Duration::from_secs(TARGET_SCAN_SECONDS))),
                Err(_) if matches!(kind.as_str(),"log"|"patch"|"credential_record") => fingerprint_until(&path, deadline.min(Instant::now() + Duration::from_secs(TARGET_SCAN_SECONDS))),
                Err(error) => Err(error),
            };
            let bytes = measured.as_ref().ok().and_then(|v| size(v).ok());
            known_sizes.insert(path.clone(), bytes);
            if let Err(error) = &measured { reason.get_or_insert_with(|| error.to_string()); }
            items.push(CleanupItem { id: format!("item-{}", items.len()), kind, name: path.file_name().unwrap().to_string_lossy().into(),
                path, bytes, eligible: reason.is_none() && bytes.is_some(), reason });
            fingerprints.push(measured.ok().and_then(|entries| fingerprint_digest(&entries).ok()));
        }
        // Reuse each candidate's measured capacity in its parent areas; the
        // root does not recursively reopen every node_modules file again.
        locations.sort_by_key(|(_, path)| std::cmp::Reverse(path.components().count()));
        for (kind, path) in locations {
            let measured = capacity(&path, &known_sizes, deadline);
            let bytes = measured.as_ref().ok().copied(); known_sizes.insert(path.clone(), bytes);
            areas.push(SpaceArea { kind: kind.into(), path, bytes, error: measured.err().map(|e| e.to_string()) });
        }
        Ok(PreviewRecord { root_identity: data_root_identity(&self.paths)?,
            preview: CleanupPreview { preview_id: new_instance_id(), created_at_unix: now(), retention_days, areas, items }, fingerprints })
    }
    pub fn cleanup(&self, preview_id: &str, ids: &[String], protected_logs: &[String]) -> io::Result<MaintenanceStatus> {
        if ids.is_empty() || ids.len() > 1000 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() { return Err(invalid("Select unique cleanup items from the preview")); }
        let record: PreviewRecord = bounded_record(&self.paths.run_dir.join(PREVIEW_FILE))?.ok_or_else(|| invalid("Create a cleanup preview first"))?;
        if record.preview.preview_id != preview_id || record.root_identity != data_root_identity(&self.paths)?
            || now().saturating_sub(record.preview.created_at_unix) > 900 { return Err(invalid("Cleanup preview expired or changed; preview again")); }
        let previous: Option<CleanupResult> = bounded_record(&self.paths.run_dir.join(RESULT_FILE))?;
        let _patch_lease=if record.preview.items.iter().any(|item| item.kind=="patch" && ids.contains(&item.id)) {Some(patch_delete_lease(&self.paths)?)} else {None};
        if previous.as_ref().is_some_and(|v| v.preview_id == preview_id) { return Err(invalid("This preview has already been used; inspect the saved result and preview again")); }
        let fresh = self.inspect(record.preview.retention_days, protected_logs)?;
        let mut selected = Vec::new();
        for id in ids {
            let index = record.preview.items.iter().position(|v| &v.id == id).ok_or_else(|| invalid("Unknown cleanup item"))?;
            let item = &record.preview.items[index];
            let current = fresh.preview.items.iter().find(|v| v.path == item.path && v.kind == item.kind);
            if !item.eligible || !current.is_some_and(|v| v.eligible) { return Err(invalid("An item is now protected; preview again")); }
            let expected = record.fingerprints.get(index).and_then(|v| v.as_ref()).ok_or_else(|| invalid("Incomplete cleanup fingerprint"))?;
            let actual = if matches!(item.kind.as_str(),"log"|"patch"|"credential_record") { fingerprint(&item.path)? }
                else if let Some(ownership) = CleanupOwnership::load(&self.paths, &item.kind, &item.name)? { ownership.remaining()?.unwrap_or_default() }
                else { fingerprint(&item.path)? };
            if &fingerprint_digest(&actual)? != expected { return Err(invalid("An item changed since the preview; no cleanup started")); }
            selected.push((item.clone(), expected.clone()));
        }
        let mut result = CleanupResult { preview_id: preview_id.into(), state: "running".into(), started_at_unix: now(),
            items: selected.iter().map(|(v, _)| CleanupItemResult { id: v.id.clone(), name: v.name.clone(), state: "pending".into(), error: None }).collect() };
        let save = |value: &CleanupResult| write_json_atomic(&self.paths.run_dir, &self.paths.run_dir.join(RESULT_FILE), value);
        ACTIVE.lock().map_err(|_| invalid("Maintenance state lock failed"))?.insert(preview_id.into());
        let _active = ActiveCleanup(preview_id.into());
        save(&result)?;
        for (index, (item, expected)) in selected.into_iter().enumerate() {
            result.items[index].state = "deleting".into(); save(&result)?;
            let removed = (|| -> io::Result<()> {
                ensure_harness_homes_preserved(&self.paths.root, &item.path)?;
                let actual = if matches!(item.kind.as_str(),"log"|"patch"|"credential_record") { fingerprint(&item.path)? }
                    else if let Some(ownership) = CleanupOwnership::load(&self.paths, &item.kind, &item.name)? { ownership.remaining()?.unwrap_or_default() }
                    else { fingerprint(&item.path)? };
                if fingerprint_digest(&actual)? != expected { return Err(invalid("Item changed during cleanup; preserved")); }
                match item.kind.as_str() {
                    "release" => { ReleaseStore::new(self.paths.clone()).remove_with_digest(&item.name, Some(&expected))?; }
                    "log" => fs::remove_file(&item.path)?,
                    "credential_record" => { credential_record(&self.paths,&item.path)?; fs::remove_file(&item.path)?; }
                    "terminal_quarantine" => {
                        if !terminal_quarantine_name(&item.name) { return Err(invalid("Invalid quarantine directory")); }
                        CleanupOwnership::begin_bound(&self.paths,"terminal_quarantine",&item.name,serde_json::json!({"schema_version":1}),Some(&expected))?.delete(&self.paths)?;
                    }
                    "patch" => {if patch_protection(&self.paths,&item.path)?.is_some() {return Err(invalid("Patch became protected"));} fs::remove_file(&item.path)?;},
                    "diagnostic" => {
                        let manifest = if let Some(owned) = CleanupOwnership::load(&self.paths, "diagnostic", &item.name)? { owned.manifest }
                            else { bounded_record::<serde_json::Value>(&item.path.join("diagnostics.json"))?.ok_or_else(|| invalid("Diagnostic manifest disappeared"))? };
                        CleanupOwnership::begin_bound(&self.paths, "diagnostic", &item.name, manifest, Some(&expected))?.delete(&self.paths)?;
                    }
                    _ => return Err(invalid("Unrecognized cleanup category")),
                }
                Ok(())
            })();
            match removed { Ok(()) => result.items[index].state = "removed".into(), Err(error) => {
                result.items[index].state = "failed".into(); result.items[index].error = Some(error.to_string());
            } }
            save(&result)?;
        }
        result.state = "completed".into(); save(&result)?;
        Ok(MaintenanceStatus { preview: Some(record.preview), result: Some(result) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_diagnostic_is_protected_without_hiding_healthy_bundles_or_cleanup() {
        let (paths,store)=fixture();let healthy=crate::DiagnosticsStore::new(paths.clone()).collect(None).unwrap();
        let bad=paths.diagnostics_dir.join("diag-corrupt");fs::create_dir(&bad).unwrap();fs::write(bad.join("diagnostics.json"),b"{broken").unwrap();
        assert!(crate::DiagnosticsStore::new(paths.clone()).list().unwrap().iter().any(|bundle|bundle.id==healthy.id));
        let (_,warnings)=crate::DiagnosticsStore::new(paths.clone()).list_with_warnings().unwrap();assert!(warnings.iter().any(|warning|warning["bundle_id"]=="diag-corrupt"));
        let preview=store.preview(30,&[]).unwrap().preview.unwrap();let item=preview.items.iter().find(|item|item.path==bad).unwrap();
        assert!(!item.eligible);assert!(item.reason.is_some());assert!(bad.join("diagnostics.json").exists());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn patch_cleanup_rechecks_config_and_protects_snapshots_and_private_backups() {
        let (paths,store)=fixture(); let directory=paths.root.join("patches");fs::create_dir(&directory).unwrap();
        let bytes=b"hello: world";let digest=format!("{:x}",Sha256::digest(bytes));let path=directory.join(format!("{digest}.yml"));
        crate::write_private_bytes_atomic(&directory,&path,bytes).unwrap();
        let preview=store.preview(30,&[]).unwrap().preview.unwrap();let item=preview.items.iter().find(|i|i.kind=="patch").unwrap();assert!(item.eligible,"{:?}",item.reason);
        let config=serde_json::json!({"schema_version":1,"harness_preferences":{"patches":[path]}});
        let encoded=config.to_string().replace(&digest,&digest.chars().map(|c|format!("\\u{:04x}",c as u32)).collect::<String>());
        fs::write(paths.config_file.clone(),encoded).unwrap();
        assert!(store.cleanup(&preview.preview_id,&[item.id.clone()],&[]).is_err());assert!(path.exists());
        fs::remove_file(&paths.config_file).unwrap();fs::create_dir_all(paths.root.join("snapshots/test")).unwrap();
        assert!(patch_protection(&paths,&path).unwrap().unwrap().contains("Snapshots"));
        fs::remove_dir_all(&paths.root).unwrap();
    }
    fn fixture() -> (NexusPaths, MaintenanceStore) {
        let root = std::env::temp_dir().join(format!("nexus-space-{}", new_instance_id()));
        let paths = NexusPaths::from_root(root); paths.ensure_directories().unwrap();
        let default_home = Some(paths.root.join("test-user-home").join(".dsh"));
        (paths.clone(), MaintenanceStore { paths, default_home })
    }
    fn old(path: &Path, days: u64) {
        let time = SystemTime::now() - Duration::from_secs(days * 86400);
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new().access_mode(0x100).custom_flags(0x02000000).open(path).unwrap()
        };
        #[cfg(not(windows))]
        let file = fs::File::open(path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(time)).unwrap();
    }
    #[test]
    fn pending_configuration_rejects_preview_before_slot_selection() {
        let root = std::env::temp_dir().join(format!("nexus-maintenance-config-pending-{}", crate::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        fs::write(root.join("config-write.pending.json"), b"private pending").unwrap();
        let store = MaintenanceStore::new(paths);
        assert!(store.preview(30, &[]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    fn free_release(paths: &NexusPaths, name: &str) -> PathBuf {
        ReleaseStore::new(paths.clone()).register(name, "1", None, None).unwrap();
        let path = paths.releases_dir.join(name);
        fs::write(path.join("payload"), "original").unwrap(); path
    }
    fn preview(store: &MaintenanceStore) -> CleanupPreview { store.preview(30, &[]).unwrap().preview.unwrap() }
    fn selected(preview: &CleanupPreview, name: &str) -> String {
        preview.items.iter().find(|item| item.name == name && item.eligible).unwrap().id.clone()
    }
    fn diagnostic_fixture(paths: &NexusPaths, name: &str, age: u64) -> PathBuf {
        let dir = paths.diagnostics_dir.join(name);
        fs::create_dir_all(dir.join("files").join("logs")).unwrap();
        fs::write(dir.join("files").join("logs").join("old.log"), "old log").unwrap();
        let document = crate::DiagnosticsDocument { schema_version: 1, bundle: nexus_protocol::DiagnosticsBundle {
            id: name.into(), directory: dir.to_string_lossy().into_owned(), created_at_unix: now() - age * 86400, note: None,
            files: vec![nexus_protocol::DiagnosticsFile { name: "logs/old.log".into(), bytes: 7, redacted: false, truncated: false }],
        }};
        write_json_atomic(&dir, &dir.join("diagnostics.json"), &document).unwrap();
        fs::write(dir.join("export.json"), "{}").unwrap(); old(&dir, age); dir
    }
    #[test]
    fn cleanup_removes_only_confirmed_objects_and_persists_results() {
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let unknown = paths.downloads_dir.join("orphan"); fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("user-file"), "keep").unwrap();
        let backup = paths.diagnostics_dir.join("reset-backup-123"); fs::create_dir(&backup).unwrap();
        fs::write(backup.join("config.json"), "private-key").unwrap();
        let p = preview(&store);
        assert!(!p.items.iter().any(|v| v.path == unknown || v.path == backup));
        let id = selected(&p, "unused");
        assert!(store.cleanup("wrong", &[id.clone()], &[]).is_err()); assert!(slot.exists());
        let result = store.cleanup(&p.preview_id, &[id.clone()], &[]).unwrap().result.unwrap();
        assert_eq!(result.items[0].state, "removed"); assert!(!slot.exists());
        assert!(unknown.join("user-file").exists()); assert!(backup.join("config.json").exists());
        assert_eq!(MaintenanceStore::new(paths.clone()).status().unwrap().result.unwrap().state, "completed");
        assert!(store.cleanup(&p.preview_id, &[id], &[]).is_err());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn equal_size_nested_changes_and_new_launch_references_invalidate_preview() {
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let p = preview(&store); let id = selected(&p, "unused");
        rewrite_preserving_mtime(&slot.join("payload"), b"modified");
        assert!(store.cleanup(&p.preview_id, &[id], &[]).is_err()); assert!(slot.exists());
        let p = preview(&store); let id = selected(&p, "unused");
        let config = crate::NexusConfigFile { external_harness: None, harness: Some(crate::HarnessLaunchSpec::new(slot.join("payload"))), ..Default::default() };
        write_json_atomic(&paths.root, &paths.config_file, &config).unwrap();
        assert!(store.cleanup(&p.preview_id, &[id], &[]).is_err());
        let p = preview(&store);
        assert!(!p.items.iter().find(|v| v.name == "unused").unwrap().eligible);
        fs::remove_dir_all(paths.root).unwrap();
    }

    fn rewrite_preserving_mtime(path: &Path, content: &[u8]) {
        let before = fs::metadata(path).unwrap();
        assert_eq!(before.len(), content.len() as u64);
        let modified = before.modified().unwrap();
        fs::write(path, content).unwrap();
        fs::OpenOptions::new().write(true).open(path).unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn ownership_retries_preserve_equal_size_equal_mtime_content_changes() {
        let (paths, _) = fixture();
        let slot = free_release(&paths, "unused");
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(slot.join("manifest.json")).unwrap()).unwrap();
        let ownership = CleanupOwnership::begin(&paths, "release", "unused", manifest.clone()).unwrap();
        let record_path = CleanupOwnership::record_path(&paths, "release", "unused").unwrap();
        let original_record = fs::read(&record_path).unwrap();
        rewrite_preserving_mtime(&slot.join("payload"), b"modified");
        assert!(ownership.remaining().is_err());
        assert!(ownership.delete(&paths).is_err());
        assert!(CleanupOwnership::begin_bound(&paths, "release", "unused", manifest, None).is_err());
        assert_eq!(fs::read(slot.join("payload")).unwrap(), b"modified");
        assert_eq!(fs::read(record_path).unwrap(), original_record);
        fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn legacy_ownership_without_content_hash_is_preserved_without_adoption() {
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(slot.join("manifest.json")).unwrap()).unwrap();
        CleanupOwnership::begin(&paths, "release", "unused", manifest.clone()).unwrap();
        let record_path = CleanupOwnership::record_path(&paths, "release", "unused").unwrap();
        let mut legacy: serde_json::Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        for entry in legacy["entries"].as_array_mut().unwrap() { entry.as_object_mut().unwrap().remove("content_sha256"); }
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        crate::write_private_bytes_atomic(&paths.run_dir, &record_path, &legacy_bytes).unwrap();
        assert!(CleanupOwnership::load(&paths, "release", "unused").is_err());
        assert!(CleanupOwnership::begin_bound(&paths, "release", "unused", manifest, None).is_err());
        let legacy_record: CleanupOwnership = serde_json::from_slice(&legacy_bytes).unwrap();
        assert!(legacy_record.delete(&paths).is_err());
        free_release(&paths, "healthy-unused");
        let status = preview(&store);
        let preserved = status.items.iter().find(|item| item.name == "unused").unwrap();
        assert!(!preserved.eligible);
        assert!(preserved.reason.as_deref().unwrap().contains("lacks verified file contents"));
        let healthy = selected(&status, "healthy-unused");
        store.cleanup(&status.preview_id, &[healthy], &[]).unwrap();
        assert!(!paths.releases_dir.join("healthy-unused").exists());
        assert_eq!(fs::read(slot.join("payload")).unwrap(), b"original");
        assert_eq!(fs::read(record_path).unwrap(), legacy_bytes);
        fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn content_inspection_byte_and_time_budgets_fail_closed() {
        let (paths, _) = fixture();
        let slot = free_release(&paths, "unused");
        let mut budget = ContentScanBudget { deadline: Instant::now() + Duration::from_secs(1), remaining: 1 };
        assert!(entry_identity(&slot.join("payload"), "payload".into(), &mut budget).is_err());
        let mut budget = ContentScanBudget { deadline: Instant::now() - Duration::from_secs(1), remaining: MAX_CONTENT_SCAN_BYTES };
        assert!(entry_identity(&slot.join("payload"), "payload".into(), &mut budget).is_err());
        assert_eq!(fs::read(slot.join("payload")).unwrap(), b"original");
        fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn quarantine_and_credential_records_have_retained_previewed_cleanup() {
        let (paths,store)=fixture();
        let quarantine=paths.run_dir.join(format!("terminal-leases-corrupt-{}",crate::agent_auth::random_hex().unwrap()));
        fs::create_dir(&quarantine).unwrap();fs::write(quarantine.join("evidence"),b"damaged").unwrap();old(&quarantine,60);
        let recent=paths.run_dir.join("terminal-leases-corrupt");fs::create_dir(&recent).unwrap();
        let backup=paths.root.join("original-home");fs::create_dir(&backup).unwrap();fs::write(backup.join(".env"),b"keep").unwrap();
        let mut records=Vec::new();
        for index in 0..6 {
            let id=format!("cold-fixture-{index}");let record=paths.run_dir.join(format!("credential-recovery-{id}.json"));
            crate::write_private_json_atomic(&paths.root,&record,&serde_json::json!({"schema_version":1,"operation_id":id,
                "created_at_unix":now()-60*86400,"previous_home":backup,"imported_home":paths.root.join("imported"),"files":[".env"]})).unwrap();
            old(&record,60+index);records.push(record);
        }
        write_json_atomic(&paths.root,&paths.root.join("cold-operation.json"),&serde_json::json!({"credential_recovery_path":records[4]})).unwrap();
        let p=preview(&store);
        for path in records.iter().take(3).chain([&records[4],&recent]) {assert!(!p.items.iter().find(|v|v.path==*path).unwrap().eligible);}
        let ids=vec![selected(&p,quarantine.file_name().unwrap().to_str().unwrap()),selected(&p,records[3].file_name().unwrap().to_str().unwrap())];
        let result=store.cleanup(&p.preview_id,&ids,&[]).unwrap().result.unwrap();
        assert!(result.items.iter().all(|v|v.state=="removed"));
        assert!(!quarantine.exists());assert!(!records[3].exists());
        assert_eq!(fs::read(backup.join(".env")).unwrap(),b"keep");
        assert!(records[4].exists());assert!(recent.exists());
        let orphan_name=format!("terminal-leases-corrupt-{}",crate::agent_auth::random_hex().unwrap());
        let orphan=paths.run_dir.join(&orphan_name);fs::create_dir(&orphan).unwrap();old(&orphan,60);
        let owned=CleanupOwnership::begin(&paths,"terminal_quarantine",&orphan_name,serde_json::json!({"schema_version":1})).unwrap();
        assert!(owned.delete_inner(&paths,2).is_err());assert!(!orphan.exists());
        let p=preview(&store);let id=selected(&p,&orphan_name);
        assert_eq!(store.cleanup(&p.preview_id,&[id],&[]).unwrap().result.unwrap().items[0].state,"removed");
        assert!(CleanupOwnership::load(&paths,"terminal_quarantine",&orphan_name).unwrap().is_none());
        // Adding new evidence after preview invalidates the selected directory.
        old(&recent,60);let p=preview(&store);let id=selected(&p,"terminal-leases-corrupt");
        fs::write(recent.join("new-evidence"),b"preserve").unwrap();
        assert!(store.cleanup(&p.preview_id,&[id],&[]).is_err());assert!(recent.join("new-evidence").exists());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn replaced_quarantine_files_are_protected_with_and_without_ownership() {
        let (paths,store)=fixture();
        let name="terminal-leases-corrupt";
        let target=paths.run_dir.join(name);
        fs::write(&target,b"unowned file").unwrap();old(&target,60);
        assert!(!preview(&store).items.iter().find(|item|item.name==name).unwrap().eligible);
        fs::remove_file(&target).unwrap();fs::create_dir(&target).unwrap();old(&target,60);
        let before=preview(&store);let id=selected(&before,name);
        let owner=CleanupOwnership::begin(&paths,"terminal_quarantine",name,serde_json::json!({})).unwrap();
        fs::remove_dir(&target).unwrap();fs::write(&target,b"replacement must stay").unwrap();old(&target,60);
        assert!(store.cleanup(&before.preview_id,&[id],&[]).is_err());
        let after=store.inspect(30,&[]).unwrap();
        let index=after.preview.items.iter().position(|item|item.name==name).unwrap();
        assert!(!after.preview.items[index].eligible);
        assert!(after.fingerprints[index].is_none());
        assert!(owner.delete(&paths).is_err());
        assert_eq!(fs::read(&target).unwrap(),b"replacement must stay");
        assert!(CleanupOwnership::load(&paths,"terminal_quarantine",name).unwrap().is_some());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn quarantine_ownership_cannot_authorize_a_replacement_junction() {
        let (paths,store)=fixture();
        let name="terminal-leases-corrupt";let target=paths.run_dir.join(name);
        let outside=paths.root.join("unrelated-evidence");fs::create_dir(&outside).unwrap();fs::write(outside.join("keep"),b"keep").unwrap();
        fs::create_dir(&target).unwrap();old(&target,60);
        let before=preview(&store);let id=selected(&before,name);
        let owner=CleanupOwnership::begin(&paths,"terminal_quarantine",name,serde_json::json!({})).unwrap();
        fs::remove_dir(&target).unwrap();ReleaseStore::create_dir_junction(&target,&outside).unwrap();
        assert!(!preview(&store).items.iter().find(|item|item.name==name).unwrap().eligible);
        assert!(store.cleanup(&before.preview_id,&[id],&[]).is_err());
        assert!(owner.delete(&paths).is_err());
        assert_eq!(fs::read(outside.join("keep")).unwrap(),b"keep");
        fs::remove_dir(&target).unwrap();fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn protected_release_capacity_does_not_generate_a_deletion_fingerprint() {
        let (paths, store) = fixture();
        let current = free_release(&paths, "current");
        let unused = free_release(&paths, "unused");
        ReleaseStore::new(paths.clone()).promote("current").unwrap();
        let record = store.inspect(30, &[]).unwrap();
        let index = record.preview.items.iter().position(|item| item.path == current).unwrap();
        assert!(!record.preview.items[index].eligible);
        let expected: u64 = fs::read_dir(&current).unwrap().map(|entry| entry.unwrap().metadata().unwrap().len()).sum();
        assert_eq!(record.preview.items[index].bytes, Some(expected));
        assert!(record.fingerprints[index].is_none());
        let index = record.preview.items.iter().position(|item| item.path == unused).unwrap();
        assert!(record.preview.items[index].eligible);
        assert!(record.fingerprints[index].is_some(), "Deletion candidates still require content evidence");
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn release_template_protection_matches_preview_and_actual_cleanup() {
        let (paths, store) = fixture();
        let current = free_release(&paths, "current");
        let unused = free_release(&paths, "unused");
        let releases = ReleaseStore::new(paths.clone());
        releases.promote("current").unwrap();
        let mut harness = crate::HarnessLaunchSpec::new(paths.root.join("node.exe"));
        harness.working_dir = Some(PathBuf::from("{release_root}"));
        harness.args = vec!["{release_root}/apps/cli/lib/bin.js".into(), "{profile}".into()];
        let mut config = crate::NexusConfigFile { external_harness: None, harness: Some(harness), ..Default::default() };
        write_json_atomic(&paths.root, &paths.config_file, &config).unwrap();
        let p = preview(&store);
        assert!(!p.items.iter().find(|v| v.name == "current").unwrap().eligible);
        let id = selected(&p, "unused");
        assert!(crate::ensure_configuration_paths_preserved(&paths, &unused).is_ok());
        // Node arguments and runtime references must protect the same slot in
        // both preview and the independent final deletion check.
        config.harness.as_mut().unwrap().args.push(unused.join("payload").to_string_lossy().into_owned());
        write_json_atomic(&paths.root, &paths.config_file, &config).unwrap();
        assert!(!preview(&store).items.iter().find(|v| v.name == "unused").unwrap().eligible);
        assert!(releases.remove("unused").is_err());
        assert!(store.cleanup(&p.preview_id, &[id], &[]).is_err());
        config.harness.as_mut().unwrap().args.pop();
        config.harness.as_mut().unwrap().working_dir = Some(PathBuf::from("unknown-relative-cwd"));
        write_json_atomic(&paths.root, &paths.config_file, &config).unwrap();
        assert!(!preview(&store).items.iter().find(|v| v.name == "unused").unwrap().eligible);
        assert!(releases.remove("unused").is_err());
        config.harness.as_mut().unwrap().working_dir = Some(PathBuf::from("{release_root}"));
        write_json_atomic(&paths.root, &paths.config_file, &config).unwrap();
        let p = preview(&store); let id = selected(&p, "unused");
        let result = store.cleanup(&p.preview_id, &[id], &[]).unwrap().result.unwrap();
        assert_eq!(result.items[0].state, "removed");
        assert!(!unused.exists()); assert!(current.join("payload").exists());
        fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn cleanup_environment_reference_child() {
        let Some(root) = std::env::var_os("NEXUS_TEST_CLEANUP_REFERENCE_ROOT") else { return; };
        let paths = NexusPaths::from_root(PathBuf::from(root));
        let store = MaintenanceStore { default_home: Some(paths.root.join("test-user-home/.dsh")), paths: paths.clone() };
        assert!(!preview(&store).items.iter().find(|v| v.name == "unused").unwrap().eligible);
        assert!(ReleaseStore::new(paths.clone()).remove("unused").is_err());
        assert!(paths.releases_dir.join("unused/payload").exists());
    }

    #[test]
    fn cleanup_environment_node_reference_is_protected_by_both_entrypoints() {
        let (paths, _store) = fixture();
        let unused = free_release(&paths, "unused");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "maintenance::tests::cleanup_environment_reference_child", "--nocapture"])
            .env("NEXUS_TEST_CLEANUP_REFERENCE_ROOT", &paths.root)
            .env(crate::HARNESS_PROGRAM_ENV, paths.root.join("node.exe"))
            .env(crate::HARNESS_ARGS_ENV, serde_json::to_string(&vec![unused.join("payload").to_string_lossy().into_owned()]).unwrap())
            .output().unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn expired_and_forged_item_confirmations_never_delete() {
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let p = preview(&store);
        assert!(store.cleanup(&p.preview_id, &["../../unknown".into()], &[]).is_err());
        let mut record: PreviewRecord = bounded_record(&paths.run_dir.join(PREVIEW_FILE)).unwrap().unwrap();
        record.preview.created_at_unix = now() - 901;
        write_json_atomic(&paths.run_dir, &paths.run_dir.join(PREVIEW_FILE), &record).unwrap();
        assert!(store.cleanup(&p.preview_id, &[selected(&p, "unused")], &[]).is_err());
        assert!(slot.join("payload").exists());
        assert!(store.status().unwrap().result.is_none());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn retention_preserves_current_logs_and_removes_only_older_known_logs() {
        let (paths, store) = fixture();
        for i in 0..12 {
            let path = paths.logs_dir.join(format!("harness-run-{i:02}.stdout.log"));
            fs::write(&path, "log").unwrap(); old(&path, 40 + i);
        }
        let unknown = paths.logs_dir.join("user-notes.log"); fs::write(&unknown, "keep").unwrap(); old(&unknown, 100);
        let protected = "harness-run-11.stdout.log".to_owned();
        let p = store.preview(30, &[protected.clone()]).unwrap().preview.unwrap();
        assert!(!p.items.iter().find(|v| v.name == protected).unwrap().eligible);
        let id = selected(&p, "harness-run-10.stdout.log");
        store.cleanup(&p.preview_id, &[id], &[protected.clone()]).unwrap();
        assert!(paths.logs_dir.join(protected).exists()); assert!(unknown.exists());
        assert!(!paths.logs_dir.join("harness-run-10.stdout.log").exists());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn missing_sizes_are_unknown_and_interrupted_results_do_not_block_new_preview() {
        let (paths, store) = fixture();
        fs::remove_dir(&paths.downloads_dir).unwrap();
        let p = preview(&store);
        let area = p.areas.iter().find(|v| v.path == paths.downloads_dir).unwrap();
        assert_eq!(area.bytes, None); assert!(area.error.is_some());
        let result = CleanupResult { preview_id: p.preview_id, state: "running".into(), started_at_unix: now(),
            items: vec![CleanupItemResult { id: "item-0".into(), name: "unused".into(), state: "deleting".into(), error: None }] };
        write_json_atomic(&paths.run_dir, &paths.run_dir.join(RESULT_FILE), &result).unwrap();
        assert_eq!(store.status().unwrap().result.unwrap().state, "interrupted");
        preview(&store);
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn diagnostics_preserve_last_failure_and_unknown_contents() {
        let (paths, store) = fixture();
        for i in 0..6 {
            let id = format!("diag-{i}"); let dir = paths.diagnostics_dir.join(&id);
            fs::create_dir_all(dir.join("files")).unwrap();
            let document = crate::DiagnosticsDocument { schema_version: 1, bundle: nexus_protocol::DiagnosticsBundle {
                id, directory: dir.to_string_lossy().into_owned(), created_at_unix: now() - (40 + i) * 86400,
                note: (i == 5).then(|| "Automatic crash capture".into()), files: Vec::new(),
            }};
            write_json_atomic(&dir, &dir.join("diagnostics.json"), &document).unwrap();
            fs::write(dir.join("export.json"), "{}").unwrap();
            // Empty generated files directory is known even with no captures.
            fs::remove_dir(dir.join("files")).unwrap();
            if i == 4 { fs::create_dir(dir.join("unknown-user-directory")).unwrap(); }
            old(&dir, 40 + i);
        }
        let p = preview(&store);
        assert!(!p.items.iter().find(|v| v.name == "diag-5").unwrap().eligible);
        assert!(!p.items.iter().find(|v| v.name == "diag-4").unwrap().eligible);
        let id = selected(&p, "diag-3");
        store.cleanup(&p.preview_id, &[id], &[]).unwrap();
        assert!(!paths.diagnostics_dir.join("diag-3").exists());
        assert!(paths.diagnostics_dir.join("diag-4").exists()); assert!(paths.diagnostics_dir.join("diag-5").exists());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn final_release_directory_lock_keeps_external_ownership_until_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        free_release(&paths, "current");
        let releases = ReleaseStore::new(paths.clone()); releases.promote("current").unwrap();
        let locked = fs::OpenOptions::new().read(true).share_mode(3).custom_flags(0x02000000).open(&slot).unwrap();
        assert!(releases.remove("unused").is_err());
        assert!(slot.exists()); assert!(!slot.join("manifest.json").exists());
        let marker = CleanupOwnership::record_path(&paths, "release", "unused").unwrap(); assert!(marker.exists());
        assert!(releases.promote("unused").is_err()); assert!(releases.release_root("current").is_ok());
        free_release(&paths, "other"); releases.remove("other").unwrap(); assert!(marker.exists());
        drop(locked);
        let p = preview(&store);
        store.cleanup(&p.preview_id, &[selected(&p, "unused")], &[]).unwrap();
        assert!(!slot.exists()); assert!(!marker.exists()); fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn diagnostic_file_and_directory_locks_remain_retryable_without_adopting_new_files() {
        use std::os::windows::fs::OpenOptionsExt;
        for directory_lock in [false, true] {
            let (paths, store) = fixture();
            let target = diagnostic_fixture(&paths, "diag-old", 60);
            for index in 0..3 { diagnostic_fixture(&paths, &format!("diag-new-{index}"), 1); }
            let locked = if directory_lock {
                fs::OpenOptions::new().read(true).share_mode(3).custom_flags(0x02000000).open(&target).unwrap()
            } else { fs::OpenOptions::new().read(true).share_mode(1).open(target.join("files").join("logs").join("old.log")).unwrap() };
            let p = preview(&store);
            let result = store.cleanup(&p.preview_id, &[selected(&p, "diag-old")], &[]).unwrap().result.unwrap();
            assert_eq!(result.items[0].state, "failed");
            let marker = CleanupOwnership::record_path(&paths, "diagnostic", "diag-old").unwrap(); assert!(marker.exists());
            if directory_lock { assert!(!target.join("diagnostics.json").exists()); }
            fs::write(target.join("new-user-file"), "do not delete").unwrap();
            let p = preview(&store);
            assert!(!p.items.iter().find(|item| item.name == "diag-old").unwrap().eligible);
            assert!(target.join("new-user-file").exists());
            fs::remove_file(target.join("new-user-file")).unwrap(); drop(locked);
            let p = preview(&store);
            store.cleanup(&p.preview_id, &[selected(&p, "diag-old")], &[]).unwrap();
            assert!(!target.exists()); assert!(!marker.exists()); fs::remove_dir_all(paths.root).unwrap();
        }
    }
    #[test]
    fn final_directory_crash_boundaries_survive_new_previews_and_other_cleanup() {
        for kind in ["release", "diagnostic"] {
            for cut in [1, 2] {
                let (paths, store) = fixture();
                let name = if kind == "release" { "unused" } else { "diag-old" };
                let target = if kind == "release" { free_release(&paths, name) } else { diagnostic_fixture(&paths, name, 60) };
                let manifest_file = if kind == "release" { "manifest.json" } else { "diagnostics.json" };
                let manifest: serde_json::Value = bounded_record(&target.join(manifest_file)).unwrap().unwrap();
                let ownership = CleanupOwnership::begin(&paths, kind, name, manifest).unwrap();
                assert!(ownership.delete_inner(&paths, cut).is_err());
                let record_path = CleanupOwnership::record_path(&paths, kind, name).unwrap(); assert!(record_path.exists());
                assert_eq!(target.exists(), cut == 1);
                if kind == "release" { assert!(ReleaseStore::new(paths.clone()).register(name, "2", None, None).is_err()); }
                free_release(&paths, "other");
                let p = preview(&store);
                store.cleanup(&p.preview_id, &[selected(&p, "other")], &[]).unwrap();
                assert!(record_path.exists());
                let p = preview(&store);
                store.cleanup(&p.preview_id, &[selected(&p, name)], &[]).unwrap();
                assert!(!record_path.exists()); assert!(!target.exists());
                fs::remove_dir_all(paths.root).unwrap();
            }
        }
    }
    #[cfg(windows)]
    #[test]
    fn pnpm_style_junctions_and_symlinks_delete_only_links_with_unicode_spaces() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let outside = paths.root.join("外部 数据 目标"); fs::create_dir(&outside).unwrap();
        fs::write(outside.join("保留 文件.txt"), "external user data").unwrap();
        let modules = slot.join("node_modules"); fs::create_dir(&modules).unwrap();
        ReleaseStore::create_dir_junction(&modules.join("中文 junction"), &outside).unwrap();
        symlink_dir(&outside, modules.join("中文 symlink")).unwrap();
        symlink_file(outside.join("保留 文件.txt"), modules.join("file link")).unwrap();
        let tree = fingerprint(&slot).unwrap();
        assert_eq!(tree.iter().filter(|e| e.link_target.is_some()).count(), 3);
        assert!(!tree.iter().any(|e| e.relative.ends_with("保留 文件.txt")));
        let digest = fingerprint_digest(&tree).unwrap();
        let mut reverse = tree.clone(); reverse.reverse(); assert_eq!(fingerprint_digest(&reverse).unwrap(), digest);
        let p = preview(&store); let id = selected(&p, "unused");
        store.cleanup(&p.preview_id, &[id], &[]).unwrap();
        assert_eq!(fs::read_to_string(outside.join("保留 文件.txt")).unwrap(), "external user data");
        assert!(!slot.exists()); fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn ownership_cannot_adopt_files_appearing_after_preview_digest_validation() {
        let (paths, _) = fixture(); let slot = free_release(&paths, "unused");
        let observed = fingerprint(&slot).unwrap(); let digest = fingerprint_digest(&observed).unwrap();
        let manifest: serde_json::Value = bounded_record(&slot.join("manifest.json")).unwrap().unwrap();
        fs::write(slot.join("new-user-file"), "preserve").unwrap();
        assert!(CleanupOwnership::begin_bound(&paths, "release", "unused", manifest, Some(&digest)).is_err());
        assert!(!CleanupOwnership::record_path(&paths, "release", "unused").unwrap().exists());
        assert!(slot.join("new-user-file").exists()); fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    #[ignore = "Explicit read-only acceptance against an existing installed slot; never deletes or writes it"]
    fn readonly_real_installed_slot_fingerprint() {
        let path = PathBuf::from(std::env::var_os("NEXUS_TEST_READONLY_SLOT").expect("Explicit read-only slot path"));
        let started = Instant::now(); let entries = fingerprint(&path).unwrap();
        let digest = fingerprint_digest(&entries).unwrap();
        let serialized_bytes = serde_json::to_vec(&entries).unwrap().len();
        assert!(entries.len() > 80_000 && entries.len() < MAX_ENTRIES);
        assert!(serialized_bytes as u64 + 4096 < MAX_OWNERSHIP_BYTES);
        assert!(entries.iter().filter(|e| e.link_target.is_some()).count() > 1000);
        println!("readonly entries={} links={} ownership_entry_bytes={} elapsed_ms={} digest={}", entries.len(),
            entries.iter().filter(|e| e.link_target.is_some()).count(), serialized_bytes, started.elapsed().as_millis(), digest);
    }
    #[cfg(windows)]
    #[test]
    fn cleanup_failure_is_durable_and_next_preview_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let (paths, store) = fixture();
        let slot = free_release(&paths, "unused");
        let current = free_release(&paths, "current");
        let releases = ReleaseStore::new(paths.clone()); releases.promote("current").unwrap();
        let locked = fs::OpenOptions::new().read(true).share_mode(1).open(slot.join("payload")).unwrap();
        let p = preview(&store); let id = selected(&p, "unused");
        let result = store.cleanup(&p.preview_id, &[id], &[]).unwrap().result.unwrap();
        assert_eq!(result.items[0].state, "failed"); assert!(result.items[0].error.is_some());
        assert!(slot.join("payload").exists());
        assert!(releases.load().unwrap().find("unused").is_none());
        assert!(releases.promote("unused").is_err());
        let current_id = releases.load().unwrap().current_release.unwrap();
        assert_eq!(fs::canonicalize(releases.release_root(&current_id).unwrap()).unwrap(), fs::canonicalize(&current).unwrap());
        assert_eq!(store.status().unwrap().result.unwrap().items[0].state, "failed");
        drop(locked);
        let p = preview(&store);
        let id = selected(&p, "unused");
        store.cleanup(&p.preview_id, &[id], &[]).unwrap();
        assert!(!slot.exists());
        fs::remove_dir_all(paths.root).unwrap();
    }
}
