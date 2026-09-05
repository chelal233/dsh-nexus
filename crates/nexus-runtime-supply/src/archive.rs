use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use crate::{CancellationToken, Result, SupplyError};

#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_entry_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

pub fn extract_node_zip(
    archive_path: &Path,
    destination: &Path,
    expected_root: &str,
    limits: ArchiveLimits,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    validate_expected_root(expected_root)?;
    prepare_empty_destination(destination)?;
    let file = File::open(archive_path)?;
    if !file.metadata()?.is_file() {
        return Err(SupplyError::UnsafeArchive(
            "Node ZIP is not a regular file".to_owned(),
        ));
    }
    let mut archive = ZipArchive::new(file)
        .map_err(|error| SupplyError::UnsafeArchive(format!("invalid ZIP: {error}")))?;
    if archive.len() > limits.max_entries {
        return Err(SupplyError::UnsafeArchive(
            "ZIP contains too many entries".to_owned(),
        ));
    }
    let mut seen = BTreeMap::new();
    let mut total = 0_u64;
    for index in 0..archive.len() {
        cancellation.check()?;
        let mut entry = archive.by_index(index).map_err(|error| {
            SupplyError::UnsafeArchive(format!("cannot read ZIP entry: {error}"))
        })?;
        if entry.encrypted() {
            return Err(SupplyError::UnsafeArchive(
                "encrypted ZIP entries are not supported".to_owned(),
            ));
        }
        if entry.name().contains('\\') {
            return Err(SupplyError::UnsafeArchive(
                "ZIP entry uses a backslash separator".to_owned(),
            ));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o040000 && kind != 0o100000 {
                return Err(SupplyError::UnsafeArchive(
                    "ZIP links and device entries are forbidden".to_owned(),
                ));
            }
        }
        let path = entry.enclosed_name().ok_or_else(|| {
            SupplyError::UnsafeArchive("ZIP path escapes the destination".to_owned())
        })?;
        let path = validate_archive_path(&path, expected_root)?;
        let is_dir = entry.is_dir();
        record_entry(&mut seen, &path, is_dir)?;
        if is_dir {
            create_directory_checked(destination, &path)?;
            continue;
        }
        if !entry.is_file() || entry.size() > limits.max_entry_bytes {
            return Err(SupplyError::UnsafeArchive(
                "ZIP entry is not a bounded regular file".to_owned(),
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| SupplyError::UnsafeArchive("ZIP size overflow".to_owned()))?;
        if total > limits.max_total_bytes {
            return Err(SupplyError::UnsafeArchive(
                "ZIP exceeds the extracted size limit".to_owned(),
            ));
        }
        let target = destination.join(&path);
        create_parent_checked(destination, &target)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let declared_size = entry.size();
        let actual = copy_bounded(
            &mut entry,
            &mut output,
            declared_size,
            limits.max_entry_bytes,
            cancellation,
        )?;
        if actual != declared_size {
            return Err(SupplyError::UnsafeArchive(
                "ZIP entry size changed while extracting".to_owned(),
            ));
        }
        output.sync_all()?;
    }
    let root = destination.join(expected_root);
    validate_extracted_tree(destination, &root, limits)?;
    Ok(root)
}

pub fn extract_pnpm_tarball(
    archive_path: &Path,
    destination: &Path,
    limits: ArchiveLimits,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    prepare_empty_destination(destination)?;
    let file = File::open(archive_path)?;
    if !file.metadata()?.is_file() {
        return Err(SupplyError::UnsafeArchive(
            "pnpm tarball is not a regular file".to_owned(),
        ));
    }
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut seen = BTreeMap::new();
    let mut total = 0_u64;
    let mut entry_count = 0_usize;
    for entry in archive
        .entries()
        .map_err(|error| SupplyError::UnsafeArchive(format!("invalid tarball: {error}")))?
    {
        cancellation.check()?;
        entry_count += 1;
        if entry_count > limits.max_entries {
            return Err(SupplyError::UnsafeArchive(
                "tarball contains too many entries".to_owned(),
            ));
        }
        let mut entry = entry
            .map_err(|error| SupplyError::UnsafeArchive(format!("invalid tar entry: {error}")))?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(SupplyError::UnsafeArchive(
                "tar links and device entries are forbidden".to_owned(),
            ));
        }
        let path = entry
            .path()
            .map_err(|error| SupplyError::UnsafeArchive(format!("invalid tar path: {error}")))?;
        let path = validate_archive_path(&path, "package")?;
        let is_dir = entry_type.is_dir();
        record_entry(&mut seen, &path, is_dir)?;
        if is_dir {
            create_directory_checked(destination, &path)?;
            continue;
        }
        let size = entry.size();
        if size > limits.max_entry_bytes {
            return Err(SupplyError::UnsafeArchive(
                "tar entry exceeds the per-file size limit".to_owned(),
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| SupplyError::UnsafeArchive("tar size overflow".to_owned()))?;
        if total > limits.max_total_bytes {
            return Err(SupplyError::UnsafeArchive(
                "tarball exceeds the extracted size limit".to_owned(),
            ));
        }
        let target = destination.join(&path);
        create_parent_checked(destination, &target)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let actual = copy_bounded(
            &mut entry,
            &mut output,
            size,
            limits.max_entry_bytes,
            cancellation,
        )?;
        if actual != size {
            return Err(SupplyError::UnsafeArchive(
                "tar entry size changed while extracting".to_owned(),
            ));
        }
        output.sync_all()?;
    }
    let root = destination.join("package");
    validate_extracted_tree(destination, &root, limits)?;
    Ok(root)
}

fn validate_expected_root(root: &str) -> Result<()> {
    if root.is_empty() || root.contains(['/', '\\', ':']) || root.chars().any(char::is_control) {
        return Err(SupplyError::UnsafeArchive(
            "expected archive root is not a safe component".to_owned(),
        ));
    }
    Ok(())
}

fn prepare_empty_destination(destination: &Path) -> Result<()> {
    if destination.exists() {
        return Err(SupplyError::UnsafeArchive(
            "extraction destination already exists".to_owned(),
        ));
    }
    fs::create_dir(destination)?;
    reject_reparse(destination)?;
    Ok(())
}

fn validate_archive_path(path: &Path, expected_root: &str) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(SupplyError::UnsafeArchive(
            "archive entry path is empty or absolute".to_owned(),
        ));
    }
    let mut components = path.components();
    let first = components.next().ok_or_else(|| {
        SupplyError::UnsafeArchive("archive entry has no root component".to_owned())
    })?;
    if first != Component::Normal(expected_root.as_ref()) {
        return Err(SupplyError::UnsafeArchive(format!(
            "archive entry is outside expected {expected_root} root"
        )));
    }
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(SupplyError::UnsafeArchive(
                "archive entry contains traversal components".to_owned(),
            ));
        };
        let text = component.to_string_lossy();
        if text.is_empty()
            || text.contains(':')
            || text.ends_with([' ', '.'])
            || text.chars().any(char::is_control)
        {
            return Err(SupplyError::UnsafeArchive(
                "archive entry contains a Windows-unsafe component".to_owned(),
            ));
        }
    }
    Ok(path.to_owned())
}

