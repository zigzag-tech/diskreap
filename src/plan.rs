//! Stage 1: scan → plan. Read-only. Every candidate gets a verdict (ok / skip
//! with the reason) using the same `check` that stage 2 re-runs right before it acts.

use crate::act;
use crate::git;
use crate::mounts::{self, Mount};
use crate::procs::{InUse, Refs};
use crate::rules::{Rules, Strat, CACHES, REPORT_ONLY};
use crate::util::{self, fs_space, now, which, DAY, GB};
use crate::walk::{self, Opts};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Low,
    Critical,
}

/// Low: free < 15% or < 30 GiB. Critical: free < 5% or < 10 GiB.
/// Thresholds are a share of the filesystem, clamped to absolute bounds: a
/// bare percentage cries wolf on a 1.8 TB disk with 270 GB free, a bare floor on
/// a 30 GB VM that is half empty. The floor itself is capped at a share of the
/// filesystem, or a 15 GB tmpfs would read "low" at 56% free (both seen in the
/// fleet scans).
fn clamp_share(total: u64, pct: u64, floor: u64, floor_cap_pct: u64, hi: u64) -> u64 {
    let lo = floor.min(total * floor_cap_pct / 100);
    (total * pct / 100).clamp(lo, hi.max(lo))
}

/// Low: free < 15%, at least min(10 GiB, 30%), at most 100 GiB.
/// Critical: free < 5%, at least min(3 GiB, 10%), at most 30 GiB.
pub fn level_of(total: u64, avail: u64) -> Level {
    if avail < clamp_share(total, 5, 3 * GB, 10, 30 * GB) {
        Level::Critical
    } else if avail < clamp_share(total, 15, 10 * GB, 30, 100 * GB) {
        Level::Low
    } else {
        Level::Ok
    }
}

