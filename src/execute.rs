//! Supervised execution of a reviewed selection.
//!
//! Called only for a `diskreap.execute/1` a person confirmed (the contract admits
//! human writers only; the service checks the principal again). Every item is
//! re-verified immediately before it is touched — the scanner's own `check` for
//! plan items, `reap`'s floor for report-only ones — and protect paths, protect
//! rules and `.diskreap-keep` apply however the item got into the selection.
//!
//! `archive` copies with `offload-verified.sh SRC DEST LOG` (rclone copy
//! --checksum, then rclone check --checksum; the script deletes the source only
//! on zero differences with the source unchanged and not open). Without a
//! configured destination an archive is refused, never turned into a delete.
//! Personal data is never deleted outright.

use crate::candidates::Cand;
use crate::util::{expand_tilde, fs_space, home};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Archive {
    pub script: PathBuf,
    pub dest: String,
    pub log: PathBuf,
}

/// `archive` in diskreap's config.json: `{"dest": "cos:bucket/prefix", "script": "...", "log": "..."}`.
pub fn archive_config() -> Option<Archive> {
    let cfg = crate::util::config_json()?;
    let a = cfg.get("archive")?;
    let dest = a.get("dest")?.as_str()?.trim_end_matches('/').to_string();
    if dest.is_empty() {
        return None;
    }
    let script = a
        .get("script")
        .and_then(|v| v.as_str())
        .map(expand_tilde)
        .unwrap_or_else(|| home().join("xc-setup/scripts/offload-verified.sh"));
    let log = a
        .get("log")
        .and_then(|v| v.as_str())
        .map(expand_tilde)
        .unwrap_or_else(|| crate::util::state_dir().join("archive.log"));
    script.is_file().then_some(Archive { script, dest, log })
}

pub fn archive_kind(a: &Archive) -> String {
    a.dest.split(':').next().unwrap_or("").chars().take(32).collect()
}

pub struct Outcome {
    /// done | refused | failed | skipped
    pub state: &'static str,
    pub reason: String,
    pub freed: u64,
    pub archive_ref: String,
}

fn refused(reason: impl Into<String>) -> Outcome {
    Outcome { state: "refused", reason: reason.into(), freed: 0, archive_ref: String::new() }
}

/// What one execute shares across its items: snapshots are taken once.
pub struct Ctx<'a> {
    pub plan: Option<&'a crate::plan::Plan>,
    pub archive: Option<&'a Archive>,
    pub approval: &'a str,
    pub apply: bool,
    pub inuse: &'a crate::procs::InUse,
    pub refs: &'a crate::procs::Refs,
    pub policy: &'a crate::feedback::Policy,
}

/// Checks that hold for every action, before anything category-specific.
fn floor(c: &Cand, action: &str, cx: &Ctx) -> Result<(), String> {
    let p = Path::new(&c.path);
    if !p.exists() {
        return Err("gone".into());
    }
    if c.holds_work {
        return Err("worktree holds work (uncommitted or unmerged): resolve it with git".into());
    }
    if c.tier == "protected" {
        return Err("protected".into());
    }
    if action == "delete" && c.personal {
        return Err("personal data is archived, never deleted outright".into());
    }
    let mut a = Some(p);
    while let Some(dir) = a {
        if dir.join(".diskreap-keep").exists() {
            return Err(format!("protected by {}/.diskreap-keep", dir.display()));
        }
        a = dir.parent();
    }
    if let Some(rule) = cx
        .policy
        .enabled()
        .find(|r| r.kind == "protect" && r.matches(&c.category, &c.path))
    {
        return Err(format!("protected by rule {}", rule.rule));
    }
    if let Some(src) = cx.refs.within(p) {
        return Err(format!("protected/referenced: {src}"));
    }
    if let Some(h) = cx.inuse.under(p) {
        return Err(format!("in use ({})", h.display()));
    }
    Ok(())
}

