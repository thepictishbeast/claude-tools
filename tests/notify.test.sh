#!/usr/bin/env bash
# notify must push, must not repeat itself, and must never lose an alert.
#
# The last one is the important one. ntfy runs on the same host it
# reports about, so the moment the host is in trouble is exactly the
# moment push stops working. If that swallowed the alert, the monitor
# would be silent precisely when it matters.
set -uo pipefail
S="${NOTIFY_SH:-/home/paul/projects/claude-tools/lib/notify.sh}"
P=0; F=0
ok(){ echo "ok   $1"; P=$((P+1)); }
no(){ echo "FAIL $1 — $2"; F=$((F+1)); }
[ -f "$S" ] || { echo "no such script: $S" >&2; exit 2; }

T=$(mktemp -d /tank/scratch/.nt.XXXXXX); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin"

# fake curl: records the push, and can be told to fail (host down)
cat > "$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$PUSHLOG"
if [ -f "$FAILFLAG" ]; then echo -n "000"; else echo -n "200"; fi
EOF
cat > "$T/bin/sendmail" <<'EOF'
#!/usr/bin/env bash
cat >> "$MAILLOG"; printf '\n===MAIL===\n' >> "$MAILLOG"
EOF
chmod +x "$T/bin/curl" "$T/bin/sendmail"

run(){ PUSHLOG="$T/push" MAILLOG="$T/mail" FAILFLAG="$T/faildown" \
       PATH="$T/bin:$PATH" NOTIFY_STATE="$T/state" \
       NOTIFY_REMIND_SEC="${REMIND:-86400}" bash "$S" "$@"; }
pushes(){ { grep -c . "$T/push" 2>/dev/null || true; } | head -1; }
mails(){ { grep -c '===MAIL===' "$T/mail" 2>/dev/null || true; } | head -1; }
: > "$T/push"; : > "$T/mail"

# 1. a new condition pushes
echo "disk 94% full" | run --key disk --title "disk" >/dev/null 2>&1
[ "$(pushes)" -eq 1 ] && ok "a new condition pushes once" || no "first push" "sent $(pushes)"

# 2. the same condition, repeatedly: silence. This is the 261-email bug.
for _ in 1 2 3 4; do echo "disk 94% full" | run --key disk >/dev/null 2>&1; done
[ "$(pushes)" -eq 1 ] && ok "unchanged condition stays quiet" \
  || no "repeat spam" "sent $(pushes) for one unchanged condition"

# 3. a CHANGED body is new information and must get through
echo "disk 99% full" | run --key disk >/dev/null 2>&1
[ "$(pushes)" -eq 2 ] && ok "a changed condition pushes again" || no "change missed" "sent $(pushes)"

# 4. distinct keys must not shadow each other
echo "cert expires in 3 days" | run --key cert >/dev/null 2>&1
[ "$(pushes)" -eq 3 ] && ok "a different key is tracked separately" || no "key collision" "sent $(pushes)"

# 5. still unresolved gets its daily reminder, labelled as one
REMIND=0 run --key disk --title disk <<< "disk 99% full" >/dev/null 2>&1
[ "$(pushes)" -eq 4 ] && ok "persistent condition is repeated after the interval" \
  || no "no reminder" "sent $(pushes)"
grep -q 'still unresolved' "$T/push" && ok "the reminder says it is a reminder" \
  || no "reminder wording" "indistinguishable from a fresh alert"

# 6. resolving sends exactly one all-clear, then nothing
run --resolve disk >/dev/null 2>&1
[ "$(pushes)" -eq 5 ] && ok "resolve sends one all-clear" || no "all-clear" "sent $(pushes)"
run --resolve disk >/dev/null 2>&1
[ "$(pushes)" -eq 5 ] && ok "resolving twice stays silent" || no "double all-clear" "sent $(pushes)"

# 7. THE IMPORTANT ONE: ntfy down must not lose the alert
touch "$T/faildown"
echo "mail queue backed up" | run --key mailq --title mailq >/dev/null 2>&1
[ "$(mails)" -ge 1 ] && ok "falls back to mail when push fails" \
  || no "alert lost" "push failed and nothing was mailed"
grep -q 'mail queue backed up' "$T/mail" && ok "the fallback carries the message" \
  || no "empty fallback" "mail sent but body missing"

# 8. a key is mandatory — without one there is no dedup and we are back
#    to the 261-email failure
echo body | run --title "no key" >/dev/null 2>&1
[ $? -ne 0 ] && ok "refuses to run without a key" || no "no key accepted" "dedup would be impossible"

echo "────────────────"; echo "pass=$P fail=$F"; exit "$F"
