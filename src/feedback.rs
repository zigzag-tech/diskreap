//! What people decided, and the policy it turns into.
//!
//! Files under `~/.local/state/diskreap/` (all bounded, rule: storage never grows
//! without a limit):
//! - `feedback.jsonl` — one line per recorded decision, with its comment. At 2 MiB
//!   it is rotated once to `feedback.jsonl.1` (the older file is dropped).
//! - `policy.json`    — at most 256 rules. A 257th is refused (`policy_full`), never
//!   silently dropped.
//! - `runs/<id>.json` — the candidates and decisions of a supervised run; the 3
//!   newest are kept.
//!
//! Rule kinds: `protect` (never a candidate, and protected for `scan`/`auto` too,
//! through the same check as `protect_paths`), `keep` / `archive` / `delete`
//! (preferences that set the proposed action and never override the safety
//! floor), `category_off`. Learned rules start disabled and apply only once a
//! person enables them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const MAX_RULES: usize = 256;
pub const FEEDBACK_MAX_BYTES: u64 = 2 << 20;
pub const RUNS_KEPT: usize = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Source {
    pub path: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub at_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rule {
    pub rule: String,
    /// protect | keep | archive | delete | category_off
    pub kind: String,
    #[serde(default)]
    pub path_glob: String,
    #[serde(default)]
    pub category: String,
    /// user | learned | config
    pub origin: String,
    pub enabled: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub hits: u64,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub sources: Vec<Source>,
}

impl Rule {
    pub fn matches(&self, category: &str, path: &str) -> bool {
        if !self.category.is_empty() && self.category != category {
            return false;
        }
        if self.path_glob.is_empty() {
            return !self.category.is_empty();
        }
        let g = crate::util::expand_tilde(&self.path_glob);
        glob::Pattern::new(&g.to_string_lossy()).is_ok_and(|p| p.matches(path)) || Path::new(path).starts_with(&g)
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct Policy {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Decision {
    pub t_ms: i64,
    pub run: String,
    pub path: String,
    pub category: String,
    /// delete | archive | keep | protect
    pub action: String,
    #[serde(default)]
    pub comment: String,
    /// none | path | pattern
    #[serde(default)]
    pub remember: String,
    #[serde(default)]
    pub by: String,
}

fn dir() -> PathBuf {
    crate::util::state_dir()
}

pub fn rule_id(kind: &str, glob: &str, category: &str) -> String {
    format!("r-{}", &crate::sha::hex(format!("{kind}\0{glob}\0{category}").as_bytes())[..12])
}

impl Policy {
    pub fn path() -> PathBuf {
        dir().join("policy.json")
    }
    pub fn load() -> Policy {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> Result<(), String> {
        let tmp = Self::path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, Self::path()).map_err(|e| e.to_string())
    }
    /// Insert or replace by id. Refuses past MAX_RULES.
    pub fn upsert(&mut self, rule: Rule) -> Result<(), String> {
        if let Some(r) = self.rules.iter_mut().find(|r| r.rule == rule.rule) {
            *r = rule;
            return Ok(());
        }
        if self.rules.len() >= MAX_RULES {
            return Err("policy_full".into());
        }
        self.rules.push(rule);
        Ok(())
    }
    pub fn enabled(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(|r| r.enabled)
    }
    /// The first enabled rule (protect before preferences) that applies.
    pub fn first_match(&self, category: &str, path: &str) -> Option<&Rule> {
        let order = ["protect", "category_off", "keep", "archive", "delete"];
        order.iter().find_map(|k| self.enabled().find(|r| r.kind == *k && r.matches(category, path)))
    }
}

/// Enabled `protect` rules as globs, for the scanner's protected-path check (the
/// same check `protect_paths` uses), so a protection made in the plugin holds for
/// `scan` and the unattended `auto` timer too.
pub fn protect_globs() -> Vec<(String, String)> {
    Policy::load()
        .enabled()
        .filter(|r| r.kind == "protect" && !r.path_glob.is_empty())
        .map(|r| (r.path_glob.clone(), r.rule.clone()))
        .collect()
}

pub fn record(d: &Decision) -> Result<(), String> {
    let path = dir().join("feedback.jsonl");
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= FEEDBACK_MAX_BYTES {
        std::fs::rename(&path, dir().join("feedback.jsonl.1")).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    writeln!(f, "{}", serde_json::to_string(d).unwrap()).map_err(|e| e.to_string())
}

/// Every decision on record, oldest first (both files; malformed lines skipped).
pub fn history() -> Vec<Decision> {
    let mut out = vec![];
    for name in ["feedback.jsonl.1", "feedback.jsonl"] {
        if let Ok(f) = std::fs::File::open(dir().join(name)) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(d) = serde_json::from_str::<Decision>(&line) {
                    out.push(d);
                }
            }
        }
    }
    out
}

/// The last decision per path.
pub fn last_by_path(history: &[Decision]) -> BTreeMap<String, Decision> {
    history.iter().map(|d| (d.path.clone(), d.clone())).collect()
}

/// Rules a decision creates directly: `remember: path|pattern`.
pub fn remembered_rule(d: &Decision) -> Option<Rule> {
    let glob = match d.remember.as_str() {
        "path" => d.path.clone(),
        "pattern" => format!("{}/*", Path::new(&d.path).parent()?.display()),
        _ => return None,
    };
    let kind = match d.action.as_str() {
        "keep" | "protect" => "protect",
        "archive" => "archive",
        "delete" => "delete",
        _ => return None,
    };
    Some(Rule {
        rule: rule_id(kind, &glob, ""),
        kind: kind.into(),
        path_glob: glob,
        category: String::new(),
        origin: "user".into(),
        enabled: true,
        note: d.comment.chars().take(280).collect(),
        hits: 0,
        created_ms: d.t_ms,
        sources: vec![Source { path: d.path.chars().take(300).collect(), comment: d.comment.clone(), at_ms: d.t_ms }],
    })
}

fn sources(ds: &[&Decision]) -> Vec<Source> {
    ds.iter()
        .rev()
        .take(3)
        .map(|d| Source { path: d.path.chars().take(300).collect(), comment: d.comment.chars().take(280).collect(), at_ms: d.t_ms })
        .collect()
}

/// Suggestions from repeated unremembered decisions: two `keep`s under one parent
/// with no `delete` there → `keep <parent>/*`; three `keep`s in a category with no
/// `delete` → `category_off`. Always created DISABLED, citing the decisions.
pub fn learned(history: &[Decision], existing: &Policy) -> Vec<Rule> {
    let last = last_by_path(history);
    let mut by_parent: BTreeMap<String, Vec<&Decision>> = BTreeMap::new();
    let mut by_cat: BTreeMap<String, Vec<&Decision>> = BTreeMap::new();
    for d in last.values() {
        if let Some(p) = Path::new(&d.path).parent() {
            by_parent.entry(p.display().to_string()).or_default().push(d);
        }
        by_cat.entry(d.category.clone()).or_default().push(d);
    }
    fn unremembered_keeps<'a>(ds: &[&'a Decision]) -> Option<Vec<&'a Decision>> {
        if ds.iter().any(|d| d.action == "delete") {
            return None;
        }
        Some(ds.iter().copied().filter(|d| d.action == "keep" && d.remember != "path" && d.remember != "pattern").collect())
    }
    let mut out = vec![];
    let known = |id: &str| existing.rules.iter().any(|r| r.rule == id);
    for (parent, ds) in &by_parent {
        if let Some(keeps) = unremembered_keeps(ds) {
            if keeps.len() >= 2 {
                let glob = format!("{parent}/*");
                let id = rule_id("keep", &glob, "");
                if !known(&id) {
                    out.push(Rule {
                        rule: id, kind: "keep".into(), path_glob: glob, category: String::new(), origin: "learned".into(),
                        enabled: false, note: format!("kept {} items here", keeps.len()), hits: 0,
                        created_ms: keeps.last().map(|d| d.t_ms).unwrap_or(0), sources: sources(&keeps),
                    });
                }
            }
        }
    }
    for (cat, ds) in &by_cat {
        if cat.is_empty() || cat == "report" {
            continue;
        }
        if let Some(keeps) = unremembered_keeps(ds) {
            if keeps.len() >= 3 {
                let id = rule_id("category_off", "", cat);
                if !known(&id) {
                    out.push(Rule {
                        rule: id, kind: "category_off".into(), path_glob: String::new(), category: cat.clone(),
                        origin: "learned".into(), enabled: false, note: format!("every {cat} item offered was kept"),
                        hits: 0, created_ms: keeps.last().map(|d| d.t_ms).unwrap_or(0), sources: sources(&keeps),
                    });
                }
            }
        }
    }
    out
}

