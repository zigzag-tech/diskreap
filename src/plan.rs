//! Stage 1: scan → plan. Read-only. Every candidate gets a verdict (ok / skip
//! with the reason) using the same `check` that stage 2 re-runs right before it acts.

use crate::act;
use crate::git;
use crate::mounts::{self, Mount};
use crate::procs::{InUse, Owners, Refs};
use crate::rules::{is_log, Rules, Strat, CACHES, REPORT_ONLY};
use crate::util::{self, fs_space, now, which, DAY, GB};
use crate::walk::{self, Opts};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// Keep-above policy (0.6): a disk is `low` when less than min(15%, 100 GiB) is free, and
/// once `auto` acts it works until 15% is free again. Thresholds are a share of the
/// filesystem; their lower bound is absolute, capped at a share of the filesystem, or a
/// 15 GB tmpfs would read "low" at 56% free. Only the trigger is capped (the operator's
/// rule, 2026-11-14: a big disk with 100 GiB free is not under pressure). The stop target
/// is not: with both capped at 100 GiB a 1.8 TiB disk sat at 3% free for weeks without
/// `auto` doing useful recovery (zz-tower0, 2026-11), so once triggered it climbs back to
/// 15%. `critical` keeps its 30 GiB cap: it is the emergency band (shortest horizons, the
/// needs-supervision marker), not the trigger to act.
fn clamp_share(total: u64, pct: u64, floor: u64, floor_cap_pct: u64, hi: u64) -> u64 {
    let lo = floor.min(total * floor_cap_pct / 100);
    (total * pct / 100).clamp(lo, hi.max(lo))
}

/// Free-space share `auto` restores, and below which a disk is `low` (up to LOW_CAP).
pub const KEEP_FREE_PCT: u64 = 15;
/// `low` never needs more free than this, however big the disk; the stop target has no cap.
pub const LOW_CAP: u64 = 100 * GB;

/// Free space below which a disk is `low`: min(15%, 100 GiB), at least min(10 GiB, 30%).
pub fn low_avail(total: u64) -> u64 {
    clamp_share(total, KEEP_FREE_PCT, 10 * GB, 30, LOW_CAP)
}

/// Low: free < low_avail.
/// Critical: free < 5%, at least min(3 GiB, 10%), at most 30 GiB.
pub fn level_of(total: u64, avail: u64) -> Level {
    if avail < clamp_share(total, 5, 3 * GB, 10, 30 * GB) {
        Level::Critical
    } else if avail < low_avail(total) {
        Level::Low
    } else {
        Level::Ok
    }
}

/// `auto` stops once free space is back above this: 15%, at least min(15 GiB, 40%), no cap.
pub fn target_avail(total: u64) -> u64 {
    clamp_share(total, KEEP_FREE_PCT, 15 * GB, 40, u64::MAX)
}

/// The operator's free-space floor (`util::min_free_bytes`), capped at half the filesystem
/// so a typo cannot make a small disk permanently "low".
fn effective_floor(total: u64, floor: Option<u64>) -> Option<u64> {
    floor.map(|f| f.min(total / 2))
}

/// `level_of` for `$HOME`'s filesystem, with the operator floor: free space below it is at
/// least Low. It never lowers a level and never creates Critical.
pub fn level_with_floor(total: u64, avail: u64, floor: Option<u64>) -> Level {
    let base = level_of(total, avail);
    match effective_floor(total, floor) {
        Some(f) if base == Level::Ok && avail < f => Level::Low,
        _ => base,
    }
}

/// `auto` target for `$HOME`'s filesystem: the built-in target, or 25% above the floor.
pub fn target_with_floor(total: u64, floor: Option<u64>) -> u64 {
    let t = target_avail(total);
    match effective_floor(total, floor) {
        Some(f) => t.max(f + f / 4),
        None => t,
    }
}

pub fn home_level(total: u64, avail: u64) -> Level {
    level_with_floor(total, avail, util::min_free_bytes())
}

/// Free space on `$HOME`'s filesystem below which it is `low`, operator floor included.
pub fn home_low(total: u64) -> u64 {
    let l = low_avail(total);
    effective_floor(total, util::min_free_bytes()).map_or(l, |f| l.max(f))
}

pub fn home_target(total: u64) -> u64 {
    target_with_floor(total, util::min_free_bytes())
}

/// The fast merged-worktree rule never goes below this: a tree touched in the last half hour is live.
pub const MIN_MERGED_IDLE_MINUTES: i64 = 30;

/// Temp entries are judged at most this deep below their temp root: deep enough for the
/// entries of a Claude Code session, `/tmp/claude-<uid>/<project>/<session>/<entry>`.
pub const TMP_MAX_DEPTH: usize = 4;
/// An unknown `~/.cache/<app>` goes (under pressure) when nothing in it was used for this long.
pub const UNKNOWN_CACHE_IDLE_DAYS: i64 = 3;
/// Backups of one series that are never auto-removed.
pub const BACKUPS_KEPT: usize = 3;
/// The reason every under-pressure rule gives when the level is ok.
pub const PRESSURE_ONLY: &str = "only under disk pressure (level low/critical)";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Policy {
    pub build_idle_days: i64,
    pub worktree_idle_days: i64,
    /// Operator fast rule (`worktree_merged_idle_minutes`): a CLEAN worktree whose HEAD is a strict
    /// ancestor of origin's default branch is removable after this many minutes idle instead of
    /// `worktree_idle_days`. None = off.
    #[serde(default)]
    pub worktree_merged_idle_minutes: Option<i64>,
    /// Globs on the worktree's branch name (`protect_worktree_branches`): never removed.
    #[serde(default)]
    pub protect_worktree_branches: Vec<String>,
    /// Extra `origin/<branch>` refs that also count as "merged" (`worktree_integration_refs`), for
    /// repos that land on an integration branch (e.g. `origin/dev`) and move the default branch
    /// only on release. A worktree merged into ANY of them (or the default branch) qualifies.
    #[serde(default)]
    pub worktree_integration_refs: Vec<String>,
    pub log_min_bytes: u64,
    pub log_idle_hours: i64,
    /// Percent of each cache's normal age horizon.
    pub cache_age_pct: i64,
    /// Stale user-owned entries directly under /tmp, /var/tmp, $TMPDIR.
    pub tmp_idle_days: i64,
    pub docker_until: String,
    pub docker_unused_images: bool,
    /// Level low/critical: unknown app caches, old backups and offloaded data may go too.
    /// Absent in an older plan = false.
    #[serde(default)]
    pub pressure_rules: bool,
}

