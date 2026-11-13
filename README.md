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
- **"Not running" doesn't mean "not needed".** A venv belonging to an on-demand (socket/timer/lease-started) service looks idle. diskreap refuses to touch any project that a systemd unit, cron entry or launchd plist references, or that a small config file one of them names references (a broker that starts engines on demand names their interpreters there). A reference through a directory symlink protects the symlink's target too.
- **The scan must not disturb what it measures.** Listing a directory updates its atime, and `git status` rewrites the index. Naive tools therefore see everything as "just used". diskreap counts only *file* atime as use, and runs git with `--no-optional-locks`.
- **Read-only trees.** Sealed release copies (`chmod a-w`) make `rm -rf` fail. diskreap restores `u+rwx` on the directories it empties. It never uses sudo and refuses to run as root.
- **Categories, not paths.** Nothing is specific to one machine: rules match *kinds* of data and gate them on evidence (git-ignored, untracked, idle, merged, unreferenced).

## Categories

| Category | What | Gate | Action |
|---|---|---|---|
| `docker` | build cache, dangling images, and (critical only) images no container uses, created 7d+ ago | age (24h / 1h) | `docker builder/image prune`; `docker rmi` per unused tag (`image prune -a` silently skips tagged images on the containerd image store) |
| `tmp` | user-owned entries under `/tmp`, `/var/tmp`, `$TMPDIR`: top level first; an entry kept because of something *inside* it (an open file, a path on a command line, a recent write) is split and its entries judged one by one, down to 4 levels below the temp root (a Claude Code session's entries, `/tmp/claude-<uid>/<project>/<session>/<entry>`). A directory that is itself a process's cwd, or holds a socket, is never split | idle ≥ 7d (2d critical), not in use, no socket/FIFO inside (a tmux/ssh-agent rendezvous looks idle), every directory above it ours and nobody's cwd | delete |
| `log` | `*.log`, `*.log.N` ≥ 500 MB (100 MB critical) | unwritten 24h, not open, not tracked | zstd (lossless) |
| `build-output` | any directory carrying a valid `CACHEDIR.TAG` (the cache-tagging standard Cargo, pip and others use; honored even outside git), `node_modules`, Cargo `target`, `build`†, `.dart_tool`, `.gradle`†, `.next`/`.nuxt`/`.svelte-kit`/`.angular`†, Python venvs (by `pyvenv.cfg`), `Pods`†, `.build`†, `.tox`, `.pytest_cache`, … | git-ignored, no tracked files, idle ≥ 3d (1d critical), project not in use, not referenced by a service | delete |
| `cache` | pip, uv, npm, pnpm, yarn, bun, cargo, go, gradle, maven, pub, conda pkgs, Playwright, Puppeteer, Chrome/Chromium/Firefox, Hugging Face, ModelScope, torch, Android, Xcode, `~/Library/Caches` | last use (file atime) beyond the horizon; files held open are kept; the tool's own prune where one exists | prune |
| `cache` (unknown app, **under pressure**) | `~/.cache/<app>` with no row in the table above | judged as ONE unit, never file by file (aging a live content store's files breaks its pins): nothing inside used (mtime or file atime) for 3d, nothing in use, not referenced, no socket, no mount | delete the dir |
| `offloaded` (**under pressure**) | paths in the offload ledger (below) | current size and newest mtime equal the ledger's, not a git repo, not in use, not referenced | delete |
| `worktree` | linked git worktrees | clean, merged into the main branch (incl. patch-equivalent), not locked, no stash entry naming the branch, no ignored data except build artifacts or files identical to the main checkout's, idle ≥ 3d (30d if HEAD is detached; or `worktree_merged_idle_minutes`, below), not in use, not referenced | `git worktree remove` (branch kept) |
| `backup` (**under pressure**) | dated entries in a directory named `*backup*` (any case) or its undated subdirectories (two levels), grouped into series by name with stamps stripped (`unchain-20261104-043001.dump` → `unchain#dump`; a stamp is a run of digits and `-_.:` with ≥ 4 digits) | older than the newest 3 of its series (only a sibling at least half its size counts as newer, so a job writing stubs cannot push real backups out), idle ≥ 1d, not in use, not referenced; never inside a git repo or an app's own dir (a dot-dir or an install root) | delete |
| report only | app caches inside repos, data unmodified for 90d+, trash; the newest 3 backups of a series, undated backups, backup dirs in repos/app dirs; unknown `~/.cache/<app>` ≥ 1 GiB still written or in use; full scan: git-ignored data ≥ 1 GiB in a checkout, `protect_paths` entries ≥ 1 GiB, dirs ≥ 1 GiB owned by another user (root) | — | listed for a human |

† only next to a matching project marker (`Cargo.toml`, `package.json`, `build.gradle`, `Podfile`, `Package.swift`, …).

"In use" covers the cwd, executable, open files and mmapped libraries of every process you can inspect, plus the bind-mount sources of running Docker containers and the cwd of every tmux pane. The tmux check matters because sandboxed agents are often non-dumpable, so `/proc/<pid>/cwd` is unreadable even for your own uid. On macOS this comes from `lsof`. If the snapshot can't be taken, everything counts as in use.

## Tiers

- **Always safe** (any level, horizons by level): `docker`, `log`, `tmp`, `build-output`, the `cache` table, merged clean `worktree`s.
- **Safe under pressure** (scan level `low` or `critical` only; at `ok` they say `only under disk pressure`): unknown app caches, `backup`s beyond the newest 3, `offloaded` data.
- **Needs a human** (`report`, never removed; `why` names the rule): see the last row above. Dirty/unmerged worktrees are listed under `WORKTREES NEEDING A HUMAN`.

**Offload ledger.** A tool that copies data off-host appends one JSON object per line to `~/.local/state/diskreap/offloaded.jsonl` *after* verifying the copy:

```json
{"path": "/home/u/histo-reader-archive", "bytes": 27487790694, "mtime": 1789370235, "remote": "oss://bucket/archive/", "verified_at": 1789371000}
```

`bytes` is the sum of `st_size` of every non-directory entry (for a file, its size; `find P -type f -printf '%s\n'` summed), `mtime` the newest mtime of any entry, the root included (unix seconds), `remote` non-empty, `verified_at` > 0. The last line for a path wins; malformed lines are ignored with a warning. Anything that changed since (one byte, one touch) is skipped.

**Still critical.** When `auto` ends with `$HOME` still `critical`, it writes `~/.local/state/diskreap/needs-supervision.json` = `{"host","since","updated","level","avail_bytes","total_bytes","version"}` (`since` kept from an earlier marker; unix seconds) and exits **3**. Any non-dry run that finds the level below critical removes it. `status --json` carries it as `"needs_supervision"` (the object, or `null`), for an orchestrator to start a supervised run.

## Pressure levels

diskreap watches `$HOME`'s filesystem and every temp root (`/tmp`, `/var/tmp`, `$TMPDIR`) that lives on a different one. A full tmpfs counts too: it is RAM. `status` and the hook report each one under pressure, and temp entries are judged by their own filesystem's level.


| Level | Free space | Effect |
|---|---|---|
| ok | ≥ 20% of the disk (at least min(10 GiB, 30%); no upper cap) | daily maintenance only: clearly stale items (build output idle 14d, worktrees 7d, caches at 2× horizon, logs ≥ 1 GB idle 7d, Docker cache older than 7d) |
| low | below that | normal horizons |
| critical | < 5% of the disk, clamped to [3, 30] GiB | shorter horizons (build/worktree 1d, logs 100 MB/6h, caches at 25% of their horizon, unused Docker images) |

**Operator floor.** A host that keeps a large working set (an object store, build caches) can raise the `low` threshold with `min_free_gib`, in `~/.config/diskreap/config.json` (`{"min_free_gib": 120}`, path overridable with `DISKREAP_CONFIG`) or `DISKREAP_MIN_FREE_GIB` (wins over the file). Free space on `$HOME`'s filesystem below it counts as `low` (never `critical`, never a lower level), `auto` acts on it even under `--min-level critical`, and its stop target becomes at least 125% of the floor. A floor above half the filesystem is capped there; a malformed value exits 2. Temp filesystems are not affected.

**Protected paths.** Also in `~/.config/diskreap/config.json`: `protect_paths` is a list of globs (`~` allowed) that are never removed, and `protect_config_refs` is a list of `{"file": F, "keys": [K, ...]}`: every string stored under key K, at any depth of JSON file F, is protected (a missing F is ignored, an unparsable F warns). Use the second form for a tool that registers checkouts by path, e.g. `{"protect_config_refs": [{"file": "~/.config/zzops/config.human.json", "keys": ["sourceRoot"]}]}`. A `protect_paths` path, everything inside it, and any directory above it are skipped for every category. A `protect_config_refs` path and the directories above it are skipped, like a path named in a service unit; a malformed key exits 2.

**Fast rule for merged worktrees.** Hosts where agents create a worktree per task (several an hour, GBs each) fill up faster than a 3 to 7 day idle window frees them. `{"worktree_merged_idle_minutes": 120}` makes a worktree removable after that many minutes idle (minimum 30; default: off, the idle-days rule applies) when ALL of these hold: its HEAD is a strict ancestor of the already-fetched `origin/<default branch>` (no network; a missing `origin/` ref means skip, local `main` is not trusted), `git status` is clean, no process or tmux pane has it as cwd (or holds files open in it), nothing in it was touched inside the window, it is not locked, not protected by `protect_paths`/`protect_config_refs`, its branch matches no glob in `protect_worktree_branches` (e.g. `["agent/keep-*"]`), and the usual stash/ignored-data checks pass. Removal is `git worktree remove --force` plus `git worktree prune` (so submodule checkouts do not block it, and no submodule is ever deinitialised); the local branch is kept. Detached-HEAD worktrees keep the 30 day pin horizon.

**Integration branches.** If a repo lands on an integration branch and moves the default branch only on release, add `"worktree_integration_refs": ["origin/dev"]`: a worktree merged into the default branch or any listed `origin/<branch>` qualifies (same strict/idle/clean rules; the reason line names the ref that matched).

**Worktrees that hold work are reported, never deleted.** `diskreap scan` ends with `WORKTREES NEEDING A HUMAN`: every dirty or unmerged worktree, biggest first, with size, kind, time since last activity, branch, what is wrong, a best-effort owner (a process or tmux pane with that cwd, or the `git worktree lock` reason) and the path, plus a count and total size. `status` repeats the count from the last scan. `clean` (dry run or not) prints why each worktree is removed (`clean, merged into origin/main, idle 3h (needs 2h, worktree_merged_idle_minutes); branch X kept, no process inside`).

**Keep above 20%.** The installed hourly guard runs `auto --min-level low`: it acts whenever less than 20% of the disk is free (or less than `min_free_gib`) and keeps going until 20% is free again (at least min(15 GiB, 40%); no upper cap, so a 1.8 TiB disk aims for 360 GiB). It escalates in this order: Docker, logs, temp dirs, build output, caches, offloaded data, worktrees, old backups. It runs the full scan only if the quick plan wasn't enough, and at most every 6 h unless the disk is critical (a disk that safe cleanup cannot bring back would otherwise be walked every hour). What is left is report-only: a person or a supervised run decides about it.

## Install

```bash
git clone https://github.com/zigzag-tech/diskreap && cd diskreap
./install.sh                 # binary + hourly keep-above-20% guard (systemd user / launchd / cron) + agent skill links
./install.sh --claude-hook   # also warn Claude Code sessions when the disk is low
```

Or just the binary: `cargo install --git https://github.com/zigzag-tech/diskreap`.

On Linux, user timers need lingering to run while you're logged out: `sudo loginctl enable-linger $USER`.

## Usage

```bash
diskreap status                 # free space, level, last plan
diskreap scan [--full] [-v] [--json] [--level low|critical]
diskreap clean [--apply] [-c CATEGORY]...   # docker log tmp build-output cache offloaded worktree backup
diskreap auto [--dry-run] [--min-level ok|low|critical]   # schedule `--min-level ok` daily for maintenance; exit 3 = still critical
diskreap reap PATH --approval NOTE   # remove one human-approved report-only item
diskreap status|clean --json          # machine-readable, for orchestrators
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

Test hooks (never set them on a real host): `DISKREAP_TMP_ROOTS` (`:`-separated) replaces the temp roots, so the suite never touches the machine's `/tmp`; `DISKREAP_TEST_AVAIL_BYTES` makes every filesystem report that many bytes free (announced on stderr), to reach `critical` without filling a disk.

## License

MIT
