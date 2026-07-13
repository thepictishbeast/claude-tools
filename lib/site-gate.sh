#!/usr/bin/env bash
# site-gate: run every registered site's quality gate and alert on regressions.
# Registry: sites.toml next to this script (TOML-ish, parsed line-wise: no deps).
# Levels:
#   full   - run the repo's own gate command (tests + build + browser audit)
#   health - HTTP 200 + security headers + TLS-expiry check (no repo needed)
# Usage: site-gate.sh [--health-only] [--site NAME]
# Exit: 0 all green; 1 any regression (after emailing NOTIFY).
# Installed by claude-tools; run via claude-sitegate.timer (daily).
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
REG="$DIR/sites.toml"
NOTIFY="william@plausiden.com"
HEALTH_ONLY=0; ONLY_SITE=""
for a in "$@"; do
  [[ "$a" == "--health-only" ]] && HEALTH_ONLY=1
  [[ "$a" == --site=* ]] && ONLY_SITE="${a#--site=}"
done

fails=(); summary=()

health_check() { # name url
  local name="$1" url="$2" code hdr_missing="" days
  code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 "$url" 2>/dev/null || echo 000)
  if [[ "$code" != "200" ]]; then
    fails+=("$name: HTTP $code from $url"); summary+=("$name: FAIL (HTTP $code)"); return
  fi
  for h in strict-transport-security x-content-type-options; do
    curl -sSI --max-time 20 "$url" 2>/dev/null | grep -qi "^$h:" || hdr_missing+="$h "
  done
  # days until cert expiry
  local host=${url#https://}; host=${host%%/*}
  local end epoch_end epoch_now
  end=$(echo | openssl s_client -servername "$host" -connect "$host:443" 2>/dev/null | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)
  if [[ -n "$end" ]]; then
    epoch_end=$(date -d "$end" +%s); epoch_now=$(date +%s)
    days=$(( (epoch_end - epoch_now) / 86400 ))
    if (( days < 14 )); then fails+=("$name: TLS cert expires in ${days}d"); fi
  else
    days="?"
  fi
  if [[ -n "$hdr_missing" ]]; then
    fails+=("$name: missing security headers: $hdr_missing")
    summary+=("$name: WARN (headers: $hdr_missing| cert ${days}d)")
  else
    summary+=("$name: ok (200, headers, cert ${days}d)")
  fi
}

full_gate() { # name repo cmd
  local name="$1" repo="$2" cmd="$3" out
  out=$(cd "$repo" && timeout 900 bash -c "$cmd" 2>&1 | tail -20)
  if echo "$out" | grep -q "GATE GREEN"; then
    summary+=("$name: GATE GREEN")
  else
    fails+=("$name: gate FAILED - last output:
$out")
    summary+=("$name: GATE FAILED")
  fi
}

# --- parse registry ---------------------------------------------------------
name=""; url=""; level=""; repo=""; cmd=""
flush() {
  [[ -z "$name" ]] && return
  [[ -n "$ONLY_SITE" && "$name" != "$ONLY_SITE" ]] && { name=""; return; }
  if [[ "$level" == "full" && $HEALTH_ONLY -eq 0 && -n "$repo" ]]; then
    full_gate "$name" "$repo" "${cmd:-./check.sh}"
  else
    health_check "$name" "$url"
  fi
  name=""; url=""; level=""; repo=""; cmd=""
}
while IFS= read -r line; do
  line="${line%%#*}"
  case "$line" in
    \[\[site\]\]*) flush ;;
    *name*=*)  name=$(echo "$line"  | sed 's/.*=\s*"\(.*\)".*/\1/') ;;
    *url*=*)   url=$(echo "$line"   | sed 's/.*=\s*"\(.*\)".*/\1/') ;;
    *level*=*) level=$(echo "$line" | sed 's/.*=\s*"\(.*\)".*/\1/') ;;
    *repo*=*)  repo=$(echo "$line"  | sed 's/.*=\s*"\(.*\)".*/\1/') ;;
    *cmd*=*)   cmd=$(echo "$line"   | sed 's/.*=\s*"\(.*\)".*/\1/') ;;
  esac
done < "$REG"
flush

echo "== site-gate $(date -u +%FT%TZ) =="
printf '%s\n' "${summary[@]}"

if ((${#fails[@]})); then
  {
    echo "Subject: [site-gate] ${#fails[@]} regression(s) detected"
    echo "To: $NOTIFY"
    echo ""
    printf '%s\n\n' "${fails[@]}"
    echo "-- site-gate on $(hostname), $(date -u +%FT%TZ)"
  } | sendmail "$NOTIFY" 2>/dev/null || echo "WARN: sendmail failed" >&2
  echo "REGRESSIONS: ${#fails[@]} (mailed $NOTIFY)"
  exit 1
fi
echo "ALL GREEN"
