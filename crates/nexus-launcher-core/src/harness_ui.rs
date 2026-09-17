//! Safe, bounded observation of a Harness authentication URL in Agent-owned logs.
//!
//! The parser is shared by the legacy Launcher compatibility API and the
//! independent Agent endpoint. It never reads Harness source or process
//! arguments; only the current durable log-session files are considered.

use std::{
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use nexus_core::{log_file_identity, HarnessLogSession, HarnessLogSessionStore, NexusPaths};
use nexus_protocol::HarnessResponse;
use reqwest::Url;
use serde::{Deserialize, Serialize};

pub const HARNESS_LOG_TAIL_BYTES: u64 = 64 * 1024;
// Shared by readiness observers, the API observer and the retention worker.
// A single bounded critical section prevents punching out evidence another
// observer is in the process of validating/publishing.
static LOG_EVIDENCE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
const EVIDENCE_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessUiInfo {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Return the newest loopback Harness URL from a bounded log tail.
///
/// Harness remains an opaque upstream process. The launcher only observes the
/// text it already redirected to its own logs; it does not inspect `$HOME/.dsh`
/// or infer a URL from a process command line. A URL is considered usable only
/// when it is plain HTTP and loopback-bound. Token-bearing URLs are preferred
/// because readiness/health messages may also contain a loopback URL. The
/// observer keeps only a bounded in-memory cursor. An unchanged byte snapshot
/// reuses its candidate; every content/length change rescans the bounded tail
/// so a rotation cannot masquerade as an append merely because its sliding
/// overlap happens to match.
pub fn read_harness_ui_info(paths: &NexusPaths) -> HarnessUiInfo {
    let mut observer = HarnessLogObserver::default();
    match HarnessLogSessionStore::new(paths.clone()).read() {
        Ok(Some(session)) => {
            read_harness_ui_info_with_observer(paths, &mut observer, Some(&session))
        }
        Ok(None) => unavailable_harness_ui_info("Harness log session marker is not available"),
        Err(error) => {
            unavailable_harness_ui_info(format!("Harness log session marker is invalid: {error}"))
        }
    }
}

pub fn read_harness_ui_info_with_observer(
    paths: &NexusPaths,
    observer: &mut HarnessLogObserver,
    session: Option<&HarnessLogSession>,
) -> HarnessUiInfo {
    let Ok(_guard) = LOG_EVIDENCE_GATE.lock() else { return unavailable_harness_ui_info("Log evidence lock unavailable"); };
    let _file_guard = match acquire_evidence_lock(paths) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return unavailable_harness_ui_info("Log evidence is busy or unavailable; retry shortly"),
    };
    let info = observe_harness_ui(paths, observer, session);
    // Persistence failure must not invalidate still-verifiable live evidence;
    // the retention worker reports it and does not reclaim Harness logs.
    if let Some(session) = session { let _ = persist_evidence(paths, observer, session); }
    info
}

fn observe_harness_ui(paths: &NexusPaths, observer: &mut HarnessLogObserver, session: Option<&HarnessLogSession>) -> HarnessUiInfo {
    observer.select_session(session);
    let Some(session) = session else {
        return unavailable_harness_ui_info("Harness log session marker is not available");
    };
    restore_evidence(paths, observer, session);
    let log_paths = harness_log_paths(paths, session);
    let boundaries = [
        (
            session.stdout_watermark,
            Some(session.stdout_file_identity.as_str()),
        ),
        (
            session.stderr_watermark,
            Some(session.stderr_file_identity.as_str()),
        ),
    ];
    let mut candidates = Vec::new();
    for (path, (watermark, file_identity)) in log_paths.into_iter().zip(boundaries) {
        let Ok(Some(candidate)) = observer.observe_file(&path, watermark, file_identity) else {
            continue;
        };
        if candidate.token.is_some() {
            candidates.push(candidate);
        }
    }

    candidates.sort_by(|left, right| {
        (
            left.token.is_some(),
            left.observed_at_nanos,
            left.offset,
            left.source.as_str(),
            left.sequence,
        )
            .cmp(&(
                right.token.is_some(),
                right.observed_at_nanos,
                right.offset,
                right.source.as_str(),
                right.sequence,
            ))
    });
    if let Some(candidate) = candidates.pop() {
        return HarnessUiInfo {
            available: true,
            generation: Some(session.generation),
            run_id: Some(session.run_id.clone()),
            url: Some(candidate.url),
            token: candidate.token,
            source: Some(candidate.source),
            observed_at_unix: Some(candidate.observed_at_unix),
            message: None,
        };
    }

    unavailable_harness_ui_info(
        "Current Harness authentication token not found after this run's log boundary",
    )
}

pub fn unavailable_harness_ui_info(message: impl Into<String>) -> HarnessUiInfo {
    HarnessUiInfo {
        available: false,
        generation: None,
        run_id: None,
        url: None,
        token: None,
        source: None,
        observed_at_unix: None,
        message: Some(message.into()),
    }
}

pub fn harness_observation_matches_session(
    response: &HarnessResponse,
    session: &HarnessLogSession,
) -> bool {
    response.log_session_run_id.as_deref() == Some(session.run_id.as_str())
        && response.generation == Some(session.generation)
        && response.log_session_generation == Some(session.generation)
        && response.log_stdout_watermark == Some(session.stdout_watermark)
        && response.log_stderr_watermark == Some(session.stderr_watermark)
        && response.log_stdout_file_identity.as_deref()
            == Some(session.stdout_file_identity.as_str())
        && response.log_stderr_file_identity.as_deref()
            == Some(session.stderr_file_identity.as_str())
        && response.log_stdout_name.as_deref() == Some(session.stdout_log_name.as_str())
        && response.log_stderr_name.as_deref() == Some(session.stderr_log_name.as_str())
        && response.log_session_launch_pending == Some(session.launch_pending)
}

#[derive(Debug, Default)]
pub struct HarnessLogObserver {
    pub files: HashMap<PathBuf, HarnessLogCursor>,
    sequence: u64,
    session: Option<(String, u64, u64, u64, String, String, String, String, bool)>,
    session_initialized: bool,
    scanned: HashMap<PathBuf, u64>,
}

#[derive(Debug, Clone)]
pub struct HarnessLogCursor {
    pub offset: u64,
    pub fingerprint: u64,
    pub candidate: Option<HarnessUrlCandidate>,
    evidence: Option<TokenEvidence>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenEvidence { start: u64, bytes: Vec<u8> }
impl std::fmt::Debug for TokenEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEvidence").field("start", &self.start).field("bytes", &"[REDACTED]").finish()
    }
}

