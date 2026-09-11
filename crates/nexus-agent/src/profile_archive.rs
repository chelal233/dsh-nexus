//! Reversible profile removal. Rename the complete directory; never walk or
//! delete its contents (which may contain shared dependency junctions).
use nexus_core::{NexusPaths, ProfileStore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema_version: u32,
    id: String,
    data_root_id: String,
    home: PathBuf,
    profile: String,
    created_at_unix: u64,
    phase: String,
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn same_profile(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}
fn validate_archivable_name(name: &str) -> io::Result<()> {
    nexus_core::validate_profile_name(name)?;
    if name.eq_ignore_ascii_case("node_modules") || name.starts_with('.') {
        return Err(invalid(
            "Shared dependencies and internal directories cannot be deleted",
        ));
    }
    Ok(())
}
fn ordinary(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || nexus_core::path_is_reparse(&metadata) {
            return Err(invalid(
                "Profile archive paths must be ordinary directories",
            ));
        }
    }
    Ok(())
}
fn overlaps(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    let (a, b) = (normalize(a), normalize(b));
    a == b || a.starts_with(&(b.clone() + "/")) || b.starts_with(&(a + "/"))
}
fn root(paths: &NexusPaths, home: &Path) -> io::Result<PathBuf> {
    ordinary(home)?;
    let home = fs::canonicalize(home)?;
    let namespace = nexus_core::data_root_identity(paths)?
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let archive = home.join(".nexus-deleted-profiles").join(namespace);
    for ancestor in archive.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !nexus_core::path_is_reparse(&metadata) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Ok(_) => {
                return Err(invalid(&format!(
                    "Deleted profile directory cannot be linked: {}",
                    ancestor.display()
                )))
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("Cannot inspect {}: {error}", ancestor.display()),
                ))
            }
        }
    }
    let config = nexus_core::ConfigStore::new(paths.clone()).load()?;
    if let Some(external) = config.external_harness {
        let external = fs::canonicalize(external.root)?;
        if overlaps(&archive, &external) || overlaps(&home.join("profiles"), &external) {
            return Err(invalid("External Harness program files cannot be moved"));
        }
    }
    if archive.try_exists()? {
        ordinary(&archive)?;
    }
    Ok(archive)
}
fn read(paths: &NexusPaths, home: &Path, id: &str) -> io::Result<(PathBuf, Record)> {
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err(invalid("Invalid deleted profile ID"));
    }
    let directory = root(paths, home)?.join(id);
    ordinary(&directory)?;
    let bytes = nexus_core::read_regular_file_bounded(&directory.join("record.json"), 65536)?
        .ok_or_else(|| invalid("Deleted profile record is missing"))?;
    let record: Record = serde_json::from_slice(&bytes)?;
    if record.schema_version != 1
        || record.id != id
        || record.data_root_id != nexus_core::data_root_identity(paths)?
        || !super::dsh::same_native_path(&record.home, &fs::canonicalize(home)?)
    {
        return Err(invalid(
            "Deleted profile belongs to another home or Nexus data directory",
        ));
    }
    validate_archivable_name(&record.profile)?;
    if !matches!(
        record.phase.as_str(),
        "archiving" | "archived" | "restoring" | "restored" | "rolled_back"
    ) {
        return Err(invalid("Unsupported deleted profile phase"));
    }
    Ok((directory, record))
}
fn save(directory: &Path, record: &Record) -> io::Result<()> {
    nexus_core::write_private_json_atomic(directory, &directory.join("record.json"), record)
}
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn unstarted(directory: &Path) -> io::Result<bool> {
    ordinary(directory)?;
    Ok(!exists(&directory.join("record.json"))? && !exists(&directory.join("profile"))?)
}
// Only discard our completed journal, never a directory containing profile data.
fn discard_finished(directory: &Path) -> io::Result<()> {
    ordinary(directory)?;
    if exists(&directory.join("profile"))? {
        return Err(invalid("Archived files must be preserved"));
    }
    fs::remove_file(directory.join("record.json"))?;
    fs::remove_dir(directory)
}
pub(crate) fn recover_if_present(paths: &NexusPaths, store: &ProfileStore) -> io::Result<()> {
    let home = super::dsh::resolve_dsh_home_for_paths(paths)?;
    if !exists(&home.join(".nexus-deleted-profiles"))? {
        return Ok(());
    }
    let parent = root(paths, &home)?;
    if !exists(&parent)? {
        return Ok(());
    }
    ordinary(&parent)?;
    for (index, entry) in fs::read_dir(parent)?.take(257).enumerate() {
        if index == 256 {
            return Err(invalid("Too many deleted profiles to reconcile"));
        }
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if unstarted(&entry.path())? {
            continue;
        }
        let (directory, mut record) = read(paths, &home, &id)?;
        if matches!(record.phase.as_str(), "restored" | "rolled_back") {
            discard_finished(&directory)?;
            continue;
        }
        if record.phase == "archived" {
            continue;
        }
        let original = home.join("profiles").join(&record.profile);
        let payload = directory.join("profile");
        let (at_original, at_archive) = (exists(&original)?, exists(&payload)?);
        if at_original == at_archive {
            return Err(invalid(
                "Deleted profile transaction is ambiguous; both locations are preserved",
            ));
        }
        ordinary(if at_archive { &payload } else { &original })?;
        let mut catalog = store.load()?;
        if at_archive {
            if same_profile(&catalog.active_profile, &record.profile) {
                return Err(invalid(
                    "Pending deletion conflicts with the current profile",
                ));
            }
            catalog
                .profiles
                .retain(|name| !same_profile(name, &record.profile));
            record.phase = "archived".into();
        } else {
            if !catalog
                .profiles
                .iter()
                .any(|name| same_profile(name, &record.profile))
            {
                catalog.profiles.push(record.profile.clone());
            }
            record.phase = "restored".into();
        }
        store.write(&catalog)?;
        save(&directory, &record)?;
        if !at_archive {
            discard_finished(&directory)?;
        }
    }
    Ok(())
}
pub(crate) fn list(paths: &NexusPaths, home: &Path) -> io::Result<Value> {
    let parent = root(paths, home)?;
    if !exists(&parent)? {
        return Ok(json!({"deleted":[]}));
    }
    ordinary(&parent)?;
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    for (index, entry) in fs::read_dir(&parent)?.take(257).enumerate() {
        if index == 256 {
            warnings.push("Deleted profile listing is incomplete".to_owned());
            break;
        }
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if unstarted(&entry.path()).unwrap_or(false) {
            continue;
        }
        match read(paths, home, &id) {
            Ok((directory, record)) => {
                if exists(&directory.join("profile"))? {
                    ordinary(&directory.join("profile"))?;
                    let occupied = exists(&home.join("profiles").join(&record.profile))?;
                    items.push(json!({"id":id,"profile":record.profile,"created_at_unix":record.created_at_unix,"can_restore":!occupied,"phase":record.phase}));
                }
            }
            Err(error) => warnings.push(error.to_string()),
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["created_at_unix"].as_u64()));
    Ok(json!({"deleted":items,"warnings":warnings}))
}
fn ensure_unreferenced(home: &Path, name: &str) -> io::Result<()> {
    // Inspect every direct edge, not just the final logical source, so an
    // intermediate generated profile is protected as well.
    let profiles = home.join("profiles");
    ordinary(&profiles)?;
    for (index, entry) in fs::read_dir(profiles)?.take(1025).enumerate() {
        if index == 1024 {
            return Err(invalid("Too many profiles to verify deletion dependencies"));
        }
        let entry = entry?;
        let other = entry.file_name().to_string_lossy().into_owned();
        if same_profile(&other, name)
            || other.eq_ignore_ascii_case("node_modules")
            || other.starts_with('.')
        {
            continue;
        }
        if nexus_core::validate_profile_name(&other).is_err() {
            continue;
        }
        if !fs::symlink_metadata(entry.path())?.is_dir() {
            continue;
        }
        ordinary(&entry.path())?;
        if let Some(bytes) = nexus_core::read_regular_file_bounded(
            &entry.path().join(".nexus-compatibility.json"),
            65536,
        )? {
            let value: Value = serde_json::from_slice(&bytes)?;
            let source = value["source_profile"]
                .as_str()
                .ok_or_else(|| invalid("Compatibility source is missing"))?;
            if same_profile(source, name) {
                return Err(invalid(&format!(
                    "Profile is used by {other}; remove that generated profile first"
                )));
            }
        }
    }
    Ok(())
}
pub(crate) fn change(
    paths: &NexusPaths,
    store: &ProfileStore,
    home: &Path,
    name_or_id: &str,
    restore: bool,
) -> io::Result<Value> {
    nexus_core::terminal_lease::ensure_all_idle(paths)?;
    let parent = root(paths, home)?;
    let home = fs::canonicalize(home)?;
    ordinary(&home.join("profiles"))?;
    let mut catalog = store.load()?;
    let (directory, mut record, source, target) = if restore {
        let (directory, record) = read(paths, &home, name_or_id)?;
        let source = directory.join("profile");
        ordinary(&source)?;
        let target = home.join("profiles").join(&record.profile);
        if exists(&target)? {
            return Err(invalid(
                "A profile with this name already exists; nothing was overwritten",
            ));
        }
        (directory, record, source, target)
    } else {
        validate_archivable_name(name_or_id)?;
        // Reserve room before publishing another journal. Existing archives
        // must always remain within the recovery/listing budget.
        if exists(&parent)? && fs::read_dir(&parent)?.take(256).count() >= 256 {
            return Err(invalid("Deleted profile storage is full; restore an existing profile before deleting another"));
        }
        if same_profile(&catalog.active_profile, name_or_id) {
            return Err(invalid("The current profile cannot be deleted"));
        }
        ensure_unreferenced(&home, name_or_id)?;
        let source = home.join("profiles").join(name_or_id);
        ordinary(&source)?;
        fs::create_dir_all(&parent)?;
        ordinary(&parent)?;
        let directory = parent.join(nexus_core::new_instance_id());
        fs::create_dir(&directory)?;
        let id = directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let record = Record {
            schema_version: 1,
            id,
            data_root_id: nexus_core::data_root_identity(paths)?,
            home: home.clone(),
            profile: name_or_id.into(),
            created_at_unix: nexus_core::unix_time_seconds(),
            phase: "archiving".into(),
        };
        let target = directory.join("profile");
        (directory, record, source, target)
    };
    record.phase = if restore { "restoring" } else { "archiving" }.into();
    save(&directory, &record)?;
    // Both paths are validated descendants of this explicit Harness home;
    // same-volume rename never traverses internal links or removes contents.
    fs::rename(&source, &target)?;
    if restore {
        if !catalog
            .profiles
            .iter()
            .any(|name| same_profile(name, &record.profile))
        {
            catalog.profiles.push(record.profile.clone());
        }
    } else {
        catalog
            .profiles
            .retain(|name| !same_profile(name, &record.profile));
    }
    if let Err(error) = store.write(&catalog) {
        if let Err(rollback) = fs::rename(&target, &source) {
            return Err(invalid(&format!(
                "Index update failed: {error}; profile remains recoverable at {}: {rollback}",
                target.display()
            )));
        }
        record.phase = if restore { "archived" } else { "rolled_back" }.into();
        if save(&directory, &record).is_ok() && !restore {
            let _ = discard_finished(&directory);
        }
        return Err(error);
    }
    record.phase = if restore { "restored" } else { "archived" }.into();
    let warning = save(&directory, &record)
        .and_then(|()| {
            if restore {
                discard_finished(&directory)
            } else {
                Ok(())
            }
        })
        .err()
        .map(|e| e.to_string());
    Ok(
        json!({"profile":record.profile,"archive_id":record.id,"restored":restore,"warning":warning}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        paths: NexusPaths,
        home: PathBuf,
        store: ProfileStore,
    }
    impl Fixture {
        fn new() -> Self {
            let paths = NexusPaths::from_root(std::env::temp_dir().join(format!(
                "nexus-profile-archive-{}",
                nexus_core::agent_auth::random_hex().unwrap()
            )));
            paths.ensure_directories().unwrap();
            let home = paths.root.join("harness-data");
            for name in ["web", "old", "node_modules"] {
                fs::create_dir_all(home.join("profiles").join(name)).unwrap();
            }
            fs::write(home.join("profiles/old/session.bin"), b"session\0bytes").unwrap();
            fs::write(home.join("profiles/node_modules/shared.bin"), b"shared").unwrap();
            nexus_core::ConfigStore::new(paths.clone())
                .transaction(|doc| {
                    doc.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
                        home: Some(home.to_string_lossy().into_owned()),
                        ..Default::default()
                    });
                    Ok(())
                })
                .unwrap();
            let store = ProfileStore::new(paths.clone());
            let mut catalog = store.load().unwrap();
            catalog.active_profile = "web".into();
            catalog.profiles = vec!["web".into(), "old".into()];
            store.write(&catalog).unwrap();
            Self { paths, home, store }
        }
        fn change(&self, name: &str, restore: bool) -> io::Result<Value> {
            change(&self.paths, &self.store, &self.home, name, restore)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.paths.root);
        }
    }
    #[test]
    fn round_trip_preserves_full_profile_and_shared_files() {
        let f = Fixture::new();
        let result = f.change("old", false).unwrap();
        let id = result["archive_id"].as_str().unwrap();
        assert!(!f.home.join("profiles/old").exists());
        assert_eq!(
            list(&f.paths, &f.home).unwrap()["deleted"][0]["profile"],
            "old"
        );
        assert!(!f.store.load().unwrap().profiles.contains(&"old".into()));
        f.change(id, true).unwrap();
        assert_eq!(
            fs::read(f.home.join("profiles/old/session.bin")).unwrap(),
            b"session\0bytes"
        );
        assert_eq!(
            fs::read(f.home.join("profiles/node_modules/shared.bin")).unwrap(),
            b"shared"
        );
        assert_eq!(f.store.load().unwrap().active_profile, "web");
        assert!(list(&f.paths, &f.home).unwrap()["deleted"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn current_shared_internal_and_referenced_profiles_are_protected() {
        let f = Fixture::new();
        for name in [
            "web",
            "node_modules",
            "NODE_MODULES",
            ".internal",
            "../outside",
        ] {
            assert!(f.change(name, false).is_err(), "{name}");
        }
        let generated = f.home.join("profiles/generated");
        fs::create_dir(&generated).unwrap();
        fs::write(
            generated.join(".nexus-compatibility.json"),
            br#"{"source_profile":"old"}"#,
        )
        .unwrap();
        assert!(f
            .change("old", false)
            .unwrap_err()
            .to_string()
            .contains("used by generated"));
        #[cfg(windows)]
        {
            assert!(f
                .change("WEB", false)
                .unwrap_err()
                .to_string()
                .contains("current profile"));
            assert!(f
                .change("OLD", false)
                .unwrap_err()
                .to_string()
                .contains("used by generated"));
        }
        assert_eq!(
            fs::read(f.home.join("profiles/old/session.bin")).unwrap(),
            b"session\0bytes"
        );
    }
    #[test]
    fn restore_collision_keeps_both_copies() {
        let f = Fixture::new();
        let result = f.change("old", false).unwrap();
        let id = result["archive_id"].as_str().unwrap();
        fs::create_dir(f.home.join("profiles/old")).unwrap();
        fs::write(f.home.join("profiles/old/session.bin"), b"new").unwrap();
        assert!(f.change(id, true).is_err());
        assert_eq!(
            list(&f.paths, &f.home).unwrap()["deleted"][0]["can_restore"],
            false
        );
        assert_eq!(
            fs::read(f.home.join("profiles/old/session.bin")).unwrap(),
            b"new"
        );
        assert_eq!(
            fs::read(
                read(&f.paths, &f.home, id)
                    .unwrap()
                    .0
                    .join("profile/session.bin")
            )
            .unwrap(),
            b"session\0bytes"
        );
    }
    #[test]
    fn interrupted_rename_reconciles_catalog_without_moving_or_losing_bytes() {
        let f = Fixture::new();
        let result = f.change("old", false).unwrap();
        let id = result["archive_id"].as_str().unwrap();
        let (dir, mut record) = read(&f.paths, &f.home, id).unwrap();
        record.phase = "archiving".into();
        save(&dir, &record).unwrap();
        let mut catalog = f.store.load().unwrap();
        catalog.profiles.push("old".into());
        f.store.write(&catalog).unwrap();
        recover_if_present(&f.paths, &f.store).unwrap();
        assert!(!f.store.load().unwrap().profiles.contains(&"old".into()));
        record.phase = "restoring".into();
        save(&dir, &record).unwrap();
        fs::rename(dir.join("profile"), f.home.join("profiles/old")).unwrap();
        recover_if_present(&f.paths, &f.store).unwrap();
        assert!(f.store.load().unwrap().profiles.contains(&"old".into()));
        assert_eq!(
            fs::read(f.home.join("profiles/old/session.bin")).unwrap(),
            b"session\0bytes"
        );
        recover_if_present(&f.paths, &f.store).unwrap();
    }
    #[test]
    fn other_data_root_archive_does_not_block_startup() {
        let f = Fixture::new();
        fs::create_dir_all(f.home.join(".nexus-deleted-profiles/other-root")).unwrap();
        recover_if_present(&f.paths, &f.store).unwrap();
    }
    #[test]
    fn archive_capacity_never_blocks_restoring_existing_profiles() {
        let f=Fixture::new();let result=f.change("old",false).unwrap();
        let id=result["archive_id"].as_str().unwrap();
        let (original,record)=read(&f.paths,&f.home,id).unwrap();let parent=original.parent().unwrap();
        for index in 1..256 {
            let mut copy:Record=serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
            copy.id=format!("archive-{index}");copy.profile=format!("old-{index}");
            let directory=parent.join(&copy.id);fs::create_dir_all(directory.join("profile")).unwrap();save(&directory,&copy).unwrap();
        }
        fs::create_dir(f.home.join("profiles/another")).unwrap();
        assert!(f.change("another",false).unwrap_err().to_string().contains("storage is full"));
        recover_if_present(&f.paths,&f.store).unwrap();
        f.change(id,true).unwrap();
        assert_eq!(fs::read(f.home.join("profiles/old/session.bin")).unwrap(),b"session\0bytes");
        f.change("another",false).unwrap();
        assert_eq!(fs::read_dir(parent).unwrap().count(),256);
    }
    #[test]
    fn unstarted_archive_does_not_block_restore_and_finished_journals_do_not_accumulate() {
        let f = Fixture::new();
        let parent = root(&f.paths, &f.home).unwrap();
        fs::create_dir_all(parent.join("empty-before-record")).unwrap();
        recover_if_present(&f.paths, &f.store).unwrap();
        for _ in 0..3 {
            let result = f.change("old", false).unwrap();
            f.change(result["archive_id"].as_str().unwrap(), true)
                .unwrap();
        }
        assert_eq!(
            fs::read_dir(parent).unwrap().count(),
            1,
            "only the unstarted directory remains"
        );
    }
    #[cfg(windows)]
    #[test]
    fn live_terminal_blocks_profile_mutations() {
        let f = Fixture::new();
        nexus_core::terminal_lease::register(&f.paths, "test-release", std::process::id()).unwrap();
        assert_eq!(
            f.change("old", false).unwrap_err().kind(),
            io::ErrorKind::ResourceBusy
        );
        assert!(f.home.join("profiles/old/session.bin").exists());
    }
    #[cfg(windows)]
    #[test]
    fn failed_index_publication_rolls_back_directory_move() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        let _lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&f.paths.profiles_file)
            .unwrap();
        assert!(f.change("old", false).is_err());
        assert_eq!(
            fs::read(f.home.join("profiles/old/session.bin")).unwrap(),
            b"session\0bytes"
        );
        assert!(f.store.load().unwrap().profiles.contains(&"old".into()));
        assert!(list(&f.paths, &f.home).unwrap()["deleted"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[cfg(windows)]
    #[test]
    fn internal_dependency_junction_moves_without_traversing_shared_target() {
        use std::os::windows::process::CommandExt;
        let f = Fixture::new();
        let link = f.home.join("profiles").join("old").join("node_modules");
        let shared = f.home.join("profiles").join("node_modules");
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&shared)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = f.change("old", false).unwrap();
        let id = result["archive_id"].as_str().unwrap();
        assert_eq!(fs::read(shared.join("shared.bin")).unwrap(), b"shared");
        f.change(id, true).unwrap();
        assert!(nexus_core::path_is_reparse(
            &fs::symlink_metadata(&link).unwrap()
        ));
        assert_eq!(fs::read(link.join("shared.bin")).unwrap(), b"shared");
        fs::remove_dir(link).unwrap();
    }
}
