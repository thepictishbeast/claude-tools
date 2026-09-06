#!/usr/bin/env bash
# Who is actually sending people to plausiden.com.
#
# The data was already being collected and nobody was reading it. The
# Caddy metrics log for plausiden.com uses a deny-list filter that strips
# cookies, auth, user-agent and every fingerprinting header — but it does
# NOT strip Referer. So referrals have been recorded all along.
#
# What that record showed on 2026-09-05, over 24,716 requests spanning two
# weeks: seven referers total, six of them plausiden.com linking to
# itself, and exactly ONE genuine cross-site click, from
# https://prosperityclub.com/about.
#
# That single number is the point of this script. Referer alone will never
# be reliable attribution: browsers omit it on privacy settings, apps and
# email clients strip it, and this estate sets
# `Referrer-Policy: strict-origin-when-cross-origin` on every vhost, which
# is a deliberate privacy choice worth keeping. So the report counts two
# things side by side:
#
#   * Referer — honest but sparse, and only for links we do not control.
#   * ?ref= tag — survives a stripped referer, because it rides in the URL
#     the sibling site chose to publish. This is the reliable one for
#     links we own (prosperityclub.com, erminewallet.org, ...).
#
# A tagged click with no referer is the normal case, not an anomaly.
#
#   referral-report.sh [--days N] [--quiet] [--notify]
set -uo pipefail

LOG="${REFERRAL_LOG:-/var/log/caddy/plausiden-metrics.log}"
DAYS="${REFERRAL_DAYS:-7}"
NOTIFY="${REFERRAL_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
SEND=0; QUIET=0
while [ $# -gt 0 ]; do
  case "$1" in
    --days)   DAYS="$2"; shift 2 ;;
    --notify) SEND=1; shift ;;
    --quiet)  QUIET=1; shift ;;
    *) echo "referral-report: unknown argument $1" >&2; exit 2 ;;
  esac
done

ls "$LOG"* >/dev/null 2>&1 || { echo "referral-report: no log at $LOG" >&2; exit 2; }

# Being unable to READ the log is not the same as there being nothing to
# report, and the difference matters: the Caddy logs are caddy:caddy 0600,
# so running this as the wrong user produces a confident, permanent
# "no referrals" that looks exactly like a quiet week. Fail loudly instead.
if ! head -c1 "$LOG" >/dev/null 2>&1; then
  echo "referral-report: $LOG exists but is not readable as $(id -un) —" >&2
  echo "  refusing to report zero, because that is indistinguishable from real data." >&2
  exit 2
fi

report=$(REFERRAL_LOG="$LOG" REFERRAL_DAYS="$DAYS" python3 <<'PY'
import json, glob, gzip, os, time, collections
from urllib.parse import urlsplit, parse_qs

log   = os.environ["REFERRAL_LOG"]
days  = int(os.environ["REFERRAL_DAYS"])
since = time.time() - days * 86400

# Our own hostnames. A link from plausiden.com to plausiden.com is
# navigation, not a referral, and counting it would flatter the numbers.
OURS = {"plausiden.com", "www.plausiden.com", "plausiden.org", "www.plausiden.org"}

refs = collections.Counter()     # external referring site -> hits
tags = collections.Counter()     # ?ref= value -> hits
landing = collections.defaultdict(collections.Counter)
total = internal = 0

for path in sorted(glob.glob(log + "*")):
    opener = gzip.open if path.endswith(".gz") else open
    try:
        fh = opener(path, "rt", errors="ignore")
    except OSError:
        continue
    with fh:
        for line in fh:
            try:
                d = json.loads(line)
            except Exception:
                continue
            if (d.get("ts") or 0) < since:
                continue
            total += 1
            req = d.get("request", {}) or {}
            uri = req.get("uri", "") or ""

            # ?ref= tag — reliable, because it does not depend on the
            # browser choosing to send anything.
            q = parse_qs(urlsplit(uri).query)
            # Three spellings, one meaning. `?ref=` is what this estate
            # emitted first; `?from=` + `?at=` is the convention the
            # sacred.vote session shipped and the one everything is
            # converging on, because it also records WHICH placement was
            # clicked. utm_source is what a third party will send.
            #
            # The receiver stays liberal on purpose: narrowing it would
            # silently discard every click already emitted under the old
            # spelling, and a migration that loses its own history is not
            # worth running.
            for key in ("ref", "from", "utm_source"):
                for v in q.get(key, []):
                    if v:
                        at = (q.get("at") or [""])[0]
                        label = f"{v}:{at}" if at else v
                        tags[label] += 1
                        landing[label][urlsplit(uri).path] += 1

            hdrs = req.get("headers", {}) or {}
            for k, v in hdrs.items():
                if k.lower() != "referer":
                    continue
                val = v[0] if isinstance(v, list) else v
                host = (urlsplit(val).hostname or "").lower()
                if not host:
                    continue
                if host in OURS:
                    internal += 1
                else:
                    refs[host] += 1
                    landing[host][urlsplit(uri).path] += 1

out = []
out.append(f"Requests examined: {total} over the last {days} day(s)")
out.append("")
out.append("Referred by another site (Referer header):")
if refs:
    for host, n in refs.most_common(15):
        top = ", ".join(p for p, _ in landing[host].most_common(2))
        out.append(f"  {n:>5}  {host}   -> {top}")
else:
    out.append("  none. No other site sent a click that the browser was willing to attribute.")
out.append(f"  ({internal} same-site navigations ignored)")
out.append("")
out.append("Tagged links (?ref= / utm_source) — survives a stripped referer:")
if tags:
    for tag, n in tags.most_common(15):
        top = ", ".join(p for p, _ in landing[tag].most_common(2))
        out.append(f"  {n:>5}  {tag}   -> {top}")
else:
    out.append("  none seen. If the sibling sites carry untagged links, their")
    out.append("  traffic is indistinguishable from direct visits.")

print("\n".join(out))
print(f"__TOTAL__ {sum(refs.values()) + sum(tags.values())}")
PY
) || { echo "referral-report: failed to parse $LOG" >&2; exit 2; }

count=$(printf '%s' "$report" | sed -n 's/^__TOTAL__ //p')
body=$(printf '%s' "$report" | grep -v '^__TOTAL__')

[ "$QUIET" = 1 ] || printf '%s\n' "$body"

# Only speak when there is something to say. A weekly "0 referrals" push
# is how a useful report turns into noise people stop reading.
if [ "$SEND" = 1 ] && [ "${count:-0}" -gt 0 ] && [ -x "$NOTIFY" ]; then
  printf '%s\n' "$body" | "$NOTIFY" --key referral-report --title "Referral traffic to plausiden.com" \
    --priority low --tags chart_with_upwards_trend
fi
exit 0