fn verify_evidence(file: &mut fs::File, evidence: &TokenEvidence, watermark: u64, length: u64) -> io::Result<bool> {
    if evidence.start < watermark || evidence.bytes.is_empty() || evidence.bytes.len() as u64 > HARNESS_LOG_TAIL_BYTES {
        return Ok(false);
    }
    let Some(end) = evidence.start.checked_add(evidence.bytes.len() as u64) else { return Ok(false); };
    // A complete URL always has a following delimiter; EOF alone is not one.
    if end >= length { return Ok(false); }
    let read_start = evidence.start.saturating_sub(1);
    let left = usize::from(evidence.start != 0);
    let mut bytes = vec![0; left + evidence.bytes.len() + 1];
    file.seek(SeekFrom::Start(read_start))?;
    file.read_exact(&mut bytes)?;
    Ok((left == 0 || bytes[0].is_ascii_whitespace())
        && bytes[left..left + evidence.bytes.len()] == evidence.bytes
        && bytes.last().is_some_and(u8::is_ascii_whitespace))
}

#[derive(Debug)]
pub struct HarnessLogSnapshot {
    pub length: u64,
    pub start: u64,
    pub file_identity: String,
    pub modified_at_nanos: u128,
    pub fingerprint: u64,
    pub bytes: Vec<u8>,
    pub left_delimited: bool,
    pub right_delimited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessUrlCandidate {
    pub url: String,
    pub token: Option<String>,
    pub source: String,
    pub observed_at_unix: u64,
    pub observed_at_nanos: u128,
    pub offset: u64,
    pub sequence: u64,
}

impl HarnessLogObserver {
    pub fn invalidate(&mut self) {
        self.files.clear();
        self.scanned.clear();
        self.session = None;
        self.session_initialized = false;
    }

