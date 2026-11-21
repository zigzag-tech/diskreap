//! agent-spool: per-session artifacts a coding agent leaves in its XDG state dir.
//!
//! Terminal agents keep a conversation database (`*.sqlite`) in `$XDG_STATE_HOME/<app>/…`
//! and, beside it, files that matter only while a conversation might still be resumed:
//! spooled tool results (`tool-output/<call-id>.txt`, or any directory of `call_*.txt` /
//! `toolu_*.txt` files), trimmed history segments (`history-<session>-<n>.txt`),
//! per-session checkpoint pre-images (`checkpoints/<session-id>/`), and per-run temp files
//! (`model-choices-*.json`). Nothing prunes them, so they grow with every session.
//!
//! Detection is by layout, never by an app's path: an *agent state dir* is a directory
//! under the XDG state root holding a `*.sqlite` file, a `tool-output/` directory, or a
//! tool-call spool directory. Inside it only the patterns above are units; each goes when
//! its newest modification AND file access are older than its horizon (30 days for spools,
//! history and checkpoints, 7 for per-run temp files), and no process holds it. A
//! checkpoint session is one unit: its pre-images restore together or not at all. Anything
//! named `*.sqlite*` (the conversation database, its -wal/-shm/-journal) is never a unit.

use crate::act;
use crate::mounts::{self, Mount};
use crate::plan::Ctx;
use crate::util::{now, DAY};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Spooled tool output, trimmed history and checkpoint pre-images: the resume horizon.
pub const SPOOL_IDLE_DAYS: i64 = 30;
/// Per-run temp files (`model-choices-*.json`): useful to the one run that wrote them.
pub const RUN_TEMP_IDLE_DAYS: i64 = 7;
/// `$XDG_STATE_HOME/<app>/<sub>/…`: how deep agent state dirs are looked for.
const MAX_DEPTH: usize = 4;

/// `$XDG_STATE_HOME` when set to an absolute path (the spec ignores a relative one),
/// else `~/.local/state`.
pub fn state_root(home: &Path) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"))
}

fn is_db(name: &str) -> bool {
    name.contains(".sqlite") || name.ends_with(".db") || name.contains(".db-")
}

/// A file named for a tool call: `call_<id>.txt` (OpenAI-style ids), `toolu_<id>.txt`
/// (Anthropic-style), or a spooled history segment.
fn is_call_file(name: &str) -> bool {
    name.ends_with(".txt") && (name.starts_with("call_") || name.starts_with("toolu_") || name.starts_with("history-"))
}

/// A session id as agents write them: a UUID or similar (hex digits and dashes, ≥ 8 chars).
fn is_session_id(name: &str) -> bool {
    name.len() >= 8 && name.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn entries(d: &Path) -> Vec<(String, PathBuf, std::fs::Metadata)> {
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), p, m))
        })
        .collect()
}

/// A directory holding only tool-call spool files (at least one).
fn is_call_spool(d: &Path) -> bool {
    let es = entries(d);
    !es.is_empty() && es.iter().all(|(n, _, m)| m.is_file() && is_call_file(n))
}

pub fn is_agent_state_dir(d: &Path) -> bool {
    entries(d).iter().any(|(n, p, m)| {
        (m.is_file() && is_db(n) && n.ends_with(".sqlite"))
            || (m.is_dir() && (n == "tool-output" || is_call_spool(p)))
    })
}

/// Agent state dirs at most MAX_DEPTH below `root`, never below a mount or through a symlink.
pub fn agent_state_dirs(root: &Path, mounts: &[Mount]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        if depth > 0 && is_agent_state_dir(&d) {
            out.push(d.clone());
        }
        if depth == MAX_DEPTH {
            continue;
        }
        for (n, p, m) in entries(&d) {
            if m.is_dir() && n != "tool-output" && n != "checkpoints" && mounts::below(mounts, &p).is_empty() {
                stack.push((p, depth + 1));
            }
        }
    }
    out.sort();
    out
}

pub struct Unit {
    pub path: PathBuf,
    pub days: i64,
    pub kind: String,
}

/// What in an agent state dir may go, each with its horizon. Only regular files and
/// checkpoint session directories; never a symlink, never a database.
pub fn units(d: &Path) -> Vec<Unit> {
    let mut v = Vec::new();
    let unit = |path: PathBuf, days: i64, kind: &str| Unit { path, days, kind: kind.to_string() };
    for (n, p, m) in entries(d) {
        if is_db(&n) || m.file_type().is_symlink() {
            continue;
        }
        if m.is_file() {
            if n.starts_with("history-") && n.ends_with(".txt") {
                v.push(unit(p, SPOOL_IDLE_DAYS, "history"));
            } else if n.starts_with("model-choices-") && n.ends_with(".json") {
                v.push(unit(p, RUN_TEMP_IDLE_DAYS, "model-choices"));
            }
        } else if m.is_dir() && n == "checkpoints" {
            for (s, q, sm) in entries(&p) {
                if sm.is_dir() && is_session_id(&s) {
                    v.push(unit(q, SPOOL_IDLE_DAYS, "checkpoints"));
                }
            }
        } else if m.is_dir() && (n == "tool-output" || is_call_spool(&p)) {
            for (f, q, fm) in entries(&p) {
                if fm.is_file() && !is_db(&f) {
                    v.push(unit(q, SPOOL_IDLE_DAYS, &n));
                }
            }
        }
    }
    v
}

