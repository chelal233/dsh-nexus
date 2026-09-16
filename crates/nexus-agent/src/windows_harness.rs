//! Hidden, dedicated console and an isolated CTRL_C sender for owned Harnesses.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    process::{Command, ExitStatus},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Console::{
            AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
            CTRL_C_EVENT,
        },
        Threading::*,
    },
};

pub(crate) enum Child {
    Native {
        handle: OwnedHandle,
        pid: u32,
        exited: Option<ExitStatus>,
    },
    #[cfg(test)]
    Tokio(tokio::process::Child),
}
#[cfg(test)]
impl From<tokio::process::Child> for Child {
    fn from(child: tokio::process::Child) -> Self {
        Self::Tokio(child)
    }
}
impl Child {
    pub(crate) fn id(&self) -> Option<u32> {
        match self {
            Self::Native { pid, exited, .. } => exited.is_none().then_some(*pid),
            #[cfg(test)]
            Self::Tokio(child) => child.id(),
        }
    }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Native { handle, exited, .. } => {
                if exited.is_some() {
                    return Ok(*exited);
                }
                match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
                    WAIT_TIMEOUT => Ok(None),
                    WAIT_OBJECT_0 => {
                        let mut code = 0;
                        if unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) } == 0 {
                            return Err(io::Error::last_os_error());
                        }
                        *exited = Some(ExitStatus::from_raw(code));
                        Ok(*exited)
                    }
                    _ => Err(io::Error::last_os_error()),
                }
            }
            #[cfg(test)]
            Self::Tokio(child) => child.try_wait(),
        }
    }
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        match self {
            Self::Native { handle, .. } => {
                if unsafe { TerminateProcess(handle.as_raw_handle(), 1) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let _ = self.wait().await?;
                Ok(())
            }
            #[cfg(test)]
            Self::Tokio(child) => child.kill().await,
        }
    }
    pub(crate) async fn request_stop(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        let (pid, created) = match self {
            Self::Native { handle, pid, .. } => (*pid, creation_time(handle.as_raw_handle())?),
            #[cfg(test)]
            Self::Tokio(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "test child has no owned console",
                ))
            }
        };
        let mut helper = tokio::process::Command::new(std::env::current_exe()?);
        #[cfg(not(test))]
        helper.args(["--signal-console", &pid.to_string(), &created.to_string()]);
        #[cfg(test)]
        helper
            .args([
                "--exact",
                "windows_harness::tests::signal_helper_process",
                "--nocapture",
            ])
            .env("NEXUS_TEST_SIGNAL", format!("{pid} {created}"));
        helper
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let output = tokio::time::timeout(Duration::from_secs(2), helper.output())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "CTRL_C sender exceeded its execution deadline",
                )
            })??;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "CTRL_C sender failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if let Self::Native {
            handle,
            exited: None,
            ..
        } = self
        {
            unsafe {
                TerminateProcess(handle.as_raw_handle(), 1);
            }
        }
    }
}

fn creation_time(handle: std::os::windows::io::RawHandle) -> io::Result<u64> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

