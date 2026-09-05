//! Native DSH home/profile resolution and owned dependency materialization.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use nexus_core::{
    build_pnpm_args, build_runtime_child_env, resolve_runtime_command, validate_profile_name,
    ConfigStore, NexusPaths,
};

pub(crate) const DSH_HOME_ENV: &str = "DSH_HOME";
pub(crate) const DEFAULT_MATERIALIZATION_TIMEOUT: Duration = Duration::from_secs(900);

pub(crate) fn resolve_dsh_home() -> io::Result<PathBuf> {
    if let Some(value) = env::var_os(DSH_HOME_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DSH_HOME must be an absolute path",
            ));
        }
        return Ok(path);
    }
    let native_home = if cfg!(windows) {
        env::var_os("USERPROFILE")
    } else {
        env::var_os("HOME")
    }
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "native home is unavailable"))?;
    if !native_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native home must be an absolute path",
        ));
    }
    Ok(native_home.join(".dsh"))
}

pub(crate) fn canonical_dsh_home(path: &Path) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH home must be an existing ordinary directory",
        ));
    }
    fs::canonicalize(path)
}

pub(crate) fn same_native_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        fn comparable(path: &Path) -> String {
            let mut value = path.to_string_lossy().replace('/', "\\");
            if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
                value = format!(r"\\{unc}");
            } else if let Some(dos) = value.strip_prefix(r"\\?\") {
                value = dos.to_owned();
            }
            value.to_ascii_lowercase()
        }
        comparable(left) == comparable(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub(crate) fn profile_directory(dsh_home: &Path, profile: &str) -> io::Result<PathBuf> {
    validate_profile_name(profile)?;
    let home = canonical_dsh_home(dsh_home)?;
    let profiles = home.join("profiles");
    let profile_dir = profiles.join(profile);
    let metadata = fs::symlink_metadata(&profile_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH profile must be an existing ordinary directory",
        ));
    }
    let canonical = fs::canonicalize(&profile_dir)?;
    if !canonical.starts_with(&profiles) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "DSH profile resolves outside DSH_HOME/profiles",
        ));
    }
    Ok(canonical)
}

/// Locate the official built CLI entry without invoking Corepack or a shell.
#[allow(dead_code)] // Shared now so the subsequent plugin-control phase cannot diverge.
pub(crate) fn locate_built_cli(release_root: &Path) -> io::Result<PathBuf> {
    let release_root = fs::canonicalize(release_root)?;
    let candidate = release_root.join("apps/cli/lib/bin.js");
    let metadata = fs::symlink_metadata(&candidate)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "built DSH CLI apps/cli/lib/bin.js is not an ordinary file",
        ));
    }
    let candidate = fs::canonicalize(candidate)?;
    if !candidate.starts_with(&release_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "built DSH CLI resolves outside its release root",
        ));
    }
    Ok(candidate)
}

pub(crate) fn profile_is_initialized(dsh_home: &Path, profile: &str) -> io::Result<bool> {
    let profile_dir = match profile_directory(dsh_home, profile) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let package = profile_dir.join("package.json");
    let metadata = match fs::symlink_metadata(&package) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Ok(false);
    }
    let bytes = fs::read(package)?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(value.is_object())
}

pub(crate) fn materialize_profile(
    paths: &NexusPaths,
    dsh_home: &Path,
    profile: &str,
) -> io::Result<()> {
    materialize_profile_with_timeout(paths, dsh_home, profile, DEFAULT_MATERIALIZATION_TIMEOUT)
}

