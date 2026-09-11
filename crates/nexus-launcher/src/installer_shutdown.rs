//! Installer-only shutdown. The payload runs from the installer's temporary
//! directory, so it neither locks nor depends on the previous installed CLI.
use std::{ffi::OsString, path::PathBuf};
const INNER_BUDGET_SECS: u64 = 150;
const OUTER_BUDGET_SECS: u64 = INNER_BUDGET_SECS + 30;

/// Called only after the installer holds and verifies the installed process.
/// No credentials are placed on the command line or returned to PowerShell.
pub async fn authenticated_shutdown() -> Result<(), String> {
    let root = std::env::var_os("NEXUS_INSTALL_AUTH_ROOT").ok_or("Missing installer data root")?;
    let paths = nexus_core::NexusPaths::from_root(root.into());
    let record = paths.read_agent_discovery().map_err(|e| e.to_string())?.ok_or("Missing Agent discovery")?;
    let expected_pid: u32 = std::env::var("NEXUS_INSTALL_AUTH_PID").map_err(|e| e.to_string())?.parse().map_err(|_| "Invalid process id")?;
    let instance = std::env::var("NEXUS_INSTALL_AUTH_INSTANCE").map_err(|e| e.to_string())?;
    if record.pid != expected_pid || record.instance_id != instance { return Err("Agent changed during installer shutdown".into()); }
    let client = nexus_launcher_core::AgentClient::new(record.port).map_err(|e| e.to_string())?
        .with_credential_paths(paths)
        .with_expected_identity(nexus_launcher_core::AgentIdentity { data_root_id: record.data_root_id, instance_id: record.instance_id });
    let _: nexus_protocol::LifecycleAccepted = client.post_empty("/v1/shutdown").await.map_err(|e| e.to_string())?;
    Ok(())
}

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
            .env("NEXUS_INSTALL_STOP_BUDGET_MS", (INNER_BUDGET_SECS * 1000).to_string())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|error| format!("cannot run installer shutdown: {error}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(OUTER_BUDGET_SECS);
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
    fn run_installer_functions(script: &str, helper: Option<&std::path::Path>) {
        let source = include_str!("installer_shutdown.ps1");
        let functions = source.split("\ntry {").next().unwrap();
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = std::process::Command::new(powershell);
        command.args(["-NoProfile", "-NonInteractive", "-Command", &format!("{functions}\n{script}")]);
        if let Some(helper) = helper {
            command.env("NEXUS_INSTALL_STOP_HELPER", helper);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }

    #[cfg(windows)]
    #[test]
    fn msi_tmp_helper_executes_without_file_associations_and_preserves_errors() {
        let directory = std::env::temp_dir().join(format!("nexus-msi-helper-test-{}-{}",
            std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&directory).unwrap();
        let helper = directory.join("MSI helper.tmp");
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
        std::fs::copy(cmd, &helper).unwrap();
        run_installer_functions(r#"
$result = Invoke-InstallerHelper '/d /c echo installer-parser-probe'
if ($result.Trim() -cne 'installer-parser-probe') { throw 'Missing helper stdout' }
$failure = $null
try { $null = Invoke-InstallerHelper '/d /c echo original-helper-error 1>&2 & exit /b 7' }
catch { $failure = $_.Exception.Message }
if (-not $failure -or -not $failure.Contains('code 7') -or -not $failure.Contains('original-helper-error')) { throw 'Helper failure was not preserved' }
"#, Some(&helper));
        std::fs::remove_file(&helper).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn installer_defers_only_the_exact_console_owner_entrypoint() {
        run_installer_functions(r#"
if (-not (Test-HarnessCommandOwner @('agent.exe', '--harness-command', 'node.exe'))) { throw 'Owner not recognized' }
foreach ($arguments in @(
    @('agent.exe', '--data-dir', 'C:\data', '--instance-id', 'id'),
    @('agent.exe', '--data-dir', '--harness-command'),
    @('agent.exe', '--harness-command'),
    @('agent.exe', '--HARNESS-COMMAND', 'node.exe'),
    @('agent.exe', '--signal-console', '123', '456')
)) {
    if (Test-HarnessCommandOwner $arguments) { throw 'Non-owner was incorrectly deferred' }
}
"#, None);
    }
    #[cfg(windows)]
    #[test]
    fn installer_helpers_have_per_call_and_shared_deadlines_and_are_reaped() {
        assert!(OUTER_BUDGET_SECS >= INNER_BUDGET_SECS + 30);
        let directory=std::env::temp_dir().join(format!("nexus-helper-budget-{}",nexus_core::unix_time_nanos_for_update()));
        std::fs::create_dir(&directory).unwrap();
        let helper=directory.join("budget-helper.exe");
        std::fs::copy(PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe"),&helper).unwrap();
        run_installer_functions(r#"
$failed = $false
try { $null = Invoke-InstallerHelper '/d /c for /L %i in (0,0,1) do @rem hold' $env:NEXUS_INSTALL_STOP_HELPER 150 }
catch { $failed = $_.Exception.Message.Contains('execution deadline') }
if (-not $failed) { throw 'Helper did not time out' }
# The executable cannot be removed while a timed-out helper still owns it.
[IO.File]::Delete($env:NEXUS_INSTALL_STOP_HELPER)
$script:InstallerBudgetMs = [int]$script:InstallerClock.ElapsedMilliseconds + 100
if ((Remaining-InstallerMilliseconds 45000) -gt 100) { throw 'Per-call budget ignored overall budget' }
[Threading.Thread]::Sleep(110)
$failed = $false
try { $null = Invoke-InstallerHelper '--build-identity' 'must-not-start.exe' 10000 }
catch { $failed = $_.Exception.Message.Contains('overall deadline') }
if (-not $failed) { throw 'Expired overall deadline launched another helper' }
"#,Some(&helper));
        std::fs::remove_dir(directory).unwrap();
    }

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
