//! Everything that differs between Linux, macOS and Windows lives here; the
//! rest of diskreap is one code path for all three.

use std::fs::{FileType, Metadata};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

fn secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn mtime(m: &Metadata) -> i64 {
    secs(m.modified())
}

pub fn atime(m: &Metadata) -> i64 {
    secs(m.accessed())
}

/// "Last used": a FILE's atime counts; a directory's does not — listing it
/// (as this scan does) moves it.
pub fn last_use(m: &Metadata) -> i64 {
    if m.is_dir() {
        mtime(m)
    } else {
        atime(m).max(mtime(m))
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .expect("neither HOME nor USERPROFILE is set")
}

/// canonicalize without the Windows `\\?\` prefix.
pub fn canon(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if cfg!(windows) {
        let s = c.to_string_lossy();
        if let Some(r) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(r);
        }
    }
    c
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    pub fn alloc(m: &Metadata) -> u64 {
        m.blocks() * 512
    }
    pub fn dev(m: &Metadata) -> u64 {
        m.dev()
    }
    pub fn hardlink_key(m: &Metadata) -> Option<(u64, u64)> {
        (!m.is_dir() && m.nlink() > 1).then(|| (m.dev(), m.ino()))
    }
    pub fn is_special(ft: &FileType) -> bool {
        ft.is_socket() || ft.is_fifo()
    }
    pub fn owned_by_me(m: &Metadata) -> bool {
        m.uid() == unsafe { libc::geteuid() }
    }
    pub fn is_privileged() -> bool {
        unsafe { libc::geteuid() == 0 }
    }
    /// Directories: u+rwx so they can be listed and emptied (sealed release
    /// copies are chmod a-w). Files need nothing on Unix.
    pub fn make_removable(p: &Path, m: &Metadata) -> std::io::Result<()> {
        if m.is_dir() && m.mode() & 0o700 != 0o700 {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m.mode() | 0o700))?;
        }
        Ok(())
    }
    pub fn fs_space(p: &Path) -> Option<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)] // field types differ across Unixes
        let f = s.f_frsize as u64;
        #[allow(clippy::unnecessary_cast)]
        Some((s.f_blocks as u64 * f, s.f_bavail as u64 * f))
    }
    pub fn hostname() -> String {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..n]).into_owned();
        }
        "unknown".into()
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            avail: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }

    /// NTFS reports logical size; good enough for estimates (df-style
    /// measurement is what `clean` reports as freed).
    pub fn alloc(m: &Metadata) -> u64 {
        m.len()
    }
    /// The walker never follows reparse points (junctions, symlinks, mounted
    /// folders), so it cannot leave the volume; there is no device id to compare.
    pub fn dev(_: &Metadata) -> u64 {
        0
    }
    pub fn hardlink_key(_: &Metadata) -> Option<(u64, u64)> {
        None
    }
    pub fn is_special(_: &FileType) -> bool {
        false
    }
    pub fn owned_by_me(_: &Metadata) -> bool {
        true // per-user temp dir: everything in it is ours
    }
    pub fn is_privileged() -> bool {
        false
    }
    /// Read-only attribute blocks deletion of files on Windows.
    pub fn make_removable(p: &Path, m: &Metadata) -> std::io::Result<()> {
        let mut perm = m.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(p, perm)?;
        }
        Ok(())
    }
    pub fn fs_space(p: &Path) -> Option<(u64, u64)> {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0)
            .then_some((total, avail))
    }
    pub fn hostname() -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
    }
}

pub use imp::*;