fn materialize_profile_with_timeout(
    paths: &NexusPaths,
    dsh_home: &Path,
    profile: &str,
    timeout: Duration,
) -> io::Result<()> {
    let profile_dir = profile_directory(dsh_home, profile)?;
    let config = ConfigStore::new(paths.clone()).load()?;
    let runtime = config.runtime.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "runtime is not configured; dependency materialization remains pending",
        )
    })?;
    let command = resolve_runtime_command(&runtime, "pnpm")?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "pinned pnpm is unavailable; dependency materialization remains pending",
        )
    })?;
    let mut args = command.prefix_args;
    args.extend(build_pnpm_args(
        &runtime,
        ["install".into(), "--frozen-lockfile".into()],
    ));
    let child_env = build_runtime_child_env(&runtime, env::var_os("PATH").as_deref())?;
    let mut process = Command::new(command.program);
    process
        .args(args)
        .current_dir(profile_dir)
        .env(DSH_HOME_ENV, dsh_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in child_env {
        process.env(key, value);
    }
    let status = run_owned_process(&mut process, timeout)?;
    if !status.success() {
        return Err(io::Error::other(match status.code() {
            Some(code) => format!("pnpm materialization exited with code {code}"),
            None => "pnpm materialization exited without a code".to_owned(),
        }));
    }
    Ok(())
}

fn run_owned_process(command: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    run_owned_process_inner(command, timeout, ProcessTreeFault::None)
}

#[derive(Clone, Copy)]
enum ProcessTreeFault {
    None,
    #[cfg(test)]
    BeforeJobCreate,
    #[cfg(test)]
    BeforeAssign,
    #[cfg(test)]
    BeforeResume,
}

