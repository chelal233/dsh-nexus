//! Private individual files. Never changes a parent directory's permissions.
use std::{fs::{self, File}, io, path::Path};

/// Check simultaneous additional writes by actual target volume, using the
/// caller's available quota. Existing bytes and rename-only backups are not
/// additional allocations. Call before publication, never from rollback.
pub fn ensure_space_budget(targets: &[(&Path, u64)]) -> io::Result<()> {
    #[cfg(windows)] { check_space_budget(targets, windows::space_for_path) }
    #[cfg(unix)] { check_space_budget(targets, unix::space_for_path) }
    #[cfg(not(any(unix, windows)))] { let _ = targets; Ok(()) } // Same Windows-only policy as Nexus install preflight.
}
#[cfg(any(windows, unix, test))]
fn check_space_budget(targets: &[(&Path, u64)], probe: impl Fn(&Path) -> io::Result<(String, u64)>) -> io::Result<()> {
    let mut volumes = std::collections::BTreeMap::<String, (u64, u64)>::new();
    for (path, bytes) in targets {
        let (volume, available) = probe(path)?;
        let entry = volumes.entry(volume).or_insert((0, available));
        entry.0 = entry.0.checked_add(*bytes).ok_or_else(|| io::Error::other("Space budget overflow"))?;
        entry.1 = entry.1.min(available);
    }
    for (_, (required, available)) in volumes {
        if available < required { return Err(io::Error::new(io::ErrorKind::StorageFull,
            format!("Insufficient target-volume space: {available} bytes available to this user, {required} additional bytes required"))); }
    }
    Ok(())
}

/// Creates a new owned directory; does not alter any existing parent.
pub fn create_new_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)] { windows::create_directory(path) }
    #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; fs::DirBuilder::new().mode(0o700).create(path) }
    #[cfg(not(any(unix, windows)))] { let _ = path; Err(io::Error::other("private directories unsupported")) }
}

pub fn create_new_private(path: &Path) -> io::Result<File> {
    #[cfg(windows)] { windows::create(path) }
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
    }
    #[cfg(not(any(unix, windows)))] { let _ = path; Err(io::Error::other("private files are unsupported on this platform")) }
}

/// Stream into a newly created file whose private ACL is established before any bytes are written.
pub fn write_new_private_stream(path: &Path, input: &mut impl io::Read) -> io::Result<()> {
    let mut file = create_new_private(path)?;
    let result = (|| { verify_private(&file)?; io::copy(input, &mut file)?; file.sync_all()?; verify_private(&file) })();
    drop(file);
    if result.is_err() { let _ = fs::remove_file(path); }
    result
}

pub fn secure_existing_private(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("private file must be an ordinary file"));
    }
    #[cfg(windows)] { windows::secure(path) }
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(any(unix, windows)))] { Err(io::Error::other("private files are unsupported on this platform")) }
}

/// Verifies actual filesystem protection, including files on shared storage.
pub fn verify_private(file: &File) -> io::Result<()> {
    #[cfg(windows)] { windows::verify(file) }
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o077 == 0 { Ok(()) }
        else { Err(io::Error::other("private file is accessible to other users")) }
    }
    #[cfg(not(any(unix, windows)))] { let _ = file; Err(io::Error::other("private files are unsupported on this platform")) }
}

