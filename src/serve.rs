//! `diskreap serve`: this host's diskreap as a Benchday stream service.
//!
//! Registers over the Benchday daemon's local ingress as the `host-service`
//! authority for four intent contracts and the native producer of three state
//! contracts (Benchday `openspec/changes/disk-cleanup-plugin`, design D1–D6):
//!
//! | contract | role |
//! |---|---|
//! | `diskreap.scan/1` | read-only scan with instructions (`submitted → scanning → proposed`) |
//! | `diskreap.decision/1` | a person's per-item decisions + comments (human only) |
//! | `diskreap.execute/1` | confirmed delete/archive of the decided set (human only) |
//! | `diskreap.policy.edit/1` | edits to this host's rules (human only) |
//! | `diskreap.host/1` | free space, level, version, the current selection |
//! | `diskreap.candidate/1` | the current run's candidates |
//! | `diskreap.policy/1` | protect lists, preferences, learned rules |
//!
//! State partials are not durable, so on every live lease the service republishes
//! from its own files. Intent records are kept in `intents.json` (64 newest) and
//! re-announced after a reconnect.

use crate::candidates::{self, Cand, ItemDecision, Run};
use crate::feedback::{self, Decision, Policy, Rule};
use crate::ingress::{self, Conn, Event};
use crate::instructions::{self, Interp};
use crate::plan::{self, Level, Plan, ScanOpts};
use crate::util::{fs_space, home, now};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SCAN: &str = "diskreap.scan/1";
const DECISION: &str = "diskreap.decision/1";
const EXECUTE: &str = "diskreap.execute/1";
const POLICY_EDIT: &str = "diskreap.policy.edit/1";
const HOST: &str = "diskreap.host/1";
const CANDIDATE: &str = "diskreap.candidate/1";
const POLICY: &str = "diskreap.policy/1";
const INTENTS: &[&str] = &[SCAN, DECISION, EXECUTE, POLICY_EDIT];
const STATES: &[&str] = &[HOST, CANDIDATE, POLICY];
const INTENTS_KEPT: usize = 64;

struct Svc {
    conn: Conn,
    machine: String,
    epochs: BTreeMap<String, u64>,
    seq: AtomicU64,
    intents: Mutex<BTreeMap<String, (String, Value, i64)>>,
    leases: Mutex<HashSet<String>>,
    busy: Mutex<Option<String>>,
    cancel: AtomicBool,
    since_ms: i64,
    published_run: Mutex<Option<(String, Vec<String>)>>,
}

fn log(msg: &str) {
    eprintln!("diskreap serve: {msg}");
}

pub fn run() -> ! {
    let mut backoff = 0.25f64;
    loop {
        let started = std::time::Instant::now();
        match session() {
            Ok(()) => log("session ended"),
            Err(e) => log(&format!("session: {e}")),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 0.25;
        }
        std::thread::sleep(Duration::from_secs_f64(backoff));
        backoff = (backoff * 2.0).min(10.0);
    }
}

fn session() -> Result<(), String> {
    let path = ingress::socket_path().ok_or("no ingress socket path (HOME unset)")?;
    let (conn, events) = Conn::open(&path)?;
    let mut epochs = BTreeMap::new();
    for c in INTENTS.iter().chain(STATES) {
        epochs.insert(c.to_string(), conn.serve(c).map_err(|e| format!("serve {c}: {e}"))?);
    }
    let machine = conn.daemon.clone();
    log(&format!("serving {} contracts for machine {machine}", epochs.len()));
    let svc = Arc::new(Svc {
        conn,
        machine,
        epochs,
        seq: AtomicU64::new(1),
        intents: Mutex::new(load_intents()),
        leases: Mutex::new(HashSet::new()),
        busy: Mutex::new(None),
        cancel: AtomicBool::new(false),
        since_ms: now() * 1000,
        published_run: Mutex::new(None),
    });
    svc.reannounce();
    ensure_latest_run(&svc);
    // Free space changes without us: refresh the host row while someone watches.
    let tick = svc.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let alive2 = alive.clone();
    std::thread::spawn(move || {
        while alive2.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(30));
            if tick.leases.lock().unwrap().contains(HOST) {
                tick.publish_host();
            }
        }
    });
    let result = loop {
        match events.recv() {
            Ok(Event::Lease { contract, live }) => {
                if live {
                    svc.leases.lock().unwrap().insert(contract.clone());
                    svc.republish(&contract);
                } else {
                    svc.leases.lock().unwrap().remove(&contract);
                }
            }
            Ok(Event::Write(frame)) => handle_write(&svc, frame),
            Ok(Event::Down(why)) => break Err(why),
            Err(_) => break Err("event channel closed".into()),
        }
    };
    alive.store(false, Ordering::Relaxed);
    result
}

