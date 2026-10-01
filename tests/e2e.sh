#!/usr/bin/env bash
# End-to-end: build a throwaway $HOME with real git repos/worktrees and check
# every verdict, then apply and check what is gone and what survived.
#   tests/e2e.sh [path/to/diskreap]
set -euo pipefail

B="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/diskreap}"
T="$(mktemp -d)"
trap 'chmod -R u+w "$T" 2>/dev/null; rm -rf "$T"' EXIT
H="$T/home"
mkdir -p "$H"
mkdir -p "$T/tmp"
export TMPDIR="$T/tmp" HOME="$H" DISKREAP_STATE="$T/state" GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
big() { mkdir -p "$(dirname "$1")"; head -c "$2" /dev/urandom >"$1"; }

cd "$H"
git init -q -b main repo
cd repo
printf 'artifacts/\nnode_modules/\n*.log\n.env\nvenv/\n' >.gitignore
echo '{}' >package.json
echo 'A=1' >.env
git add .gitignore package.json && git commit -qm init
big node_modules/a/big 30M
big artifacts/run1/hub/node_modules/x/big 20M
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

# Age everything 60 days, then make one model recently used.
chmod -R u+w "$H/repo/artifacts"
find "$H" "$T/tmp" -mindepth 1 -exec touch -h -a -m -d '60 days ago' {} +
chmod -R a-w "$H/repo/artifacts/run1/hub"
touch -a "$H/.cache/huggingface/hub/models--new/w"

cd /
DISKREAP_DEBUG=1 "$B" scan --level low --json >"$T/plan.json"
v() { local p="$1"; [[ "$p" == /* ]] || p="$H/$p"; jq -r --arg p "$p" '.items[] | select(.path==$p) | if .ok then "ok" else "skip: " + .why end' "$T/plan.json"; }
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
expect svc/venv "skip: project referenced by"
expect plain/node_modules "skip: not inside a git repo"
expect exported/daemon/target ok
expect .cache/huggingface/hub ok
expect "$T/tmp/stale-build" ok
expect "$T/tmp/live-session" "skip: holds a socket"

"$B" clean --apply -c build-output -c worktree -c cache -c log -c tmp >/dev/null
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
kept repo/app.log
gone "$T/tmp/stale-build"
[ -S "$T/tmp/live-session/sock" ] && echo "ok    live-session socket kept" || { echo "FAIL  socket dir removed"; fail=1; }
git -C "$H/repo" rev-parse -q --verify wt-merged >/dev/null && echo "ok    branch wt-merged kept" || { echo "FAIL  branch deleted"; fail=1; }
exit $fail