/// This entry point runs only in a disposable process, never in the Agent.
pub fn signal_helper(args: &[String]) -> io::Result<()> {
    if args.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CTRL_C requires pid and process creation time",
        ));
    }
    let pid: u32 = args[0]
        .parse()
        .map_err(|_| io::Error::other("invalid CTRL_C pid"))?;
    let expected: u64 = args[1]
        .parse()
        .map_err(|_| io::Error::other("invalid CTRL_C creation time"))?;
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    if creation_time(process.as_raw_handle())? != expected
        || unsafe { WaitForSingleObject(raw, 0) } != WAIT_TIMEOUT
    {
        return Err(io::Error::other(
            "CTRL_C target is no longer the owned live process",
        ));
    }
    unsafe {
        FreeConsole();
    }
    if unsafe { AttachConsole(pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Only this helper ignores CTRL_C. The target inherits no ignore setting.
    if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

unsafe extern "system" fn keep_command_owner(event: u32) -> i32 {
    i32::from(event == CTRL_C_EVENT)
}

/// Preserve std::Command's PATH resolution and Windows batch-file quoting.
/// A handler function, unlike the NULL/ignore flag, is not inherited by children.
pub fn command_helper(args: &[std::ffi::OsString]) -> io::Result<i32> {
    let Some(program) = args.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Harness command is required",
        ));
    };
    // The Launcher starts Agent with CREATE_NEW_PROCESS_GROUP. Its inherited
    // CTRL_C-ignore attribute survives even a new hidden console. Clear it in
    // this dedicated owner before spawning Harness, never in the Agent itself.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { SetConsoleCtrlHandler(Some(keep_command_owner), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let status = Command::new(program)
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Harness argument contains NUL",
        ));
    }
    Ok(value)
}
fn quoted(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut out = vec![34];
    let mut slashes = 0;
    for ch in wide(value)? {
        if ch == 92 {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            92,
            if ch == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n(92, slashes * 2));
    out.push(34);
    Ok(out)
}
fn duplicate(file: &File) -> io::Result<OwnedHandle> {
    let mut handle = std::ptr::null_mut();
    let current = unsafe { GetCurrentProcess() };
    if unsafe {
        DuplicateHandle(
            current,
            file.as_raw_handle(),
            current,
            &mut handle,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast());
        }
    }
}

