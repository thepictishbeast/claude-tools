#!/usr/bin/env bash
# Submit a real inquiry to the live site and prove it reached the store.
#
# The unit tests already prove the code paths: spam is logged and not
# notified, the honeypot is logged and not notified, validation refuses
# what it should. Those tests pass whether or not the deployed service can
# actually write to its database.
#
# Everything that would silently swallow a real client sits BETWEEN the
# code and the disk: a moved database path, a permissions change, a full
# filesystem, a service running as the wrong user, a deploy that shipped
# a stale binary. In every one of those cases the form still returns its
# thank-you page, because the acknowledgement is rendered before the write
# is confirmed — that is deliberate, so a logging failure cannot leak a
# stack trace to a visitor, and it is exactly why the visitor cannot tell
# you the submission was lost.
#
# So this does the only thing that settles it: sends a submission through
# the public form and then looks in the database for it.
#
# Rows are marked PROBE in the name so a human skimming the log can tell
# them from real inquiries at a glance. They are deliberately NOT deleted
# afterwards — a probe that tidies up cannot prove the row persisted, and
# the row is the evidence.
#
#   inquiry-probe.sh [--url URL] [--db PATH] [--notify] [--quiet]
set -uo pipefail

URL="${PROBE_URL:-https://plausiden.com/contact}"
DB="${PROBE_DB:-/var/lib/plausiden-site/feedback.db}"
NOTIFY="${PROBE_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
TOPIC="${PROBE_TOPIC:-plausiden-site}"
WAIT="${PROBE_WAIT:-10}"
SEND=0; QUIET=0
while [ $# -gt 0 ]; do
  case "$1" in
    --url) URL="$2"; shift 2 ;;
    --db) DB="$2"; shift 2 ;;
    --notify) SEND=1; shift ;;
    --quiet) QUIET=1; shift ;;
    *) echo "inquiry-probe: unknown argument $1" >&2; exit 2 ;;
  esac
done
say(){ [ "$QUIET" = 1 ] || printf '%s\n' "$*"; }

[ -r "$DB" ] || { echo "inquiry-probe: cannot read $DB as $(id -un)" >&2; exit 2; }

token="PROBE-$(date -u +%Y%m%dT%H%M%S)-$RANDOM"
before=$(sqlite3 -readonly "$DB" "select count(*) from inquiry" 2>/dev/null || echo -1)
[ "$before" -ge 0 ] || { echo "inquiry-probe: cannot query the inquiry table" >&2; exit 2; }

say "  submitting $token"
code=$(curl -s -o /dev/null -w '%{http_code}' -m 30 -X POST "$URL" \
  --data-urlencode "name=Automated capture probe $token" \
  --data-urlencode "email=probe@plausiden.com" \
  --data-urlencode "phone=" \
  --data-urlencode "company=PlausiDen internal" \
  --data-urlencode "service=IT Operations" \
  --data-urlencode "message=Automated end-to-end capture probe, token $token. This verifies that a submission made through the public form reaches the inquiry log. Not a real enquiry; no reply is expected." \
  --data-urlencode "website=" 2>/dev/null)

problems=""
case "$code" in
  2??|3??) say "  form accepted the submission (HTTP $code)" ;;
  *) problems="the form did not accept a valid submission (HTTP $code)"$'\n' ;;
esac

# The acknowledgement page is not proof. Look in the database.
found=0
for _ in $(seq 1 "$WAIT"); do
  n=$(sqlite3 -readonly "$DB" \
      "select count(*) from inquiry where message like '%$token%'" 2>/dev/null || echo 0)
  [ "${n:-0}" -ge 1 ] && { found=1; break; }
  sleep 1
done

if [ "$found" -eq 0 ]; then
  after=$(sqlite3 -readonly "$DB" "select count(*) from inquiry" 2>/dev/null || echo -1)
  problems="${problems}the submission was accepted but never reached the store — ${before} rows before, ${after} after, and no row carries the probe token"$'\n'
else
  row=$(sqlite3 -readonly "$DB" \
    "select coalesce(verdict,'?')||'|'||coalesce(spam_score,'?')||'|'||coalesce(notified,'?')||'|'||length(coalesce(message,'')) \
     from inquiry where message like '%$token%' order by id desc limit 1" 2>/dev/null)
  IFS='|' read -r verdict score notified mlen <<< "$row"
  say "  stored: verdict=$verdict score=$score notified=$notified message=${mlen}B"

  # A probe classified as spam means the classifier would bin a real,
  # plainly-worded enquiry. That is a lead lost, not a probe failure.
  [ "$verdict" = "ham" ] || problems="${problems}a plainly-worded enquiry was classified '$verdict' — a real one would be treated the same"$'\n'
  # Truncation is silent and only visible by length.
  [ "${mlen:-0}" -ge 150 ] || problems="${problems}the stored message is only ${mlen} bytes — it is being truncated"$'\n'
  [ "$notified" = "1" ] || problems="${problems}a ham enquiry was stored with notified=$notified — it would never have reached anyone"$'\n'
fi

if [ -n "$problems" ]; then
  say ""
  printf '  FAIL %s' "$problems"
  [ "$SEND" = 1 ] && printf 'A test submission through the public contact form did not survive the trip.\n\n%s\nToken: %s\nThe form still returns its thank-you page in every one of these cases, so a real visitor could not tell you this had happened.\n' \
    "$problems" "$token" | "$NOTIFY" --key inquiry-probe --title "The contact form is losing submissions" \
      --priority max --tags rotating_light --topic "$TOPIC" >/dev/null 2>&1
  exit 1
fi
say "inquiry-probe: a submission made through the public form reached the store intact"
[ "$SEND" = 1 ] && "$NOTIFY" --resolve inquiry-probe --topic "$TOPIC" >/dev/null 2>&1
exit 0