fn run_owned_process_inner(
    command: &mut Command,
    timeout: Duration,
    fault: ProcessTreeFault,
) -> io::Result<ExitStatus> {
    configure_owned_process(command);
    let child = command.spawn()?;
    let mut tree = OwnedProcessTree::new(child, fault)?;
    if let Err(error) = tree.resume(fault) {
        return tree.cleanup_failure(error);
    }
    let deadline = Instant::now() + timeout;
    loop {
        match tree.try_wait() {
            Ok(Some(status)) => {
                tree.wait_for_tree_exit(Duration::from_secs(5))?;
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => return tree.cleanup_failure(error),
        }
        if Instant::now() >= deadline {
            tree.terminate_and_wait(Duration::from_secs(10))?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "dependency materialization timed out after {}s",
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn configure_owned_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }
}

struct OwnedProcessTree {
    child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl OwnedProcessTree {
    fn new(mut child: Child, fault: ProcessTreeFault) -> io::Result<Self> {
        #[cfg(windows)]
        {
            #[cfg(not(test))]
            let _ = fault;
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeJobCreate) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("injected failure before job creation"));
            }
            let job = match create_kill_on_close_job() {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeAssign) {
                let _ = child.kill();
                let _ = child.wait();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                return Err(io::Error::other("injected failure before job assignment"));
            }
            if let Err(error) = assign_child_to_job(&child, job) {
                let _ = child.kill();
                let _ = child.wait();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                return Err(error);
            }
            return Ok(Self { child, job });
        }
        #[cfg(not(windows))]
        {
            let _ = fault;
            Ok(Self { child })
        }
    }

    fn resume(&mut self, fault: ProcessTreeFault) -> io::Result<()> {
        #[cfg(windows)]
        {
            #[cfg(not(test))]
            let _ = fault;
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeResume) {
                return Err(io::Error::other("injected failure before process resume"));
            }
            resume_process_primary_thread(self.child.id())
        }
        #[cfg(not(windows))]
        {
            let _ = fault;
            Ok(())
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate_and_wait(&mut self, timeout: Duration) -> io::Result<()> {
        terminate_tree(self)?;
        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.child.try_wait()?;
            if tree_is_empty(self)? {
                let _ = self.child.wait();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned materialization process tree did not terminate",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn cleanup_failure<T>(&mut self, primary: io::Error) -> io::Result<T> {
        match self.terminate_and_wait(Duration::from_secs(10)) {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(io::Error::new(
                primary.kind(),
                format!("{primary}; owned process cleanup also failed: {cleanup}"),
            )),
        }
    }

    fn wait_for_tree_exit(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let empty = match tree_is_empty(self) {
                Ok(empty) => empty,
                Err(error) => return self.cleanup_failure(error),
            };
            if empty {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.terminate_and_wait(Duration::from_secs(10))?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
            let _ = self.child.try_wait();
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
        #[cfg(unix)]
        {
            let _ = terminate_tree(self);
            let _ = self.child.try_wait();
        }
    }
}

#[cfg(unix)]
fn terminate_tree(tree: &mut OwnedProcessTree) -> io::Result<()> {
    let pid = i32::try_from(tree.child.id())
        .map_err(|_| io::Error::other("child process ID exceeds i32"))?;
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn tree_is_empty(tree: &OwnedProcessTree) -> io::Result<bool> {
    let pid = i32::try_from(tree.child.id())
        .map_err(|_| io::Error::other("child process ID exceeds i32"))?;
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        Ok(false)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        return Err(error);
    }
    Ok(job)
}

#[cfg(windows)]
fn assign_child_to_job(
    child: &Child,
    job: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let process = child.as_raw_handle().cast();
    let ok =
        unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job, process) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn resume_process_primary_thread(process_id: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let mut thread_ids = Vec::new();
    while found {
        if entry.th32OwnerProcessID == process_id {
            thread_ids.push(entry.th32ThreadID);
        }
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    if thread_ids.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "suspended materialization process has {} primary-thread candidates",
                thread_ids.len()
            ),
        ));
    }
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_ids[0]) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    let resumed = unsafe { ResumeThread(thread) };
    unsafe { CloseHandle(thread) };
    if resumed == u32::MAX {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn terminate_tree(tree: &mut OwnedProcessTree) -> io::Result<()> {
    let ok = unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(tree.job, 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn tree_is_empty(tree: &OwnedProcessTree) -> io::Result<bool> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicAccountingInformation, QueryInformationJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    };
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let ok = unsafe {
        QueryInformationJobObject(
            tree.job,
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(accounting.ActiveProcesses == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{NexusConfigFile, RuntimeConfig, RuntimePin};
    use nexus_protocol::{RuntimeInstallMode, RuntimeOwnership, RuntimeSource};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-owned-process-{}-{}-{}",
            label,
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("creates isolated process test directory");
        root
    }

    fn executable_on_path(name: &str) -> Option<PathBuf> {
        let path = env::var_os("PATH")?;
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    }

    #[test]
    fn materializer_uses_pinned_node_exact_pnpm_policy_and_bound_home() {
        let root = test_dir("argv");
        let paths = NexusPaths::from_root(root.join("nexus-data"));
        let home = root.join("dsh-home");
        let profile = home.join("profiles/demo");
        fs::create_dir_all(&profile).expect("creates synthetic profile");
        fs::write(profile.join("package.json"), r#"{"name":"demo"}"#)
            .expect("writes synthetic package");
        let fake_pnpm = root.join("fake-pnpm.js");
        fs::write(
            &fake_pnpm,
            r#"const fs = require('fs');
const path = require('path');
fs.writeFileSync(path.join(process.cwd(), 'materialized.json'), JSON.stringify({
  argv: process.argv.slice(2),
  dshHome: process.env.DSH_HOME,
  cwd: process.cwd()
}));
"#,
        )
        .expect("writes fake pnpm entry");
        let node = executable_on_path(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("test host provides Node required by the DSH runtime contract");
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile {
                runtime: Some(RuntimeConfig {
                    node: Some(RuntimePin {
                        path: node,
                        ownership: RuntimeOwnership::System,
                    }),
                    pnpm: Some(RuntimePin {
                        path: fake_pnpm,
                        ownership: RuntimeOwnership::System,
                    }),
                    git: None,
                    source: RuntimeSource::Official,
                    mode: RuntimeInstallMode::Portable,
                }),
                ..Default::default()
            })
            .expect("writes pinned runtime config");

        materialize_profile_with_timeout(&paths, &home, "demo", Duration::from_secs(10))
            .expect("fake pnpm materialization succeeds");
        let observed: serde_json::Value = serde_json::from_slice(
            &fs::read(profile.join("materialized.json")).expect("reads fake pnpm observation"),
        )
        .expect("parses fake pnpm observation");
        assert_eq!(
            observed["argv"],
            serde_json::json!([
                "--config.minimumReleaseAge=0",
                "--registry=https://registry.npmjs.org",
                "install",
                "--frozen-lockfile"
            ])
        );
        assert!(same_native_path(
            Path::new(observed["dshHome"].as_str().expect("DSH_HOME is text")),
            &home
        ));
        assert!(same_native_path(
            Path::new(observed["cwd"].as_str().expect("cwd is text")),
            &fs::canonicalize(&profile).expect("canonical profile")
        ));
        fs::remove_dir_all(root).expect("removes isolated materialization test");
    }

    #[cfg(windows)]
    fn powershell() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot exists"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe")
    }

    #[cfg(windows)]
    fn ps_literal(path: &Path) -> String {
        path.to_string_lossy().replace('\'', "''")
    }

    #[cfg(windows)]
    #[test]
    fn timeout_ends_owned_descendant_before_returning() {
        let root = test_dir("descendant");
        let marker = root.join("descendant-writes.txt");
        let child_pid = root.join("descendant.pid");
        let child_script = root.join("child.ps1");
        let parent_script = root.join("parent.ps1");
        fs::write(
            &child_script,
            r#"param([string]$Marker)
while ($true) {
  Add-Content -LiteralPath $Marker -Value 'owned'
  Start-Sleep -Milliseconds 20
}
"#,
        )
        .expect("writes child fixture");
        fs::write(
            &parent_script,
            format!(
                r#"$child = Start-Process -FilePath "$PSHOME\powershell.exe" -ArgumentList @('-NoProfile','-File','{}','{}') -PassThru
$child.Id | Set-Content -LiteralPath '{}'
while ($true) {{ Start-Sleep -Seconds 1 }}
"#,
                ps_literal(&child_script),
                ps_literal(&marker),
                ps_literal(&child_pid),
            ),
        )
        .expect("writes parent fixture");

        let mut command = Command::new(powershell());
        command.args(["-NoProfile", "-File"]).arg(&parent_script);
        let error = run_owned_process(&mut command, Duration::from_secs(3))
            .expect_err("controlled process tree times out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(child_pid.is_file(), "descendant started before cleanup");
        assert!(marker.is_file(), "descendant wrote before cleanup");
        let length = fs::metadata(&marker).expect("marker metadata").len();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            fs::metadata(&marker)
                .expect("marker remains readable")
                .len(),
            length,
            "owned descendant cannot write after timeout returns"
        );
        fs::remove_dir_all(root).expect("removes isolated process test directory");
    }

    #[cfg(windows)]
    #[test]
    fn setup_and_resume_failures_leave_suspended_child_inert() {
        for (index, fault) in [
            ProcessTreeFault::BeforeJobCreate,
            ProcessTreeFault::BeforeAssign,
            ProcessTreeFault::BeforeResume,
        ]
        .into_iter()
        .enumerate()
        {
            let root = test_dir(&format!("fault-{index}"));
            let marker = root.join("started.txt");
            let script = format!(
                "Set-Content -LiteralPath '{}' -Value 'started'",
                ps_literal(&marker)
            );
            let mut command = Command::new(powershell());
            command.args(["-NoProfile", "-Command", &script]);
            run_owned_process_inner(&mut command, Duration::from_secs(1), fault)
                .expect_err("injected ownership failure is returned");
            thread::sleep(Duration::from_millis(100));
            assert!(
                !marker.exists(),
                "suspended child never executes after ownership failure"
            );
            fs::remove_dir_all(root).expect("removes isolated process test directory");
        }
    }
}
