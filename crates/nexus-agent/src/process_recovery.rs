//! Durable ownership for disposable commands. Recovery proves quiescence before
//! deleting their working files; an Agent restart is not that proof.
use std::{fs, io, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Record { version: u32, job: String }

pub(crate) struct Owner {
    directory: PathBuf,
    id: String,
    _lease: fs::File,
    pub(crate) job: String,
    #[cfg(unix)]
    pid: fs::File,
}

fn directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !nexus_core::path_is_reparse(&meta) => Ok(()),
        Ok(_) => Err(io::Error::other("Process ownership directory is not a plain directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
        Err(e) => Err(e),
    }
}

impl Owner {
    pub(crate) fn create(directory_path: &Path, job: Option<&str>) -> io::Result<Self> {
        directory(directory_path)?;
        // The private-file native API needs the Windows extended path form for
        // nested diagnostic work directories that exceed MAX_PATH.
        let canonical_directory = fs::canonicalize(directory_path)?;
        let directory_path = canonical_directory.as_path();
        let id = nexus_core::agent_auth::random_hex()?;
        let lease = fs::File::options().read(true).write(true).create_new(true)
            .open(directory_path.join(format!("{id}.lock")))?;
        lease.try_lock().map_err(io::Error::other)?;
        let job = job.map(str::to_owned).unwrap_or_else(|| format!("Global\\NexusCommand-{id}"));
        #[cfg(unix)]
        let pid = {
            use std::io::Write;
            let mut file = fs::File::options().read(true).write(true).create_new(true)
                .open(directory_path.join(format!("{id}.pid")))?;
            file.write_all(b"0                   \n")?;
            file.sync_all()?;
            file
        };
        nexus_core::write_private_json_atomic(directory_path, &directory_path.join(format!("{id}.json")),
            &Record { version: 1, job: job.clone() })?;
        Ok(Self { directory: directory_path.to_owned(), id, _lease: lease, job, #[cfg(unix)] pid })
    }

    /// Install a parent-death guardian before exec. The child and guardian inherit
    /// the lease across fork, closing the spawn/record race even if Agent dies.
    #[cfg(unix)]
    pub(crate) fn configure(&self, command: &mut std::process::Command) -> io::Result<std::os::unix::net::UnixStream> {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        let read_fd = read.as_raw_fd();
        let write_fd = write.as_raw_fd();
        let lease_fd = self._lease.as_raw_fd();
        let pid_fd = self.pid.as_raw_fd();
        let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(256, 1_048_576) as i32;
        command.process_group(0);
        unsafe { command.pre_exec(move || {
            // Only async-signal-safe libc calls in the forked child.
            let group = libc::getpid();
            let mut bytes = [b' '; 21]; bytes[20] = b'\n';
            let mut number = group as u32; let mut index = 10;
            loop { index -= 1; bytes[index] = b'0' + (number % 10) as u8; number /= 10; if number == 0 { break; } }
            let count = libc::pwrite(pid_fd, bytes.as_ptr().cast(), bytes.len(), 0);
            if count != bytes.len() as isize || libc::fsync(pid_fd) != 0 { return Err(io::Error::last_os_error()); }
            let guardian = libc::fork();
            if guardian < 0 { return Err(io::Error::last_os_error()); }
            if guardian == 0 {
                for fd in 0..max_fd { if fd != read_fd && fd != lease_fd { libc::close(fd); } }
                let mut byte = 0u8;
                loop {
                    let n = libc::read(read_fd, (&mut byte as *mut u8).cast(), 1);
                    if n >= 0 { break; }
                    if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) { break; }
                }
                libc::kill(-group, libc::SIGKILL);
                libc::_exit(0);
            }
            libc::close(read_fd); libc::close(write_fd);
            Ok(())
        }); }
        // The closure needs the descriptor alive until spawn. Command's closure
        // owns this read end; it is CLOEXEC in the actual executable.
        unsafe { command.pre_exec(move || { let _ = &read; Ok(()) }); }
        Ok(write)
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        let directory = self.directory.clone(); let id = self.id.clone();
        remove_if_present(&directory.join(format!("{id}.json")))?;
        #[cfg(unix)] remove_if_present(&directory.join(format!("{id}.pid")))?;
        drop(self);
        remove_if_present(&directory.join(format!("{id}.lock")))
    }

    #[cfg(unix)]
    pub(crate) fn group_is_empty(&self) -> io::Result<bool> {
        group_is_empty(&self.directory.join(format!("{}.pid", self.id)))
    }
}

#[cfg(unix)]
fn group_is_empty(path: &Path) -> io::Result<bool> {
    let bytes = nexus_core::read_regular_file_bounded(path, 64)?
        .ok_or_else(|| io::Error::other("Missing process group identity; preserved"))?;
    let group: i32 = std::str::from_utf8(&bytes).map_err(io::Error::other)?.trim().parse().map_err(io::Error::other)?;
    if group < 0 { return Err(io::Error::other("Invalid process group identity; preserved")); }
    Ok(group == 0 || unsafe { libc::kill(-group, 0) } == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) { Ok(()) => Ok(()), Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()), Err(e) => Err(e) }
}

/// Old records without recoverable process identity cannot safely be upgraded
/// by guessing from a dead parent PID. A later OS boot is positive evidence.
pub(crate) fn require_legacy_reboot(record: &Path) -> io::Result<()> {
    use std::time::{Duration, SystemTime};
    #[cfg(windows)]
    let boot = SystemTime::now().checked_sub(Duration::from_millis(unsafe {
        windows_sys::Win32::System::SystemInformation::GetTickCount64()
    })).ok_or_else(|| io::Error::other("Cannot establish operating system boot time"))?;
    #[cfg(target_os = "macos")]
    let boot = {
        let mut value: libc::timeval = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&value);
        if unsafe { libc::sysctlbyname(c"kern.boottime".as_ptr(), (&mut value as *mut libc::timeval).cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        SystemTime::UNIX_EPOCH + Duration::from_secs(value.tv_sec.try_into().map_err(io::Error::other)?)
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let boot = {
        let text = fs::read_to_string("/proc/uptime")?;
        let seconds: f64 = text.split_whitespace().next().ok_or_else(|| io::Error::other("Missing system uptime"))?.parse().map_err(io::Error::other)?;
        SystemTime::now().checked_sub(Duration::try_from_secs_f64(seconds).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("Cannot establish operating system boot time"))?
    };
    if fs::metadata(record)?.modified()?.checked_add(Duration::from_secs(2)).is_some_and(|time| time < boot) { return Ok(()); }
    Err(io::Error::other("This interrupted operation was created by an older Nexus without recoverable process identity. Restart the computer once, then retry; Nexus will recover it automatically and preserve configuration."))
}

/// A live lease covers queued workers and the process creation boundary, while
/// the durable Job/group covers descendants after the original owner has died.
pub(crate) fn reconcile(path: &Path) -> io::Result<()> {
    if !path.try_exists()? { return Ok(()); }
    directory(path)?;
    let mut entries = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?; let name = entry.file_name(); let name = name.to_string_lossy();
        if !name.ends_with(".lock") { continue; }
        entries += 1;
        if entries > 4096 { return Err(io::Error::other("Process recovery record limit exceeded")); }
        let id = &name[..name.len()-5];
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(io::Error::other("Invalid process ownership record; preserved")); }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Invalid process lease; preserved")); }
        let lease = fs::File::options().read(true).write(true).open(entry.path())?;
        lease.try_lock().map_err(|_| io::Error::new(io::ErrorKind::ResourceBusy, "Previous owned command is still stopping; retry after it exits"))?;
        let record_path = path.join(format!("{id}.json"));
        if let Some(bytes) = nexus_core::read_regular_file_bounded(&record_path, 4096)? {
            let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if record.version != 1 || !record.job.starts_with("Global\\Nexus") || record.job.len() > 200 {
                return Err(io::Error::other("Unsupported process ownership record; preserved"));
            }
            #[cfg(windows)]
            let empty = crate::dsh::named_operation_job_is_empty(&record.job)?;
            #[cfg(unix)]
            let empty = group_is_empty(&path.join(format!("{id}.pid")))?;
            if !empty { return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Previous command descendants are still stopping; working files retained")); }
            remove_if_present(&record_path)?;
        }
        #[cfg(unix)] remove_if_present(&path.join(format!("{id}.pid")))?;
        drop(lease);
        remove_if_present(&entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::{Command, Stdio}, time::{Duration, Instant}};

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!("nexus-process-recovery-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir(&path).unwrap(); path
    }

    #[test]
    fn prepared_record_is_recoverable_but_live_lease_blocks_cleanup() {
        let root = fixture();
        let owner = Owner::create(&root, None).unwrap();
        assert!(reconcile(&root).is_err());
        drop(owner); // Agent died before process creation.
        reconcile(&root).unwrap();
        reconcile(&root).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn failed_exec_settles_ownership_before_releasing_work() {
        let root = fixture();
        let command = Command::new("nexus-missing-executable-for-recovery-test");
        let error = crate::cold::run_owned_command(command, "missing executable", Duration::from_secs(10), &root, &nexus_core::CancellationToken::default()).await.unwrap_err();
        assert!(crate::cold::command_owner_quiescent(&error));
        reconcile(&root.join("owned-processes")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_owner_helper() {
        let Some(root) = std::env::var_os("NEXUS_CRASH_OWNER_FIXTURE") else { return; };
        let root = PathBuf::from(root);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut command = Command::new("node");
            command.args(["-e", "const fs=require('fs'),cp=require('child_process'); const child=cp.spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'}); fs.writeFileSync(process.argv[1],JSON.stringify({pid:process.pid,child:child.pid}));setInterval(()=>{},1000)"])
                .arg(root.join("ready.json")).stdin(Stdio::null());
            crate::cold::run_owned_command(command, "crash fixture", Duration::from_secs(60), &root, &nexus_core::CancellationToken::default()).await.unwrap();
        });
    }

    #[test]
    fn forced_owner_exit_reaps_descendants_and_allows_next_command() {
        let root = fixture();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_recovery::tests::crash_owner_helper", "--nocapture"])
            .env("NEXUS_CRASH_OWNER_FIXTURE", &root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !root.join("ready.json").exists() && Instant::now() < deadline && helper.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let ready = root.join("ready.json").exists();
        let blocked = reconcile(&root.join("owned-processes")).is_err();
        helper.kill().unwrap(); helper.wait().unwrap();
        assert!(ready, "owned fixture did not become ready: {}", root.display());
        assert!(blocked, "live owner must block cleanup");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match reconcile(&root.join("owned-processes")) {
                Ok(()) => break,
                Err(e) if Instant::now() >= deadline => panic!("recovery did not settle: {e}; {}", root.display()),
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        // A second real command proves recovery does not leave a permanent gate.
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut command = Command::new("node"); command.args(["-e", "process.exit(0)"]);
            crate::cold::run_owned_command(command, "recovered command", Duration::from_secs(10), &root, &nexus_core::CancellationToken::default()).await.unwrap();
        });
        fs::remove_dir_all(root).unwrap();
    }
}
