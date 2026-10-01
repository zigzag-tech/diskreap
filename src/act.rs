//! Stage 2: act on a plan. Every item is re-checked (`plan::check`) against a
//! fresh process snapshot immediately before anything is touched; every action
//! is appended to actions.jsonl.

use crate::git;
use crate::mounts::{self, Mount};
use crate::plan::{self, Ctx, Item, Plan};
use crate::procs::{InUse, Refs};
use crate::util::{self, fs_space, human, now, run, which, DAY};
use crate::walk::{self, Opts};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

/// Hang-proof size of one tree (same walker, sizing everything).
pub fn du(p: &Path, mounts: &[Mount]) -> u64 {
    struct None_;
    impl walk::Classify for None_ {
        fn child_dir(&self, _: &Path, _: &str, _: &[String]) -> Option<walk::DirHit> {
            None
        }
        fn self_dir(&self, _: &Path, _: &[(String, bool)]) -> Option<walk::DirHit> {
            None
        }
        fn file_interest(&self, _: &str) -> bool {
            false
        }
        fn file(&self, _: &Path, _: &str, _: u64) -> Option<&'static str> {
            None
        }
    }
    walk::walk(
        Opts {
            root: p.to_path_buf(),
            skip: mounts::below(mounts, p),
            size_all: true,
            report_depth: 0,
            max_depth: None,
            stall: Duration::from_secs(15),
            threads: 16,
        },
        Box::new(None_),
    )
    .total
}

/// rm -rf that stays on one filesystem, never follows symlinks, and copes with
/// read-only trees (sealed release copies are chmod a-w): it restores u+rwx on
/// directories it is about to empty — never sudo.
pub fn remove_tree(p: &Path) -> std::io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if let Some(parent) = p.parent() {
        ensure_owner_rwx(parent)?;
    }
    rm_rec(p, md.dev(), &md)
}

fn ensure_owner_rwx(d: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(d)?;
    if m.is_dir() && m.mode() & 0o700 != 0o700 {
        fs::set_permissions(d, fs::Permissions::from_mode(m.mode() | 0o700))?;
    }
    Ok(())
}

fn rm_rec(p: &Path, dev: u64, md: &fs::Metadata) -> std::io::Result<()> {
    if !md.is_dir() {
        return fs::remove_file(p);
    }
    if md.dev() != dev {
        return Err(std::io::Error::other(format!(
            "{} is another filesystem",
            p.display()
        )));
    }
    ensure_owner_rwx(p)?;
    for e in fs::read_dir(p)? {
        let e = e?;
        let cm = fs::symlink_metadata(e.path())?;
        rm_rec(&e.path(), dev, &cm)?;
    }
    fs::remove_dir(p)
}

pub struct CacheResult {
    pub total: u64,
    pub bytes: u64,
}

/// Measure (delete=false) or prune (delete=true) one cache per its strategy.
/// "Last used" = max(atime, mtime); a file or entry held by a process is kept.
pub fn cache_pass(it: &Item, cx: &Ctx, delete: bool) -> CacheResult {
    let c = it.cache.as_ref().unwrap();
    let dir = Path::new(&it.path);
    let mut r = CacheResult { total: 0, bytes: 0 };
    if c.strat == "cmd" {
        r.total = du(dir, cx.mounts);
        if delete {
            let args: Vec<String> = c.cmd.iter().map(|s| plan::expand(s, cx.home)).collect();
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = run(&a, Some(dir), Duration::from_secs(1800));
            r.bytes = r.total.saturating_sub(du(dir, cx.mounts));
        }
        return r;
    }
    let cutoff = now() - c.days * DAY;
    let root_dev = match fs::symlink_metadata(dir) {
        Ok(m) => m.dev(),
        Err(_) => return r,
    };
    if c.strat == "age-files" {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let Ok(m) = fs::symlink_metadata(e.path()) else {
                    continue;
                };
                if m.is_dir() {
                    if m.dev() == root_dev {
                        stack.push(e.path());
                    }
                    continue;
                }
                let b = m.blocks() * 512;
                r.total += b;
                if m.atime().max(m.mtime()) < cutoff
                    && cx.inuse.under(&e.path()).is_none()
                    && (!delete || fs::remove_file(e.path()).is_ok())
                {
                    r.bytes += b;
                }
            }
        }
        return r;
    }
    // age-entries
    let mut level = vec![dir.to_path_buf()];
    for i in 0..c.depth {
        let last = i + 1 == c.depth;
        // Intermediate levels are directories (index/org dirs); the final level
        // is the unit of deletion and may be a file (a .crate tarball).
        level = level
            .iter()
            .flat_map(|d| {
                fs::read_dir(d)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
            })
            .filter(|p| last || fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false))
            .collect();
    }
    for e in level {
        let (b, used) = tree_usage(&e, root_dev);
        r.total += b;
        if used < cutoff && cx.inuse.under(&e).is_none() && (!delete || remove_tree(&e).is_ok()) {
            r.bytes += b;
        }
    }
    r
}

