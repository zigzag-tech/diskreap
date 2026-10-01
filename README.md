# diskreap

A hang-proof disk scanner and a **safe, categorized** cleaner for developer
machines and the coding agents that work on them.

It is built around two stages:

1. **Scan** (read-only), writing a plan:
   - quick (seconds)
   - full (about 30 s per TB on NVMe)
2. **Clean**: every item is re-verified right before anything is touched. It's a dry run unless you pass `--apply`.

`diskreap auto` runs both stages from an hourly timer, but only when the disk is actually getting full.

```
$ diskreap scan
host — 220.8G free of 1.8T (12%), level Low; quick scan in 11.8s

docker: 36.0G reclaimable in 1 item(s), 0 skipped
build-output: 17.4G reclaimable in 64 item(s), 71 skipped
      3.0G  ~/langchainjs/node_modules
      …
cache: 4.8G reclaimable in 11 item(s), 0 skipped
      4.5G  ~/.cache/google-chrome  unused > 7d (4.5G total)
worktree: 244.6M reclaimable in 3 item(s), 40 skipped
      2.8G  ~/worktrees/app/feature-x  — 16 uncommitted/untracked change(s)
REPORT ONLY (a human decides):
     94.1G  ~/project/.cache  — app cache, modified 2m ago — safe only if the app re-derives it
     46.2G  ~/old-images      — cold: nothing modified in 481d

TOTAL reclaimable now: 58.5G. Next: `diskreap clean` (dry run) → `diskreap clean --apply`
```

## Why another cleaner

Every rule here comes from cleaning a real 1.8 TB workstation that hit 100% while several coding agents were running on it:

- **`du` hangs.** A home directory holding sshfs, NFS, rclone or SMB mounts will block a plain `du`/`find` forever on one dead mount. diskreap reads the mount table first and never opens a mountpoint below its root. It also never crosses a device boundary, and a watchdog abandons and reports any directory whose syscalls stall for more than 15 s.
- **Speed.** The walk runs in parallel and stats nothing outside candidates in quick mode. A full walk of 1.3 TB took 32 s, against more than 10 minutes for `du`.
- **"Not running" doesn't mean "not needed".** A venv belonging to an on-demand (socket/timer/lease-started) service looks idle. diskreap refuses to touch any project that a systemd unit, cron entry or launchd plist references.
- **The scan must not disturb what it measures.** Listing a directory updates its atime, and `git status` rewrites the index. Naive tools therefore see everything as "just used". diskreap counts only *file* atime as use, and runs git with `--no-optional-locks`.
- **Read-only trees.** Sealed release copies (`chmod a-w`) make `rm -rf` fail. diskreap restores `u+rwx` on the directories it empties. It never uses sudo and refuses to run as root.
- **Categories, not paths.** Nothing is specific to one machine: rules match *kinds* of data and gate them on evidence (git-ignored, untracked, idle, merged, unreferenced).

## Categories

| Category | What | Gate | Action |
|---|---|---|---|
| `docker` | build cache, dangling images, and (critical only) images no container uses, created 7d+ ago | age (24h / 1h) | `docker builder/image prune`; `docker rmi` per unused tag (`image prune -a` silently skips tagged images on the containerd image store) |
| `tmp` | user-owned entries directly under `/tmp`, `/var/tmp`, `$TMPDIR` | idle ≥ 7d (2d critical), not in use, no socket/FIFO inside (a tmux/ssh-agent rendezvous looks idle) | delete |
| `log` | `*.log`, `*.log.N` ≥ 500 MB (100 MB critical) | unwritten 24h, not open, not tracked | zstd (lossless) |
| `build-output` | `node_modules`, Cargo `target`, `build`†, `.dart_tool`, `.gradle`†, `.next`/`.nuxt`/`.svelte-kit`/`.angular`†, Python venvs (by `pyvenv.cfg`), `Pods`†, `.build`†, `.tox`, `.pytest_cache`, … | git-ignored, no tracked files, idle ≥ 3d (1d critical), project not in use, not referenced by a service | delete |
| `cache` | pip, uv, npm, pnpm, yarn, bun, cargo, go, gradle, maven, pub, conda pkgs, Playwright, Puppeteer, Chrome/Chromium/Firefox, Hugging Face, ModelScope, torch, Android, Xcode, `~/Library/Caches` | last use (file atime) beyond the horizon; files held open are kept; the tool's own prune where one exists | prune |
| `worktree` | linked git worktrees | clean, merged into the main branch (incl. patch-equivalent), not locked, no stash entry naming the branch, no ignored data except build artifacts or files identical to the main checkout's, idle ≥ 3d (30d if HEAD is detached), not in use, not referenced | `git worktree remove` (branch kept) |
| report only | app caches inside repos, data unmodified for 90d+, trash | — | listed for a human |

† only next to a matching project marker (`Cargo.toml`, `package.json`, `build.gradle`, `Podfile`, `Package.swift`, …).

"In use" covers the cwd, executable, open files and mmapped libraries of every process you can inspect, plus the bind-mount sources of running Docker containers and the cwd of every tmux pane. The tmux check matters because sandboxed agents are often non-dumpable, so `/proc/<pid>/cwd` is unreadable even for your own uid. On macOS this comes from `lsof`. If the snapshot can't be taken, everything counts as in use.

## Pressure levels

| Level | Free space | Effect |
|---|---|---|
| ok | ≥ 15% and ≥ 30 GiB | `auto` does nothing |
| low | < 15% or < 30 GiB | normal horizons |
| critical | < 5% or < 10 GiB | shorter horizons (build/worktree 1d, logs 100 MB/6h, caches at 25% of their horizon, unused Docker images) |

`auto` stops as soon as free space is back above max(20%, 40 GiB). It escalates in this order: Docker, logs, temp dirs, build output, caches, worktrees. It only runs the full scan if the quick plan wasn't enough.

## Install

```bash
git clone https://github.com/zigzag-tech/diskreap && cd diskreap
./install.sh                 # binary + hourly timer (systemd user / launchd) + agent skill links
./install.sh --claude-hook   # also warn Claude Code sessions when the disk is low
```

Or just the binary: `cargo install --git https://github.com/zigzag-tech/diskreap`.

On Linux, user timers need lingering to run while you're logged out: `sudo loginctl enable-linger $USER`.

## Usage

```bash
diskreap status                 # free space, level, last plan
diskreap scan [--full] [-v] [--json] [--level low|critical]
diskreap clean [--apply] [-c CATEGORY]...
diskreap auto [--dry-run] [--force]
diskreap du [PATH] [--depth N] [--top N]   # hang-proof du
```

- **State:** `~/.local/state/diskreap/`. It holds `plan.json` and `actions.jsonl`, an append-only audit log of everything that was compressed, pruned or removed. Per-item `est_bytes` are logical sizes. Each run's `run-summary` line holds the measured `df` gain, which on btrfs/ZFS keeps growing for a while after the run.
- **Protect a directory:** put an empty `.diskreap-keep` file in it.

## Agent skill

`skills/disk-cleanup/SKILL.md` teaches coding agents (Claude Code, Codex, and others) the two-stage workflow. It also tells them to present report-only items to a human rather than delete them.

## Tests

```bash
cargo test                  # unit tests
cargo build --release && tests/e2e.sh   # real git repos/worktrees in a throwaway $HOME
```

## License

MIT
