//! Explicit downloads only. Startup consumes a pinned local configuration file.
use std::{io, time::Duration};
use sha2::{Digest, Sha256};
use nexus_protocol::HarnessPreferencesPayload;
use nexus_core::NexusPaths;
pub(crate) const LIMIT: u64 = 1024 * 1024;
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

#[derive(Clone)]
struct PatchPreview {
    _lease: std::sync::Arc<nexus_core::maintenance::PatchReadLease>,
    root: std::path::PathBuf,
    revision: String,
    preferences: HarnessPreferencesPayload,
    files: Vec<(std::path::PathBuf, String)>,
    expires: u64,
}
static PREVIEWS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<String, PatchPreview>>> = std::sync::OnceLock::new();
fn previews() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, PatchPreview>> { PREVIEWS.get_or_init(Default::default) }

pub(crate) async fn list_refs(entry: &nexus_protocol::HarnessPatchEntry, page: u16) -> io::Result<serde_json::Value> {
    if !(1..=20).contains(&page) { return Err(invalid("GitHub reference page must be between 1 and 20; enter a ref manually beyond this limit")); }
    let source = source_url(&entry.source)?;
    if !entry.source.starts_with("https://github.com/") { return Err(invalid("Reference lists require a GitHub file URL")); }
    let parts: Vec<_> = source.path().split('/').filter(|s| !s.is_empty()).collect();
    let kind = entry.github_ref_kind.as_deref().unwrap_or("branch");
    let endpoint = match kind { "branch" => "branches", "tag" => "tags", _ => return Err(invalid("Commit references are entered directly")) };
    let url = reqwest::Url::parse(&format!("https://api.github.com/repos/{}/{}/{endpoint}?per_page=50&page={page}", parts[0], parts[1])).map_err(|_| invalid("Invalid GitHub repository"))?;
    let client = reqwest::Client::builder().https_only(true).redirect(reqwest::redirect::Policy::none()).user_agent("Nexus-Launcher")
        .connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(15)).build().map_err(io::Error::other)?;
    let value = github_object(&client, url).await?;
    let values = value.as_array().ok_or_else(|| invalid("Invalid GitHub reference list"))?;
    if values.len() > 50 { return Err(invalid("GitHub reference list exceeds limit")); }
    let entries = values.iter().map(|value| {
        let name = value.get("name").and_then(|v| v.as_str()).filter(|s| s.len() <= 256).ok_or_else(|| invalid("Invalid GitHub reference name"))?;
        let commit = value.pointer("/commit/sha").and_then(|v| v.as_str()).filter(|s| commit_sha(s)).ok_or_else(|| invalid("Invalid GitHub reference commit"))?;
        Ok(serde_json::json!({"name":name,"commit":commit}))
    }).collect::<io::Result<Vec<_>>>()?;
    Ok(serde_json::json!({"entries":entries,"page":page,"next_page":if values.len() == 50 && page < 20 {Some(page+1)} else {None},"limited":page==20 && values.len()==50}))
}