/// (allocated bytes, newest max(atime, mtime)) of a tree — caches are local and
/// modest, so a sequential pass is fine here.
fn tree_usage(p: &Path, dev: u64) -> (u64, i64) {
    let Ok(m) = fs::symlink_metadata(p) else {
        return (0, i64::MAX);
    };
    let mut b = m.blocks() * 512;
    // Directory atime moves whenever anything lists it (including this scan):
    // only a FILE's atime means "used".
    let mut used = if m.is_dir() {
        m.mtime()
    } else {
        m.atime().max(m.mtime())
    };
    if m.is_dir() && m.dev() == dev {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                let (cb, cu) = tree_usage(&e.path(), dev);
                b += cb;
                used = used.max(cu);
            }
        }
    }
    (b, used)
}

pub fn docker_reclaimable(unused_images: bool) -> Option<(u64, String)> {
    let o = run(
        &[
            "docker",
            "system",
            "df",
            "--format",
            "{{.Type}}|{{.Reclaimable}}",
        ],
        None,
        Duration::from_secs(60),
    )?;
    if !o.ok {
        return None;
    }
    let mut total = 0;
    let mut parts = Vec::new();
    for l in o.stdout.lines() {
        let (t, r) = l.split_once('|')?;
        let b = plan::parse_size(r.split(' ').next().unwrap_or("0"));
        if t == "Build Cache" || (t == "Images" && unused_images) {
            total += b;
            parts.push(format!("{t} {}", human(b)));
        }
    }
    Some((
        total,
        format!(
            "reclaimable (docker's estimate, incl. recent): {}",
            parts.join(", ")
        ),
    ))
}

fn docker_clean(p: &plan::Policy) -> bool {
    let t = Duration::from_secs(3600);
    let until = format!("until={}", p.docker_until);
    let mut ok = run(
        &["docker", "builder", "prune", "-f", "--filter", &until],
        None,
        t,
    )
    .map(|o| o.ok)
    .unwrap_or(false);
    ok &= run(&["docker", "image", "prune", "-f"], None, t)
        .map(|o| o.ok)
        .unwrap_or(false);
    if p.docker_unused_images {
        ok &= run(
            &[
                "docker",
                "image",
                "prune",
                "-a",
                "-f",
                "--filter",
                "until=168h",
            ],
            None,
            t,
        )
        .map(|o| o.ok)
        .unwrap_or(false);
    }
    ok
}

fn compress(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let t = Duration::from_secs(7200);
    if which("zstd").is_some() {
        run(&["zstd", "-q", "-T0", "--rm", "--", &s], None, t)
            .map(|o| o.ok)
            .unwrap_or(false)
    } else {
        run(&["gzip", "--", &s], None, t)
            .map(|o| o.ok)
            .unwrap_or(false)
    }
}

/// Order of escalation: lossless/regenerable first, whole worktrees last.
const ORDER: &[&str] = &["docker", "log", "build-output", "cache", "worktree"];

pub struct ApplyOpts<'a> {
    pub apply: bool,
    pub categories: &'a [String],
    /// Stop once this much is free (auto mode).
    pub until_avail: Option<u64>,
}