    fn select_session(&mut self, session: Option<&HarnessLogSession>) {
        let selected = session.map(|session| {
            (
                session.run_id.clone(),
                session.generation,
                session.stdout_watermark,
                session.stderr_watermark,
                session.stdout_file_identity.clone(),
                session.stderr_file_identity.clone(),
                session.stdout_log_name.clone(),
                session.stderr_log_name.clone(),
                session.launch_pending,
            )
        });
        if !self.session_initialized || self.session != selected {
            self.files.clear();
            self.scanned.clear();
            self.sequence = 0;
            self.session = selected;
            self.session_initialized = true;
        }
    }

    fn observe_file(
        &mut self,
        path: &Path,
        session_watermark: u64,
        expected_file_identity: Option<&str>,
    ) -> io::Result<Option<HarnessUrlCandidate>> {
        let mut file = fs::File::open(path)?;
        let snapshot = read_log_snapshot(&mut file)?;
        self.observe_snapshot(path, &mut file, snapshot, session_watermark, expected_file_identity)
    }

    fn observe_snapshot(&mut self, path: &Path, file: &mut fs::File, snapshot: HarnessLogSnapshot,
        session_watermark: u64, expected_file_identity: Option<&str>) -> io::Result<Option<HarnessUrlCandidate>> {
        let previous = self.files.remove(path);
        if expected_file_identity.is_some_and(|expected| expected != snapshot.file_identity)
            || snapshot.length < session_watermark { return Ok(None); }
        // Re-read the original bytes and both boundaries on this SAME handle,
        // even when length, timestamps and the bounded tail have not changed.
        let verified = previous.as_ref().and_then(|previous| previous.evidence.as_ref())
            .map(|evidence| verify_evidence(file, evidence, session_watermark, snapshot.length))
            .transpose()?.unwrap_or(false);
        let mut candidate = if verified { previous.as_ref().and_then(|old| old.candidate.clone()) } else { None };
        let mut evidence = if verified { previous.and_then(|old| old.evidence) } else { None };
        // Keep the original seen boundary fixed throughout this scan. Older
        // words must not acquire the append's timestamp before we reach the
        // retained word again, even when several URLs remain in the tail.
        let retained_end = candidate.as_ref().map(|candidate| candidate.offset);
        for (word_start, word_end) in log_word_ranges(&snapshot.bytes) {
            if (word_start == 0 && !snapshot.left_delimited)
                || (word_end == snapshot.bytes.len() && !snapshot.right_delimited)
            {
                continue;
            }
            let absolute_start = snapshot.start + word_start as u64;
            let absolute_end = snapshot.start + word_end as u64;
            // The word must begin at or after the durable EOF boundary and
            // end after it. This rejects a URL whose bytes straddle the
            // previous and current run while allowing output in a new file to
            // begin at offset zero.
            if absolute_start < session_watermark || absolute_end <= session_watermark
                || retained_end.is_some_and(|end| absolute_end <= end) {
                continue;
            }
            let Ok(word) = std::str::from_utf8(&snapshot.bytes[word_start..word_end]) else {
                continue;
            };
            let cleaned = trim_log_url(word);
            let Some((url, token)) = parse_loopback_harness_url(cleaned) else {
                continue;
            };
            let next_evidence = TokenEvidence { start: absolute_start, bytes: snapshot.bytes[word_start..word_end].to_vec() };
            if evidence.as_ref().is_some_and(|old| old.start == next_evidence.start && old.bytes == next_evidence.bytes) { continue; }
            let next = HarnessUrlCandidate {
                url,
                token,
                source: path.display().to_string(),
                observed_at_unix: (snapshot.modified_at_nanos / 1_000_000_000) as u64,
                observed_at_nanos: snapshot.modified_at_nanos,
                offset: absolute_end,
                sequence: self.sequence,
            };
            self.sequence = self.sequence.wrapping_add(1);
            if candidate.as_ref().map_or(true, |current| {
                harness_candidate_cmp(current, &next).is_lt()
            }) {
                candidate = Some(next);
                evidence = Some(next_evidence);
            }
        }

        self.files.insert(
            path.to_owned(),
            HarnessLogCursor {
                offset: snapshot.length,
                fingerprint: snapshot.fingerprint,
                candidate: candidate.clone(),
                evidence,
            },
        );
        Ok(candidate)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableEvidence {
    schema_version: u32,
    session: HarnessLogSession,
    streams: Vec<StreamEvidence>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamEvidence {
    scanned: u64,
    candidate: Option<HarnessUrlCandidate>,
    evidence: Option<TokenEvidence>,
}

fn same_evidence_session(left: &HarnessLogSession, right: &HarnessLogSession) -> bool {
    // This latch records backup bookkeeping, not a new log writer. All run,
    // generation, file, watermark, launch-state and schema fields still bind.
    let mut left = left.clone(); let mut right = right.clone();
    left.healthy_snapshot_attempted = false; right.healthy_snapshot_attempted = false;
    left == right
}

fn evidence_path(paths: &NexusPaths) -> io::Result<PathBuf> {
    for directory in [&paths.root, &paths.run_dir] {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Evidence directory is not ordinary")); }
    }
    Ok(paths.run_dir.join("harness-token-evidence.json"))
}

fn acquire_evidence_lock(paths: &NexusPaths) -> io::Result<fs::File> {
    // The compatibility launcher is a separate process: a Rust mutex alone
    // cannot protect its index writes against the Agent's sparse reclamation.
    let _ = evidence_path(paths)?;
    let path = paths.run_dir.join("harness-evidence.lock");
    let mut options = fs::OpenOptions::new(); options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) { return Err(io::Error::other("Evidence lock is not ordinary")); }
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "Evidence is being maintained"),
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

fn read_evidence(paths: &NexusPaths) -> io::Result<Option<Vec<u8>>> {
    let path = evidence_path(paths)?;
    let metadata = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None), other => other?,
    };
    if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > EVIDENCE_LIMIT { return Err(io::Error::other("Unsafe evidence index")); }
    let mut options = fs::OpenOptions::new(); options.read(true);
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || nexus_core::path_is_reparse(&file.metadata()?) { return Err(io::Error::other("Evidence identity changed")); }
    nexus_private_evidence_check(&file)?;
    let mut bytes = Vec::new(); file.take(EVIDENCE_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > EVIDENCE_LIMIT { return Err(io::Error::other("Evidence index exceeds budget")); }
    Ok(Some(bytes))
}

// Core exposes the same private-file verifier used for Agent credentials.
fn nexus_private_evidence_check(file: &fs::File) -> io::Result<()> { nexus_core::verify_private_file(file) }

fn restore_evidence(paths: &NexusPaths, observer: &mut HarnessLogObserver, session: &HarnessLogSession) {
    let Ok(Some(bytes)) = read_evidence(paths) else { return; };
    let Ok(index) = serde_json::from_slice::<DurableEvidence>(&bytes) else { return; };
    if index.schema_version != 1 || !same_evidence_session(&index.session, session) || index.streams.len() != 2 { return; }
    for (i, (path, saved)) in harness_log_paths(paths, session).into_iter().zip(index.streams).enumerate() {
        let (identity, watermark) = if i == 0 { (&session.stdout_file_identity, session.stdout_watermark) } else { (&session.stderr_file_identity, session.stderr_watermark) };
        let Ok(mut file) = fs::File::open(&path) else { continue; };
        let Ok(metadata) = file.metadata() else { continue; };
        if log_file_identity(&file).ok().as_ref() != Some(identity) || saved.scanned < watermark || saved.scanned > metadata.len() { continue; }
        observer.scanned.entry(path.clone()).and_modify(|old| *old = (*old).max(saved.scanned)).or_insert(saved.scanned);
        let (Some(candidate), Some(evidence)) = (saved.candidate, saved.evidence) else { continue; };
        if evidence.bytes.len() as u64 > HARNESS_LOG_TAIL_BYTES
            || !verify_evidence(&mut file, &evidence, watermark, metadata.len()).unwrap_or(false)
            || candidate.offset != evidence.start.saturating_add(evidence.bytes.len() as u64)
            || candidate.source != path.display().to_string() { continue; }
        let Ok(word) = std::str::from_utf8(&evidence.bytes) else { continue; };
        if parse_loopback_harness_url(trim_log_url(word)) != Some((candidate.url.clone(), candidate.token.clone())) { continue; }
        let replace = observer.files.get(&path).and_then(|c| c.candidate.as_ref())
            .is_none_or(|old| old.offset < candidate.offset);
        if replace {
            observer.sequence = observer.sequence.max(candidate.sequence.saturating_add(1));
            observer.files.insert(path, HarnessLogCursor { offset: metadata.len(), fingerprint: 0, candidate: Some(candidate), evidence: Some(evidence) });
        }
    }
}

fn persist_evidence(paths: &NexusPaths, observer: &HarnessLogObserver, session: &HarnessLogSession) -> io::Result<()> {
    if !matches!(HarnessLogSessionStore::new(paths.clone()).read(), Ok(Some(ref current)) if current == session) {
        return Err(io::Error::other("Log session changed before evidence publication"));
    }
    let streams = harness_log_paths(paths, session).into_iter().enumerate().map(|(i, path)| {
        let cursor = observer.files.get(&path);
        StreamEvidence { scanned: observer.scanned.get(&path).copied().unwrap_or(if i == 0 { session.stdout_watermark } else { session.stderr_watermark }),
            candidate: cursor.and_then(|c| c.candidate.clone()), evidence: cursor.and_then(|c| c.evidence.clone()) }
    }).collect();
    let bytes = serde_json::to_vec(&DurableEvidence { schema_version: 1, session: session.clone(), streams })?;
    if bytes.len() as u64 > EVIDENCE_LIMIT { return Err(io::Error::other("Evidence index exceeds budget")); }
    if read_evidence(paths)?.as_deref() == Some(bytes.as_slice()) { return Ok(()); }
    nexus_core::write_private_bytes_atomic(&paths.run_dir, &evidence_path(paths)?, &bytes)
}

/// At most 512 KiB scanned per stream per cycle. Unscanned bytes are never
/// reclaimed, so high-volume writers produce visible backlog, not lost tokens.
pub fn retain_harness_logs(paths: &NexusPaths, observer: &mut HarnessLogObserver, session: &HarnessLogSession)
    -> io::Result<Vec<(String, nexus_core::log_retention::FileStatus, u64)>> {
    let _guard = LOG_EVIDENCE_GATE.lock().map_err(|_| io::Error::other("Log evidence lock unavailable"))?;
    let _file_guard = acquire_evidence_lock(paths)?;
    observe_harness_ui(paths, observer, Some(session));
    for (i, path) in harness_log_paths(paths, session).into_iter().enumerate() {
        let (identity, watermark) = if i == 0 { (&session.stdout_file_identity, session.stdout_watermark) } else { (&session.stderr_file_identity, session.stderr_watermark) };
        let mut file = fs::File::open(&path)?;
        let length = file.metadata()?.len();
        let mut start = observer.scanned.get(&path).copied().unwrap_or(watermark);
        if start > length || log_file_identity(&file)? != *identity { return Err(io::Error::other("Log identity or scan boundary changed")); }
        for _ in 0..8 {
            if start >= length { break; }
            let end = start.saturating_add(HARNESS_LOG_TAIL_BYTES).min(length);
            let mut snapshot = read_log_window(&mut file, start, end)?;
            // Leave a partial final word to the next scan, preserving its left delimiter.
            let advance = snapshot.bytes.iter().rposition(u8::is_ascii_whitespace).map(|p| p + 1);
            let next = match advance {
                Some(count) => { snapshot.bytes.truncate(count); snapshot.right_delimited = true; start + count as u64 },
                None if end - start == HARNESS_LOG_TAIL_BYTES => end,
                None => break,
            };
            observer.observe_snapshot(&path, &mut file, snapshot, watermark, Some(identity))?;
            start = next;
        }
        observer.scanned.insert(path, start);
    }
    // No irreversible reclamation until the complete index is privately durable.
    persist_evidence(paths, observer, session)?;
    let mut status = Vec::new();
    for (i, path) in harness_log_paths(paths, session).into_iter().enumerate() {
        let identity = if i == 0 { &session.stdout_file_identity } else { &session.stderr_file_identity };
        let protected: Vec<_> = observer.files.get(&path).and_then(|c| c.evidence.as_ref())
            .map(|e| vec![e.start.saturating_sub(1)..e.start + e.bytes.len() as u64 + 1]).unwrap_or_default();
        let scanned = observer.scanned.get(&path).copied().unwrap_or(0);
        // Keep the delimiter immediately before the next scan window intact.
        let result = nexus_core::log_retention::maintain(&path, Some(identity), &protected, Some(scanned.saturating_sub(1)))?;
        let backlog = result.logical_bytes.saturating_sub(nexus_core::log_retention::TAIL_BYTES).saturating_sub(scanned);
        status.push((path.file_name().unwrap().to_string_lossy().into_owned(), result, backlog));
    }
    Ok(status)
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use std::io::Write;
    fn fixture() -> (NexusPaths, HarnessLogSession) {
        let root = std::env::temp_dir().join(format!("nexus-evidence-retention-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let paths = NexusPaths::from_root(root); paths.ensure_directories().unwrap();
        let out = fs::OpenOptions::new().create_new(true).append(true).open(paths.logs_dir.join("harness-test.stdout.log")).unwrap();
        let err = fs::OpenOptions::new().create_new(true).append(true).open(paths.logs_dir.join("harness-test.stderr.log")).unwrap();
        let session = HarnessLogSession::new("retention-test".into(), 1, 0, 0,
            log_file_identity(&out).unwrap(), log_file_identity(&err).unwrap(),
            "harness-test.stdout.log".into(), "harness-test.stderr.log".into(), true, 1);
        HarnessLogSessionStore::new(paths.clone()).write(&session).unwrap();
        (paths, session)
    }
    #[test]
    fn restart_restores_only_private_same_session_original_bytes() {
        let (paths, session) = fixture();
        let path = paths.logs_dir.join(&session.stdout_log_name);
        fs::write(&path, b"http://127.0.0.1:3080/?token=PRIVATE_SENTINEL\n").unwrap();
        let mut observer = HarnessLogObserver::default();
        assert!(read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session)).available);
        let index = evidence_path(&paths).unwrap();
        nexus_core::verify_private_file(&fs::File::open(&index).unwrap()).unwrap();
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(&vec![b'x'; 128 * 1024]).unwrap();
        let saved: DurableEvidence = serde_json::from_slice(&read_evidence(&paths).unwrap().expect("private index persists")).expect("private index decodes");
        assert!(same_evidence_session(&saved.session, &session));
        let saved_stream = &saved.streams[0];
        let evidence = saved_stream.evidence.as_ref().expect("index retains URL evidence");
        let candidate = saved_stream.candidate.as_ref().expect("index retains URL candidate");
        assert_eq!(candidate.source, path.display().to_string());
        assert_eq!(candidate.offset, evidence.start + evidence.bytes.len() as u64);
        let mut original = fs::File::open(&path).unwrap();
        assert_eq!(log_file_identity(&original).unwrap(), session.stdout_file_identity);
        let length = original.metadata().unwrap().len();
        assert!(verify_evidence(&mut original, evidence, session.stdout_watermark, length).unwrap());
        let mut restarted = HarnessLogObserver::default();
        let restored = read_harness_ui_info_with_observer(&paths, &mut restarted, Some(&session));
        assert_eq!(restored.token.as_deref(), Some("PRIVATE_SENTINEL"), "restoration result: {restored:?}");
        let mut healthy = session.clone(); healthy.healthy_snapshot_attempted = true;
        let restored = read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&healthy));
        assert_eq!(restored.token.as_deref(), Some("PRIVATE_SENTINEL"), "restoration result: {restored:?}");
        // No new URL in the tail can rescue a changed generation.
        let mut other = session.clone(); other.generation += 1;
        assert!(!read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&other)).available);
        // A same-length edit at the original evidence offset is rejected.
        let mut writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::Start(0)).unwrap(); writer.write_all(b"X").unwrap(); drop(writer);
        assert!(!read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session)).available);
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn separate_handles_cannot_race_evidence_publication_and_reclamation() {
        let (paths, session) = fixture();
        let held = acquire_evidence_lock(&paths).unwrap();
        assert_eq!(acquire_evidence_lock(&paths).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(!read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session)).available);
        assert!(retain_harness_logs(&paths, &mut HarnessLogObserver::default(), &session).is_err());
        drop(held);
        drop(acquire_evidence_lock(&paths).unwrap());
        fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn retention_preserves_old_token_and_refuses_reclaim_without_private_index() {
        let (paths, session) = fixture();
        let path = paths.logs_dir.join(&session.stdout_log_name);
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"http://127.0.0.1:3080/?token=KEEP_ME\n").unwrap();
        let mut observer = HarnessLogObserver::default();
        read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        let mut chunk = vec![b'x'; 64 * 1024]; *chunk.last_mut().unwrap() = b'\n';
        for _ in 0..160 { writer.write_all(&chunk).unwrap(); }
        let before = writer.metadata().unwrap().len();
        for _ in 0..6 { retain_harness_logs(&paths, &mut observer, &session).unwrap(); }
        assert_eq!(writer.metadata().unwrap().len(), before);
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session)).token.as_deref(), Some("KEEP_ME"));
        assert_eq!(log_file_identity(&writer).unwrap(), session.stdout_file_identity);
        let index = evidence_path(&paths).unwrap(); fs::remove_file(&index).unwrap(); fs::create_dir(&index).unwrap();
        let identity = log_file_identity(&writer).unwrap();
        assert!(retain_harness_logs(&paths, &mut observer, &session).is_err());
        assert_eq!(log_file_identity(&writer).unwrap(), identity);
        drop(writer); fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn scanner_finds_unobserved_token_before_reclaim_and_stale_observers_cannot_rollback() {
        let (paths, session) = fixture();
        let path = paths.logs_dir.join(&session.stdout_log_name);
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let mut chunk = vec![b'x'; 64 * 1024]; *chunk.last_mut().unwrap() = b'\n';
        for _ in 0..12 { writer.write_all(&chunk).unwrap(); }
        writer.write_all(b"http://127.0.0.1:3080/?token=MIDDLE\n").unwrap();
        for _ in 0..160 { writer.write_all(&chunk).unwrap(); }
        let mut scanner = HarnessLogObserver::default();
        retain_harness_logs(&paths, &mut scanner, &session).unwrap();
        retain_harness_logs(&paths, &mut scanner, &session).unwrap();
        let mut stale = HarnessLogObserver::default();
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut stale, Some(&session)).token.as_deref(), Some("MIDDLE"));
        writer.write_all(b"http://127.0.0.1:3080/?token=NEWEST\n").unwrap();
        read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session));
        for _ in 0..2 { writer.write_all(&chunk).unwrap(); }
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut stale, Some(&session)).token.as_deref(), Some("NEWEST"));
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session)).token.as_deref(), Some("NEWEST"));
        drop(writer); fs::remove_dir_all(paths.root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn short_slices_catch_up_with_continuous_writes_above_old_scan_rate() {
        let (paths, session) = fixture();
        let path = paths.logs_dir.join(&session.stdout_log_name);
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"http://127.0.0.1:3080/?token=LIVE_TOKEN\n").unwrap();
        let mut observer = HarnessLogObserver::default();
        read_harness_ui_info_with_observer(&paths, &mut observer, Some(&session));
        let mut chunk = vec![b'x'; 256 * 1024];
        for line in chunk.chunks_mut(128) { *line.last_mut().unwrap() = b'\n'; }
        for _ in 0..40 { writer.write_all(&chunk).unwrap(); }
        let first = retain_harness_logs(&paths, &mut observer, &session).unwrap();
        assert!(first[0].2 > 0);
        let mut last = first;
        let started = std::time::Instant::now();
        for _ in 0..16 {
            // Bounded live writer: 256 KiB every 100 ms, far above the old
            // 512 KiB / 5 s scanner. Each maintenance call is still one slice.
            std::thread::sleep(std::time::Duration::from_millis(100));
            writer.write_all(&chunk).unwrap();
            last = retain_harness_logs(&paths, &mut observer, &session).unwrap();
        }
        assert_eq!(last[0].2, 0);
        assert!(last[0].1.allocated_bytes <= nexus_core::log_retention::TAIL_BYTES + 256 * 1024);
        assert!(started.elapsed() < std::time::Duration::from_secs(15), "bounded workload should not stall controls");
        assert_eq!(read_harness_ui_info_with_observer(&paths, &mut HarnessLogObserver::default(), Some(&session)).token.as_deref(), Some("LIVE_TOKEN"));
        drop(writer); fs::remove_dir_all(paths.root).unwrap();
    }
}

