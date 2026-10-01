//! What can be reclaimed, by CATEGORY — never by project-specific path.
//!
//! - build-output : regenerable build/dependency trees (node_modules, target, …), only when git-ignored, untracked, and the project is idle.
//! - log          : huge old log files → compressed losslessly (zstd/gzip).
//! - cache        : re-downloadable tool & model caches, pruned by last use.
//! - worktree     : linked git worktrees that are clean, merged, and idle.
//! - docker       : build cache and dangling/unused images.
//! - report-only  : cold large dirs, trash, unmerged worktrees — a human decides.

use crate::walk::{Classify, DirHit};
use std::path::Path;

pub struct Rules {
    pub quick: bool,
}

const BUILD_MARKERS: &[&str] = &[
    "pubspec.yaml",
    "build.gradle",
    "build.gradle.kts",
    "CMakeLists.txt",
    "package.json",
    "setup.py",
    "pyproject.toml",
    "meson.build",
];

impl Classify for Rules {
    fn child_dir(&self, _path: &Path, name: &str, sib: &[String]) -> Option<DirHit> {
        let has = |f: &str| sib.iter().any(|s| s == f);
        let ok = match name {
            "node_modules" | ".dart_tool" | ".tox" | ".pytest_cache" | ".mypy_cache"
            | ".ruff_cache" | ".parcel-cache" | ".turbo" => true,
            "target" => has("Cargo.toml"),
            "build" => BUILD_MARKERS.iter().any(|m| has(m)),
            ".gradle" => [
                "settings.gradle",
                "settings.gradle.kts",
                "build.gradle",
                "build.gradle.kts",
            ]
            .iter()
            .any(|m| has(m)),
            ".next" | ".nuxt" | ".svelte-kit" | ".angular" => has("package.json"),
            ".venv" | "venv" => has("pyproject.toml") || has("requirements.txt") || has("setup.py"),
            "Pods" => has("Podfile"),
            ".build" => has("Package.swift"),
            _ => false,
        };
        if ok {
            return Some(DirHit {
                kind: "build-output",
                prune: true,
                size: true,
            });
        }
        // App-local caches: only the app knows if they re-derive — report, never auto-clean.
        let cachey = name == ".cache"
            || name == "cache"
            || name.ends_with("-cache")
            || name.ends_with("_cache");
        cachey.then_some(DirHit {
            kind: "app-cache",
            prune: false,
            size: !self.quick,
        })
    }

    fn self_dir(&self, _path: &Path, entries: &[(String, bool)]) -> Option<DirHit> {
        // The Cache Directory Tagging standard: Cargo `target/`, pip and others
        // declare "regenerable" with CACHEDIR.TAG — honored even outside git.
        if entries
            .iter()
            .any(|(n, is_dir)| n == "CACHEDIR.TAG" && !is_dir)
        {
            return Some(DirHit {
                kind: "build-output",
                prune: true,
                size: true,
            });
        }
        // A Python virtualenv under any name (python-venv-ocr, env, …).
        if entries
            .iter()
            .any(|(n, is_dir)| n == "pyvenv.cfg" && !is_dir)
        {
            return Some(DirHit {
                kind: "build-output",
                prune: true,
                size: true,
            });
        }
        // A `.git` FILE marks a linked worktree (or a submodule — filtered later).
        // Quick scan does not size worktrees up front; only eligible ones get sized.
        entries
            .iter()
            .any(|(n, is_dir)| n == ".git" && !is_dir)
            .then_some(DirHit {
                kind: "worktree",
                prune: false,
                size: !self.quick,
            })
    }

    fn file_interest(&self, name: &str) -> bool {
        is_log(name)
    }

    fn file(&self, _path: &Path, _name: &str, size: u64) -> Option<&'static str> {
        (size >= 100 << 20).then_some("log")
    }
}

