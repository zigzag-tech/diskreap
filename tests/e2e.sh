#!/usr/bin/env bash
# End-to-end: build a throwaway $HOME with real git repos/worktrees and check
# every verdict, then apply and check what is gone and what survived.
#   tests/e2e.sh [path/to/diskreap]
set -euo pipefail

B="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/diskreap}"
T="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null; chmod -R u+w "$T" 2>/dev/null; rm -rf "$T"' EXIT
H="$T/home"
mkdir -p "$H"
mkdir -p "$T/tmp"
# DISKREAP_TMP_ROOTS: judge (and clean) only our own temp root, never this machine's /tmp.
export TMPDIR="$T/tmp" DISKREAP_TMP_ROOTS="$T/tmp" HOME="$H" DISKREAP_STATE="$T/state" GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
big() { mkdir -p "$(dirname "$1")"; head -c "$2" /dev/urandom >"$1"; }
huge() { mkdir -p "$(dirname "$1")"; fallocate -l "$2" "$1" 2>/dev/null || head -c "$2" /dev/zero >"$1"; }

cd "$H"
git init -q -b main repo
cd repo
printf 'artifacts/\nnode_modules/\n*.log\n.env\nvenv/\n' >.gitignore
echo '{}' >package.json
echo 'A=1' >.env
git add .gitignore package.json && git commit -qm init
git init -q --bare ../origin.git && git remote add origin ../origin.git && git push -q origin main   # origin/main, as agents' clones have
big node_modules/a/big 30M
big artifacts/run1/hub/node_modules/x/big 20M
echo '{}' >artifacts/run1/hub/package.json   # a staged release copy carries its manifest
chmod -R a-w artifacts/run1/hub # sealed release copy
head -c 120M /dev/zero >app.log

wt() { git worktree add -q "../$1" -b "$1"; }
wt wt-merged
wt wt-unmerged && (cd ../wt-unmerged && echo x >f && git add f && git commit -qm wip)
wt wt-stashed && (cd ../wt-stashed && echo y >>package.json && git stash -q)
wt wt-locked && git worktree lock ../wt-locked
wt wt-envdiff && echo 'A=2' >../wt-envdiff/.env
wt wt-envsame && cp .env ../wt-envsame/.env
git worktree add -q --detach ../wt-detached
# Fast-rule fixtures (aged 3 hours below, not 60 days): clean+merged, dirty, protected-by-branch, unmerged.
wt wt-recent; wt wt-recentdirty; wt keepme-recent; wt wt-recentahead
echo z >>../wt-recentdirty/package.json
(cd ../wt-recentahead && echo a >g && git add g && git commit -qm ahead)
big ../wt-merged/node_modules/z/big 15M

# A project whose venv an (absent) service references: protected.
git init -q -b main "$H/svc"
(cd "$H/svc" && printf 'venv/\n' >.gitignore && echo x >requirements.txt && git add . && git commit -qm i)
big "$H/svc/venv/lib/big" 12M
touch "$H/svc/venv/pyvenv.cfg"
mkdir -p "$H/.config/systemd/user"
printf '[Service]\nExecStart=%s/svc/venv/bin/python -m srv\n' "$H" >"$H/.config/systemd/user/svc.service"

mkdir -p "$H/plain/node_modules" && big "$H/plain/node_modules/f" 12M
# An exported (non-git) Cargo tree: target/ declares itself a cache via CACHEDIR.TAG.
big "$H/exported/daemon/target/release/big" 12M
printf 'Signature: 8a477f597d28d172789f06886806bc55\n' >"$H/exported/daemon/target/CACHEDIR.TAG"
echo '[package]' >"$H/exported/daemon/Cargo.toml"
big "$H/.cache/huggingface/hub/models--old/w" 11M
big "$H/.cache/huggingface/hub/models--new/w" 11M

big "$T/tmp/stale-build/blob" 11M
big "$T/tmp/live-session/blob" 11M
python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])" "$T/tmp/live-session/sock"

