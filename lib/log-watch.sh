#!/usr/bin/env bash
# log-watch: hourly anomaly scan across ALL projects' operational surfaces.
#   1. Caddy access logs  - 5xx spike per vhost in the last hour
#   2. Caddy error output - TLS/cert errors in the last hour (journald)
#   3. systemd            - failed units (anything, incl. per-project services)
#   4. disk               - root or /tank above 90%
# Regressions email NOTIFY via local sendmail. Companion to site-gate.sh
# (external checks); this is the inside view. Installed by claude-tools;
# run via claude-logwatch.timer.
set -uo pipefail
NOTIFY="william@plausiden.com"
WINDOW_MIN=65
issues=()

# 1. 5xx spike per vhost access log (JSON or console format both match ' 5[0-9][0-9] ')
for log in /var/log/caddy/*.log; do
  [[ -f "$log" ]] || continue
  # only lines from the last hour: cheap approach - tail the recent chunk
  n5xx=$(tail -n 5000 "$log" | grep -cE '"status":5[0-9]{2}|" 5[0-9]{2} ') || true
  if (( n5xx > 20 )); then
    issues+=("$(basename "$log"): ${n5xx} 5xx responses in recent traffic")
  fi
done

# 2. caddy service errors in the last hour
cerr=$(journalctl -u caddy --since "-${WINDOW_MIN}min" -p err --no-pager -q 2>/dev/null | tail -5)
[[ -n "$cerr" ]] && issues+=("caddy journal errors:
$cerr")

# 3. failed units
failed=$(systemctl --failed --no-legend --plain 2>/dev/null | awk '{print $1}')
[[ -n "$failed" ]] && issues+=("failed systemd units:
$failed")

# 4. disk
# every real mounted filesystem (found the hard way: /home filled while
# only / and /tank were watched, 2026-07-13)
while read -r mount use; do
  use=${use%\%}
  (( use >= 90 )) && issues+=("$mount at ${use}% disk usage")
done < <(df --output=target,pcent -x tmpfs -x devtmpfs -x overlay 2>/dev/null | tail -n +2)

# ---------------------------------------------------------------------------
# Mail on CHANGE, not on state. This ran hourly and mailed whenever any issue
# existed, so one persistent condition became mail forever: two stale transient
# `run-*.service` units from a failed `caddy reload` produced 261 identical
# messages, 43% of the inbox, about a host that was healthy the whole time.
#
# Now: alert when the anomaly set changes, remind once a day while it persists
# so a real problem cannot be silently forgotten, and say so when it clears.
# ---------------------------------------------------------------------------
STATE="${LOGWATCH_STATE:-/var/lib/log-watch/state}"
REMIND_SEC="${LOGWATCH_REMIND_SEC:-86400}"
mkdir -p "$(dirname "$STATE")" 2>/dev/null || STATE=/tmp/.log-watch-state

send(){ # send <subject> <body>
  { echo "Subject: $1"; echo "To: $NOTIFY"; echo ""; printf '%s\n' "$2"
    echo; echo "-- log-watch, $(date -u +%FT%TZ)"
  } | sendmail "$NOTIFY" 2>/dev/null || echo "WARN: sendmail failed" >&2
}

if ((${#issues[@]})); then
  body=$(printf '%s\n\n' "${issues[@]}")
  now_sum=$(printf '%s' "$body" | md5sum | cut -d' ' -f1)
  prev_sum=$(cut -d' ' -f1 < "$STATE" 2>/dev/null || echo "")
  prev_at=$(cut -d' ' -f2 < "$STATE" 2>/dev/null || echo 0)
  age=$(( $(date +%s) - ${prev_at:-0} ))

  if [[ "$now_sum" != "$prev_sum" ]]; then
    send "[log-watch] ${#issues[@]} anomaly(ies) on $(hostname)" "$body"
    echo "ANOMALIES: ${#issues[@]} (changed — mailed $NOTIFY)"
  elif (( age >= REMIND_SEC )); then
    send "[log-watch] still unresolved after $((age/3600))h on $(hostname)" \
         "$body
These are the SAME anomalies as the last alert. Daily reminder only."
    echo "ANOMALIES: ${#issues[@]} (unchanged ${age}s — daily reminder sent)"
  else
    echo "ANOMALIES: ${#issues[@]} (unchanged, already reported — no mail)"
  fi
  # refresh the timestamp only when we actually mailed, so the daily reminder
  # measures time since the last ALERT rather than since the last scan
  if [[ "$now_sum" != "$prev_sum" ]] || (( age >= REMIND_SEC )); then
    printf '%s %s\n' "$now_sum" "$(date +%s)" > "$STATE"
  else
    printf '%s %s\n' "$now_sum" "${prev_at:-$(date +%s)}" > "$STATE"
  fi
  printf ' - %s\n' "${issues[@]}" | head -10
  exit 1
fi

# recovered: tell him once, then go quiet
if [[ -s "$STATE" ]]; then
  send "[log-watch] all clear on $(hostname)" "Previously reported anomalies have cleared."
  : > "$STATE"
  echo "log-watch: recovered (mailed all-clear)"
fi
echo "log-watch: clean ($(date -u +%FT%TZ))"
