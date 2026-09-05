//! Read-only runtime environment observation.
//!
//! Runtime discovery never installs, downloads, writes configuration, changes
//! PATH, or starts a Harness. It only probes already existing executables so
//! the UI can show an honest preflight result before a later install flow.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use nexus_core::NexusPaths;
use nexus_protocol::{RuntimeListResponse, RuntimeToolStatus};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::timeout,
};

const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_SHIM_BYTES: usize = 16 * 1024;
const MAX_PORTABLE_RUNTIME_ENTRIES: usize = 64;

const REASON_NOT_FOUND: &str = "not_found";
const REASON_PROBE_CWD_UNAVAILABLE: &str = "probe_cwd_unavailable";
const REASON_SHIM_UNREADABLE: &str = "shim_unreadable";
const REASON_COREPACK_SHIM_UNVERIFIED: &str = "corepack_shim_unverified";
const REASON_UNSAFE_CMD_PATH: &str = "unsafe_cmd_path";
const REASON_UNSUPPORTED_SHIM: &str = "unsupported_shim";
const REASON_PROBE_FAILED: &str = "probe_failed";
const REASON_INVALID_VERSION_OUTPUT: &str = "invalid_version_output";

const COREPACK_PROBE_ENV: &[(&str, &str)] = &[
    // Do not let a Corepack-backed command reach a registry during discovery.
    ("COREPACK_ENABLE_NETWORK", "0"),
    // Do not wait for a download prompt or accept an implicit latest manager.
    ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
    ("COREPACK_DEFAULT_TO_LATEST", "0"),
    ("COREPACK_ENABLE_AUTO_PIN", "0"),
    // Ignore package.json packageManager resolution in the probe cwd.
    ("COREPACK_ENABLE_PROJECT_SPEC", "0"),
];

/// Tools required by the git-upstream Harness workflow.
pub const RUNTIME_TOOLS: &[&str] = &["git", "node", "pnpm"];

#[derive(Debug, Clone)]
struct ProbeConfig {
    search_path: Option<OsString>,
    data_root: PathBuf,
    data_root_is_safe: bool,
    portable_root: PathBuf,
    probe_cwd: Option<PathBuf>,
}

struct ProbeFailure {
    source: &'static str,
    path: String,
    reason: &'static str,
}

impl ProbeConfig {
    fn from_paths(paths: &NexusPaths) -> Self {
        let data_root_is_safe = fs::symlink_metadata(&paths.root)
            .map(|metadata| !is_reparse_point(&metadata))
            .unwrap_or(false);
        let data_root = fs::canonicalize(&paths.root).unwrap_or_else(|_| paths.root.clone());
        Self {
            search_path: env::var_os("PATH"),
            data_root,
            data_root_is_safe,
            portable_root: paths.root.join("runtimes"),
            probe_cwd: select_probe_cwd(),
        }
    }
}

/// Observe every known runtime tool. Results are ordered as `RUNTIME_TOOLS`.
pub async fn observe_runtimes(paths: &NexusPaths) -> RuntimeListResponse {
    let config = ProbeConfig::from_paths(paths);
    let (git, node, pnpm) = tokio::join!(
        observe_tool("git", config.clone()),
        observe_tool("node", config.clone()),
        observe_tool("pnpm", config),
    );
    let tools = vec![git, node, pnpm];
    debug_assert_eq!(tools.len(), RUNTIME_TOOLS.len());
    RuntimeListResponse::new(tools)
}

async fn observe_tool(name: &str, config: ProbeConfig) -> RuntimeToolStatus {
    let mut first_failure = None;
    for path in system_candidates(name, config.search_path.as_deref()) {
        match probe_path(name, &path, config.probe_cwd.as_deref()).await {
            Ok(version) => return available_status(name, version, "system", &path),
            Err(reason) => record_failure(&mut first_failure, "system", &path, reason),
        }
    }

    // Git is guided toward a user-managed installation. Nexus-owned portable
    // runtimes are intentionally limited to Node and pnpm in this phase.
    if name != "git" {
        for path in portable_candidates(name, &config) {
            match probe_path(name, &path, config.probe_cwd.as_deref()).await {
                Ok(version) => return available_status(name, version, "nexus", &path),
                Err(reason) => record_failure(&mut first_failure, "nexus", &path, reason),
            }
        }
    }

    unavailable_status(name, first_failure)
}

