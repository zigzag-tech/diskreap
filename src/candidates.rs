//! A plan seen as things a person decides about.
//!
//! Every candidate carries what a human needs: size, age, category, a safety tier,
//! a proposed action, diskreap's own reason, whether it can be regenerated,
//! whether it looks like personal data, who is using it, and what was decided
//! about the same path last time.
//!
//! Tiers: `safe` (an always-safe category diskreap would clean now), `pressure`
//! (safe only under disk pressure), `human` (report only — diskreap never removes
//! these on its own), `protected` (a protect path or rule covers it; shown so the
//! space is explained, never actionable).

use crate::feedback::{Decision, Policy};
use crate::instructions::Interp;
use crate::plan::{Plan, PRESSURE_ONLY};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const MAX_CANDIDATES: usize = 400;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Cand {
    pub candidate: String,
    pub path: String,
    pub bytes: u64,
    pub newest_s: i64,
    pub category: String,
    pub tier: String,
    pub action: String,
    pub why: String,
    pub regenerable: bool,
    pub personal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_use: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rule: String,
    /// The plan item this came from is `ok` (stage 2 would act on it) — or, for a
    /// pressure item, would be under pressure. Report-only and worktrees needing a
    /// human are false.
    pub plan_ok: bool,
    /// A dirty or unmerged worktree: holds work, never removed here.
    #[serde(default)]
    pub holds_work: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Decision>,
}

pub fn candidate_id(machine: &str, path: &str) -> String {
    crate::sha::hex(format!("{machine}\0{path}").as_bytes())[..16].to_string()
}

fn regenerable(category: &str) -> bool {
    matches!(category, "docker" | "build-output" | "cache" | "tmp" | "log" | "worktree" | "offloaded")
}

const PERSONAL_DIRS: &[&str] = &["pictures", "photos", "dcim", "movies", "music", "documents", "desktop", "相册", "照片"];
const PERSONAL_WORDS: &[&str] = &["wedding", "photo", "family", "vacation", "婚"];
const MEDIA_EXT: &[&str] = &[
    "jpg", "jpeg", "heic", "heif", "png", "raw", "cr2", "cr3", "nef", "arw", "dng", "mov", "mp4", "m4v", "avi",
    "mkv", "3gp", "mp3", "flac", "m4a", "wav", "pdf", "docx", "doc", "xlsx", "pptx", "pages", "key", "numbers",
];

/// Personal data: a path under a well-known personal folder, a name that says so,
/// or a directory whose sampled files are mostly photos/video/documents. The
/// sample is bounded (two levels, 400 entries) so a huge tree costs nothing.
pub fn looks_personal(path: &str, home: &Path) -> bool {
    let p = Path::new(path);
    let rel = p.strip_prefix(home).unwrap_or(p);
    let lower = rel.to_string_lossy().to_lowercase();
    if rel.components().next().is_some_and(|c| PERSONAL_DIRS.contains(&c.as_os_str().to_string_lossy().to_lowercase().as_str())) {
        return true;
    }
    if PERSONAL_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    let (mut media, mut files, mut seen) = (0u32, 0u32, 0u32);
    let mut stack = vec![(p.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 400 {
                break;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && depth < 1 {
                stack.push((e.path(), depth + 1));
            } else if ft.is_file() {
                files += 1;
                let ext = e.path().extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                if MEDIA_EXT.contains(&ext.as_str()) {
                    media += 1;
                }
            }
        }
    }
    files >= 10 && media * 2 >= files
}

pub struct Inputs<'a> {
    pub plan: &'a Plan,
    pub machine: &'a str,
    pub interp: &'a Interp,
    pub policy: &'a Policy,
    pub last: &'a BTreeMap<String, Decision>,
    pub archive_configured: bool,
    pub now: i64,
}

