mod act;
mod git;
mod mounts;
mod plan;
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
    },
    /// Timer entry point: if free space is low, quick scan + clean until the
    /// target is reached, escalating to a full scan if needed.
    Auto {
        #[arg(long)]
        dry_run: bool,
        /// Act even when space is fine (uses the `low` policy).
        #[arg(long)]
        force: bool,
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
    if unsafe { libc::geteuid() } == 0 && std::env::var_os("DISKREAP_ALLOW_ROOT").is_none() {
        eprintln!("diskreap: refusing to run as root (it cleans the invoking user's files)");
        std::process::exit(2);
    }
    match Cli::parse().cmd {
        Cmd::Status { hook } => status(hook),
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
        Cmd::Clean { apply, categories } => {
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
            let freed = act::apply(
                &plan,
                &act::ApplyOpts {
                    apply,
                    categories: &categories,
                    until_avail: None,
                },
            );
            println!(
                "{} {}",
                if apply { "freed" } else { "would free ~" },
                human(freed)
            );
        }
        Cmd::Auto { dry_run, force } => auto(dry_run, force),
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

fn status(hook: bool) {
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    let lvl = level_of(t, a);
    let pct = if t > 0 { a * 100 / t } else { 0 };
    if hook {
        if lvl != Level::Ok {
            println!(
                "DISK {}: only {} free ({pct}%) on {} ({}). Before writing large outputs, run `diskreap scan` and \
                 follow the disk-cleanup skill; `diskreap auto` also runs hourly. Do not use plain `du`/`find` over $HOME (network mounts hang).",
                if lvl == Level::Critical { "CRITICAL" } else { "LOW" },
                human(a),
                util::hostname(),
                home().display()
            );
        }
        return;
    }
    println!(
        "{}: {} free of {} ({pct}%) — level {:?}, auto target {}",
        util::hostname(),
        human(a),
        human(t),
        lvl,
        human(target_avail(t))
    );
    if let Some(p) = Plan::load() {
        println!(
            "last plan: {} scan {} ago, {} reclaimable",
            p.mode,
            ago(p.created),
            human(p.reclaimable())
        );
    }
}

fn auto(dry_run: bool, force: bool) {
    let _lock = lock_or_exit("auto.lock");
    let (t, a) = fs_space(&home()).unwrap_or((0, 0));
    let lvl = level_of(t, a);
    println!(
        "[{}] {}: {} free ({:?})",
        now(),
        util::hostname(),
        human(a),
        lvl
    );
    if lvl == Level::Ok && !force {
        return;
    }
    let target = target_avail(t);
    for full in [false, true] {
        let plan = plan::scan(&ScanOpts {
            full,
            level: lvl.max(Level::Low),
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
        let freed = act::apply(
            &plan,
            &act::ApplyOpts {
                apply: !dry_run,
                categories: &[],
                until_avail: Some(target),
            },
        );
        println!(
            "{} {}",
            if dry_run { "would free ~" } else { "freed" },
            human(freed)
        );
        let now_avail = fs_space(&home()).map(|s| s.1).unwrap_or(0);
        if dry_run || now_avail >= target {
            return;
        }
    }
    let now_avail = fs_space(&home()).map(|s| s.1).unwrap_or(0);
    if level_of(t, now_avail) != Level::Ok {
        println!("still {:?} after safe cleanup: a human/agent must review `diskreap scan --full -v` (report-only items)", level_of(t, now_avail));
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
