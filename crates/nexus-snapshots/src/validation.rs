use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde_json::Value as JsonValue;
use serde_yaml_ng::Value as YamlValue;
use sha2::{Digest, Sha256};

use crate::{
    new_identifier, Result, SnapshotError, SnapshotFilePolicy, SnapshotFileRecord,
    SnapshotFileState, SnapshotKind, SnapshotManifest, SnapshotStore, StructuredFormat,
    FILE_POLICY, MAX_MANIFEST_BYTES, MAX_TOTAL_SNAPSHOT_BYTES, SNAPSHOT_SCHEMA_VERSION,
};

const MAX_REDACTED_PATHS: usize = 1024;
const MAX_REDACTED_PATH_BYTES: usize = 512;
const MAX_OMISSION_REASON_BYTES: usize = 512;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;

pub(crate) struct CapturedAllowedFile {
    pub record: SnapshotFileRecord,
    pub bytes: Option<Vec<u8>>,
    pub stored_size: u64,
}

pub(crate) fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name == "node_modules"
        || name.chars().any(|character| {
            character.is_control() || character == '/' || character == '\\' || character == ':'
        })
    {
        return Err(SnapshotError::InvalidProfileName(name.to_owned()));
    }
    Ok(())
}

pub(crate) fn validate_identifier(identifier: &str) -> Result<()> {
    if identifier.is_empty()
        || identifier.len() > 128
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SnapshotError::InvalidIdentifier(identifier.to_owned()));
    }
    Ok(())
}

/// Exact namespace emitted by new_identifier: timestamp, process, sequence.
pub(crate) fn is_generated_name(name: &str, prefix: &str) -> bool {
    let Some(suffix) = name.strip_prefix(prefix) else { return false; };
    let parts: Vec<_> = suffix.split('-').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit()) && part.parse::<u64>().is_ok())
}

pub(crate) fn validate_metadata_text(name: &str, value: &str, limit: usize) -> Result<()> {
    if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(SnapshotError::InvalidManifest(format!(
            "{name} must be non-empty, free of control characters, and at most {limit} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn canonical_secure_root(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| SnapshotError::io(format!("inspect root {}", path.display()), error))?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(SnapshotError::UnsafePath(format!(
            "root is not a regular directory: {}",
            path.display()
        )));
    }
    fs::canonicalize(path)
        .map_err(|error| SnapshotError::io(format!("canonicalize root {}", path.display()), error))
}

pub(crate) fn resolve_under(base: &Path, anchor: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative(relative)?;
    if !anchor.starts_with(base) {
        return Err(SnapshotError::UnsafePath(format!(
            "anchor {} escapes {}",
            anchor.display(),
            base.display()
        )));
    }
    validate_existing_ancestors(base, anchor)?;
    let candidate = anchor.join(relative);
    validate_existing_ancestors(base, &candidate)?;
    Ok(candidate)
}

