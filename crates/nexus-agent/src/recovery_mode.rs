//! Durable launch pause, independent of restorable Harness configuration.
use std::{fs, io};
use nexus_core::NexusPaths;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record { schema_version: u32, paused: bool }

fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    { use std::os::windows::fs::MetadataExt; metadata.file_attributes() & 0x400 != 0 }
    #[cfg(not(windows))]
    { metadata.file_type().is_symlink() }
}

// Only ordinary, readable and bounded records may be repaired automatically.
fn read_record(paths: &NexusPaths) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(&paths.run_dir) {
        Ok(metadata) if linked(&metadata) || !metadata.is_dir() => return Err(io::Error::other("Harness recovery directory must be ordinary")),
        Ok(_) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let path = paths.run_dir.join("harness-recovery.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if linked(&metadata) || !metadata.is_file() || metadata.len() > 4096 =>
            return Err(io::Error::other("Harness recovery record must be a small ordinary file")),
        Ok(_) => {},
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if linked(&metadata) || !metadata.is_file() { return Err(io::Error::other("Harness recovery record is linked or not a file")); }
    let mut bytes = Vec::new();
    use io::Read;
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 { return Err(io::Error::other("Harness recovery record exceeds its size limit")); }
    Ok(Some(bytes))
}

fn parse(bytes: &[u8]) -> io::Result<bool> {
    let record: Record = serde_json::from_slice(bytes)?;
    if record.schema_version != 1 { return Err(io::Error::other("unsupported Harness recovery record")); }
    Ok(record.paused)
}

pub(crate) fn paused(paths: &NexusPaths) -> io::Result<bool> {
    read_record(paths)?.map(|bytes| parse(&bytes)).unwrap_or(Ok(false))
}

pub(crate) fn set_paused(paths: &NexusPaths, paused: bool) -> io::Result<()> {
    if let Some(bytes) = read_record(paths)? {
        if let Err(error) = parse(&bytes) {
            if !paused { return Err(error); }
            // Keep the original in place until atomic replacement succeeds.
            // A failed backup or replacement must never turn corruption into NotFound.
            let backup = paths.run_dir.join(format!("harness-recovery.invalid-{}.json", nexus_core::unix_time_nanos_for_update()));
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(backup)?;
            use io::Write;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
    }
    nexus_core::write_json_atomic(&paths.run_dir, &paths.run_dir.join("harness-recovery.json"),
        &Record { schema_version: 1, paused })
}

pub(crate) fn ensure_start_allowed(paths: &NexusPaths) -> Result<(), crate::supervisor::HarnessSupervisorError> {
    if paused(paths).map_err(crate::supervisor::HarnessSupervisorError::Configuration)? {
        return Err(crate::supervisor::HarnessSupervisorError::RecoveryPaused);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_mode_rejects_linked_directory_without_touching_target() {
        let root = std::env::temp_dir().join(format!("nexus-pause-link-{}", nexus_core::unix_time_nanos_for_update()));
        let slot = root.join("slot");
        let package = slot.join("vendor").join("fixture");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), br#"{"name":"fixture"}"#).unwrap();
        fs::write(package.join("harness-recovery.json"), b"{").unwrap();
        let home = root.join("home");
        nexus_core::ReleaseStore::heal_module_farm(&home, &slot).unwrap();
        let link = home.join("profiles").join("node_modules").join("fixture");
        let mut paths = NexusPaths::from_root(root.join("data"));
        paths.run_dir = link.clone();
        assert!(paused(&paths).is_err());
        assert!(set_paused(&paths, true).is_err());
        assert_eq!(fs::read(package.join("harness-recovery.json")).unwrap(), b"{");
        #[cfg(windows)] fs::remove_dir(link).unwrap();
        #[cfg(not(windows))] fs::remove_file(link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_pause_persists_and_corruption_blocks_start() {
        let root = std::env::temp_dir().join(format!("nexus-pause-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        assert!(!paused(&paths).unwrap());
        set_paused(&paths, true).unwrap();
        assert!(matches!(ensure_start_allowed(&paths), Err(crate::supervisor::HarnessSupervisorError::RecoveryPaused)));
        set_paused(&paths, false).unwrap();
        assert!(ensure_start_allowed(&paths).is_ok());
        fs::write(paths.run_dir.join("harness-recovery.json"), b"{").unwrap();
        assert!(ensure_start_allowed(&paths).is_err());
        assert!(set_paused(&paths, false).is_err());
        set_paused(&paths, true).unwrap();
        assert!(paused(&paths).unwrap());
        let backup = fs::read_dir(&paths.run_dir).unwrap().map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("harness-recovery.invalid-")).unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"{");
        set_paused(&paths, false).unwrap();
        fs::write(paths.run_dir.join("harness-recovery.json"), br#"{"schema_version":99,"paused":false}"#).unwrap();
        assert!(set_paused(&paths, false).is_err());
        set_paused(&paths, true).unwrap();
        assert!(paused(&paths).unwrap());
        #[cfg(windows)] {
            use std::os::windows::fs::OpenOptionsExt;
            let path = paths.run_dir.join("harness-recovery.json");
            fs::write(&path, b"{").unwrap();
            let locked = fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
            assert!(set_paused(&paths, true).is_err());
            drop(locked);
            assert_eq!(fs::read(path).unwrap(), b"{");
        }
        fs::write(paths.run_dir.join("harness-recovery.json"), vec![b'x'; 4097]).unwrap();
        assert!(set_paused(&paths, true).is_err());
        assert_eq!(fs::metadata(paths.run_dir.join("harness-recovery.json")).unwrap().len(), 4097);
        fs::remove_file(paths.run_dir.join("harness-recovery.json")).unwrap();
        fs::create_dir(paths.run_dir.join("harness-recovery.json")).unwrap();
        assert!(set_paused(&paths, true).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