/// `auto` stops once free space is back above this: 20% clamped to [15, 150] GiB.
pub fn target_avail(total: u64) -> u64 {
    clamp_share(total, 20, 15 * GB, 40, 150 * GB)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Policy {
    pub build_idle_days: i64,
    pub worktree_idle_days: i64,
    pub log_min_bytes: u64,
    pub log_idle_hours: i64,
    /// Percent of each cache's normal age horizon.
    pub cache_age_pct: i64,
    /// Stale user-owned entries directly under /tmp, /var/tmp, $TMPDIR.
    pub tmp_idle_days: i64,
    pub docker_until: String,
    pub docker_unused_images: bool,
}

impl Policy {
    pub fn for_level(l: Level) -> Policy {
        match l {
            Level::Critical => Policy {
                build_idle_days: 1,
                worktree_idle_days: 1,
                log_min_bytes: 100 << 20,
                log_idle_hours: 6,
                cache_age_pct: 25,
                tmp_idle_days: 2,
                docker_until: "1h".into(),
                docker_unused_images: true,
            },
            // Daily maintenance while space is fine: only what is clearly stale.
            Level::Ok => Policy {
                build_idle_days: 14,
                worktree_idle_days: 7,
                log_min_bytes: 1 << 30,
                log_idle_hours: 7 * 24,
                cache_age_pct: 200,
                tmp_idle_days: 14,
                docker_until: "168h".into(),
                docker_unused_images: false,
            },
            Level::Low => Policy {
                build_idle_days: 3,
                worktree_idle_days: 3,
                log_min_bytes: 500 << 20,
                log_idle_hours: 24,
                cache_age_pct: 100,
                tmp_idle_days: 7,
                docker_until: "24h".into(),
                docker_unused_images: false,
            },
        }
    }

    pub fn cache_days(&self, days: i64) -> i64 {
        (days * self.cache_age_pct / 100).max(2)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    pub cat: String,
    pub path: String,
    pub bytes: u64,
    pub newest: i64,
    /// true = stage 2 will act on it (after re-checking); false = skip / report-only.
    pub ok: bool,
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CacheRef {
    pub strat: String,
    pub depth: usize,
    pub days: i64,
    pub cmd: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Plan {
    pub host: String,
    pub created: i64,
    pub mode: String,
    pub level: Level,
    pub policy: Policy,
    pub home: String,
    pub total: u64,
    pub avail: u64,
    pub scan_secs: f64,
    /// Seconds per scan phase, in order — where a slow scan spends its time.
    #[serde(default)]
    pub phase_secs: Vec<(String, f64)>,
    pub items: Vec<Item>,
    /// Largest directories (full scan): (path, bytes, newest mtime).
    pub top: Vec<(String, u64, i64)>,
    pub stalled: Vec<String>,
    pub skipped_mounts: Vec<String>,
    pub kept: Vec<String>,
}

impl Plan {
    pub fn path() -> PathBuf {
        util::state_dir().join("plan.json")
    }
    pub fn save(&self) {
        let _ = std::fs::write(Self::path(), serde_json::to_vec_pretty(self).unwrap());
    }
    pub fn load() -> Option<Plan> {
        serde_json::from_slice(&std::fs::read(Self::path()).ok()?).ok()
    }
    pub fn reclaimable(&self) -> u64 {
        self.items.iter().filter(|i| i.ok).map(|i| i.bytes).sum()
    }
}

pub struct Ctx<'a> {
    /// Judged by the pressure of the temp dirs' own filesystem (a full tmpfs
    /// while $HOME is fine — zz-joe, 2026-10-01).
    pub tmp_idle_days: i64,
    pub home: &'a Path,
    pub inuse: &'a InUse,
    pub refs: &'a Refs,
    pub policy: &'a Policy,
    pub mounts: &'a [Mount],
}

/// Shared by stage 1 (verdict) and stage 2 (re-verification right before acting).
/// Err(reason) = do not touch.
pub fn check(it: &Item, cx: &Ctx) -> Result<(), String> {
    let p = Path::new(&it.path);
    if it.cat != "docker" && it.cat != "cache" && it.cat != "tmp" {
        if !p.starts_with(cx.home) || p == cx.home {
            return Err("outside $HOME".into());
        }
        if std::fs::symlink_metadata(p).is_err() {
            return Err("gone".into());
        }
    }
    let t = now();
    match it.cat.as_str() {
        "tmp" => {
            let md = std::fs::symlink_metadata(p).map_err(|_| "gone")?;
            if !tmp_roots().iter().any(|r| p.parent() == Some(r.as_path())) {
                return Err("not directly under a temp dir".into());
            }
            if !crate::platform::owned_by_me(&md) {
                return Err("owned by another user".into());
            }
            if let Some(h) = cx.inuse.under(p) {
                return Err(format!("in use ({})", h.display()));
            }
            let st = act::tree_stats(p);
            if st.special {
                return Err("holds a socket or FIFO (a live session's rendezvous)".into());
            }
            idle(t, st.newest, cx.tmp_idle_days)
        }
        "build-output" if under_install_root(p, cx.home).is_some() => Err(format!(
            "inside an installation root ({}): installed software, not build output",
            under_install_root(p, cx.home).unwrap()
        )),
        "build-output" if !has_project_manifest(p.parent().unwrap_or(p)) => {
            Err("no project manifest beside it: an installation, not build output".into())
        }
        "build-output" if cache_tagged(p) && git::toplevel(p.parent().unwrap_or(p)).is_none() => {
            // Declared regenerable by its producer: no git evidence needed, but
            // the same use/reference/idle floor as any build output.
            let parent = p.parent().unwrap_or(p);
            if let Some(h) = cx.inuse.under(parent) {
                return Err(format!("project in use ({})", h.display()));
            }
            if let Some(src) = cx.refs.within(parent) {
                return Err(format!("project referenced by {src}"));
            }
            idle(
                t,
                it.newest.max(util::mtime_of(p)),
                cx.policy.build_idle_days,
            )
        }
        "build-output" => {
            let top = git::toplevel(p.parent().unwrap_or(p)).ok_or("not inside a git repo")?;
            if let Some(h) = cx.inuse.under(&top) {
                return Err(format!("project in use ({})", h.display()));
            }
            if let Some(src) = cx.refs.within(&top) {
                return Err(format!("project referenced by {src}"));
            }
            let last = it.newest.max(git::activity(&top)).max(util::mtime_of(p));
            idle(t, last, cx.policy.build_idle_days)?;
            if !git::ignored_untracked(&top, p) {
                return Err("not git-ignored, or contains tracked files".into());
            }
            Ok(())
        }
        "worktree" => {
            let gitdir = git::linked_worktree_gitdir(p).ok_or("not a linked worktree")?;
            if git::locked(&gitdir) {
                return Err("locked (git worktree lock)".into());
            }
            if let Some(h) = cx.inuse.under(p) {
                return Err(format!("in use ({})", h.display()));
            }
            if let Some(src) = cx.refs.within(p) {
                return Err(format!("referenced by {src}"));
            }
            if !gitdir.exists() {
                // Its admin dir was pruned or belongs to a deleted clone: git can no
                // longer vouch for the files — a person must look.
                return Err("orphaned worktree (its git admin dir is gone)".into());
            }
            // Cheapest decisive checks first: on a busy host most worktrees are
            // active, and `git status` over 300 of them took 226 s (xc-tower-ubuntu).
            let top_mtime = std::fs::read_dir(p)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| util::mtime_of(&e.path()))
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            // Detached HEAD is often a deliberate pin (referenced by path elsewhere): wait much longer.
            let days = if git::detached(p) {
                cx.policy.worktree_idle_days.max(30)
            } else {
                cx.policy.worktree_idle_days
            };
            idle(t, it.newest.max(git::activity(p)).max(top_mtime), days)?;
            let base = git::base_ref(p).ok_or("no main/master/origin HEAD to compare with")?;
            if !git::merged_into(p, &base) {
                return Err(format!("has commits not in {base}"));
            }
            if git::stash_names_branch(p) {
                return Err("a stash entry names its branch".into());
            }
            match git::dirty_count(p) {
                Some(0) => {}
                Some(n) => return Err(format!("{n} uncommitted/untracked change(s)")),
                None => return Err("git status failed".into()),
            }
            let main = git::main_checkout(p).ok_or("main checkout not found")?;
            if let Some(f) = git::ignored_data(p, &main) {
                return Err(format!("holds ignored data not in the main checkout: {f}"));
            }
            Ok(())
        }
        "log" => {
            if cx.inuse.under(p).is_some() {
                return Err("open by a process".into());
            }
            if let Some(src) = cx.refs.within(p) {
                return Err(format!("referenced by {src}"));
            }
            let md = std::fs::metadata(p).map_err(|e| e.to_string())?;
            if md.len() < cx.policy.log_min_bytes {
                return Err(format!("< {}", util::human(cx.policy.log_min_bytes)));
            }
            let mt = crate::platform::mtime(&md);
            if t - mt < cx.policy.log_idle_hours * 3600 {
                return Err(format!("written {} ago", util::ago(mt)));
            }
            if let Some(top) = git::toplevel(p.parent().unwrap_or(p)) {
                if git::tracked(&top, p) {
                    return Err("tracked by git".into());
                }
            }
            if which("zstd").is_none() && which("gzip").is_none() {
                return Err("no zstd/gzip".into());
            }
            Ok(())
        }
        "cache" => {
            let c = it.cache.as_ref().ok_or("bad plan item")?;
            if c.strat == "cmd" {
                let bin = expand(&c.cmd[0], cx.home);
                if !(Path::new(&bin).is_file() || which(&bin).is_some()) {
                    return Err(format!("{} not installed", c.cmd[0]));
                }
                return Ok(());
            }
            if let Some(m) = mounts::containing(cx.mounts, p) {
                if m.opts.split(',').any(|o| o == "noatime") {
                    return Err("filesystem mounted noatime: last-use unknown".into());
                }
            }
            Ok(())
        }
        "docker" => which("docker")
            .map(|_| ())
            .ok_or_else(|| "docker not installed".into()),
        _ => Err("report only".into()),
    }
}

fn idle(t: i64, last: i64, days: i64) -> Result<(), String> {
    if t - last < days * DAY {
        Err(format!("active {} ago (< {days}d)", util::ago(last)))
    } else {
        Ok(())
    }
}

/// System and per-user temp dirs (on macOS $TMPDIR is a per-user /var/folders dir).
pub fn tmp_roots() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = vec!["/tmp".into(), "/var/tmp".into(), std::env::temp_dir()];
    v.iter_mut().for_each(|p| *p = crate::platform::canon(p));
    v.sort();
    v.dedup();
    v.retain(|p| p.is_dir());
    v
}

/// Where installed software lives. A venv or a tagged directory here is an
/// installation (a uv/pipx tool, an app's runtime or models), never build output.
const INSTALL_ROOTS: &[&str] = &[
    ".local/share", ".local/lib", ".local/pipx", ".cargo", ".rustup", ".nvm", ".volta", ".pyenv", ".sdkman",
    ".npm-global", ".bun/install/global", "go/pkg", "anaconda3", "miniconda3", ".conda", ".julia",
    "Library/Application Support", "AppData/Roaming", "AppData/Local/Programs",
];

pub fn under_install_root(p: &Path, home: &Path) -> Option<&'static str> {
    INSTALL_ROOTS.iter().copied().find(|r| p.starts_with(home.join(r)))
}