fn harness_log_paths(paths: &NexusPaths, session: &HarnessLogSession) -> [PathBuf; 2] {
    [
        paths.logs_dir.join(&session.stdout_log_name),
        paths.logs_dir.join(&session.stderr_log_name),
    ]
}

fn harness_candidate_cmp(
    left: &HarnessUrlCandidate,
    right: &HarnessUrlCandidate,
) -> std::cmp::Ordering {
    (
        left.token.is_some(),
        left.observed_at_nanos,
        left.offset,
        left.source.as_str(),
        left.sequence,
    )
        .cmp(&(
            right.token.is_some(),
            right.observed_at_nanos,
            right.offset,
            right.source.as_str(),
            right.sequence,
        ))
}

fn read_log_snapshot(file: &mut fs::File) -> io::Result<HarnessLogSnapshot> {
    let length = file.metadata()?.len();
    read_log_window(file, length.saturating_sub(HARNESS_LOG_TAIL_BYTES), length)
}

fn read_log_window(file: &mut fs::File, start: u64, end: u64) -> io::Result<HarnessLogSnapshot> {
    let metadata = file.metadata()?;
    let modified_at_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let length = metadata.len();
    let file_identity = log_file_identity(&file)?;
    if end > length || start > end || end - start > HARNESS_LOG_TAIL_BYTES { return Err(io::Error::other("Invalid log scan window")); }
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = Vec::new();
    let expected_len = end.saturating_sub(read_start);
    (&mut *file).take(expected_len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Harness log changed while its bounded snapshot was read",
        ));
    }
    let left_delimited = if start == 0 {
        true
    } else {
        let delimiter = bytes
            .first()
            .copied()
            .is_some_and(|byte| byte.is_ascii_whitespace());
        if !bytes.is_empty() {
            bytes.remove(0);
        }
        delimiter
    };
    let right_delimited = bytes
        .last()
        .copied()
        .is_none_or(|byte| byte.is_ascii_whitespace());
    let fingerprint = fingerprint_bytes(&bytes, length);
    Ok(HarnessLogSnapshot {
        length,
        start,
        file_identity,
        modified_at_nanos,
        fingerprint,
        bytes,
        left_delimited,
        right_delimited,
    })
}