fn validate_relative(relative: &Path) -> Result<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SnapshotError::InvalidPath(
            relative.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

fn validate_existing_ancestors(base: &Path, candidate: &Path) -> Result<()> {
    let relative = candidate.strip_prefix(base).map_err(|_| {
        SnapshotError::UnsafePath(format!(
            "{} is outside {}",
            candidate.display(),
            base.display()
        ))
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(SnapshotError::UnsafePath(candidate.display().to_string()));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_link_or_reparse(&metadata) {
                    return Err(SnapshotError::UnsafePath(format!(
                        "reparse point or symbolic link: {}",
                        current.display()
                    )));
                }
                let canonical = fs::canonicalize(&current).map_err(|error| {
                    SnapshotError::io(
                        format!("canonicalize existing path {}", current.display()),
                        error,
                    )
                })?;
                if !canonical.starts_with(base) {
                    return Err(SnapshotError::UnsafePath(format!(
                        "{} resolves outside {}",
                        current.display(),
                        base.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(SnapshotError::io(
                    format!("inspect path {}", current.display()),
                    error,
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn ensure_directory_tree(base: &Path, directory: &Path) -> Result<()> {
    let relative = directory.strip_prefix(base).map_err(|_| {
        SnapshotError::UnsafePath(format!(
            "{} is outside {}",
            directory.display(),
            base.display()
        ))
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(SnapshotError::UnsafePath(directory.display().to_string()));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() || is_link_or_reparse(&metadata) {
                    return Err(SnapshotError::UnsafePath(format!(
                        "directory component is unsafe: {}",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|create_error| {
                    SnapshotError::io(
                        format!("create directory {}", current.display()),
                        create_error,
                    )
                })?;
                set_private_directory_permissions(&current)?;
            }
            Err(error) => {
                return Err(SnapshotError::io(
                    format!("inspect directory {}", current.display()),
                    error,
                ));
            }
        }
    }
    validate_directory_tree(base, directory)
}

pub(crate) fn ensure_new_directory(base: &Path, directory: &Path) -> Result<()> {
    if directory.exists() {
        return Err(SnapshotError::InvalidState(format!(
            "new directory already exists: {}",
            directory.display()
        )));
    }
    let parent = directory.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("directory has no parent: {}", directory.display()))
    })?;
    ensure_directory_tree(base, parent)?;
    fs::create_dir(directory).map_err(|error| {
        SnapshotError::io(format!("create directory {}", directory.display()), error)
    })?;
    set_private_directory_permissions(directory)
}

pub(crate) fn validate_directory_tree(base: &Path, directory: &Path) -> Result<()> {
    validate_existing_ancestors(base, directory)?;
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        SnapshotError::io(format!("inspect directory {}", directory.display()), error)
    })?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(SnapshotError::UnsafePath(format!(
            "not a regular directory: {}",
            directory.display()
        )));
    }
    Ok(())
}

pub(crate) fn validate_optional_directory_tree(base: &Path, directory: &Path) -> Result<bool> {
    match fs::symlink_metadata(directory) {
        Ok(_) => {
            validate_directory_tree(base, directory)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validate_existing_ancestors(base, directory)?;
            Ok(false)
        }
        Err(error) => Err(SnapshotError::io(
            format!("inspect optional directory {}", directory.display()),
            error,
        )),
    }
}

pub(crate) fn bounded_subdirectories(directory: &Path, bound: usize) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| {
        SnapshotError::io(format!("read directory {}", directory.display()), error)
    })? {
        let entry = entry.map_err(|error| {
            SnapshotError::io(format!("read entry in {}", directory.display()), error)
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            SnapshotError::io(format!("inspect {}", entry.path().display()), error)
        })?;
        if !metadata.is_dir() || is_link_or_reparse(&metadata) {
            return Err(SnapshotError::UnsafePath(format!(
                "unexpected non-directory or reparse point in {}: {}",
                directory.display(),
                entry.path().display()
            )));
        }
        result.push(entry.path());
        if result.len() >= bound {
            return Err(SnapshotError::Capacity(format!(
                "directory inventory exceeds bound {}: {}",
                bound.saturating_sub(1),
                directory.display()
            )));
        }
    }
    Ok(result)
}

pub(crate) fn capture_allowed_file(
    path: &Path,
    policy: &SnapshotFilePolicy,
) -> Result<CapturedAllowedFile> {
    let Some((source, mode)) = read_regular_bounded(path, policy.max_bytes)? else {
        return Ok(CapturedAllowedFile {
            record: SnapshotFileRecord {
                path: policy.manifest_path.to_owned(),
                scope: policy.scope,
                state: SnapshotFileState::Missing,
                source_size: 0,
                stored_size: 0,
                sha256: None,
                mode: None,
                redacted_paths: Vec::new(),
                omitted_reason: None,
            },
            bytes: None,
            stored_size: 0,
        });
    };
    let source_size = source.len() as u64;
    let sanitized = match sanitize_structured(policy.format, &source, policy.manifest_path) {
        Ok(sanitized) => sanitized,
        Err(error) => {
            let reason = match error {
                SnapshotError::StructuredData { message, .. }
                    if message.starts_with("invalid JSON") =>
                {
                    "invalid JSON; file omitted to avoid unsafe byte copying"
                }
                SnapshotError::StructuredData { message, .. }
                    if message.starts_with("invalid YAML") =>
                {
                    "invalid YAML; file omitted to avoid unsafe byte copying"
                }
                _ => "structured redaction failed; file omitted to avoid unsafe byte copying",
            };
            return Ok(CapturedAllowedFile {
                record: SnapshotFileRecord {
                    path: policy.manifest_path.to_owned(),
                    scope: policy.scope,
                    state: SnapshotFileState::Omitted,
                    source_size,
                    stored_size: 0,
                    sha256: None,
                    mode: Some(mode),
                    redacted_paths: Vec::new(),
                    omitted_reason: Some(truncate_string(reason, MAX_OMISSION_REASON_BYTES)),
                },
                bytes: None,
                stored_size: 0,
            });
        }
    };
    if sanitized.bytes.len() as u64 > policy.max_bytes {
        return Err(SnapshotError::Oversized {
            path: policy.manifest_path.to_owned(),
            size: sanitized.bytes.len() as u64,
            limit: policy.max_bytes,
        });
    }
    let stored_size = sanitized.bytes.len() as u64;
    let hash = sha256_hex(&sanitized.bytes);
    Ok(CapturedAllowedFile {
        record: SnapshotFileRecord {
            path: policy.manifest_path.to_owned(),
            scope: policy.scope,
            state: SnapshotFileState::Present,
            source_size,
            stored_size,
            sha256: Some(hash),
            mode: Some(mode),
            redacted_paths: sanitized.redacted_paths,
            omitted_reason: None,
        },
        bytes: Some(sanitized.bytes),
        stored_size,
    })
}

pub(crate) fn validate_profile_package(bytes: &[u8]) -> Result<()> {
    let value: JsonValue = serde_json::from_slice(bytes).map_err(|error| {
        SnapshotError::InvalidManifest(format!("profile/package.json is not valid JSON: {error}"))
    })?;
    if !value.is_object() {
        return Err(SnapshotError::InvalidManifest(
            "profile/package.json must contain a JSON object".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn read_regular_bounded(path: &Path, limit: u64) -> Result<Option<(Vec<u8>, u32)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SnapshotError::io(
                format!("inspect file {}", path.display()),
                error,
            ));
        }
    };
    if !metadata.is_file() || is_link_or_reparse(&metadata) {
        return Err(SnapshotError::UnsafePath(format!(
            "snapshot source is not a regular non-reparse file: {}",
            path.display()
        )));
    }
    if metadata.len() > limit {
        return Err(SnapshotError::Oversized {
            path: path.display().to_string(),
            size: metadata.len(),
            limit,
        });
    }
    // Open without following reparse points, then re-verify through the open
    // handle so a swap between the check and the open cannot redirect the read.
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)
        .map_err(|error| SnapshotError::io(format!("open file {}", path.display()), error))?;
    let opened = file.metadata().map_err(|error| {
        SnapshotError::io(format!("inspect file {}", path.display()), error)
    })?;
    if !opened.is_file() || is_link_or_reparse(&opened) {
        return Err(SnapshotError::UnsafePath(format!(
            "snapshot source identity changed to a non-regular file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SnapshotError::io(format!("read file {}", path.display()), error))?;
    if bytes.len() as u64 > limit {
        return Err(SnapshotError::Oversized {
            path: path.display().to_string(),
            size: bytes.len() as u64,
            limit,
        });
    }
    Ok(Some((bytes, portable_mode(&metadata))))
}

struct SanitizedContent {
    bytes: Vec<u8>,
    redacted_paths: Vec<String>,
}

fn sanitize_structured(
    format: StructuredFormat,
    bytes: &[u8],
    display_path: &str,
) -> Result<SanitizedContent> {
    let text = std::str::from_utf8(bytes).map_err(|error| SnapshotError::StructuredData {
        path: display_path.to_owned(),
        message: format!("content is not UTF-8: {error}"),
    })?;
    match format {
        StructuredFormat::Json => {
            let mut value: JsonValue =
                serde_json::from_str(text).map_err(|error| SnapshotError::StructuredData {
                    path: display_path.to_owned(),
                    message: format!("invalid JSON: {error}"),
                })?;
            let mut redacted = Vec::new();
            redact_json(&mut value, &mut Vec::new(), &mut redacted)?;
            let output = if redacted.is_empty() {
                bytes.to_vec()
            } else {
                serde_json::to_vec_pretty(&value).map_err(|error| {
                    SnapshotError::StructuredData {
                        path: display_path.to_owned(),
                        message: format!("cannot encode redacted JSON: {error}"),
                    }
                })?
            };
            Ok(SanitizedContent {
                bytes: output,
                redacted_paths: redacted,
            })
        }
        StructuredFormat::Yaml => {
            let mut value: YamlValue =
                serde_yaml_ng::from_str(text).map_err(|error| SnapshotError::StructuredData {
                    path: display_path.to_owned(),
                    message: format!("invalid YAML: {error}"),
                })?;
            let mut redacted = Vec::new();
            redact_yaml(&mut value, &mut Vec::new(), &mut redacted)?;
            let output = if redacted.is_empty() {
                bytes.to_vec()
            } else {
                serde_yaml_ng::to_string(&value)
                    .map_err(|error| SnapshotError::StructuredData {
                        path: display_path.to_owned(),
                        message: format!("cannot encode redacted YAML: {error}"),
                    })?
                    .into_bytes()
            };
            Ok(SanitizedContent {
                bytes: output,
                redacted_paths: redacted,
            })
        }
    }
}

pub(crate) fn merge_current_secrets(
    policy: &SnapshotFilePolicy,
    snapshot: &[u8],
    snapshot_redacted_paths: &[String],
    current: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let Some(current) = current else {
        return Ok(snapshot.to_vec());
    };
    let snapshot_text =
        std::str::from_utf8(snapshot).map_err(|error| SnapshotError::StructuredData {
            path: policy.manifest_path.to_owned(),
            message: format!("snapshot is not UTF-8: {error}"),
        })?;
    let current_text =
        std::str::from_utf8(current).map_err(|error| SnapshotError::StructuredData {
            path: policy.manifest_path.to_owned(),
            message: format!("current file is not UTF-8: {error}"),
        })?;
    match policy.format {
        StructuredFormat::Json => {
            let mut desired: JsonValue = serde_json::from_str(snapshot_text).map_err(|error| {
                SnapshotError::StructuredData {
                    path: policy.manifest_path.to_owned(),
                    message: format!("invalid snapshot JSON: {error}"),
                }
            })?;
            let current: JsonValue = serde_json::from_str(current_text).map_err(|error| {
                SnapshotError::StructuredData {
                    path: policy.manifest_path.to_owned(),
                    message: format!("current JSON is invalid; refusing to overwrite it: {error}"),
                }
            })?;
            let mut current_clone = current.clone();
            let mut current_sensitive = Vec::new();
            redact_json(&mut current_clone, &mut Vec::new(), &mut current_sensitive)?;
            let paths = redaction_union(snapshot_redacted_paths, &current_sensitive)?;
            for pointer in paths {
                if let Some(secret) = get_json_pointer(&current, &pointer)? {
                    insert_json_pointer(&mut desired, &pointer, secret.clone())?;
                }
            }
            if snapshot_redacted_paths.is_empty() && current_sensitive.is_empty() {
                Ok(snapshot.to_vec())
            } else {
                serde_json::to_vec_pretty(&desired).map_err(|error| SnapshotError::StructuredData {
                    path: policy.manifest_path.to_owned(),
                    message: format!("cannot encode restored JSON: {error}"),
                })
            }
        }
        StructuredFormat::Yaml => {
            let mut desired: YamlValue =
                serde_yaml_ng::from_str(snapshot_text).map_err(|error| {
                    SnapshotError::StructuredData {
                        path: policy.manifest_path.to_owned(),
                        message: format!("invalid snapshot YAML: {error}"),
                    }
                })?;
            let current: YamlValue = serde_yaml_ng::from_str(current_text).map_err(|error| {
                SnapshotError::StructuredData {
                    path: policy.manifest_path.to_owned(),
                    message: format!("current YAML is invalid; refusing to overwrite it: {error}"),
                }
            })?;
            let mut current_clone = current.clone();
            let mut current_sensitive = Vec::new();
            redact_yaml(&mut current_clone, &mut Vec::new(), &mut current_sensitive)?;
            let paths = redaction_union(snapshot_redacted_paths, &current_sensitive)?;
            for pointer in paths {
                if let Some(secret) = get_yaml_pointer(&current, &pointer)? {
                    insert_yaml_pointer(&mut desired, &pointer, secret.clone())?;
                }
            }
            if snapshot_redacted_paths.is_empty() && current_sensitive.is_empty() {
                Ok(snapshot.to_vec())
            } else {
                serde_yaml_ng::to_string(&desired)
                    .map(|text| text.into_bytes())
                    .map_err(|error| SnapshotError::StructuredData {
                        path: policy.manifest_path.to_owned(),
                        message: format!("cannot encode restored YAML: {error}"),
                    })
            }
        }
    }
}

fn redaction_union(snapshot: &[String], current: &[String]) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    for path in snapshot.iter().chain(current) {
        validate_redacted_path(path)?;
        paths.insert(path.clone());
        if paths.len() > MAX_REDACTED_PATHS {
            return Err(SnapshotError::InvalidManifest(
                "redacted path count exceeds bound".to_owned(),
            ));
        }
    }
    Ok(paths.into_iter().collect())
}

fn redact_json(
    value: &mut JsonValue,
    path: &mut Vec<String>,
    redacted: &mut Vec<String>,
) -> Result<()> {
    match value {
        JsonValue::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                path.push(key.clone());
                if is_sensitive_key(&key) {
                    map.remove(&key);
                    push_redacted(path, redacted)?;
                } else if let Some(child) = map.get_mut(&key) {
                    redact_json(child, path, redacted)?;
                }
                path.pop();
            }
        }
        JsonValue::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                redact_json(item, path, redacted)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn redact_yaml(
    value: &mut YamlValue,
    path: &mut Vec<String>,
    redacted: &mut Vec<String>,
) -> Result<()> {
    match value {
        YamlValue::Mapping(map) => {
            let keys: Vec<YamlValue> = map.keys().cloned().collect();
            for key in keys {
                let Some(key_text) = key.as_str() else {
                    return Err(SnapshotError::StructuredData {
                        path: encode_pointer(path),
                        message: "non-string YAML mapping key cannot be tracked safely".to_owned(),
                    });
                };
                path.push(key_text.to_owned());
                if is_sensitive_key(key_text) {
                    map.remove(&key);
                    push_redacted(path, redacted)?;
                } else if let Some(child) = map.get_mut(&key) {
                    redact_yaml(child, path, redacted)?;
                }
                path.pop();
            }
        }
        YamlValue::Sequence(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                redact_yaml(item, path, redacted)?;
                path.pop();
            }
        }
        YamlValue::Tagged(tagged) => redact_yaml(&mut tagged.value, path, redacted)?,
        _ => {}
    }
    Ok(())
}

fn push_redacted(path: &[String], redacted: &mut Vec<String>) -> Result<()> {
    if redacted.len() >= MAX_REDACTED_PATHS {
        return Err(SnapshotError::StructuredData {
            path: "snapshot".to_owned(),
            message: "sensitive field count exceeds bound".to_owned(),
        });
    }
    let pointer = encode_pointer(path);
    if pointer.len() > MAX_REDACTED_PATH_BYTES {
        return Err(SnapshotError::StructuredData {
            path: "snapshot".to_owned(),
            message: "sensitive field path exceeds bound".to_owned(),
        });
    }
    redacted.push(pointer);
    Ok(())
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "apikey"
            | "apitoken"
            | "token"
            | "authtoken"
            | "accesstoken"
            | "refreshtoken"
            | "password"
            | "passphrase"
            | "secret"
            | "secretkey"
            | "clientsecret"
            | "privatekey"
    )
}

fn encode_pointer(path: &[String]) -> String {
    path.iter().fold(String::new(), |mut pointer, segment| {
        pointer.push('/');
        pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
        pointer
    })
}

fn decode_pointer(pointer: &str) -> Result<Vec<String>> {
    validate_redacted_path(pointer)?;
    pointer[1..]
        .split('/')
        .map(|segment| {
            let mut decoded = String::new();
            let mut characters = segment.chars();
            while let Some(character) = characters.next() {
                if character != '~' {
                    decoded.push(character);
                    continue;
                }
                match characters.next() {
                    Some('0') => decoded.push('~'),
                    Some('1') => decoded.push('/'),
                    _ => {
                        return Err(SnapshotError::InvalidManifest(format!(
                            "invalid JSON pointer escape in {pointer}"
                        )));
                    }
                }
            }
            Ok(decoded)
        })
        .collect()
}

fn validate_redacted_path(pointer: &str) -> Result<()> {
    if pointer.is_empty()
        || !pointer.starts_with('/')
        || pointer.len() > MAX_REDACTED_PATH_BYTES
        || pointer.chars().any(char::is_control)
    {
        return Err(SnapshotError::InvalidManifest(format!(
            "invalid redacted path: {pointer}"
        )));
    }
    Ok(())
}

fn get_json_pointer<'a>(value: &'a JsonValue, pointer: &str) -> Result<Option<&'a JsonValue>> {
    let segments = decode_pointer(pointer)?;
    let mut current = value;
    for segment in segments {
        current = match current {
            JsonValue::Object(map) => match map.get(&segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            JsonValue::Array(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    SnapshotError::InvalidManifest(format!("invalid array path: {pointer}"))
                })?;
                match items.get(index) {
                    Some(value) => value,
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

fn insert_json_pointer(value: &mut JsonValue, pointer: &str, secret: JsonValue) -> Result<()> {
    let segments = decode_pointer(pointer)?;
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| SnapshotError::InvalidManifest(format!("empty secret path: {pointer}")))?;
    let mut current = value;
    for segment in parents {
        current = match current {
            JsonValue::Object(map) => map.get_mut(segment),
            JsonValue::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index)),
            _ => None,
        }
        .ok_or_else(|| SnapshotError::StructuredData {
            path: pointer.to_owned(),
            message: "cannot preserve secret because its parent path changed".to_owned(),
        })?;
    }
    match current {
        JsonValue::Object(map) => {
            map.insert(last.clone(), secret);
            Ok(())
        }
        JsonValue::Array(items) => {
            let index = last
                .parse::<usize>()
                .map_err(|_| SnapshotError::StructuredData {
                    path: pointer.to_owned(),
                    message: "cannot preserve secret at non-numeric array index".to_owned(),
                })?;
            let slot = items
                .get_mut(index)
                .ok_or_else(|| SnapshotError::StructuredData {
                    path: pointer.to_owned(),
                    message: "cannot preserve secret because array shape changed".to_owned(),
                })?;
            *slot = secret;
            Ok(())
        }
        _ => Err(SnapshotError::StructuredData {
            path: pointer.to_owned(),
            message: "cannot preserve secret because its container changed".to_owned(),
        }),
    }
}

fn get_yaml_pointer<'a>(value: &'a YamlValue, pointer: &str) -> Result<Option<&'a YamlValue>> {
    let segments = decode_pointer(pointer)?;
    let mut current = unwrap_yaml_tag(value);
    for segment in segments {
        current = unwrap_yaml_tag(match current {
            YamlValue::Mapping(map) => match map.get(YamlValue::String(segment)) {
                Some(value) => value,
                None => return Ok(None),
            },
            YamlValue::Sequence(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    SnapshotError::InvalidManifest(format!("invalid YAML array path: {pointer}"))
                })?;
                match items.get(index) {
                    Some(value) => value,
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        });
    }
    Ok(Some(current))
}

fn insert_yaml_pointer(value: &mut YamlValue, pointer: &str, secret: YamlValue) -> Result<()> {
    let segments = decode_pointer(pointer)?;
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| SnapshotError::InvalidManifest(format!("empty secret path: {pointer}")))?;
    let mut current = value;
    for segment in parents {
        current = unwrap_yaml_tag_mut(current);
        current = match current {
            YamlValue::Mapping(map) => map.get_mut(YamlValue::String(segment.clone())),
            YamlValue::Sequence(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index)),
            _ => None,
        }
        .ok_or_else(|| SnapshotError::StructuredData {
            path: pointer.to_owned(),
            message: "cannot preserve secret because its parent path changed".to_owned(),
        })?;
    }
    current = unwrap_yaml_tag_mut(current);
    match current {
        YamlValue::Mapping(map) => {
            map.insert(YamlValue::String(last.clone()), secret);
            Ok(())
        }
        YamlValue::Sequence(items) => {
            let index = last
                .parse::<usize>()
                .map_err(|_| SnapshotError::StructuredData {
                    path: pointer.to_owned(),
                    message: "cannot preserve secret at non-numeric array index".to_owned(),
                })?;
            let slot = items
                .get_mut(index)
                .ok_or_else(|| SnapshotError::StructuredData {
                    path: pointer.to_owned(),
                    message: "cannot preserve secret because array shape changed".to_owned(),
                })?;
            *slot = secret;
            Ok(())
        }
        _ => Err(SnapshotError::StructuredData {
            path: pointer.to_owned(),
            message: "cannot preserve secret because its container changed".to_owned(),
        }),
    }
}