/// Build output sits beside the manifest that regenerates it.
const PROJECT_MANIFESTS: &[&str] = &[
    "Cargo.toml", "package.json", "pyproject.toml", "setup.py", "setup.cfg", "Pipfile", "uv.lock", "poetry.lock",
    "pubspec.yaml", "build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "pom.xml",
    "CMakeLists.txt", "meson.build", "Podfile", "Package.swift", "go.mod", "tox.ini", "Gemfile", "mix.exs",
];

pub fn has_project_manifest(dir: &Path) -> bool {
    PROJECT_MANIFESTS.iter().any(|m| dir.join(m).is_file())
        || std::fs::read_dir(dir).map(|rd| rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("requirements") && n.ends_with(".txt")
        })).unwrap_or(false)
}

/// Valid Cache Directory Tagging signature (https://bford.info/cachedir/).
pub fn cache_tagged(dir: &Path) -> bool {
    std::fs::read(dir.join("CACHEDIR.TAG"))
        .map(|b| b.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55"))
        .unwrap_or(false)
}

/// A filesystem diskreap can clean, with its own pressure level.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FsState {
    pub path: String,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub level: Level,
}

/// $HOME's filesystem first, then each temp root on a different filesystem.
pub fn watched_filesystems() -> Vec<FsState> {
    let mut out: Vec<FsState> = Vec::new();
    let mut seen: Vec<(u64, u64)> = Vec::new();
    for p in std::iter::once(util::home()).chain(tmp_roots()) {
        let Some((t, a)) = fs_space(&p) else { continue };
        // Same size and (nearly) same free space ≈ same filesystem; cheap and portable.
        if seen
            .iter()
            .any(|(st, sa)| *st == t && sa.abs_diff(a) < 64 << 20)
        {
            continue;
        }
        seen.push((t, a));
        out.push(FsState {
            path: p.to_string_lossy().into_owned(),
            total_bytes: t,
            avail_bytes: a,
            level: level_of(t, a),
        });
    }
    out
}