/// `x.log`, `x.log.3` — not already-compressed rotations.
pub fn is_log(n: &str) -> bool {
    if n.ends_with(".log") {
        return true;
    }
    match n.rfind(".log.") {
        Some(i) => {
            let rest = &n[i + 5..];
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Strat {
    /// Delete individual files not used (max of atime, mtime) for N days.
    AgeFiles,
    /// Delete whole entries at this depth (a model, a crate, a browser build)
    /// whose newest use is older than N days.
    AgeEntries(usize),
    /// The tool's own prune command (it knows its store's invariants).
    Cmd(&'static [&'static str]),
}

pub struct Cache {
    pub rel: &'static str,
    pub strat: Strat,
    pub days: i64,
}

const fn c(rel: &'static str, strat: Strat, days: i64) -> Cache {
    Cache { rel, strat, days }
}

/// Re-downloadable caches, relative to $HOME. Absent entries are ignored, so one
/// table serves Linux and macOS. `~/` in a command means $HOME.
pub const CACHES: &[Cache] = &[
    // language package managers
    c(".cache/pip", Strat::AgeFiles, 30),
    c(".cache/uv", Strat::Cmd(&["uv", "cache", "prune"]), 0),
    c(".npm/_cacache", Strat::AgeFiles, 30),
    c(
        ".local/share/pnpm/store",
        Strat::Cmd(&["pnpm", "store", "prune"]),
        0,
    ),
    c(
        "Library/pnpm/store",
        Strat::Cmd(&["pnpm", "store", "prune"]),
        0,
    ),
    c(".cache/yarn", Strat::AgeEntries(2), 30),
    c(".bun/install/cache", Strat::AgeEntries(1), 30),
    c(".cargo/registry/cache", Strat::AgeEntries(2), 30),
    c(".cargo/registry/src", Strat::AgeEntries(2), 30),
    c(".cargo/git/checkouts", Strat::AgeEntries(1), 30),
    c(".cache/go-build", Strat::AgeFiles, 30),
    c(".gradle/wrapper/dists", Strat::AgeEntries(1), 30),
    c(".gradle/caches/build-cache-1", Strat::AgeFiles, 30),
    c(".m2/repository", Strat::AgeEntries(2), 60),
    c(".pub-cache/hosted", Strat::AgeEntries(2), 30),
    c(
        "anaconda3/pkgs",
        Strat::Cmd(&["~/anaconda3/bin/conda", "clean", "-a", "-y"]),
        0,
    ),
    c(
        "miniconda3/pkgs",
        Strat::Cmd(&["~/miniconda3/bin/conda", "clean", "-a", "-y"]),
        0,
    ),
    c(".cache/node-gyp", Strat::AgeEntries(1), 30),
    c(".cache/typescript", Strat::AgeEntries(1), 30),
    // browsers & test browsers
    c(".cache/ms-playwright", Strat::AgeEntries(1), 30),
    c(".cache/puppeteer", Strat::AgeEntries(2), 30),
    c(".cache/google-chrome", Strat::AgeFiles, 7),
    c(".cache/chromium", Strat::AgeFiles, 7),
    c(".cache/mozilla", Strat::AgeFiles, 7),
    // ML model hubs (one entry = one model). Re-downloads are large and can be slow: longer horizon.
    c(".cache/huggingface/hub", Strat::AgeEntries(1), 45),
    c(".cache/huggingface/xet", Strat::AgeFiles, 30),
    c(".cache/modelscope/hub/models", Strat::AgeEntries(2), 45),
    c(".cache/torch/hub", Strat::AgeFiles, 45),
    // platform
    c(".android/cache", Strat::AgeFiles, 30),
    c("Library/Caches", Strat::AgeFiles, 30),
    c(
        "Library/Developer/Xcode/DerivedData",
        Strat::AgeEntries(1),
        7,
    ),
    c(
        "Library/Developer/Xcode/iOS DeviceSupport",
        Strat::AgeEntries(1),
        90,
    ),
    c(
        "Library/Developer/CoreSimulator/Caches",
        Strat::AgeFiles,
        30,
    ),
];

/// Never auto-cleaned, but reported with their size.
pub const REPORT_ONLY: &[(&str, &str)] = &[
    (
        ".local/share/Trash",
        "trash — empty it if nothing there is needed",
    ),
    (".Trash", "trash — empty it if nothing there is needed"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sib(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn logs_detected_but_not_compressed_rotations() {
        assert!(is_log("lane.log"));
        assert!(is_log("app.log.3"));
        assert!(!is_log("app.log.3.zst"));
        assert!(!is_log("app.log.gz"));
        assert!(!is_log("catalog"));
    }

    #[test]
    fn build_dirs_need_a_project_marker() {
        let r = Rules { quick: true };
        let p = Path::new("/x");
        assert!(r.child_dir(p, "node_modules", &[]).is_some());
        assert!(r.child_dir(p, "target", &sib(&["Cargo.toml"])).is_some());
        assert!(r.child_dir(p, "target", &sib(&["README"])).is_none());
        assert!(r.child_dir(p, "build", &sib(&["notes.txt"])).is_none());
        assert!(r.child_dir(p, "build", &sib(&["pubspec.yaml"])).is_some());
        assert!(r
            .child_dir(p, "venv", &sib(&["requirements.txt"]))
            .is_some());
        assert!(r.child_dir(p, "src", &sib(&["Cargo.toml"])).is_none());
    }

    #[test]
    fn git_file_marks_worktree_git_dir_does_not() {
        let r = Rules { quick: false };
        let p = Path::new("/x");
        assert!(r.self_dir(p, &[(".git".into(), false)]).is_some());
        assert!(r.self_dir(p, &[(".git".into(), true)]).is_none());
        assert_eq!(
            r.self_dir(p, &[("pyvenv.cfg".into(), false)]).unwrap().kind,
            "build-output"
        );
        assert_eq!(
            r.self_dir(p, &[("CACHEDIR.TAG".into(), false)])
                .unwrap()
                .kind,
            "build-output"
        );
        assert_eq!(r.child_dir(p, "blob-cache", &[]).unwrap().kind, "app-cache");
    }
}