fn unwrap_yaml_tag(mut value: &YamlValue) -> &YamlValue {
    while let YamlValue::Tagged(tagged) = value {
        value = &tagged.value;
    }
    value
}

fn unwrap_yaml_tag_mut(mut value: &mut YamlValue) -> &mut YamlValue {
    while let YamlValue::Tagged(tagged) = value {
        value = &mut tagged.value;
    }
    value
}

pub(crate) fn plugin_count_from_package_bytes(bytes: &[u8]) -> u64 {
    serde_json::from_slice::<JsonValue>(bytes)
        .ok()
        .and_then(|value| {
            value
                .pointer("/dsh/profile/bundles")
                .and_then(JsonValue::as_array)
                .map(|bundles| bundles.len() as u64)
        })
        .unwrap_or(0)
}

pub(crate) fn read_manifest_bounded(directory: &Path) -> Result<SnapshotManifest> {
    let manifest_path = directory.join("manifest.json");
    let Some((bytes, _)) = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)? else {
        return Err(SnapshotError::InvalidManifest(format!(
            "missing {}",
            manifest_path.display()
        )));
    };
    serde_json::from_slice(&bytes).map_err(|error| {
        SnapshotError::InvalidManifest(format!("cannot parse {}: {error}", manifest_path.display()))
    })
}