fn record_entry(seen: &mut BTreeMap<String, bool>, path: &Path, is_dir: bool) -> Result<()> {
    let key = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if seen.insert(key.clone(), is_dir).is_some() {
        return Err(SupplyError::UnsafeArchive(
            "archive contains a duplicate or case-colliding path".to_owned(),
        ));
    }
    let mut parent = Path::new(&key);
    while let Some(next) = parent.parent() {
        if next.as_os_str().is_empty() {
            break;
        }
        if seen.get(&next.to_string_lossy().to_string()) == Some(&false) {
            return Err(SupplyError::UnsafeArchive(
                "archive places a child below a file".to_owned(),
            ));
        }
        parent = next;
    }
    if !is_dir
        && seen
            .keys()
            .any(|existing| existing.starts_with(&(key.clone() + "/")))
    {
        return Err(SupplyError::UnsafeArchive(
            "archive replaces a directory with a file".to_owned(),
        ));
    }
    Ok(())
}

fn create_directory_checked(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(SupplyError::UnsafeArchive(
                "invalid extraction directory".to_owned(),
            ));
        };
        current.push(component);
        if current.exists() {
            let metadata = fs::symlink_metadata(&current)?;
            if !metadata.is_dir() {
                return Err(SupplyError::UnsafeArchive(
                    "archive directory collides with a file".to_owned(),
                ));
            }
            reject_reparse_metadata(&metadata)?;
        } else {
            fs::create_dir(&current)?;
            reject_reparse(&current)?;
        }
    }
    Ok(())
}