/// Returns bytes freed (measured by statvfs when applying).
pub fn apply(plan: &Plan, o: &ApplyOpts) -> u64 {
    let home = Path::new(&plan.home).to_path_buf();
    let mounts = mounts::list();
    let inuse = InUse::snapshot();
    let refs = Refs::collect(&home);
    let cx = Ctx {
        home: &home,
        inuse: &inuse,
        refs: &refs,
        policy: &plan.policy,
        mounts: &mounts,
    };
    let avail = || fs_space(&home).map(|s| s.1).unwrap_or(0);
    let start_avail = avail();
    let mut est = 0u64;
    let verb = if o.apply { "" } else { "would " };
    for cat in ORDER {
        if !o.categories.is_empty() && !o.categories.iter().any(|c| c == cat) {
            continue;
        }
        for it in plan.items.iter().filter(|i| i.ok && i.cat == *cat) {
            if let Some(t) = o.until_avail {
                if o.apply && avail() >= t {
                    println!("target reached: {} free", human(avail()));
                    return avail().saturating_sub(start_avail);
                }
            }
            if let Err(why) = plan::check(it, &cx) {
                println!("  skip    {:<12} {}  ({why})", it.cat, it.path);
                log(it, "skip", &why, 0);
                continue;
            }
            let (action, done) = match it.cat.as_str() {
                "build-output" => (
                    "delete",
                    !o.apply || remove_tree(Path::new(&it.path)).is_ok(),
                ),
                "log" => ("compress", !o.apply || compress(Path::new(&it.path))),
                "worktree" => (
                    "remove-worktree",
                    !o.apply || remove_wt(Path::new(&it.path)),
                ),
                "docker" => ("docker-prune", !o.apply || docker_clean(&plan.policy)),
                "cache" => {
                    if o.apply {
                        let r = cache_pass(it, &cx, true);
                        println!("  pruned  cache        {}  ({})", it.path, human(r.bytes));
                        log(it, "prune-cache", "", r.bytes);
                        est += r.bytes;
                        continue;
                    }
                    ("prune-cache", true)
                }
                _ => continue,
            };
            println!(
                "  {verb}{action:<8} {:<12} {}  ({})",
                it.cat,
                it.path,
                human(it.bytes)
            );
            if o.apply {
                log(
                    it,
                    action,
                    if done { "" } else { "FAILED" },
                    if done { it.bytes } else { 0 },
                );
            }
            if done {
                est += it.bytes;
            }
        }
    }
    if o.apply {
        avail().saturating_sub(start_avail)
    } else {
        est
    }
}

fn remove_wt(p: &Path) -> bool {
    // Sealed (read-only) trees inside make `git worktree remove` fail: unseal first.
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let _ = unseal(&e.path());
        }
    }
    matches!(git::main_checkout(p), Some(main) if git::remove_worktree(&main, p))
}

fn unseal(p: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(p)?;
    if m.is_dir() {
        ensure_owner_rwx(p)?;
        for e in fs::read_dir(p)?.flatten() {
            unseal(&e.path())?;
        }
    }
    Ok(())
}

fn log(it: &Item, action: &str, note: &str, bytes: u64) {
    let line = serde_json::json!({
        "t": now(), "host": util::hostname(), "action": action, "cat": it.cat,
        "path": it.path, "bytes": bytes, "note": note,
    });
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(util::state_dir().join("actions.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_tree_handles_read_only_trees() {
        let d = std::env::temp_dir().join(format!("diskreap-rm-{}", std::process::id()));
        let sealed = d.join("rel/hub/node_modules/x");
        fs::create_dir_all(&sealed).unwrap();
        fs::write(sealed.join("f.js"), "x").unwrap();
        for p in [
            sealed.as_path(),
            sealed.parent().unwrap(),
            &d.join("rel/hub"),
            &d.join("rel"),
        ] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o555)).unwrap();
        }
        remove_tree(&d.join("rel/hub/node_modules")).unwrap();
        assert!(!d.join("rel/hub/node_modules").exists());
        assert!(d.join("rel/hub").exists());
        fs::set_permissions(d.join("rel"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(d.join("rel/hub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }
}
