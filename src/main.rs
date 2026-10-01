mod act;
mod git;
mod mounts;
mod plan;
mod platform;
mod procs;
mod rules;
mod util;
mod walk;

use clap::{Parser, Subcommand};
use plan::{level_of, target_avail, Level, Plan, ScanOpts};
use std::path::PathBuf;
use util::{ago, fs_space, home, human, now};

/// Hang-proof disk scanner and safe, categorized cleaner.
///
/// Two stages: `scan` (read-only; quick by default, --full for everything)
/// writes a plan; `clean` re-verifies each item and acts only with --apply.
/// `auto` does both when the disk is low (run it from a timer).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Free space and pressure level. --hook prints only when space is low (for agent session hooks).
    Status {
        #[arg(long)]
        hook: bool,
        #[arg(long)]
        json: bool,
    },
    /// Stage 1: find what can be reclaimed, write the plan. Quick by default.
    Scan {
        /// Complete walk: deep candidates, cold-data report, largest dirs.
        #[arg(long)]
        full: bool,
        /// Policy to judge with (default: from current free space, at least `low`).
        #[arg(long)]
        level: Option<Level>,
        #[arg(long)]
        json: bool,
        /// Show skipped candidates too.
        #[arg(long, short)]
        verbose: bool,
    },
    /// Stage 2: act on the last plan. Dry run unless --apply.
    Clean {
        #[arg(long)]
        apply: bool,
        /// Only these categories: docker, log, tmp, build-output, cache, worktree.
        #[arg(long = "category", short = 'c')]
        categories: Vec<String>,
        /// Print one JSON result (actions, estimates, measured df gain).
        #[arg(long)]
        json: bool,
    },
    /// Remove ONE report-only path a human approved (supervised mode). Keeps the
    /// safety floor: inside $HOME, not a repository, not in use, not referenced.
    Reap {
        path: PathBuf,
        /// Who approved it and why (recorded in the audit log).
        #[arg(long)]
        approval: String,
        #[arg(long)]
        json: bool,
    },
    /// Timer entry point: if free space is low, quick scan + clean until the
    /// target is reached, escalating to a full scan if needed.
    Auto {
        #[arg(long)]
        dry_run: bool,
        /// Act even when space is fine (uses the current level's policy).
        #[arg(long)]
        force: bool,
        /// Act only at this pressure level or worse: `critical` for an hourly
        /// guard, `ok` for a daily maintenance run (Ok uses the conservative policy).
        #[arg(long, default_value = "low")]
        min_level: Level,
    },
    /// Hang-proof `du`: sizes of PATH's subdirectories, skipping other mounts.
    Du {
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        depth: usize,
        #[arg(long, default_value_t = 30)]
        top: usize,
    },
}

const THREADS: usize = 32;

fn main() {
    if platform::is_privileged() && std::env::var_os("DISKREAP_ALLOW_ROOT").is_none() {
        eprintln!("diskreap: refusing to run as root (it cleans the invoking user's files)");
        std::process::exit(2);
    }
    match Cli::parse().cmd {
        Cmd::Status { hook, json } => status(hook, json),
        Cmd::Scan {
            full,
            level,
            json,
            verbose,
        } => {
            let _lock = lock_or_exit("scan.lock");
            let lvl = level.unwrap_or_else(current_level);
            let plan = plan::scan(&ScanOpts {
                full,
                level: lvl,
                root: None,
                threads: THREADS,
            });
            plan.save();
            if json {
                println!("{}", serde_json::to_string_pretty(&plan).unwrap());
            } else {
                print_plan(&plan, verbose);
            }
        }
        Cmd::Clean {
            apply,
            categories,
            json,
        } => {
            let _lock = lock_or_exit("clean.lock");
            let Some(plan) = Plan::load() else {
                eprintln!("no plan yet — run `diskreap scan` first");
                std::process::exit(1);
            };
            if now() - plan.created > 6 * 3600 {
                eprintln!(
                    "plan is {} old — run `diskreap scan` again",
                    ago(plan.created)
                );
                std::process::exit(1);
            }
            let r = act::apply(
                &plan,
                &act::ApplyOpts {
                    apply,
                    categories: &categories,
                    until_avail: None,
                    quiet: json,
                },
            );
            if json {
                println!("{}", serde_json::to_string(&r).unwrap());
            } else if apply {
                println!(
                    "freed {} (df; estimate {})",
                    human(r.freed_df_bytes),
                    human(r.est_bytes)
                );
            } else {
                println!("would free ~{}", human(r.est_bytes));
            }
        }
        Cmd::Reap {
            path,
            approval,
            json,
        } => {
            let _lock = lock_or_exit("clean.lock");
            let r = act::reap_approved(&path, &approval);
            if json {
                println!(
                    "{}",
                    match &r {
                        Ok(b) => serde_json::json!({"path": path, "ok": true, "freed_df_bytes": b}),
                        Err(e) => serde_json::json!({"path": path, "ok": false, "reason": e}),
                    }
                );
            } else {
                match &r {
                    Ok(b) => println!("removed {} (freed {})", path.display(), human(*b)),
                    Err(e) => eprintln!("refused {}: {e}", path.display()),
                }
            }
            if r.is_err() {
                std::process::exit(1);
            }
        }
        Cmd::Auto {
            dry_run,
            force,
            min_level,
        } => auto(dry_run, force, min_level),
        Cmd::Du { path, depth, top } => du(path.unwrap_or_else(home), depth, top),
    }
}