/// Delete one plan item through the same code `clean --apply` runs, so the
/// category's own action (worktree removal, cache prune, docker prune) and its
/// re-check apply unchanged.
fn delete_plan_item(c: &Cand, plan: &crate::plan::Plan, apply: bool) -> Outcome {
    let Some(item) = plan.items.iter().find(|i| i.path == c.path) else {
        return refused("not in the current plan: scan again");
    };
    let mut one = serde_json::to_value(plan).ok().and_then(|v| serde_json::from_value::<crate::plan::Plan>(v).ok()).unwrap();
    let mut it = item.clone();
    it.ok = true; // a pressure-only item a person chose; `check` still re-verifies it
    one.items = vec![it];
    one.review.clear();
    let r = crate::act::apply(
        &one,
        &crate::act::ApplyOpts { apply, categories: std::slice::from_ref(&c.category), until_avail: None, quiet: true },
    );
    match r.actions.first() {
        Some(a) if a.action == "skip" => refused(a.note.clone()),
        Some(a) if !a.ok => Outcome { state: "failed", reason: format!("{} failed", a.action), freed: 0, archive_ref: String::new() },
        Some(a) if !apply => Outcome { state: "skipped", reason: format!("dry run: would {}", a.action), freed: 0, archive_ref: String::new() },
        Some(a) => Outcome { state: "done", reason: a.action.clone(), freed: r.freed_df_bytes, archive_ref: String::new() },
        None => refused("nothing to do for this category"),
    }
}

fn archive(c: &Cand, a: &Archive, apply: bool) -> Outcome {
    let name = Path::new(&c.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let dest = format!("{}/{}/{}", a.dest, crate::util::hostname(), name);
    if !apply {
        return Outcome { state: "skipped", reason: format!("dry run: would archive to {dest} and verify checksums"), freed: 0, archive_ref: dest };
    }
    let before = fs_space(&home()).map(|s| s.1).unwrap_or(0);
    let script = a.script.to_string_lossy().to_string();
    let log = a.log.to_string_lossy().to_string();
    // Hours for a big tree is normal; the script itself is the safety (verify, then delete).
    let out = crate::util::run(&[&script, &c.path, &dest, &log], None, Duration::from_secs(12 * 3600));
    match out {
        Some(o) if o.ok && !Path::new(&c.path).exists() => {
            let freed = fs_space(&home()).map(|s| s.1).unwrap_or(0).saturating_sub(before);
            crate::act::append_log(&serde_json::json!({
                "t": crate::util::now(), "host": crate::util::hostname(), "action": "archived-delete",
                "cat": c.category, "path": c.path, "remote": dest, "freed_df_bytes": freed,
            }));
            Outcome { state: "done", reason: "archived, checksums verified, source removed".into(), freed, archive_ref: dest }
        }
        Some(o) if o.ok => Outcome { state: "failed", reason: "archive script succeeded but the source is still there".into(), freed: 0, archive_ref: dest },
        Some(_) => Outcome { state: "failed", reason: format!("archive_mismatch or copy failure: see {log}"), freed: 0, archive_ref: dest },
        None => Outcome { state: "failed", reason: "archive script did not finish in 12h".into(), freed: 0, archive_ref: dest },
    }
}

/// Act on one decided candidate.
pub fn one(c: &Cand, action: &str, cx: &Ctx) -> Outcome {
    if let Err(why) = floor(c, action, cx) {
        return refused(why);
    }
    let skipped = |reason: &str| Outcome { state: "skipped", reason: reason.into(), freed: 0, archive_ref: String::new() };
    match action {
        "archive" => match cx.archive {
            Some(a) => archive(c, a, cx.apply),
            None => refused("archive_unconfigured: no archive destination on this host"),
        },
        "delete" if c.plan_ok => match cx.plan {
            Some(p) => delete_plan_item(c, p, cx.apply),
            None => refused("no current plan: scan again"),
        },
        "delete" if !cx.apply => skipped("dry run: would remove (reap floor re-checked)"),
        "delete" => match crate::act::reap_approved(Path::new(&c.path), cx.approval) {
            Ok(freed) => Outcome { state: "done", reason: "removed (approved)".into(), freed, archive_ref: String::new() },
            Err(e) => refused(e),
        },
        _ => skipped("kept"),
    }
}