fn record_failure(
    first_failure: &mut Option<ProbeFailure>,
    source: &'static str,
    path: &Path,
    reason: &'static str,
) {
    if first_failure.is_none() {
        *first_failure = Some(ProbeFailure {
            source,
            path: display_path(path),
            reason,
        });
    }
}

fn available_status(name: &str, version: String, source: &str, path: &Path) -> RuntimeToolStatus {
    RuntimeToolStatus {
        name: name.to_owned(),
        available: true,
        version: Some(version),
        source: Some(source.to_owned()),
        path: Some(display_path(path)),
        reason: None,
    }
}

fn unavailable_status(name: &str, failure: Option<ProbeFailure>) -> RuntimeToolStatus {
    let (source, path, reason) = match failure {
        Some(failure) => (
            Some(failure.source.to_owned()),
            Some(failure.path),
            Some(failure.reason.to_owned()),
        ),
        None => (None, None, Some(REASON_NOT_FOUND.to_owned())),
    };
    RuntimeToolStatus {
        name: name.to_owned(),
        available: false,
        version: None,
        source,
        path,
        reason,
    }
}

fn system_candidates(name: &str, search_path: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(search_path) = search_path else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for directory in env::split_paths(search_path) {
        for executable_name in executable_names(name) {
            let Some(path) = canonical_file(&directory.join(executable_name)) else {
                continue;
            };
            if !candidates.iter().any(|existing| existing == &path) {
                candidates.push(path);
            }
        }
    }
    candidates
}

fn portable_candidates(name: &str, config: &ProbeConfig) -> Vec<PathBuf> {
    if !config.data_root_is_safe {
        return Vec::new();
    }
    let Some(data_root) = canonical_directory(&config.data_root) else {
        return Vec::new();
    };
    let Some(portable_root) = canonical_portable_root(&data_root, &config.portable_root) else {
        return Vec::new();
    };

    let mut runtime_dirs = Vec::new();
    let Ok(entries) = fs::read_dir(&portable_root) else {
        return Vec::new();
    };
    for entry in entries.take(MAX_PORTABLE_RUNTIME_ENTRIES) {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            runtime_dirs.push(path);
        }
    }
    runtime_dirs.sort_by(|left, right| {
        left.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(
                &right
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase(),
            )
    });

    let mut candidates = Vec::new();
    let mut roots = Vec::with_capacity(runtime_dirs.len() + 1);
    roots.push(portable_root.clone());
    roots.extend(runtime_dirs);
    for runtime_dir in roots {
        for subdirectory in ["", "bin", "cmd", "node_modules/.bin"] {
            let directory = if subdirectory.is_empty() {
                runtime_dir.clone()
            } else {
                runtime_dir.join(subdirectory)
            };
            for executable_name in executable_names(name) {
                let Some(path) =
                    canonical_file_within(&portable_root, &directory.join(executable_name))
                else {
                    continue;
                };
                if !candidates.iter().any(|existing| existing == &path) {
                    candidates.push(path);
                }
            }
        }
    }
    candidates
}

fn executable_names(name: &str) -> Vec<&'static str> {
    if cfg!(windows) {
        match name {
            "git" => vec!["git.exe", "git.cmd"],
            "node" => vec!["node.exe", "node"],
            // .cmd is the normal Windows package-manager shim. Keep it ahead
            // of extensionless fallbacks so PATH lookup is deterministic.
            "pnpm" => vec!["pnpm.cmd", "pnpm.exe", "pnpm"],
            _ => Vec::new(),
        }
    } else {
        match name {
            "git" => vec!["git"],
            "node" => vec!["node", "nodejs"],
            "pnpm" => vec!["pnpm"],
            _ => Vec::new(),
        }
    }
}