impl Policy {
    pub fn for_level(l: Level) -> Policy {
        match l {
            Level::Critical => Policy {
                build_idle_days: 1,
                worktree_idle_days: 1,
                worktree_merged_idle_minutes: None,
                protect_worktree_branches: vec![],
                worktree_integration_refs: vec![],
                log_min_bytes: 100 << 20,
                log_idle_hours: 6,
                cache_age_pct: 25,
                tmp_idle_days: 2,
                docker_until: "1h".into(),
                docker_unused_images: true,
                pressure_rules: true,
            },
            // Daily maintenance while space is fine: only what is clearly stale.
            Level::Ok => Policy {
                build_idle_days: 14,
                worktree_idle_days: 7,
                worktree_merged_idle_minutes: None,
                protect_worktree_branches: vec![],
                worktree_integration_refs: vec![],
                log_min_bytes: 1 << 30,
                log_idle_hours: 7 * 24,
                cache_age_pct: 200,
                tmp_idle_days: 14,
                docker_until: "168h".into(),
                docker_unused_images: false,
                pressure_rules: false,
            },
            Level::Low => Policy {
                build_idle_days: 3,
                worktree_idle_days: 3,
                worktree_merged_idle_minutes: None,
                protect_worktree_branches: vec![],
                worktree_integration_refs: vec![],
                log_min_bytes: 500 << 20,
                log_idle_hours: 24,
                cache_age_pct: 100,
                tmp_idle_days: 7,
                docker_until: "24h".into(),
                docker_unused_images: false,
                pressure_rules: true,
            },
        }
    }