fn lock_or_exit(name: &str) -> std::fs::File {
    util::try_lock(name).unwrap_or_else(|| {
        eprintln!("another diskreap {name} is running");
        std::process::exit(0);
    })
}

fn current_level() -> Level {
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    level_of(t, a)
}

fn status(hook: bool, json: bool) {
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    let fss = plan::watched_filesystems();
    let worst = fss.iter().map(|f| f.level).max().unwrap_or(level_of(t, a));
    if json {
        // Top-level numbers stay $HOME's filesystem; `level` is the worst watched one.
        println!(
            "{}",
            serde_json::json!({
                "host": util::hostname(), "home": home(), "total_bytes": t, "avail_bytes": a,
                "level": worst, "home_level": level_of(t, a), "target_avail_bytes": target_avail(t),
                "filesystems": fss, "version": env!("CARGO_PKG_VERSION"),
            })
        );
        return;
    }
    if hook {
        for f in fss.iter().filter(|f| f.level != Level::Ok) {
            let pct = if f.total_bytes > 0 {
                f.avail_bytes * 100 / f.total_bytes
            } else {
                0
            };
            println!(
                "DISK {}: only {} free ({pct}%) on {} ({}). Before writing large outputs, run `diskreap scan` and \
                 follow the disk-cleanup skill. Do not use plain `du`/`find` over $HOME (network mounts hang).",
                if f.level == Level::Critical { "CRITICAL" } else { "LOW" },
                human(f.avail_bytes),
                util::hostname(),
                f.path
            );
        }
        return;
    }
    for f in &fss {
        let pct = if f.total_bytes > 0 {
            f.avail_bytes * 100 / f.total_bytes
        } else {
            0
        };
        println!(
            "{}: {} — {} free of {} ({pct}%), level {:?}",
            util::hostname(),
            f.path,
            human(f.avail_bytes),
            human(f.total_bytes),
            f.level
        );
    }
    println!("auto target for $HOME: {}", human(target_avail(t)));
    if let Some(p) = Plan::load() {
        println!(
            "last plan: {} scan {} ago, {} reclaimable",
            p.mode,
            ago(p.created),
            human(p.reclaimable())
        );
    }
}

fn auto(dry_run: bool, force: bool, min_level: Level) {
    let _lock = lock_or_exit("auto.lock");
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    let home_level = level_of(t, a);
    let worst = plan::watched_filesystems()
        .iter()
        .map(|f| f.level)
        .max()
        .unwrap_or(home_level);
    println!(
        "[{}] {}: {} free in $HOME ({:?}); worst watched filesystem {:?}",
        now(),
        util::hostname(),
        human(a),
        home_level,
        worst
    );
    if worst < min_level && !force {
        return;
    }
    // Only a temp filesystem is under pressure: clean temp entries (judged by
    // that filesystem's level), not $HOME by $HOME's healthy policy.
    let tmp_only = home_level < min_level && !force;
    let cats: Vec<String> = if tmp_only {
        vec!["tmp".into()]
    } else {
        Vec::new()
    };
    let target = target_avail(t);
    for full in [false, true] {
        let plan = plan::scan(&ScanOpts {
            full,
            level: home_level,
            root: None,
            threads: THREADS,
        });
        plan.save();
        println!(
            "{} scan: {:.0}s, {} reclaimable",
            plan.mode,
            plan.scan_secs,
            human(plan.reclaimable())
        );
        let r = act::apply(
            &plan,
            &act::ApplyOpts {
                apply: !dry_run,
                categories: &cats,
                until_avail: if tmp_only { None } else { Some(target) },
                quiet: false,
            },
        );
        let freed = if dry_run {
            r.est_bytes
        } else {
            r.freed_df_bytes
        };
        println!(
            "{} {}",
            if dry_run { "would free ~" } else { "freed" },
            human(freed)
        );
        let now_avail = fs_space(&home()).map(|s| s.1).unwrap_or(0);
        if dry_run || tmp_only || now_avail >= target {
            return;
        }
    }
    let now_avail = fs_space(&home()).map(|s| s.1).unwrap_or(0);
    if level_of(t, now_avail) != Level::Ok {
        println!(
            "still {:?} after safe cleanup: a human/agent must review `diskreap scan --full -v` (report-only items)",
            level_of(t, now_avail)
        );
    }
}