fn read_patch(path: &std::path::Path) -> io::Result<Vec<u8>> {
    nexus_core::read_regular_file_bounded(path, LIMIT)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Patch file is missing: {}", path.display())))
}
fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn bounded_changes(old: &[u8], new: &[u8]) -> serde_json::Value {
    let (old, old_redacted) = nexus_core::redact_diagnostics_payload(old);
    let (new, new_redacted) = nexus_core::redact_diagnostics_payload(new);
    let old = String::from_utf8_lossy(&old); let new = String::from_utf8_lossy(&new);
    let before: Vec<_> = old.lines().collect(); let after: Vec<_> = new.lines().collect();
    let mut lines = Vec::new(); let mut budget = 16 * 1024usize; let mut changed = 0usize; let mut text_truncated = false;
    for index in 0..before.len().max(after.len()) {
        if before.get(index) == after.get(index) { continue; } changed += 1;
        if lines.len() >= 80 || budget == 0 { continue; }
        text_truncated |= [before.get(index), after.get(index)].into_iter().flatten().any(|line| line.chars().count() > 256);
        let trim = |line: Option<&&str>| line.map(|s| s.chars().take(256).collect::<String>());
        let row = serde_json::json!({"line":index+1,"before":trim(before.get(index)),"after":trim(after.get(index))});
        let size = row.to_string().len(); if size > budget { budget = 0; continue; } budget -= size; lines.push(row);
    }
    serde_json::json!({"changed_lines":changed,"truncated":text_truncated || changed>lines.len(),"redacted":old_redacted||new_redacted,"lines":lines})
}
pub(crate) async fn preview_update(paths: &NexusPaths, revision: &str, p: HarnessPreferencesPayload) -> io::Result<serde_json::Value> {
    let lease=nexus_core::maintenance::protect_patch_cache(paths)?;
    {
        let mut pending = previews().lock().map_err(|_| io::Error::other("Patch preview lock failed"))?;
        pending.retain(|_, value| value.expires > nexus_core::unix_time_seconds());
        if pending.len() >= 16 { return Err(invalid("Too many pending patch previews; cancel a preview first")); }
    }
    let old = nexus_core::ConfigStore::new(paths.clone()).snapshot()?;
    if old.revision != revision { return Err(invalid("Configuration changed; reload before previewing")); }
    let previous = old.document.harness_preferences.unwrap_or_default();
    let next = fetch(paths, p).await?;
    let mut rows = Vec::new(); let mut files = Vec::new();
    for (index, entry) in enabled_entries(&next).into_iter().enumerate() {
        let path = if entry.source.starts_with("https://") { paths.root.join("patches").join(format!("{}.yml", entry.sha256.as_deref().ok_or_else(|| invalid("Patch has not been downloaded"))?)) } else { std::path::PathBuf::from(&entry.source) };
        let bytes = read_patch(&path)?; let hash = digest(&bytes);
        let prior = previous.patch_entries.iter().flatten().find(|prior| nexus_core::patch_identity(prior) == nexus_core::patch_identity(&entry));
        let old_hash = prior.and_then(|prior| prior.sha256.as_deref());
        // A missing/damaged baseline must not prevent downloading its repair.
        let old_bytes = old_hash.and_then(|hash| read_patch(&paths.root.join("patches").join(format!("{hash}.yml"))).ok());
        rows.push(serde_json::json!({"index":index,"source":entry.source,"old_sha256":old_hash,"new_sha256":hash,"old_commit":prior.and_then(|p|p.resolved_commit.as_deref()),"new_commit":entry.resolved_commit,"new_bytes":bytes.len(),"old_bytes":old_bytes.as_ref().map(Vec::len),"changes":bounded_changes(old_bytes.as_deref().unwrap_or(&[]),&bytes)}));
        files.push((path, hash));
    }
    let id = nexus_core::agent_auth::random_hex()?; let now = nexus_core::unix_time_seconds();
    let mut previews = previews().lock().map_err(|_| io::Error::other("Patch preview lock failed"))?;
    previews.retain(|_, value| value.expires > now);
    if previews.len() >= 16 { return Err(invalid("Too many pending patch previews; wait for a preview to expire")); }
    previews.insert(id.clone(), PatchPreview { _lease:lease, root:paths.root.clone(), revision:revision.into(), preferences:next, files, expires:now+600 });
    let expiring_id=id.clone(); tokio::spawn(async move {tokio::time::sleep(Duration::from_secs(600)).await; finish_preview(&expiring_id);});
    Ok(serde_json::json!({"preview_id":id,"revision":revision,"expires_at_unix":now+600,"entries":rows}))
}
pub(crate) fn preview_candidate(paths: &NexusPaths, revision: &str, id: &str) -> io::Result<HarnessPreferencesPayload> {
    let preview = previews().lock().map_err(|_| io::Error::other("Patch preview lock failed"))?.get(id).cloned().ok_or_else(|| invalid("Patch preview expired; create a new preview"))?;
    if preview.root != paths.root || preview.revision != revision || preview.expires <= nexus_core::unix_time_seconds() { return Err(invalid("Patch preview no longer matches this configuration; create a new preview")); }
    for (path, hash) in preview.files { if digest(&read_patch(&path)?) != hash { return Err(invalid("Previewed patch contents changed; create a new preview")); } }
    Ok(preview.preferences)
}
pub(crate) fn finish_preview(id: &str) { if let Ok(mut previews) = previews().lock() { previews.remove(id); } }
pub(crate) fn discard_preview(paths: &NexusPaths, id: &str) {
    if let Ok(mut pending) = previews().lock() {
        if pending.get(id).is_some_and(|value| value.root == paths.root) { pending.remove(id); }
    }
}
fn matches_file(entry: &nexus_protocol::HarnessPatchEntry, path: &str) -> bool {
    entry.source == path || (entry.source.starts_with("https://") && std::path::Path::new(path).file_name().and_then(|n| n.to_str()) == Some(&format!("{}.yml", entry.sha256.as_deref().unwrap_or("not-downloaded"))))
}