    /// Fill the operator-config fields (`worktree_merged_idle_minutes`,
    /// `protect_worktree_branches`). Malformed values are a loud exit 2, like every other key.
    pub fn with_operator_config(mut self) -> Policy {
        let Some(cfg) = util::config_json() else { return self };
        let bad = |what: &str| -> ! {
            eprintln!("diskreap: config {what}");
            std::process::exit(2)
        };
        self.worktree_merged_idle_minutes = None;
        if let Some(v) = cfg.get("worktree_merged_idle_minutes") {
            match v.as_i64() {
                Some(m) if m >= MIN_MERGED_IDLE_MINUTES => self.worktree_merged_idle_minutes = Some(m),
                _ => bad(&format!(
                    "worktree_merged_idle_minutes must be an integer >= {MIN_MERGED_IDLE_MINUTES} (minutes), got {v}"
                )),
            }
        }
        self.protect_worktree_branches.clear();
        if let Some(v) = cfg.get("protect_worktree_branches") {
            let Some(arr) = v.as_array() else { bad("protect_worktree_branches must be an array of glob strings") };
            for g in arr {
                let Some(g) = g.as_str() else { bad("protect_worktree_branches must be an array of glob strings") };
                if glob::Pattern::new(g).is_err() {
                    bad(&format!("protect_worktree_branches: bad glob {g:?}"));
                }
                self.protect_worktree_branches.push(g.to_string());
            }
        }
        self.worktree_integration_refs.clear();
        if let Some(v) = cfg.get("worktree_integration_refs") {
            let Some(arr) = v.as_array() else { bad("worktree_integration_refs must be an array of origin/<branch> refs") };
            for r in arr {
                match r.as_str() {
                    Some(r) if r.starts_with("origin/") && r.len() > 7 => self.worktree_integration_refs.push(r.to_string()),
                    _ => bad(&format!("worktree_integration_refs: {r} is not an origin/<branch> ref")),
                }
            }
        }
        self
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

/// A worktree diskreap will not touch because it holds work: uncommitted changes or unmerged commits.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewWt {
    pub path: String,
    pub bytes: u64,
    pub newest: i64,
    pub branch: String,
    /// "dirty" | "unmerged" (unmerged wins when both hold).
    pub kind: String,
    /// e.g. "12 uncommitted" or "3 commits not in origin/main, 5 uncommitted".
    pub state: String,
    /// Best-effort holder: process cwd / tmux pane / git lock reason, or "-".
    pub owner: String,
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
    /// Dirty or unmerged worktrees (scan only): never deleted, listed so a human decides.
    #[serde(default)]
    pub review: Vec<ReviewWt>,
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
            let (root, depth) = tmp_place(p).ok_or("not inside a temp dir")?;
            if depth > TMP_MAX_DEPTH {
                return Err(format!("more than {TMP_MAX_DEPTH} levels below {}", root.display()));
            }
            if !crate::platform::owned_by_me(&md) {
                return Err("owned by another user".into());
            }
            // Below the top level we got here by splitting a kept directory: every directory
            // in between must still be our own, and nobody's workspace.
            for a in p.ancestors().skip(1).take(depth - 1) {
                match std::fs::symlink_metadata(a) {
                    Ok(m) if m.is_dir() && crate::platform::owned_by_me(&m) => {}
                    _ => return Err(format!("{} is not our own directory", a.display())),
                }
                if cx.inuse.holds(a) {
                    return Err(format!("inside {}, which a process works in", a.display()));
                }
            }
            if let Some(h) = cx.inuse.under(p) {
                return Err(format!("in use ({})", h.display()));
            }
            let st = act::tree_stats(p);
            if st.special {
                return Err("holds a socket or FIFO (a live session's rendezvous)".into());
            }
            if st.keep || keep_marked(p, &root) {
                return Err("protected by .diskreap-keep".into());
            }
            // A split entry's siblings were in use or recently written: an input of that work
            // (extracted with old mtimes, `cp -a`) that was READ recently is part of it too.
            idle(t, if depth > 1 { st.used } else { st.newest }, cx.tmp_idle_days)
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
            if let Some(b) = git::branch(p) {
                if let Some(g) = cx
                    .policy
                    .protect_worktree_branches
                    .iter()
                    .find(|g| glob::Pattern::new(g).is_ok_and(|pt| pt.matches(&b)))
                {
                    return Err(format!("branch {b} protected by protect_worktree_branches {g}"));
                }
            }
            // Cheapest decisive checks first: on a busy host most worktrees are
            // active, and `git status` over 300 of them took 226 s (xc-tower-ubuntu).
            let last = worktree_last_active(p, it.newest);
            let (window, fast) = worktree_window(p, cx.policy);
            if t - last < window {
                return Err(format!("active {} ago (< {})", util::ago(last), human_window(window)));
            }
            let base = git::base_ref(p).ok_or("no main/master/origin HEAD to compare with")?;
            if fast && t - last < worktree_slow_window(p, cx.policy) {
                // The fast rule leans on the remote's word, not on a local branch that may
                // hold unpushed merges: origin's already-fetched default branch, strict ancestry.
                if !base.starts_with("origin/") {
                    return Err(format!("fast rule needs an origin/ default branch ref; only {base} exists"));
                }
                if git::landed_ref(p, &base, &cx.policy.worktree_integration_refs, true).is_none() {
                    return Err(format!("HEAD is not an ancestor of {base} (fast rule needs a plain merge)"));
                }
            }
            if git::landed_ref(p, &base, &cx.policy.worktree_integration_refs, false).is_none() {
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
            if c.strat == "whole" {
                return unknown_cache(p, c.days, cx);
            }
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
        "backup" => old_backup(p, cx),
        "offloaded" => offloaded(p, cx),
        _ => Err("report only".into()),
    }
}

fn pressure(p: &Policy) -> Result<(), String> {
    if p.pressure_rules {
        Ok(())
    } else {
        Err(PRESSURE_ONLY.into())
    }
}

/// A `.diskreap-keep` in `p` or any directory above it, up to and including `stop`.
/// (The walker honors it for walked categories; these are found without the walker.)
fn keep_marked(p: &Path, stop: &Path) -> bool {
    p.ancestors()
        .take_while(|a| a.starts_with(stop))
        .any(|a| a.join(".diskreap-keep").is_file())
}

/// The consumer-side gates every under-pressure rule shares: no process holds or names
/// anything inside, no service/config/PATH entry references it, no `.diskreap-keep`.
fn unused(p: &Path, st: &act::TreeStats, cx: &Ctx) -> Result<(), String> {
    if let Some(h) = cx.inuse.under(p) {
        return Err(format!("in use ({})", h.display()));
    }
    if let Some(src) = cx.refs.within(p) {
        return Err(format!("referenced by {src}"));
    }
    if st.special {
        return Err("holds a socket or FIFO".into());
    }
    if st.keep || keep_marked(p, cx.home) {
        return Err("protected by .diskreap-keep".into());
    }
    Ok(())
}

/// `~/.cache/<app>` with no row in CACHES. XDG makes `~/.cache` non-essential data, but only
/// the app knows its invariants (a live content store pins files by name), so it is judged as
/// ONE unit: the newest use of anything inside, never file by file. Pressure is checked last
/// so a live one still says why (scan reports it).
fn unknown_cache(p: &Path, days: i64, cx: &Ctx) -> Result<(), String> {
    if p.parent() != Some(cx.home.join(".cache").as_path()) {
        return Err("not a ~/.cache/<app> directory".into());
    }
    let md = std::fs::symlink_metadata(p).map_err(|_| "gone")?;
    if !md.is_dir() {
        return Err("not a directory".into());
    }
    if !mounts::below(cx.mounts, p).is_empty() {
        return Err("holds a mount".into());
    }
    let st = act::tree_stats(p);
    unused(p, &st, cx)?;
    idle(now(), st.used, days)?;
    pressure(cx.policy)
}

fn is_backup_dir(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().contains("backup"))
}