async fn probe_path(
    name: &str,
    path: &Path,
    probe_cwd: Option<&Path>,
) -> Result<String, &'static str> {
    // A Corepack-generated .cmd can download a manager and mutate the user's
    // Corepack cache. We cannot prove it is a read-only observation, so skip
    // the shim. A later runtime provisioning phase may resolve it explicitly.
    match is_corepack_shim(path) {
        Some(true) => return Err(REASON_COREPACK_SHIM_UNVERIFIED),
        Some(false) => {}
        None => return Err(REASON_SHIM_UNREADABLE),
    }

    let probe_cwd = probe_cwd
        .filter(|path| is_safe_probe_cwd(path))
        .ok_or(REASON_PROBE_CWD_UNAVAILABLE)?;

    #[cfg(windows)]
    if is_cmd_or_bat_path(path) && !is_safe_cmd_path(path) {
        return Err(REASON_UNSAFE_CMD_PATH);
    }

    let mut command = version_command(path).ok_or(REASON_UNSUPPORTED_SHIM)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(probe_cwd)
        .kill_on_drop(true);
    apply_probe_environment(&mut command);

    let output = run_version_probe(command)
        .await
        .ok_or(REASON_PROBE_FAILED)?;
    parse_version(name, &output).ok_or(REASON_INVALID_VERSION_OUTPUT)
}

fn version_command(path: &Path) -> Option<Command> {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            if !is_safe_cmd_path(path) {
                return None;
            }
            let mut command = Command::new("cmd.exe");
            // `call` is required for a batch file. The path has already been
            // checked by is_safe_cmd_path: cmd.exe cannot safely carry shell
            // metacharacters through this /c form, so such candidates are
            // reported unavailable instead of being invoked.
            command.args(["/d", "/s", "/c", "call"]);
            // cmd.exe does not understand the Win32 verbatim `\\?\\` prefix
            // returned by canonicalize, while the child executable does.
            command.arg(display_path(path));
            command.arg("--version");
            return Some(command);
        }
        // PowerShell shims need a shell policy decision and are not the
        // common executable form needed by this read-only preflight.
        if extension.as_deref() == Some("ps1") {
            return None;
        }
    }

    let mut command = Command::new(path);
    command.arg("--version");
    Some(command)
}

fn apply_probe_environment(command: &mut Command) {
    for (name, value) in COREPACK_PROBE_ENV {
        command.env(name, value);
    }
    // Keep npm/pnpm from reading or writing the user's userconfig while they
    // answer --version. This points only at the platform null device.
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .env("NPM_CONFIG_USERCONFIG", null_device)
        .env("npm_config_userconfig", null_device)
        .env("npm_config_update_notifier", "false")
        .env("CI", "1");

    #[cfg(windows)]
    {
        // Keep a console-subsystem probe from opening a visible console next
        // to the native launcher.
        command.creation_flags(0x0800_0000);
    }
}

async fn run_version_probe(mut command: Command) -> Option<Vec<u8>> {
    let mut child = command.spawn().ok()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            stop_child(&mut child).await;
            return None;
        }
    };

    let result = timeout(TOOL_PROBE_TIMEOUT, async {
        let mut bytes = Vec::new();
        let mut limited = stdout.take((MAX_PROBE_OUTPUT_BYTES + 1) as u64);
        let read = limited.read_to_end(&mut bytes).await;
        if read.is_err() || bytes.len() > MAX_PROBE_OUTPUT_BYTES {
            return None;
        }
        match child.wait().await {
            Ok(status) if status.success() => Some(bytes),
            _ => None,
        }
    })
    .await;

    let output = result.ok().flatten();
    if output.is_none() {
        stop_child(&mut child).await;
    }
    output
}