fn intents_path() -> std::path::PathBuf {
    crate::util::state_dir().join("intents.json")
}

fn load_intents() -> BTreeMap<String, (String, Value, i64)> {
    std::fs::read(intents_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<(String, String, Value, i64)>>(&b).ok())
        .map(|v| v.into_iter().map(|(id, c, b, t)| (id, (c, b, t))).collect())
        .unwrap_or_default()
}

impl Svc {
    fn epoch(&self, contract: &str) -> u64 {
        self.epochs.get(contract).copied().unwrap_or(1)
    }

    /// Mint and record an intent partial; the newest 64 intents are kept on disk.
    fn mint(&self, contract: &str, body: Value) -> Value {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let partial = ingress::intent_partial(contract, &self.machine, self.epoch(contract), seq, &body);
        let id = body["intent_id"].as_str().unwrap_or("").to_string();
        let mut held = self.intents.lock().unwrap();
        held.insert(id, (contract.to_string(), body, now()));
        while held.len() > INTENTS_KEPT {
            let oldest = held.iter().min_by_key(|(_, (_, _, t))| *t).map(|(k, _)| k.clone()).unwrap();
            held.remove(&oldest);
        }
        let rows: Vec<(String, String, Value, i64)> = held.iter().map(|(k, (c, b, t))| (k.clone(), c.clone(), b.clone(), *t)).collect();
        drop(held);
        let _ = std::fs::write(intents_path(), serde_json::to_vec(&rows).unwrap());
        partial
    }

    /// Publish a later rung of an intent (not an answer to a write).
    fn advance(&self, contract: &str, body: Value) {
        let partial = self.mint(contract, body);
        if let Err(e) = self.conn.intent_partial(contract, &partial) {
            log(&format!("intent_partial {contract}: {e}"));
        }
    }

    fn reannounce(&self) {
        let held: Vec<(String, Value)> = self.intents.lock().unwrap().values().map(|(c, b, _)| (c.clone(), b.clone())).collect();
        for (c, b) in held {
            self.advance(&c, b);
        }
    }

    fn publish(&self, contract: &str, subject: Value, body: Value) {
        if let Err(e) = self.conn.publish(contract, subject, body) {
            log(&format!("publish {contract}: {e}"));
        }
    }

    fn republish(&self, contract: &str) {
        match contract {
            HOST => self.publish_host(),
            CANDIDATE => {
                if let Some(run) = Run::latest() {
                    self.publish_run(&run);
                }
            }
            POLICY => self.publish_policy(),
            _ => {}
        }
    }