pub(crate) fn spawn(
    command: &Command,
    logs: Option<(&File, &File)>,
    job: &crate::dsh::WindowsJob,
) -> io::Result<Child> {
    // Every executable needs the isolated owner to clear inherited CTRL_C
    // suppression before it runs. All descendants remain in the original Job.
    let executable = std::env::current_exe()?;
    let mut program = wide(executable.as_os_str())?;
    program.push(0);
    let mut arguments = quoted(executable.as_os_str())?;
    #[cfg(not(test))]
    {
        arguments.push(32);
        arguments.extend(quoted(OsStr::new("--harness-command"))?);
        arguments.push(32);
        arguments.extend(quoted(command.get_program())?);
        for arg in command.get_args() {
            arguments.push(32);
            arguments.extend(quoted(arg)?);
        }
    }
    #[cfg(test)]
    for arg in [
        "--exact",
        "windows_harness::tests::command_helper_process",
        "--nocapture",
    ] {
        arguments.push(32);
        arguments.extend(quoted(OsStr::new(arg))?);
    }
    arguments.push(0);
    let mut directory = command
        .get_current_dir()
        .map(|p| wide(p.as_os_str()))
        .transpose()?;
    if let Some(dir) = &mut directory {
        dir.push(0);
    }
    let mut env: Vec<_> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        env.retain(|(old, _)| {
            !old.to_string_lossy()
                .eq_ignore_ascii_case(&key.to_string_lossy())
        });
        if let Some(value) = value {
            env.push((key.to_owned(), value.to_owned()));
        }
    }
    #[cfg(test)]
    {
        let args: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        env.retain(|(key, _)| !key.eq_ignore_ascii_case("NEXUS_TEST_COMMAND"));
        env.push((
            "NEXUS_TEST_COMMAND".into(),
            serde_json::to_string(&args)?.into(),
        ));
    }
    env.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (key, value) in env {
        block.extend(wide(&key)?);
        block.push(61);
        block.extend(wide(&value)?);
        block.push(0);
    }
    block.push(0);
    let null = File::options().read(true).write(true).open("NUL")?;
    let input = duplicate(&null)?;
    let output = duplicate(logs.map_or(&null, |l| l.0))?;
    let error = duplicate(logs.map_or(&null, |l| l.1))?;
    let mut handles = [
        input.as_raw_handle(),
        output.as_raw_handle(),
        error.as_raw_handle(),
    ];
    let mut size = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size);
    }
    let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    if unsafe { InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = Attributes(storage);
    if unsafe {
        UpdateProcThreadAttribute(
            attributes.0.as_mut_ptr().cast(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            std::mem::size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Assignment is part of CreateProcess, so even termination before it returns
    // cannot leave an unowned suspended process behind.
    let mut jobs = [job.raw_handle()];
    if unsafe { UpdateProcThreadAttribute(attributes.0.as_mut_ptr().cast(), 0,
        0x0002000d /* PROC_THREAD_ATTRIBUTE_JOB_LIST */, jobs.as_mut_ptr().cast(),
        std::mem::size_of_val(&jobs), std::ptr::null_mut(), std::ptr::null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESHOWWINDOW | STARTF_USESTDHANDLES;
    startup.StartupInfo.wShowWindow = 0;
    startup.StartupInfo.hStdInput = input.as_raw_handle();
    startup.StartupInfo.hStdOutput = output.as_raw_handle();
    startup.StartupInfo.hStdError = error.as_raw_handle();
    startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            program.as_ptr(),
            arguments.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NEW_CONSOLE
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            block.as_ptr().cast(),
            directory.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(process.hThread);
    }
    let child = Child::Native {
        handle: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
        pid: process.dwProcessId,
        exited: None,
    };
    // On failure Child and Job drops terminate the still-suspended process.
    crate::dsh::resume_process_primary_thread(process.dwProcessId)?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, Ordering},
        time::Instant,
    };
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn handler(event: u32) -> i32 {
        if event == CTRL_C_EVENT {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    #[test]
    fn signal_helper_process() {
        let Ok(args) = std::env::var("NEXUS_TEST_SIGNAL") else {
            return;
        };
        if let Err(error) = signal_helper(
            &args
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        ) {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }

    #[test]
    fn command_helper_process() {
        let Ok(args) = std::env::var("NEXUS_TEST_COMMAND") else {
            return;
        };
        let args: Vec<String> = serde_json::from_str(&args).unwrap();
        match command_helper(&args.into_iter().map(Into::into).collect::<Vec<_>>()) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn console_fixture_process() {
        let Some(root) = std::env::var_os("NEXUS_TEST_CONSOLE_ROOT").map(PathBuf::from) else {
            return;
        };
        assert_ne!(unsafe { SetConsoleCtrlHandler(Some(handler), 1) }, 0);
        println!("stdout remains connected");
        eprintln!("stderr remains connected");
        std::fs::write(root.join("ready"), "ready").unwrap();
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(20) {
            if INTERRUPTED.swap(false, Ordering::SeqCst) {
                std::fs::write(root.join("signal-received"), "CTRL_C").unwrap();
                if std::env::var_os("NEXUS_TEST_IGNORE").is_none() {
                    std::process::exit(130);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::process::exit(99);
    }

    fn fixture(root: &Path, ignore: bool) -> (Child, crate::dsh::WindowsJob) {
        std::fs::create_dir_all(root).unwrap();
        let stdout = File::create(root.join("stdout")).unwrap();
        let stderr = File::create(root.join("stderr")).unwrap();
        // The same regression can explicitly exercise an installed/bundled Node
        // without making the default suite depend on an external runtime.
        let mut command = if let Some(node) = std::env::var_os("NEXUS_TEST_NODE") {
            let mut command = Command::new(node);
            command.args(["-e", r#"const fs=require('fs'),p=require('path'),r=process.env.NEXUS_TEST_CONSOLE_ROOT;
process.on('SIGINT',()=>{fs.writeFileSync(p.join(r,'signal-received'),'CTRL_C');if(!process.env.NEXUS_TEST_IGNORE)process.exit(130)});
console.log('stdout remains connected');console.error('stderr remains connected');fs.writeFileSync(p.join(r,'ready'),'ready');setInterval(()=>{},100);"#]);
            command
        } else {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "windows_harness::tests::console_fixture_process",
                "--nocapture",
            ]);
            command
        };
        command.env("NEXUS_TEST_CONSOLE_ROOT", root);
        if ignore {
            command.env("NEXUS_TEST_IGNORE", "1");
        }
        let job = crate::dsh::WindowsJob::new().unwrap();
        let child = spawn(&command, Some((&stdout, &stderr)), &job).unwrap();
        assert!(job.contains_pid(child.id().unwrap()).unwrap());
        (child, job)
    }
    async fn ready(root: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fixture starts before signalling");
    }

    #[tokio::test]
    async fn ctrl_c_reaches_owned_console_preserves_logs_and_does_not_signal_other_console() {
        let root = std::env::temp_dir().join(format!(
            "nexus-graceful-{}",
            nexus_core::agent_auth::random_hex().unwrap()
        ));
        let target = root.join("target");
        let other = root.join("other");
        let (mut child, _job) = fixture(&target, false);
        let (mut unrelated, _other_job) = fixture(&other, false);
        ready(&target).await;
        ready(&other).await;
        assert!(
            signal_helper(&[unrelated.id().unwrap().to_string(), "0".to_owned()]).is_err(),
            "stale PID identity must fail before attaching or signalling"
        );
        child.request_stop().await.unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.code(), Some(130));
        assert_eq!(
            std::fs::read_to_string(target.join("signal-received")).unwrap(),
            "CTRL_C"
        );
        assert!(std::fs::read_to_string(target.join("stdout"))
            .unwrap()
            .contains("stdout remains connected"));
        assert!(std::fs::read_to_string(target.join("stderr"))
            .unwrap()
            .contains("stderr remains connected"));
        assert!(unrelated.try_wait().unwrap().is_none());
        assert!(!other.join("signal-received").exists());
        unrelated.kill().await.unwrap();
        drop(child);
        drop(unrelated);
        drop(_job);
        drop(_other_job);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn ignored_ctrl_c_can_still_be_stopped_after_bounded_wait() {
        let root = std::env::temp_dir().join(format!(
            "nexus-graceful-ignore-{}",
            nexus_core::agent_auth::random_hex().unwrap()
        ));
        let (mut child, job) = fixture(&root, true);
        ready(&root).await;
        child.request_stop().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), child.wait())
                .await
                .is_err()
        );
        assert!(root.join("signal-received").exists());
        child.kill().await.unwrap();
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(1));
        drop(child);
        drop(job);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "Current host denies CREATE_BREAKAWAY_FROM_JOB (Windows error 5); requires real-environment acceptance"]
    async fn launcher_process_flags_do_not_make_harness_ignore_ctrl_c() {
        probe_parent_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .await;
    }

    #[tokio::test]
    async fn inherited_ctrl_c_ignore_is_cleared_before_harness() {
        // Isolate the causal inheritance property without asking to leave a Job.
        // This does not replace the full Launcher-flags acceptance above.
        probe_parent_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW).await;
    }

    async fn probe_parent_flags(flags: u32) {
        // Reproduce the real Launcher -> detached Agent -> console -> Harness
        // chain. A normal cargo test parent does not carry CTRL_C suppression.
        let mut parent = tokio::process::Command::new(std::env::current_exe().unwrap());
        parent.args(["--exact", "windows_harness::tests::ctrl_c_reaches_owned_console_preserves_logs_and_does_not_signal_other_console", "--nocapture"])
            .creation_flags(flags)
            .kill_on_drop(true).stdin(std::process::Stdio::null());
        let output = tokio::time::timeout(Duration::from_secs(10), parent.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "detached Agent must preserve graceful signalling: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn path_commands_and_batch_files_keep_standard_command_semantics() {
        let root = std::env::temp_dir().join(format!(
            "nexus-graceful-command-{}",
            nexus_core::agent_auth::random_hex().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("with space.cmd");
        std::fs::write(&script, "@echo off\r\necho batch-output\r\nexit /b 7\r\n").unwrap();
        for (program, args, code, marker) in [
            (
                std::ffi::OsString::from("cmd.exe"),
                vec!["/d", "/c", "echo path-output"],
                0,
                "path-output",
            ),
            (
                std::env::var_os("ComSpec").expect("Windows command interpreter"),
                vec!["/C", "echo absolute-output & exit /b 0"],
                0,
                "absolute-output",
            ),
            (script.into_os_string(), vec![], 7, "batch-output"),
        ] {
            let stdout = File::create(root.join("stdout")).unwrap();
            let stderr = File::create(root.join("stderr")).unwrap();
            let job = crate::dsh::WindowsJob::new().unwrap();
            let mut command = Command::new(program);
            command.args(args).current_dir(&root);
            let mut child = spawn(&command, Some((&stdout, &stderr)), &job).unwrap();
            let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(exit.code(), Some(code));
            assert!(std::fs::read_to_string(root.join("stdout"))
                .unwrap()
                .contains(marker));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