/// A backup's series: its name with each date/time/counter stamp replaced by `#`; None = no
/// stamp to rotate by. A stamp is a run of digits and `-_.:` (and an ISO `T` between digits)
/// holding at least 4 digits and touching no letter: `d895982f` is a hash, a name, not a date
/// (a content-addressed store in a backup dir must never be "rotated"). A short number glued
/// to a word stays in the name: `db1` and `db2` are two databases, not two dates of one.
pub fn backup_series(name: &str) -> Option<String> {
    let c: Vec<char> = name.chars().collect();
    let in_run = |i: usize| {
        c[i].is_ascii_digit()
            || "-_.:".contains(c[i])
            || (c[i] == 'T' && i > 0 && i + 1 < c.len() && c[i - 1].is_ascii_digit() && c[i + 1].is_ascii_digit())
    };
    let (mut out, mut stamped, mut i) = (String::new(), false, 0);
    while i < c.len() {
        if !in_run(i) {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < c.len() && in_run(i) {
            i += 1;
        }
        let run: String = c[start..i].iter().collect();
        let glued = start > 0 && c[start - 1].is_alphanumeric();
        let lead = if glued { run.chars().take_while(|x| x.is_ascii_digit()).count() } else { 0 };
        let rest = &run[lead..];
        let digits = rest.chars().filter(|x| x.is_ascii_digit()).count();
        let touches_end = i < c.len() && rest.ends_with(|x: char| x.is_ascii_digit());
        if lead < 4 && digits >= 4 && !touches_end {
            out.push_str(&run[..lead]);
            out.push('#');
            stamped = true;
        } else {
            out.push_str(&run);
        }
    }
    stamped.then_some(out)
}

/// Why a *backup* dir's contents are not ours to rotate, if they are not: a repository's
/// files are git's business, and an app's own backups (under a dot-dir or an install root
/// such as `Library/Application Support`) are rotated by the app.
fn backup_unmanaged(r: &Path, home: &Path) -> Option<String> {
    if let Some(top) = git::toplevel(r) {
        return Some(format!("backup dir inside a git repository ({})", top.display()));
    }
    let rel = r.strip_prefix(home).ok()?;
    if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
        return Some("an app's own backup dir (under a dot-dir): the app rotates it".into());
    }
    under_install_root(r, home).map(|ir| format!("an app's own backup dir (under {ir}): the app rotates it"))
}

/// A dated backup that is not one of the newest BACKUPS_KEPT of its series (same directory,
/// same name once stamps are stripped). Only a sibling at least half its size counts as
/// newer: a broken job writing stubs every night must not push the real backups out.
fn old_backup(p: &Path, cx: &Ctx) -> Result<(), String> {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let series = backup_series(&name).ok_or("no date stamp to rotate by")?;
    let root = p
        .ancestors()
        .skip(1)
        .take(3)
        .find(|a| is_backup_dir(a))
        .ok_or("not inside a *backup* directory")?;
    if let Some(why) = backup_unmanaged(root, cx.home) {
        return Err(why);
    }
    let md = std::fs::symlink_metadata(p).map_err(|_| "gone")?;
    if md.file_type().is_symlink() {
        return Err("a symlink".into());
    }
    let st = act::tree_stats(p);
    let parent = p.parent().ok_or("no parent")?;
    let newer = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path() != p && backup_series(&e.file_name().to_string_lossy()).as_deref() == Some(series.as_str()))
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
        .map(|e| (e.path(), act::tree_stats(&e.path())))
        .filter(|(q, s)| s.bytes >= st.bytes / 2 && (s.newest, q.as_path()) > (st.newest, p))
        .count();
    if newer < BACKUPS_KEPT {
        return Err(format!("one of the newest {BACKUPS_KEPT} full-size backups of series {series}"));
    }
    unused(p, &st, cx)?;
    // A backup written within the day may belong to a run still in progress.
    idle(now(), st.newest, 1)?;
    pressure(cx.policy)
}

/// One line of `offloaded.jsonl`: someone copied `path` off-host to `remote` and verified
/// the copy at `verified_at`, when the tree held `bytes` (sum of st_size of its
/// non-directory entries) and its newest mtime was `mtime`.
#[derive(Deserialize, Clone, Debug)]
pub struct Offloaded {
    pub path: String,
    pub bytes: u64,
    pub mtime: i64,
    pub remote: String,
    pub verified_at: i64,
}

/// The offload ledger, last record per path winning. Malformed lines are ignored (with a
/// warning when `warn`): another tool writes this file, and a bad line must not stop a guard.
pub fn offload_ledger(warn: bool) -> Vec<Offloaded> {
    let Ok(text) = std::fs::read_to_string(util::state_dir().join("offloaded.jsonl")) else {
        return Vec::new();
    };
    let mut v: Vec<Offloaded> = Vec::new();
    for (i, l) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        match serde_json::from_str::<Offloaded>(l) {
            Ok(r) if !r.remote.trim().is_empty() && r.verified_at > 0 => {
                v.retain(|o| o.path != r.path);
                v.push(r);
            }
            Ok(_) if warn => eprintln!("diskreap: offloaded.jsonl line {}: needs a remote and verified_at; ignored", i + 1),
            Err(e) if warn => eprintln!("diskreap: offloaded.jsonl line {}: {e}; ignored", i + 1),
            _ => {}
        }
    }
    v
}

/// Data the ledger says is safely off-host, still exactly as it was when copied.
fn offloaded(p: &Path, cx: &Ctx) -> Result<(), String> {
    let rec = offload_ledger(false)
        .into_iter()
        .find(|r| util::expand_tilde(&r.path) == p)
        .ok_or("no longer in the offload ledger")?;
    let md = std::fs::symlink_metadata(p).map_err(|_| "gone")?;
    if md.file_type().is_symlink() {
        return Err("a symlink".into());
    }
    if md.is_dir() && p.join(".git").exists() {
        return Err("is a git repository/worktree: remove it with git".into());
    }
    let st = act::tree_stats(p);
    if st.apparent != rec.bytes || st.newest != rec.mtime {
        return Err(format!(
            "changed since it was offloaded (now {} bytes, mtime {}; ledger {} / {})",
            st.apparent, st.newest, rec.bytes, rec.mtime
        ));
    }
    unused(p, &st, cx)?;
    pressure(cx.policy)
}

