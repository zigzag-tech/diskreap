//! "Is anything using this path right now?" — a snapshot of every path a live
//! process holds: cwd, executable, open files and mmapped libraries (a running
//! server may have its node_modules native addons mapped with cwd elsewhere),
//! plus bind-mount sources of running Docker containers (their processes are
//! usually another uid and invisible in /proc to us).

use crate::util::{run, which};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Default)]
pub struct InUse {
    set: BTreeSet<PathBuf>,
    /// False when the snapshot could not be taken; callers must then treat
    /// everything as in use.
    pub complete: bool,
}

impl InUse {
    pub fn snapshot() -> InUse {
        let mut u = InUse {
            set: BTreeSet::new(),
            complete: true,
        };
        if cfg!(target_os = "linux") {
            u.proc_linux();
        } else {
            u.lsof();
        }
        u.docker_mounts();
        u.tmux_panes();
        u
    }

    /// Sandboxed agents are often non-dumpable: /proc/<pid>/cwd is unreadable
    /// even for our own uid. Their terminal pane's cwd is not.
    fn tmux_panes(&mut self) {
        if which("tmux").is_none() {
            return;
        }
        if let Some(o) = run(
            &["tmux", "list-panes", "-a", "-F", "#{pane_current_path}"],
            None,
            Duration::from_secs(10),
        ) {
            o.stdout.lines().for_each(|l| self.add(l.trim()));
        }
    }

    #[cfg(test)]
    pub fn from(paths: &[&str]) -> InUse {
        InUse {
            set: paths.iter().map(PathBuf::from).collect(),
            complete: true,
        }
    }

    /// First held path at or below `dir`. Paths order component-wise, so all
    /// descendants of `dir` sort contiguously right after it.
    pub fn under(&self, dir: &Path) -> Option<&PathBuf> {
        if !self.complete {
            return Some(self.sentinel_unknown());
        }
        self.set
            .range(dir.to_path_buf()..)
            .next()
            .filter(|p| p.starts_with(dir))
    }

    fn sentinel_unknown(&self) -> &PathBuf {
        static UNKNOWN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        UNKNOWN.get_or_init(|| PathBuf::from("<process snapshot unavailable>"))
    }

    fn add(&mut self, s: &str) {
        let s = s.strip_suffix(" (deleted)").unwrap_or(s);
        if s.starts_with('/') {
            self.set.insert(PathBuf::from(s));
        }
    }

    fn proc_linux(&mut self) {
        let Ok(rd) = std::fs::read_dir("/proc") else {
            self.complete = false;
            return;
        };
        for e in rd.flatten() {
            let pid = e.file_name();
            if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let base = e.path();
            for l in ["cwd", "exe"] {
                if let Ok(t) = std::fs::read_link(base.join(l)) {
                    self.add(&t.to_string_lossy());
                }
            }
            if let Ok(fds) = std::fs::read_dir(base.join("fd")) {
                for fd in fds.flatten() {
                    if let Ok(t) = std::fs::read_link(fd.path()) {
                        self.add(&t.to_string_lossy());
                    }
                }
            }
            if let Ok(maps) = std::fs::read_to_string(base.join("maps")) {
                for line in maps.lines() {
                    // addr perms offset dev inode   pathname
                    if let Some(i) = line.find(" /") {
                        self.add(line[i + 1..].trim_start());
                    }
                }
            }
        }
    }

    fn lsof(&mut self) {
        match run(
            &["lsof", "-nP", "-w", "-Fn"],
            None,
            Duration::from_secs(120),
        ) {
            Some(o) if !o.stdout.is_empty() => {
                for l in o.stdout.lines() {
                    if let Some(p) = l.strip_prefix('n') {
                        self.add(p);
                    }
                }
            }
            _ => self.complete = false,
        }
    }

    fn docker_mounts(&mut self) {
        if which("docker").is_none() {
            return;
        }
        let t = Duration::from_secs(20);
        let Some(ids) = run(&["docker", "ps", "-q"], None, t) else {
            return;
        };
        let ids: Vec<&str> = ids.stdout.split_whitespace().collect();
        if ids.is_empty() {
            return;
        }
        let mut args = vec![
            "docker",
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Source}}\n{{end}}",
        ];
        args.extend(ids);
        match run(&args, None, t) {
            Some(o) => o.stdout.lines().for_each(|l| self.add(l.trim())),
            None => self.complete = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_matches_descendants_only() {
        let u = InUse::from(&["/a/b/c/file", "/a/b0/x"]);
        assert!(u.under(Path::new("/a/b")).is_some());
        assert!(u.under(Path::new("/a/b/c")).is_some());
        assert!(u.under(Path::new("/a/b/d")).is_none());
        assert!(u.under(Path::new("/a/bb")).is_none());
        let unknown = InUse {
            set: BTreeSet::new(),
            complete: false,
        };
        assert!(unknown.under(Path::new("/anything")).is_some());
    }
}

