//! Spawn independent Agent with an explicit standard-handle inheritance list.
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
        CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::Threading::*,
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
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Agent argument contains NUL",
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
    stdout: &File,
    stderr: &File,
    flags: u32,
) -> io::Result<Child> {
    let mut program = wide(command.get_program())?;
    program.push(0);
    let mut arguments = quoted(command.get_program())?;
    for argument in command.get_args() {
        arguments.push(32);
        arguments.extend(quoted(argument)?);
    }
    arguments.push(0);
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
    let output = duplicate(stdout)?;
    let error = duplicate(stderr)?;
    let mut handles = [
        input.as_raw_handle(),
        output.as_raw_handle(),
        error.as_raw_handle(),
    ];
    let mut size = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
    }
    let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    if unsafe { InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut size) }
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
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
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
            flags | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            block.as_ptr().cast(),
            std::ptr::null(),
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
    Ok(Child::Native {
        handle: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
        pid: process.dwProcessId,
        exited: None,
    })
}