fn fingerprint_bytes(bytes: &[u8], length: u64) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    length.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn log_word_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_whitespace() {
            if let Some(start) = start.take() {
                ranges.push((start, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        ranges.push((start, bytes.len()));
    }
    ranges
}

fn trim_log_url(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character.is_control()
            || matches!(
                character,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '.'
            )
    })
}

pub fn parse_loopback_harness_url(raw: &str) -> Option<(String, Option<String>)> {
    if raw.is_empty() {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "http"
        || url.username() != ""
        || url.password().is_some()
        || !is_loopback_host(url.host_str()?)
        || url.port_or_known_default().is_none()
        || url.as_str().chars().any(char::is_control)
    {
        return None;
    }
    if url
        .query_pairs()
        .any(|(key, value)| key.is_empty() || value.is_empty())
    {
        return None;
    }
    if let Some(fragment) = url.fragment() {
        if fragment.is_empty()
            || fragment.split('&').any(|part| {
                let Some((key, value)) = part.split_once('=') else {
                    return true;
                };
                key.is_empty() || value.is_empty()
            })
        {
            return None;
        }
    }
    let token = url
        .query_pairs()
        .find_map(|(key, value)| is_token_key(&key).then(|| value.into_owned()))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            url.fragment().and_then(|fragment| {
                fragment.split('&').find_map(|part| {
                    let (key, value) = part.split_once('=')?;
                    is_token_key(key).then(|| value.to_owned())
                })
            })
        });
    Some((url.as_str().to_owned(), token))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn is_token_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "token" | "access_token" | "auth_token" | "session_token" | "authorization"
    )
}