/// Probe the volume containing the nearest existing parent of `path`, using
/// the space actually available to unprivileged callers. Mirrors the Windows
/// probe: same parent walk, same quota semantics, keyed by filesystem id.
#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    pub(super) fn space_for_path(path: &Path) -> io::Result<(String, u64)> {
        let mut existing = path;
        loop {
            match fs::metadata(existing) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
            existing = existing.parent().ok_or_else(|| io::Error::other("Target has no existing parent"))?;
        }
        let existing = fs::canonicalize(existing)?;
        let cpath = std::ffi::CString::new(existing.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("Target path contains NUL"))?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let available = u64::try_from(stat.f_bavail).map_err(|_| io::Error::other("negative free-block count"))?
            .checked_mul(u64::try_from(stat.f_frsize).unwrap_or(0))
            .ok_or_else(|| io::Error::other("Space budget overflow"))?;
        // statvfs exposes a scalar filesystem id (unlike statfs::f_fsid).
        Ok((format!("{:x}", stat.f_fsid), available))
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle}}, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE, GENERIC_WRITE},
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    pub(super) fn space_for_path(path: &Path) -> io::Result<(String, u64)> {
        // A new target inherits its nearest existing parent's actual volume.
        // Canonicalization resolves junctions before choosing the volume.
        let mut existing = path;
        loop {
            match fs::metadata(existing) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
            existing = existing.parent().ok_or_else(|| io::Error::other("Target has no existing parent"))?;
        }
        let existing = fs::canonicalize(existing)?;
        let wide: Vec<u16> = existing.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut root = vec![0u16; 32768];
        if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 { return Err(io::Error::last_os_error()); }
        let mut name = [0u16; 64];
        if unsafe { GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32) } == 0 { return Err(io::Error::last_os_error()); }
        let mut available = 0u64;
        if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, ptr::null_mut(), ptr::null_mut()) } == 0 { return Err(io::Error::last_os_error()); }
        let length = name.iter().position(|unit| *unit == 0).unwrap_or(name.len());
        Ok((String::from_utf16_lossy(&name[..length]), available))
    }
    struct Local(*mut c_void);
    impl Drop for Local { fn drop(&mut self) { if !self.0.is_null() { unsafe { LocalFree(self.0); } } } }
    struct Token(HANDLE);
    impl Drop for Token { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }

    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 { return Err(io::Error::last_os_error()); }
        let _text = Local(text.cast());
        let mut length = 0;
        while length < 256 && unsafe { *text.add(length) } != 0 { length += 1; }
        if length == 256 { return Err(io::Error::other("invalid user SID")); }
        Ok(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) }))
    }
    fn user_sid() -> io::Result<String> {
        let mut handle = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 { return Err(io::Error::last_os_error()); }
        let token = Token(handle);
        let mut needed = 0;
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed); }
        if needed == 0 || needed > 65536 { return Err(io::Error::other("cannot determine current user SID")); }
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe { GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) } == 0 { return Err(io::Error::last_os_error()); }
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        sid_string(user.User.Sid)
    }
    fn descriptor() -> io::Result<Local> { descriptor_flags("") }
    fn descriptor_flags(flags: &str) -> io::Result<Local> {
        // TokenUser is present in both elevated and unelevated tokens. OW/BA
        // alone would lock out the same user after UAC elevation changes.
        let sddl: Vec<u16> = format!("D:P(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)(A;{flags};FA;;;{})", user_sid()?)
            .encode_utf16().chain(Some(0)).collect();
        let mut sd = ptr::null_mut();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, ptr::null_mut()) } == 0 { return Err(io::Error::last_os_error()); }
        Ok(Local(sd))
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) { return Err(io::Error::other("private file path contains NUL")); }
        value.push(0); Ok(value)
    }
    fn ordinary(file: &File) -> io::Result<()> {
        use std::os::windows::fs::MetadataExt;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("private file must not be a reparse point"));
        }
        Ok(())
    }
    pub(super) fn create_directory(path: &Path) -> io::Result<()> {
        let sd = descriptor_flags("OICI")?;
        let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: 0 };
        let name = wide(path)?;
        if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 { return Err(io::Error::last_os_error()); }
        let handle = unsafe { CreateFileW(name.as_ptr(), READ_CONTROL, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_handle(handle) };
        verify_flags(&file, (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8, true)
    }
    pub(super) fn create(path: &Path) -> io::Result<File> {
        let sd = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: 0 };
        let name = wide(path)?;
        let handle = unsafe { CreateFileW(name.as_ptr(), GENERIC_WRITE | READ_CONTROL, 0, &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT, ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_handle(handle) };
        if let Err(error) = ordinary(&file).and_then(|_| verify(&file)) {
            drop(file); let _ = fs::remove_file(path);
            return Err(io::Error::other(format!("cannot safely store credentials at this location: {error}")));
        }
        Ok(file)
    }
    pub(super) fn secure(path: &Path) -> io::Result<()> {
        let name = wide(path)?;
        let handle = unsafe { CreateFileW(name.as_ptr(), READ_CONTROL | WRITE_DAC, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, ptr::null(), OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_handle(handle) };
        ordinary(&file)?;
        let sd = descriptor()?;
        let mut dacl = ptr::null_mut(); let mut present = 0; let mut defaulted = 0;
        if unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) } == 0 { return Err(io::Error::last_os_error()); }
        let result = unsafe { SetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, ptr::null_mut(), ptr::null_mut(), dacl, ptr::null()) };
        if result != 0 { return Err(io::Error::from_raw_os_error(result as i32)); }
        verify(&file)
    }
    #[cfg(test)]
    pub(super) fn public_test_directory(path: &Path, set: bool) -> io::Result<()> {
        let name = wide(path)?;
        let handle = unsafe { CreateFileW(name.as_ptr(), READ_CONTROL | WRITE_DAC, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, ptr::null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_handle(handle) };
        if set {
            let sddl: Vec<u16> = "D:P(A;OICI;FA;;;WD)".encode_utf16().chain(Some(0)).collect();
            let mut sd = ptr::null_mut();
            if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, ptr::null_mut()) } == 0 { return Err(io::Error::last_os_error()); }
            let _sd = Local(sd);
            let mut dacl = ptr::null_mut(); let mut present = 0; let mut defaulted = 0;
            if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0 { return Err(io::Error::last_os_error()); }
            let result = unsafe { SetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, ptr::null_mut(), ptr::null_mut(), dacl, ptr::null()) };
            if result != 0 { return Err(io::Error::from_raw_os_error(result as i32)); }
        }
        let mut sd = ptr::null_mut(); let mut dacl = ptr::null_mut();
        let result = unsafe { GetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, ptr::null_mut(), ptr::null_mut(), &mut dacl, ptr::null_mut(), &mut sd) };
        let _sd = Local(sd);
        if result != 0 { return Err(io::Error::from_raw_os_error(result as i32)); }
        assert_eq!(unsafe { (*dacl).AceCount }, 1);
        let mut ace = ptr::null_mut(); unsafe { assert_ne!(GetAce(dacl, 0, &mut ace), 0); }
        let entry = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(sid_string(ptr::addr_of!(entry.SidStart).cast_mut().cast())?, "S-1-1-0");
        assert_eq!(entry.Header.AceFlags, 3); // OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn verify_inherited(file: &File) -> io::Result<()> { verify_flags(file, INHERITED_ACE as u8, false) }
    pub(super) fn verify(file: &File) -> io::Result<()> { verify_flags(file, 0, true) }
    fn verify_flags(file: &File, flags: u8, require_protected: bool) -> io::Result<()> {
        let mut sd = ptr::null_mut(); let mut dacl = ptr::null_mut();
        let result = unsafe { GetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, ptr::null_mut(), ptr::null_mut(), &mut dacl, ptr::null_mut(), &mut sd) };
        let _sd = Local(sd);
        if result != 0 { return Err(io::Error::from_raw_os_error(result as i32)); }
        let mut control = 0; let mut revision = 0;
        if sd.is_null() || dacl.is_null() || unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0
            || (require_protected && control & SE_DACL_PROTECTED == 0) { return Err(io::Error::other("filesystem did not preserve a protected private ACL")); }
        let expected = [user_sid()?, "S-1-5-18".into(), "S-1-5-32-544".into()];
        let mut seen = std::collections::HashSet::new();
        let count = unsafe { (*dacl).AceCount };
        if count == 0 || count > 3 { return Err(io::Error::other("unexpected private ACL entries")); }
        for index in 0..count {
            let mut ace = ptr::null_mut();
            if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 { return Err(io::Error::last_os_error()); }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            // ACCESS_ALLOWED_ACE_TYPE is the Win32 ACE type 0.
            if header.AceType != 0 || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>() { return Err(io::Error::other("private ACL has an unexpected ACE type")); }
            let entry = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            if entry.Header.AceFlags != flags || entry.Mask != FILE_ALL_ACCESS {
                return Err(io::Error::other("private ACL contains an unexpected access grant"));
            }
            let sid = sid_string(ptr::addr_of!(entry.SidStart).cast_mut().cast())?;
            if !expected.contains(&sid) { return Err(io::Error::other("private file grants another user access")); }
            seen.insert(sid);
        }
        if !expected.iter().all(|sid| seen.contains(sid)) { return Err(io::Error::other("private ACL lacks current user or recovery access")); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn archive_stream_is_private_before_writing_and_never_clobbers_existing_files() {
        let root = std::env::temp_dir().join(format!("nexus-private-stream-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&root).unwrap();
        #[cfg(windows)] windows::public_test_directory(&root, true).unwrap();
        let target = root.join("archive.tmp");
        write_new_private_stream(&target, &mut &b"archive credential bytes"[..]).unwrap();
        verify_private(&File::open(&target).unwrap()).unwrap();
        assert!(write_new_private_stream(&target, &mut &b"replacement"[..]).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"archive credential bytes");
        let published = root.join("archive.tar.gz"); fs::hard_link(&target, &published).unwrap();
        verify_private(&File::open(&published).unwrap()).unwrap();
        struct Broken;
        impl io::Read for Broken { fn read(&mut self, _: &mut [u8]) -> io::Result<usize> { Err(io::Error::other("broken input")) } }
        let failed = root.join("failed.tmp"); assert!(write_new_private_stream(&failed, &mut Broken).is_err()); assert!(!failed.exists());
        #[cfg(windows)] windows::public_test_directory(&root, false).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn private_at_creation_and_existing_content_is_preserved() {
        let root = std::env::temp_dir().join(format!("nexus-private-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&root).unwrap();
        #[cfg(windows)] windows::public_test_directory(&root, true).unwrap();
        let path = root.join("credential.json");
        let mut file = create_new_private(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        verify_private(&file).unwrap();
        file.write_all(b"original bytes").unwrap(); drop(file);
        secure_existing_private(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original bytes");
        verify_private(&File::open(&path).unwrap()).unwrap();
        #[cfg(windows)] windows::public_test_directory(&root, false).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod space_budget_tests {
    use super::*;
    #[test]
    fn sums_same_volume_peaks_but_checks_distinct_targets_independently() {
        let targets=[(Path::new("stage"),60),(Path::new("archive"),50)];
        assert_eq!(check_space_budget(&targets, |_| Ok(("same".into(),100))).unwrap_err().kind(),io::ErrorKind::StorageFull);
        check_space_budget(&targets, |path| Ok((path.to_string_lossy().into_owned(),60))).unwrap();
        check_space_budget(&[(Path::new("restore-new-bytes"),60)], |_| Ok(("same".into(),60))).unwrap();
        assert!(check_space_budget(&[(Path::new("a"),u64::MAX),(Path::new("b"),1)], |_| Ok(("same".into(),u64::MAX))).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn missing_target_uses_existing_parent_and_caller_quota() {
        let root=std::env::temp_dir();
        let (volume,available)=windows::space_for_path(&root).unwrap();
        assert!(available>0);
        assert_eq!(windows::space_for_path(&root.join("nexus-space-uncreated/file.tmp")).unwrap().0,volume);
    }
    #[cfg(unix)]
    #[test]
    fn missing_target_uses_existing_parent_and_caller_quota_unix() {
        let root=std::env::temp_dir();
        let (volume,available)=unix::space_for_path(&root).unwrap();
        assert!(available>0);
        assert_eq!(unix::space_for_path(&root.join("nexus-space-uncreated/file.tmp")).unwrap().0,volume);
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;
    #[test]
    fn private_directory_is_created_before_content_and_never_reuses_existing_path() {
        let root = std::env::temp_dir().join(format!("private-directory-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        create_new_private_directory(&root).unwrap();
        assert!(create_new_private_directory(&root).is_err());
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/env"),b"test-secret").unwrap();
        #[cfg(windows)] { windows::verify_inherited(&fs::File::open(root.join("nested/env")).unwrap()).unwrap(); }
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o777,0o700); }
        fs::remove_dir_all(root).unwrap();
    }
}
