#!/bin/sh
# session-refresh.sh — SessionStart hook: keep claude-tools + PlausiDen-Meta
# current so every Claude instance works from the same toolkit + governance.
# MUST be fast and never block a session: 15s timeout per pull, always exit 0.
# Installed 2026-07-07 (task #40). Hook JSON snippet lives in META.md §2b.

TOOLS="${CLAUDE_TOOLS_DIR:-$HOME/Development/claude-tools}"
META="${PLAUSIDEN_META_DIR:-$HOME/PlausiDen-Meta}"
LOG=""

refresh() {
  d="$1"; name="$2"
  [ -d "$d/.git" ] || { LOG="$LOG $name:absent"; return; }
  if [ -n "$(git -C "$d" status --porcelain 2>/dev/null)" ]; then
    LOG="$LOG $name:dirty-skipped"; return
  fi
  before=$(git -C "$d" rev-parse HEAD 2>/dev/null)
  if ! timeout 15 git -C "$d" pull --ff-only --quiet 2>/dev/null; then
    LOG="$LOG $name:pull-failed"; return
  fi
  after=$(git -C "$d" rev-parse HEAD 2>/dev/null)
  if [ "$before" != "$after" ]; then
    n=$(git -C "$d" rev-list --count "$before..$after" 2>/dev/null || echo '?')
    LOG="$LOG $name:updated+$n"
  else
    LOG="$LOG $name:current"
  fi
}

refresh "$TOOLS" claude-tools
refresh "$META" meta

case "$LOG" in
  *claude-tools:updated*)
    ( cd "$TOOLS" && sh install.sh -f >/dev/null 2>&1 ) \
      && LOG="$LOG (skills synced; NEW skills load next session; re-read META.md)" \
      || LOG="$LOG (install.sh FAILED - run manually)"
    ;;
esac
case "$LOG" in
  *meta:updated*) LOG="$LOG (PlausiDen-Meta changed - re-read it before ecosystem work)";;
esac

echo "session-refresh:$LOG"
exit 0