/// Policy for temp entries: the stricter of the scan level and the temp filesystems' own.
pub fn tmp_idle_days(scan_level: Level) -> i64 {
    let tmp_level = watched_filesystems()
        .iter()
        .skip(1)
        .map(|f| f.level)
        .max()
        .unwrap_or(Level::Ok);
    Policy::for_level(scan_level.max(tmp_level)).tmp_idle_days
}

pub fn expand(s: &str, home: &Path) -> String {
    match s.strip_prefix("~/") {
        Some(r) => home.join(r).to_string_lossy().into_owned(),
        None => s.to_string(),
    }
}

pub struct ScanOpts {
    pub full: bool,
    pub level: Level,
    pub root: Option<PathBuf>,
    pub threads: usize,
}

pub fn scan(o: &ScanOpts) -> Plan {
    let started = Instant::now();
    let home = util::home();
    let root = o.root.clone().unwrap_or_else(|| home.clone());
    let policy = Policy::for_level(o.level);
    let mounts = mounts::list();
    let (total, avail) = fs_space(&home).unwrap_or((0, 0));

    let mut phases: Vec<(String, f64)> = Vec::new();
    let mut lap = Instant::now();
    let mut mark = |name: &str| {
        phases.push((name.to_string(), lap.elapsed().as_secs_f64()));
        lap = Instant::now();
    };
    let w = walk::walk(
        Opts {
            skip: mounts::below(&mounts, &root),
            root: root.clone(),
            size_all: o.full,
            report_depth: 3,
            max_depth: if o.full { None } else { Some(6) },
            stall: Duration::from_secs(15),
            threads: o.threads,
        },
        Box::new(Rules { quick: !o.full }),
    );
    mark("walk");
    let inuse = InUse::snapshot();
    let refs = Refs::collect(&home);
    let cx = Ctx {
        tmp_idle_days: tmp_idle_days(o.level),
        home: &home,
        inuse: &inuse,
        refs: &refs,
        policy: &policy,
        mounts: &mounts,
    };
    if std::env::var_os("DISKREAP_DEBUG").is_some() {
        for f in &w.found {
            eprintln!("found {} {} {}", f.kind, f.path.display(), f.bytes);
        }
    }
    let mut items: Vec<Item> = Vec::new();

    // Worktrees first: a build-output inside an ok worktree goes with the worktree.
    mark("in-use+refs");
    let wts: Vec<Item> = w
        .found
        .iter()
        .filter(|f| f.kind == "worktree" && git::linked_worktree_gitdir(&f.path).is_some()) // not submodules
        .map(|f| item("worktree", &f.path, f.bytes, f.newest))
        .collect();
    let mut wts = verdicts(wts, &cx);
    for it in wts.iter_mut().filter(|i| i.ok) {
        let sized = w
            .found
            .iter()
            .any(|f| f.sized && f.path == Path::new(&it.path));
        if !sized {
            it.bytes = act::du(Path::new(&it.path), &mounts); // quick scan sizes eligible ones only
        }
    }
    let ok_worktrees: Vec<PathBuf> = wts
        .iter()
        .filter(|i| i.ok)
        .map(|i| PathBuf::from(&i.path))
        .collect();
    items.extend(wts);
    mark("worktrees");
    let builds: Vec<Item> = w
        .found
        .iter()
        .filter(|f| {
            f.kind == "build-output"
                && f.bytes >= 10 << 20
                && !ok_worktrees.iter().any(|wt| f.path.starts_with(wt))
        })
        .map(|f| item("build-output", &f.path, f.bytes, f.newest))
        .collect();
    items.extend(verdicts(builds, &cx));
    mark("build-output");
    for f in &w.files {
        if ok_worktrees.iter().any(|wt| f.path.starts_with(wt)) {
            continue;
        }
        let mut it = item("log", &f.path, f.size * 9 / 10, f.mtime);
        verdict(&mut it, &cx);
        items.push(it);
    }

    mark("logs");
    for c in CACHES {
        let dir = home.join(c.rel);
        if !dir.is_dir() {
            continue;
        }
        let (strat, depth, cmd) = match c.strat {
            Strat::AgeFiles => ("age-files", 0, vec![]),
            Strat::AgeEntries(d) => ("age-entries", d, vec![]),
            Strat::Cmd(a) => ("cmd", 0, a.iter().map(|s| s.to_string()).collect()),
        };
        let days = policy.cache_days(c.days);
        let mut it = item("cache", &dir, 0, 0);
        it.cache = Some(CacheRef {
            strat: strat.into(),
            depth,
            days,
            cmd,
        });
        verdict(&mut it, &cx);
        if it.ok {
            let r = act::cache_pass(&it, &cx, false);
            it.bytes = r.bytes;
            if strat == "cmd" {
                it.why = format!(
                    "{} total; the tool prunes what is unused",
                    util::human(r.total)
                );
            } else if r.bytes == 0 {
                it.ok = false;
                it.why = format!("{} total, nothing unused > {days}d", util::human(r.total));
            } else {
                it.why = format!("unused > {days}d ({} total)", util::human(r.total));
            }
        }
        items.push(it);
    }

    mark("caches");
    if which("docker").is_some() {
        let mut it = item("docker", Path::new("docker"), 0, 0);
        verdict(&mut it, &cx);
        if it.ok {
            match act::docker_reclaimable(policy.docker_unused_images) {
                Some((b, why)) => {
                    it.bytes = b;
                    it.why = why;
                    it.ok = b > 0;
                }
                None => {
                    it.ok = false;
                    it.why = "docker daemon not answering".into();
                }
            }
        }
        items.push(it);
    }

    for f in w
        .found
        .iter()
        .filter(|f| f.kind == "app-cache" && f.bytes >= GB)
    {
        // Nested app caches (.cache/foo-cache) report once, at the outermost.
        if w.found
            .iter()
            .any(|o| o.kind == "app-cache" && o.path != f.path && f.path.starts_with(&o.path))
        {
            continue;
        }
        if git::toplevel(f.path.parent().unwrap_or(&f.path)).is_none() {
            continue; // ~/.cache and friends: the CACHES table handles those
        }
        let mut it = item("report", &f.path, f.bytes, f.newest);
        it.why = format!(
            "app cache, modified {} ago — safe only if the app re-derives it",
            util::ago(f.newest)
        );
        items.push(it);
    }
    mark("docker");
    let mut tmps: Vec<Item> = Vec::new();
    for root in tmp_roots() {
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let st = act::tree_stats(&e.path());
            if st.bytes >= 10 << 20 {
                tmps.push(item("tmp", &e.path(), st.bytes, st.newest));
            }
        }
    }
    items.extend(verdicts(tmps, &cx));

    for (rel, why) in REPORT_ONLY {
        let p = home.join(rel);
        if p.is_dir() {
            let b = act::du(&p, &mounts);
            if b > 100 << 20 {
                let mut it = item("report", &p, b, 0);
                it.why = why.to_string();
                items.push(it);
            }
        }
    }

    mark("tmp+report");
    // Cold data (full scan): big, untouched for 90d, not a candidate already.
    let mut top: Vec<(String, u64, i64)> = Vec::new();
    if o.full {
        let mut nodes = w.nodes.clone();
        nodes.sort_by(|a, b| b.2.cmp(&a.2));
        let mut cold: Vec<PathBuf> = Vec::new();
        for (p, depth, b, newest) in &nodes {
            if *depth >= 1 && *depth <= 2 && top.len() < 25 {
                top.push((p.to_string_lossy().into_owned(), *b, *newest));
            }
            if *depth >= 1
                && *b >= 5 * GB
                && now() - newest > 90 * DAY
                && !cold.iter().any(|c| p.starts_with(c))
            {
                cold.push(p.clone());
                let mut it = item("report", p, *b, *newest);
                it.why = format!("cold: nothing modified in {}", util::ago(*newest));
                items.push(it);
            }
        }
    }

    // Keep plans small: tiny skips are noise.
    let debug = std::env::var_os("DISKREAP_DEBUG").is_some();
    items.retain(|i| {
        debug || i.ok || i.bytes >= 100 << 20 || i.cat == "docker" || i.cat == "worktree"
    });
    items.sort_by(|a, b| b.ok.cmp(&a.ok).then(b.bytes.cmp(&a.bytes)));

    let s = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    Plan {
        host: util::hostname(),
        created: now(),
        mode: if o.full {
            "full".into()
        } else {
            "quick".into()
        },
        level: level_of(total, avail),
        policy,
        home: home.to_string_lossy().into_owned(),
        total,
        avail,
        scan_secs: started.elapsed().as_secs_f64(),
        phase_secs: phases,
        items,
        top,
        stalled: s(&w.stalled),
        skipped_mounts: s(&w.skipped_mounts),
        kept: s(&w.kept),
    }
}

