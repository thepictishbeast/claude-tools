#!/usr/bin/env bash
# Black-box proof of `claude-loop sentinel`: feed the REAL binary synthetic
# transcripts in throwaway --state-dirs (email + desktop disabled) and assert
# the outcome per case. No real sessions, no real emails, no malicious content —
# only benign text that *baits* the detector. Reports a false-positive /
# false-negative matrix. Run: BIN=~/.local/bin/claude-loop bash test/sentinel-proof.sh
set -u
BIN="${CLAUDE_LOOP:-${BIN:-claude-loop}}"
BASE="$(mktemp -d)"; trap 'rm -rf "$BASE"' EXIT
pass=0; fail=0; fp=0; fn=0

# err <code> <entrypoint> <message> → one isApiErrorMessage transcript line
err(){ printf '{"message":{"content":[{"type":"text","text":"%s"}]},"error":"%s","isApiErrorMessage":true,"entrypoint":"%s"}' "$3" "$1" "$2"; }

# run <name> <expected-status> <jsonl> [mtime]
run(){
  local name="$1" expected="$2" content="$3" mtime="${4:-}"
  local d="$BASE/$name"; mkdir -p "$d/projects/s"
  printf '{"type":"user"}\n%s\n' "$content" > "$d/projects/s/11111111-1111-1111-1111-111111111111.jsonl"
  printf '{"email_enabled":false,"desktop_enabled":false}' > "$d/.sentinel.json"
  [ -n "$mtime" ] && touch -d "$mtime" "$d/projects/s/"*.jsonl
  local out status
  out=$("$BIN" --state-dir "$d" sentinel 2>/dev/null)
  status=$(printf '%s' "$out" | grep -oE '"status":"[^"]*"' | head -1 | sed 's/.*:"//;s/"$//')
  if [ "$status" = "$expected" ]; then printf 'PASS  %-26s → %s\n' "$name" "$status"; pass=$((pass+1))
  else printf 'FAIL  %-26s → got "%s" expected "%s"\n' "$name" "$status" "$expected"; fail=$((fail+1))
    case "$expected:$status" in ACTIONED:*) fn=$((fn+1));; *:ACTIONED) fp=$((fp+1));; esac
  fi
}

CYBER="API Error: Opus 4.8's safeguards flagged this message for a cybersecurity topic. Apply for an exemption: https://claude.com/form/cyber-use-case?token=x"

echo "== true positives (interactive/loop cyber + usage-policy blocks) =="
run cyber_cli            ACTIONED "$(err invalid_request cli "$CYBER")"
run usage_policy_cli     ACTIONED "$(err invalid_request cli 'Claude Code is unable to respond to this request, which appears to violate our Usage Policy.')"
run fn_phrasing_variant  ACTIONED "$(err invalid_request cli 'API Error: safeguards flagged for a cybersecurity topic.')"
echo "== sub-agent scope (real block in sdk-* → ignore) =="
run cyber_sdkpy          ignored-sdk-subagent "$(err invalid_request sdk-py "$CYBER")"
run cyber_sdkcli         ignored-sdk-subagent "$(err invalid_request sdk-cli "$CYBER")"
echo "== true negatives (transient/other API errors → quiet) =="
run overloaded_cli       healthy "$(err overloaded_error cli 'Overloaded')"
run ratelimit_cli        healthy "$(err rate_limit_error cli 'Number of request tokens has exceeded your rate limit')"
run auth_cli             healthy "$(err authentication_failed cli 'Not logged in')"
run server_err_cli       healthy "$(err api_error cli 'Internal server error')"
echo "== false-positive bait (benign text w/ scary words → quiet) =="
run fp_flagged_billing   healthy "$(err invalid_request cli 'Your payment method was flagged for manual review by billing.')"
run fp_violates_policy   healthy "$(err invalid_request cli 'Request rejected: it violates our retry policy, please back off.')"
run fp_security_word     healthy "$(err invalid_request cli 'A security update is required before this request can proceed.')"
run fp_content_mention   healthy '{"type":"assistant","message":{"content":[{"type":"text","text":"Let us discuss the cybersecurity topic and the cyber-use-case form."}]}}'
echo "== recency + clean =="
run recency_old_block    healthy "$(err invalid_request cli "$CYBER")" "20 minutes ago"
run clean_transcript     healthy '{"type":"assistant","message":{"content":[{"type":"text","text":"all good"}]}}'
echo "== idempotency (same incident twice → 2nd already-actioned) =="
d="$BASE/idem"; mkdir -p "$d/projects/s"
printf '{"type":"user"}\n%s\n' "$(err invalid_request cli "$CYBER")" > "$d/projects/s/22222222-2222-2222-2222-222222222222.jsonl"
printf '{"email_enabled":false,"desktop_enabled":false}' > "$d/.sentinel.json"
s1=$("$BIN" --state-dir "$d" sentinel 2>/dev/null | grep -oE '"status":"[^"]*"' | head -1 | sed 's/.*:"//;s/"$//')
s2=$("$BIN" --state-dir "$d" sentinel 2>/dev/null | grep -oE '"status":"[^"]*"' | head -1 | sed 's/.*:"//;s/"$//')
if [ "$s1" = ACTIONED ] && [ "$s2" = already-actioned ]; then printf 'PASS  %-26s → %s then %s\n' idempotency "$s1" "$s2"; pass=$((pass+1)); else printf 'FAIL  idempotency → %s then %s\n' "$s1" "$s2"; fail=$((fail+1)); fi

echo; echo "passed=$pass failed=$fail  FALSE-POSITIVES=$fp  FALSE-NEGATIVES=$fn"
[ "$fail" -eq 0 ] && echo "PROVEN: 0 false positives, 0 false negatives across the corpus." || { echo "REGRESSIONS ABOVE."; exit 1; }
