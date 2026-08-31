#!/usr/bin/env bash
# mailbox-digest: make sure nothing arriving at a secondary address goes unseen.
#
# Mail is delivered to several addresses on this host and only william@ is read
# day to day. Nothing forwards the others, so anything arriving at them is
# invisible:
#
#   security@  inbound security reports from researchers. The newest was a
#              private hardening note about a path-traversal escape. Missing one
#              of these is the worst outcome on the whole box.
#   team@      postmaster@, abuse@ and @outreach.* all land here. Live traffic.
#   tlsrpt@    TLS reporting. Informational, but silence is not evidence of
#              health — it should be visible somewhere.
#
# Forwarding everything would just move the noise. This sends ONE digest, only
# when something new has arrived, and says nothing at all otherwise.
#
# security@ is treated as urgent: it is reported on every run rather than
# waiting for the daily digest, because a security report that sits for a day
# has already cost the day.
#
#   mailbox-digest.sh [--dry-run]
set -uo pipefail
NOTIFY="${DIGEST_NOTIFY:-william@plausiden.com}"
ROOT="${DIGEST_ROOT:-/var/mail/vhosts/plausiden.com}"
STATE="${DIGEST_STATE:-/var/lib/mailbox-digest/seen}"
BOXES="${DIGEST_BOXES:-security team tlsrpt site salesman kadyn}"
URGENT="${DIGEST_URGENT:-security}"
DRY=0; [ "${1:-}" = "--dry-run" ] && DRY=1
mkdir -p "$(dirname "$STATE")" 2>/dev/null || STATE=/tmp/.mailbox-digest-seen
touch "$STATE" 2>/dev/null

# Subject + From of one message, decoded, without pulling in the body.
head_of(){ python3 - "$1" <<'PY' 2>/dev/null
import sys, email
from email.header import decode_header, make_header
def dec(v):
    try: return str(make_header(decode_header(v or "")))
    except Exception: return str(v or "")
try:
    m = email.message_from_binary_file(open(sys.argv[1], 'rb'))
except Exception:
    sys.exit(0)
print(f"{dec(m.get('From'))[:38]} | {dec(m.get('Subject'))[:66]}")
PY
}

body=""; urgent_body=""; total=0
for box in $BOXES; do
  d="$ROOT/$box"; [ -d "$d" ] || continue
  new=""
  n=0
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    id="$box:$(basename "$f")"
    grep -qxF "$id" "$STATE" 2>/dev/null && continue      # already reported
    line=$(head_of "$f"); [ -n "$line" ] || continue
    new="${new}    ${line}"$'\n'
    n=$((n+1))
    [ "$DRY" = 1 ] || printf '%s\n' "$id" >> "$STATE"
  done < <(find "$d" -type f \( -path '*/new/*' -o -path '*/cur/*' \) -newermt '-30 days' 2>/dev/null | sort)

  [ "$n" -eq 0 ] && continue
  total=$((total+n))
  block="  ${box}@ — ${n} new"$'\n'"${new}"
  case " $URGENT " in *" $box "*) urgent_body="${urgent_body}${block}"$'\n' ;;
                   *) body="${body}${block}"$'\n' ;; esac
done

send(){ { echo "Subject: $1"; echo "To: $NOTIFY"; echo ""; printf '%s\n' "$2"
          echo "-- mailbox-digest, $(date -u +%FT%TZ)"; } | sendmail "$NOTIFY" 2>/dev/null \
        || echo "WARN: sendmail failed" >&2; }

if [ -n "$urgent_body" ]; then
  [ "$DRY" = 1 ] || send "[mail] SECURITY: new report waiting" \
    "Mail has arrived at an address you do not read day to day.

${urgent_body}Read it:  security@plausiden.com"
  echo "URGENT: security mail present"
fi
if [ -n "$body" ]; then
  [ "$DRY" = 1 ] || send "[mail] ${total} new across your other addresses" \
    "These arrived at addresses that are not forwarded anywhere, so they are
only visible if you open the mailbox directly.

${body}"
  echo "digest: $total new"
fi
[ "$total" -eq 0 ] && echo "mailbox-digest: nothing new"
exit 0
