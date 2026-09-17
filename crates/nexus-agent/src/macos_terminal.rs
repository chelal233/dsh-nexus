//! Terminal.app launch files and a lease-before-exec handshake.
//! User paths are single shell arguments; no user shell files or global PATH edits.
use nexus_core::NexusPaths;
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) struct Options<'a> {
    pub node: &'a Path,
    pub entry: &'a Path,
    pub args: &'a [String],
    pub pnpm: Option<&'a Path>,
    pub pnpm_is_script: bool,
    pub profile_dir: &'a Path,
    pub env: &'a [(OsString, OsString)],
    pub notification_monitor: bool,
}

fn quote(value: &OsStr) -> io::Result<String> {
    let text = value
        .to_str()
        .ok_or_else(|| io::Error::other("Terminal paths must be valid UTF-8"))?;
    if text.contains('\0') {
        return Err(io::Error::other("Terminal argument contains NUL"));
    }
    Ok(format!("'{}'", text.replace('\'', "'\"'\"'")))
}

fn executable(root: &Path, path: &Path, content: &str) -> io::Result<()> {
    nexus_core::write_private_bytes_atomic(root, path, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn shim(program: &Path, args: &[OsString]) -> io::Result<String> {
    let mut script = format!("#!/bin/sh\nexec {}", quote(program.as_os_str())?);
    for arg in args {
        script.push(' ');
        script.push_str(&quote(arg)?);
    }
    script.push_str(" \"$@\"\n");
    Ok(script)
}

pub(crate) struct Prepared {
    directory: PathBuf,
    command: PathBuf,
    pid: PathBuf,
    ready: PathBuf,
    cleanup_on_drop: bool,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        // Only our exact generated files; never recursively delete a terminal cwd.
        for name in [
            "launch.command",
            "pid",
            "pid.tmp",
            "ready",
            "node",
            "npm",
            "pnpm",
            "dsh",
            "monitor.mjs",
        ] {
            let _ = fs::remove_file(self.directory.join(name));
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

fn prepare(paths: &NexusPaths, options: &Options<'_>) -> io::Result<Prepared> {
    paths.ensure_directories()?;
    let directory = paths.run_dir.join(format!(
        "mac-terminal-{}",
        nexus_core::agent_auth::random_hex()?
    ));
    nexus_private_file::create_new_private_directory(&directory)?;
    let prepared = Prepared {
        command: directory.join("launch.command"),
        pid: directory.join("pid"),
        ready: directory.join("ready"),
        directory,
        cleanup_on_drop: true,
    };
    let write = |name: &str, contents: &str| {
        executable(&paths.root, &prepared.directory.join(name), contents)
    };
    write("node", &shim(options.node, &[])?)?;
    let npm_parent = options
        .node
        .parent()
        .ok_or_else(|| io::Error::other("Node path has no parent"))?;
    let npm_entry = npm_parent.join("node_modules/npm/bin/npm-cli.js");
    write(
        "npm",
        &if npm_entry.is_file() {
            shim(options.node, &[npm_entry.into_os_string()])?
        } else {
            shim(&npm_parent.join("npm"), &[])?
        },
    )?;
    write(
        "pnpm",
        &match options.pnpm {
            Some(pnpm) if options.pnpm_is_script => {
                shim(options.node, &[pnpm.as_os_str().to_owned()])?
            }
            Some(pnpm) => shim(pnpm, &[])?,
            None => {
                "#!/bin/sh\nprintf '%s\\n' 'Nexus: pnpm runtime is not configured' >&2\nexit 127\n"
                    .into()
            }
        },
    )?;
    let mut args = vec![options.entry.as_os_str().to_owned()];
    args.extend(options.args.iter().map(OsString::from));
    write("dsh", &shim(options.node, &args)?)?;
    let pid = quote(prepared.pid.as_os_str())?;
    let ready = quote(prepared.ready.as_os_str())?;
    let pid_temp = quote(prepared.directory.join("pid.tmp").as_os_str())?;
    let mut script = format!(
        "#!/bin/sh\numask 077\nprintf '%s\\n' \"$$\" > {pid_temp}\n/bin/mv -f {pid_temp} {pid}\ncount=0\nwhile [ ! -f {ready} ]; do\n  count=$((count + 1))\n  if [ \"$count\" -ge 300 ]; then\n    printf '%s\\n' 'Nexus: terminal registration timed out; reopen from Launcher.' >&2\n    exit 1\n  fi\n  /bin/sleep 0.1\ndone\n/bin/rm -f {ready} {pid}\n"
    );
    for (key, value) in options.env {
        let key = key
            .to_str()
            .ok_or_else(|| io::Error::other("Invalid terminal environment key"))?;
        if key.is_empty()
            || key.bytes().enumerate().any(|(i, b)| {
                !(b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
            })
        {
            return Err(io::Error::other("Invalid terminal environment key"));
        }
        script.push_str(&format!("export {key}={}\n", quote(value)?));
    }
    script.push_str(&format!("export PATH={}:\"$PATH\"\ncd {} || exit 1\nprintf '%s\\n' 'Nexus DSH terminal: dsh / node / npm / pnpm'\n",
        quote(prepared.directory.as_os_str())?, quote(options.profile_dir.as_os_str())?));
    if options.notification_monitor {
        let monitor = prepared.directory.join("monitor.mjs");
        nexus_core::write_private_bytes_atomic(
            &paths.root,
            &monitor,
            include_bytes!("../../../plugins/nexus-notifications/terminal/monitor.mjs"),
        )?;
        script.push_str(&format!(
            "exec {} {} {} {}\n",
            quote(options.node.as_os_str())?,
            quote(monitor.as_os_str())?,
            quote(paths.run_dir.join("notifications.json").as_os_str())?,
            quote(paths.root.join("notification-settings.json").as_os_str())?
        ));
    } else {
        // -f bypasses user startup files so they cannot undo this session's pinned runtime.
        // exec retains the PID/creation identity already held by the terminal lease.
        script.push_str("exec /bin/zsh -f -i\n");
    }
    write("launch.command", &script)?;
    Ok(prepared)
}

#[cfg(target_os = "macos")]
pub(crate) async fn open(
    paths: &NexusPaths,
    release: &str,
    options: &Options<'_>,
) -> io::Result<()> {
    use std::{process::Stdio, time::Duration};
    let mut prepared = prepare(paths, options)?;
    let opened = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("/usr/bin/open")
            .args(["-a", "Terminal"])
            .arg(&prepared.command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| io::Error::other("Opening Terminal.app timed out"))??;
    if !opened.success() {
        return Err(io::Error::other(
            "Terminal.app did not accept the terminal session",
        ));
    }
    let pid = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(bytes) = nexus_core::read_regular_file_bounded(&prepared.pid, 32)? {
                let pid: u32 = std::str::from_utf8(&bytes)
                    .map_err(io::Error::other)?
                    .trim()
                    .parse()
                    .map_err(io::Error::other)?;
                if pid <= 1 {
                    return Err(io::Error::other("Invalid terminal process identity"));
                }
                return Ok::<u32, io::Error>(pid);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::other("Terminal.app did not start the session; reopen from Launcher")
    })??;
    let lease = nexus_core::terminal_lease::register(paths, release, pid)?;
    // If Agent dies before this durable grant, the inert script exits on timeout.
    // If it dies afterward, the lease still protects the release across restart.
    if let Err(e) = nexus_core::write_private_bytes_atomic(&paths.root, &prepared.ready, b"ready") {
        let _ = fs::remove_file(&lease);
        return Err(e);
    }
    // Agent shutdown cancels watchers, but a granted terminal may still be live.
    prepared.cleanup_on_drop = false;
    let paths = paths.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match nexus_core::terminal_lease::is_live(&paths, &lease) {
                Ok(false) => {
                    prepared.cleanup_on_drop = true;
                    drop(prepared);
                    break;
                }
                Ok(true) => {}
                Err(_) => break, // Uncertain liveness: preserve its shims.
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_arguments_round_trip_without_expansion() {
        let shell = if cfg!(windows) {
            PathBuf::from("C:/Program Files/Git/bin/bash.exe")
        } else {
            PathBuf::from("/bin/sh")
        };
        assert!(
            shell.is_file(),
            "POSIX shell required for terminal argument regression"
        );
        for value in [
            "",
            "space 中文 ' \" ; $HOME $(printf expanded) `printf expanded`\nline",
            "--profile",
            "a\\b",
        ] {
            let output = std::process::Command::new(&shell)
                .args([
                    "-c",
                    &format!("printf '%s' {}", quote(OsStr::new(value)).unwrap()),
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
        }
        assert!(quote(OsStr::new("nul\0value")).is_err());
    }

    #[test]
    fn granted_sessions_survive_watcher_disposal_and_monitor_is_packaged() {
        let root = std::env::temp_dir().join(format!(
            "nexus-mac-terminal-{}",
            nexus_core::agent_auth::random_hex().unwrap()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let env = vec![("DSH_HOME".into(), OsString::from("/tmp/user's home 中文"))];
        let options = Options {
            node: Path::new("/runtime/node"),
            entry: Path::new("/harness/bin.js"),
            args: &["--profile".into(), "web".into()],
            pnpm: None,
            pnpm_is_script: false,
            profile_dir: Path::new("/profile"),
            env: &env,
            notification_monitor: true,
        };
        let mut prepared = prepare(&paths, &options).unwrap();
        let directory = prepared.directory.clone();
        assert_eq!(
            fs::read(directory.join("monitor.mjs")).unwrap(),
            include_bytes!("../../../plugins/nexus-notifications/terminal/monitor.mjs")
        );
        let script = fs::read_to_string(&prepared.command).unwrap();
        assert!(!script.contains("exec /bin/zsh"));
        assert!(script.find("while [ ! -f").unwrap() < script.find("export DSH_HOME").unwrap());
        prepared.cleanup_on_drop = false;
        drop(prepared);
        assert!(
            directory.join("dsh").is_file(),
            "Agent watcher shutdown must not remove live shims"
        );
        let cleanup = Prepared {
            directory: directory.clone(),
            command: directory.join("launch.command"),
            pid: directory.join("pid"),
            ready: directory.join("ready"),
            cleanup_on_drop: true,
        };
        drop(cleanup);
        assert!(!directory.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_handshake_holds_release_before_running_profile_commands() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let root = std::env::temp_dir().join(format!(
            "nexus-mac-terminal-{}",
            nexus_core::agent_auth::random_hex().unwrap()
        ));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        let profile = root.join("profile ' 中文");
        fs::create_dir(&profile).unwrap();
        let entry = root.join("capture ' 中文.sh");
        fs::write(&entry, "printf '%s\\n' \"$DSH_HOME\" \"$PWD\" \"$@\"\n").unwrap();
        let env = vec![(
            "DSH_HOME".into(),
            OsString::from("/tmp/home ' $(printf BAD) 中文"),
        )];
        let args = vec![
            "--profile".into(),
            "web".into(),
            "--patch".into(),
            "patch ' $(printf BAD).json".into(),
        ];
        let options = Options {
            node: Path::new("/bin/sh"),
            entry: &entry,
            args: &args,
            pnpm: None,
            pnpm_is_script: false,
            profile_dir: &profile,
            env: &env,
            notification_monitor: false,
        };
        let prepared = prepare(&paths, &options).unwrap();
        let script = fs::read_to_string(&prepared.command)
            .unwrap()
            .replace("exec /bin/zsh -f -i", "exec dsh 'argument with spaces'");
        fs::write(&prepared.command, script).unwrap();
        let mut child = Command::new("/bin/sh")
            .arg(&prepared.command)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !prepared.pid.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(&prepared.pid).unwrap().trim(),
            child.id().to_string()
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "not allowed to execute before durable registration"
        );
        let lease = nexus_core::terminal_lease::register(&paths, "test-slot", child.id()).unwrap();
        assert!(nexus_core::terminal_lease::ensure_release_idle(&paths, "test-slot").is_err());
        fs::write(&prepared.ready, b"ready").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("/tmp/home ' $(printf BAD) 中文\n"));
        assert!(text.ends_with(
            "--profile\nweb\n--patch\npatch ' $(printf BAD).json\nargument with spaces\n"
        ));
        assert!(!nexus_core::terminal_lease::is_live(&paths, &lease).unwrap());
        nexus_core::terminal_lease::ensure_release_idle(&paths, "test-slot").unwrap();
        drop(prepared);
        fs::remove_dir_all(root).unwrap();
    }
}