fn print_plan(p: &Plan, verbose: bool) {
    let pct = if p.total > 0 {
        p.avail * 100 / p.total
    } else {
        0
    };
    println!(
        "{} — {} free of {} ({pct}%), level {:?}; {} scan in {:.1}s",
        p.host,
        human(p.avail),
        human(p.total),
        p.level,
        p.mode,
        p.scan_secs
    );
    let mut cats: Vec<&str> = p.items.iter().map(|i| i.cat.as_str()).collect();
    cats.dedup();
    let mut seen = std::collections::HashSet::new();
    for cat in [
        "docker",
        "log",
        "tmp",
        "build-output",
        "cache",
        "worktree",
        "report",
    ] {
        if !cats.contains(&cat) || !seen.insert(cat) {
            continue;
        }
        let ok: Vec<_> = p.items.iter().filter(|i| i.cat == cat && i.ok).collect();
        let no: Vec<_> = p.items.iter().filter(|i| i.cat == cat && !i.ok).collect();
        let sum: u64 = ok.iter().map(|i| i.bytes).sum();
        if cat == "report" {
            println!("\nREPORT ONLY (a human decides):");
        } else {
            println!(
                "\n{cat}: {} reclaimable in {} item(s), {} skipped",
                human(sum),
                ok.len(),
                no.len()
            );
        }
        for i in ok.iter().take(if verbose { usize::MAX } else { 12 }) {
            println!("  {:>8}  {}  {}", human(i.bytes), i.path, i.why);
        }
        if ok.len() > 12 && !verbose {
            println!("  … {} more (-v)", ok.len() - 12);
        }
        let show_skips = verbose || cat == "report" || cat == "worktree";
        for i in no
            .iter()
            .filter(|_| show_skips)
            .take(if verbose { usize::MAX } else { 15 })
        {
            println!("  {:>8}  {}  — {}", human(i.bytes), i.path, i.why);
        }
    }
    if !p.top.is_empty() {
        println!("\nLARGEST DIRECTORIES:");
        for (path, b, n) in p.top.iter().take(15) {
            println!("  {:>8}  {path}  (modified {} ago)", human(*b), ago(*n));
        }
    }
    if !p.stalled.is_empty() {
        println!(
            "\nSTALLED (abandoned after 15s — dead mount or failing disk?): {}",
            p.stalled.join(", ")
        );
    }
    if !p.skipped_mounts.is_empty() {
        println!(
            "\nskipped {} other mount(s) under $HOME",
            p.skipped_mounts.len()
        );
    }
    if !p.kept.is_empty() {
        println!("protected by .diskreap-keep: {}", p.kept.join(", "));
    }
    println!(
        "\nTOTAL reclaimable now: {}{}. Next: `diskreap clean` (dry run) → `diskreap clean --apply`",
        human(p.reclaimable()),
        if p.mode == "quick" { " (quick scan; `--full` finds deeper items)" } else { "" }
    );
}

fn du(p: PathBuf, depth: usize, top: usize) {
    let mounts = mounts::list();
    let t = std::time::Instant::now();
    let w = walk::walk(
        walk::Opts {
            skip: mounts::below(&mounts, &p),
            root: p.clone(),
            size_all: true,
            report_depth: depth,
            max_depth: None,
            stall: std::time::Duration::from_secs(15),
            threads: THREADS,
        },
        Box::new(rules::Rules { quick: false }),
    );
    let mut nodes: Vec<_> = w.nodes.into_iter().filter(|n| n.1 >= 1).collect();
    nodes.sort_by(|a, b| b.2.cmp(&a.2));
    println!(
        "{:>8}  {}  ({:.1}s)",
        human(w.total),
        p.display(),
        t.elapsed().as_secs_f64()
    );
    for (path, _, b, n) in nodes.iter().take(top) {
        println!(
            "{:>8}  {}  (modified {} ago)",
            human(*b),
            path.display(),
            ago(*n)
        );
    }
    for s in &w.stalled {
        println!("STALLED: {}", s.display());
    }
    if !w.skipped_mounts.is_empty() || w.errors > 0 {
        println!(
            "(skipped {} mount(s), {} unreadable)",
            w.skipped_mounts.len(),
            w.errors
        );
    }
}