# Regression 2026-10-02: installations that look like idle build output.
TAG='Signature: 8a477f597d28d172789f06886806bc55'
big "$H/.local/share/uv/tools/ytdl/lib/big" 12M            # uv tool venv: tagged by uv itself
touch "$H/.local/share/uv/tools/ytdl/pyvenv.cfg"; echo "$TAG" >"$H/.local/share/uv/tools/ytdl/CACHEDIR.TAG"
mkdir -p "$H/.local/bin" "$H/.local/share/uv/tools/ytdl/bin"; touch "$H/.local/share/uv/tools/ytdl/bin/ytdl"
ln -s "$H/.local/share/uv/tools/ytdl/bin/ytdl" "$H/.local/bin/ytdl"
big "$H/apps/tool/venv/lib/big" 12M; touch "$H/apps/tool/venv/pyvenv.cfg"   # venv with no project beside it
printf '#!%s/apps/tool/venv/bin/python\n' "$H" >"$H/.local/bin/tool"; chmod +x "$H/.local/bin/tool"
git init -q -b main "$H/active"                               # tagged Cargo target in a repo someone works in
(cd "$H/active" && echo '[package]' >Cargo.toml && printf 'target/\n' >.gitignore && git add . && git commit -qm i)
big "$H/active/target/debug/big" 12M; echo "$TAG" >"$H/active/target/CACHEDIR.TAG"

# Temp split one level down (and further, to 4 levels): a kept dir's entries are judged one by one.
big "$T/tmp/sess/old-a/blob" 11M; big "$T/tmp/sess/live/blob" 11M                 # live/blob held open below
big "$T/tmp/sess2/mod/x.mjs" 11M; big "$T/tmp/sess2/other/blob" 11M              # x.mjs named on a command line only
big "$T/tmp/claude-1000/proj/sess/scratch-old/blob" 11M; big "$T/tmp/claude-1000/proj/sess/live.txt" 11M
big "$T/tmp/deep/a/b/c/old/blob" 11M; big "$T/tmp/deep/a/b/c/live" 11M          # c is 4 deep: never split
big "$T/tmp/sess/input/blob" 11M                                                  # old mtime, read recently (atime set below)
# Unknown ~/.cache/<app> dirs: one idle, one (>= 1 GiB) held open.
big "$H/.cache/oldapp/store/blob" 11M
huge "$H/.cache/liveapp/pins/blob" 1100M
# Backups: a dated series of 5 (two beyond the newest 3), and a series whose newer runs are stubs.
for d in 1 2 3 4 5; do big "$H/db-backups/host1/unchain-2026110$d-043001.dump" 11M; done
big "$H/db-backups/stubs/site-20261101.tar" 11M
big "$H/db-backups/stubs/notes/a" 1K; big "$H/old-backup/cas/d895982f4f09ac4f" 11M   # undated: one report each, never rotated
for d in 2 3 4; do echo stub >"$H/db-backups/stubs/site-2026110$d.tar"; done
# Offloaded: one unchanged since its ledger line, one changed.
big "$H/archive/run1/blob" 11M; big "$H/archive/run2/blob" 11M

# Age everything 60 days, then make one model recently used.
chmod -R u+w "$H/repo/artifacts"
find "$H" "$T/tmp" -mindepth 1 -exec touch -h -a -m -d '60 days ago' {} +
chmod -R a-w "$H/repo/artifacts/run1/hub"
touch -a "$H/.cache/huggingface/hub/models--new/w"
touch "$H/active/.git/index"   # someone ran git there just now
for d in 1 2 3 4 5; do touch -d "$((11 - d)) days ago" "$H/db-backups/host1/unchain-2026110$d-043001.dump"; done
touch -d '10 days ago' "$H/db-backups/stubs/site-20261101.tar"
touch -a "$T/tmp/sess/input/blob"
for d in 2 3 4; do touch -d "$((10 - d)) days ago" "$H/db-backups/stubs/site-2026110$d.tar"; done
ledger() { # path bytes mtime
  printf '{"path":"%s","bytes":%s,"mtime":%s,"remote":"oss://bucket/x","verified_at":%s}\n' "$1" "$2" "$3" "$(date +%s)" >>"$T/state/offloaded.jsonl"
}
tree_bytes() { find "$1" -type f -printf '%s\n' | awk '{s+=$1} END {print s+0}'; }
tree_mtime() { find "$1" -printf '%T@\n' | sort -n | tail -1 | cut -d. -f1; }
mkdir -p "$T/state"
ledger "$H/archive/run1" "$(tree_bytes "$H/archive/run1")" "$(tree_mtime "$H/archive/run1")"
ledger "$H/archive/run2" "$(( $(tree_bytes "$H/archive/run2") + 1 ))" "$(tree_mtime "$H/archive/run2")"
echo 'not json' >>"$T/state/offloaded.jsonl"
for w in wt-recent wt-recentdirty keepme-recent wt-recentahead; do   # worked on 3 hours ago: idle for the fast rule, not for the 3d rule
  find "$H/$w" "$H/repo/.git/worktrees/$w" -exec touch -h -a -m -d '3 hours ago' {} +
