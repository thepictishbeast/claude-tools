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

if ((${#issues[@]})); then
  {
    echo "Subject: [log-watch] ${#issues[@]} anomaly(ies) on $(hostname)"
    echo "To: $NOTIFY"
    echo ""
    printf '%s\n\n' "${issues[@]}"
    echo "-- log-watch, $(date -u +%FT%TZ)"
  } | sendmail "$NOTIFY" 2>/dev/null || echo "WARN: sendmail failed" >&2
  echo "ANOMALIES: ${#issues[@]} (mailed $NOTIFY)"
  printf ' - %s\n' "${issues[@]}" | head -10
  exit 1
fi
echo "log-watch: clean ($(date -u +%FT%TZ))"
