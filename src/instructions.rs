//! Per-run instructions for a supervised scan: structured fields plus free text.
//!
//! The one rule: instructions only NARROW. They remove candidates or lower a
//! proposed action (delete → archive → keep); they can never make diskreap
//! consider a path its safety floor excludes. Free text is read by a
//! deterministic clause parser first; only the clauses it does not understand go
//! to an LLM (a Harmony router, when one answers), which must answer with the
//! same structured fields. What was understood, what was not, and which
//! interpreter ran are all returned so the person sees them before deciding.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub const CATEGORIES: &[&str] = &["docker", "log", "tmp", "build-output", "cache", "offloaded", "worktree", "backup", "report"];

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Interp {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only_categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_globs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_age_days: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_free_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_only: bool,
}

pub struct Interpretation {
    pub interp: Interp,
    pub unparsed: Vec<String>,
    pub interpreter: String,
}

/// Merge `other` into `self`, keeping the narrower value of every field.
fn narrow(a: &mut Interp, b: Interp) {
    for c in b.only_categories {
        if !a.only_categories.contains(&c) {
            a.only_categories.push(c);
        }
    }
    for g in b.exclude_globs {
        if !a.exclude_globs.contains(&g) {
            a.exclude_globs.push(g);
        }
    }
    for g in b.include_globs {
        if !a.include_globs.contains(&g) {
            a.include_globs.push(g);
        }
    }
    a.min_age_days = a.min_age_days.max(b.min_age_days);
    a.min_bytes = a.min_bytes.max(b.min_bytes);
    // A lower target frees less: narrower.
    a.target_free_pct = match (a.target_free_pct, b.target_free_pct) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    };
    a.report_only |= b.report_only;
}

/// Clamp untrusted structured input to the bounds the contract declares.
pub fn sanitize(mut i: Interp) -> Interp {
    i.only_categories.retain(|c| CATEGORIES.contains(&c.as_str()));
    i.only_categories.truncate(9);
    for v in [&mut i.exclude_globs, &mut i.include_globs] {
        v.retain(|g| !g.is_empty() && g.len() <= 256 && glob::Pattern::new(&crate::util::expand_tilde(g).to_string_lossy()).is_ok());
        v.truncate(8);
    }
    i.min_age_days = i.min_age_days.map(|d| d.clamp(0, 3650));
    i.target_free_pct = i.target_free_pct.filter(|p| (1.0..=95.0).contains(p));
    i
}

fn category_word(w: &str) -> Option<&'static str> {
    Some(match w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
        "cache" | "caches" => "cache",
        "docker" => "docker",
        "log" | "logs" => "log",
        "tmp" | "temp" | "temporary" => "tmp",
        "build" | "builds" | "build-output" | "node_modules" | "target" | "artifacts" => "build-output",
        "worktree" | "worktrees" => "worktree",
        "backup" | "backups" => "backup",
        "offloaded" => "offloaded",
        _ => return None,
    })
}

fn age_days(n: i64, unit: &str) -> Option<i64> {
    let u = unit.trim_end_matches('s');
    Some(match u {
        "d" | "day" => n,
        "w" | "wk" | "week" => n * 7,
        "m" | "mo" | "month" => n * 30,
        "y" | "yr" | "year" => n * 365,
        _ => return None,
    })
}

/// "30d", "30 days", "2 weeks" starting at token i → days and tokens consumed.
fn duration_at(t: &[&str], i: usize) -> Option<(i64, usize)> {
    let tok = t.get(i)?;
    let split = tok.find(|c: char| !c.is_ascii_digit()).unwrap_or(tok.len());
    let n: i64 = tok[..split].parse().ok()?;
    if split < tok.len() {
        return Some((age_days(n, &tok[split..])?, 1));
    }
    Some((age_days(n, t.get(i + 1)?)?, 2))
}

/// "5G", "500 MB", "1.5 GiB" → bytes.
fn size_at(t: &[&str], i: usize) -> Option<u64> {
    let tok = t.get(i)?;
    let split = tok.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(tok.len());
    let n: f64 = tok[..split].parse().ok()?;
    let unit = if split < tok.len() { tok[split..].to_string() } else { t.get(i + 1)?.to_string() };
    let mult = match unit.trim_end_matches('b').trim_end_matches("i") {
        "k" => 1u64 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    Some((n * mult as f64) as u64)
}

fn path_token(t: &str) -> Option<String> {
    let t = t.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == ',' || c == '.');
    (t.starts_with('~') || t.starts_with('/')).then(|| t.trim_end_matches('/').to_string())
}