pub(crate) fn load_valid_snapshot(
    store: &SnapshotStore,
    directory: &Path,
) -> Result<SnapshotManifest> {
    validate_directory_tree(store.data_root(), directory)?;
    let manifest = read_manifest_bounded(directory)?;
    validate_identifier(&manifest.snapshot_id)?;
    validate_metadata_text("DSH version", &manifest.dsh_version, 128)?;
    if let SnapshotKind::Manual { label: Some(label) } = &manifest.kind {
        validate_metadata_text("manual label", label, 128)?;
    }
    if manifest.schema != SNAPSHOT_SCHEMA_VERSION {
        return Err(SnapshotError::InvalidManifest(format!(
            "unsupported schema {}",
            manifest.schema
        )));
    }
    if manifest.profile_name != store.profile_name() {
        return Err(SnapshotError::InvalidManifest(format!(
            "profile {} does not match store {}",
            manifest.profile_name,
            store.profile_name()
        )));
    }
    if manifest.files.len() != FILE_POLICY.len() {
        return Err(SnapshotError::InvalidManifest(format!(
            "expected {} files, found {}",
            FILE_POLICY.len(),
            manifest.files.len()
        )));
    }
    let files_dir = directory.join("files");
    validate_directory_tree(store.data_root(), &files_dir)?;
    let mut total = 0_u64;
    let mut source_total = 0_u64;
    let mut present_count = 0_u64;
    let mut actual_plugin_count = None;
    let mut expected_blobs = BTreeSet::new();
    for (index, (entry, policy)) in manifest.files.iter().zip(FILE_POLICY.iter()).enumerate() {
        if entry.path != policy.manifest_path || entry.scope != policy.scope {
            return Err(SnapshotError::InvalidPath(entry.path.clone()));
        }
        if entry.source_size > policy.max_bytes || entry.stored_size > policy.max_bytes {
            return Err(SnapshotError::Oversized {
                path: entry.path.clone(),
                size: entry.source_size.max(entry.stored_size),
                limit: policy.max_bytes,
            });
        }
        source_total = source_total.checked_add(entry.source_size).ok_or_else(|| {
            SnapshotError::Oversized {
                path: "snapshot source total".to_owned(),
                size: u64::MAX,
                limit: MAX_TOTAL_SNAPSHOT_BYTES,
            }
        })?;
        if entry.mode.is_some_and(|mode| mode & !0o777 != 0) {
            return Err(SnapshotError::InvalidManifest(format!(
                "invalid mode for {}",
                entry.path
            )));
        }
        if entry.redacted_paths.len() > MAX_REDACTED_PATHS {
            return Err(SnapshotError::InvalidManifest(format!(
                "too many redacted paths for {}",
                entry.path
            )));
        }
        for pointer in &entry.redacted_paths {
            validate_redacted_path(pointer)?;
            let _ = decode_pointer(pointer)?;
        }
        match entry.state {
            SnapshotFileState::Present => {
                present_count += 1;
                let expected_hash = entry.sha256.as_deref().ok_or_else(|| {
                    SnapshotError::InvalidManifest(format!("{} has no hash", entry.path))
                })?;
                validate_sha256(expected_hash)?;
                if entry.mode.is_none() || entry.omitted_reason.is_some() {
                    return Err(SnapshotError::InvalidManifest(format!(
                        "{} has inconsistent present metadata",
                        entry.path
                    )));
                }
                let blob_name = index.to_string();
                expected_blobs.insert(blob_name.clone());
                let blob = files_dir.join(&blob_name);
                let Some((bytes, _)) = read_regular_bounded(&blob, policy.max_bytes)? else {
                    return Err(SnapshotError::Integrity(format!(
                        "missing blob for {}",
                        entry.path
                    )));
                };
                if bytes.len() as u64 != entry.stored_size {
                    return Err(SnapshotError::Integrity(format!(
                        "size mismatch for {}",
                        entry.path
                    )));
                }
                let actual = sha256_hex(&bytes);
                if actual != expected_hash {
                    return Err(SnapshotError::Integrity(format!(
                        "SHA-256 mismatch for {}",
                        entry.path
                    )));
                }
                if index == 0 {
                    validate_profile_package(&bytes)?;
                    actual_plugin_count = Some(plugin_count_from_package_bytes(&bytes));
                }
                total = total.checked_add(entry.stored_size).ok_or_else(|| {
                    SnapshotError::Oversized {
                        path: "snapshot total".to_owned(),
                        size: u64::MAX,
                        limit: MAX_TOTAL_SNAPSHOT_BYTES,
                    }
                })?;
            }
            SnapshotFileState::Missing => {
                if entry.source_size != 0
                    || entry.stored_size != 0
                    || entry.sha256.is_some()
                    || entry.mode.is_some()
                    || !entry.redacted_paths.is_empty()
                    || entry.omitted_reason.is_some()
                {
                    return Err(SnapshotError::InvalidManifest(format!(
                        "{} has inconsistent missing metadata",
                        entry.path
                    )));
                }
            }
            SnapshotFileState::Omitted => {
                if entry.stored_size != 0
                    || entry.sha256.is_some()
                    || !entry.redacted_paths.is_empty()
                    || entry.omitted_reason.as_ref().is_none_or(|reason| {
                        reason.is_empty() || reason.len() > MAX_OMISSION_REASON_BYTES
                    })
                {
                    return Err(SnapshotError::InvalidManifest(format!(
                        "{} has inconsistent omitted metadata",
                        entry.path
                    )));
                }
            }
        }
    }
    if total > MAX_TOTAL_SNAPSHOT_BYTES
        || source_total > MAX_TOTAL_SNAPSHOT_BYTES
        || total != manifest.total_bytes
        || present_count != manifest.file_count
    {
        return Err(SnapshotError::Integrity(
            "snapshot aggregate counts do not match manifest".to_owned(),
        ));
    }
    if manifest.files[0].state != SnapshotFileState::Present {
        return Err(SnapshotError::InvalidManifest(
            "profile/package.json must be present in every restorable snapshot".to_owned(),
        ));
    }
    if actual_plugin_count != Some(manifest.plugin_count) {
        return Err(SnapshotError::Integrity(
            "snapshot plugin count does not match profile/package.json".to_owned(),
        ));
    }
    let mut actual_blobs = BTreeSet::new();
    for entry in fs::read_dir(&files_dir).map_err(|error| {
        SnapshotError::io(
            format!("read blob directory {}", files_dir.display()),
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            SnapshotError::io(format!("read blob entry in {}", files_dir.display()), error)
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            SnapshotError::io(format!("inspect blob {}", entry.path().display()), error)
        })?;
        if !metadata.is_file() || is_link_or_reparse(&metadata) {
            return Err(SnapshotError::UnsafePath(format!(
                "unexpected blob type: {}",
                entry.path().display()
            )));
        }
        actual_blobs.insert(entry.file_name().to_string_lossy().into_owned());
        if actual_blobs.len() > FILE_POLICY.len() {
            return Err(SnapshotError::InvalidManifest(
                "too many snapshot blobs".to_owned(),
            ));
        }
    }
    if actual_blobs != expected_blobs {
        return Err(SnapshotError::InvalidManifest(
            "snapshot contains missing or unexpected blobs".to_owned(),
        ));
    }
    Ok(manifest)
}

