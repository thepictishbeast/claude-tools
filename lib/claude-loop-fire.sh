#!/bin/sh
# claude-loop-fire.sh — local pre-fire gate for an OS-cron-driven Claude loop.
#
# Run this from the OS scheduler (cron / systemd timer), NOT inside the agent.
# It is the piece that makes "stop the loop even when the agent is fully
# API-blocked" actually work: the crash-guard + DISABLED check run LOCALLY (no
# API) BEFORE Claude is invoked, so a halted/disabled loop never even makes the
# API call that would be blocked. Pair it with `claude-loop watchdog` on a
# shorter timer (the watchdog sets DISABLED when a fire is stuck/blocked).
#
#   Usage: claude-loop-fire.sh <label> <prompt-file>
#
# IMPORTANT: because THIS wrapper runs `guard` (recording the per-fire "start"),
# the fire prompt must NOT also run `guard` — it should only do work and end with
# `claude-loop checkpoint --label <label> ...` to close the iteration. (That is,
# build the prompt with `claude-loop prep` WITHOUT --label in this model.)
set -u
label="${1:?usage: claude-loop-fire.sh <label> <prompt-file>}"
promptfile="${2:?usage: claude-loop-fire.sh <label> <prompt-file>}"
BIN="$(command -v claude-loop 2>/dev/null || echo "$HOME/.local/bin/claude-loop")"
CLAUDE="$(command -v claude 2>/dev/null || echo "$HOME/.local/bin/claude")"

# Local gate (no API). guard exits 3 if a prior fire is unmatched/DISABLED.
if ! "$BIN" guard --label "$label"; then
  echo "[claude-loop-fire] $label halted by guard/DISABLED — not firing (no API call)." >&2
  exit 0
fi

# Only now spend an API call. If this headless fire is blocked and dies, it
# never writes a checkpoint -> the "start" stays unmatched -> the watchdog
# disables the loop -> the NEXT run of this wrapper is short-circuited above.
exec "$CLAUDE" -p "$(cat "$promptfile")"
