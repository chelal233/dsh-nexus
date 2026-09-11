//! Launcher-side evidence collection, usable while Agent is unavailable.
use std::{fs, io::{self, Read, Seek, SeekFrom}, path::{Path, PathBuf}, time::{Duration, Instant}};
use nexus_core::{NexusPaths, path_is_reparse, redact_diagnostics_payload};
use serde_json::{json, Value};
const FILE_LIMIT: usize = 256 * 1024;

fn read_source(path: &Path, tail: bool) -> io::Result<(Vec<u8>, bool)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || path_is_reparse(&metadata) { return Err(io::Error::other("Not an ordinary file")); }
    let mut options = fs::OpenOptions::new(); options.read(true);
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || path_is_reparse(&metadata) { return Err(io::Error::other("File identity changed")); }
    let truncated = metadata.len() > FILE_LIMIT as u64;
    if truncated && tail { file.seek(SeekFrom::End(-(FILE_LIMIT as i64)))?; }
    let mut bytes = Vec::new(); file.take(FILE_LIMIT as u64).read_to_end(&mut bytes)?;
    if truncated && tail {
        // Drop a partial first line; protect PEM bodies whose BEGIN is outside the tail.
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') { bytes.drain(..=newline); }
    }
    Ok((bytes, truncated))
}

/// Reads a fixed allowlist only. Does not settle transactions or contact Agent.
pub fn export(paths: &NexusPaths, context: Value) -> io::Result<PathBuf> {
    let started = Instant::now();
    let mut files = serde_json::Map::new();
    let mut failures = serde_json::Map::new();
    let root_ok = fs::symlink_metadata(&paths.root).is_ok_and(|m| m.is_dir() && !path_is_reparse(&m));
    let candidates = [
        ("config.json", false), ("cold-operation.json", false), ("install-operation.json", false),
        ("release-pointers.json", false), ("run/agent.json", false),
        ("run/harness-log-session.json", false), ("run/harness-recovery.json", false),
        ("run/last-capture.json", false), ("compatibility/latest.json", false),
        ("logs/agent.stdout.log", true), ("logs/agent.stderr.log", true),
    ];
    for (relative, tail) in candidates {
        if started.elapsed() > Duration::from_secs(5) {
            failures.insert("budget".into(), json!("Collection time budget reached")); break;
        }
        let path = paths.root.join(relative);
        let parent_ok = path.parent().is_some_and(|parent| fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir() && !path_is_reparse(&m)));
        if !root_ok || !parent_ok { failures.insert(relative.into(), json!("Directory missing, inaccessible or linked")); continue; }
        match read_source(&path, tail) {
            Ok((bytes, truncated)) => {
                // Incomplete JSON cannot be safely interpreted as complete configuration.
                if truncated && !tail { failures.insert(relative.into(), json!("File exceeds diagnostic limit")); continue; }
                let (bounded, omitted) = if truncated && tail { nexus_core::omit_truncated_private_key_prefix(&bytes) } else { (bytes.as_slice(), false) };
                let (safe, redacted) = redact_diagnostics_payload(bounded);
                let redacted = redacted || omitted;
                files.insert(relative.into(), json!({"text":String::from_utf8_lossy(&safe),"truncated":truncated,"redacted":redacted}));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => { failures.insert(relative.into(), json!({"kind":format!("{:?}",error.kind()),"os_code":error.raw_os_error()})); }
        }
    }
    let safe_context = redact_diagnostics_payload(&serde_json::to_vec(&context)?).0;
    let document = json!({"schema_version":1,"kind":"launcher_startup","created_at_unix":nexus_core::unix_time_seconds(),
        "context":serde_json::from_slice::<Value>(&safe_context).unwrap_or(Value::Null),
        "files":files,"unavailable":failures});
    let bytes = serde_json::to_vec_pretty(&document)?;
    let destination = std::env::temp_dir().join(format!("nexus-startup-diagnostics-{}.json",nexus_core::agent_auth::random_hex()?));
    nexus_core::write_private_bytes_atomic(&std::env::temp_dir(), &destination, &bytes)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_tail_omits_private_key_prefix_even_with_invalid_utf8() {
        let root = std::env::temp_dir().join(format!("nexus-tail-test-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone()); paths.ensure_directories().unwrap();
        let mut log = b"-----BEGIN PRIVATE KEY-----\n".to_vec();
        for _ in 0..12000 { log.extend_from_slice(b"PRIVATE_BASE64_SENTINEL\n"); }
        log.extend_from_slice(b"\xff\n-----END PRIVATE KEY-----\nCURRENT_FAILURE\n");
        fs::write(paths.logs_dir.join("agent.stderr.log"), log).unwrap();
        let destination = export(&paths, json!({})).unwrap();
        let output = fs::read_to_string(&destination).unwrap();
        assert!(output.contains("CURRENT_FAILURE"));
        assert!(!output.contains("PRIVATE_BASE64_SENTINEL"));
        fs::remove_file(destination).unwrap(); fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn offline_export_keeps_current_failure_and_redacts_without_reading_credentials() {
        let root = std::env::temp_dir().join(format!("nexus-offline-diagnostic-test-{}",nexus_core::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone());
        paths.ensure_directories().unwrap();
        fs::write(&paths.config_file, br#"{"api_key":"CONFIG_SECRET","harness_preferences":{"home":"C:/data"}}"#).unwrap();
        fs::write(paths.run_dir.join("agent-credential.json"), "CREDENTIAL_MUST_NOT_APPEAR").unwrap();
        let mut log = vec![b'x'; FILE_LIMIT + 32]; log.extend_from_slice(b"\nCURRENT_FAILURE\napi_key=LOG_SECRET\n");
        fs::write(paths.logs_dir.join("agent.stderr.log"), log).unwrap();
        let destination = export(&paths, json!({"buildId":"test-build","startup_error":null,
            "observed_agent_error":"Agent is not responding at http://127.0.0.1:12345\ntoken=OBSERVED_SECRET"})).unwrap();
        let output = fs::read_to_string(&destination).unwrap();
        assert!(output.contains("CURRENT_FAILURE") && output.contains("test-build"));
        assert!(output.contains("Agent is not responding"), "export must preserve the current observed failure after a successful startup");
        for secret in ["CONFIG_SECRET","LOG_SECRET","CREDENTIAL_MUST_NOT_APPEAR", "OBSERVED_SECRET"] { assert!(!output.contains(secret)); }
        assert_eq!(fs::read_to_string(paths.run_dir.join("agent-credential.json")).unwrap(), "CREDENTIAL_MUST_NOT_APPEAR");
        fs::remove_file(destination).unwrap(); fs::remove_dir_all(root).unwrap();
    }
}
