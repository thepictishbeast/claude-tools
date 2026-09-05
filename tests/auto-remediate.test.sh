#!/usr/bin/env bash
# The gates are the product. Fixing code is the easy half; not making
# things worse unattended is the hard half, and every test here pins a
# gate that exists because of something that actually happened.
#
# NOTE ON THE STUBS: they live on /tank/scratch, never /tmp. /tmp is
# mounted noexec here, so a stub placed there cannot execute and bash
# silently falls through to the REAL command. That exact mistake stopped
# Caddy and took every site down for two and a half minutes while the
# test appeared to pass. Each stub announces itself, and the harness
# refuses to run until it has confirmed interception.
set -uo pipefail
S="${REMEDIATE_SH:-/home/paul/projects/claude-tools/lib/auto-remediate.sh}"
P=0; F=0
ok(){ echo "ok   $1"; P=$((P+1)); }
no(){ echo "FAIL $1 — $2"; F=$((F+1)); }
[ -f "$S" ] || { echo "no such script: $S" >&2; exit 2; }

T=$(mktemp -d /tank/scratch/.rem.XXXXXX) || exit 2
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/state"

# ── stubs ───────────────────────────────────────────────────────────────
cat > "$T/bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "STUB-GH $*" >> "$GHLOG"
args="$*"
case "$args" in
  *"workflow list"*) cat "$FIX_WFLIST" ;;
  *"run list"*)      cat "$FIX_RUNS" ;;
  *"run view"*)      cat "$FIX_LOG" 2>/dev/null || true ;;
  *"pr create"*)     echo "https://github.com/x/y/pull/1" ;;
