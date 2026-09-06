//! Embedded Git runs in an owned Agent child, never an unkillable I/O thread.
use std::{ffi::OsString, fs, io, path::{Path, PathBuf}, process::{Command, Stdio}, time::{Duration, Instant}};
use git2::{AutotagOption, Direction, FetchOptions, Repository};
use nexus_core::{validate_update_ref, validate_update_source, write_json_atomic};
use nexus_runtime_supply::CancellationToken;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Operation {
    Tags { source: String },
    Clone { source: String, reference: String, candidate: PathBuf, parent: PathBuf },
    Head { candidate: PathBuf, parent: PathBuf },
}
#[derive(Serialize, Deserialize)]
struct Request { operation: Operation, output: PathBuf }

#[derive(Clone)]
pub(crate) struct ExternalGit { pub program: PathBuf, pub prefix: Vec<OsString> }
pub(crate) fn selected_external(runtime: &nexus_core::RuntimeConfig) -> Option<ExternalGit> {
    nexus_core::resolve_runtime_command(runtime, "git").ok().flatten()
        .map(|command| ExternalGit { program: command.program, prefix: command.prefix_args })
}

async fn external_run(operation: &Operation, external: &ExternalGit, directory: &Path, duration: Duration, cancellation: &CancellationToken) -> io::Result<serde_json::Value> {
    fs::create_dir_all(directory)?;
    let output = directory.join(format!("system-git-{}.stdout", nexus_core::unix_time_nanos_for_update()));
    let file = fs::OpenOptions::new().write(true).create_new(true).open(&output)?;
    let mut command = Command::new(&external.program);
    command.args(&external.prefix).stdin(Stdio::null()).stdout(file);
    match operation {
        Operation::Tags { source } => { command.args(["ls-remote", "--tags", source]); },
        Operation::Clone { source, reference, candidate, .. } => { command.args(["-c", "core.longpaths=true", "clone", "--no-tags", "--depth", "1", "--branch", reference, source]).arg(candidate); },
        Operation::Head { candidate, .. } => { command.args(["rev-parse", "--verify", "HEAD"]).current_dir(candidate); },
    }
    let result = crate::cold::run_owned_command(command, "system Git", duration, directory, cancellation).await;
    if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) { return result.map(|_| serde_json::Value::Null); }
    let bytes = if result.is_ok() { read_bounded(&output, 1024 * 1024) } else { Ok(Vec::new()) };
    let _ = fs::remove_file(output);
    result?;
    let bytes = bytes?;
    if bytes.len() > 1024 * 1024 { return Err(io::Error::other("System Git output exceeds limit")); }
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    match operation {
        Operation::Tags { .. } => {
            Ok(serde_json::json!(crate::updater::parse_ls_remote_tags(&text)))
        },
        Operation::Head { .. } => {
            let revision = text.trim();
            if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::other("System Git returned an invalid revision"));
            }
            Ok(serde_json::json!(revision.to_ascii_lowercase()))
        },
        Operation::Clone { .. } => Ok(serde_json::Value::Null),
    }
}

async fn run_preferred(operation: Operation, directory: &Path, duration: Duration, cancellation: &CancellationToken, external: Option<ExternalGit>) -> io::Result<serde_json::Value> {
    let start = Instant::now();
    if cancellation.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "Git operation cancelled")); }
    if let Operation::Clone { candidate, parent, .. } = &operation {
        candidate_in_parent(candidate, parent)?;
        if fs::symlink_metadata(candidate).is_ok() { return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Git candidate already exists")); }
    }
    let original = if let Some(external) = external {
        match external_run(&operation, &external, directory, duration, cancellation).await {
            Ok(value) => return Ok(value),
            Err(error) if cancellation.is_cancelled() || error.kind() == io::ErrorKind::TimedOut || error.kind() == io::ErrorKind::Interrupted || !crate::cold::command_owner_quiescent(&error) => return Err(error),
            Err(error) => Some(error),
        }
    } else { None };
    if cancellation.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "Git operation cancelled")); }
    if let Operation::Clone { candidate, parent, .. } = &operation {
        crate::cold::remove_owned_directory(parent, candidate)?;
    }
    let remaining = duration.checked_sub(start.elapsed()).filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Git operation exhausted its total timeout"))?;
    tracing::info!(system_error = ?original.as_ref().map(ToString::to_string), "using embedded Git fallback");
    match run(operation, directory, remaining, cancellation).await {
        Err(error) if crate::cold::command_owner_quiescent(&error) => {
            if let Some(original) = original { Err(io::Error::new(error.kind(), format!("System Git failed: {original}; embedded fallback failed: {error}"))) } else { Err(error) }
        },
        result => result,
    }
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return Err(io::Error::other("Git output exceeds limit")); }
    Ok(bytes)
}
fn git_error(error: git2::Error) -> io::Error { io::Error::other(format!("embedded Git: {}", error.message())) }
fn validate_source(source: &str) -> io::Result<()> {
    validate_update_source(source)?;
    if !source.starts_with("https://") { return Err(io::Error::other("Embedded Git requires an HTTPS upstream")); }
    Ok(())
}
fn candidate_in_parent(candidate: &Path, parent: &Path) -> io::Result<()> {
    let parent = fs::canonicalize(parent)?;
    let actual_parent = fs::canonicalize(candidate.parent().ok_or_else(|| io::Error::other("Missing candidate parent"))?)?;
    if parent != actual_parent || candidate.file_name().is_none() {
        return Err(io::Error::other("Git candidate escapes its owned parent"));
    }
    if let Ok(metadata) = fs::symlink_metadata(candidate) {
        #[cfg(windows)]
        { use std::os::windows::fs::MetadataExt;
          if metadata.file_attributes() & 0x400 != 0 { return Err(io::Error::other("Git candidate cannot be a reparse point")); } }
        if metadata.file_type().is_symlink() { return Err(io::Error::other("Git candidate cannot be a link")); }
    }
    Ok(())
}