pub(crate) fn read_snapshot_blob(
    store: &SnapshotStore,
    snapshot_id: &str,
    index: usize,
) -> Result<Vec<u8>> {
    let directory = store.snapshot_directory(snapshot_id)?;
    let policy = FILE_POLICY
        .get(index)
        .ok_or_else(|| SnapshotError::InvalidPath(format!("snapshot blob index {index}")))?;
    let blob = directory.join("files").join(index.to_string());
    let Some((bytes, _)) = read_regular_bounded(&blob, policy.max_bytes)? else {
        return Err(SnapshotError::Integrity(format!(
            "missing blob for {}",
            policy.manifest_path
        )));
    };
    Ok(bytes)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

pub(crate) fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SnapshotError::InvalidManifest(format!(
            "invalid SHA-256: {value}"
        )));
    }
    Ok(())
}

pub(crate) fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("file has no parent: {}", path.display()))
    })?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || is_link_or_reparse(&metadata) => {
            return Err(SnapshotError::UnsafePath(format!(
                "durable-write destination is not a regular file: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(SnapshotError::io(
                format!("inspect durable-write destination {}", path.display()),
                error,
            ));
        }
    }
    let temporary = parent.join(format!(".tmp-{}", new_identifier("write")));
    let mut file = nexus_private_file::create_new_private(&temporary)
        .map_err(|error| {
            SnapshotError::io(
                format!("create temporary file {}", temporary.display()),
                error,
            )
        })?;
    let result = (|| {
        file.write_all(bytes).map_err(|error| {
            SnapshotError::io(
                format!("write temporary file {}", temporary.display()),
                error,
            )
        })?;
        file.sync_all().map_err(|error| {
            SnapshotError::io(
                format!("sync temporary file {}", temporary.display()),
                error,
            )
        })?;
        drop(file);
        replace_file(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn rename_durable(source: &Path, destination: &Path) -> Result<()> {
    move_file_durable(source, destination, false)?;
    sync_rename_parents(source, destination)
}

pub(crate) fn replace_durable(source: &Path, destination: &Path) -> Result<()> {
    move_file_durable(source, destination, true)?;
    sync_rename_parents(source, destination)
}

#[cfg(windows)]
fn move_file_durable(source: &Path, destination: &Path, replace: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let result = unsafe { MoveFileExW(source_wide.as_ptr(), destination_wide.as_ptr(), flags) };
    if result == 0 {
        return Err(SnapshotError::io(
            format!(
                "durably move {} to {}",
                source.display(),
                destination.display()
            ),
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn move_file_durable(source: &Path, destination: &Path, _replace: bool) -> Result<()> {
    fs::rename(source, destination).map_err(|error| {
        SnapshotError::io(
            format!(
                "durably move {} to {}",
                source.display(),
                destination.display()
            ),
            error,
        )
    })
}

fn sync_rename_parents(source: &Path, destination: &Path) -> Result<()> {
    let source_parent = source.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("source has no parent: {}", source.display()))
    })?;
    let destination_parent = destination.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!(
            "destination has no parent: {}",
            destination.display()
        ))
    })?;
    sync_directory(source_parent)?;
    if destination_parent != source_parent {
        sync_directory(destination_parent)?;
    }
    Ok(())
}

pub(crate) fn remove_directory_all_durable(directory: &Path) -> Result<()> {
    let parent = directory.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("directory has no parent: {}", directory.display()))
    })?;
    fs::remove_dir_all(directory).map_err(|error| {
        SnapshotError::io(format!("remove directory {}", directory.display()), error)
    })?;
    sync_directory(parent)
}