fn enabled_entries(p: &HarnessPreferencesPayload) -> Vec<nexus_protocol::HarnessPatchEntry> {
    let mut entries: Vec<_> = p.patch_entries.iter().flatten().filter(|entry| entry.enabled).cloned().collect();
    for path in p.patches.iter().flatten() {
        // A legacy launch path may still enable the cached contents of a
        // disabled remote entry. Preserve the remote source identity rather
        // than treating its cache pathname as an unrelated local patch.
        let known: Vec<_> = p.patch_entries.iter().flatten().filter(|entry| matches_file(entry, path)).collect();
        if known.is_empty() {
            entries.push(nexus_protocol::HarnessPatchEntry { source: path.clone(), enabled: true, ..Default::default() });
        } else {
            for entry in known {
                if !entries.iter().any(|enabled| nexus_core::patch_identity(enabled) == nexus_core::patch_identity(entry)) {
                    let mut active = entry.clone(); active.enabled = true; entries.push(active);
                }
            }
        }
    }
    entries
}
pub(crate) fn record_failure(paths: &NexusPaths, p: &HarnessPreferencesPayload, stage: &str) -> io::Result<()> {
    let directory = paths.root.join("patch-failures");
    for entry in enabled_entries(p) {
        std::fs::create_dir_all(&directory)?;
        if nexus_core::path_is_reparse(&std::fs::symlink_metadata(&directory)?) { return Err(invalid("Patch failure directory must not be a link")); }
        nexus_core::write_private_json_atomic(&directory, &directory.join(format!("{}.json", nexus_core::patch_identity(&entry))),
            &serde_json::json!({"format_version":1,"stage":stage,"observed_at_unix":nexus_core::unix_time_seconds()}))?;
    }
    Ok(())
}
pub(crate) fn acknowledge_disabled(paths: &NexusPaths, p: &HarnessPreferencesPayload) -> io::Result<()> {
    // Called only after the configuration transaction commits. A failed save
    // cannot acknowledge a failure on the user's behalf.
    let enabled = enabled_entries(p).iter().map(nexus_core::patch_identity).collect::<std::collections::HashSet<_>>();
    for entry in p.patch_entries.iter().flatten().filter(|entry| !entry.enabled && !enabled.contains(&nexus_core::patch_identity(entry))) {
        let path = paths.root.join("patch-failures").join(format!("{}.json", nexus_core::patch_identity(entry)));
        match std::fs::remove_file(path) { Ok(()) => {}, Err(error) if error.kind() == io::ErrorKind::NotFound => {}, Err(error) => return Err(error) }
    }
    Ok(())
}
pub(crate) fn check_failures(paths: &NexusPaths, p: &HarnessPreferencesPayload) -> io::Result<()> {
    for entry in enabled_entries(p) {
        let path = paths.root.join("patch-failures").join(format!("{}.json", nexus_core::patch_identity(&entry)));
        if nexus_core::read_regular_file_bounded(&path, 8192)?.is_some() {
            return Err(invalid("An enabled patch previously failed. Disable it and save in Settings before launching. For an unclassified combination failure, disable the enabled patches one by one; no plugin was automatically removed."));
        }
    }
    Ok(())
}
pub(crate) fn validate_for_paths(paths: &NexusPaths, p: &HarnessPreferencesPayload) -> io::Result<()> {
    check_failures(paths, p)?;
    for path in p.patches.iter().flatten() {
        let entries = enabled_entries(p).into_iter().filter(|entry| matches_file(entry, path)).collect();
        let one = HarnessPreferencesPayload { patches: Some(vec![path.clone()]), patch_entries: Some(entries), ..Default::default() };
        if let Err(error) = validate(&one) { record_failure(paths, &one, "file_validation")?; return Err(error); }
    }
    Ok(())
}

