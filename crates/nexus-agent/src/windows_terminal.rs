//! A console child must receive fresh console handles, not the Agent's log pipes.
use std::{ffi::OsStr, io, os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, process::Command};
use windows_sys::Win32::{Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::{CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW, STARTF_USESHOWWINDOW}};

pub(crate) struct ConsoleChild { handle: OwnedHandle, pid: u32 }

impl ConsoleChild {
    pub(crate) fn id(&self) -> u32 { self.pid }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<u32>> {
        let handle = self.handle.as_raw_handle();
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 { return Err(io::Error::last_os_error()); }
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() { return Ok(()); }
        if unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) } == 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
    pub(crate) fn wait(&mut self) -> io::Result<()> {
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 5000) } != WAIT_OBJECT_0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "console child did not exit"));
        }
        Ok(())
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) { return Err(io::Error::new(io::ErrorKind::InvalidInput, "console argument contains NUL")); }
    Ok(value)
}

// Windows argv quoting; no shell interpolation. All user paths remain env values.
fn quoted(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut output = vec![34];
    let mut slashes = 0;
    for ch in wide(value)? {
        if ch == 92 { slashes += 1; continue; }
        output.extend(std::iter::repeat_n(92, if ch == 34 { slashes * 2 + 1 } else { slashes }));
        slashes = 0;
        output.push(ch);
    }
    output.extend(std::iter::repeat_n(92, slashes * 2));
    output.push(34);
    Ok(output)
}

pub(crate) fn spawn(command: &Command, visible: bool) -> io::Result<ConsoleChild> {
    let mut program = wide(command.get_program())?; program.push(0);
    let mut args = quoted(command.get_program())?;
    for argument in command.get_args() { args.push(32); args.extend(quoted(argument)?); }
    args.push(0);
    // PowerShell exposes verbatim cwd paths as provider-qualified locations, which breaks pnpm.
    // Normalize only the console boundary; keep canonical paths for validation and identity.
    let mut directory = command.get_current_dir().map(|p| wide(&nexus_core::node_script_argument(p))).transpose()?;
    if let Some(path) = &mut directory { path.push(0); }
    let mut env: Vec<_> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        // Nexus overrides only ASCII environment names (PATH, DSH_HOME, ...).
        env.retain(|(old, _)| !old.to_string_lossy().eq_ignore_ascii_case(&key.to_string_lossy()));
        if let Some(value) = value { env.push((key.to_owned(), value.to_owned())); }
    }
    env.sort_by_cached_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (key, value) in env { block.extend(wide(&key)?); block.push(61); block.extend(wide(&value)?); block.push(0); }
    block.push(0);
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    if !visible { startup.dwFlags = STARTF_USESHOWWINDOW; startup.wShowWindow = 0; }
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // Deliberately omit STARTF_USESTDHANDLES and handle inheritance. Windows
    // supplies this new console's input/output handles, even for a headless parent.
    let created = unsafe { CreateProcessW(program.as_ptr(), args.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0,
        CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT, block.as_ptr().cast(),
        directory.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()), &startup, &mut process) };
    if created == 0 { return Err(io::Error::last_os_error()); }
    unsafe { CloseHandle(process.hThread); }
    Ok(ConsoleChild { handle: unsafe { OwnedHandle::from_raw_handle(process.hProcess) }, pid: process.dwProcessId })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_console_has_interactive_handles_and_preserves_environment() {
        let root = std::env::temp_dir().join(format!("nexus-console-{}-{}", std::process::id(), nexus_core::agent_auth::random_hex().unwrap()));
        std::fs::create_dir(&root).unwrap();
        let result = root.join("console result.txt");
        let shell = crate::test_powershell();
        let mut command = Command::new(shell);
        command.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command", "[IO.File]::WriteAllText($env:NEXUS_CONSOLE_RESULT, ('{0}|{1}|{2}|{3}' -f [Console]::IsInputRedirected, [Console]::IsOutputRedirected, $env:NEXUS_CONSOLE_VALUE, (Get-Location).Path))"])
            .current_dir(std::fs::canonicalize(&root).unwrap()).env("NEXUS_CONSOLE_RESULT", &result).env("NEXUS_CONSOLE_VALUE", "space %PATH% & ! ' 中文");
        let mut child = spawn(&command, false).unwrap();
        let began = std::time::Instant::now();
        while !result.exists() && began.elapsed().as_secs() < 10 { std::thread::sleep(std::time::Duration::from_millis(100)); }
        let value = std::fs::read_to_string(&result);
        let running = child.try_wait().unwrap().is_none();
        child.kill().unwrap(); child.wait().unwrap();
        let value = value.unwrap();
        let observed_directory = value.strip_prefix("False|False|space %PATH% & ! ' 中文|").expect("interactive handles and environment preserved");
        assert_eq!(std::fs::canonicalize(observed_directory).unwrap(), std::fs::canonicalize(&root).unwrap());
        std::fs::remove_dir_all(&root).unwrap();
        assert!(running, "-NoExit must remain interactive after initialization");
    }
}
