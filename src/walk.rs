//! Parallel, single-filesystem, hang-proof directory walker.
//!
//! - Never opens a mountpoint below the root (path-skipped from the mount table)
//!   and never crosses a device boundary it discovers while statting.
//! - A watchdog abandons any directory whose syscalls stall longer than
//!   `stall` (a dead network mount we did not know about, a wedged disk):
//!   the path is reported and the scan finishes without it.
//! - In "discovery" mode (quick scan) it only reads directory entries — no stat —
//!   except inside classified candidates, which are sized fully.

use crate::platform;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Debug)]
pub struct Acc {
    pub bytes: AtomicU64,
    pub newest: AtomicI64,
}

impl Acc {
    fn add(&self, b: u64, mtime: i64) {
        self.bytes.fetch_add(b, Relaxed);
        self.newest.fetch_max(mtime, Relaxed);
    }
}

pub struct DirHit {
    pub kind: &'static str,
    /// Stop classifying inside (a node_modules inside a node_modules is not a new candidate).
    pub prune: bool,
    /// Size this subtree fully even in discovery mode.
    pub size: bool,
}

pub trait Classify: Send + Sync {
    /// A child directory `name`, judged with its parent's entry names (`siblings`).
    fn child_dir(&self, path: &Path, name: &str, siblings: &[String]) -> Option<DirHit>;
    /// A directory judged by its own entries (`(name, is_dir)`).
    fn self_dir(&self, path: &Path, entries: &[(String, bool)]) -> Option<DirHit>;
    /// Cheap name pre-filter: only these files get stat'ed in discovery mode.
    fn file_interest(&self, name: &str) -> bool;
    fn file(&self, path: &Path, name: &str, size: u64) -> Option<&'static str>;
}

pub struct Opts {
    pub root: PathBuf,
    pub skip: Vec<PathBuf>,
    /// Size everything (full scan) vs. discovery-only outside candidates (quick scan).
    pub size_all: bool,
    /// Record per-directory totals down to this depth (full scan only).
    pub report_depth: usize,
    /// Discovery depth limit outside candidates (quick scan); None = unlimited.
    pub max_depth: Option<usize>,
    pub stall: Duration,
    pub threads: usize,
}

#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub kind: &'static str,
    pub bytes: u64,
    pub newest: i64,
    pub sized: bool,
}

#[derive(Debug, Clone)]
pub struct FileHit {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Default)]
pub struct WalkResult {
    pub total: u64,
    /// (path, depth, bytes, newest mtime) for directories down to `report_depth`.
    pub nodes: Vec<(PathBuf, usize, u64, i64)>,
    pub found: Vec<Found>,
    pub files: Vec<FileHit>,
    pub kept: Vec<PathBuf>,
    pub skipped_mounts: Vec<PathBuf>,
    pub stalled: Vec<PathBuf>,
    pub errors: u64,
}

struct Item {
    path: PathBuf,
    depth: usize,
    accs: Vec<Arc<Acc>>,
    detect: bool,
    sizing: bool,
}

#[derive(Default)]
struct Slot {
    cur: Option<(PathBuf, Instant)>,
    abandoned: bool,
}

struct Q {
    items: Vec<Item>,
    pending: usize,
    done: bool,
}