async fn stop_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = timeout(CHILD_CLEANUP_TIMEOUT, child.wait()).await;
}

fn parse_version(name: &str, output: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?;
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    match name {
        "git" => {
            let value = line.strip_prefix("git version ")?;
            let value = value.split_whitespace().next()?;
            valid_dot_version(value, true).then(|| line.to_owned())
        }
        "node" => {
            let value = line.strip_prefix('v')?;
            valid_node_version(value).then(|| line.to_owned())
        }
        "pnpm" => valid_dot_version(line, false).then(|| line.to_owned()),
        _ => None,
    }
}

fn valid_node_version(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(core, build)| (core, Some(build)));
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    let segments = core.split('.').collect::<Vec<_>>();
    segments.len() == 3
        && segments.iter().all(|segment| {
            !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
        })
        && prerelease.map_or(true, valid_version_suffix)
        && build.map_or(true, valid_version_suffix)
}

fn valid_dot_version(value: &str, allow_labels: bool) -> bool {
    if allow_labels {
        let mut segments = value.split('.');
        let numeric = segments.by_ref().take(3).collect::<Vec<_>>();
        numeric.len() == 3
            && numeric.iter().all(|segment| {
                !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
            })
            && segments.all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
            })
    } else {
        let (without_build, build) = value
            .split_once('+')
            .map_or((value, None), |(core, build)| (core, Some(build)));
        let (core, prerelease) = without_build
            .split_once('-')
            .map_or((without_build, None), |(core, prerelease)| {
                (core, Some(prerelease))
            });
        let segments = core.split('.').collect::<Vec<_>>();
        segments.len() == 3
            && segments.iter().all(|segment| {
                !segment.is_empty() && segment.chars().all(|character| character.is_ascii_digit())
            })
            && prerelease.map_or(true, valid_version_suffix)
            && build.map_or(true, valid_version_suffix)
    }
}

fn valid_version_suffix(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

fn is_corepack_shim(path: &Path) -> Option<bool> {
    #[cfg(not(windows))]
    {
        let _ = path;
        return Some(false);
    }

    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("cmd" | "bat")) {
            return Some(false);
        }
        let contents = read_bounded(path, MAX_SHIM_BYTES)?;
        let contents = std::str::from_utf8(&contents).ok()?.to_ascii_lowercase();
        Some(contents.contains("corepack"))
    }
}

#[cfg(windows)]
fn is_cmd_or_bat_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "cmd" | "bat"))
}

fn is_safe_cmd_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    !text.is_empty()
        && !text.chars().any(|character| {
            matches!(
                character,
                '&' | '|' | '<' | '>' | '^' | '%' | '!' | '(' | ')' | '"' | '\r' | '\n'
            )
        })
}

fn canonical_file(path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    fs::metadata(&path).ok()?.is_file().then_some(path)
}

fn canonical_directory(path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    fs::metadata(&path).ok()?.is_dir().then_some(path)
}

fn canonical_portable_root(data_root: &Path, portable_root: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(portable_root).ok()?;
    if is_reparse_point(&metadata) {
        return None;
    }
    let portable_root = canonical_directory(portable_root)?;
    if portable_root == data_root || !is_within(data_root, &portable_root) {
        return None;
    }
    Some(portable_root)
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        // FILE_ATTRIBUTE_REPARSE_POINT. This also rejects directory junctions
        // whose target happens to remain under the data root.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn canonical_file_within(root: &Path, path: &Path) -> Option<PathBuf> {
    let path = canonical_file(path)?;
    is_within(root, &path).then_some(path)
}

fn is_within(root: &Path, path: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = display_path(root)
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase();
        let path = display_path(path).to_ascii_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with(['\\', '/']))
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        return rest.to_owned();
    }
    text.into_owned()
}

fn select_probe_cwd() -> Option<PathBuf> {
    let candidate = env::temp_dir();
    is_safe_probe_cwd(&candidate)
        .then(|| fs::canonicalize(candidate).ok())
        .flatten()
}

