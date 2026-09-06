#!/usr/bin/env bash
# One daily read of what the machine did, and what is waiting on Paul.
#
# There are now nine timers writing to seven ntfy topics. Each is right
# to be separate — you mute cert alerts without going deaf to the contact
# form — but nobody should have to visit seven places to answer "is
# anything wrong, and is anything waiting on me".
#
# This is NOT the escalation path. Urgent things go through escalate.sh
# the moment they happen, in three categories, and never wait for a
# digest. This is the other half: the once-a-day picture, sent whether or
# not anything is wrong, because a digest that only arrives with bad news
# trains you to feel dread when it appears and to assume silence means
# healthy — and silence is exactly what a broken timer produces.
#
# It reports what it could not determine as its own line rather than
# omitting it. A digest that quietly drops a check it failed to read is
# the same failure as a monitor that has never been green.
#
#   loop-digest.sh [--notify]
set -uo pipefail

NOTIFY="${DIGEST_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
TOPIC="${DIGEST_TOPIC:-plausiden-alerts}"
SEND=0
[ "${1:-}" = "--notify" ] && SEND=1

line(){ printf '%s\n' "$*"; }
out=""
add(){ out="${out}$*"$'\n'; }

add "[automated — the Claude loop on plausiden-prime, not a person]"
add ""
add "$(date -u '+%A %d %B, %H:%M UTC')"
add ""

# ── Did the machinery run at all ────────────────────────────────────────
add "WATCHERS"
stale=0; failed=0
for u in cert-watch site-watch backup-verify auto-remediate inquiry-probe \
         git-acl-guard referral-report prospect-refresh; do
  systemctl list-unit-files "$u.service" >/dev/null 2>&1 || continue
  res=$(systemctl show "$u.service" -p Result --value 2>/dev/null)
  ts=$(systemctl show "$u.service" -p ExecMainExitTimestamp --value 2>/dev/null)
  if [ -z "$ts" ]; then
    if systemctl is-enabled "$u.timer" >/dev/null 2>&1; then
      # Ask systemd for the next elapse directly. Parsing columns out of
      # `list-timers` is fragile — the field positions shift with the
      # locale and with how long the unit names are.
      us=$(systemctl show "$u.timer" -p NextElapseUSecRealtime --value 2>/dev/null)
      nxt=""
      case "$us" in
        *[0-9]*) nxt=$(date -d "@$(( ${us%%[!0-9]*} / 1000000 ))" '+%a %d %b %H:%M' 2>/dev/null) ;;
      esac
      add "  $u — scheduled${nxt:+ for $nxt}, not yet due"
    else
      add "  $u — has never run and has no timer"; stale=$((stale+1))
    fi
    continue
  fi
  age_h=$(( ( $(date +%s) - $(date -d "$ts" +%s 2>/dev/null || echo 0) ) / 3600 ))
  case "$res" in
    success) [ "$age_h" -gt 48 ] && { add "  $u — last ran ${age_h}h ago, which is longer than its interval"; stale=$((stale+1)); } ;;
    *) add "  $u — last run: $res"; failed=$((failed+1)) ;;
  esac
done
[ "$stale" -eq 0 ] && [ "$failed" -eq 0 ] && add "  all ran on schedule and exited clean"

# ── The contact form, because a missed enquiry is the expensive one ─────
add ""
add "CONTACT FORM"
DB=/var/lib/plausiden-site/feedback.db
if [ -r "$DB" ]; then
  real=$(sqlite3 -readonly "$DB" "select count(*) from inquiry where name not like 'Automated capture probe%' and received_at > datetime('now','-1 day')" 2>/dev/null)
  caught=$(sqlite3 -readonly "$DB" "select count(*) from inquiry where (notified=0 or notified='false') and name not like 'Automated capture probe%'" 2>/dev/null)
  add "  ${real:-?} submission(s) in the last day, ${caught:-?} caught-and-not-notified in total"
  if [ "${real:-0}" -gt 0 ]; then
    # Collect into the accumulator rather than printing. A `while read`
    # pipeline runs in a subshell, so anything it echoes escapes $out
    # entirely and lands above the header.
    rows=$(sqlite3 -readonly "$DB" \
      "select '    '||verdict||'  '||substr(name,1,30) from inquiry where name not like 'Automated capture probe%' and received_at > datetime('now','-1 day')" 2>/dev/null)
    [ -n "$rows" ] && add "$rows"
  fi
else
  add "  could not read the inquiry store — that is a finding, not a quiet day"
fi

# ── Outbound, which is the point of the exercise ───────────────────────
add ""
add "OUTBOUND"
SHEET=/var/lib/plausiden-outreach/callsheet.tsv
if [ -r "$SHEET" ]; then
  tot=$(( $(grep -c . "$SHEET") - 1 ))
  ph=$(awk -F'\t' 'NR>1 && $6!=""' "$SHEET" | wc -l)
  urgent=$(awk -F'\t' 'NR>1 && ($8 ~ /expired/ || $8 ~ /expires in/)' "$SHEET" | wc -l)
  add "  $tot practices with findings, $ph reachable by phone"
  [ "$urgent" -gt 0 ] && add "  $urgent with a certificate expired or expiring — these decay, the rest do not"
else
  add "  no call sheet yet"
fi

# ── What is blocked on Paul, stated plainly every single day ───────────
add ""
add "WAITING ON YOU"
blocked=0
[ -r /tank/secrets/reddit.env ] || { add "  reddit.env — the only channel where automated posting is permitted"; blocked=$((blocked+1)); }
[ -r /tank/secrets/meta.env ]   || { add "  meta.env — one token covers the Page and the ads API"; blocked=$((blocked+1)); }
[ "$blocked" -eq 0 ] && add "  nothing — both credential files are in place"

add ""
add "Sent once a day whether or not anything is wrong, so that silence"
add "from this digest means the digest itself has stopped, rather than"
add "meaning everything is fine."

printf '%s' "$out"
if [ "$SEND" = 1 ] && [ -x "$NOTIFY" ]; then
  # A daily heartbeat must not be deduplicated into silence, so the body
  # carries the date and therefore always differs.
  printf '%s' "$out" | "$NOTIFY" --key loop-digest --title "Daily: $( [ "$failed" -gt 0 ] && echo "$failed watcher(s) failed" || echo "all watchers clean" )" \
    --priority low --tags newspaper --topic "$TOPIC" >/dev/null 2>&1
fi
exit 0
