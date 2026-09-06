//! Installer-only shutdown. The payload runs from the installer's temporary
//! directory, so it neither locks nor depends on the previous installed CLI.
use std::{ffi::OsString, path::PathBuf};

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
