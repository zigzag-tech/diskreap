use crate::util::{mtime_of, run};
use std::path::{Path, PathBuf};
use std::time::Duration;

const T: Duration = Duration::from_secs(60);

fn git(dir: &Path, args: &[&str]) -> Option<crate::util::Out> {
    // --no-optional-locks: our own `git status` must not refresh the index —
    // that would bump its mtime and make every worktree we inspect look active.
    let mut a = vec!["git", "--no-optional-locks", "-C"];
    let d = dir.to_string_lossy();
    a.push(&d);
    a.extend_from_slice(args);
    run(&a, None, T)
}

fn line(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args)
        .filter(|o| o.ok)
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    line(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Ignored by .gitignore AND containing no tracked file.
pub fn ignored_untracked(top: &Path, p: &Path) -> bool {
    let rel = p
        .strip_prefix(top)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned();
    let ignored = git(top, &["check-ignore", "-q", "--no-index", "--", &rel])
        .map(|o| o.ok)
        .unwrap_or(false);
    ignored
        && git(top, &["ls-files", "--", &rel])
            .map(|o| o.ok && o.stdout.trim().is_empty())
            .unwrap_or(false)
}

pub fn tracked(top: &Path, p: &Path) -> bool {
    let rel = p
        .strip_prefix(top)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned();
    git(top, &["ls-files", "--error-unmatch", "--", &rel])
        .map(|o| o.ok)
        .unwrap_or(true)
}

/// Newest mtime among the repo's git metadata (index, HEAD, reflog): moves on
/// any status/add/commit/checkout, i.e. whenever someone works in the tree.
pub fn activity(dir: &Path) -> i64 {
    let Some(gd) = line(dir, &["rev-parse", "--absolute-git-dir"]) else {
        return i64::MAX;
    };
    let gd = PathBuf::from(gd);
    ["index", "HEAD", "logs/HEAD"]
        .iter()
        .map(|f| mtime_of(&gd.join(f)))
        .max()
        .unwrap_or(0)
}

/// The gitdir a `.git` FILE points to, if it is a linked worktree (not a submodule).
pub fn linked_worktree_gitdir(wt: &Path) -> Option<PathBuf> {
    let s = std::fs::read_to_string(wt.join(".git")).ok()?;
    let gd = s.trim().strip_prefix("gitdir:")?.trim();
    let gd = if Path::new(gd).is_absolute() {
        PathBuf::from(gd)
    } else {
        wt.join(gd)
    };
    gd.parent()?
        .file_name()
        .filter(|n| *n == "worktrees")
        .map(|_| gd.clone())
}

pub fn dirty_count(wt: &Path) -> Option<usize> {
    git(wt, &["status", "--porcelain", "--untracked-files=normal"])
        .filter(|o| o.ok)
        .map(|o| o.stdout.lines().count())
}

/// The branch new work lands on: origin/HEAD, else main/master (remote or local).
pub fn base_ref(wt: &Path) -> Option<String> {
    if let Some(r) = line(
        wt,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Some(r);
    }
    ["origin/main", "origin/master", "main", "master"]
        .iter()
        .find(|r| {
            git(wt, &["rev-parse", "--verify", "-q", r])
                .map(|o| o.ok)
                .unwrap_or(false)
        })
        .map(|r| r.to_string())
}

/// Every commit on HEAD is in `base` — directly or as a patch-equivalent (rebased) copy.
pub fn merged_into(wt: &Path, base: &str) -> bool {
    if git(wt, &["merge-base", "--is-ancestor", "HEAD", base])
        .map(|o| o.ok)
        .unwrap_or(false)
    {
        return true;
    }
    git(wt, &["cherry", base, "HEAD"])
        .filter(|o| o.ok)
        .map(|o| !o.stdout.lines().any(|l| l.starts_with('+')))
        .unwrap_or(false)
}

/// The main checkout owning a linked worktree, for `git worktree remove`.
pub fn main_checkout(wt: &Path) -> Option<PathBuf> {
    let common = PathBuf::from(line(
        wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    common.parent().map(Path::to_path_buf)
}

pub fn remove_worktree(main: &Path, wt: &Path) -> bool {
    let w = wt.to_string_lossy();
    let ok = git(main, &["worktree", "remove", "--force", &w])
        .map(|o| o.ok)
        .unwrap_or(false);
    let _ = git(main, &["worktree", "prune"]);
    ok
}

/// HEAD not on a branch — often a deliberate pin that something references by path.
pub fn detached(wt: &Path) -> bool {
    git(wt, &["symbolic-ref", "-q", "HEAD"])
        .map(|o| !o.ok)
        .unwrap_or(true)
}