/// Project a plan into at most MAX_CANDIDATES candidates, biggest first.
/// Returns (candidates, truncated count, rule hits by rule id).
pub fn project(i: &Inputs) -> (Vec<Cand>, usize, BTreeMap<String, u64>) {
    let home = Path::new(&i.plan.home).to_path_buf();
    let mut raw: Vec<Cand> = vec![];
    for it in &i.plan.items {
        let (tier, plan_ok) = if it.cat == "report" && it.why.starts_with("protected") {
            ("protected", false) // the scanner lists protected paths >= 1 GiB so the space is explained
        } else if it.cat == "report" {
            ("human", false)
        } else if it.ok {
            (if matches!(it.cat.as_str(), "backup" | "offloaded") { "pressure" } else { "safe" }, true)
        } else if it.why == PRESSURE_ONLY {
            ("pressure", true)
        } else {
            continue; // skipped for a safety reason: in use, referenced, too new — not a candidate
        };
        raw.push(base(i.machine, &it.path, it.bytes, it.newest, &it.cat, tier, &it.why, plan_ok, false, String::new()));
    }
    for wt in &i.plan.review {
        let why = format!("{} worktree on {}: {}", wt.kind, wt.branch, wt.state);
        let owner = if wt.owner == "-" { String::new() } else { wt.owner.clone() };
        raw.push(base(i.machine, &wt.path, wt.bytes, wt.newest, "worktree", "human", &why, false, true, owner));
    }
    let mut hits = BTreeMap::new();
    let inuse = crate::procs::InUse::snapshot();
    let mut out = vec![];
    for mut c in raw {
        if c.tier != "protected" {
            if i.interp.admits(&c.category, &c.path, c.bytes, c.newest_s, i.now).is_err() {
                continue;
            }
            if let Some(rule) = i.policy.first_match(&c.category, &c.path) {
                *hits.entry(rule.rule.clone()).or_insert(0) += 1;
                if matches!(rule.kind.as_str(), "protect" | "category_off") {
                    continue; // a person said: not this
                }
                c.rule = rule.rule.clone();
            }
        }
        c.regenerable = regenerable(&c.category) && !c.holds_work;
        if c.tier == "human" || c.tier == "protected" {
            c.personal = looks_personal(&c.path, &home);
            c.in_use = inuse.under(Path::new(&c.path)).map(|p| p.display().to_string());
        }
        c.action = propose(&c, i.archive_configured, i.policy);
        if i.interp.report_only {
            c.action = "keep".into();
        }
        c.last = i.last.get(&c.path).cloned();
        out.push(c);
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    apply_target(&mut out, i);
    let truncated = out.len().saturating_sub(MAX_CANDIDATES);
    out.truncate(MAX_CANDIDATES);
    (out, truncated, hits)
}

#[allow(clippy::too_many_arguments)]
fn base(machine: &str, path: &str, bytes: u64, newest: i64, cat: &str, tier: &str, why: &str, plan_ok: bool, holds_work: bool, owner: String) -> Cand {
    Cand {
        candidate: candidate_id(machine, path),
        path: path.into(),
        bytes,
        newest_s: newest,
        category: cat.into(),
        tier: tier.into(),
        action: "keep".into(),
        why: why.chars().take(300).collect(),
        regenerable: false,
        personal: false,
        in_use: None,
        owner,
        rule: String::new(),
        plan_ok,
        holds_work,
        last: None,
    }
}

/// The proposed action. Personal data, work-holding worktrees and protected paths
/// are kept; regenerable data diskreap would clean is deleted; the rest is
/// archived when a destination exists, kept otherwise. A preference rule may
/// lower that, or raise keep→archive, but never makes non-regenerable data
/// `delete`.
fn propose(c: &Cand, archive: bool, policy: &Policy) -> String {
    if c.tier == "protected" || c.personal || c.holds_work {
        return "keep".into();
    }
    let default = if c.regenerable && c.plan_ok { "delete" } else if archive { "archive" } else { "keep" };
    let pref = policy.rules.iter().find(|r| r.rule == c.rule).map(|r| r.kind.as_str());
    match pref {
        Some("keep") => "keep",
        Some("archive") if archive => "archive",
        Some("delete") if c.regenerable && c.plan_ok => "delete",
        _ => default,
    }
    .into()
}

/// `target_free_pct`: propose only enough regenerable deletes to reach the target.
fn apply_target(cands: &mut [Cand], i: &Inputs) {
    let Some(pct) = i.interp.target_free_pct else { return };
    let total = i.plan.total.max(1);
    let goal = (total as f64 * pct / 100.0) as u64;
    let mut projected = i.plan.avail;
    for c in cands.iter_mut() {
        if c.action == "delete" {
            if projected >= goal {
                c.action = "keep".into();
                c.why = format!("target {pct}% free already reached; {}", c.why).chars().take(300).collect();
            } else {
                projected += c.bytes;
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ItemDecision {
    pub action: String,
    #[serde(default)]
    pub comment: String,
    pub at_ms: i64,
}

/// A supervised run on disk: the candidates and what was decided.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Run {
    pub id: String,
    pub machine: String,
    pub created_ms: i64,
    pub plan_created: i64,
    #[serde(default)]
    pub interp: Interp,
    #[serde(default)]
    pub candidates: Vec<Cand>,
    #[serde(default)]
    pub decisions: BTreeMap<String, ItemDecision>,
    #[serde(default)]
    pub truncated: usize,
}

pub struct Selection {
    pub count: u64,
    pub bytes: u64,
    pub digest: String,
    pub delete_count: u64,
    pub archive_count: u64,
}

impl Run {
    pub fn path(id: &str) -> std::path::PathBuf {
        crate::feedback::runs_dir().join(format!("{}.json", id.replace(['/', '.'], "_")))
    }
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path(&self.id);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }
    /// The newest run on disk.
    pub fn latest() -> Option<Run> {
        let rd = std::fs::read_dir(crate::feedback::runs_dir()).ok()?;
        let mut best: Option<Run> = None;
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                if let Some(r) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Run>(&b).ok()) {
                    if best.as_ref().is_none_or(|b| r.created_ms > b.created_ms) {
                        best = Some(r);
                    }
                }
            }
        }
        best
    }
    pub fn get(&self, id: &str) -> Option<&Cand> {
        self.candidates.iter().find(|c| c.candidate == id)
    }
    /// The decided delete/archive set and its digest: what an execute must name.
    pub fn selection(&self) -> Selection {
        let mut lines = vec![];
        let (mut bytes, mut del, mut arc) = (0, 0, 0);
        for (id, d) in &self.decisions {
            if d.action == "delete" || d.action == "archive" {
                if let Some(c) = self.get(id) {
                    lines.push(format!("{id}:{}", d.action));
                    bytes += c.bytes;
                    if d.action == "delete" { del += 1 } else { arc += 1 }
                }
            }
        }
        lines.sort();
        Selection {
            count: lines.len() as u64,
            bytes,
            digest: format!("sha256:{}", crate::sha::hex(lines.join("\n").as_bytes())),
            delete_count: del,
            archive_count: arc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_by_folder_name_and_by_content() {
        let home = std::env::temp_dir().join(format!("dr-cand-{}", std::process::id()));
        let shots = home.join("trip");
        std::fs::create_dir_all(&shots).unwrap();
        for n in 0..12 {
            std::fs::write(shots.join(format!("IMG_{n}.HEIC")), b"x").unwrap();
        }
        assert!(looks_personal(&home.join("Pictures/x").display().to_string(), &home));
        assert!(looks_personal(&home.join("wedding-2025").display().to_string(), &home));
        assert!(looks_personal(&shots.display().to_string(), &home));
        assert!(!looks_personal(&home.join("node_modules").display().to_string(), &home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn selection_digest_follows_decisions() {
        let mut r = Run::default();
        r.candidates.push(base("m", "/h/a", 10, 0, "cache", "safe", "", true, false, String::new()));
        r.candidates.push(base("m", "/h/b", 5, 0, "report", "human", "", false, false, String::new()));
        let empty = r.selection().digest;
        let a = r.candidates[0].candidate.clone();
        r.decisions.insert(a.clone(), ItemDecision { action: "delete".into(), comment: String::new(), at_ms: 1 });
        let s = r.selection();
        assert_eq!((s.count, s.bytes, s.delete_count), (1, 10, 1));
        assert_ne!(s.digest, empty);
        r.decisions.insert(a, ItemDecision { action: "keep".into(), comment: String::new(), at_ms: 2 });
        assert_eq!(r.selection().digest, empty);
    }

    #[test]
    fn proposals_never_delete_what_cannot_be_regenerated() {
        let p = Policy::default();
        let mut c = base("m", "/h/old", 1, 0, "report", "human", "", false, false, String::new());
        assert_eq!(propose(&c, true, &p), "archive");
        assert_eq!(propose(&c, false, &p), "keep");
        c.personal = true;
        assert_eq!(propose(&c, true, &p), "keep");
        let mut g = base("m", "/h/nm", 1, 0, "build-output", "safe", "", true, false, String::new());
        g.regenerable = true;
        assert_eq!(propose(&g, false, &p), "delete");
    }
}
