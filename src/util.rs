use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub const DAY: i64 = 86_400;
pub const GB: u64 = 1 << 30;

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

pub fn ago(t: i64) -> String {
    if t <= 0 {
        return "-".into();
    }
    let s = (now() - t).max(0);
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < DAY {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / DAY)
    }
}

pub struct Out {
    pub ok: bool,
    pub stdout: String,
}

/// Run a command with a hard timeout. `None` if it could not start or was killed
/// for running too long — callers treat that as "unknown", never as success.
pub fn run(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Option<Out> {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let mut child = cmd.spawn().ok()?;
    let mut so = child.stdout.take()?;
    // Drain stdout on a thread so a chatty child cannot block on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let stdout = reader.join().unwrap_or_default();
                return Some(Out {
                    ok: st.success(),
                    stdout,
                });
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None; // reader thread is abandoned: a grandchild may still hold the pipe
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|d| {
            let exts: &[&str] = if cfg!(windows) {
                &["", ".exe", ".cmd", ".bat"]
            } else {
                &[""]
            };
            exts.iter()
                .map(move |e| d.join(format!("{bin}{e}")))
                .collect::<Vec<_>>()
        })
        .find(|p| p.is_file())
}

/// (total, available-to-user) bytes of the filesystem holding `p` — statvfs /
/// GetDiskFreeSpaceEx on a local path never touches other mounts.
pub fn fs_space(p: &Path) -> Option<(u64, u64)> {
    crate::platform::fs_space(p)
}

pub fn hostname() -> String {
    crate::platform::hostname()
}

pub fn home() -> PathBuf {
    crate::platform::home()
}

pub fn state_dir() -> PathBuf {
    let d = std::env::var_os("DISKREAP_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/diskreap"));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Operator-set free-space floor for `$HOME`'s filesystem, in bytes, or None.
/// `DISKREAP_MIN_FREE_GIB` wins over `min_free_gib` in the JSON config file
/// (`$DISKREAP_CONFIG`, default `~/.config/diskreap/config.json`). A host that
/// keeps a large working set (an object store, build caches) sets it above the
/// built-in thresholds so cleanup starts before that store is refused space.
/// A malformed value is a loud exit, never a silent fallback to the defaults.
pub fn min_free_bytes() -> Option<u64> {
    let (raw, src) = match std::env::var("DISKREAP_MIN_FREE_GIB") {
        Ok(v) => (v, "DISKREAP_MIN_FREE_GIB".to_string()),
        Err(_) => {
            let path = std::env::var_os("DISKREAP_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".config/diskreap/config.json"));
            let text = std::fs::read_to_string(&path).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("diskreap: {} is not valid JSON: {e}", path.display());
                std::process::exit(2)
            });
            (
                v.get("min_free_gib")?.to_string(),
                format!("min_free_gib in {}", path.display()),
            )
        }
    };
    parse_min_free_gib(&raw).unwrap_or_else(|| {
        eprintln!("diskreap: {src} must be a positive number of GiB, got {raw:?}");
        std::process::exit(2)
    })
}

/// Some(Some(bytes)) for a positive number, Some(None) for 0 / empty (unset), None if malformed.
fn parse_min_free_gib(raw: &str) -> Option<Option<u64>> {
    let t = raw.trim();
    if t.is_empty() {
        return Some(None);
    }
    let v: f64 = t.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v > 0.0).then(|| (v * GB as f64) as u64))
}

/// Exclusive non-blocking lock held for the life of the returned file.
pub fn try_lock(name: &str) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir().join(name))
        .ok()?;
    f.try_lock().ok().map(|_| f)
}

pub fn mtime_of(p: &Path) -> i64 {
    std::fs::symlink_metadata(p)
        .map(|m| crate::platform::mtime(&m))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_free_parsing() {
        assert_eq!(parse_min_free_gib("120"), Some(Some(120 * GB)));
        assert_eq!(parse_min_free_gib(" 0.5 "), Some(Some(GB / 2)));
        assert_eq!(parse_min_free_gib("0"), Some(None));
        assert_eq!(parse_min_free_gib(""), Some(None));
        assert_eq!(parse_min_free_gib("-1"), None);
        assert_eq!(parse_min_free_gib("lots"), None);
        assert_eq!(parse_min_free_gib("\"120\""), None);
    }
}