    fn publish_host(&self) {
        let (total, avail) = fs_space(&home()).unwrap_or((0, 0));
        let level = plan::home_level(total, avail);
        let archive = crate::execute::archive_config();
        // Benchday's diskreap.host/1 schema is closed (additionalProperties: false): the daemon
        // refuses a record with an unknown field. Agree any new field with the owner of the
        // Benchday Disk Cleanup plugin and land it in the schema before adding it here.
        let mut body = json!({
            "state": "present", "machine": self.machine, "version": env!("CARGO_PKG_VERSION"),
            "avail_bytes": avail, "total_bytes": total, "level": level,
            "used_pct": if total > 0 { ((total - avail.min(total)) as f64 * 1000.0 / total as f64).round() / 10.0 } else { 0.0 },
            "target_free_pct": plan::KEEP_FREE_PCT, "low_cap_bytes": plan::LOW_CAP,
            "needs_supervision": crate::util::state_dir().join("needs-supervision.json").exists(),
            "archive": if archive.is_some() { "configured" } else { "unconfigured" },
            "serving_since_ms": self.since_ms,
        });
        if let Some(a) = &archive {
            body["archive_dest_kind"] = json!(crate::execute::archive_kind(a));
        }
        if let Some(p) = Plan::load() {
            body["last_scan_ms"] = json!(p.created * 1000);
        }
        if let Some(run) = Run::latest() {
            let s = run.selection();
            body["latest_run"] = json!(run.id);
            body["selection"] = json!({"run": run.id, "count": s.count, "bytes": s.bytes, "digest": s.digest,
                                        "delete_count": s.delete_count, "archive_count": s.archive_count});
        }
        self.publish(HOST, json!({"machine": self.machine}), body);
    }

    fn candidate_body(&self, run: &Run, c: &Cand) -> Value {
        let mut b = json!({
            "state": "present", "run": run.id, "candidate": c.candidate, "machine": self.machine,
            "path": c.path, "bytes": c.bytes, "newest_s": c.newest_s, "category": c.category, "tier": c.tier,
            "action": c.action, "why": c.why, "regenerable": c.regenerable, "personal": c.personal,
        });
        if let Some(u) = &c.in_use {
            b["in_use"] = json!({"note": u.chars().take(128).collect::<String>()});
        }
        if !c.owner.is_empty() {
            b["owner"] = json!(c.owner.chars().take(160).collect::<String>());
        }
        if !c.rule.is_empty() {
            b["rule"] = json!(c.rule);
        }
        if let Some(d) = run.decisions.get(&c.candidate) {
            b["decision"] = json!({"action": d.action, "comment": d.comment, "at_ms": d.at_ms});
        }
        if let Some(l) = &c.last {
            b["last"] = json!({"action": l.action, "comment": l.comment.chars().take(280).collect::<String>(), "at_ms": l.t_ms});
        }
        b
    }

    fn publish_candidate(&self, run: &Run, c: &Cand, outcome: Option<&Value>) {
        let mut b = self.candidate_body(run, c);
        if let Some(o) = outcome {
            b["outcome"] = o.clone();
        }
        self.publish(CANDIDATE, json!({"run": run.id, "candidate": c.candidate}), b);
    }

    /// Publish a run's candidates; tombstone the previously published run's.
    fn publish_run(&self, run: &Run) {
        let mut prev = self.published_run.lock().unwrap();
        if let Some((old, ids)) = prev.take() {
            if old != run.id {
                for id in ids {
                    self.publish(CANDIDATE, json!({"run": old, "candidate": id}),
                                 json!({"state": "removed", "run": old, "candidate": id, "machine": self.machine}));
                }
            }
        }
        for c in &run.candidates {
            self.publish_candidate(run, c, None);
        }
        *prev = Some((run.id.clone(), run.candidates.iter().map(|c| c.candidate.clone()).collect()));
    }

    fn publish_rule(&self, r: &Rule) {
        let mut m = json!({});
        if !r.path_glob.is_empty() {
            m["path_glob"] = json!(r.path_glob);
        }
        if !r.category.is_empty() {
            m["category"] = json!(r.category);
        }
        self.publish(POLICY, json!({"machine": self.machine, "rule": r.rule}), json!({
            "state": "present", "machine": self.machine, "rule": r.rule, "kind": r.kind, "match": m,
            "origin": r.origin, "enabled": r.enabled, "note": r.note, "hits": r.hits, "created_ms": r.created_ms,
            "sources": r.sources,
        }));
    }

    fn publish_policy(&self) {
        for r in config_rules().iter().chain(Policy::load().rules.iter()) {
            self.publish_rule(r);
        }
    }

    fn refuse(&self, contract: &str, id: &str, code: &str, detail: &str) -> Value {
        self.mint(contract, json!({"intent_id": id, "status": "refused", "machine": self.machine,
                                   "refusal": code, "detail": detail.chars().take(256).collect::<String>()}))
    }
}