fn tags(source: &str, directory: &Path) -> io::Result<Vec<String>> {
    let repository = Repository::init_bare(directory.join("advertise"))
        .map_err(git_error)?;
    let directory = repository.path().to_owned();
    let result = (|| {
        let mut remote = repository.remote_anonymous(source).map_err(git_error)?;
        remote.connect(Direction::Fetch).map_err(git_error)?;
        let mut tags: Vec<String> = remote.list().map_err(git_error)?.iter()
            .filter_map(|head| head.name().strip_prefix("refs/tags/"))
            .filter(|tag| !tag.ends_with("^{}") && validate_update_ref(tag).is_ok())
            .map(str::to_owned).collect();
        tags.sort(); tags.dedup(); tags.reverse();
        Ok(tags)
    })();
    drop(repository);
    let _ = fs::remove_dir_all(directory);
    result
}

fn clone_ref(source: &str, reference: &str, candidate: &Path) -> io::Result<String> {
    validate_update_ref(reference)?;
    if candidate.exists() { return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Git candidate already exists")); }
    let repository = Repository::init(candidate).map_err(git_error)?;
    repository.config().map_err(git_error)?.set_bool("core.longpaths", true).map_err(git_error)?;
    let mut remote = repository.remote("origin", source).map_err(git_error)?;
    remote.connect(Direction::Fetch).map_err(git_error)?;
    let tag = format!("refs/tags/{reference}");
    let branch = format!("refs/heads/{reference}");
    let selected = if remote.list().map_err(git_error)?.iter().any(|head| head.name() == tag) { tag }
        else if remote.list().map_err(git_error)?.iter().any(|head| head.name() == branch) { branch }
        else { return Err(io::Error::new(io::ErrorKind::NotFound, "Requested Git ref is not advertised")); };
    remote.disconnect().map_err(git_error)?;
    let mut fetch = FetchOptions::new();
    // Production workers accept HTTPS only; local fixture transport does not
    // implement shallow fetch in libgit2.
    if source.starts_with("https://") { fetch.depth(1); }
    fetch.download_tags(AutotagOption::None);
    remote.fetch(&[format!("+{selected}:{selected}")], Some(&mut fetch), None).map_err(git_error)?;
    let commit = repository.find_reference(&selected).map_err(git_error)?.peel_to_commit().map_err(git_error)?;
    repository.set_head_detached(commit.id()).map_err(git_error)?;
    repository.checkout_head(Some(git2::build::CheckoutBuilder::new().force())).map_err(git_error)?;
    Ok(commit.id().to_string())
}

/// Private worker entry; called before constructing the Agent HTTP runtime.
pub fn execute_request(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
        return Err(io::Error::other("Invalid Git worker request"));
    }
    let request: Request = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    let directory = fs::canonicalize(path.parent().ok_or_else(|| io::Error::other("Missing request directory"))?)?;
    if fs::canonicalize(request.output.parent().ok_or_else(|| io::Error::other("Missing output directory"))?)? != directory {
        return Err(io::Error::other("Git output escapes request directory"));
    }
    let value = match request.operation {
        Operation::Tags { source } => { validate_source(&source)?; serde_json::json!(tags(&source, &directory)?) }
        Operation::Clone { source, reference, candidate, parent } => {
            validate_source(&source)?; candidate_in_parent(&candidate, &parent)?;
            serde_json::json!(clone_ref(&source, &reference, &candidate)?)
        }
        Operation::Head { candidate, parent } => {
            candidate_in_parent(&candidate, &parent)?;
            let repository = Repository::open(&candidate).map_err(git_error)?;
            let revision = repository.head().map_err(git_error)?.peel_to_commit().map_err(git_error)?.id().to_string();
            serde_json::json!(revision)
        }
    };
    write_json_atomic(&directory, &request.output, &value)
}

