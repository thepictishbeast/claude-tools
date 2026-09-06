#!/usr/bin/env bash
# One check, one ntfy topic, per site — so a phone can subscribe per domain.
#
# The other monitors here are organised by concern: cert-watch covers every
# certificate in one digest, referral-report covers every site in one
# summary. That is right for those jobs and wrong for this one. If a single
# site is down you want that site's alert, not a bulletin about the estate,
# and you want to mute one domain without going deaf to the rest.
#
# Each host is checked for the two things that actually take a site off the
# air: it stops answering as expected, or its certificate expires and a
# browser refuses before anyone notices.
#
# "As expected" is per-host, and that detail is the whole point. Checking
# "/" for a 2xx would report permanent failure on four healthy hosts here:
# mta-sts, autoconfig, autodiscover and lk-jwt all 404 at the root by
# design and serve a specific path. It would also flag dev1 and dev4,
# where a 401 is the fleet-console gate working — a 200 there would be the
# emergency. A monitor that fires on healthy services is worse than none,
# because it trains the reader to ignore it, which is exactly how the DKIM
# check stayed red for four months without anyone looking.
#
# Config: /etc/site-watch.conf, one line of
#     <host>  <topic-slug>  <path>  <expected-status-regex>
#
#   site-watch.sh [--config FILE] [--dry-run] [--quiet]
set -uo pipefail

CONF="${SITE_WATCH_CONF:-/etc/site-watch.conf}"
NOTIFY="${SITE_WATCH_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
CERT_WARN_DAYS="${SITE_WATCH_CERT_DAYS:-21}"
TIMEOUT="${SITE_WATCH_TIMEOUT:-15}"
DRY=0; QUIET=0; HEARTBEAT=0
while [ $# -gt 0 ]; do
  case "$1" in
    --config)    CONF="$2"; shift 2 ;;
    --dry-run)   DRY=1; shift ;;
    --quiet)     QUIET=1; shift ;;
    # Post an "all ok" to every site's topic even when nothing is wrong.
    #
    # Without this a healthy site publishes nothing at all, so its channel
    # sits empty — and an empty channel is indistinguishable from one that
    # was never wired up. That is the same failure as a check nobody
    # notices is red, only inverted: you cannot tell working silence from
    # broken silence.
    #
    # A periodic heartbeat gives silence a meaning. If a topic has said
    # nothing for longer than the heartbeat interval, the monitor itself
    # has stopped, and that is worth knowing.
    --heartbeat) HEARTBEAT=1; shift ;;
    *) echo "site-watch: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -r "$CONF" ] || { echo "site-watch: cannot read $CONF" >&2; exit 2; }

ok=0; bad=0; checked=0
while read -r host slug path expect _rest; do
  case "$host" in ''|\#*) continue ;; esac
  [ -n "$slug" ] && [ -n "$path" ] && [ -n "$expect" ] || {
    echo "site-watch: malformed line for '$host' — skipping" >&2; continue; }
  checked=$((checked+1))
  problems=""

  code=$(curl -s -o /dev/null -w '%{http_code}' -m "$TIMEOUT" "https://${host}${path}" 2>/dev/null)
  if [ "$code" = "000" ]; then
    problems="${problems}  no response at all — DNS, TLS handshake, or the service is down"$'\n'
  elif ! printf '%s' "$code" | grep -qE "$expect"; then
    problems="${problems}  HTTP $code at ${path} (expected ${expect})"$'\n'
  fi

  # Certificate. Checked even when HTTP is fine, because expiry is the
  # failure you want warning of rather than notice of.
  end=$(echo | timeout "$TIMEOUT" openssl s_client -connect "${host}:443" -servername "$host" 2>/dev/null \
        | openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)
  if [ -n "$end" ]; then
    end_s=$(date -d "$end" +%s 2>/dev/null || echo 0)
    if [ "$end_s" -gt 0 ]; then
      days=$(( (end_s - $(date +%s)) / 86400 ))
      [ "$days" -lt "$CERT_WARN_DAYS" ] && \
        problems="${problems}  certificate expires in ${days} days — renewal has already missed at least once"$'\n'
    fi
  elif [ "$code" != "000" ]; then
    problems="${problems}  could not read the TLS certificate"$'\n'
  fi

  if [ -n "$problems" ]; then
    bad=$((bad+1))
    [ "$QUIET" = 1 ] || printf '  %-28s FAIL\n%s' "$host" "$problems"
    [ "$DRY" = 1 ] || printf '%s' "$problems" | "$NOTIFY" --key "site:$host" \
        --title "$host is failing" --priority high --tags rotating_light \
        --topic "plausiden-$slug" >/dev/null 2>&1
  else
    ok=$((ok+1))
    [ "$QUIET" = 1 ] || printf '  %-28s ok (%s)  -> plausiden-%s\n' "$host" "$code" "$slug"
    [ "$DRY" = 1 ] || "$NOTIFY" --resolve "site:$host" --topic "plausiden-$slug" >/dev/null 2>&1
    if [ "$HEARTBEAT" = 1 ] && [ "$DRY" = 0 ]; then
      # The timestamp is deliberate. notify.sh suppresses an unchanged
      # message, which is right for alerts and wrong for a heartbeat —
      # a heartbeat that gets deduplicated into silence is not a
      # heartbeat. Varying the body makes each one genuinely new.
      # `low`, not `min`. On Android a min-priority message is not shown
      # as a notification at all — it lands silently in the app's list,
      # which is indistinguishable from nothing arriving. That defeats a
      # check-in whose entire purpose is being visible. `low` shows it
      # without sound or vibration.
      #
      # Say what was actually checked and what the answer was, so the
      # check-in carries evidence rather than just the word "ok". A
      # reassurance you cannot verify is worth very little — and if the
      # certificate figure starts falling week on week, that is visible
      # here before it becomes an alert.
      cert_line="certificate could not be read"
      if [ -n "${end:-}" ] && [ "${end_s:-0}" -gt 0 ]; then
        cert_line="certificate good for $(( (end_s - $(date +%s)) / 86400 )) more days"
      fi
      printf 'Responding normally.\n\n  checked   https://%s%s\n  answered  HTTP %s\n  TLS       %s\n  at        %s\n\nThis is the weekly check-in, not an alert. It means the monitor itself is alive: if this topic goes quiet for more than a week, something has stopped watching.\n' \
        "$host" "$path" "$code" "$cert_line" "$(date -u '+%a %d %b, %H:%M UTC')" \
        | "$NOTIFY" --key "beat:$host" --title "$host — all good" \
            --priority low --tags heavy_check_mark --topic "plausiden-$slug" >/dev/null 2>&1
    fi
  fi
done < "$CONF"

[ "$checked" -gt 0 ] || { echo "site-watch: no hosts configured in $CONF" >&2; exit 2; }
echo "site-watch: $ok ok, $bad failing, $checked checked"
[ "$bad" -eq 0 ]
