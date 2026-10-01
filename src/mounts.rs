//! Mount table, read WITHOUT touching any mounted filesystem. The walker uses it
//! to never open a mountpoint below its root — that is what keeps a dead sshfs /
//! NFS / rclone mount from hanging the scan (a single stat on one blocks in-kernel).

use crate::util::run;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Mount {
    pub path: PathBuf,
    #[allow(dead_code)] // shown in Debug output; kept for diagnostics
    pub fstype: String,
    pub opts: String,
}

pub fn list() -> Vec<Mount> {
    if cfg!(windows) {
        // No mount table to read: the walker never follows junctions or
        // mounted folders (they are reparse points), and drives are separate roots.
        Vec::new()
    } else if cfg!(target_os = "linux") {
        parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
    } else {
        run(&["/sbin/mount"], None, Duration::from_secs(10))
            .map(|o| parse_bsd(&o.stdout))
            .unwrap_or_default()
    }
}

pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let f: Vec<&str> = left.split(' ').collect();
            Some(Mount {
                path: PathBuf::from(unescape(f.get(4)?)),
                opts: f.get(5)?.to_string(),
                fstype: right.split(' ').next()?.to_string(),
            })
        })
        .collect()
}

/// macOS / BSD `mount` output: `/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse)`
pub fn parse_bsd(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|l| {
            let (_, rest) = l.split_once(" on ")?;
            let i = rest.rfind(" (")?;
            let inner = rest[i + 2..].trim_end_matches(')');
            let mut parts = inner.split(", ");
            let fstype = parts.next()?.to_string();
            Some(Mount {
                path: PathBuf::from(&rest[..i]),
                fstype,
                opts: parts.collect::<Vec<_>>().join(","),
            })
        })
        .collect()
}

/// mountinfo escapes space, tab, newline and backslash as \ooo octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every mountpoint strictly below `root`: the walker skips these by path,
/// before any syscall on them.
pub fn below(mounts: &[Mount], root: &Path) -> Vec<PathBuf> {
    mounts
        .iter()
        .filter(|m| m.path != root && m.path.starts_with(root))
        .map(|m| m.path.clone())
        .collect()
}

/// The mount that holds `p` (longest mountpoint prefix).
pub fn containing<'a>(mounts: &'a [Mount], p: &Path) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|m| p.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_parses_escaped_paths_and_types() {
        let s = "36 35 98:0 / /mnt/my\\040disk rw,noatime master:1 - fuse.sshfs host:/ rw\n\
                 1 0 8:1 / / rw,relatime - ext4 /dev/sda1 rw";
        let m = parse_mountinfo(s);
        assert_eq!(m[0].path, PathBuf::from("/mnt/my disk"));
        assert_eq!(m[0].fstype, "fuse.sshfs");
        assert!(m[0].opts.contains("noatime"));
        assert_eq!(
            below(&m, Path::new("/mnt")),
            vec![PathBuf::from("/mnt/my disk")]
        );
        assert_eq!(containing(&m, Path::new("/home/x")).unwrap().fstype, "ext4");
    }

    #[test]
    fn bsd_mount_output_parses() {
        let m = parse_bsd("/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse)\nhost:/x on /Volumes/n s (smbfs, nodev)");
        assert_eq!(m[0].fstype, "apfs");
        assert_eq!(m[1].path, PathBuf::from("/Volumes/n s"));
    }
}