async fn github_url(client: &reqwest::Client, entry: &mut nexus_protocol::HarnessPatchEntry) -> io::Result<reqwest::Url> {
    let url = reqwest::Url::parse(&entry.source).map_err(|_| invalid("Invalid patch URL"))?;
    let pieces: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if pieces.len() < 5 || pieces[2] != "blob" { return Err(invalid("Use a GitHub file URL")); }
    let kind = entry.github_ref_kind.as_deref().unwrap_or(if commit_sha(pieces[3]) { "commit" } else { "branch" });
    let name = entry.github_ref_name.as_deref().unwrap_or(pieces[3]);
    if kind == "commit" {
        if !commit_sha(name) { return Err(invalid("GitHub commit must be a complete 40-character hexadecimal SHA")); }
        let name = name.to_owned();
        let file = entry.github_file_path.clone().unwrap_or_else(|| pieces[4..].join("/"));
        entry.resolved_commit = Some(name.clone());
        return Ok(raw_commit_url(pieces[0], pieces[1], &name, &file));
    }
    let mut api = reqwest::Url::parse("https://api.github.com/").unwrap();
    api.path_segments_mut().unwrap().extend(["repos", pieces[0], pieces[1], "git", "ref", if kind == "tag" { "tags" } else { "heads" }]).push(name);
    let mut object = github_object(client, api).await?.get("object").cloned().ok_or_else(|| invalid("GitHub ref has no object"))?;
    for _ in 0..5 {
        let sha = object.get("sha").and_then(|v| v.as_str()).filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| invalid("Invalid GitHub commit"))?.to_owned();
        match object.get("type").and_then(|v| v.as_str()) {
            Some("commit") => {
                entry.resolved_commit = Some(sha.clone());
                let file_path = entry.github_file_path.clone().unwrap_or_else(|| {
                    let suffix = pieces[3..].join("/");
                    suffix.strip_prefix(&format!("{name}/")).map(str::to_owned).unwrap_or_else(|| pieces[4..].join("/"))
                });
                // set_path preserves percent escapes in copied GitHub URLs,
                // while encoding spaces and Unicode entered as a file path.
                return Ok(raw_commit_url(pieces[0], pieces[1], &sha, &file_path));
            },
            Some("tag") => {
                let api = reqwest::Url::parse(&format!("https://api.github.com/repos/{}/{}/git/tags/{sha}", pieces[0], pieces[1])).map_err(|_| invalid("Invalid GitHub tag"))?;
                object = github_object(client, api).await?.get("object").cloned().ok_or_else(|| invalid("GitHub tag has no object"))?;
            },
            _ => return Err(invalid("GitHub ref does not identify a commit")),
        }
    }
    Err(invalid("GitHub tag resolution limit exceeded"))
}
fn commit_sha(value: &str) -> bool { value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) }
fn raw_commit_url(owner: &str, repository: &str, sha: &str, file_path: &str) -> reqwest::Url {
    let mut raw = reqwest::Url::parse("https://raw.githubusercontent.com/").unwrap();
    raw.set_path(&format!("/{owner}/{repository}/{sha}/{file_path}")); raw
}
async fn github_object(client: &reqwest::Client, url: reqwest::Url) -> io::Result<serde_json::Value> {
    let mut response = client.get(url).send().await.map_err(|error| io::Error::other(error.without_url()))?;
    if !response.status().is_success() { return Err(io::Error::other(format!("GitHub ref lookup: HTTP {}", response.status()))); }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| io::Error::other(error.without_url()))? {
        if bytes.len() + chunk.len() > 64 * 1024 { return Err(invalid("GitHub response exceeds limit")); } bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("Invalid GitHub response"))
}