#[derive(Default)]
pub struct Pass {
    /// Allocated bytes of every unit found.
    pub total: u64,
    /// Of those, idle past their horizon and not held (deleted, when `delete`).
    pub bytes: u64,
    /// Reclaimable units per kind (`tool-output`, `host-exec`, `model-choices`, …).
    pub counts: BTreeMap<String, usize>,
}

impl Pass {
    pub fn summary(&self) -> String {
        self.counts.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")
    }
}

/// Measure (delete=false) or prune (delete=true) one agent state dir. Every unit is judged
/// right here, at the moment it would be removed.
pub fn pass(d: &Path, cx: &Ctx, delete: bool) -> Pass {
    let t = now();
    let mut r = Pass::default();
    for u in units(d) {
        let st = act::tree_stats(&u.path);
        r.total += st.bytes;
        let idle = t - st.newest.max(st.used) >= u.days * DAY;
        let free = cx.inuse.under(&u.path).is_none() && cx.refs.within(&u.path).is_none() && !st.special && !st.keep;
        if idle && free && (!delete || act::remove_tree(&u.path).is_ok()) {
            r.bytes += st.bytes;
            *r.counts.entry(u.kind).or_default() += 1;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Level, Policy};
    use crate::procs::{InUse, Refs};
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn put(p: &Path, days_old: u64) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![7u8; 8192]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(days_old * 86400);
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(t).set_accessed(t))
            .unwrap();
    }

    /// Directory mtimes count as activity (a file added or removed): age them too.
    fn age_dir(p: &Path, days_old: u64) {
        let t = SystemTime::now() - Duration::from_secs(days_old * 86400);
        fs::File::open(p).unwrap().set_times(fs::FileTimes::new().set_modified(t).set_accessed(t)).unwrap();
    }

    fn fixture(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dr-spool-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    fn run(d: &Path, inuse: &InUse, delete: bool) -> Pass {
        let policy = Policy::for_level(Level::Ok);
        let refs = Refs::default();
        let cx = Ctx { tmp_idle_days: 7, home: d, inuse, refs: &refs, policy: &policy, mounts: &[] };
        pass(d, &cx, delete)
    }

    #[test]
    fn old_spools_go_recent_and_databases_stay() {
        let root = fixture("a");
        let s = root.join("agent/rust");
        put(&s.join("conversations.sqlite"), 400);
        put(&s.join("conversations.sqlite-wal"), 400);
        put(&s.join("transcript-1.jsonl"), 400); // not a spool pattern
        put(&s.join("tool-output/call_old.txt"), 40);
        put(&s.join("tool-output/history-abc-1.txt"), 40);
        put(&s.join("tool-output/call_new.txt"), 2);
        put(&s.join("tool-output/stray.sqlite"), 400);
        put(&s.join("host-exec/call_1.txt"), 45);
        put(&s.join("host-exec/call_2.txt"), 10);
        put(&s.join("model-choices-1.json"), 8);
        put(&s.join("model-choices-2.json"), 3);
        put(&s.join("history-loose-1.txt"), 10); // 30d horizon, not model-choices' 7
        put(&s.join("checkpoints/20720273-fcac-4e86/turns/t1"), 40);
        put(&s.join("checkpoints/31a0c2e1-aaaa-4e86/turns/t1"), 40);
        put(&s.join("checkpoints/31a0c2e1-aaaa-4e86/blobs/b1"), 1); // live session: whole unit kept
        put(&s.join("checkpoints/notes/x"), 400); // not a session id
        for d in ["checkpoints/20720273-fcac-4e86/turns", "checkpoints/20720273-fcac-4e86"] {
            age_dir(&s.join(d), 40);
        }

        assert_eq!(agent_state_dirs(&root, &[]), vec![s.clone()]);
        let dry = run(&s, &InUse::from(&[]), false);
        assert_eq!(dry.summary(), "checkpoints 1, host-exec 1, model-choices 1, tool-output 2");
        assert!(s.join("tool-output/call_old.txt").exists(), "a dry run deletes nothing");

        let held = s.join("host-exec/call_1.txt");
        let r = run(&s, &InUse::from(&[held.to_str().unwrap()]), true);
        assert_eq!(r.summary(), "checkpoints 1, model-choices 1, tool-output 2", "a held file stays");
        for gone in ["tool-output/call_old.txt", "tool-output/history-abc-1.txt", "model-choices-1.json", "checkpoints/20720273-fcac-4e86"] {
            assert!(!s.join(gone).exists(), "{gone} should be reclaimed");
        }
        for kept in [
            "conversations.sqlite", "conversations.sqlite-wal", "transcript-1.jsonl", "tool-output/call_new.txt",
            "tool-output/stray.sqlite", "host-exec/call_1.txt", "host-exec/call_2.txt", "model-choices-2.json",
            "history-loose-1.txt", "checkpoints/31a0c2e1-aaaa-4e86/turns/t1", "checkpoints/notes/x",
        ] {
            assert!(s.join(kept).exists(), "{kept} must be kept");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_agent_layouts_qualify() {
        let root = fixture("b");
        put(&root.join("app/notes/call_1.txt"), 90);
        put(&root.join("app/notes/readme.md"), 90); // a mixed dir is not a spool
        put(&root.join("app/model-choices-1.json"), 90); // no database or spool beside it
        put(&root.join("other/history.json"), 90);
        put(&root.join("diskreap/plan.json"), 90);
        assert!(agent_state_dirs(&root, &[]).is_empty());
        // A directory of tool-call files alone marks its parent as an agent state dir.
        put(&root.join("x/a/spool/call_9.txt"), 90);
        assert_eq!(agent_state_dirs(&root, &[]), vec![root.join("x/a")]);
        fs::remove_dir_all(&root).unwrap();
    }
}
