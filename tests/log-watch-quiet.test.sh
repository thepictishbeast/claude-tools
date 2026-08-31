#!/usr/bin/env bash
# log-watch must alert on CHANGE, not re-send unchanged state.
#
# It used to mail whenever any anomaly existed. Two stale transient
# `run-*.service` units, left behind by a `caddy reload` that failed once,
# therefore produced 261 identical "1 anomaly(ies)" emails — 43% of the inbox —
# about a host that was healthy throughout. The signal was real once and noise
# 260 times.
#
# Required behaviour: mail when the anomaly set changes, stay quiet while it is
# unchanged, remind once a day so a genuine problem is not forgotten, and send
# one all-clear when it resolves.
set -uo pipefail
S="${LOG_WATCH:-/home/paul/projects/claude-tools/lib/log-watch.sh}"
P=0; F=0
ok(){ echo "ok   $1"; P=$((P+1)); }
no(){ echo "FAIL $1 — $2"; F=$((F+1)); }
[ -f "$S" ] || { echo "no such script: $S" >&2; exit 2; }

T=$(mktemp -d /tank/scratch/.lw.XXXXXX); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin"
# fake sendmail: record each delivery instead of sending one
cat > "$T/bin/sendmail" <<'EOF'
#!/usr/bin/env bash
cat >> "$MAILLOG"
printf '\n===MAIL===\n' >> "$MAILLOG"
EOF
chmod +x "$T/bin/sendmail"
# fake systemctl: the anomaly source we control. df/journalctl stay real but
# quiet on a healthy host, so failed-units is the lever.
mkfake(){ cat > "$T/bin/systemctl" <<EOF
#!/usr/bin/env bash
case "\$*" in
  *"--failed"*|*"list-units"*) printf '%s' "$1" ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$T/bin/systemctl"; }

run(){ MAILLOG="$T/mail" PATH="$T/bin:$PATH" LOGWATCH_STATE="$T/state" \
       LOGWATCH_REMIND_SEC="${1:-86400}" bash "$S" >/dev/null 2>&1 || true; }
mails(){ { grep -c '===MAIL===' "$T/mail" 2>/dev/null || true; } | head -1; }

: > "$T/mail"

# 1. a new anomaly mails once
mkfake 'run-p123-i456.service loaded failed failed junk'
run; [ "$(mails)" -eq 1 ] && ok "new anomaly sends one alert" || no "first alert" "sent $(mails)"

# 2. the SAME anomaly, four more scans: silence. This is the 261-email case.
run; run; run; run
[ "$(mails)" -eq 1 ] && ok "unchanged anomaly stays quiet across repeat scans" \
  || no "repeat spam" "sent $(mails) mails for one unchanged condition"

# 3. a DIFFERENT anomaly is a new signal and must get through
mkfake 'nginx.service loaded failed failed other'
run; [ "$(mails)" -eq 2 ] && ok "a changed anomaly alerts again" || no "change missed" "sent $(mails)"

# 4. still-unresolved gets a daily reminder, so nothing is silently forgotten
run 0
[ "$(mails)" -eq 3 ] && ok "persistent anomaly gets its periodic reminder" \
  || no "no reminder" "sent $(mails)"
grep -q 'still unresolved' "$T/mail" && ok "the reminder is labelled as a reminder" \
  || no "reminder wording" "reminder not distinguishable from a fresh alert"

# 5. when it clears, one all-clear and then silence
mkfake ''
run; run; run
[ "$(mails)" -eq 4 ] && ok "recovery sends exactly one all-clear" || no "recovery" "sent $(mails)"
grep -q 'all clear' "$T/mail" && ok "all-clear is identifiable" || no "all-clear wording" "not found"

echo "────────────────"; echo "pass=$P fail=$F"; exit "$F"
