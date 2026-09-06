#!/usr/bin/env bash
# The report has to be right about two things that are easy to get wrong:
# a site linking to itself is not a referral, and a tagged click usually
# arrives with NO referer at all (browsers strip it, and this estate sets
# Referrer-Policy: strict-origin-when-cross-origin deliberately).
#
# Fixtures live on /tank/scratch, never /tmp — /tmp is noexec here, and
# that difference has already cost an outage.
set -uo pipefail
S="${REFERRAL_SH:-/home/paul/projects/claude-tools/lib/referral-report.sh}"
P=0; F=0
ok(){ echo "ok   $1"; P=$((P+1)); }
no(){ echo "FAIL $1 — $2"; F=$((F+1)); }
[ -f "$S" ] || { echo "no such script: $S" >&2; exit 2; }

T=$(mktemp -d /tank/scratch/.ref.XXXXXX) || exit 2
trap 'rm -rf "$T"' EXIT

python3 - "$T/metrics.log" <<'PY'
import json, sys, time
now = time.time()
def line(uri, referer=None, ts=None):
    r = {"host": "plausiden.com", "method": "GET", "proto": "HTTP/2.0", "uri": uri}
    if referer: r["headers"] = {"Referer": [referer]}
    return json.dumps({"ts": ts if ts else now - 3600, "status": 200, "request": r})
rows = [
    line("/", "https://prosperityclub.com/about"),      # a real referral
    line("/services", "https://prosperityclub.com/"),   # same site again
    line("/", "https://erminewallet.org/"),             # a second referrer
    line("/", "https://plausiden.com/"),                # SAME-SITE: must not count
    line("/about", "https://www.plausiden.com/blog"),   # same-site variant too
    line("/?ref=prosperityclub"),                       # tagged, NO referer at all
    line("/?ref=prosperityclub"),
    line("/contact?utm_source=erminewallet"),           # utm_source counts as well
    line("/old", "https://prosperityclub.com/x", ts=now - 40*86400),  # outside window
]
open(sys.argv[1], "w").write("\n".join(rows) + "\n")
PY

run(){ REFERRAL_LOG="$T/metrics.log" bash "$S" "$@" 2>&1; }
out=$(run --days 7)

grep -q 'prosperityclub.com' <<<"$out" && ok "an external referrer is reported" || no "referrer missing" "$out"
grep -q 'erminewallet.org'   <<<"$out" && ok "a second referrer is reported separately" || no "second referrer" "not listed"

# The one most likely to be got wrong: plausiden.com linking to itself.
grep -qE '^\s+[0-9]+\s+(www\.)?plausiden\.(com|org)' <<<"$out" \
  && no "counted a same-site navigation as a referral" "inflates the numbers" \
  || ok "same-site navigation is excluded, not counted as a referral"
grep -q '2 same-site navigations ignored' <<<"$out" \
  && ok "same-site hits are reported as ignored, not silently dropped" \
  || no "same-site accounting" "did not say how many were ignored"

# Tagged clicks must be counted even though they carry no Referer.
grep -qE 'prosperityclub' <<<"$(sed -n '/Tagged links/,$p' <<<"$out")" \
  && ok "a ?ref= tag is counted with no referer present" || no "tag missed" "tagged section empty"
grep -qE 'erminewallet' <<<"$(sed -n '/Tagged links/,$p' <<<"$out")" \
  && ok "utm_source is counted alongside ref" || no "utm_source" "not counted"

# The window must actually bound things.
grep -q 'over the last 7 day' <<<"$out" && ok "reports the window it used" || no "window" "not stated"
n=$(grep -oE 'Requests examined: [0-9]+' <<<"$out" | grep -oE '[0-9]+')
[ "$n" = "8" ] && ok "the 40-day-old request is outside the window and excluded" \
  || no "window not applied" "counted $n, expected 8"

# Empty input must read as "nothing to report", not crash or imply success.
: > "$T/metrics.log"
out2=$(run --days 7)
grep -qi 'none' <<<"$out2" && ok "an empty log reports nothing rather than failing" || no "empty log" "$out2"

echo "────────────────"; echo "pass=$P fail=$F"; exit "$F"
