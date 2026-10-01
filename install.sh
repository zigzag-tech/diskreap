#!/usr/bin/env bash
# install.sh — build diskreap, schedule `diskreap auto` hourly, link the agent skill.
#
#   ./install.sh                 # binary + timer + skill links
#   ./install.sh --claude-hook   # also add a Claude Code SessionStart hook that
#                                # warns agents when the disk is low
#   ./install.sh --no-timer      # binary + skill only
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
timer=1
hook=0
for a in "$@"; do
  case "$a" in
    --no-timer) timer=0 ;;
    --claude-hook) hook=1 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

cargo install --quiet --locked --path "$here" --force
bin="$(command -v diskreap || echo "$HOME/.cargo/bin/diskreap")"
echo "installed $bin"

# Agent skill: link into every agent skill dir that exists.
for d in "$HOME/.claude/skills" "$HOME/.codex/skills" "$HOME/.agents/skills"; do
  [ -d "$d" ] || continue
  [ -e "$d/disk-cleanup" ] && continue # already provided (e.g. by a synced dotfiles repo)
  ln -sfn "$here/skills/disk-cleanup" "$d/disk-cleanup"
  echo "linked $d/disk-cleanup"
done

if [ "$timer" = 1 ]; then
  case "$(uname -s)" in
    Linux)
      ud="$HOME/.config/systemd/user"
      mkdir -p "$ud"
      sed "s#@BIN@#$bin#" "$here/deploy/diskreap.service" > "$ud/diskreap.service"
      cp "$here/deploy/diskreap.timer" "$ud/diskreap.timer"
      systemctl --user daemon-reload
      systemctl --user enable --now diskreap.timer
      if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != "yes" ]; then
        echo "note: user timers stop at logout; run 'sudo loginctl enable-linger $USER' to keep them running"
      fi
      ;;
    Darwin)
      plist="$HOME/Library/LaunchAgents/io.github.diskreap.auto.plist"
      mkdir -p "$HOME/Library/LaunchAgents" "$HOME/.local/state/diskreap"
      sed -e "s#@BIN@#$bin#" -e "s#@HOME@#$HOME#g" "$here/deploy/io.github.diskreap.auto.plist" > "$plist"
      # gui/<uid> exists only with a login session; over SSH fall back to user/<uid>.
      loaded=0
      for dom in "gui/$(id -u)" "user/$(id -u)"; do
        launchctl bootout "$dom" "$plist" 2>/dev/null || true
        if launchctl bootstrap "$dom" "$plist" 2>/dev/null; then loaded=1; echo "launchd: loaded in $dom"; break; fi
      done
      if [ "$loaded" = 0 ]; then
        # Over SSH with no GUI session the launchd domains refuse (125 / 5): use cron.
        rm -f "$plist"
        line="17 * * * * PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin $bin auto --min-level critical >> $HOME/.local/state/diskreap/auto.log 2>&1"
        { crontab -l 2>/dev/null | grep -v 'diskreap auto'; echo "$line"; } | crontab -
        echo "launchd unavailable (no GUI session) — scheduled via crontab instead"
      fi
      ;;
  esac
  echo "scheduled: diskreap auto, hourly"
fi

if [ "$hook" = 1 ]; then
  settings="$HOME/.claude/settings.json"
  command -v jq >/dev/null || { echo "jq is required for --claude-hook" >&2; exit 1; }
  [ -f "$settings" ] || echo '{}' > "$settings"
  # Literal $HOME (the hook runs in a shell): one settings.json can serve
  # machines whose homes differ (/home/x vs /Users/x). Never fail a session.
  hbin="$bin"
  case "$bin" in "$HOME"/*) hbin="\$HOME/${bin#"$HOME"/}" ;; esac
  cmd="\"$hbin\" status --hook 2>/dev/null || true"
  tmp="$(mktemp)"
  # Replace any earlier diskreap hook (older path/format), then add ours once.
  jq --arg c "$cmd" '
    .hooks.SessionStart = (
      [(.hooks.SessionStart // [])[]
        | .hooks = [.hooks[]? | select((.command // "") | contains("diskreap status --hook") | not)]
        | select(.hooks | length > 0)]
      + [{"hooks":[{"type":"command","command":$c,"timeout":5}]}])' \
    "$settings" > "$tmp" && cat "$tmp" > "$settings" && rm -f "$tmp"
  echo "Claude Code SessionStart hook: $cmd"
fi