fn item(cat: &str, p: &Path, bytes: u64, newest: i64) -> Item {
    Item {
        cat: cat.into(),
        path: p.to_string_lossy().into_owned(),
        bytes,
        newest,
        ok: false,
        why: String::new(),
        cache: None,
    }
}

/// Verdicts are dominated by git subprocesses: run them in parallel.
fn verdicts(mut v: Vec<Item>, cx: &Ctx) -> Vec<Item> {
    let n = v.len().div_ceil(16).max(1);
    std::thread::scope(|s| {
        for chunk in v.chunks_mut(n) {
            s.spawn(move || chunk.iter_mut().for_each(|it| verdict(it, cx)));
        }
    });
    v
}

fn verdict(it: &mut Item, cx: &Ctx) {
    match check(it, cx) {
        Ok(()) => it.ok = true,
        Err(e) => {
            it.ok = false;
            it.why = e;
        }
    }
}

/// Docker's own estimate, used only for display.
pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let i = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let v: f64 = s[..i].parse().unwrap_or(0.0);
    let mult = match s[i..].trim().to_ascii_uppercase().as_str() {
        "KB" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        _ => 1.0,
    };
    (v * mult) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        let t = 1800 * GB; // big disk: 15% would be 270 GiB — capped at 100
        assert_eq!(level_of(t, 270 * GB), Level::Ok);
        assert_eq!(level_of(t, 99 * GB), Level::Low);
        assert_eq!(level_of(t, 29 * GB), Level::Critical);
        assert_eq!(target_avail(t), 150 * GB);
        let small = 30 * GB; // small VM: 50% free is fine
        assert_eq!(level_of(small, 15 * GB), Level::Ok);
        assert_eq!(level_of(small, 8 * GB), Level::Low); // floor = min(10 GiB, 30%) = 9 GiB
        assert_eq!(level_of(small, 2 * GB), Level::Critical);
        assert_eq!(level_of(400 * GB, 39 * GB), Level::Low); // 15% = 60 GiB
        let tmpfs = 15 * GB + GB / 2; // half-empty small tmpfs is fine; nearly full is not
        assert_eq!(level_of(tmpfs, 8 * GB), Level::Ok);
        assert_eq!(level_of(tmpfs, 3 * GB), Level::Low);
        assert_eq!(level_of(tmpfs, GB), Level::Critical);
    }

    #[test]
    fn docker_sizes() {
        assert_eq!(parse_size("93.41GB"), 93_410_000_000);
        assert_eq!(parse_size("0B"), 0);
        assert_eq!(
            parse_size("436.8MB (75%)".split(' ').next().unwrap()),
            436_800_000
        );
    }
}
