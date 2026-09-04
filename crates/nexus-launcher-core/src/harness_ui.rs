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
    observer.select_session(session);
    let Some(session) = session else {
        return unavailable_harness_ui_info("Harness log session marker is not available");
    };
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
}

#[derive(Debug, Clone)]
pub struct HarnessLogCursor {
    pub offset: u64,
    pub fingerprint: u64,
    pub candidate: Option<HarnessUrlCandidate>,
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

#[derive(Debug, Clone)]
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
        let snapshot = read_log_snapshot(path)?;
        if expected_file_identity.is_some_and(|expected| expected != snapshot.file_identity) {
            self.files.remove(path);
            return Ok(None);
        }
        let previous = self.files.get(path).cloned();
        if previous.as_ref().is_some_and(|previous| {
            snapshot.length == previous.offset && snapshot.fingerprint == previous.fingerprint
        }) {
            let previous = previous.expect("same-file cursor is present");
            self.files.insert(
                path.to_owned(),
                HarnessLogCursor {
                    offset: snapshot.length,
                    fingerprint: snapshot.fingerprint,
                    candidate: previous.candidate.clone(),
                },
            );
            return Ok(previous.candidate);
        }

        // A bounded overlap cannot prove that a file was appended: a rotated
        // file may preserve the overlap while replacing bytes before it. Parse
        // the complete current tail and discard candidates no longer present.
        // A session watermark belongs to the append-only file that existed
        // when the Agent started Harness. A shorter replacement cannot prove
        // where this run begins, so fail closed instead of treating its first
        // byte as current output and potentially reviving an old token.
        if snapshot.length < session_watermark {
            self.files.remove(path);
            return Ok(None);
        }
        let mut candidate = None;
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
            if absolute_start < session_watermark || absolute_end <= session_watermark {
                continue;
            }
            let Ok(word) = std::str::from_utf8(&snapshot.bytes[word_start..word_end]) else {
                continue;
            };
            let cleaned = trim_log_url(word);
            let Some((url, token)) = parse_loopback_harness_url(cleaned) else {
                continue;
            };
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
            }
        }

        self.files.insert(
            path.to_owned(),
            HarnessLogCursor {
                offset: snapshot.length,
                fingerprint: snapshot.fingerprint,
                candidate: candidate.clone(),
            },
        );
        Ok(candidate)
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

fn read_log_snapshot(path: &Path) -> io::Result<HarnessLogSnapshot> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let modified_at_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let length = metadata.len();
    let file_identity = log_file_identity(&file)?;
    let start = length.saturating_sub(HARNESS_LOG_TAIL_BYTES);
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = Vec::new();
    let expected_len = length.saturating_sub(read_start);
    (&mut file).take(expected_len).read_to_end(&mut bytes)?;
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
