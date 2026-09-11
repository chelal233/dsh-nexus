//! Live interactive terminals keep their selected release available across Agent restarts.
use std::{fs, io, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};
use crate::{NexusPaths, path_is_reparse};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease { schema_version: u32, release: String, pid: u32, creation: u64 }

#[cfg(windows)]
fn process_creation(pid: u32) -> io::Result<Option<u64>> {
    use windows_sys::Win32::{Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, GetProcessTimes, WaitForSingleObject}};
    let handle = unsafe { OpenProcess(0x00100000 | 0x1000, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) { Ok(None) } else { Err(error) };
    }
    struct Held(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Held { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }
    let held = Held(handle);
    if unsafe { WaitForSingleObject(held.0, 0) } == WAIT_OBJECT_0 { return Ok(None); }
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut birth, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe { GetProcessTimes(held.0, &mut birth, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some((birth.dwHighDateTime as u64) << 32 | birth.dwLowDateTime as u64))
}
#[cfg(not(windows))]
fn process_creation(_pid: u32) -> io::Result<Option<u64>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Interactive terminals require Windows"))
}
fn ordinary_dir(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || path_is_reparse(&m) { return Err(io::Error::other("Terminal lease directory must be ordinary")); }
    Ok(())
}
// Another reader may have removed or quarantined the same entry already.
fn present<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn quarantine(paths: &NexusPaths, path: &Path) -> io::Result<()> {
    let mut directory = paths.run_dir.join("terminal-leases-corrupt");
    match fs::create_dir(&directory) {
        Ok(()) => {},
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
        Err(error) => return Err(error),
    }
    if ordinary_dir(&directory).is_err() {
        // A foreign file or junction at the preferred location must neither
        // receive evidence nor block all terminal/release/profile operations.
        directory = paths.run_dir.join(format!("terminal-leases-corrupt-{}", crate::agent_auth::random_hex()?));
        nexus_private_file::create_new_private_directory(&directory)?;
    }
    // Rename the entry itself, including directories/junctions. Never read,
    // traverse or delete its children or the target of a reparse point.
    present(fs::rename(path, directory.join(crate::agent_auth::random_hex()?)))?;
    Ok(())
}
fn records(paths: &NexusPaths) -> io::Result<Vec<(PathBuf, Lease)>> {
    ordinary_dir(&paths.root)?;
    match ordinary_dir(&paths.run_dir) { Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]), result => result? }
    let directory = paths.run_dir.join("terminal-leases");
    match fs::symlink_metadata(&directory) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e), Ok(_) => ordinary_dir(&directory)?,
    }
    let mut result = vec![];
    let Some(entries) = present(fs::read_dir(&directory))? else { return Ok(result); };
    for entry in entries {
        let Some(entry) = present(entry)? else { continue; };
        let path = entry.path();
        let Some(metadata) = present(fs::symlink_metadata(&path))? else { continue; };
        if !metadata.is_file() || path_is_reparse(&metadata) {
            quarantine(paths, &path)?;
            continue;
        }
        let mut options = fs::OpenOptions::new(); options.read(true);
        #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
        let Some(file) = present(options.open(&path))? else { continue; };
        let metadata = file.metadata()?;
        if !metadata.is_file() || path_is_reparse(&metadata) {
            drop(file);
            quarantine(paths, &path)?;
            continue;
        }
        use std::io::Read;
        let parsed = serde_json::from_reader::<_, Lease>(file.take(4097));
        let lease = match parsed {
            Ok(lease) if lease.schema_version == 1 && crate::is_valid_release_id(&lease.release) => lease,
            _ => {
                quarantine(paths, &path)?;
                continue;
            }
        };
        if process_creation(lease.pid)? == Some(lease.creation) {
            // Bound memory, but finish cleaning stale entries in this pass.
            if result.len() <= 128 { result.push((path, lease)); }
        } else { present(fs::remove_file(path))?; }
    }
    if result.len() > 128 { return Err(io::Error::other("Terminal lease capacity reached")); }
    Ok(result)
}
pub fn ensure_release_idle(paths: &NexusPaths, release: &str) -> io::Result<()> {
    for (_, lease) in records(paths)? {
        if lease.release == release && process_creation(lease.pid)? == Some(lease.creation) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy,
                "Close the DSH terminal using this release before deleting it"));
        }
    }
    Ok(())
}
pub fn ensure_all_idle(paths: &NexusPaths) -> io::Result<()> {
    for (_, lease) in records(paths)? {
        if process_creation(lease.pid)? == Some(lease.creation) {
            return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Close DSH terminals before deleting or restoring a profile"));
        }
    }
    Ok(())
}
pub fn register(paths: &NexusPaths, release: &str, pid: u32) -> io::Result<PathBuf> {
    crate::validate_release_id(release)?;
    let existing = records(paths)?;
    let mut live = 0;
    for (path, lease) in existing {
        if process_creation(lease.pid)? == Some(lease.creation) { live += 1; }
        else { present(fs::remove_file(path))?; }
    }
    if live >= 64 { return Err(io::Error::other("Close an existing DSH terminal before opening another")); }
    let creation = process_creation(pid)?.ok_or_else(|| io::Error::other("Terminal exited before registration"))?;
    let directory = paths.run_dir.join("terminal-leases");
    fs::create_dir_all(&directory)?; ordinary_dir(&directory)?;
    let path = directory.join(format!("{}.json", crate::agent_auth::random_hex()?));
    crate::write_private_bytes_atomic(&paths.root, &path,
        &serde_json::to_vec(&Lease { schema_version: 1, release: release.into(), pid, creation })?)?;
    Ok(path)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn damaged_and_stale_records_self_heal_without_losing_live_protection() {
        let root = std::env::temp_dir().join(format!("nexus-terminal-heal-{}", crate::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let live = register(&paths, "slot-a", std::process::id()).unwrap();
        let directory = live.parent().unwrap();
        fs::write(directory.join("damaged.json"), b"{broken").unwrap();
        for i in 0..4100 {
            fs::write(directory.join(format!("stale-{i}.json")), serde_json::to_vec(&Lease {
                schema_version: 1, release: "slot-b".into(), pid: std::process::id(),
                creation: process_creation(std::process::id()).unwrap().unwrap() ^ 1,
            }).unwrap()).unwrap();
        }
        ensure_release_idle(&paths, "slot-b").unwrap();
        assert_eq!(ensure_all_idle(&paths).unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        assert_eq!(ensure_release_idle(&paths, "slot-a").unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
        let quarantined = fs::read_dir(paths.run_dir.join("terminal-leases-corrupt")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::read(quarantined).unwrap(), b"{broken");
        register(&paths, "slot-c", std::process::id()).unwrap();
        fs::remove_file(live).unwrap();
        ensure_release_idle(&paths, "slot-a").unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stray_directories_and_junctions_are_quarantined_without_following_targets() {
        let root = std::env::temp_dir().join(format!("nexus-lease-entries-{}", crate::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let live = register(&paths, "slot-a", std::process::id()).unwrap();
        let directory = live.parent().unwrap();
        let stray = directory.join("stray"); fs::create_dir(&stray).unwrap();
        fs::write(stray.join("preserved.txt"), b"stray data").unwrap();
        let target = root.join("unrelated-target"); fs::create_dir(&target).unwrap();
        fs::write(target.join("untouched.txt"), b"external data").unwrap();
        let junction = directory.join("junction");
        assert!(std::process::Command::new("cmd").args(["/C", "mklink", "/J"])
            .arg(&junction).arg(&target).output().unwrap().status.success());
        ensure_release_idle(&paths, "slot-b").unwrap();
        assert_eq!(ensure_all_idle(&paths).unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
        assert_eq!(fs::read(target.join("untouched.txt")).unwrap(), b"external data");
        let mut ordinary = 0; let mut links = 0;
        for entry in fs::read_dir(paths.run_dir.join("terminal-leases-corrupt")).unwrap() {
            let path = entry.unwrap().path();
            if path_is_reparse(&fs::symlink_metadata(&path).unwrap()) {
                links += 1;
                fs::remove_dir(&path).unwrap(); // Unlink only; never traverse the junction.
            } else {
                ordinary += 1;
                assert_eq!(fs::read(path.join("preserved.txt")).unwrap(), b"stray data");
            }
        }
        assert_eq!((ordinary, links), (1, 1));
        assert_eq!(fs::read(target.join("untouched.txt")).unwrap(), b"external data");
        register(&paths, "slot-c", std::process::id()).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn repeated_and_concurrent_cleanup_accepts_already_missing_entries() {
        let root = std::env::temp_dir().join(format!("nexus-lease-race-{}", crate::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let live = register(&paths, "slot-a", std::process::id()).unwrap();
        let path = live.parent().unwrap().join("bad.json"); fs::write(&path, b"bad").unwrap();
        quarantine(&paths, &path).unwrap();
        quarantine(&paths, &path).unwrap(); // Deterministic losing rename race.
        fs::write(&path, b"stale").unwrap();
        present(fs::remove_file(&path)).unwrap();
        assert!(present(fs::remove_file(&path)).unwrap().is_none());
        assert!(present(fs::symlink_metadata(&path)).unwrap().is_none());
        assert!(present(fs::File::open(&path)).unwrap().is_none());
        assert_eq!(present::<()>(Err(io::Error::from(io::ErrorKind::PermissionDenied))).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        for i in 0..128 { fs::write(live.parent().unwrap().join(format!("bad-{i}")), b"broken").unwrap(); }
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2).map(|_| scope.spawn(|| {
                barrier.wait(); ensure_release_idle(&paths, "slot-b")
            })).collect();
            for handle in handles { handle.join().unwrap().unwrap(); }
        });
        assert_eq!(fs::read_dir(live.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(ensure_all_idle(&paths).unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unavailable_quarantine_uses_an_independent_directory() {
        for link in [false,true] {
            let root=std::env::temp_dir().join(format!("nexus-quarantine-fallback-{}",crate::agent_auth::random_hex().unwrap()));
            let paths=NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
            let live=register(&paths,"live",std::process::id()).unwrap();
            let blocked=paths.run_dir.join("terminal-leases-corrupt");
            let target=root.join("unrelated");fs::create_dir(&target).unwrap();
            fs::write(target.join("untouched"),b"original").unwrap();
            if link { assert!(std::process::Command::new("cmd").args(["/C","mklink","/J"]).arg(&blocked).arg(&target).output().unwrap().status.success()); }
            else { fs::write(&blocked,b"placeholder").unwrap(); }
            fs::write(live.parent().unwrap().join("broken.json"),b"damaged evidence").unwrap();
            ensure_release_idle(&paths,"unused").unwrap();
            assert_eq!(ensure_release_idle(&paths,"live").unwrap_err().kind(),io::ErrorKind::ResourceBusy);
            let fallback=fs::read_dir(&paths.run_dir).unwrap().flatten().find(|entry|entry.file_name().to_string_lossy().starts_with("terminal-leases-corrupt-")).unwrap().path();
            let evidence=fs::read_dir(fallback).unwrap().next().unwrap().unwrap().path();
            assert_eq!(fs::read(evidence).unwrap(),b"damaged evidence");
            assert_eq!(fs::read(target.join("untouched")).unwrap(),b"original");
            assert_eq!(fs::read_dir(&target).unwrap().count(),1);
            if link { fs::remove_dir(&blocked).unwrap(); } else { assert_eq!(fs::read(&blocked).unwrap(),b"placeholder"); }
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn live_terminal_blocks_only_its_release_and_pid_reuse_is_not_ownership() {
        let root = std::env::temp_dir().join(format!("nexus-terminal-lease-test-{}", crate::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone());
        fs::create_dir_all(&root).unwrap();
        ensure_release_idle(&paths, "slot-a").expect("never-started root has no terminal leases");
        paths.ensure_directories().unwrap();
        let record = register(&paths, "slot-a", std::process::id()).unwrap();
        assert_eq!(ensure_release_idle(&paths, "slot-a").unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        assert_eq!(ensure_all_idle(&paths).unwrap_err().kind(), io::ErrorKind::ResourceBusy);
        ensure_release_idle(&paths, "slot-b").unwrap();
        let mut lease: Lease = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        lease.creation ^= 1;
        fs::write(&record, serde_json::to_vec(&lease).unwrap()).unwrap();
        ensure_release_idle(&paths, "slot-a").unwrap();
        ensure_all_idle(&paths).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
