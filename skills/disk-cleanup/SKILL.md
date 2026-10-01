---
name: disk-cleanup
description: Free disk space safely with `diskreap` — a hang-proof scanner plus a categorized, re-verifying cleaner. Use when a disk is low/full ("No space left on device", ENOSPC, builds failing on write, a session hook says DISK LOW/CRITICAL), when asked to "clean up disk", "free space", "what's using my disk", "why is the disk full", or before writing large outputs on a nearly full machine. Two stages: scan (quick or full, read-only) → clean (dry run, then --apply).
---

# Disk cleanup with diskreap

`diskreap` never deletes anything during a scan, and re-checks every item right
before acting. **Use it instead of `du` / `find` / `ncdu` over `$HOME`**: those
stat into network mounts (sshfs, NFS, rclone, SMB) and can hang forever;
diskreap reads the mount table first and never opens another mount, and a
watchdog abandons any directory that stalls.

If `diskreap` is not installed: `cargo install --git https://github.com/zigzag-tech/diskreap`
(then optionally `./install.sh` from a clone for the hourly timer).

## Stage 1 — scan (read-only)

| Command | When | Cost |
|---|---|---|
| `diskreap status` | how bad is it? | instant (statvfs) |
| `diskreap scan` | **quick**: candidates down to depth 6, only candidates are sized | seconds |
| `diskreap scan --full` | **complete**: every file sized, deep candidates, largest dirs, cold-data report | ~30 s per TB on NVMe |
| `diskreap du [PATH] --depth N` | hang-proof `du` replacement | like `--full`, one tree |

Start with the quick scan. Run `--full` when the quick result is not enough, when
you need to know *where* the space is, or when looking for cold data. Add `-v`
to see every candidate and why each skipped one was skipped; `--json` for machine use.
A scan writes the plan to `~/.local/state/diskreap/plan.json`.

### Reading the report

Each category lists what is reclaimable now, plus skipped items with the reason:

- **docker** — build cache older than 24h (1h when critical) and dangling images;
  unused images older than 7d only when critical. Docker's figure is an upper
  bound (it includes recent cache that is kept).
- **log** — `*.log` / `*.log.N` ≥ 500 MB (100 MB critical), unwritten for 24h,
  not open, not tracked by git → compressed with zstd (lossless).
- **build-output** — regenerable trees: `node_modules`, Cargo `target`, `build`
  (with a project marker), `.dart_tool`, `.gradle`, `.next`/`.nuxt`/…, Python
  virtualenvs (found by `pyvenv.cfg`, any name), `Pods`, tool caches. Only if
  git-ignored, untracked, idle ≥ 3d (1d critical), the project is not in use by
  any process or container, and **no systemd unit / cron / launchd job
  references the project** (an on-demand service is usually not running).
- **cache** — re-downloadable caches (pip, npm, pnpm, uv, cargo, go, gradle,
  maven, pub, conda pkgs, Playwright/Puppeteer browsers, browser caches,
  Hugging Face / ModelScope / torch models, Xcode DerivedData, `~/Library/Caches`),
  pruned by last use (file atime) — only entries unused for the horizon go.
- **worktree** — linked git worktrees that are clean (no changes, no untracked
  files), fully merged into the main branch (incl. rebased equivalents), idle
  ≥ 3d (30d when HEAD is detached — likely a pin), and not in use. The branch is kept.
- **REPORT ONLY** — never auto-cleaned: app-local caches inside repos, data not
  modified for 90d+, trash. **A human decides.** Present these to the user with
  sizes; do not delete them yourself unless the user says which.

Skipped worktrees ("uncommitted changes", "commits not in main") are often
the biggest remaining items: tell the user; do not discard work.

## Stage 2 — clean

```bash
diskreap clean                         # dry run: exactly what would happen
diskreap clean --apply                 # act on the plan (re-verifies each item first)
diskreap clean --apply -c docker -c build-output   # only some categories
```

The plan expires after 6h — rescan if needed. Every action is appended to
`~/.local/state/diskreap/actions.jsonl` (what, where, bytes).

## Automatic mode

`diskreap auto` (installed as an hourly systemd user timer / launchd agent)
does nothing while free space is fine. When free space drops below 15% or 30 GiB
(**low**) it runs a quick scan + clean, escalating to a full scan, and stops once
free space is back above max(20%, 40 GiB). Below 5% or 10 GiB (**critical**) the
age horizons shorten. Check its log with `journalctl --user -u diskreap` (Linux)
or `~/.local/state/diskreap/auto.log` (macOS).

## Protecting something

Put an empty `.diskreap-keep` file in a directory: nothing inside it is ever a
candidate. Use it for anything expensive to rebuild that a rule would otherwise match.

## Workflow for an agent

1. `diskreap status` — level and free space.
2. `diskreap scan` → summarize the categories and totals for the user.
3. If the user asked to clean (or the disk is critical and you are blocked):
   `diskreap clean` (show the dry run), then `diskreap clean --apply`.
4. If still low: `diskreap scan --full -v`, then present the REPORT ONLY and
   skipped-worktree items, biggest first, and let the user choose. Never `rm -rf`
   report-only items, data, backups, or unmerged work on your own judgment.
5. Never run diskreap as root or with sudo; it only cleans the invoking user's files.