done

cd /
# Consumers: open files, and a path named only on a command line (node --import=… closes the file).
sleep 600 <"$T/tmp/sess/live/blob" &
sleep 600 <"$T/tmp/claude-1000/proj/sess/live.txt" &
sleep 600 <"$T/tmp/deep/a/b/c/live" &
sleep 600 <"$H/.cache/liveapp/pins/blob" &
bash -c 'sleep 600; :' x "--import=$T/tmp/sess2/mod/x.mjs" &   # `; :` stops bash exec-ing sleep (losing argv)
sleep 0.5
DISKREAP_DEBUG=1 "$B" scan --level low --json >"$T/plan.json" 2>"$T/scan.err"
v() { local p="$1"; [[ "$p" == /* ]] || p="$H/$p"; jq -r --arg p "$p" '.items[] | select(.path==$p) | if .ok then "ok" else "skip: " + .why end' "${PLAN:-$T/plan.json}"; }
fail=0
expect() {
  local got; got="$(v "$1")"
  if [[ "$got" == $2* ]]; then echo "ok    $1 → $got"; else echo "FAIL  $1 → '$got' (want '$2…')"; fail=1; fi
}
expect repo/node_modules ok
expect repo/artifacts/run1/hub/node_modules ok
expect repo/app.log "skip: < 500"
expect wt-merged ok
expect wt-unmerged "skip: has commits not in"
expect wt-stashed "skip: a stash entry"
expect wt-locked "skip: locked"
expect wt-envdiff "skip: holds ignored data"
expect wt-envsame ok
expect wt-detached ok # 60d idle > 30d pin horizon
expect wt-recent "skip: active"   # no worktree_merged_idle_minutes configured: the 3d rule holds
expect svc/venv "skip: project referenced by"
expect plain/node_modules "skip: no project manifest"
expect .local/share/uv/tools/ytdl "skip: inside an installation root"
expect apps/tool/venv "skip: no project manifest"
expect active/target "skip: active"
expect exported/daemon/target ok
expect .cache/huggingface/hub ok
expect "$T/tmp/stale-build" ok
expect "$T/tmp/live-session" "skip: holds a socket"
expect "$T/tmp/sess" "skip: in use"
expect "$T/tmp/sess/old-a" ok
expect "$T/tmp/sess/live" "skip: in use"
expect "$T/tmp/sess/input" "skip: active"   # split entries count file atime as use
expect "$T/tmp/sess2/other" ok
expect "$T/tmp/sess2/mod/x.mjs" "skip: in use"
expect "$T/tmp/claude-1000/proj/sess/scratch-old" ok
expect "$T/tmp/deep/a/b/c" "skip: in use"
[ -z "$(v "$T/tmp/deep/a/b/c/old")" ] && echo "ok    temp is not judged below $T/tmp/deep/a/b/c (4 levels)" || { echo "FAIL  temp went deeper than 4 levels"; fail=1; }
cat_of() { jq -r --arg p "$H/$1" '.items[] | select(.path==$p) | .cat + ": " + .why' "${PLAN:-$T/plan.json}"; }
expect_cat() {
  local got; got="$(cat_of "$1")"
  if [[ "$got" == $2* ]]; then echo "ok    $1 → $got"; else echo "FAIL  $1 → '$got' (want '$2…')"; fail=1; fi
}
expect .cache/oldapp ok
expect_cat .cache/liveapp "report: unknown app cache, in use"
expect db-backups/host1/unchain-20261101-043001.dump ok
expect db-backups/host1/unchain-20261102-043001.dump ok
for d in 3 4 5; do expect_cat "db-backups/host1/unchain-2026110$d-043001.dump" "report: one of the newest 3 backups"; done
expect_cat db-backups/stubs/site-20261101.tar "report: backup kept: one of the newest 3 full-size"
expect_cat db-backups/stubs/notes "report: backup without a date stamp"
expect_cat old-backup "report: backup dir with no dated backups"
[ -z "$(v old-backup/cas/d895982f4f09ac4f)" ] && echo "ok    hash-named backup content is not a series" || { echo "FAIL  hash judged"; fail=1; }
expect archive/run1 ok
expect archive/run2 "skip: changed since it was offloaded"
grep -q "offloaded.jsonl line 3" "$T/scan.err" && echo "ok    malformed ledger line warned" || { echo "FAIL  ledger warning"; fail=1; }
sed -i '$d' "$T/state/offloaded.jsonl"
# Not under pressure: the same items wait.
DISKREAP_DEBUG=1 "$B" scan --level ok --json >"$T/plan-ok.json" 2>/dev/null
PLAN="$T/plan-ok.json"
expect .cache/oldapp "skip: only under disk pressure"
expect db-backups/host1/unchain-20261101-043001.dump "skip: only under disk pressure"
expect archive/run1 "skip: only under disk pressure"
expect_cat .cache/liveapp "report: unknown app cache, in use"
unset PLAN
DISKREAP_DEBUG=1 "$B" scan --level low --json >"$T/plan.json" 2>/dev/null   # clean acts on the last plan

"$B" clean --apply -c build-output -c worktree -c cache -c log -c tmp -c backup -c offloaded >/dev/null 2>&1
gone() { if [ -e "$H/$1" ] || { [[ "$1" == /* ]] && [ -e "$1" ]; }; then echo "FAIL  $1 still exists"; fail=1; else echo "ok    $1 removed"; fi; }
kept() { if [ -e "$H/$1" ]; then echo "ok    $1 kept"; else echo "FAIL  $1 was removed"; fail=1; fi; }
gone repo/node_modules
gone exported/daemon/target
gone repo/artifacts/run1/hub/node_modules
gone wt-merged
gone .cache/huggingface/hub/models--old
kept .cache/huggingface/hub/models--new
kept wt-unmerged
kept wt-stashed
kept wt-envdiff/.env
kept svc/venv
kept .local/share/uv/tools/ytdl
kept apps/tool/venv
kept active/target
kept repo/app.log
gone "$T/tmp/stale-build"
gone "$T/tmp/sess/old-a"; gone "$T/tmp/sess2/other"; gone "$T/tmp/claude-1000/proj/sess/scratch-old"
for f in sess/input/blob sess/live/blob sess2/mod/x.mjs claude-1000/proj/sess/live.txt deep/a/b/c/old/blob; do
  [ -e "$T/tmp/$f" ] && echo "ok    $f kept" || { echo "FAIL  $f removed"; fail=1; }
done
gone .cache/oldapp; kept .cache/liveapp
gone db-backups/host1/unchain-20261101-043001.dump; gone db-backups/host1/unchain-20261102-043001.dump
kept db-backups/host1/unchain-20261103-043001.dump; kept db-backups/stubs/site-20261101.tar
gone archive/run1; kept archive/run2
[ -S "$T/tmp/live-session/sock" ] && echo "ok    live-session socket kept" || { echo "FAIL  socket dir removed"; fail=1; }
git -C "$H/repo" rev-parse -q --verify wt-merged >/dev/null && echo "ok    branch wt-merged kept" || { echo "FAIL  branch deleted"; fail=1; }
# Fast rule: worktree_merged_idle_minutes + protect_worktree_branches (config file), scan then dry run then apply.
echo '{"worktree_merged_idle_minutes": 60, "protect_worktree_branches": ["keepme-*"]}' >"$T/fast.json"
DISKREAP_CONFIG="$T/fast.json" "$B" scan --level low --json >"$T/plan2.json"
PLAN="$T/plan2.json"
expect wt-recent ok
expect wt-recentdirty "skip: 1 uncommitted"
expect wt-recentahead "skip: HEAD is not an ancestor"
expect keepme-recent "skip: branch keepme-recent protected"
[ "$(jq -r '[.review[] | select(.path|endswith("wt-recentdirty")) | .kind] | first' "$T/plan2.json")" = dirty ] && echo "ok    review lists the dirty worktree" || { echo "FAIL  review: dirty"; fail=1; }
[ "$(jq -r '[.review[] | select(.path|endswith("wt-recentahead")) | .kind] | first' "$T/plan2.json")" = unmerged ] && echo "ok    review lists the unmerged worktree" || { echo "FAIL  review: unmerged"; fail=1; }
[ "$(jq -r '[.review[] | select(.path|endswith("wt-recent") or endswith("keepme-recent"))] | length' "$T/plan2.json")" = 0 ] && echo "ok    review omits clean merged worktrees" || { echo "FAIL  review: clean listed"; fail=1; }
DISKREAP_CONFIG="$T/fast.json" "$B" clean -c worktree >"$T/dry.txt"
grep -q "would remove-worktree.*wt-recent .*branch wt-recent kept" "$T/dry.txt" && echo "ok    dry run says why" || { echo "FAIL  dry run reason"; cat "$T/dry.txt"; fail=1; }
DISKREAP_CONFIG="$T/fast.json" "$B" scan --level low | grep -q "WORKTREES NEEDING A HUMAN" && echo "ok    scan prints the report section" || { echo "FAIL  report section"; fail=1; }
DISKREAP_CONFIG="$T/fast.json" "$B" clean --apply -c worktree >/dev/null
gone wt-recent
kept wt-recentdirty; kept wt-recentahead; kept keepme-recent
git -C "$H/repo" rev-parse -q --verify wt-recent >/dev/null && echo "ok    fast rule keeps the branch" || { echo "FAIL  fast rule deleted branch"; fail=1; }
echo '{"worktree_merged_idle_minutes": 5}' >"$T/bad.json"
rc=0; DISKREAP_CONFIG="$T/bad.json" "$B" scan >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] && echo "ok    worktree_merged_idle_minutes below the floor exits 2" || { echo "FAIL  bad minutes rc=$rc"; fail=1; }
# Operator free-space floor. The scratch fs may have more than half free (a floor is capped at half
# the filesystem), so assert on the auto target, which a floor always raises: floor + 25%.
tgt() { "$B" status --json | sed -n 's/.*"target_avail_bytes":\([0-9]*\).*/\1/p'; }
base=$(tgt)
[ "$(DISKREAP_MIN_FREE_GIB=100000000 tgt)" -gt "$base" ] && echo "ok    min_free_gib env raises the auto target" || { echo "FAIL  env floor"; fail=1; }
mkdir -p "$H/.config/diskreap" && echo '{"min_free_gib": 100000000}' >"$H/.config/diskreap/config.json"
[ "$(tgt)" -gt "$base" ] && echo "ok    min_free_gib config file raises the auto target" || { echo "FAIL  config floor"; fail=1; }
rm "$H/.config/diskreap/config.json"
rc=0; DISKREAP_MIN_FREE_GIB=lots "$B" status >/dev/null 2>&1 || rc=$?
[ "$rc" = 2 ] && echo "ok    malformed min_free_gib exits 2" || { echo "FAIL  malformed floor rc=$rc"; fail=1; }
# Still critical after the safe cleanup: marker + exit 3; cleared once space is fine. An empty PATH
# keeps auto away from this machine's docker; DISKREAP_TEST_AVAIL_BYTES fakes the free space.
M="$T/state/needs-supervision.json"
mkdir -p "$T/nobin"
rc=0; PATH="$T/nobin" DISKREAP_TEST_AVAIL_BYTES=1 "$B" auto --min-level critical >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] && echo "ok    still critical: auto exits 3" || { echo "FAIL  critical auto rc=$rc"; fail=1; }
jq -e '.level == "critical" and .avail_bytes == 1 and .total_bytes > 0 and (.host|length) > 0 and .since > 0 and .updated >= .since and (.version|length) > 0' "$M" >/dev/null \
  && echo "ok    marker written" || { echo "FAIL  marker: $(cat "$M" 2>&1)"; fail=1; }
since=$(jq .since "$M"); sleep 1.1
PATH="$T/nobin" DISKREAP_TEST_AVAIL_BYTES=1 "$B" auto --min-level critical >/dev/null 2>&1 || true
[ "$(jq .since "$M")" = "$since" ] && [ "$(jq .updated "$M")" -gt "$since" ] && echo "ok    marker keeps since" || { echo "FAIL  since not kept"; fail=1; }
[ "$(DISKREAP_TEST_AVAIL_BYTES=1 "$B" status --json 2>/dev/null | jq -r .needs_supervision.level)" = critical ] && echo "ok    status --json carries needs_supervision" || { echo "FAIL  status needs_supervision"; fail=1; }
rc=0; PATH="$T/nobin" DISKREAP_TEST_AVAIL_BYTES=$((1 << 62)) "$B" auto --min-level critical >/dev/null 2>&1 || rc=$?
[ "$rc" = 0 ] && [ ! -e "$M" ] && echo "ok    fine again: marker cleared, exit 0" || { echo "FAIL  marker not cleared rc=$rc"; fail=1; }
[ "$("$B" status --json | jq -c .needs_supervision)" = null ] && echo "ok    status --json needs_supervision null" || { echo "FAIL  status not null"; fail=1; }
exit $fail