fn is_safe_probe_cwd(path: &Path) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    if !path.is_dir() {
        return false;
    }
    // Corepack walks upward from cwd when looking for packageManager. Check
    // the complete ancestor chain, while touching only three known filenames.
    for ancestor in path.ancestors() {
        for manifest in ["package.json", "pnpm-workspace.yaml", "pnpm-workspace.yml"] {
            if ancestor.join(manifest).is_file() {
                return false;
            }
        }
    }
    true
}

fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture_root(label: &str) -> PathBuf {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nexus-runtime-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(root.join("data/runtimes")).expect("fixture directories create");
        fs::create_dir_all(root.join("cwd")).expect("fixture cwd creates");
        root
    }

    fn fixture_name(name: &str) -> &'static str {
        if cfg!(windows) {
            match name {
                "pnpm" => "pnpm.cmd",
                "git" => "git.exe",
                _ => "node.exe",
            }
        } else {
            match name {
                "git" => "git",
                "pnpm" => "pnpm",
                _ => "node",
            }
        }
    }

    fn write_version_fixture(directory: &Path, name: &str, output: &str) -> PathBuf {
        fs::create_dir_all(directory).expect("fixture directory creates");
        let path = directory.join(fixture_name(name));
        if cfg!(windows) {
            fs::write(
                &path,
                format!("@echo off\r\necho {output}\r\nexit /b 0\r\n"),
            )
            .expect("windows fixture writes");
        } else {
            fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"))
                .expect("unix fixture writes");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .expect("unix fixture is executable");
            }
        }
        path
    }

    fn config_for(root: &Path, search_dir: Option<&Path>) -> ProbeConfig {
        let data_root = fs::canonicalize(root.join("data")).expect("data root canonicalizes");
        let probe_cwd = fs::canonicalize(root.join("cwd")).expect("probe cwd canonicalizes");
        let search_path = search_dir.map(|directory| {
            env::join_paths([directory.as_os_str()]).expect("fixture path encodes")
        });
        ProbeConfig {
            search_path,
            data_root,
            data_root_is_safe: true,
            portable_root: root.join("data/runtimes"),
            probe_cwd: Some(probe_cwd),
        }
    }

    #[tokio::test]
    async fn missing_tool_is_unavailable_without_error() {
        let root = fixture_root("missing");
        let config = config_for(&root, None);
        let status = observe_tool("node", config).await;
        assert!(!status.available);
        assert!(status.version.is_none());
        assert!(status.source.is_none());
        assert_eq!(status.reason.as_deref(), Some(REASON_NOT_FOUND));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn first_valid_system_source_wins_over_portable_runtime() {
        let root = fixture_root("priority");
        let system_dir = root.join("system");
        let portable_dir = root.join("data/runtimes/pnpm-v1");
        let system = write_version_fixture(&system_dir, "pnpm", "10.1.0");
        write_version_fixture(&portable_dir, "pnpm", "9.2.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.version.as_deref(), Some("10.1.0"));
        let expected_path = display_path(&fs::canonicalize(system).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn portable_runtime_is_discovered_only_inside_data_root() {
        let root = fixture_root("portable");
        let portable = root.join("data/runtimes/pnpm-v1");
        let path = write_version_fixture(&portable, "pnpm", "10.3.0");
        let config = config_for(&root, None);
        let status = observe_tool("pnpm", config).await;
        assert_eq!(status.source.as_deref(), Some("nexus"));
        assert_eq!(status.version.as_deref(), Some("10.3.0"));
        let expected_path = display_path(&fs::canonicalize(path).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn malformed_stdout_is_unavailable() {
        let root = fixture_root("malformed");
        let system_dir = root.join("system");
        write_version_fixture(&system_dir, "pnpm", "pnpm is not a version");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert!(status.version.is_none());
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_INVALID_VERSION_OUTPUT)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corepack_probe_environment_is_explicitly_non_networking() {
        assert_eq!(
            COREPACK_PROBE_ENV,
            [
                ("COREPACK_ENABLE_NETWORK", "0"),
                ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
                ("COREPACK_DEFAULT_TO_LATEST", "0"),
                ("COREPACK_ENABLE_AUTO_PIN", "0"),
                ("COREPACK_ENABLE_PROJECT_SPEC", "0"),
            ]
        );
        assert!(is_safe_probe_cwd(&std::env::temp_dir()));
    }

    #[test]
    fn project_manifest_probe_cwd_is_rejected() {
        let root = fixture_root("project-cwd");
        let cwd = root.join("cwd");
        fs::write(cwd.join("package.json"), b"{}\n").expect("project fixture writes");
        assert!(!is_safe_probe_cwd(&cwd));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn version_parser_rejects_empty_and_accepts_tool_formats() {
        assert!(parse_version("node", b"\n  ").is_none());
        assert!(parse_version("node", b"v24.19.0\n").is_some());
        assert!(parse_version("pnpm", b"10.15.0\n").is_some());
        assert!(parse_version("git", b"git version 2.51.0.windows.1\n").is_some());
        assert!(parse_version("node", b"node version unknown\n").is_none());
    }

    #[test]
    fn cmd_paths_with_shell_metacharacters_are_rejected_explicitly() {
        assert!(is_safe_cmd_path(Path::new(
            r"C:\Program Files\nodejs\pnpm.cmd"
        )));
        for path in [
            r"C:\tool&escape\pnpm.cmd",
            r"C:\tool|escape\pnpm.cmd",
            r"C:\tool<escape\pnpm.cmd",
            r"C:\tool>escape\pnpm.cmd",
            r"C:\tool^escape\pnpm.cmd",
            r"C:\tool%escape\pnpm.cmd",
            r"C:\tool!escape\pnpm.cmd",
            r"C:\tool(escape)\pnpm.cmd",
        ] {
            assert!(
                !is_safe_cmd_path(Path::new(path)),
                "unsafe path accepted: {path}"
            );
        }
        #[cfg(windows)]
        assert!(version_command(Path::new(r"C:\tool&escape\pnpm.cmd")).is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cmd_pnpm_shim_is_invoked_through_cmd() {
        let root = fixture_root("windows-shim");
        let system_dir = root.join("system");
        let path = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("10.15.0"));
        let expected_path = display_path(&fs::canonicalize(path).unwrap());
        assert_eq!(status.path.as_deref(), Some(expected_path.as_str()));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn corepack_cmd_shim_is_unavailable_without_running_it() {
        let root = fixture_root("corepack-shim");
        let system_dir = root.join("system");
        let path = system_dir.join("pnpm.cmd");
        fs::create_dir_all(&system_dir).expect("fixture directory creates");
        fs::write(
            &path,
            "@SETLOCAL\r\nnode \"%~dp0\\node_modules\\corepack\\dist\\pnpm.js\" %*\r\n",
        )
        .expect("corepack fixture writes");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        assert_eq!(
            status.reason.as_deref(),
            Some(REASON_COREPACK_SHIM_UNVERIFIED)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cmd_path_with_shell_metacharacter_is_not_invoked() {
        let root = fixture_root("unsafe-cmd-path");
        let system_dir = root.join("system&escape");
        let path = write_version_fixture(&system_dir, "pnpm", "10.15.0");
        let config = config_for(&root, Some(&system_dir));
        let status = observe_tool("pnpm", config).await;
        assert!(!status.available);
        assert_eq!(status.source.as_deref(), Some("system"));
        assert_eq!(status.reason.as_deref(), Some(REASON_UNSAFE_CMD_PATH));
        assert_eq!(
            status.path.as_deref(),
            Some(display_path(&fs::canonicalize(path).unwrap()).as_str())
        );
        let _ = fs::remove_dir_all(root);
    }
}
