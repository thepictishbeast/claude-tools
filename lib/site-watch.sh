#!/usr/bin/env bash
# One check, one topic, per site — so a phone can subscribe per domain.
#
# The other monitors here are organised by concern: cert-watch covers every
# certificate in one digest, referral-report covers every site in one
# summary. That is right for those jobs and wrong for this one. If a single
# site is down you want that site's alert, not a bulletin about the estate,
# and you want to be able to mute one domain without going deaf to the rest.
#
# So each site gets its own ntfy topic, `plausiden-<slug>`, and this checks
# the two things that actually take a site off the air:
#
#   * it stops answering, or answers with the wrong status
#   * its certificate expires (a browser refuses before you notice)
#
# Dedup, the daily reminder and the all-clear all come from notify.sh, so a
# site that stays down alerts once and then reminds daily rather than every
# ten minutes.
#
#   site-watch.sh [--sites "host=slug,..."] [--dry-run]
set -uo pipefail

# host=slug. The slug becomes the ntfy topic suffix, so it is what you
# subscribe to on the phone.
SITES="${SITE_WATCH_SITES:-\
plausiden.com=plausiden,\
prosperityclub.com=prosperityclub,\
erminewallet.org=erminewallet,\
william-armstrong.com=william-armstrong,\
ntfy.plausiden.com=ntfy,\
cloud.plausiden.com=cloud,\
matrix.plausiden.com=matrix,\
vault.plausiden.com=vault}"

NOTIFY="${SITE_WATCH_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
CERT_WARN_DAYS="${SITE_WATCH_CERT_DAYS:-21}"
TIMEOUT="${SITE_WATCH_TIMEOUT:-15}"
DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --sites)   SITES="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    *) echo "site-watch: unknown argument $1" >&2; exit 2 ;;
  esac
done

ok=0; bad=0
IFS=',' read -ra ENTRIES <<< "$(printf '%s' "$SITES" | tr -d ' \\')"
for entry in "${ENTRIES[@]}"; do
  [ -n "$entry" ] || continue
  host="${entry%%=*}"; slug="${entry##*=}"
  [ -n "$host" ] && [ -n "$slug" ] || continue
  problems=""

  code=$(curl -s -o /dev/null -w '%{http_code}' -m "$TIMEOUT" "https://$host/" 2>/dev/null)
  # 2xx and 3xx are both fine: several of these redirect by design
  # (cloud -> /login, crm -> a session route). Only a 4xx/5xx, or no
  # answer at all, means the site is actually failing a visitor.
  case "$code" in
    2??|3??) : ;;
    000) problems="${problems}  no response at all (DNS, TLS handshake, or the service is down)"$'\n' ;;
    *)   problems="${problems}  HTTP $code — the site answered, but not with a page"$'\n' ;;
  esac

  end=$(echo | timeout "$TIMEOUT" openssl s_client -connect "$host:443" -servername "$host" 2>/dev/null \
        | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)
  if [ -n "$end" ]; then
    end_s=$(date -d "$end" +%s 2>/dev/null || echo 0)
    days=$(( (end_s - $(date +%s)) / 86400 ))
    if [ "$end_s" -gt 0 ] && [ "$days" -lt "$CERT_WARN_DAYS" ]; then
      problems="${problems}  certificate expires in ${days} days — renewal has already missed once"$'\n'
    fi
  elif [ "$code" != "000" ]; then
    problems="${problems}  could not read the TLS certificate"$'\n'
  fi

  if [ -n "$problems" ]; then
    bad=$((bad+1))
    printf '  %-28s FAIL\n%s' "$host" "$problems"
    [ "$DRY" = 1 ] || printf '%s' "$problems" | "$NOTIFY" --key "site:$host" \
        --title "$host is failing" --priority high --tags rotating_light \
        --topic "plausiden-$slug" >/dev/null 2>&1
  else
    ok=$((ok+1))
    printf '  %-28s ok (HTTP %s)\n' "$host" "$code"
    [ "$DRY" = 1 ] || "$NOTIFY" --resolve "site:$host" --topic "plausiden-$slug" >/dev/null 2>&1
  fi
done

echo "site-watch: $ok ok, $bad failing"
[ "$bad" -eq 0 ]