pub(crate) fn source_url(source: &str) -> io::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(source).map_err(|_| invalid("Invalid patch URL"))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() || url.host_str().is_none() { return Err(invalid("Patch URL must use HTTPS without credentials, query or fragment")); }
    if url.host_str() == Some("github.com") {
        let pieces: Vec<_> = url.path().split('/').filter(|s| !s.is_empty()).collect();
        if pieces.len() < 5 || pieces[2] != "blob" { return Err(invalid("Use a GitHub file URL or a raw HTTPS file URL")); }
        let path = format!("/{}/{}/{}", pieces[0], pieces[1], pieces[3..].join("/"));
        url.set_host(Some("raw.githubusercontent.com")).map_err(|_| invalid("Invalid patch URL"))?;
        url.set_path(&path);
    }
    Ok(url)
}

pub(crate) async fn fetch(paths: &NexusPaths, mut p: HarnessPreferencesPayload) -> io::Result<HarnessPreferencesPayload> {
    let _lease=nexus_core::maintenance::protect_patch_cache(paths)?;
    let client = reqwest::Client::builder().https_only(true).redirect(reqwest::redirect::Policy::none())
        .user_agent("Nexus-Launcher").connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(30)).build().map_err(|_| invalid("Unable to initialize patch downloader"))?;
    // One total budget, rather than up to 32 individual timeouts.
    let mut attempted = None;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        for (index, entry) in p.patch_entries.iter_mut().flatten().enumerate().filter(|(_, entry)| entry.enabled && entry.source.starts_with("https://")) {
            attempted = Some(entry.clone());
            let url = if entry.source.starts_with("https://github.com/") { github_url(&client, entry).await? } else { source_url(&entry.source)? };
            let mut response = client.get(url).send().await.map_err(|error| io::Error::other(format!("Patch {}: {}", index + 1, error.without_url())))?;
            if !response.status().is_success() { return Err(io::Error::other(format!("Patch {}: HTTP {}; redirects are not followed", index + 1, response.status()))); }
            if response.content_length().is_some_and(|n| n > LIMIT) { return Err(invalid("Patch exceeds the 1 MiB limit")); }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| io::Error::other(error.without_url()))? {
                if bytes.len() + chunk.len() > LIMIT as usize { return Err(invalid("Patch exceeds the 1 MiB limit")); }
                bytes.extend_from_slice(&chunk);
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| invalid("Patch must be a UTF-8 configuration file"))?;
            if text.trim_start().starts_with('<') || text.contains('\0') { return Err(invalid("Patch response is not a configuration file")); }
            let digest = format!("{:x}", Sha256::digest(&bytes));
            let directory = paths.root.join("patches");
            std::fs::create_dir_all(&directory)?;
            if nexus_core::path_is_reparse(&std::fs::symlink_metadata(&directory)?) { return Err(invalid("Patch cache must not be a link")); }
            nexus_core::write_private_bytes_atomic(&directory, &directory.join(format!("{digest}.yml")), &bytes)?;
            entry.sha256 = Some(digest);
            entry.cache_identity = Some(nexus_core::patch_identity(entry));
        }
        Ok(p)
    }).await.map_err(|_| invalid("Patch download time budget exceeded; cached configuration was retained")).and_then(|result| result);
    if result.is_err() {
        if let Some(entry) = attempted { record_failure(paths, &HarnessPreferencesPayload { patch_entries: Some(vec![entry]), ..Default::default() }, "download")?; }
    }
    result
}

pub(crate) fn validate(p: &HarnessPreferencesPayload) -> io::Result<()> {
    for path in p.patches.iter().flatten() {
        let bytes = nexus_core::read_regular_file_bounded(std::path::Path::new(path), LIMIT).map_err(|error| io::Error::new(error.kind(), format!("Patch {path}: {error}")))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Patch {path}: enabled file is missing; download it or disable it in Settings")))?;
        if std::str::from_utf8(&bytes).is_err() { return Err(invalid("Patch must be a UTF-8 configuration file")); }
        for entry in p.patch_entries.iter().flatten().filter(|e| e.enabled && e.source.starts_with("https://")) {
            let Some(hash) = &entry.sha256 else { return Err(invalid("An enabled remote patch has not been downloaded")); };
            if std::path::Path::new(path).file_name().and_then(|s| s.to_str()) == Some(&format!("{hash}.yml")) && format!("{:x}", Sha256::digest(&bytes)) != *hash { return Err(invalid("Cached patch checksum changed; download it again before launch")); }
        }
    }
    Ok(())
}

