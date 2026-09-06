//! Data-root disk policy. Nexus relies on atomic rename publication and
//! Windows junctions, so the data root is only accepted on fixed NTFS or
//! ReFS volumes; removable media, network drives, and other filesystems are
//! rejected before any state is written. Install preflight also requires a
//! minimum amount of free space on the volume.

/// Free space required before a cold install starts: an upstream clone plus
/// a pnpm build comfortably fits, with headroom for the published release.
pub const MIN_INSTALL_FREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Reason a volume is not acceptable for Nexus state.
#[derive(Debug, PartialEq, Eq)]
pub enum VolumeRejection {
    /// A network share: atomic publication across SMB is not dependable.
    Remote,
    /// Removable media (USB stick, SD card): surprise removal corrupts state.
    Removable,
    /// Optical, virtual, or otherwise unrecognized drive class.
    UnsupportedDrive(u32),
    /// A filesystem other than NTFS/ReFS (for example FAT32/exFAT), which
    /// does not provide the junction and atomicity guarantees Nexus needs.
    Filesystem(String),
    /// The volume for the path could not be resolved at all (unreachable
    /// path, missing volume, OS error).
    Volume(String),
}

impl VolumeRejection {
    pub fn message(&self) -> String {
        match self {
            VolumeRejection::Remote => {
                "the volume is a network drive".to_owned()
            }
            VolumeRejection::Removable => {
                "the volume is removable media".to_owned()
            }
            VolumeRejection::UnsupportedDrive(kind) => {
                format!("the volume has an unsupported drive type (code {kind})")
            }
            VolumeRejection::Filesystem(name) => {
                format!(
                    "the volume uses {name}; only NTFS and ReFS are supported"
                )
            }
            VolumeRejection::Volume(detail) => {
                format!("the volume for this path could not be resolved: {detail}")
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{VolumeRejection, MIN_INSTALL_FREE_BYTES};
    use std::{
        ffi::OsString,
        io,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;

    fn wide(path: &Path) -> Vec<u16> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        value.push(0);
        value
    }

    fn volume_root_for(path: &Path) -> io::Result<PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
        let mut buffer = [0u16; 512];
        let ok = unsafe {
            GetVolumePathNameW(wide(path).as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
        Ok(PathBuf::from(OsString::from_wide(&buffer[..len])))
    }

    /// Classify one volume root. Pure over its inputs so the policy is testable.
    pub fn classify_volume(drive_type: u32, filesystem: String) -> Result<(), VolumeRejection> {
        match drive_type {
            DRIVE_FIXED => {}
            DRIVE_REMOTE => return Err(VolumeRejection::Remote),
            DRIVE_REMOVABLE => return Err(VolumeRejection::Removable),
            other => return Err(VolumeRejection::UnsupportedDrive(other)),
        }
        let normalized = filesystem.trim().to_ascii_uppercase();
        if normalized != "NTFS" && normalized != "REFS" {
            return Err(VolumeRejection::Filesystem(filesystem.trim().to_owned()));
        }
        Ok(())
    }

    pub fn validate_volume(path: &Path) -> Result<(), VolumeRejection> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetDriveTypeW, GetVolumeInformationW,
        };
        let root = volume_root_for(path)
            .map_err(|error| VolumeRejection::Volume(error.to_string()))?;
        let root_wide = wide(&root);
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        let mut filesystem = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                root_wide.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        };
        if ok == 0 {
            return Err(VolumeRejection::UnsupportedDrive(drive_type));
        }
        let len = filesystem
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(filesystem.len());
        let name = String::from_utf16_lossy(&filesystem[..len]);
        classify_volume(drive_type, name)
    }

    pub fn free_bytes(path: &Path) -> io::Result<u64> {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let root = volume_root_for(path)?;
        let root_wide = wide(&root);
        let mut free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root_wide.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut free,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(free)
    }

    pub fn ensure_free_space(path: &Path, required_bytes: u64) -> io::Result<()> {
        let free = free_bytes(path)?;
        if free < required_bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "insufficient disk space: {:.1} GiB available, at least {:.1} GiB required",
                    free as f64 / (1024.0 * 1024.0 * 1024.0),
                    required_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                ),
            ));
        }
        Ok(())
    }

    pub fn default_preflight_free_bytes() -> u64 {
        MIN_INSTALL_FREE_BYTES
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    use super::VolumeRejection;

    pub fn classify_volume(_drive_type: u32, _filesystem: String) -> Result<(), VolumeRejection> {
        Ok(())
    }

    pub fn validate_volume(_path: &std::path::Path) -> Result<(), VolumeRejection> {
        // Windows is the only platform with the NTFS/ReFS policy in this
        // release; other platforms are untested and left permissive here.
        Ok(())
    }

    pub fn ensure_free_space(_path: &std::path::Path, _required_bytes: u64) -> io::Result<()> {
        // Space preflight is Windows-only in this release.
        Ok(())
    }

    pub fn default_preflight_free_bytes() -> u64 {
        super::MIN_INSTALL_FREE_BYTES
    }
}

pub use imp::{classify_volume, default_preflight_free_bytes, ensure_free_space, validate_volume};

#[cfg(test)]
mod tests {
    use super::{VolumeRejection, MIN_INSTALL_FREE_BYTES};

    #[test]
    fn volume_policy_accepts_fixed_ntfs_and_refs_only() {
        assert_eq!(super::classify_volume(3, "NTFS".to_owned()), Ok(()));
        assert_eq!(super::classify_volume(3, "ntfs".to_owned()), Ok(()));
        assert_eq!(super::classify_volume(3, "ReFS".to_owned()), Ok(()));
        assert_eq!(
            super::classify_volume(4, "NTFS".to_owned()),
            Err(VolumeRejection::Remote)
        );
        assert_eq!(
            super::classify_volume(2, "NTFS".to_owned()),
            Err(VolumeRejection::Removable)
        );
        assert!(matches!(
            super::classify_volume(5, "NTFS".to_owned()),
            Err(VolumeRejection::UnsupportedDrive(5))
        ));
        assert!(matches!(
            super::classify_volume(3, "FAT32".to_owned()),
            Err(VolumeRejection::Filesystem(name)) if name == "FAT32"
        ));
    }

    #[test]
    fn rejection_messages_name_the_actual_problem() {
        assert!(
            VolumeRejection::Remote
                .message()
                .contains("network drive")
        );
        assert!(
            VolumeRejection::Filesystem("exFAT".to_owned())
                .message()
                .contains("exFAT")
        );
        assert!(MIN_INSTALL_FREE_BYTES >= 4 * 1024 * 1024 * 1024);
    }
}