async fn run(operation: Operation, directory: &Path, duration: Duration, cancellation: &CancellationToken) -> io::Result<serde_json::Value> {
    fs::create_dir_all(directory)?;
    let nonce = nexus_core::unix_time_nanos_for_update();
    let parent = directory;
    let owned = directory.join(format!("embedded-git-{nonce}"));
    fs::create_dir(&owned)?;
    let directory = owned.as_path();
    let input = directory.join(format!("git-request-{nonce}.json"));
    let output = directory.join(format!("git-result-{nonce}.json"));
    write_json_atomic(directory, &input, &Request { operation, output: output.clone() })?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--git-worker").arg(&input).stdin(Stdio::null()).stdout(Stdio::null());
    let result = crate::cold::run_owned_command(command, "embedded Git", duration, directory, cancellation).await;
    if result.as_ref().err().is_some_and(|error| !crate::cold::command_owner_quiescent(error)) { return result.map(|_| serde_json::Value::Null); }
    let data = if result.is_ok() { read_bounded(&output, 1024 * 1024) } else { Ok(Vec::new()) };
    let _ = fs::remove_file(input); let _ = fs::remove_file(output);
    crate::cold::remove_owned_directory(parent, directory)?;
    result?;
    let data = data?;
    if data.len() > 1024 * 1024 { return Err(io::Error::other("Git result exceeds limit")); }
    serde_json::from_slice(&data).map_err(io::Error::other)
}

pub(crate) async fn list_tags(source: &str, directory: &Path, duration: Duration, external: Option<ExternalGit>) -> io::Result<Vec<String>> {
    validate_source(source)?;
    let source = source.to_owned(); let directory = directory.to_owned();
    tokio::spawn(async move {
        serde_json::from_value(run_preferred(Operation::Tags { source }, &directory, duration, &CancellationToken::default(), external).await?).map_err(io::Error::other)
    }).await.map_err(io::Error::other)?
}
pub(crate) async fn clone_candidate(source: &str, reference: &str, candidate: &Path, directory: &Path, duration: Duration, cancellation: &CancellationToken, external: Option<ExternalGit>) -> io::Result<()> {
    validate_source(source)?; validate_update_ref(reference)?;
    let parent = candidate.parent().ok_or_else(|| io::Error::other("Missing candidate parent"))?.to_owned();
    run_preferred(Operation::Clone { source: source.to_owned(), reference: reference.to_owned(), candidate: candidate.to_owned(), parent }, directory, duration, cancellation, external).await?;
    Ok(())
}
pub(crate) async fn head(candidate: &Path, directory: &Path, cancellation: &CancellationToken, external: Option<ExternalGit>) -> io::Result<String> {
    let parent = candidate.parent().ok_or_else(|| io::Error::other("Missing candidate parent"))?.to_owned();
    serde_json::from_value(run_preferred(Operation::Head { candidate: candidate.to_owned(), parent }, directory, Duration::from_secs(30), cancellation, external).await?).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nexus-git-{label}-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir_all(&root).unwrap(); root
    }
    #[test]
    fn embedded_lists_refs_and_checks_out_requested_tag_and_head() {
        let root = root("refs");
        let source = root.join("source");
        let repository = Repository::init(&source).unwrap();
        fs::write(source.join("payload.txt"), "tagged").unwrap();
        let mut index = repository.index().unwrap(); index.add_path(Path::new("payload.txt")).unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let first = repository.commit(Some("HEAD"), &signature, &signature, "first", &tree, &[]).unwrap();
        let commit = repository.find_commit(first).unwrap();
        repository.tag_lightweight("v-test", commit.as_object(), false).unwrap();
        repository.commit(Some("HEAD"), &signature, &signature, "later", &tree, &[&commit]).unwrap();
        let source_url = source.to_string_lossy();
        assert_eq!(tags(&source_url, &root).unwrap(), vec!["v-test"]);
        let target = root.join("candidate");
        assert_eq!(clone_ref(&source_url, "v-test", &target).unwrap(), first.to_string());
        let cloned = Repository::open(&target).unwrap();
        assert_eq!(cloned.head().unwrap().target(), Some(first));
        assert_eq!(fs::read_to_string(target.join("payload.txt")).unwrap(), "tagged");
        assert!(clone_ref(&source_url, "v-test", &target).is_err());
        drop(cloned); drop(commit); drop(tree); drop(index); drop(repository);
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn pre_cancelled_git_never_starts_a_worker() {
        let root = root("cancel"); let cancellation = CancellationToken::default(); cancellation.cancel();
        let result = run_preferred(Operation::Tags { source: "https://example.invalid/repo".into() },
            &root, Duration::from_secs(1), &cancellation, None).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn system_git_timeout_does_not_launch_fallback() {
        let root = root("timeout");
        let script = root.join("sleep.cmd");
        fs::write(&script, "@echo off\r\nping -n 8 127.0.0.1 >nul\r\n").unwrap();
        let external = ExternalGit { program: PathBuf::from("cmd.exe"), prefix: vec!["/D".into(), "/C".into(), script.into_os_string()] };
        let result = run_preferred(Operation::Tags { source: "https://example.invalid/repo".into() },
            &root, Duration::from_millis(150), &CancellationToken::default(), Some(external)).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(!fs::read_dir(&root).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("embedded-git-")));
        fs::remove_dir_all(root).unwrap();
    }
}