/// Paths named in service definitions — systemd units, cron, launchd plists.
/// A lease-/socket-/timer-started service is usually NOT running when we look,
/// yet deleting its venv or node_modules breaks its next start.
#[derive(Default)]
pub struct Refs {
    /// (referenced path, file that references it)
    refs: Vec<(PathBuf, String)>,
}

impl Refs {
    pub fn collect(home: &Path) -> Refs {
        let mut files: Vec<PathBuf> = Vec::new();
        let dirs = [
            PathBuf::from("/etc/systemd/system"),
            PathBuf::from("/etc/systemd/user"),
            home.join(".config/systemd/user"),
            PathBuf::from("/etc/cron.d"),
            home.join("Library/LaunchAgents"),
            PathBuf::from("/Library/LaunchAgents"),
            PathBuf::from("/Library/LaunchDaemons"),
        ];
        for d in dirs {
            collect_files(&d, 2, &mut files);
        }
        files.push(PathBuf::from("/etc/crontab"));
        let mut r = Refs::default();
        for f in files {
            if let Ok(s) = std::fs::read_to_string(&f) {
                r.scan_text(&s, &f.to_string_lossy(), home);
            }
        }
        if let Some(o) = run(&["crontab", "-l"], None, Duration::from_secs(10)) {
            r.scan_text(&o.stdout, "crontab", home);
        }
        r.path_commands(home);
        r
    }

    /// Commands on PATH that live inside a tree: uv/pipx tools are symlinks into
    /// their venv, `pip --user` scripts carry the venv's interpreter in `#!`. A
    /// tool venv looks like idle build output (and uv even tags it CACHEDIR.TAG);
    /// deleting it removed `yt-dlp`, `kimi`, `mitmproxy` from the fleet (2026-10-02).
    fn path_commands(&mut self, home: &Path) {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        dirs.extend([home.join(".local/bin"), home.join("bin"), home.join(".cargo/bin")]);
        dirs.sort();
        dirs.dedup();
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let src = format!("command {} on PATH", p.display());
                if let Ok(t) = std::fs::read_link(&p) {
                    let t = if t.is_absolute() { t } else { d.join(t) };
                    self.refs.push((t, src));
                    continue;
                }
                // `#!/path/to/venv/bin/python` — read only the first line of small files.
                if std::fs::metadata(&p).map(|m| m.is_file() && m.len() < 1 << 20).unwrap_or(false) {
                    if let Ok(f) = std::fs::File::open(&p) {
                        use std::io::{BufRead, BufReader, Read};
                        let mut line = String::new();
                        let _ = BufReader::new(f.take(512)).read_line(&mut line);
                        if let Some(interp) = line.strip_prefix("#!") {
                            if let Some(first) = interp.split_whitespace().next() {
                                if first.starts_with('/') && Path::new(first).starts_with(home) {
                                    self.refs.push((PathBuf::from(first), src));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn scan_text(&mut self, s: &str, src: &str, home: &Path) {
        let seps = |c: char| c.is_whitespace() || "\"'=:;,<>()[]{}`".contains(c);
        for tok in s.split(seps) {
            let tok = tok.trim_end_matches('/');
            // $HOME itself (WorkingDirectory=~) would protect everything: ignore it.
            if tok.starts_with('/') && Path::new(tok) != home {
                self.refs.push((PathBuf::from(tok), src.to_string()));
            }
        }
    }

    /// A service references something at or below `dir`.
    pub fn within(&self, dir: &Path) -> Option<&str> {
        self.refs
            .iter()
            .find(|(p, _)| p.starts_with(dir))
            .map(|(_, s)| s.as_str())
    }

    #[cfg(test)]
    pub fn from_text(s: &str, home: &Path) -> Refs {
        let mut r = Refs::default();
        r.scan_text(s, "test", home);
        r
    }
}

fn collect_files(d: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() && depth > 0 => collect_files(&p, depth - 1, out),
            Ok(t) if t.is_file() || t.is_symlink() => out.push(p),
            _ => {}
        }
    }
}

#[cfg(test)]
mod refs_tests {
    use super::*;

    #[test]
    fn unit_file_paths_protect_their_project() {
        let home = Path::new("/home/u");
        let r = Refs::from_text(
            "[Service]\nWorkingDirectory=/home/u\nExecStart=/home/u/app/cli/venv/bin/python -m srv\nEnvironment=PATH=/home/u/.cargo/bin:/usr/bin\n",
            home,
        );
        assert!(r.within(Path::new("/home/u/app")).is_some());
        assert!(r.within(Path::new("/home/u/app/cli/venv")).is_some());
        assert!(
            r.within(Path::new("/home/u/other")).is_none(),
            "WorkingDirectory=$HOME must not protect everything"
        );
        assert!(r.within(Path::new("/home/u/.cargo")).is_some());
    }
}
