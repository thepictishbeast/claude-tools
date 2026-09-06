#!/usr/bin/env bash
# Surface the few things Paul must not miss, and nothing else.
#
# A loop that reports everything is a loop nobody reads, and the endpoint
# of that is the 261 identical emails about a healthy host. But a loop
# that reports nothing hides the two categories that genuinely need a
# person, so silence is not the answer either.
#
# Only three things qualify. Everything else stays in the task list and
# the commit log, where it can be found when wanted rather than pushed
# when not.
#
#   DECISION   work is stopped until Paul chooses. Not "would be nice to
#              confirm" — actually blocked, with the options named.
#   RISK       something is live and wrong, or about to be: a client
#              could be affected, money is moving, data is exposed.
#   TIME       a window is closing. A certificate expiring, a dispute
#              period running out. Useless if it arrives after.
#
# Every message says outright that it came from an automated loop and
# not from a person typing. Paul asked for that explicitly and it is the
# right default anyway: a human should never have to work out whether
# they are reading a person or a process, particularly at 3am on a phone.
#
#   escalate.sh --kind decision|risk|time --title "..." [--deadline "..."] <<< body
set -uo pipefail

NOTIFY="${ESCALATE_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
TOPIC="${ESCALATE_TOPIC:-plausiden-alerts}"
LOG="${ESCALATE_LOG:-/var/lib/claude-loop/escalations.jsonl}"
KIND=""; TITLE=""; DEADLINE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KIND="$2"; shift 2 ;;
    --title) TITLE="$2"; shift 2 ;;
    --deadline) DEADLINE="$2"; shift 2 ;;
    *) echo "escalate: unknown argument $1" >&2; exit 2 ;;
  esac
done
case "$KIND" in
  decision|risk|time) : ;;
  *) echo "escalate: --kind must be decision, risk or time — if it is none of those, it is not an escalation" >&2; exit 2 ;;
esac
[ -n "$TITLE" ] || { echo "escalate: --title is required" >&2; exit 2; }

body=$(cat)
[ -n "$body" ] || { echo "escalate: body is empty" >&2; exit 2; }

case "$KIND" in
  decision) prefix="NEEDS YOUR DECISION"; prio="high";  tag="raised_hand" ;;
  risk)     prefix="LIVE RISK";           prio="max";   tag="rotating_light" ;;
  time)     prefix="CLOSING WINDOW";      prio="high";  tag="hourglass" ;;
esac

# The self-identifying line. It is first, not buried in a signature,
# because the only question worth answering immediately is "is a person
# waiting on the other end of this".
msg="[automated — written by the Claude loop on plausiden-prime, not by a person]

${body}"
[ -n "$DEADLINE" ] && msg="${msg}

Deadline: ${DEADLINE}"
msg="${msg}

No one is waiting on a reply in real time. This was pushed because it
matched one of three categories that are not allowed to sit in a log:
a decision that blocks work, something live and wrong, or a window
about to close."

mkdir -p "$(dirname "$LOG")" 2>/dev/null
printf '{"ts":"%s","kind":"%s","title":%s}\n' \
  "$(date -u +%FT%TZ)" "$KIND" "$(printf '%s' "$TITLE" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
  >> "$LOG" 2>/dev/null

printf '%s\n' "$msg" | "$NOTIFY" --key "escalate:$KIND:$TITLE" \
  --title "$prefix — $TITLE" --priority "$prio" --tags "$tag" --topic "$TOPIC"