pub fn runs_dir() -> PathBuf {
    let d = dir().join("runs");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Keep the newest RUNS_KEPT run files.
pub fn prune_runs() {
    let Ok(rd) = std::fs::read_dir(runs_dir()) else { return };
    let mut files: Vec<(i64, PathBuf)> = rd.flatten().map(|e| (crate::util::mtime_of(&e.path()), e.path())).collect();
    files.sort();
    while files.len() > RUNS_KEPT {
        let (_, p) = files.remove(0);
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(path: &str, action: &str, remember: &str, cat: &str) -> Decision {
        Decision { t_ms: 1, run: "r".into(), path: path.into(), category: cat.into(), action: action.into(),
                   comment: "c".into(), remember: remember.into(), by: "user:x".into() }
    }

    #[test]
    fn remembered_keep_becomes_an_enabled_protect_rule() {
        let r = remembered_rule(&d("/h/datasets/raw", "keep", "path", "report")).unwrap();
        assert_eq!((r.kind.as_str(), r.enabled, r.origin.as_str()), ("protect", true, "user"));
        assert!(r.matches("report", "/h/datasets/raw/a.bin"));
        let p = remembered_rule(&d("/h/models/a", "keep", "pattern", "cache")).unwrap();
        assert_eq!(p.path_glob, "/h/models/*");
        assert!(remembered_rule(&d("/h/x", "keep", "none", "cache")).is_none());
    }

    #[test]
    fn repeated_keeps_suggest_a_disabled_rule_unless_something_was_deleted() {
        let h = vec![d("/h/models/a", "keep", "none", "cache"), d("/h/models/b", "keep", "none", "cache")];
        let s = learned(&h, &Policy::default());
        let r = s.iter().find(|r| r.kind == "keep").unwrap();
        assert_eq!((r.path_glob.as_str(), r.enabled, r.origin.as_str()), ("/h/models/*", false, "learned"));
        assert_eq!(r.sources.len(), 2);
        let mut h2 = h.clone();
        h2.push(d("/h/models/c", "delete", "none", "cache"));
        assert!(learned(&h2, &Policy::default()).iter().all(|r| r.kind != "keep"));
    }

    #[test]
    fn protect_wins_over_preferences_and_disabled_rules_do_nothing() {
        let mut p = Policy::default();
        p.upsert(Rule { rule: "a".into(), kind: "delete".into(), path_glob: "/h/x/*".into(), category: String::new(),
                        origin: "user".into(), enabled: true, note: String::new(), hits: 0, created_ms: 0, sources: vec![] }).unwrap();
        p.upsert(Rule { rule: "b".into(), kind: "protect".into(), path_glob: "/h/x/keep".into(), category: String::new(),
                        origin: "user".into(), enabled: true, note: String::new(), hits: 0, created_ms: 0, sources: vec![] }).unwrap();
        p.upsert(Rule { rule: "c".into(), kind: "protect".into(), path_glob: "/h/x/other".into(), category: String::new(),
                        origin: "learned".into(), enabled: false, note: String::new(), hits: 0, created_ms: 0, sources: vec![] }).unwrap();
        assert_eq!(p.first_match("cache", "/h/x/keep").unwrap().rule, "b");
        assert_eq!(p.first_match("cache", "/h/x/other").unwrap().rule, "a");
    }
}
