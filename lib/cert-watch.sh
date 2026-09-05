#!/usr/bin/env bash
# Alert when a certificate is running out — i.e. when renewal has quietly
# stopped working, whatever the reason.
#
# Renewal on this host has several ways to fail silently, and today
# produced a new one. Every certificate was issued with authenticator =
# standalone, so a pre-hook stopped Caddy to free port 80. When
# ntfy.plausiden.com was later issued via webroot — which serves the
# challenge THROUGH Caddy and therefore needs it up — that same
# unconditional hook guaranteed its renewal would fail. Nothing would have
# reported it. The certificate would simply have expired, and the first
# symptom would have been the phone quietly no longer receiving alerts.
#
# The pre-hook is now conditional, but that fixes one cause. This checks
# the OUTCOME, so it catches every cause: a hook regression, a DNS change,
# a firewall rule, an expired ACME account, a rate limit.
#
# Certificates are valid 90 days and certbot renews at 30 days remaining,
# so anything under ~21 days means renewal has already failed at least
# once. That is the signal, and there is still a fortnight to act on it.
#
#   cert-watch.sh [--warn-days N] [--dry-run]
set -uo pipefail

WARN_DAYS="${CERT_WARN_DAYS:-21}"
CRIT_DAYS="${CERT_CRIT_DAYS:-7}"
LIVE="${CERT_LIVE_DIR:-/etc/letsencrypt/live}"
NOTIFY="${CERT_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --warn-days) WARN_DAYS="$2"; shift 2 ;;
    --dry-run)   DRY=1; shift ;;
    *) echo "cert-watch: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -d "$LIVE" ] || { echo "cert-watch: no $LIVE" >&2; exit 2; }

problems=""
checked=0
for d in "$LIVE"/*/; do
  cert="$d/cert.pem"
  [ -r "$cert" ] || continue
  name=$(basename "$d")
  checked=$((checked+1))

  end=$(openssl x509 -in "$cert" -noout -enddate 2>/dev/null | cut -d= -f2)
  [ -n "$end" ] || { problems="${problems}  ${name}: certificate unreadable"$'\n'; continue; }
  end_s=$(date -d "$end" +%s 2>/dev/null) || continue
  days=$(( (end_s - $(date +%s)) / 86400 ))

  if [ "$days" -lt "$WARN_DAYS" ]; then
    auth=$(grep -oP '^\s*authenticator\s*=\s*\K\S+' "/etc/letsencrypt/renewal/${name}.conf" 2>/dev/null | head -1)
    sev="renewal has already missed at least one attempt"
    [ "$days" -lt "$CRIT_DAYS" ] && sev="CRITICAL — expires in under ${CRIT_DAYS} days"
    problems="${problems}  ${name} (${auth:-unknown}): ${days} days left — ${sev}"$'\n'
  fi
done

[ "$checked" -gt 0 ] || { echo "cert-watch: no readable certificates in $LIVE" >&2; exit 2; }

if [ -n "$problems" ]; then
  msg="Certificate renewal appears to have stopped working.

${problems}
certbot renews at 30 days remaining, so anything below that has already
failed at least once. Check:  certbot renew --dry-run --cert-name <name>

A webroot lineage needs Caddy UP during renewal; a standalone one needs it
DOWN. /etc/letsencrypt/renewal-hooks/pre/stop-caddy.sh decides which, and
getting that wrong is what this watch exists to catch."
  echo "cert-watch: ${problems}"
  [ "$DRY" = 1 ] && { echo "(dry run — not notifying)"; exit 1; }
  printf '%s\n' "$msg" | "$NOTIFY" --key cert-expiry --title "TLS certificate not renewing" \
      --priority high --tags warning
  exit 1
fi

echo "cert-watch: $checked certificates, all renewing normally (>= ${WARN_DAYS} days)"
[ "$DRY" = 1 ] || "$NOTIFY" --resolve cert-expiry >/dev/null 2>&1
exit 0
