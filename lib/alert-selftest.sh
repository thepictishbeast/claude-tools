#!/usr/bin/env bash
# Prove an alert can still reach Paul's phone.
#
# Ten watchers now depend on one delivery path: a script calls notify.sh,
# which publishes to ntfy, which is fronted by Caddy, which needs a valid
# certificate. Any link breaking makes every alert stop — and a stopped
# alert is indistinguishable from a quiet week. The estate would look
# perfectly healthy while nothing could report otherwise.
#
# So this publishes a message and then reads it back, which is the only
# way to know the path works end to end. Publishing alone proves the
# request was accepted, not that anything is retrievable.
#
# THE FALLBACK IS THE WHOLE POINT. When this check fails, the thing it is
# reporting on is the thing that would carry the report, so it must not
# use it. It sends by email instead, over the local mail server, which
# shares nothing with the push path except the machine itself.
#
# What it deliberately does NOT do: alert on success. A daily "alerting
# works" push would be one more thing to ignore, and the daily digest
# already arrives through the same channel — if the digest stops, that is
# the same signal.
#
#   alert-selftest.sh [--quiet]
set -uo pipefail

URL="${NTFY_URL:-https://ntfy.plausiden.com}"
LOCAL="${NTFY_LOCAL:-http://127.0.0.1:2586}"
TOPIC="${SELFTEST_TOPIC:-plausiden-alerts}"
TOKEN_FILE="${NTFY_TOKEN_FILE:-/tank/secrets/ntfy-alerts.txt}"
PAUL_FILE="${NTFY_PAUL_FILE:-/tank/secrets/ntfy-paul.txt}"
MAIL_TO="${SELFTEST_MAIL:-william@plausiden.com}"
QUIET=0
[ "${1:-}" = "--quiet" ] && QUIET=1
say(){ [ "$QUIET" = 1 ] || printf '%s\n' "$*"; }

problems=""
fail(){ problems="${problems}  $1"$'\n'; say "  FAIL $1"; }

token=$(sed -n 's/^token: //p' "$TOKEN_FILE" 2>/dev/null | head -1)
pw=$(sed -n 's/^password: //p' "$PAUL_FILE" 2>/dev/null | head -1)
[ -n "$token" ] || fail "no publisher token readable at $TOKEN_FILE — nothing can publish"
[ -n "$pw" ]    || fail "no subscriber password readable at $PAUL_FILE — cannot verify delivery"

marker="selftest-$(date -u +%Y%m%dT%H%M%S)-$RANDOM"

if [ -n "$token" ]; then
  # Through the public hostname, because that is the path the phone uses.
  # Testing the loopback port would pass while Caddy or the certificate
  # was broken, which is most of what can actually go wrong.
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 20 \
    -H "Authorization: Bearer $token" -H "Title: alerting self-test" \
    -H "Priority: min" -d "$marker" "$URL/$TOPIC" 2>/dev/null)
  case "$code" in
    200) say "  published through $URL" ;;
    000) fail "could not reach $URL at all — DNS, TLS or Caddy is down, and every alert is currently going nowhere" ;;
    401|403) fail "the publisher token was rejected (HTTP $code) — it has been revoked or rotated, and no watcher can publish" ;;
    *) fail "publishing returned HTTP $code" ;;
  esac
fi

# Read it back. A 200 on publish means accepted, not stored.
if [ -z "$problems" ] && [ -n "$pw" ]; then
  found=0
  for _ in 1 2 3 4 5; do
    if curl -s -u "paul:$pw" -m 15 "$LOCAL/$TOPIC/json?poll=1&since=2m" 2>/dev/null \
         | grep -q "$marker"; then found=1; break; fi
    sleep 2
  done
  [ "$found" = 1 ] && say "  read back from the topic — the path works end to end" \
    || fail "the message was accepted but could not be read back — ntfy is accepting and discarding"
fi

# The certificate that fronts all of it. Expiry here silences everything,
# and it is the failure this estate already found once.
end=$(echo | timeout 15 openssl s_client -connect "${URL#https://}:443" \
      -servername "${URL#https://}" 2>/dev/null | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)
if [ -n "$end" ]; then
  days=$(( ( $(date -d "$end" +%s 2>/dev/null || echo 0) - $(date +%s) ) / 86400 ))
  [ "$days" -lt 14 ] && fail "the certificate fronting alerting expires in $days days — when it lapses the phone stops receiving, silently"
  say "  certificate good for $days more days"
fi

if [ -n "$problems" ]; then
  # Deliberately NOT via notify.sh. The push path is the subject of the
  # report; using it would be asking a broken phone to tell you it is
  # broken.
  { echo "Subject: Alerting is broken — this arrived by email because push did not work"
    echo "From: Claude <claude@plausiden.com>"
    echo "To: $MAIL_TO"
    echo ""
    echo "[automated — the Claude loop on plausiden-prime, not a person]"
    echo ""
    echo "The path that carries every other alert is not working:"
    echo ""
    printf '%s' "$problems"
    echo ""
    echo "Ten watchers publish through it — certificates, sites, backups,"
    echo "the contact form, CI. While it is down they will all appear"
    echo "silent, and silence from them normally means healthy."
    echo ""
    echo "This message came by email on purpose. Reporting a broken push"
    echo "path over the push path would be asking a broken phone to tell"
    echo "you it is broken."
  } | sendmail "$MAIL_TO" 2>/dev/null \
    || say "  AND the email fallback failed too — nothing can reach Paul from this host"
  say ""
  say "alert-selftest: FAILED, reported by email"
  exit 1
fi
say "alert-selftest: an alert can still reach the phone"
exit 0