/// Parse one clause. None = not understood.
fn clause(c: &str, machine: &str, out: &mut Interp) -> Option<()> {
    let lower = c.to_lowercase();
    let t: Vec<&str> = lower.split_whitespace().collect();
    let raw: Vec<&str> = c.split_whitespace().collect(); // paths keep their case
    if t.is_empty() {
        return Some(());
    }
    let has = |w: &str| t.iter().any(|x| *x == w);
    let mut understood = false;
    // Paths: "never touch X", "skip X", "keep X", "protect X", "leave X alone", "exclude X".
    let protective = ["never", "skip", "exclude", "keep", "protect", "leave", "don't", "dont", "not", "without", "except"];
    let paths: Vec<String> = raw.iter().filter_map(|w| path_token(w)).collect();
    if !paths.is_empty() {
        let only = has("only") && (has("in") || has("under") || has("inside"));
        for p in &paths {
            if only {
                out.include_globs.push(format!("{p}/**"));
                out.include_globs.push(p.clone());
            } else if t.iter().any(|w| protective.contains(w)) {
                out.exclude_globs.push(p.clone());
                out.exclude_globs.push(format!("{p}/**"));
            } else {
                return None; // a path with no verb we trust: ask, don't guess
            }
        }
        understood = true;
    }
    // "on <machine>"
    if let Some(i) = t.iter().position(|w| *w == "on") {
        if let Some(m) = t.get(i + 1) {
            let m = m.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
            if m.contains('-') || m.chars().any(|c| c.is_ascii_digit()) {
                if !m.eq_ignore_ascii_case(machine) {
                    return None; // names another machine; reported in unparsed, never obeyed elsewhere
                }
                understood = true;
            }
        }
    }
    // Categories: "only caches", "just docker and logs".
    if has("only") || has("just") || t.first().is_some_and(|w| category_word(w).is_some()) {
        let cats: Vec<&str> = t.iter().filter_map(|w| category_word(w)).collect();
        if !cats.is_empty() && paths.is_empty() {
            for c in cats {
                if !out.only_categories.iter().any(|x| x == c) {
                    out.only_categories.push(c.to_string());
                }
            }
            understood = true;
        }
    }
    // Age: "older than 30d", "not used in 2 weeks", "untouched for 90 days".
    for (i, w) in t.iter().enumerate() {
        if matches!(*w, "than" | "in" | "for" | "over") {
            if let Some((d, _)) = duration_at(&t, i + 1) {
                if !(has("bigger") || has("larger")) {
                    out.min_age_days = out.min_age_days.max(Some(d));
                    understood = true;
                }
            }
        }
    }
    // Size: "bigger than 5G", "at least 1 GB".
    if has("bigger") || has("larger") || (has("at") && has("least")) {
        if let Some(i) = t.iter().position(|w| matches!(*w, "than" | "least")) {
            if let Some(b) = size_at(&t, i + 1) {
                out.min_bytes = out.min_bytes.max(Some(b));
                understood = true;
            }
        }
    }
    // Target: "free 20%", "target 25%", "until 30% free".
    if let Some(p) = t.iter().find_map(|w| w.strip_suffix('%').and_then(|n| n.parse::<f64>().ok())) {
        if has("free") || has("target") || has("until") || has("get") {
            out.target_free_pct = Some(p);
            understood = true;
        }
    }
    // Report only.
    if lower.contains("report only")
        || lower.contains("dry run")
        || lower.contains("just show")
        || lower.contains("don't delete")
        || lower.contains("do not delete")
        || lower.contains("nothing deleted")
    {
        out.report_only = true;
        understood = true;
    }
    understood.then_some(())
}

fn clauses(text: &str) -> Vec<String> {
    let mut out = vec![];
    for part in text.split(|c| c == ';' || c == '\n') {
        for sub in part.split(", ") {
            for piece in sub.split(" and ") {
                let p = piece.trim().trim_end_matches('.').trim();
                if !p.is_empty() {
                    out.push(p.chars().take(200).collect());
                }
            }
        }
    }
    out
}

pub fn parse_rules(text: &str, machine: &str) -> (Interp, Vec<String>) {
    let mut interp = Interp::default();
    let mut unparsed = vec![];
    for c in clauses(text) {
        let mut one = Interp::default();
        if clause(&c, machine, &mut one).is_some() {
            narrow(&mut interp, one);
        } else {
            unparsed.push(c);
        }
    }
    (interp, unparsed)
}

/// Ask a Harmony router to read the clauses the parser did not understand.
fn llm(clauses: &[String], machine: &str) -> Result<Interp, String> {
    let base = crate::util::config_json()
        .and_then(|c| c.get("llm_base_url").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "http://127.0.0.1:8188/v1".into());
    let system = format!(
        "You turn disk-cleanup instructions for machine {machine} into JSON. Answer ONLY a JSON object with any of: \
         only_categories (subset of {CATEGORIES:?}), exclude_globs (paths/globs never to touch, ~ allowed), \
         include_globs (only consider these), min_age_days (integer), min_bytes (integer), target_free_pct (1-95), \
         report_only (boolean). Omit what the text does not say. Never invent paths."
    );
    let body = json!({
        "model": "require:class=llm",
        "temperature": 0,
        "max_tokens": 400,
        "chat_template_kwargs": {"enable_thinking": false},
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": clauses.join("\n")}],
    });
    let out = crate::util::run(
        &[
            "curl", "-sS", "--max-time", "25", "-H", "content-type: application/json", "-H", "x-harmony-owner: diskreap",
            "-d", &body.to_string(), &format!("{}/chat/completions", base.trim_end_matches('/')),
        ],
        None,
        Duration::from_secs(30),
    )
    .ok_or("router did not answer in 30s")?;
    if !out.ok {
        return Err(format!("no router at {base}"));
    }
    let v: Value = serde_json::from_str(&out.stdout).map_err(|_| "router answered non-JSON".to_string())?;
    let text = v["choices"][0]["message"]["content"].as_str().ok_or("router answer has no content")?;
    let start = text.find('{').ok_or("model answered no JSON object")?;
    let end = text.rfind('}').ok_or("model answered no JSON object")?;
    serde_json::from_str::<Interp>(&text[start..=end]).map_err(|e| format!("model JSON: {e}"))
}