pub(crate) fn diagnostic_summary(paths: &NexusPaths) -> serde_json::Value {
    match nexus_core::load_harness_preferences(paths) {
        Ok(p) => serde_json::json!({"entries": p.patch_entries, "enabled_files": p.patches.iter().flatten().map(|path| {
            match nexus_core::read_regular_file_bounded(std::path::Path::new(path), LIMIT) {
                Ok(Some(bytes)) => serde_json::json!({"path": path, "sha256": format!("{:x}", Sha256::digest(bytes))}),
                Ok(None) => serde_json::json!({"path": path, "state": "missing"}),
                Err(error) => serde_json::json!({"path": path, "error": error.to_string()}),
            }
        }).collect::<Vec<_>>(), "contents": "omitted"}),
        Err(error) => serde_json::json!({"error":error.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test] async fn preview_is_fixed_to_revision_and_content_without_applying_or_redownloading() {
        let root = std::env::temp_dir().join(format!("nexus-patch-preview-{}", nexus_core::unix_time_nanos_for_update()));
        std::fs::create_dir_all(&root).unwrap(); let paths = NexusPaths::from_root(root.clone());
        let store = nexus_core::ConfigStore::new(paths.clone());
        let original = store.snapshot().unwrap();
        let file = root.join("a.yml"); std::fs::write(&file, "[]").unwrap();
        let p = HarnessPreferencesPayload { patches: Some(vec![file.to_string_lossy().into_owned()]), ..Default::default() };
        let result = preview_update(&paths, &original.revision, p.clone()).await.unwrap();
        let id = result["preview_id"].as_str().unwrap();
        assert_eq!(store.snapshot().unwrap().revision, original.revision, "preview never commits");
        assert_eq!(preview_candidate(&paths, &original.revision, id).unwrap(), p);
        assert!(preview_candidate(&paths, "changed-revision", id).is_err());
        std::fs::write(&file, "# changed\n[]").unwrap();
        assert!(preview_candidate(&paths, &original.revision, id).is_err());
        finish_preview(id); assert!(preview_candidate(&paths, &original.revision, id).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn diff_is_bounded_and_omits_known_secrets() {
        assert_eq!(bounded_changes(b"", "x".repeat(257).as_bytes())["truncated"], true);
        let result = bounded_changes(b"api_key: OLD_SECRET\n", b"api_key: NEW_SECRET\n");
        let encoded = result.to_string(); assert!(!encoded.contains("OLD_SECRET") && !encoded.contains("NEW_SECRET"));
        let large = (0..1000).map(|i| format!("value-{i}\n")).collect::<String>();
        let result = bounded_changes(b"", large.as_bytes());
        assert!(result["truncated"].as_bool().unwrap());
        assert!(result["lines"].as_array().unwrap().len() <= 80);
        assert!(result.to_string().len() < 17 * 1024);
    }
    #[tokio::test] async fn reference_query_rejects_invalid_scope_before_network() {
        let entry = nexus_protocol::HarnessPatchEntry { source: "https://github.com/a/b/blob/main/a.yml".into(), enabled:true, ..Default::default() };
        assert!(list_refs(&entry, 0).await.is_err()); assert!(list_refs(&entry, 21).await.is_err());
        let commit = nexus_protocol::HarnessPatchEntry { github_ref_kind:Some("commit".into()), ..entry };
        assert!(list_refs(&commit, 1).await.is_err());
    }
    #[tokio::test] async fn github_commit_permalink_is_resolved_without_a_network_lookup() {
        // No proxy/transport is contacted on this path, including offline use.
        let sha = "a".repeat(40);
        let mut entry = nexus_protocol::HarnessPatchEntry { source: format!("https://github.com/owner/repo/blob/{sha}/a.yml"), enabled: true, ..Default::default() };
        let client = reqwest::Client::new();
        assert_eq!(github_url(&client, &mut entry).await.unwrap().as_str(), format!("https://raw.githubusercontent.com/owner/repo/{sha}/a.yml"));
        assert_eq!(entry.resolved_commit.as_deref(), Some(sha.as_str()));
        entry.github_ref_kind = Some("commit".into()); entry.github_ref_name = Some("invalid".into());
        assert!(github_url(&client, &mut entry).await.is_err());
    }
    #[test] fn duplicate_disabled_entry_cannot_acknowledge_an_enabled_failure() {
        let root = std::env::temp_dir().join(format!("nexus-patch-duplicate-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        let entry = nexus_protocol::HarnessPatchEntry { source: root.join("a.yml").to_string_lossy().into_owned(), enabled: true, ..Default::default() };
        let enabled = HarnessPreferencesPayload { patch_entries: Some(vec![entry.clone()]), ..Default::default() };
        record_failure(&paths, &enabled, "test").unwrap();
        let mut disabled = entry.clone(); disabled.enabled = false;
        let duplicate = HarnessPreferencesPayload { patch_entries: Some(vec![entry.clone(), disabled.clone()]), ..Default::default() };
        acknowledge_disabled(&paths, &duplicate).unwrap(); assert!(check_failures(&paths, &enabled).is_err());
        let legacy = HarnessPreferencesPayload { patches: Some(vec![entry.source.clone()]), patch_entries: Some(vec![disabled.clone()]), ..Default::default() };
        acknowledge_disabled(&paths, &legacy).unwrap(); assert!(check_failures(&paths, &enabled).is_err());
        acknowledge_disabled(&paths, &HarnessPreferencesPayload { patch_entries: Some(vec![disabled]), ..Default::default() }).unwrap();
        check_failures(&paths, &enabled).unwrap(); std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn legacy_cache_path_retains_disabled_remote_source_failure() {
        let root = std::env::temp_dir().join(format!("nexus-patch-remote-legacy-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        let mut remote = nexus_protocol::HarnessPatchEntry { source: "https://example.com/a.yml".into(), enabled: true, sha256: Some("a".repeat(64)), ..Default::default() };
        remote.cache_identity = Some(nexus_core::patch_identity(&remote));
        record_failure(&paths, &HarnessPreferencesPayload { patch_entries: Some(vec![remote.clone()]), ..Default::default() }, "download").unwrap();
        remote.enabled = false;
        let mut legacy = HarnessPreferencesPayload { patches: Some(vec![root.join("patches").join(format!("{}.yml", "a".repeat(64))).to_string_lossy().into_owned()]), patch_entries: Some(vec![remote]), ..Default::default() };
        acknowledge_disabled(&paths, &legacy).unwrap();
        assert!(check_failures(&paths, &legacy).is_err());
        assert!(validate_for_paths(&paths, &legacy).unwrap_err().to_string().contains("previously failed"));
        legacy.patches = None;
        acknowledge_disabled(&paths, &legacy).unwrap();
        legacy.patch_entries.as_mut().unwrap()[0].enabled = true;
        check_failures(&paths, &legacy).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn raw_paths_keep_url_escapes_and_encode_unicode_spaces() {
        assert_eq!(raw_commit_url("a", "b", "commit", "folder/a%20b.yml").as_str(), "https://raw.githubusercontent.com/a/b/commit/folder/a%20b.yml");
        assert!(raw_commit_url("a", "b", "commit", "配置/extra patch.yml").as_str().ends_with("/%E9%85%8D%E7%BD%AE/extra%20patch.yml"));
    }
    #[test] fn failure_survives_restart_until_explicit_disable_and_does_not_block_other_sources() {
        let root = std::env::temp_dir().join(format!("nexus-patch-failure-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.clone());
        let entry = nexus_protocol::HarnessPatchEntry { source: root.join("patch.yml").to_string_lossy().into_owned(), enabled: true, ..Default::default() };
        let p = HarnessPreferencesPayload { patch_entries: Some(vec![entry.clone()]), ..Default::default() };
        record_failure(&paths, &p, "compatibility_combination").unwrap();
        assert!(check_failures(&NexusPaths::from_root(root.clone()), &p).is_err());
        let mut disabled = p.clone(); disabled.patch_entries.as_mut().unwrap()[0].enabled = false;
        check_failures(&paths, &disabled).unwrap();
        assert!(check_failures(&paths, &p).is_err(), "merely constructing a disabled draft cannot acknowledge");
        let mut other = p.clone(); other.patch_entries.as_mut().unwrap()[0].source.push_str("other");
        check_failures(&paths, &other).unwrap();
        acknowledge_disabled(&paths, &disabled).unwrap(); check_failures(&paths, &p).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn known_file_failure_does_not_mark_other_enabled_files() {
        let root = std::env::temp_dir().join(format!("nexus-patch-known-{}", nexus_core::unix_time_nanos_for_update()));
        std::fs::create_dir_all(&root).unwrap(); let paths = NexusPaths::from_root(root.clone());
        let good = root.join("good.yml"); std::fs::write(&good, "[]").unwrap(); let bad = root.join("missing.yml");
        let p = HarnessPreferencesPayload { patches: Some(vec![bad.to_string_lossy().into_owned(), good.to_string_lossy().into_owned()]), ..Default::default() };
        assert!(validate_for_paths(&paths, &p).is_err());
        let good = HarnessPreferencesPayload { patches: Some(vec![good.to_string_lossy().into_owned()]), ..Default::default() };
        check_failures(&paths, &good).unwrap(); std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn urls_are_explicit_and_do_not_accept_secrets() {
        assert_eq!(source_url("https://github.com/owner/repo/blob/abc123/a.yml").unwrap().as_str(), "https://raw.githubusercontent.com/owner/repo/abc123/a.yml");
        for source in ["http://example.com/a", "https://user:secret@example.com/a", "https://example.com/a?token=secret", "https://github.com/owner/repo"] { assert!(source_url(source).is_err()); }
    }
    #[test] fn cached_patch_changes_block_launch_without_exporting_contents() {
        let root = std::env::temp_dir().join(format!("nexus-patch-test-{}", nexus_core::unix_time_nanos_for_update()));
        std::fs::create_dir_all(root.join("patches")).unwrap();
        let bytes = b"[] # secret-marker";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let file = root.join("patches").join(format!("{hash}.yml"));
        std::fs::write(&file, bytes).unwrap();
        let mut p = HarnessPreferencesPayload { patches: Some(vec![file.to_string_lossy().into_owned()]), patch_entries: Some(vec![nexus_protocol::HarnessPatchEntry { source: "https://example.com/a.yml".into(), enabled: true, sha256: Some(hash), ..Default::default() }]), ..Default::default() };
        let entry = &mut p.patch_entries.as_mut().unwrap()[0]; entry.cache_identity = Some(nexus_core::patch_identity(entry));
        validate(&p).unwrap();
        let paths = NexusPaths::from_root(root.clone());
        let mut saved = p.clone(); saved.patches = None;
        nexus_core::ConfigStore::new(paths.clone()).transaction(|document| { document.harness_preferences = Some(saved); Ok(()) }).unwrap();
        let summary = diagnostic_summary(&paths).to_string();
        assert!(!summary.contains("secret-marker"));
        assert!(summary.contains("sha256"));
        std::fs::write(&file, b"[] # different").unwrap();
        assert!(validate(&p).is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(validate(&p).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod discard_tests {
    use super::*;
    #[test]
    fn discard_is_root_scoped_and_releases_only_requested_preview() {
        let id = nexus_core::agent_auth::random_hex().unwrap();
        let a = NexusPaths::from_root(std::env::temp_dir().join(format!("preview-a-{id}")));
        let b = NexusPaths::from_root(std::env::temp_dir().join(format!("preview-b-{id}")));
        a.ensure_directories().unwrap();
        let lease = nexus_core::maintenance::protect_patch_cache(&a).unwrap();
        previews().lock().unwrap().insert(id.clone(), PatchPreview {_lease:lease,root:a.root.clone(),revision:"one".into(),preferences:Default::default(),files:vec![],expires:nexus_core::unix_time_seconds()+60});
        discard_preview(&b,&id); assert!(preview_candidate(&a,"one",&id).is_ok());
        discard_preview(&a,&id); assert!(preview_candidate(&a,"one",&id).is_err());
        discard_preview(&a,&id);
        std::fs::remove_dir_all(&a.root).unwrap();
    }
}