/// (temp root, depth below it) for the closest temp root holding `p`.
fn tmp_place(p: &Path) -> Option<(PathBuf, usize)> {
    tmp_roots()
        .into_iter()
        .filter(|r| p.starts_with(r) && p != r)
        .map(|r| {
            let d = p.strip_prefix(&r).map(|x| x.components().count()).unwrap_or(0);
            (r, d)
        })
        .min_by_key(|x| x.1)
}

/// A kept temp directory is split into its entries when what keeps it is something inside
/// it (an open file, a named path, a recent write) — not the directory itself being a
/// process's cwd, and not a socket (a live session's rendezvous keeps the whole entry).
fn tmp_split(it: &Item, cx: &Ctx) -> bool {
    let p = Path::new(&it.path);
    let Some((_, depth)) = tmp_place(p) else { return false };
    let own_dir = std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && crate::platform::owned_by_me(&m));
    own_dir
        && depth < TMP_MAX_DEPTH
        && (it.why.starts_with("in use") || it.why.starts_with("active"))
        && !cx.inuse.holds(p)
        && !act::tree_stats(p).special
}

/// Newest sign of work in a worktree, cheaply: the walk's newest mtime, git's own
/// metadata (index/HEAD/reflog) and the top-level entries' mtimes.
fn worktree_last_active(p: &Path, walked_newest: i64) -> i64 {
    let top_mtime = std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| util::mtime_of(&e.path())).max().unwrap_or(0))
        .unwrap_or(0);
    walked_newest.max(git::activity(p)).max(top_mtime)
}

/// Detached HEAD is often a deliberate pin (referenced by path elsewhere): wait much longer.
fn worktree_slow_window(p: &Path, policy: &Policy) -> i64 {
    let days = if git::detached(p) {
        policy.worktree_idle_days.max(30)
    } else {
        policy.worktree_idle_days
    };
    days * DAY
}

/// (idle seconds required, whether the operator's fast rule set it).
fn worktree_window(p: &Path, policy: &Policy) -> (i64, bool) {
    let slow = worktree_slow_window(p, policy);
    match policy.worktree_merged_idle_minutes {
        Some(m) if m * 60 < slow && !git::detached(p) => (m * 60, true),
        _ => (slow, false),
    }
}

