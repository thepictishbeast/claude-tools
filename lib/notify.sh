#!/usr/bin/env bash
# notify: one way for every monitor on this host to reach Paul.
#
# Before this, each emitter invented its own delivery and its own idea of
# "don't repeat yourself". log-watch learned that lesson the expensive
# way — two stale transient units produced 261 identical emails, 43% of
# the inbox, about a host that was healthy throughout. Rebuilding that
# logic per emitter means rediscovering the same bug per emitter.
#
# So the dedup lives here, once:
#
#   * a NEW condition notifies immediately
#   * the SAME condition stays silent while it is unchanged
#   * a still-unresolved condition is repeated once a day, so a real
#     problem is not silently forgotten
#   * when it clears, exactly one all-clear, then silence
#
# Delivery is ntfy for push, because email is where alerts went to die:
# external-monitors failed daily for four months into an inbox nobody
# could distinguish from the rest of the GitHub noise. Email remains as
# the fallback, since an alert that cannot be delivered is worse than a
# noisy one.
#
#   notify.sh --key <id> --title <t> [--priority 1-5] [--tags a,b] <<< body
#   notify.sh --resolve <id>            # send the all-clear, clear state
#
# Exit 0 on delivery or on a deliberate suppression; 1 only if every
# delivery path failed.
set -uo pipefail

TOPIC="${NTFY_TOPIC:-plausiden-alerts}"
URL="${NTFY_URL:-http://127.0.0.1:2586}"
TOKEN_FILE="${NTFY_TOKEN_FILE:-/tank/secrets/ntfy-alerts.txt}"
STATE_DIR="${NOTIFY_STATE:-/var/lib/notify}"
MAIL_TO="${NOTIFY_MAIL:-william@plausiden.com}"
REMIND_SEC="${NOTIFY_REMIND_SEC:-86400}"

key=""; title=""; prio="default"; tags=""; resolve=""
while [ $# -gt 0 ]; do
  case "$1" in
    --key)      key="$2"; shift 2 ;;
    --title)    title="$2"; shift 2 ;;
    --priority) prio="$2"; shift 2 ;;
    --tags)     tags="$2"; shift 2 ;;
    --topic)    TOPIC="$2"; shift 2 ;;
    --resolve)  resolve="$2"; key="$2"; shift 2 ;;
    *) echo "notify: unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$key" ] || { echo "notify: --key is required (it is what makes dedup possible)" >&2; exit 2; }

mkdir -p "$STATE_DIR" 2>/dev/null || STATE_DIR=$(mktemp -d)
# A key may name a unit or a URL, so it cannot be a filename as-is.
safe=$(printf '%s' "$key" | md5sum | cut -c1-32)
statef="$STATE_DIR/$safe"

token=""
[ -r "$TOKEN_FILE" ] && token=$(sed -n 's/^token: //p' "$TOKEN_FILE" | head -1)

push() { # <title> <body> <tags> <priority>
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 \
    ${token:+-H "Authorization: Bearer $token"} \
    -H "Title: $1" ${3:+-H "Tags: $3"} ${4:+-H "Priority: $4"} \
    -d "$2" "$URL/$TOPIC" 2>/dev/null)
  [ "$code" = "200" ]
}

mail_it() { # <subject> <body>
  { echo "Subject: $1"; echo "To: $MAIL_TO"; echo; printf '%s\n' "$2"
  } | sendmail "$MAIL_TO" 2>/dev/null
}

deliver() { # <title> <body> <tags> <priority>
  if push "$1" "$2" "$3" "$4"; then return 0; fi
  # ntfy is on this host. If the host is the thing that is broken, push
  # is exactly what stops working — so never let that swallow an alert.
  echo "notify: ntfy unreachable, falling back to mail" >&2
  mail_it "$1" "$2"
}

if [ -n "$resolve" ]; then
  # Only speak if there was something to resolve. An all-clear for a
  # problem that never happened is noise of its own.
  [ -f "$statef" ] || { echo "notify: nothing outstanding for '$key'"; exit 0; }
  deliver "all clear: $key" "The condition previously reported for '$key' has cleared." \
          "white_check_mark" "low"
  rm -f "$statef"
  exit $?
fi

body=$(cat)
[ -n "$title" ] || title="$key"
sig=$(printf '%s' "$body" | md5sum | cut -c1-32)
now=$(date +%s)

if [ -f "$statef" ]; then
  prev_sig=$(sed -n 1p "$statef"); prev_at=$(sed -n 2p "$statef")
  if [ "$sig" = "$prev_sig" ]; then
    age=$(( now - ${prev_at:-0} ))
    if [ "$age" -lt "$REMIND_SEC" ]; then
      echo "notify: unchanged since ${age}s ago — staying quiet"
      exit 0
    fi
    title="$title (still unresolved)"
  fi
fi

deliver "$title" "$body" "$tags" "$prio"
rc=$?
printf '%s\n%s\n' "$sig" "$now" > "$statef"
exit $rc
