//! "Is anything using this path right now?" — a snapshot of every path a live
//! process holds: cwd, executable, open files and mmapped libraries (a running
//! server may have its node_modules native addons mapped with cwd elsewhere),
//! plus bind-mount sources of running Docker containers (their processes are
//! usually another uid and invisible in /proc to us), plus every absolute path
//! on a process's command line (`node --import=/tmp/…/x.mjs` loads a module and
//! closes it: no fd, no mapping, yet the process needs the file again on restart
//! or dynamic import).

use crate::util::{self, run, which};
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
            u.ps_args();
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

    /// `dir` itself is held (a process's cwd, a pane's path, a mount source, a command-line
    /// argument), not just something below it. Such a directory is someone's workspace:
    /// it is never split up and judged entry by entry.
    pub fn holds(&self, dir: &Path) -> bool {
        !self.complete || self.set.contains(dir)
    }

    /// Paths on a command line: arguments that are absolute paths, and the value of
    /// `--opt=/path`. Our own process and its ancestors are skipped: `diskreap reap X`
    /// (or the `ssh … sh -c` that runs it) is not a user of X.
    fn add_args<'a>(&mut self, args: impl Iterator<Item = &'a str>) {
        for a in args {
            self.add(a);
            if let Some((_, v)) = a.split_once('=') {
                self.add(v);
            }
        }
    }

    /// macOS/BSD: `lsof` has no argv; `ps` does (space-split, so a path with spaces is
    /// cut short — still a prefix of the real path, i.e. it protects a parent).
    fn ps_args(&mut self) {
        let Some(o) = run(&["ps", "-axww", "-o", "pid=,ppid=,args="], None, Duration::from_secs(30)) else {
            return;
        };
        let rows: Vec<(u32, u32, &str)> = o
            .stdout
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let pid = it.next()?.parse().ok()?;
                let ppid = it.next()?.parse().ok()?;
                let rest = l.trim_start().splitn(3, char::is_whitespace).nth(2).unwrap_or("");
                Some((pid, ppid, rest))
            })
            .collect();
        let parents: std::collections::HashMap<u32, u32> = rows.iter().map(|r| (r.0, r.1)).collect();
        let skip = ancestors(std::process::id(), |p| parents.get(&p).copied());
        for (pid, _, args) in rows {
            if !skip.contains(&pid) {
                self.add_args(args.split_whitespace());
            }
        }
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
        let skip = ancestors(std::process::id(), linux_ppid);
        for e in rd.flatten() {
            let pid = e.file_name();
            if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let base = e.path();
            let own = pid.to_string_lossy().parse().is_ok_and(|p: u32| skip.contains(&p));
            if let (false, Ok(cmd)) = (own, std::fs::read(base.join("cmdline"))) {
                let cmd = String::from_utf8_lossy(&cmd);
                self.add_args(cmd.split('\0'));
            }
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

/// `pid` and every ancestor of it, by a parent lookup.
fn ancestors(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> BTreeSet<u32> {
    let mut v = BTreeSet::new();
    let mut p = pid;
    while p > 1 && v.insert(p) {
        match parent(p) {
            Some(q) => p = q,
            None => break,
        }
    }
    v
}

/// PPID from /proc/<pid>/stat (field 4; the command name in field 2 may hold spaces and parens).
fn linux_ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s[s.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_paths_count_as_in_use_but_not_our_own() {
        let mut u = InUse::from(&[]);
        u.add_args(["node", "--import=/tmp/s/x.mjs", "/srv/app/main.js", "rel/path"].into_iter());
        assert!(u.under(Path::new("/tmp/s")).is_some());
        assert!(u.under(Path::new("/srv/app")).is_some());
        assert!(u.holds(Path::new("/srv/app/main.js")));
        assert!(!u.holds(Path::new("/srv/app")), "a path below is not the dir itself");
        let me = std::process::id();
        let skip = ancestors(me, linux_ppid);
        if cfg!(target_os = "linux") {
            assert!(skip.contains(&me) && skip.len() >= 2, "self and parent: {skip:?}");
        }
    }

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
        r.config_files(home);
        r.path_commands(home);
        r.operator_protected();
        r.through_links();
        r
    }

    /// Small text files a service definition names (`Environment=UNITS=/etc/x/units.json`)
    /// are scanned too, one hop: a broker that starts its engines on demand names their
    /// interpreters there, not in its unit. Missing that deleted the venv of an idle
    /// Harmony LLM unit, so every request to it failed (2026-11-06).
    fn config_files(&mut self, home: &Path) {
        let named: Vec<(PathBuf, String)> = self.refs.clone();
        let mut seen = BTreeSet::new();
        for (p, src) in named {
            let small = std::fs::metadata(&p).map(|m| m.is_file() && m.len() < 256 << 10).unwrap_or(false);
            if !small || !seen.insert(p.clone()) {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(&p) {
                self.scan_text(&s, &format!("{} (named by {src})", p.display()), home);
            }
        }
    }

    /// A path reached through a directory symlink protects the symlink's target too
    /// (`a/.venv -> b/.venv`: deleting b's venv breaks a). Only the parent is resolved;
    /// a venv's `bin/python -> /usr/bin/python3` must not move the reference out of it.
    fn through_links(&mut self) {
        let mut extra = Vec::new();
        for (p, src) in &self.refs {
            let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { continue };
            if let Ok(real) = std::fs::canonicalize(parent) {
                let real = real.join(name);
                if &real != p {
                    extra.push((real, src.clone()));
                }
            }
        }
        self.refs.extend(extra);
    }

    /// Operator-declared protection, in the diskreap config file (see README):
    /// `protect_paths` (globs) and `protect_config_refs` (`[{"file": F, "keys": [K..]}]`:
    /// every string value stored under key K anywhere in JSON file F is protected). The
    /// second form keeps a tool's own registry of checkouts (a release system's source
    /// root) from pointing at a worktree this tool then removes (2026-10-14).
    fn operator_protected(&mut self) {
        let Some(cfg) = util::config_json() else { return };
        let bad = |what: &str| -> ! {
            eprintln!("diskreap: config {what}");
            std::process::exit(2)
        };
        if let Some(v) = cfg.get("protect_paths") {
            let Some(arr) = v.as_array() else { bad("protect_paths must be an array of glob strings") };
            for g in arr {
                let Some(g) = g.as_str() else { bad("protect_paths must be an array of glob strings") };
                let pat = util::expand_tilde(g);
                let Ok(paths) = glob::glob(&pat.to_string_lossy()) else { bad(&format!("protect_paths: bad glob {g:?}")) };
                // A glob that matches nothing right now still protects its literal prefix.
                let mut any = false;
                for m in paths.flatten() {
                    any = true;
                    self.refs.push((m, format!("protect_paths {g}")));
                }
                if !any {
                    self.refs.push((pat, format!("protect_paths {g}")));
                }
            }
        }
        if let Some(v) = cfg.get("protect_config_refs") {
            let Some(arr) = v.as_array() else { bad("protect_config_refs must be an array of {file, keys}") };
            for e in arr {
                let file = e.get("file").and_then(|f| f.as_str());
                let keys: Option<Vec<&str>> = e
                    .get("keys")
                    .and_then(|k| k.as_array())
                    .map(|a| a.iter().filter_map(|k| k.as_str()).collect());
                let (Some(file), Some(keys)) = (file, keys) else { bad("protect_config_refs entries need string `file` and array `keys`") };
                let path = util::expand_tilde(file);
                let Ok(text) = std::fs::read_to_string(&path) else { continue }; // absent on this host: nothing to protect
                let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
                    eprintln!("diskreap: protect_config_refs: {} is not valid JSON; its paths are NOT protected", path.display());
                    continue;
                };
                let src = format!("{} ({})", path.display(), keys.join(","));
                collect_keyed(&doc, &keys, &src, &mut self.refs);
            }
        }
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

    /// Paths the operator protected with `protect_paths`, with the glob that did it.
    pub fn protected(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.refs
            .iter()
            .filter(|(_, s)| s.starts_with("protect_paths "))
            .map(|(p, s)| (p.as_path(), s.as_str()))
    }

    #[cfg(test)]
    pub fn from_text(s: &str, home: &Path) -> Refs {
        let mut r = Refs::default();
        r.scan_text(s, "test", home);
        r
    }
}

/// Every string value under any of `keys`, at any depth of `v`.
fn collect_keyed(v: &serde_json::Value, keys: &[&str], src: &str, out: &mut Vec<(PathBuf, String)>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m {
                match val.as_str() {
                    Some(s) if keys.contains(&k.as_str()) && !s.is_empty() => out.push((util::expand_tilde(s), src.to_string())),
                    _ => collect_keyed(val, keys, src, out),
                }
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_keyed(x, keys, src, out)),
        _ => {}
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

    #[test]
    fn a_named_config_file_and_a_linked_venv_protect_the_real_venv() {
        let d = std::env::temp_dir().join(format!("dr-linked-venv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let home = d.as_path();
        let real = home.join("engine/.venv/bin");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("python"), "").unwrap();
        std::fs::create_dir_all(home.join("fork")).unwrap();
        std::os::unix::fs::symlink(home.join("engine/.venv"), home.join("fork/.venv")).unwrap();
        let units = home.join("units.json");
        std::fs::write(&units, format!("{{\"python\": \"{}/fork/.venv/bin/python\"}}", home.display())).unwrap();
        let mut r = Refs::from_text(&format!("Environment=UNITS_FILE={}\n", units.display()), Path::new("/nowhere"));
        r.config_files(Path::new("/nowhere"));
        r.through_links();
        assert!(r.within(&home.join("engine/.venv")).is_some());
        std::fs::remove_dir_all(&d).unwrap();
    }
}

/// Best-effort "who is working here": processes whose cwd is inside a directory
/// (Linux /proc) and tmux panes whose current path is. Used only to label the
/// worktree report; never to decide anything.
#[derive(Default)]
pub struct Owners {
    cwds: Vec<(PathBuf, String)>,
}

impl Owners {
    pub fn snapshot() -> Owners {
        let mut o = Owners::default();
        if cfg!(target_os = "linux") {
            if let Ok(rd) = std::fs::read_dir("/proc") {
                for e in rd.flatten() {
                    let pid = e.file_name().to_string_lossy().into_owned();
                    if !pid.bytes().all(|b| b.is_ascii_digit()) {
                        continue;
                    }
                    let Ok(cwd) = std::fs::read_link(e.path().join("cwd")) else { continue };
                    let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
                    o.cwds.push((cwd, format!("{} pid {pid}", comm.trim())));
                }
            }
        }
        if which("tmux").is_some() {
            if let Some(out) = run(
                &["tmux", "list-panes", "-a", "-F", "#{pane_current_path}\t#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}"],
                None,
                Duration::from_secs(10),
            ) {
                for l in out.stdout.lines() {
                    if let Some((path, label)) = l.split_once('\t') {
                        o.cwds.push((PathBuf::from(path), format!("tmux {label}")));
                    }
                }
            }
        }
        o
    }

    /// Up to three distinct labels of holders at or below `dir`, then the worktree lock reason if any.
    pub fn of(&self, dir: &Path, gitdir: Option<&Path>) -> String {
        let mut v: Vec<String> = Vec::new();
        for (p, l) in &self.cwds {
            if p.starts_with(dir) && !v.contains(l) {
                v.push(l.clone());
            }
        }
        v.truncate(3);
        if let Some(g) = gitdir {
            if let Ok(r) = std::fs::read_to_string(g.join("locked")) {
                let r = r.trim();
                v.push(if r.is_empty() { "git-locked".into() } else { format!("git-locked: {r}") });
            }
        }
        if v.is_empty() { "-".into() } else { v.join(", ") }
    }
}

#[cfg(test)]
mod protect_tests {
    use super::*;

    #[test]
    fn keyed_values_at_any_depth_are_protected_and_other_keys_are_not() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"apps":[{"sourceRoot":"/w/a","adapterDirectory":"/w/b"}],"sourceRoot2":"/w/c"}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        collect_keyed(&doc, &["sourceRoot"], "cfg", &mut out);
        let r = Refs { refs: out };
        assert!(r.within(Path::new("/w/a")).is_some());
        assert!(r.within(Path::new("/w")).is_some(), "a parent of a protected path is protected too");
        assert!(r.within(Path::new("/w/b")).is_none());
        assert!(r.within(Path::new("/w/c")).is_none());
    }

    #[test]
    fn config_file_protects_globs_and_referenced_checkouts() {
        let d = std::env::temp_dir().join(format!("dr-protect-{}", std::process::id()));
        std::fs::create_dir_all(d.join("wt/keep-1")).unwrap();
        std::fs::write(d.join("reg.json"), r#"{"apps":[{"sourceRoot":"/reg/src"}]}"#).unwrap();
        let cfg = format!(
            r#"{{"protect_paths":["{0}/wt/keep-*"],"protect_config_refs":[{{"file":"{0}/reg.json","keys":["sourceRoot"]}},{{"file":"{0}/absent.json","keys":["x"]}}]}}"#,
            d.display()
        );
        std::fs::write(d.join("config.json"), cfg).unwrap();
        std::env::set_var("DISKREAP_CONFIG", d.join("config.json"));
        let mut r = Refs::default();
        r.operator_protected();
        std::env::remove_var("DISKREAP_CONFIG");
        assert!(r.within(&d.join("wt/keep-1")).is_some());
        assert!(r.within(&d.join("wt/other")).is_none());
        assert!(r.within(Path::new("/reg/src")).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