pub(crate) fn remove_empty_directory_durable(directory: &Path) -> Result<()> {
    let parent = directory.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("directory has no parent: {}", directory.display()))
    })?;
    fs::remove_dir(directory).map_err(|error| {
        SnapshotError::io(format!("remove directory {}", directory.display()), error)
    })?;
    sync_directory(parent)
}

/// Remove a potentially secret-bearing file before a durable transaction state
/// can claim cleanup. On Windows, first durably rename it to a deterministic
/// tombstone and sync an empty truncation. If the final directory deletion is
/// replayed after power loss, no secret bytes remain in the namespace entry.
pub(crate) fn remove_sensitive_file_durable(path: &Path, tombstone: &Path) -> Result<()> {
    if validate_regular_file_if_present(tombstone)? {
        truncate_and_sync(tombstone)?;
        fs::remove_file(tombstone).map_err(|error| {
            SnapshotError::io(format!("remove tombstone {}", tombstone.display()), error)
        })?;
        sync_directory(tombstone.parent().ok_or_else(|| {
            SnapshotError::UnsafePath(format!("tombstone has no parent: {}", tombstone.display()))
        })?)?;
    }
    if !validate_regular_file_if_present(path)? {
        return Ok(());
    }
    rename_durable(path, tombstone)?;
    truncate_and_sync(tombstone)?;
    fs::remove_file(tombstone).map_err(|error| {
        SnapshotError::io(format!("remove tombstone {}", tombstone.display()), error)
    })?;
    sync_directory(tombstone.parent().ok_or_else(|| {
        SnapshotError::UnsafePath(format!("tombstone has no parent: {}", tombstone.display()))
    })?)
}

