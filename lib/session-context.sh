#!/bin/sh
# session-context.sh — SessionStart context injector (Agent OS subsystem S).
# Surfaces the durable agent-registry into every new session so Claude starts
# knowing its standing projects/goals + how to query them, instead of
# rediscovering them from scratch. Emits SessionStart `additionalContext` JSON.
# Fast + fail-safe: prints `{}` and exits 0 if the registry isn't available.

BIN="${AGENT_REGISTRY_BIN:-$HOME/.local/bin/agent-registry}"
DB="${AGENT_REGISTRY_DB:-$HOME/.agent-registry/registry.db}"
command -v "$BIN" >/dev/null 2>&1 || BIN="agent-registry"
if ! command -v "$BIN" >/dev/null 2>&1 || [ ! -f "$DB" ]; then echo '{}'; exit 0; fi

stats=$("$BIN" stats 2>/dev/null | grep -v '^-' | grep -v '^total' | awk '{printf "%s/%s:%s  ", $1,$2,$3}')
total=$("$BIN" stats 2>/dev/null | awk '/^total/{print $2}')
# Active focus first: anything in_progress, then priority 1-2 open items.
active=$("$BIN" list --status in_progress --limit 10 --oneline 2>/dev/null | cut -f4)
[ -n "$active" ] || active=$("$BIN" list --priority 1 --status open --limit 6 --oneline 2>/dev/null | cut -f4)
active_block=""
[ -n "$active" ] && active_block=$(printf '%s\n' "$active" | sed 's/^/  - /')

ctx="DURABLE REGISTRY (agent-registry) — ${total:-0} tracked items across all projects/goals/tasks/wishlist.
Counts: ${stats}
Query anytime: agent-registry list --kind project|task|wishlist|check --status open --tag X --search Y --priority N ; stats ; show <id|slug> ; add \"...\" ; done <id>
This is the cross-session backbone: put new projects/goals/wishlist/checks here (they persist), not just the ephemeral session task list."
[ -n "$active_block" ] && ctx="$ctx
Active focus:
$active_block"

if command -v jq >/dev/null 2>&1; then
  jq -n --arg c "$ctx" '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$c}}'
else
  # jq-less fallback: minimal manual JSON escaping of newlines + quotes
  esc=$(printf '%s' "$ctx" | sed 's/\\/\\\\/g; s/"/\\"/g' | awk 'BEGIN{ORS="\\n"}{print}')
  printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$esc"
fi
exit 0
