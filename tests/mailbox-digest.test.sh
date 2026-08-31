#!/usr/bin/env bash
# Nothing arriving at a secondary address should go unseen — and the digest
# that ensures that must not itself become noise.
#
# Mail lands at security@, team@, tlsrpt@ and others; only william@ is read, and
# nothing forwards the rest. security@ in particular holds inbound reports from
# researchers, which is the worst possible thing to miss.
set -uo pipefail
S="${DIGEST_SH:-/home/paul/projects/claude-tools/lib/mailbox-digest.sh}"
P=0; F=0
ok(){ echo "ok   $1"; P=$((P+1)); }
no(){ echo "FAIL $1 — $2"; F=$((F+1)); }
[ -f "$S" ] || { echo "no such script: $S" >&2; exit 2; }

T=$(mktemp -d /tank/scratch/.dig.XXXXXX); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/root"/{security,team,quietbox}/{new,cur}
cat > "$T/bin/sendmail" <<'EOF'
#!/usr/bin/env bash
cat >> "$MAILLOG"; printf '\n===MAIL===\n' >> "$MAILLOG"
EOF
chmod +x "$T/bin/sendmail"

msg(){ printf 'From: %s\nSubject: %s\nDate: Mon, 1 Jan 2026 00:00:00 +0000\n\nbody\n' "$2" "$3" > "$T/root/$1/new/$4"; }

run(){ MAILLOG="$T/mail" PATH="$T/bin:$PATH" DIGEST_ROOT="$T/root" \
       DIGEST_STATE="$T/seen" DIGEST_BOXES="security team quietbox" \
       DIGEST_URGENT="security" bash "$S" >/dev/null 2>&1 || true; }
mails(){ { grep -c '===MAIL===' "$T/mail" 2>/dev/null || true; } | head -1; }
: > "$T/mail"

# 1. nothing to report -> total silence
run; [ "$(mails)" -eq 0 ] && ok "silent when nothing has arrived" || no "noisy idle" "sent $(mails)"

# 2. ordinary mail at an unread address gets surfaced
msg team 'client@example.com' 'Invoice question' m1
run; [ "$(mails)" -eq 1 ] && ok "new mail at a secondary address is reported" || no "missed" "sent $(mails)"
grep -q 'Invoice question' "$T/mail" && ok "the digest names the subject" || no "no subject" "subject absent"

# 3. the SAME mail must never be reported twice — otherwise the digest becomes
#    the very noise it was built to avoid
run; run
[ "$(mails)" -eq 1 ] && ok "already-reported mail is never repeated" || no "repeat" "sent $(mails)"

# 4. security@ is escalated, not folded into the daily digest
msg security 'researcher@example.org' 'Private security report: path traversal' s1
run
grep -q 'SECURITY' "$T/mail" && ok "security mail is escalated separately" \
  || no "security not escalated" "treated as ordinary digest traffic"
grep -q 'path traversal' "$T/mail" && ok "the security subject is carried through" || no "security subject" "absent"

# 5. a second new item still gets through after previous ones were seen
msg quietbox 'someone@example.net' 'Later message' q1
before=$(mails); run
[ "$(mails)" -gt "$before" ] && ok "later arrivals still reported" || no "stuck" "nothing sent for new mail"

# 6. and it goes quiet again once everything is seen
before=$(mails); run; run
[ "$(mails)" -eq "$before" ] && ok "returns to silence once all is seen" || no "still noisy" "kept sending"

echo "────────────────"; echo "pass=$P fail=$F"; exit "$F"