esac
EOF
cat > "$T/bin/usage-guard.sh" <<'EOF'
#!/usr/bin/env bash
echo "STUB-GUARD $*" >> "$GHLOG"
[ -f "$PAUSED" ] && exit 3 || exit 0
EOF
cat > "$T/bin/notify.sh" <<'EOF'
#!/usr/bin/env bash
echo "STUB-NOTIFY $*" >> "$NOTIFYLOG"; cat >> "$NOTIFYLOG"; echo "===" >> "$NOTIFYLOG"
EOF
cat > "$T/bin/claude" <<'EOF'
#!/usr/bin/env bash
echo "STUB-CLAUDE spawned" >> "$GHLOG"
EOF
chmod +x "$T/bin"/*

# The instrument check the /tmp incident taught us to do. The stub logs to
# $GHLOG rather than stdout, so the probe must read that file — checking
# stdout would "pass" for a stub that never ran at all.
PATH="$T/bin:$PATH" GHLOG="$T/probe.log" bash -c 'gh whatever' >/dev/null 2>&1 || true
if ! grep -q 'STUB-GH' "$T/probe.log" 2>/dev/null; then
  echo "ABORT: stubs are not intercepting — refusing to run (a real gh would be called)"; exit 2
fi
PATH="$T/bin:$PATH" GHLOG="$T/probe.log" PAUSED="$T/nonexistent" \
  bash -c '"'"$T"'/bin/usage-guard.sh" gate' >/dev/null 2>&1
grep -q 'STUB-GUARD' "$T/probe.log" || { echo "ABORT: guard stub not intercepting"; exit 2; }
echo "instrument check: gh + usage-guard stubs both intercept correctly"
echo

runs(){ # <total> <greens> <latest-conclusion>
  python3 -c "
import json
t,g,c=$1,$2,'$3'
d=[{'conclusion':c,'databaseId':999,'createdAt':'2026-09-05T00:00:00Z'}]
d+=[{'conclusion':'success','databaseId':i,'createdAt':'2026-09-01T00:00:00Z'} for i in range(g)]
d+=[{'conclusion':'failure','databaseId':i,'createdAt':'2026-09-01T00:00:00Z'} for i in range(t-g-1)]
print(json.dumps(d))"
}
echo '[{"id":1,"name":"ci","state":"active"}]' > "$T/wflist.json"
printf 'error: something broke in module X\n' > "$T/log.txt"

run(){ PATH="$T/bin:$PATH" GHLOG="$T/gh.log" NOTIFYLOG="$T/notify.log" PAUSED="$T/paused" \
  FIX_WFLIST="$T/wflist.json" FIX_RUNS="$T/runs.json" FIX_LOG="$T/log.txt" \
  REMEDIATE_STATE="$T/state" USAGE_GUARD="$T/bin/usage-guard.sh" \
  REMEDIATE_NOTIFY="$T/bin/notify.sh" CLAUDE_BIN="$T/bin/claude" GH_BIN=gh \
  bash "$S" --repo test/repo "$@" 2>&1; }
reset(){ : > "$T/gh.log"; : > "$T/notify.log"; rm -f "$T/paused"; rm -f "$T/state"/*; }

# 1. USAGE BREAKER — the 3,015-error lesson. Nothing may spawn while paused.
reset; touch "$T/paused"; runs 100 50 failure > "$T/runs.json"
out=$(run --apply)
grep -q 'standing down' <<<"$out" && ok "usage breaker stops the run" || no "usage gate" "did not stand down"
grep -q 'STUB-CLAUDE' "$T/gh.log" && no "SPAWNED WHILE PAUSED" "the breaker was bypassed" \
  || ok "nothing is spawned while the quota breaker is tripped"

# 1b. fail closed if the guard is missing entirely
reset; runs 100 50 failure > "$T/runs.json"
out=$(PATH="$T/bin:$PATH" GHLOG="$T/gh.log" NOTIFYLOG="$T/notify.log" PAUSED="$T/nope" \
  FIX_WFLIST="$T/wflist.json" FIX_RUNS="$T/runs.json" FIX_LOG="$T/log.txt" \
  REMEDIATE_STATE="$T/state" USAGE_GUARD="$T/does-not-exist" \
  REMEDIATE_NOTIFY="$T/bin/notify.sh" CLAUDE_BIN="$T/bin/claude" GH_BIN=gh \
  bash "$S" --repo test/repo --apply 2>&1)
grep -q 'standing down' <<<"$out" && ok "missing guard fails CLOSED, not open" || no "fail-open" "ran without a guard"

# 2. NEVER-GREEN — the DKIM lesson. 0 successes must never be auto-fixed.
reset; runs 78 0 failure > "$T/runs.json"
out=$(run --apply)
grep -q 'NOT remediating' <<<"$out" && ok "never-green check is refused, not patched" || no "never-green" "attempted a fix"
grep -q 'STUB-CLAUDE' "$T/gh.log" && no "SPAWNED ON NEVER-GREEN" "would have weakened the check" \
  || ok "no fixer is spawned for a check that has never passed"
grep -q 'never been green' "$T/notify.log" && ok "never-green escalates to a human with a reason" \
  || no "no escalation" "stayed silent"

# 3. A REGRESSION (has passed before) is eligible
reset; runs 100 90 failure > "$T/runs.json"
out=$(run)   # dry run
grep -q 'GATE 2 never-green: passed' <<<"$out" && ok "a check with history is treated as a regression" \
  || no "regression gate" "blocked a legitimate candidate"
grep -q 'DRY RUN' <<<"$out" && ok "dry run changes nothing by default" || no "dry run" "not honoured"

# 4. A PASSING check is left alone
reset; runs 100 90 success > "$T/runs.json"
out=$(run --apply)
grep -q 'GATE 2' <<<"$out" && no "acted on a green check" "should have skipped" \
  || ok "a currently-passing check is ignored"

# 5. ATTEMPT CAP — the runaway lesson. Same failure twice = stop.
reset; runs 100 90 failure > "$T/runs.json"
run --apply >/dev/null 2>&1
out=$(run --apply)
grep -q 'not retrying an unchanged failure' <<<"$out" \
  && ok "an unchanged failure is not retried" || no "attempt cap" "would loop on the same failure"

# 6. A DIFFERENT failure is a new signature and IS allowed through
printf 'error: a completely different fault\n' > "$T/log.txt"
out=$(run --apply)
grep -q 'not retrying' <<<"$out" && no "new failure blocked" "cap keyed on the wrong thing" \
  || ok "a genuinely new failure is still eligible"

echo "────────────────"; echo "pass=$P fail=$F"; exit "$F"