fn human_window(secs: i64) -> String {
    if secs % DAY == 0 {
        format!("{}d", secs / DAY)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// Why an OK worktree will be removed, for the dry-run line.
fn worktree_reason(it: &Item, cx: &Ctx) -> String {
    let p = Path::new(&it.path);
    let last = worktree_last_active(p, it.newest);
    let base0 = git::base_ref(p).unwrap_or_default();
    let base = git::landed_ref(p, &base0, &cx.policy.worktree_integration_refs, false).unwrap_or(base0);
    let (window, fast) = worktree_window(p, cx.policy);
    let branch = git::branch(p).map(|b| format!("branch {b} kept, ")).unwrap_or_default();
    format!(
        "clean, merged into {base}, idle {} (needs {}{}); {branch}no process inside",
        util::ago(last),
        human_window(window),
        if fast { ", worktree_merged_idle_minutes" } else { "" },
    )
}

fn idle(t: i64, last: i64, days: i64) -> Result<(), String> {
    if t - last < days * DAY {
        Err(format!("active {} ago (< {days}d)", util::ago(last)))
    } else {
        Ok(())
    }
}

/// System and per-user temp dirs (on macOS $TMPDIR is a per-user /var/folders dir).
/// Test hook: `DISKREAP_TMP_ROOTS` (`:`-separated) replaces them, so the e2e suite never
/// judges, let alone cleans, the real /tmp of the machine it runs on.
pub fn tmp_roots() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::env::var_os("DISKREAP_TMP_ROOTS") {
        Some(r) => std::env::split_paths(&r).collect(),
        None => vec!["/tmp".into(), "/var/tmp".into(), std::env::temp_dir()],
    };
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
    let home = util::home();
    for p in std::iter::once(home.clone()).chain(tmp_roots()) {
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
            // The operator floor describes $HOME's filesystem only, not a tmpfs.
            level: if p == home { home_level(t, a) } else { level_of(t, a) },
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
    /// Also classify (and size) the worktrees that hold work. Costs a `git status` per
    /// worktree, so `scan` asks for it and the hourly `auto` does not.
    pub review: bool,
}

/// Classify the skipped worktrees that hold work. Reasons already name dirty/unmerged
/// for the ones that got far enough; the rest (active, in use, locked…) are probed.
fn review_worktrees(wts: &[Item], mounts: &[Mount]) -> Vec<ReviewWt> {
    let owners = Owners::snapshot();
    let cands: Vec<&Item> = wts
        .iter()
        .filter(|i| !i.ok)
        .filter(|i| !i.why.starts_with("referenced by") && !i.why.starts_with("branch ") && !i.why.starts_with("orphaned"))
        .collect();
    let out = std::sync::Mutex::new(Vec::new());
    let n = cands.len().div_ceil(8).max(1);
    std::thread::scope(|s| {
        for chunk in cands.chunks(n) {
            let (owners, out) = (&owners, &out);
            s.spawn(move || {
                for it in chunk {
                    let p = Path::new(&it.path);
                    let Some(base) = git::base_ref(p) else { continue };
                    let ahead = if it.why.starts_with("has commits not in") || !git::merged_into(p, &base) {
                        Some(git::commits_not_in(p, &base))
                    } else {
                        None
                    };
                    let dirty = match it.why.split_once(' ') {
                        Some((n, rest)) if rest.starts_with("uncommitted/untracked") => n.parse().ok(),
                        _ => git::dirty_count(p),
                    }
                    .unwrap_or(0);
                    if ahead.is_none() && dirty == 0 {
                        continue;
                    }
                    let mut parts = Vec::new();
                    if let Some(a) = ahead {
                        parts.push(format!("{a} commit(s) not in {base}"));
                    }
                    if dirty > 0 {
                        parts.push(format!("{dirty} uncommitted"));
                    }
                    let gitdir = git::linked_worktree_gitdir(p);
                    out.lock().unwrap().push(ReviewWt {
                        path: it.path.clone(),
                        bytes: act::du(p, mounts),
                        newest: worktree_last_active(p, it.newest),
                        branch: git::branch(p).unwrap_or_else(|| "(detached)".into()),
                        kind: if ahead.is_some() { "unmerged" } else { "dirty" }.into(),
                        state: parts.join(", "),
                        owner: owners.of(p, gitdir.as_deref()),
                    });
                }
            });
        }
    });
    let mut v = out.into_inner().unwrap();
    v.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    v
}

pub fn scan(o: &ScanOpts) -> Plan {
    let started = Instant::now();
    let home = util::home();
    let root = o.root.clone().unwrap_or_else(|| home.clone());
    let policy = Policy::for_level(o.level).with_operator_config();
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
        it.why = worktree_reason(it, &cx);
        let sized = w
            .found
            .iter()
            .any(|f| f.sized && f.path == Path::new(&it.path));
        if !sized {
            it.bytes = act::du(Path::new(&it.path), &mounts); // quick scan sizes eligible ones only
        }
    }
    let review = if o.review { review_worktrees(&wts, &mounts) } else { Vec::new() };
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

    // Unknown app caches: ~/.cache/<app> with no row in CACHES, one unit each.
    let known: Vec<&str> = CACHES
        .iter()
        .filter_map(|c| c.rel.strip_prefix(".cache/"))
        .filter_map(|r| r.split('/').next())
        .collect();
    for e in std::fs::read_dir(home.join(".cache")).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if known.contains(&name.as_str())
            || !e.file_type().is_ok_and(|t| t.is_dir())
            || !mounts::below(&mounts, &p).is_empty()
        {
            continue;
        }
        let st = act::tree_stats(&p);
        if st.bytes < 10 << 20 {
            continue;
        }
        let mut it = item("cache", &p, st.bytes, st.used);
        it.cache = Some(CacheRef {
            strat: "whole".into(),
            depth: 0,
            days: UNKNOWN_CACHE_IDLE_DAYS,
            cmd: vec![],
        });
        verdict(&mut it, &cx);
        if it.ok {
            it.why = format!("unknown app cache, whole dir unused > {UNKNOWN_CACHE_IDLE_DAYS}d (under pressure only)");
        } else if it.bytes >= GB && it.why != PRESSURE_ONLY {
            it.cat = "report".into();
            it.cache = None;
            it.why = format!(
                "unknown app cache, {} — not in diskreap's cache table; only the app knows what it can lose",
                it.why
            );
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
    // Temp entries, top level first. One that is kept because of something inside it is
    // split and its entries judged one by one, down to TMP_MAX_DEPTH: one open file deep in
    // /tmp/claude-<uid> kept 8.7 GiB of dead sessions (zz-tower2, 2026-11).
    let roots = tmp_roots();
    let push_tmp = |v: &mut Vec<Item>, p: PathBuf| {
        let st = act::tree_stats(&p);
        if st.bytes >= 10 << 20 && !roots.contains(&p) {
            v.push(item("tmp", &p, st.bytes, st.newest));
        }
    };
    let mut layer: Vec<Item> = Vec::new();
    for root in &roots {
        for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
            push_tmp(&mut layer, e.path());
        }
    }
    while !layer.is_empty() {
        let mut judged = verdicts(layer, &cx);
        let mut next = Vec::new();
        for it in judged.iter_mut().filter(|i| !i.ok) {
            if tmp_split(it, &cx) {
                it.why += "; its entries are judged one by one";
                for e in std::fs::read_dir(&it.path).into_iter().flatten().flatten() {
                    push_tmp(&mut next, e.path());
                }
            }
        }
        items.extend(judged);
        layer = next;
    }

    // Backups: dated entries of a *backup* dir (and of its undated subdirectories, two
    // levels), grouped into series. All but the newest BACKUPS_KEPT of a series may go
    // under pressure; the newest, the undated, and backup dirs we do not rotate are reported.
    let backup_roots: Vec<&walk::Found> = w
        .found
        .iter()
        .filter(|f| f.kind == "backup")
        .filter(|f| !w.found.iter().any(|o| o.kind == "backup" && o.path != f.path && f.path.starts_with(&o.path)))
        .filter(|f| !ok_worktrees.iter().any(|wt| f.path.starts_with(wt)))
        .collect();
    let mut old_backups: Vec<Item> = Vec::new();
    for f in backup_roots {
        match backup_unmanaged(&f.path, &home) {
            Some(why) => {
                let mut it = item("report", &f.path, f.bytes, f.newest);
                it.why = why;
                items.push(it);
            }
            None if backup_series_in(&f.path, 0, &mut old_backups, &mut items) => {}
            None => {
                let mut it = item("report", &f.path, f.bytes, f.newest);
                it.why = "backup dir with no dated backups to rotate — a human decides".into();
                items.push(it);
            }
        }
    }
    for mut it in verdicts(old_backups, &cx) {
        if !it.ok && it.why != PRESSURE_ONLY {
            it.cat = "report".into();
            it.why = format!("backup kept: {} — a human decides", it.why);
        } else if it.ok {
            it.why = format!("older than the newest {BACKUPS_KEPT} of its series, unused (under pressure only)");
        }
        items.push(it);
    }

    // Offloaded: the ledger's paths, still exactly as they were copied.
    for r in offload_ledger(true) {
        let p = util::expand_tilde(&r.path);
        if std::fs::symlink_metadata(&p).is_err() {
            continue;
        }
        let st = act::tree_stats(&p);
        let mut it = item("offloaded", &p, st.bytes, st.newest);
        verdict(&mut it, &cx);
        if it.ok {
            it.why = format!(
                "copied to {}, verified {} ago; size and mtime unchanged (under pressure only)",
                r.remote,
                util::ago(r.verified_at)
            );
        }
        items.push(it);
    }

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
        full_reports(&w, &refs, &home, &mounts, &ok_worktrees, &mut items);
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
        level: home_level(total, avail),
        policy,
        home: home.to_string_lossy().into_owned(),
        total,
        avail,
        scan_secs: started.elapsed().as_secs_f64(),
        phase_secs: phases,
        items,
        review,
        top,
        stalled: s(&w.stalled),
        skipped_mounts: s(&w.skipped_mounts),
        kept: s(&w.kept),
    }
}

