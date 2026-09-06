#!/usr/bin/env bash
# Show what the contact form caught but did not tell anyone about.
#
# Submissions classified as spam or caught by the honeypot are recorded in
# full and deliberately do not notify. That is the right default — a bot
# should not wake anyone — but it creates a pile nobody looks at, and a
# misclassified real enquiry sits in that pile indistinguishable from the
# bots until someone goes looking.
#
# The classifier is a heuristic. It will be wrong occasionally, and being
# wrong in the direction of "silently binned a client" is the expensive
# direction. This exists so the wrongness is visible without anyone having
# to remember to check.
#
# Reports only what is NEW since the last run, so it stays readable. A
# digest that repeats itself is one people stop opening — which is how the
# 261 identical log-watch emails happened.
#
#   inquiry-digest.sh [--days 7] [--notify] [--all]
set -uo pipefail

DB="${DIGEST_DB:-/var/lib/plausiden-site/feedback.db}"
STATE="${DIGEST_STATE:-/var/lib/inquiry-digest/last-id}"
NOTIFY="${DIGEST_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
TOPIC="${DIGEST_TOPIC:-plausiden-site}"
DAYS="${DIGEST_DAYS:-7}"
SEND=0; ALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --days) DAYS="$2"; shift 2 ;;
    --notify) SEND=1; shift ;;
    --all) ALL=1; shift ;;
    *) echo "inquiry-digest: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -r "$DB" ] || { echo "inquiry-digest: cannot read $DB as $(id -un)" >&2; exit 2; }
mkdir -p "$(dirname "$STATE")" 2>/dev/null
last=0; [ "$ALL" = 0 ] && [ -f "$STATE" ] && last=$(cat "$STATE" 2>/dev/null || echo 0)

# Probe rows are ours and would otherwise dominate the digest.
rows=$(sqlite3 -readonly "$DB" \
  "select id||'|'||substr(received_at,1,16)||'|'||coalesce(verdict,'?')||'|'||coalesce(spam_score,'?')||'|'||
          replace(substr(coalesce(name,'-'),1,26),'|','/')||'|'||
          replace(substr(coalesce(reply_to,'-'),1,30),'|','/')||'|'||
          replace(substr(coalesce(message,''),1,90),'|','/')
   from inquiry
   where (notified = 0 or notified = 'false')
     and id > $last
     and name not like 'Automated capture probe%'
   order by id" 2>/dev/null)

newest=$(sqlite3 -readonly "$DB" "select coalesce(max(id),0) from inquiry" 2>/dev/null || echo "$last")

if [ -z "$rows" ]; then
  echo "inquiry-digest: nothing new caught since id $last"
  [ "$SEND" = 1 ] && printf '%s' "$newest" > "$STATE"
  exit 0
fi

n=$(printf '%s\n' "$rows" | grep -c .)
body="The contact form caught ${n} submission(s) and did not notify anyone.

Recorded in full and shown here so a misclassified real enquiry does not
sit in the pile unnoticed. If one of these is a person, reply directly —
the address is below.

"
while IFS='|' read -r id ts verdict score name email msg; do
  [ -n "$id" ] || continue
  body="${body}  #${id}  ${ts}  ${verdict} (score ${score})
      from: ${name}  <${email}>
      ${msg}

"
done <<< "$rows"

body="${body}Anything here that should have reached you is a classifier miss worth
knowing about — it is the direction that costs a client rather than a
moment's annoyance."

printf '%s\n' "$body"
if [ "$SEND" = 1 ]; then
  printf '%s\n' "$body" | "$NOTIFY" --key inquiry-digest --title "${n} submission(s) caught, none notified" \
    --priority low --tags mag --topic "$TOPIC" >/dev/null 2>&1
  printf '%s' "$newest" > "$STATE"
fi
exit 0