/// `protect_paths` from the operator config, shown read-only (origin `config`).
fn config_rules() -> Vec<Rule> {
    let Some(cfg) = crate::util::config_json() else { return vec![] };
    cfg.get("protect_paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(64)
        .map(|g| Rule {
            rule: format!("cfg-{}", &crate::sha::hex(g.as_bytes())[..12]),
            kind: "protect".into(),
            path_glob: g.chars().take(512).collect(),
            category: String::new(),
            origin: "config".into(),
            enabled: true,
            note: "protect_paths in ~/.config/diskreap/config.json".into(),
            hits: 0,
            created_ms: 0,
            sources: vec![],
        })
        .collect()
}

/// Make the host's latest plan.json visible as a run when no supervised run is
/// newer (the timer, the weekly report pass and CLI scans all write plan.json).
fn ensure_latest_run(svc: &Svc) {
    let Some(plan) = Plan::load() else { return };
    if Run::latest().is_some_and(|r| r.plan_created >= plan.created) {
        return;
    }
    let id = format!("plan-{}", plan.created);
    let run = build_run(svc, &id, &plan, &Interp::default());
    if run.save().is_ok() {
        feedback::prune_runs();
    }
}

fn build_run(svc: &Svc, id: &str, plan: &Plan, interp: &Interp) -> Run {
    let mut policy = Policy::load();
    let last = feedback::last_by_path(&feedback::history());
    let (cands, truncated, hits) = candidates::project(&candidates::Inputs {
        plan,
        machine: &svc.machine,
        interp,
        policy: &policy,
        last: &last,
        archive_configured: crate::execute::archive_config().is_some(),
        now: now(),
    });
    if !hits.is_empty() {
        for r in policy.rules.iter_mut() {
            if let Some(h) = hits.get(&r.rule) {
                r.hits += h;
            }
        }
        let _ = policy.save();
    }
    Run {
        id: id.into(),
        machine: svc.machine.clone(),
        created_ms: now() * 1000,
        plan_created: plan.created,
        interp: interp.clone(),
        candidates: cands,
        decisions: BTreeMap::new(),
        truncated,
    }
}

fn handle_write(svc: &Arc<Svc>, frame: Value) {
    let req = frame["req"].as_str().unwrap_or("").to_string();
    let contract = frame["contract"].as_str().unwrap_or("").to_string();
    let id = frame["intent_id"].as_str().unwrap_or("").to_string();
    let op = frame["op"].as_str().unwrap_or("");
    let principal = frame["principal"].as_str().unwrap_or("").to_string();
    let body = frame["payload"]["body"].clone();
    let answer = |partial: Value| {
        if let Err(e) = svc.conn.answer(&req, &partial) {
            log(&format!("write_result: {e}"));
        }
    };
    // Idempotent on (contract, intent_id): a repeat gets the current partial.
    if let Some((c, b, _)) = svc.intents.lock().unwrap().get(&id).cloned() {
        if c == contract && op == "submit" {
            let seq = svc.seq.fetch_add(1, Ordering::Relaxed);
            return answer(ingress::intent_partial(&c, &svc.machine, svc.epoch(&c), seq, &b));
        }
    }
    if id.is_empty() || id.len() > 64 {
        return answer(svc.refuse(&contract, &id, "schema_invalid", "intent_id must be 1..64 characters"));
    }
    match op {
        "submit" => {}
        "cancel" => return answer(cancel(svc, &contract, &id)),
        _ => return answer(svc.refuse(&contract, &id, "step_not_allowed", "only the authority advances this intent")),
    }
    if body["machine"].as_str() != Some(svc.machine.as_str()) {
        return answer(svc.refuse(&contract, &id, "wrong_machine", &format!("this service is {}", svc.machine)));
    }
    // Belt and braces over the contract's own `writers`: a person, through the hub.
    if contract != SCAN && !principal.starts_with("user:") {
        return answer(svc.refuse(&contract, &id, "writer_denied", "only a person may change the disk or the policy"));
    }
    let partial = match contract.as_str() {
        SCAN => scan(svc, &id, &body),
        DECISION => decide(svc, &id, &body, &principal),
        EXECUTE => execute(svc, &id, &body, &principal),
        POLICY_EDIT => policy_edit(svc, &id, &body),
        _ => svc.refuse(&contract, &id, "contract_unknown", "not served here"),
    };
    answer(partial);
}

fn cancel(svc: &Svc, contract: &str, id: &str) -> Value {
    let held = svc.intents.lock().unwrap().get(id).cloned();
    let Some((c, b, _)) = held else { return svc.refuse(contract, id, "absent", "no such intent") };
    let status = b["status"].as_str().unwrap_or("");
    let cancellable = (c == SCAN && matches!(status, "submitted" | "scanning")) || (c == EXECUTE && matches!(status, "submitted" | "verifying"));
    if !cancellable {
        return svc.refuse(contract, id, "cancel_too_late", &format!("intent is {status}"));
    }
    svc.cancel.store(true, Ordering::Relaxed);
    let mut nb = b.clone();
    nb["status"] = json!("cancelled");
    svc.mint(&c, nb)
}

fn take_busy(svc: &Svc, what: &str) -> Result<(), String> {
    let mut b = svc.busy.lock().unwrap();
    if let Some(cur) = b.as_ref() {
        return Err(format!("{cur} is running"));
    }
    *b = Some(what.into());
    svc.cancel.store(false, Ordering::Relaxed);
    Ok(())
}

fn scan(svc: &Arc<Svc>, id: &str, body: &Value) -> Value {
    let structured: Interp = serde_json::from_value(body["instructions"].clone()).unwrap_or_default();
    let text: String = body["instructions"]["text"].as_str().unwrap_or("").chars().take(1000).collect();
    let full = body["full"].as_bool().unwrap_or(false);
    if let Err(why) = take_busy(svc, &format!("scan {id}")) {
        return svc.refuse(SCAN, id, "busy", &why);
    }
    let accepted = svc.mint(SCAN, json!({"intent_id": id, "status": "submitted", "machine": svc.machine, "run": id, "full": full}));
    let (svc2, id2) = (svc.clone(), id.to_string());
    std::thread::spawn(move || {
        scan_worker(&svc2, &id2, structured, &text, full);
        *svc2.busy.lock().unwrap() = None;
    });
    accepted
}

fn scan_worker(svc: &Arc<Svc>, id: &str, structured: Interp, text: &str, full: bool) {
    let started = now() * 1000;
    let use_llm = crate::util::config_json().and_then(|c| c.get("llm").and_then(Value::as_bool)).unwrap_or(true);
    let i = instructions::interpret(structured, text, &svc.machine, use_llm);
    let mut body = json!({
        "intent_id": id, "status": "scanning", "machine": svc.machine, "run": id, "full": full,
        "interpreted": i.interp, "unparsed": i.unparsed, "interpreter": i.interpreter,
        "progress": if full { "full scan" } else { "quick scan" }, "started_ms": started,
    });
    svc.advance(SCAN, body.clone());
    // The timer may be scanning: wait for it rather than racing it (bounded).
    let mut lock = None;
    for _ in 0..600 {
        if svc.cancel.load(Ordering::Relaxed) {
            return;
        }
        lock = crate::util::try_lock("scan.lock");
        if lock.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if lock.is_none() {
        body["status"] = json!("failed");
        body["refusal"] = json!("busy");
        body["detail"] = json!("another diskreap scan held the lock for 10 minutes");
        return svc.advance(SCAN, body);
    }
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    let level = plan::home_level(t, a).max(Level::Low);
    let plan = plan::scan(&ScanOpts { full, level, root: None, threads: 32, review: true });
    plan.save();
    drop(lock);
    if svc.cancel.load(Ordering::Relaxed) {
        return;
    }
    let run = build_run(svc, id, &plan, &i.interp);
    if let Err(e) = run.save() {
        body["status"] = json!("failed");
        body["refusal"] = json!("state_unwritable");
        body["detail"] = json!(e.chars().take(200).collect::<String>());
        return svc.advance(SCAN, body);
    }
    feedback::prune_runs();
    body["progress"] = json!(format!("publishing {} candidates", run.candidates.len()));
    body["candidates"] = json!(run.candidates.len());
    svc.advance(SCAN, body.clone());
    svc.publish_run(&run);
    let s = run.selection();
    let proposed: u64 = run.candidates.iter().filter(|c| c.action != "keep").map(|c| c.bytes).sum();
    body["status"] = json!("proposed");
    body["progress"] = json!("done");
    body["reclaimable_bytes"] = json!(proposed);
    body["truncated"] = json!(run.truncated);
    body["avail_bytes"] = json!(plan.avail);
    body["total_bytes"] = json!(plan.total);
    body["selection"] = json!({"count": s.count, "bytes": s.bytes, "digest": s.digest});
    body["finished_ms"] = json!(now() * 1000);
    svc.advance(SCAN, body);
    svc.publish_host();
    svc.publish_policy();
}

fn decide(svc: &Svc, id: &str, body: &Value, principal: &str) -> Value {
    let run_id = body["run"].as_str().unwrap_or("");
    let Some(mut run) = Run::latest().filter(|r| r.id == run_id) else {
        return svc.refuse(DECISION, id, "stale_run", "decisions apply to this host's latest run: reload");
    };
    let items = body["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() || items.len() > 32 {
        return svc.refuse(DECISION, id, "schema_invalid", "1..32 items");
    }
    let mut policy = Policy::load();
    let mut changed_rules = vec![];
    let mut touched = vec![];
    let t = now() * 1000;
    for it in &items {
        let cid = it["candidate"].as_str().unwrap_or("");
        let action = it["action"].as_str().unwrap_or("");
        if !matches!(action, "delete" | "archive" | "keep" | "protect") {
            return svc.refuse(DECISION, id, "schema_invalid", "action must be delete, archive, keep or protect");
        }
        let Some(c) = run.get(cid).cloned() else {
            return svc.refuse(DECISION, id, "candidate_unknown", cid);
        };
        let comment: String = it["comment"].as_str().unwrap_or("").chars().take(280).collect();
        let remember = it["remember"].as_str().unwrap_or("none").to_string();
        let recorded_action = if action == "protect" { "keep" } else { action };
        run.decisions.insert(c.candidate.clone(), ItemDecision { action: recorded_action.into(), comment: comment.clone(), at_ms: t });
        let d = Decision {
            t_ms: t, run: run.id.clone(), path: c.path.clone(), category: c.category.clone(), action: action.into(),
            comment, remember: if action == "protect" && remember == "none" { "path".into() } else { remember },
            by: principal.chars().take(128).collect(),
        };
        if let Err(e) = feedback::record(&d) {
            return svc.refuse(DECISION, id, "state_unwritable", &e);
        }
        if let Some(r) = feedback::remembered_rule(&d) {
            if let Err(e) = policy.upsert(r.clone()) {
                return svc.refuse(DECISION, id, &e, "at most 256 rules: delete some in the Rules tab");
            }
            changed_rules.push(r);
        }
        touched.push(c);
    }
    for r in feedback::learned(&feedback::history(), &policy) {
        if policy.upsert(r.clone()).is_ok() {
            changed_rules.push(r);
        }
    }
    if let Err(e) = policy.save().and_then(|_| run.save()) {
        return svc.refuse(DECISION, id, "state_unwritable", &e);
    }
    let answer = svc.mint(DECISION, json!({"intent_id": id, "status": "recorded", "machine": svc.machine, "run": run.id,
                                            "recorded": touched.len(), "rules_created": changed_rules.len()}));
    for c in &touched {
        svc.publish_candidate(&run, c, None);
    }
    for r in &changed_rules {
        svc.publish_rule(r);
    }
    svc.publish_host();
    answer
}

fn execute(svc: &Arc<Svc>, id: &str, body: &Value, principal: &str) -> Value {
    let run_id = body["run"].as_str().unwrap_or("");
    let Some(run) = Run::latest().filter(|r| r.id == run_id) else {
        return svc.refuse(EXECUTE, id, "stale_run", "execute names this host's latest run: reload");
    };
    let s = run.selection();
    let e = &body["expect"];
    if e["digest"].as_str() != Some(s.digest.as_str()) || e["count"].as_u64() != Some(s.count) || e["bytes"].as_u64() != Some(s.bytes) {
        return svc.refuse(EXECUTE, id, "selection_changed", "the decided set changed after review: review again");
    }
    if s.count == 0 {
        return svc.refuse(EXECUTE, id, "nothing_selected", "no item is decided delete or archive");
    }
    if let Err(why) = take_busy(svc, &format!("execute {id}")) {
        return svc.refuse(EXECUTE, id, "busy", &why);
    }
    let dry = body["dry_run"].as_bool().unwrap_or(false);
    let accepted = svc.mint(EXECUTE, json!({"intent_id": id, "status": "submitted", "machine": svc.machine, "run": run.id,
                                             "dry_run": dry, "total": s.count, "expect": e}));
    let (svc2, id2, approval) = (svc.clone(), id.to_string(), format!("benchday {principal} run={} intent={id}", run.id));
    std::thread::spawn(move || {
        execute_worker(&svc2, &id2, run, dry, &approval);
        *svc2.busy.lock().unwrap() = None;
    });
    accepted
}

fn execute_worker(svc: &Arc<Svc>, id: &str, mut run: Run, dry: bool, approval: &str) {
    let lock = crate::util::try_lock("clean.lock");
    let avail = || fs_space(&home()).map(|s| s.1).unwrap_or(0);
    let mut body = json!({"intent_id": id, "status": "verifying", "machine": svc.machine, "run": run.id, "dry_run": dry,
                          "total": run.selection().count, "done": 0, "failed": 0, "skipped": 0, "freed_bytes": 0,
                          "archived_bytes": 0, "avail_before": avail()});
    if lock.is_none() {
        body["status"] = json!("failed");
        body["refusal"] = json!("busy");
        body["detail"] = json!("another diskreap clean is running");
        return svc.advance(EXECUTE, body);
    }
    svc.advance(EXECUTE, body.clone());
    let plan = Plan::load().filter(|p| p.created == run.plan_created);
    let archive = crate::execute::archive_config();
    let home = home();
    let inuse = crate::procs::InUse::snapshot();
    let refs = crate::procs::Refs::collect(&home);
    let policy = Policy::load();
    let cx = crate::execute::Ctx { plan: plan.as_ref(), archive: archive.as_ref(), approval, apply: !dry, inuse: &inuse, refs: &refs, policy: &policy };
    // Archive first: what has no other copy is safest moved before anything else is touched.
    let mut todo: Vec<(Cand, String)> = run
        .decisions
        .iter()
        .filter(|(_, d)| d.action == "delete" || d.action == "archive")
        .filter_map(|(cid, d)| run.get(cid).map(|c| (c.clone(), d.action.clone())))
        .collect();
    todo.sort_by_key(|(c, a)| (a != "archive", std::cmp::Reverse(c.bytes)));
    if svc.cancel.load(Ordering::Relaxed) {
        return;
    }
    body["status"] = json!("executing");
    for (c, action) in todo {
        body["current"] = json!(c.path.chars().take(512).collect::<String>());
        svc.advance(EXECUTE, body.clone());
        let pending = json!({"state": "running", "reason": format!("{action} in progress")});
        svc.publish_candidate(&run, &c, Some(&pending));
        let o = crate::execute::one(&c, &action, &cx);
        let key = match o.state { "done" => "done", "skipped" => "skipped", _ => "failed" };
        body[key] = json!(body[key].as_u64().unwrap_or(0) + 1);
        if o.state == "done" {
            let k = if action == "archive" { "archived_bytes" } else { "freed_bytes" };
            body[k] = json!(body[k].as_u64().unwrap_or(0) + if action == "archive" { c.bytes } else { o.freed });
            if action == "archive" {
                body["freed_bytes"] = json!(body["freed_bytes"].as_u64().unwrap_or(0) + o.freed);
            }
        }
        let mut out = json!({"state": o.state, "reason": o.reason.chars().take(200).collect::<String>(), "freed_bytes": o.freed});
        if !o.archive_ref.is_empty() {
            out["archive_ref"] = json!(o.archive_ref.chars().take(512).collect::<String>());
        }
        svc.publish_candidate(&run, &c, Some(&out));
        if o.state == "done" && !dry {
            run.decisions.remove(&c.candidate);
        }
    }
    let _ = run.save();
    body["status"] = json!("done");
    body["current"] = json!("");
    body["avail_after"] = json!(avail());
    svc.advance(EXECUTE, body);
    svc.publish_host();
}

fn policy_edit(svc: &Svc, id: &str, body: &Value) -> Value {
    let op = body["op"].as_str().unwrap_or("");
    let rule_id = body["rule"].as_str().unwrap_or("").to_string();
    if rule_id.starts_with("cfg-") {
        return svc.refuse(POLICY_EDIT, id, "config_rule", "edit protect_paths in ~/.config/diskreap/config.json");
    }
    let mut policy = Policy::load();
    let applied = |rule: &str| svc.mint(POLICY_EDIT, json!({"intent_id": id, "status": "applied", "machine": svc.machine, "rule": rule}));
    match op {
        "enable" | "disable" | "delete" => {
            let Some(pos) = policy.rules.iter().position(|r| r.rule == rule_id) else {
                return svc.refuse(POLICY_EDIT, id, "rule_unknown", &rule_id);
            };
            if op == "delete" {
                policy.rules.remove(pos);
            } else {
                policy.rules[pos].enabled = op == "enable";
            }
            if let Err(e) = policy.save() {
                return svc.refuse(POLICY_EDIT, id, "state_unwritable", &e);
            }
            let answer = applied(&rule_id);
            if op == "delete" {
                svc.publish(POLICY, json!({"machine": svc.machine, "rule": rule_id}),
                            json!({"state": "removed", "machine": svc.machine, "rule": rule_id}));
            } else {
                svc.publish_rule(&policy.rules[pos]);
            }
            answer
        }
        "upsert" => {
            let kind = body["kind"].as_str().unwrap_or("");
            if !matches!(kind, "protect" | "keep" | "archive" | "delete" | "category_off") {
                return svc.refuse(POLICY_EDIT, id, "schema_invalid", "kind");
            }
            let glob: String = body["match"]["path_glob"].as_str().unwrap_or("").chars().take(512).collect();
            let cat: String = body["match"]["category"].as_str().unwrap_or("").chars().take(32).collect();
            if glob.is_empty() && cat.is_empty() {
                return svc.refuse(POLICY_EDIT, id, "schema_invalid", "a rule needs a path glob or a category");
            }
            if !glob.is_empty() && glob::Pattern::new(&crate::util::expand_tilde(&glob).to_string_lossy()).is_err() {
                return svc.refuse(POLICY_EDIT, id, "schema_invalid", "bad glob");
            }
            let mut rule = policy.rules.iter().find(|r| r.rule == rule_id).cloned().unwrap_or(Rule {
                rule: feedback::rule_id(kind, &glob, &cat), kind: kind.into(), path_glob: String::new(), category: String::new(),
                origin: "user".into(), enabled: true, note: String::new(), hits: 0, created_ms: now() * 1000, sources: vec![],
            });
            rule.kind = kind.into();
            rule.path_glob = glob;
            rule.category = cat;
            rule.enabled = true;
            if rule.origin == "learned" {
                rule.origin = "user".into(); // a person edited it: it is theirs now
            }
            if let Some(n) = body["note"].as_str() {
                rule.note = n.chars().take(280).collect();
            }
            if let Err(e) = policy.upsert(rule.clone()).and_then(|_| policy.save()) {
                return svc.refuse(POLICY_EDIT, id, &e, "could not save the rule");
            }
            let answer = applied(&rule.rule);
            svc.publish_rule(&rule);
            answer
        }
        _ => svc.refuse(POLICY_EDIT, id, "schema_invalid", "op must be upsert, delete, enable or disable"),
    }
}