/// One directory of a backup root: dated entries become series, undated subdirectories are
/// descended (per-host / per-database folders), other undated entries are reported. Returns
/// false when nothing at or below `d` is dated: the caller then reports `d` as ONE item.
fn backup_series_in(d: &Path, depth: usize, old: &mut Vec<Item>, items: &mut Vec<Item>) -> bool {
    let mut series: BTreeMap<String, Vec<(PathBuf, act::TreeStats)>> = BTreeMap::new();
    let (mut undated, mut dated_below) = (Vec::new(), false);
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        match backup_series(&e.file_name().to_string_lossy()) {
            Some(s) => series.entry(s).or_default().push((p.clone(), act::tree_stats(&p))),
            None if ft.is_dir() && depth < 2 && backup_series_in(&p, depth + 1, old, items) => dated_below = true,
            None => undated.push(p),
        }
    }
    if series.is_empty() && !dated_below {
        return false;
    }
    for p in undated {
        let st = act::tree_stats(&p);
        let mut it = item("report", &p, st.bytes, st.newest);
        it.why = "backup without a date stamp to rotate by — a human decides".into();
        items.push(it);
    }
    for (s, mut v) in series {
        v.sort_by(|a, b| (b.1.newest, &b.0).cmp(&(a.1.newest, &a.0)));
        let n = v.len();
        for (i, (p, st)) in v.into_iter().enumerate() {
            let mut it = item(if i < BACKUPS_KEPT { "report" } else { "backup" }, &p, st.bytes, st.newest);
            if i < BACKUPS_KEPT {
                it.why = format!("one of the newest {BACKUPS_KEPT} backups of series {s} ({n} in all) — a human decides");
                items.push(it);
            } else {
                old.push(it);
            }
        }
    }
    true
}