fn create_parent_checked(root: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| SupplyError::UnsafeArchive("archive target has no parent".to_owned()))?;
    let relative = parent.strip_prefix(root).map_err(|_| {
        SupplyError::UnsafeArchive("archive target escaped extraction root".to_owned())
    })?;
    create_directory_checked(root, relative)
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    declared: u64,
    maximum: u64,
    cancellation: &CancellationToken,
) -> Result<u64> {
    let limit = declared.min(maximum);
    let mut limited = reader.take(limit.saturating_add(1));
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        cancellation.check()?;
        let count = limited.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > maximum || total > declared {
            return Err(SupplyError::UnsafeArchive(
                "archive entry exceeded its declared size".to_owned(),
            ));
        }
        writer.write_all(&buffer[..count])?;
    }
    Ok(total)
}

fn validate_extracted_tree(root: &Path, expected_root: &Path, limits: ArchiveLimits) -> Result<()> {
    if !expected_root.is_dir() {
        return Err(SupplyError::UnsafeArchive(
            "archive omitted its expected root directory".to_owned(),
        ));
    }
    let canonical_root = fs::canonicalize(root)?;
    let canonical_expected = fs::canonicalize(expected_root)?;
    if !canonical_expected.starts_with(&canonical_root) {
        return Err(SupplyError::UnsafeArchive(
            "extracted root escaped through a reparse point".to_owned(),
        ));
    }
    let mut pending = vec![expected_root.to_owned()];
    let mut entries = 0_usize;
    let mut total = 0_u64;
    while let Some(directory) = pending.pop() {
        reject_reparse(&directory)?;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            entries += 1;
            if entries > limits.max_entries {
                return Err(SupplyError::UnsafeArchive(
                    "extracted tree contains too many entries".to_owned(),
                ));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            reject_reparse_metadata(&metadata)?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                if metadata.len() > limits.max_entry_bytes {
                    return Err(SupplyError::UnsafeArchive(
                        "extracted file exceeds its size limit".to_owned(),
                    ));
                }
                total = total.checked_add(metadata.len()).ok_or_else(|| {
                    SupplyError::UnsafeArchive("extracted size overflow".to_owned())
                })?;
                if total > limits.max_total_bytes {
                    return Err(SupplyError::UnsafeArchive(
                        "extracted tree exceeds its total size limit".to_owned(),
                    ));
                }
            } else {
                return Err(SupplyError::UnsafeArchive(
                    "extracted entry is not a regular file or directory".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn reject_reparse(path: &Path) -> Result<()> {
    reject_reparse_metadata(&fs::symlink_metadata(path)?)
}

fn reject_reparse_metadata(metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(SupplyError::UnsafeArchive(
            "symlink or reparse point is forbidden".to_owned(),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SupplyError::UnsafeArchive(
                "symlink or reparse point is forbidden".to_owned(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn remove_owned_staging(cache_root: &Path, staging: &Path) -> io::Result<()> {
    let name = staging.file_name().and_then(|name| name.to_str());
    if staging.parent() != Some(cache_root)
        || !name.is_some_and(|name| name.starts_with(".staging-"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to remove a path outside the owned staging namespace",
        ));
    }
    match fs::remove_dir_all(staging) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