fn validate_regular_file_if_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&metadata) => Ok(true),
        Ok(_) => Err(SnapshotError::UnsafePath(format!(
            "expected regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SnapshotError::io(
            format!("inspect file {}", path.display()),
            error,
        )),
    }
}

fn truncate_and_sync(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| SnapshotError::io(format!("truncate {}", path.display()), error))?;
    file.sync_all()
        .map_err(|error| SnapshotError::io(format!("sync {}", path.display()), error))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    move_file_durable(source, destination, true)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    move_file_durable(source, destination, true)
}

pub(crate) fn sync_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                SnapshotError::io(format!("sync directory {}", directory.display()), error)
            })?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

pub(crate) fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
            .map_err(|error| SnapshotError::io(format!("set mode on {}", path.display()), error))?;
    }
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(path)
            .map_err(|error| SnapshotError::io(format!("inspect {}", path.display()), error))?
            .permissions();
        permissions.set_readonly(mode & 0o200 == 0);
        fs::set_permissions(path, permissions)
            .map_err(|error| SnapshotError::io(format!("set mode on {}", path.display()), error))?;
    }
    Ok(())
}

fn portable_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(windows)]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o600
        }
    }
}

fn set_private_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            SnapshotError::io(format!("secure directory {}", path.display()), error)
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
    }
    #[cfg(not(windows))]
    {
        let _ = REPARSE_POINT_ATTRIBUTE;
        false
    }
}

fn truncate_string(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

#[cfg(test)]
mod private_write_tests {
    #[test]
    fn raw_restore_backup_is_private_and_replacement_keeps_permissions() {
        let root = std::env::temp_dir().join(super::new_identifier("private-rollback-test"));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("rollback.json");
        for bytes in [b"original secret".as_slice(), b"next secret".as_slice()] {
            super::write_durable(&path, bytes).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            nexus_private_file::verify_private(&std::fs::File::open(&path).unwrap()).unwrap();
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