/// Full scan only: what no rule may remove but a human should see — protected paths,
/// git-ignored data in checkouts (a dataset, run logs), and directories another user owns.
/// Each ≥ 1 GiB, once (not inside something already reported).
fn full_reports(w: &walk::WalkResult, refs: &Refs, home: &Path, mounts: &[Mount], ok_worktrees: &[PathBuf], items: &mut Vec<Item>) {
    let covered = |items: &[Item], p: &Path| items.iter().any(|i| i.cat == "report" && p.starts_with(&i.path));
    let mut prot: Vec<(&Path, &str)> = refs.protected().filter(|(p, _)| p.starts_with(home) && p.exists()).collect();
    prot.sort();
    let mut seen: Vec<&Path> = Vec::new();
    for (p, src) in prot {
        if seen.iter().any(|s| p.starts_with(s)) {
            continue;
        }
        seen.push(p);
        let (b, newest) = act::du_stats(p, mounts);
        if b >= GB {
            items.retain(|i| !(i.cat == "report" && Path::new(&i.path) == p)); // "protected" says more than "cold"
            let mut it = item("report", p, b, newest);
            it.why = format!("protected ({src}) — never cleaned; a human decides");
            items.push(it);
        }
    }
    for f in w.found.iter().filter(|f| f.kind == "other-owner" && f.bytes >= GB) {
        if covered(items, &f.path) {
            continue;
        }
        let uid = std::fs::symlink_metadata(&f.path).map(|m| crate::platform::uid(&m)).unwrap_or(0);
        let mut it = item("report", &f.path, f.bytes, f.newest);
        it.why = format!(
            "owned by {}: diskreap cannot remove it; someone with the rights decides",
            if uid == 0 { "root".to_string() } else { format!("uid {uid}") }
        );
        items.push(it);
    }
    let tops = w.found.iter().filter(|f| {
        (f.kind == "repo" || f.kind == "worktree")
            && f.bytes >= GB
            && !ok_worktrees.contains(&f.path)
            && under_install_root(&f.path, home).is_none() // ~/.nvm's node versions are installed software
    });
    for top in tops {
        for p in git::ignored_entries(&top.path) {
            // Already judged elsewhere: inside a candidate or report, or holding another checkout.
            let judged = w.found.iter().any(|f| {
                f.path != top.path
                    && match f.kind {
                        "repo" | "worktree" => f.path.starts_with(&p),
                        _ => p.starts_with(&f.path),
                    }
            });
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if judged || covered(items, &p) || is_log(&name) {
                continue;
            }
            let (b, newest) = act::du_stats(&p, mounts);
            if b >= GB {
                let mut it = item("report", &p, b, newest);
                it.why = format!(
                    "git-ignored data in {}, modified {} ago — not build output; a human decides",
                    top.path.display(),
                    util::ago(newest)
                );
                items.push(it);
            }
        }
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
        let t = 1800 * GB; // big disk: low below min(15%, 100 GiB) = 100 GiB
        assert_eq!(level_of(t, 110 * GB), Level::Ok);
        assert_eq!(level_of(t, 99 * GB), Level::Low);
        assert_eq!(level_of(t, 55 * GB), Level::Low);
        assert_eq!(level_of(t, 29 * GB), Level::Critical);
        // ...but once triggered, auto climbs back to 15% = 270 GiB: the target has no cap
        // (zz-tower0 sat at 3% free for weeks with a capped target).
        assert_eq!(target_avail(t), 270 * GB);
        assert_eq!(low_avail(t), 100 * GB);
        // The cap takes over just above 666 GiB: 15% of 700 GiB is 105 GiB, capped to 100.
        assert_eq!(level_of(700 * GB, 99 * GB), Level::Low);
        assert_eq!(level_of(700 * GB, 101 * GB), Level::Ok);
        assert_eq!(target_avail(700 * GB), 105 * GB);
        assert_eq!(level_of(400 * GB, 59 * GB), Level::Low); // 15% = 60 GiB
        assert_eq!(level_of(400 * GB, 61 * GB), Level::Ok);
        assert_eq!(target_avail(400 * GB), 60 * GB);
        let small = 30 * GB; // small VM: 50% free is fine
        assert_eq!(level_of(small, 15 * GB), Level::Ok);
        assert_eq!(level_of(small, 8 * GB), Level::Low); // floor = min(10 GiB, 30%) = 9 GiB
        assert_eq!(level_of(small, 2 * GB), Level::Critical);
        assert_eq!(target_avail(small), 12 * GB); // min(15 GiB, 40%)
        let tmpfs = 15 * GB + GB / 2; // half-empty small tmpfs is fine; nearly full is not
        assert_eq!(level_of(tmpfs, 8 * GB), Level::Ok);
        assert_eq!(level_of(tmpfs, 3 * GB), Level::Low);
        assert_eq!(level_of(tmpfs, GB), Level::Critical);
    }

    #[test]
    fn operator_floor() {
        let t = 1800 * GB;
        // Default: 110 GiB free is Ok on a big disk (low is below 100 GiB).
        assert_eq!(level_with_floor(t, 110 * GB, None), Level::Ok);
        // Floor 120 GiB (xc-tower-ubuntu): the same disk is Low; the target stays 15% = 270
        // GiB because 125% of the floor (150 GiB) is below it.
        assert_eq!(level_with_floor(t, 110 * GB, Some(120 * GB)), Level::Low);
        assert_eq!(target_with_floor(t, Some(120 * GB)), 270 * GB);
        // A floor below the built-in trigger (40 GiB, zz-tower2) changes nothing on a big disk.
        assert_eq!(level_with_floor(t, 90 * GB, Some(40 * GB)), Level::Low);
        assert_eq!(level_with_floor(t, 110 * GB, Some(40 * GB)), Level::Ok);
        assert_eq!(target_with_floor(t, Some(40 * GB)), 270 * GB);
        // Floor 400 GiB: Low below it, and target rises to 500 GiB (floor + 25%).
        assert_eq!(level_with_floor(t, 370 * GB, Some(400 * GB)), Level::Low);
        assert_eq!(level_with_floor(t, 410 * GB, Some(400 * GB)), Level::Ok);
        assert_eq!(target_with_floor(t, Some(400 * GB)), 500 * GB);
        // Never lowers a level, never makes Critical.
        assert_eq!(level_with_floor(t, 29 * GB, Some(10 * GB)), Level::Critical);
        assert_eq!(level_with_floor(t, 50 * GB, Some(10 * GB)), Level::Low);
        assert_eq!(level_with_floor(t, 50 * GB, Some(500 * GB)), Level::Low);
        // A floor larger than half the disk is capped: a 100 GiB disk with 60 free stays Ok.
        assert_eq!(level_with_floor(100 * GB, 60 * GB, Some(500 * GB)), Level::Ok);
    }

    #[test]
    fn backup_series_strip_dates_not_ids() {
        assert_eq!(backup_series("unchain-20261104-043001.dump").as_deref(), Some("unchain#dump"));
        assert_eq!(backup_series("unchain-20261108-043001.dump"), backup_series("unchain-20261104-043001.dump"));
        assert_eq!(backup_series("2026-11-04").as_deref(), Some("#"));
        assert_eq!(backup_series("site_1699999999.tar.gz").as_deref(), Some("site#tar.gz"));
        assert_eq!(backup_series("db1.dump"), None, "a short number names a thing, not a date");
        assert_ne!(backup_series("db1-20261104.dump"), backup_series("db2-20261104.dump"));
        assert_eq!(backup_series("latest.dump"), None);
        assert_eq!(backup_series("d895982f4f09ac4f"), None, "a hex hash is a name");
        assert_eq!(backup_series("12345678abcdef"), None);
        assert_eq!(backup_series("db-2026-11-04T10-00.tar.gz").as_deref(), Some("db#tar.gz"));
        assert_eq!(backup_series("db-2026-11-04T10-00Z.tar"), None, "touches a letter: not sure, keep");
    }

    #[test]
    fn under_pressure_rules_only_at_low_or_critical() {
        assert!(!Policy::for_level(Level::Ok).pressure_rules);
        assert!(Policy::for_level(Level::Low).pressure_rules);
        assert!(Policy::for_level(Level::Critical).pressure_rules);
        let old: Policy = serde_json::from_value({
            let mut v = serde_json::to_value(Policy::for_level(Level::Critical)).unwrap();
            v.as_object_mut().unwrap().remove("pressure_rules");
            v
        })
        .unwrap();
        assert!(!old.pressure_rules, "a plan from before 0.3 never unlocks them");
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
