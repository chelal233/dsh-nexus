//! Installer-only shutdown. The payload runs from the installer's temporary
//! directory, so it neither locks nor depends on the previous installed CLI.
use std::{ffi::OsString, path::PathBuf};

/// Use the OS parser compiled into the installer payload. Never compile C#
/// in the customer's PowerShell process to obtain this Win32 entry point.
#[cfg(windows)]
fn parse_arguments(command: &std::ffi::OsStr) -> Result<Vec<String>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};
    let wide: Vec<u16> = command.encode_wide().collect();
    if wide.is_empty() || wide.contains(&0) {
        return Err("Agent command line is empty or contains NUL".into());
    }
    let wide: Vec<u16> = wide.into_iter().chain(Some(0)).collect();
    let mut count = 0;
    unsafe {
        let argv = CommandLineToArgvW(wide.as_ptr(), &mut count);
        if argv.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let result = (0..count).map(|index| {
            let value = *argv.add(index as usize);
            let mut length = 0;
            while *value.add(length) != 0 { length += 1; }
            String::from_utf16(std::slice::from_raw_parts(value, length))
                .map_err(|_| "Agent arguments contain invalid Unicode".to_owned())
        }).collect();
        LocalFree(argv.cast());
        result
    }
}

#[cfg(windows)]
pub fn print_arguments() -> Result<(), String> {
    let command = std::env::var_os("NEXUS_INSTALL_STOP_COMMAND_LINE")
        .ok_or("Missing Agent command line")?;
    let arguments = parse_arguments(&command)?;
    println!("{}", serde_json::to_string(&arguments).map_err(|error| error.to_string())?);
    Ok(())
}

fn install_dir(args: &[OsString]) -> Result<PathBuf, String> {
    if args.len() != 2 || args[0] != "--install-dir" {
        return Err("expected installer-stop --install-dir PATH".to_owned());
    }
    let directory = PathBuf::from(&args[1]);
    if !directory.is_absolute() || directory.parent().is_none() {
        return Err("installation directory must be an absolute non-root path".to_owned());
    }
    Ok(directory)
}

pub fn run(args: Vec<OsString>) -> Result<(), String> {
    let directory = install_dir(&args)?;
    #[cfg(windows)]
    {
        use std::{os::windows::process::CommandExt, process::Command};
        let system_root = std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
        let powershell =
            PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                include_str!("installer_shutdown.ps1"),
            ])
            .env("NEXUS_INSTALL_STOP_DIRECTORY", directory)
            .env("NEXUS_INSTALL_STOP_HELPER", std::env::current_exe().map_err(|error| error.to_string())?)
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|error| format!("cannot run installer shutdown: {error}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                // Only terminate our own stuck helper, never the Agent.
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    "installer shutdown verification timed out; no files were replaced".to_owned(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        if !status.success() {
            return Err("Agent shutdown could not be verified. Close Nexus and retry; installation was stopped before replacing files.".to_owned());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err("installer-stop is supported only on Windows".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn native_parser_preserves_quoted_unicode_and_metacharacters() {
        let command = std::ffi::OsStr::new(
            r#""C:\Program Files\Nexus\nexus-agent.exe" --data-dir "C:\Users\用户\data & $name" --instance-id instance"#,
        );
        assert_eq!(parse_arguments(command).unwrap(), vec![
            "C:\\Program Files\\Nexus\\nexus-agent.exe", "--data-dir",
            "C:\\Users\\用户\\data & $name", "--instance-id", "instance",
        ]);
        assert!(parse_arguments(std::ffi::OsStr::new("")).is_err());
        assert!(parse_arguments(std::ffi::OsStr::new("a\0b")).is_err());
    }

    #[test]
    fn installer_script_never_compiles_source() {
        let script = include_str!("installer_shutdown.ps1").to_ascii_lowercase();
        for forbidden in ["add-type", "csc.exe", "codedom", "compileassembly", "dotnet build"] {
            assert!(!script.contains(forbidden), "Installer must not use {forbidden}");
        }
    }

    #[test]
    fn rejects_missing_relative_and_extra_install_arguments() {
        assert!(install_dir(&[]).is_err());
        assert!(install_dir(&["--install-dir".into(), "relative".into()]).is_err());
        assert!(install_dir(&["--install-dir".into(), "relative".into(), "extra".into()]).is_err());
        let root = std::env::temp_dir().join("Nexus install path");
        assert_eq!(
            install_dir(&["--install-dir".into(), root.clone().into_os_string()]).unwrap(),
            root
        );
    }
}