#[derive(Default)]
struct Collected {
    nodes: Vec<(PathBuf, usize, Arc<Acc>)>,
    found: Vec<(PathBuf, &'static str, Arc<Acc>, bool)>,
    files: Vec<FileHit>,
    kept: Vec<PathBuf>,
    skipped: Vec<PathBuf>,
}

struct Shared {
    q: Mutex<Q>,
    cv: Condvar,
    cls: Box<dyn Classify>,
    skip: HashSet<PathBuf>,
    root_dev: u64,
    size_all: bool,
    report_depth: usize,
    max_depth: Option<usize>,
    out: Mutex<Collected>,
    inodes: Mutex<HashSet<(u64, u64)>>,
    errors: AtomicU64,
}

pub fn walk(opts: Opts, cls: Box<dyn Classify>) -> WalkResult {
    let root_md = match fs::symlink_metadata(&opts.root) {
        Ok(m) if m.is_dir() => m,
        _ => {
            return WalkResult {
                errors: 1,
                ..Default::default()
            }
        }
    };
    let root_acc = Arc::new(Acc::default());
    let sh = Arc::new(Shared {
        q: Mutex::new(Q {
            items: vec![Item {
                path: opts.root.clone(),
                depth: 0,
                accs: vec![root_acc.clone()],
                detect: true,
                sizing: opts.size_all,
            }],
            pending: 1,
            done: false,
        }),
        cv: Condvar::new(),
        cls,
        skip: opts.skip.into_iter().collect(),
        root_dev: platform::dev(&root_md),
        size_all: opts.size_all,
        report_depth: opts.report_depth,
        max_depth: opts.max_depth,
        out: Mutex::new(Collected::default()),
        inodes: Mutex::new(HashSet::new()),
        errors: AtomicU64::new(0),
    });
    if opts.size_all {
        sh.out
            .lock()
            .unwrap()
            .nodes
            .push((opts.root.clone(), 0, root_acc.clone()));
    }

    let mut slots: Vec<Arc<Mutex<Slot>>> = Vec::new();
    let spawn = |slots: &mut Vec<Arc<Mutex<Slot>>>| {
        let slot = Arc::new(Mutex::new(Slot::default()));
        slots.push(slot.clone());
        let sh = sh.clone();
        std::thread::spawn(move || worker(sh, slot));
    };
    for _ in 0..opts.threads.max(1) {
        spawn(&mut slots);
    }

    let mut stalled = Vec::new();
    loop {
        {
            let q = sh.q.lock().unwrap();
            let (mut q, _) = sh.cv.wait_timeout(q, Duration::from_millis(250)).unwrap();
            if q.pending == 0 {
                q.done = true;
                drop(q);
                sh.cv.notify_all();
                break;
            }
        }
        // Watchdog: abandon stuck workers and replace them so the scan always ends.
        let mut replace = 0;
        for slot in &slots {
            let mut s = slot.lock().unwrap();
            if s.abandoned {
                continue;
            }
            if let Some((p, t)) = &s.cur {
                if t.elapsed() > opts.stall {
                    stalled.push(p.clone());
                    s.abandoned = true;
                    replace += 1;
                }
            }
        }
        if replace > 0 {
            {
                let mut q = sh.q.lock().unwrap();
                q.pending -= replace;
            }
            for _ in 0..replace {
                spawn(&mut slots);
            }
            sh.cv.notify_all();
        }
    }

    let out = std::mem::take(&mut *sh.out.lock().unwrap());
    let get = |a: &Acc| (a.bytes.load(Relaxed), a.newest.load(Relaxed));
    WalkResult {
        total: root_acc.bytes.load(Relaxed),
        nodes: out
            .nodes
            .iter()
            .map(|(p, d, a)| {
                let (b, n) = get(a);
                (p.clone(), *d, b, n)
            })
            .collect(),
        found: out
            .found
            .iter()
            .map(|(p, k, a, sized)| {
                let (b, n) = get(a);
                Found {
                    path: p.clone(),
                    kind: k,
                    bytes: b,
                    newest: n,
                    sized: *sized,
                }
            })
            .collect(),
        files: out.files,
        kept: out.kept,
        skipped_mounts: out.skipped,
        stalled,
        errors: sh.errors.load(Relaxed),
    }
}

fn worker(sh: Arc<Shared>, slot: Arc<Mutex<Slot>>) {
    loop {
        let item = {
            let mut q = sh.q.lock().unwrap();
            loop {
                if q.done {
                    return;
                }
                if let Some(it) = q.items.pop() {
                    break it;
                }
                q = sh.cv.wait(q).unwrap();
            }
        };
        slot.lock().unwrap().cur = Some((item.path.clone(), Instant::now()));
        process(&sh, item);
        {
            let mut s = slot.lock().unwrap();
            if s.abandoned {
                return; // the watchdog already accounted for this item
            }
            s.cur = None;
        }
        sh.q.lock().unwrap().pending -= 1;
        sh.cv.notify_all();
    }
}

fn process(sh: &Shared, item: Item) {
    let rd = match fs::read_dir(&item.path) {
        Ok(r) => r,
        Err(_) => {
            sh.errors.fetch_add(1, Relaxed);
            return;
        }
    };
    let mut ents: Vec<(String, PathBuf, fs::FileType)> = Vec::new();
    for e in rd.flatten() {
        if let Ok(ft) = e.file_type() {
            ents.push((e.file_name().to_string_lossy().into_owned(), e.path(), ft));
        }
    }

    let mut accs = item.accs;
    let mut detect = item.detect;
    let mut sizing = item.sizing;
    if detect {
        if ents.iter().any(|(n, _, _)| n == ".diskreap-keep") {
            detect = false;
            sh.out.lock().unwrap().kept.push(item.path.clone());
        } else {
            let view: Vec<(String, bool)> = ents
                .iter()
                .map(|(n, _, ft)| (n.clone(), ft.is_dir()))
                .collect();
            if let Some(h) = sh.cls.self_dir(&item.path, &view) {
                let a = Arc::new(Acc::default());
                let sized = sizing || h.size;
                sh.out
                    .lock()
                    .unwrap()
                    .found
                    .push((item.path.clone(), h.kind, a.clone(), sized));
                accs.push(a);
                sizing = sized;
                if h.prune {
                    detect = false;
                }
            }
        }
    }
    let names: Vec<String> = if detect {
        ents.iter().map(|(n, _, _)| n.clone()).collect()
    } else {
        Vec::new()
    };

    let mut children = Vec::new();
    for (name, path, ft) in ents {
        if ft.is_dir() {
            if sh.skip.contains(&path) {
                sh.out.lock().unwrap().skipped.push(path);
                continue;
            }
            let mut caccs = accs.clone();
            let mut cdetect = detect;
            let mut csizing = sizing;
            if detect {
                if let Some(h) = sh.cls.child_dir(&path, &name, &names) {
                    let a = Arc::new(Acc::default());
                    let sized = sizing || h.size;
                    sh.out
                        .lock()
                        .unwrap()
                        .found
                        .push((path.clone(), h.kind, a.clone(), sized));
                    caccs.push(a);
                    csizing = sized;
                    if h.prune {
                        cdetect = false;
                    }
                }
            }
            let depth = item.depth + 1;
            if sh.size_all && depth <= sh.report_depth {
                let a = Arc::new(Acc::default());
                sh.out
                    .lock()
                    .unwrap()
                    .nodes
                    .push((path.clone(), depth, a.clone()));
                caccs.push(a);
            }
            if csizing {
                match fs::symlink_metadata(&path) {
                    Ok(m) if platform::dev(&m) != sh.root_dev => {
                        sh.out.lock().unwrap().skipped.push(path);
                        continue;
                    }
                    Ok(m) => caccs
                        .iter()
                        .for_each(|a| a.add(platform::alloc(&m), platform::mtime(&m))),
                    Err(_) => {
                        sh.errors.fetch_add(1, Relaxed);
                        continue;
                    }
                }
            } else if let Some(md) = sh.max_depth {
                if depth > md {
                    continue;
                }
            }
            children.push(Item {
                path,
                depth,
                accs: caccs,
                detect: cdetect,
                sizing: csizing,
            });
        } else if sizing || (detect && ft.is_file() && sh.cls.file_interest(&name)) {
            let m = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    sh.errors.fetch_add(1, Relaxed);
                    continue;
                }
            };
            if sizing {
                let counted =
                    platform::hardlink_key(&m).is_none_or(|k| sh.inodes.lock().unwrap().insert(k));
                let b = if counted { platform::alloc(&m) } else { 0 };
                accs.iter().for_each(|a| a.add(b, platform::mtime(&m)));
            }
            if detect
                && ft.is_file()
                && sh.cls.file_interest(&name)
                && sh.cls.file(&path, &name, m.len()).is_some()
            {
                sh.out.lock().unwrap().files.push(FileHit {
                    path,
                    size: m.len(),
                    mtime: platform::mtime(&m),
                });
            }
        }
    }
    if !children.is_empty() {
        let mut q = sh.q.lock().unwrap();
        q.pending += children.len();
        q.items.extend(children);
        drop(q);
        sh.cv.notify_all();
    }
}