/// Interpret structured instructions plus text. The LLM pass runs only when the
/// parser left something over; its absence is named, never silent.
pub fn interpret(structured: Interp, text: &str, machine: &str, use_llm: bool) -> Interpretation {
    let mut interp = sanitize(structured);
    let (parsed, mut unparsed) = parse_rules(text, machine);
    narrow(&mut interp, parsed);
    let mut interpreter = "rules".to_string();
    if !unparsed.is_empty() && use_llm {
        match llm(&unparsed, machine) {
            Ok(extra) => {
                narrow(&mut interp, sanitize(extra));
                interpreter = "rules+llm".into();
                unparsed.clear();
            }
            Err(e) => interpreter = format!("rules (llm unavailable: {})", e.chars().take(80).collect::<String>()),
        }
    }
    unparsed.truncate(8);
    Interpretation { interp: sanitize(interp), unparsed, interpreter }
}

impl Interp {
    fn matches(globs: &[String], path: &str) -> bool {
        globs.iter().any(|g| {
            let g = crate::util::expand_tilde(g);
            glob::Pattern::new(&g.to_string_lossy()).is_ok_and(|p| p.matches(path))
                || std::path::Path::new(path).starts_with(&g)
        })
    }

    /// Does this candidate survive the instructions at all?
    pub fn admits(&self, category: &str, path: &str, bytes: u64, newest: i64, now: i64) -> Result<(), String> {
        if !self.only_categories.is_empty() && !self.only_categories.iter().any(|c| c == category) {
            return Err(format!("instructions: only {}", self.only_categories.join(", ")));
        }
        if Self::matches(&self.exclude_globs, path) {
            return Err("instructions: excluded path".into());
        }
        if !self.include_globs.is_empty() && !Self::matches(&self.include_globs, path) {
            return Err("instructions: outside the included paths".into());
        }
        if let Some(d) = self.min_age_days {
            // An unknown age is not old enough: the narrower reading.
            if newest <= 0 || now - newest < d * 86400 {
                return Err(format!("instructions: newer than {d}d"));
            }
        }
        if self.min_bytes.is_some_and(|b| bytes < b) {
            return Err("instructions: smaller than the minimum".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_examples_parse_without_an_llm() {
        let (i, u) = parse_rules("only caches older than 30d on zz-tower0", "zz-tower0");
        assert_eq!(i.only_categories, vec!["cache"]);
        assert_eq!(i.min_age_days, Some(30));
        assert!(u.is_empty(), "{u:?}");
        let (i, u) = parse_rules("never touch ~/histo-reader-archive", "zz-tower2");
        assert!(i.exclude_globs.contains(&"~/histo-reader-archive".to_string()));
        assert!(i.exclude_globs.contains(&"~/histo-reader-archive/**".to_string()));
        assert!(u.is_empty());
        let (i, _) = parse_rules("free 25%; bigger than 2G; report only", "x");
        assert_eq!(i.target_free_pct, Some(25.0));
        assert_eq!(i.min_bytes, Some(2 << 30));
        assert!(i.report_only);
    }

    #[test]
    fn another_machine_or_an_unknown_verb_is_reported_not_obeyed() {
        let (i, u) = parse_rules("only caches on zz-tower2", "zz-tower0");
        assert_eq!(u, vec!["only caches on zz-tower2"]);
        assert_eq!(i, Interp::default());
        let (i, u) = parse_rules("delete ~/project including tracked files", "x");
        assert_eq!(i, Interp::default());
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn merging_keeps_the_narrower_value() {
        let structured = Interp { min_age_days: Some(60), target_free_pct: Some(30.0), ..Default::default() };
        let r = interpret(structured, "older than 7 days; target 20%", "x", false);
        assert_eq!(r.interp.min_age_days, Some(60));
        assert_eq!(r.interp.target_free_pct, Some(20.0));
        assert_eq!(r.interpreter, "rules");
    }

    #[test]
    fn admits_only_narrows() {
        let i = Interp { exclude_globs: vec!["/h/keep/**".into()], min_age_days: Some(30), ..Default::default() };
        let now = 100 * 86400;
        assert!(i.admits("cache", "/h/keep/x", 1, 1, now).is_err());
        assert!(i.admits("cache", "/h/other", 1, now - 10 * 86400, now).is_err());
        assert!(i.admits("cache", "/h/other", 1, 0, now).is_err(), "unknown age is not old");
        assert!(i.admits("cache", "/h/other", 1, now - 40 * 86400, now).is_ok());
    }
}
