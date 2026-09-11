//! Reclaim old allocations without moving append cursors or changing log identity.
use std::{fs, io, ops::Range, path::Path};
use serde::{Deserialize, Serialize};

pub const TAIL_BYTES: u64 = 8 * 1024 * 1024;
pub const INTERVAL_SECS: u64 = 5;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileStatus {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub tail_budget_bytes: u64,
}

/// One evidence island and the tail mean at most two obsolete ranges.
fn obsolete_ranges(length: u64, protected: &[Range<u64>]) -> io::Result<Vec<Range<u64>>> {
    if protected.len() > 1 || protected.iter().any(|r| r.start >= r.end || r.end > length || r.end - r.start > 65538) {
        return Err(io::Error::other("Invalid log evidence range"));
    }
    let end = length.saturating_sub(TAIL_BYTES);
    let mut cursor = 0;
    let mut result = Vec::new();
    for range in protected {
        let start = range.start.min(end);
        if start > cursor { result.push(cursor..start); }
        cursor = range.end.min(end);
    }
    if cursor < end { result.push(cursor..end); }
    Ok(result)
}

pub fn maintain(path: &Path, expected_identity: Option<&str>, protected: &[Range<u64>], scanned_through: Option<u64>) -> io::Result<FileStatus> {
    let parent = path.parent().ok_or_else(|| io::Error::other("Log directory missing"))?;
    for directory in [Some(parent), parent.parent()].into_iter().flatten() {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || crate::path_is_reparse(&metadata) { return Err(io::Error::other("Log directory is linked")); }
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::path_is_reparse(&metadata) { return Err(io::Error::other("Log is not an ordinary file")); }
    let mut options = fs::OpenOptions::new(); options.read(true).write(true);
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || crate::path_is_reparse(&metadata)
        || expected_identity.is_some_and(|id| crate::log_file_identity(&file).ok().as_deref() != Some(id)) {
        return Err(io::Error::other("Log identity changed"));
    }
    let logical_bytes = metadata.len();
    let ranges = obsolete_ranges(logical_bytes, protected)?.into_iter()
        .filter_map(|r| { let end = r.end.min(scanned_through.unwrap_or(u64::MAX)); (r.start < end).then_some(r.start..end) }).collect::<Vec<_>>();
    let allocated_bytes = reclaim(&file, &ranges)?;
    Ok(FileStatus { logical_bytes, allocated_bytes, tail_budget_bytes: TAIL_BYTES })
}

#[cfg(not(windows))]
fn reclaim(_file: &fs::File, _ranges: &[Range<u64>]) -> io::Result<u64> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Live sparse log retention requires Windows"))
}
#[cfg(windows)]
fn reclaim(file: &fs::File, ranges: &[Range<u64>]) -> io::Result<u64> {
    use std::{os::windows::io::AsRawHandle, mem::size_of};
    use windows_sys::Win32::{Storage::FileSystem::{GetFileInformationByHandleEx, FileStandardInfo, FILE_STANDARD_INFO},
        System::{IO::DeviceIoControl, Ioctl::{FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA, FILE_ZERO_DATA_INFORMATION}}};
    let handle = file.as_raw_handle();
    let mut returned = 0;
    // Never emulate unsupported sparse reclamation with truncation or zero writes.
    if unsafe { DeviceIoControl(handle, FSCTL_SET_SPARSE, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    for range in ranges {
        let data = FILE_ZERO_DATA_INFORMATION { FileOffset: i64::try_from(range.start).map_err(|_| io::Error::other("Log offset exceeds OS limit"))?,
            BeyondFinalZero: i64::try_from(range.end).map_err(|_| io::Error::other("Log offset exceeds OS limit"))? };
        if unsafe { DeviceIoControl(handle, FSCTL_SET_ZERO_DATA, &data as *const _ as _, size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut()) } == 0 { return Err(io::Error::last_os_error()); }
    }
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandleEx(handle, FileStandardInfo, &mut info as *mut _ as _, size_of::<FILE_STANDARD_INFO>() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    u64::try_from(info.AllocationSize).map_err(|_| io::Error::other("Invalid log allocation size"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn range_plan_keeps_tail_and_complete_evidence() {
        assert_eq!(obsolete_ranges(TAIL_BYTES + 100, &[10..30]).unwrap(), vec![0..10, 30..100]);
        assert_eq!(obsolete_ranges(TAIL_BYTES + 100, &[90..130]).unwrap(), vec![0..90]);
        assert!(obsolete_ranges(10, &[9..12]).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn sparse_reclamation_preserves_identity_offsets_and_append() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let path = std::env::temp_dir().join(format!("nexus-sparse-test-{}.log", crate::agent_auth::random_hex().unwrap()));
        let mut append = fs::OpenOptions::new().create_new(true).append(true).open(&path).unwrap();
        append.write_all(&vec![b'x'; (TAIL_BYTES + 4 * 1024 * 1024) as usize]).unwrap();
        let identity = crate::log_file_identity(&append).unwrap();
        let length = append.metadata().unwrap().len();
        let result = maintain(&path, Some(&identity), &[10..30], None);
        let status = result.expect("test volume must support sparse files; unsupported is a reported limitation, never a truncate fallback");
        assert_eq!(status.logical_bytes, length);
        assert!(status.allocated_bytes < length);
        let mut read = fs::File::open(&path).unwrap(); read.seek(SeekFrom::Start(10)).unwrap();
        let mut evidence = [0; 20]; read.read_exact(&mut evidence).unwrap(); assert_eq!(evidence, [b'x'; 20]);
        append.write_all(b"NEXT\n").unwrap();
        assert_eq!(append.metadata().unwrap().len(), length + 5);
        assert_eq!(crate::log_file_identity(&append).unwrap(), identity);
        drop(append); drop(read); fs::remove_file(path).unwrap();
    }
}
