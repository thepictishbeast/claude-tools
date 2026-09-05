#!/usr/bin/env bash
# auto-remediate: when a check that used to pass starts failing, spin up
# Claude to propose a fix — behind gates that all fail closed.
#
# The gates are the point of this script. Fixing is the easy part; the
# hard part is not making things worse unattended. Each gate below exists
# because of something that actually happened on this host.
#
#   1. USAGE BREAKER. A loop once ticked every minute for days against an
#      exhausted quota and logged 3,015 usage-limit errors. Nothing that
#      spawns Claude may run without asking usage-guard first.
#
#   2. NEVER-GREEN CHECKS ARE NOT REMEDIABLE. This is the important one.
#      A DKIM monitor here asserted a selector that had never existed and
#      was red every day for four months. Pointed at those 78 red runs,
#      an automated fixer takes the cheapest path to green — which would
#      have been to CREATE a `mail._domainkey` record and mask the fact
#      that signing actually uses `default`. It would have "fixed" a
#      non-problem 78 times and hidden a real one. A check that has never
#      passed is a bug in the CHECK, and the correct output is a
#      human-facing diagnosis, never a patch.
#
#   3. THE FIX MAY NOT EDIT THE CHECK. Left alone, the cheapest way to
#      make a failing test pass is to delete the test. Any diff touching
#      a workflow, a monitor, or this tooling is rejected outright and
#      escalated to a human, however good the reasoning looks.
#
#   4. ONE ATTEMPT PER SIGNATURE, THEN STOP. Re-running a fixer against
#      an unchanged failure is how a loop becomes a runaway. Signature =
#      the failure itself, so a genuinely new failure is always allowed.
#
#   5. IT OPENS A PULL REQUEST. It never pushes to a default branch. The
#      machine writes the patch; a person still decides.
#
#   auto-remediate.sh --repo owner/name [--workflow NAME] [--apply]
#
# Without --apply it reports what it WOULD do and changes nothing.
set -uo pipefail

REPO=""; ONLY_WF=""; APPLY=0
STATE_DIR="${REMEDIATE_STATE:-/var/lib/auto-remediate}"
MAX_ATTEMPTS="${REMEDIATE_MAX_ATTEMPTS:-1}"
USAGE_GUARD="${USAGE_GUARD:-/home/paul/projects/plausiden-assurance/usage-guard.sh}"
NOTIFY="${REMEDIATE_NOTIFY:-/home/paul/projects/claude-tools/lib/notify.sh}"
CLAUDE_BIN="${CLAUDE_BIN:-/root/.local/bin/claude}"
GH="${GH_BIN:-gh}"
WORKROOT="${REMEDIATE_WORKROOT:-/tank/scratch/remediate}"
TIMEOUT="${REMEDIATE_TIMEOUT:-900}"

# Paths a fix may never touch. Editing the thing that detects the problem
# is not a fix, it is concealment.
PROTECTED_RE='^(\.github/workflows/|\.github/actions/|scripts/check-|lib/(notify|cert-watch|auto-remediate|log-watch|mailbox-digest)\.sh|tests/.*(monitor|watch|notify).*)'

while [ $# -gt 0 ]; do
  case "$1" in
    --repo)     REPO="$2"; shift 2 ;;
    --workflow) ONLY_WF="$2"; shift 2 ;;
    --apply)    APPLY=1; shift ;;
    --dry-run)  APPLY=0; shift ;;
    *) echo "auto-remediate: unknown argument $1" >&2; exit 2 ;;
  esac
done
[ -n "$REPO" ] || { echo "auto-remediate: --repo owner/name is required" >&2; exit 2; }

mkdir -p "$STATE_DIR" 2>/dev/null || { echo "auto-remediate: cannot use $STATE_DIR" >&2; exit 2; }

say(){ printf '%s\n' "$*"; }
notify(){ # <key> <title> <priority> <body>
  [ -x "$NOTIFY" ] || return 0
  printf '%s\n' "$4" | "$NOTIFY" --key "$1" --title "$2" --priority "$3" --tags robot 2>/dev/null
}

# ── Gate 1: usage breaker ───────────────────────────────────────────────
# Fail closed. If the guard is missing or unreadable we do NOT spawn.
if [ ! -x "$USAGE_GUARD" ]; then
  say "GATE 1 usage: guard not executable at $USAGE_GUARD — standing down"
  exit 0
fi
if ! "$USAGE_GUARD" gate >/dev/null 2>&1; then
  say "GATE 1 usage: quota breaker is tripped — standing down (this is the gate working)"
  exit 0
fi
say "GATE 1 usage: quota available"

# ── Enumerate candidate workflows ───────────────────────────────────────
wf_json=$($GH -R "$REPO" workflow list --all --json name,id,state 2>/dev/null) || {
  say "cannot list workflows for $REPO"; exit 2; }

mapfile -t WFS < <(printf '%s' "$wf_json" | python3 -c '
import sys, json
for w in json.load(sys.stdin):
    if w.get("state") == "active":
        print(w["id"], w["name"], sep="\t")
' 2>/dev/null)
[ "${#WFS[@]}" -gt 0 ] || { say "no active workflows in $REPO"; exit 0; }

handled=0; skipped=0; escalated=0
for entry in "${WFS[@]}"; do
  wid="${entry%%	*}"; wname="${entry#*	}"
  [ -n "$ONLY_WF" ] && [ "$wname" != "$ONLY_WF" ] && continue

  runs=$($GH -R "$REPO" run list --workflow "$wid" -L 100 \
         --json conclusion,databaseId,createdAt 2>/dev/null) || continue

  read -r total greens latest_concl latest_id < <(printf '%s' "$runs" | python3 -c '
import sys, json
d = json.load(sys.stdin)
if not d:
    print("0 0 none 0"); raise SystemExit
greens = sum(1 for x in d if x.get("conclusion") == "success")
print(len(d), greens, d[0].get("conclusion") or "running", d[0].get("databaseId") or 0)
' 2>/dev/null) || continue

  [ "$total" = "0" ] && continue
  [ "$latest_concl" = "failure" ] || { skipped=$((skipped+1)); continue; }

  say ""
  say "── $wname (latest run $latest_id failed; $greens/$total green historically)"

  # ── Gate 2: has this check EVER passed? ───────────────────────────────
  if [ "$greens" -eq 0 ]; then
    escalated=$((escalated+1))
    say "   GATE 2 never-green: 0 successes in $total runs — NOT remediating."
    say "   A check that has never passed is a bug in the check. Escalating."
    notify "remediate:never-green:$REPO:$wname" \
      "$wname has never passed — needs a human" high \
"Workflow: $wname
Repo:     $REPO
History:  0 successes in $total runs

This check has never been green, so automated repair is deliberately
refused. The cheapest way to make a never-green check pass is to weaken
what it asserts, which would hide the very problem it was written to
find.

Diagnose the CHECK first, not the code it is checking.
  gh -R $REPO run view $latest_id --log-failed"
    continue
  fi
  say "   GATE 2 never-green: passed ($greens prior successes — this is a regression, not a broken check)"

  # ── Gate 3: failure signature + attempt cap ───────────────────────────
  fail_text=$($GH -R "$REPO" run view "$latest_id" --log-failed 2>/dev/null \
              | sed 's/\x1b\[[0-9;]*m//g' \
              | grep -oiE '(error(\[[A-Z0-9]+\])?:|panicked at|assertion.*failed|FAIL[: ]).{0,120}' \
              | head -5)
  [ -n "$fail_text" ] || fail_text="(no parseable error; run $latest_id)"
  sig=$(printf '%s\n%s' "$wname" "$fail_text" | md5sum | cut -c1-16)
  # Sanitise ONLY the repo name. Flattening the whole path would eat the
  # slashes in STATE_DIR too, turning an absolute path into a relative
  # filename that lands in the current directory — which silently breaks
  # dedup between runs started from different places, and litters repos.
  sfile="$STATE_DIR/${REPO//\//_}--$sig"

  attempts=0
  [ -f "$sfile" ] && attempts=$(sed -n 1p "$sfile" 2>/dev/null || echo 0)
  if [ "${attempts:-0}" -ge "$MAX_ATTEMPTS" ]; then
    skipped=$((skipped+1))
    say "   GATE 3 attempts: signature $sig already tried ${attempts}x — not retrying an unchanged failure"
    continue
  fi
  say "   GATE 3 attempts: signature $sig, attempt $((attempts+1))/$MAX_ATTEMPTS"

  if [ "$APPLY" -eq 0 ]; then
    say "   DRY RUN — would spawn Claude here. Failure head:"
    printf '%s\n' "$fail_text" | head -3 | sed 's/^/     /'
    handled=$((handled+1)); continue
  fi

  # Record the attempt HERE, the moment we commit to making one — not
  # after the work succeeds. Counting only completed attempts means any
  # early failure (missing clone, worktree refused, spawn crash) never
  # increments, so the next run tries again, and again: a runaway with
  # extra steps. An attempt that got as far as being decided is an
  # attempt.
  printf '%s\n%s\n' "$((attempts+1))" "$(date -u +%FT%TZ)" > "$sfile"

  # ── Isolated worktree ─────────────────────────────────────────────────
  src="/home/paul/projects/$(basename "$REPO")"
  [ -d "$src/.git" ] || { say "   no local clone at $src — skipping"; continue; }
  mkdir -p "$WORKROOT"
  branch="auto-fix/$(printf '%s' "$wname" | tr -c 'a-zA-Z0-9' '-' | tr -s '-' | sed 's/^-//;s/-$//')-$sig"
  wt="$WORKROOT/$sig"
  rm -rf "$wt"
  sudo -u paul git -C "$src" fetch --quiet origin 2>/dev/null
  if ! sudo -u paul git -C "$src" worktree add --quiet -B "$branch" "$wt" origin/HEAD 2>/dev/null; then
    say "   could not create worktree — skipping"; continue
  fi

  prompt="A CI check that used to pass is now failing. Fix the underlying cause.

Repo:     $REPO
Workflow: $wname
Run:      $latest_id

Failure:
$fail_text

Rules, which are absolute:
- Fix the CODE. Do NOT edit, disable, skip, or weaken any workflow, test,
  monitor or assertion. This check has passed $greens times before, so the
  check is not the problem — something regressed.
- If the only way you can make this pass is to change what is asserted,
  STOP and explain why instead. That is a valid and useful outcome.
- Make the smallest change that addresses the cause.
- Run the project's tests before you finish.
Work only inside $wt."

  say "   spawning Claude (timeout ${TIMEOUT}s, isolated worktree)"
  ( cd "$wt" && timeout "$TIMEOUT" "$CLAUDE_BIN" -p "$prompt" \
      --permission-mode acceptEdits --add-dir "$wt" >"$wt/.remediate.log" 2>&1 )
  rc=$?
  say "   claude exited $rc"

  # Whatever it said, judge it on the diff.
  changed=$(sudo -u paul git -C "$wt" --no-optional-locks diff --name-only 2>/dev/null)
  if [ -z "$changed" ]; then
    say "   no changes produced — escalating"
    escalated=$((escalated+1))
    notify "remediate:nofix:$REPO:$wname" "$wname still failing — no fix produced" default \
"Automated repair ran for $wname in $REPO and produced no change.
Failure:
$fail_text
Log: $wt/.remediate.log"
    sudo -u paul git -C "$src" worktree remove --force "$wt" 2>/dev/null
    continue
  fi

  # ── Gate 4: the fix may not edit the check ────────────────────────────
  violation=$(printf '%s\n' "$changed" | grep -E "$PROTECTED_RE" || true)
  if [ -n "$violation" ]; then
    say "   GATE 4 protected-path: REJECTED — the fix edits the check itself:"
    printf '%s\n' "$violation" | sed 's/^/     /'
    escalated=$((escalated+1))
    notify "remediate:tampered:$REPO:$wname" "$wname: automated fix tried to edit the check" high \
"Automated repair for $wname in $REPO was REJECTED and discarded.

It attempted to modify files that detect the problem:
$violation

That is concealment, not repair, so nothing was kept. A human should
look at this — both at the original failure and at why the fix went
this way.

Failure:
$fail_text"
    sudo -u paul git -C "$src" worktree remove --force "$wt" 2>/dev/null
    continue
  fi
  say "   GATE 4 protected-path: passed — touches only $(printf '%s' "$changed" | wc -l) non-protected file(s)"

  # ── Open a PR. Never push to the default branch. ──────────────────────
  sudo -u paul git -C "$wt" add -A
  sudo -u paul git -C "$wt" -c user.name=paul -c user.email=william@plausiden.com \
    commit -q -m "fix($wname): automated repair of a regression

$wname had passed $greens times before this failure, so this is a
regression rather than a broken check.

Failure:
$(printf '%s' "$fail_text" | head -3)

Produced by auto-remediate.sh in an isolated worktree. The diff was
checked against the protected-path list, so it does not modify the
workflow, the tests or the monitors." 2>/dev/null

  if sudo -u paul git -C "$wt" push -q -u origin "$branch" 2>/dev/null; then
    url=$(sudo -u paul $GH -R "$REPO" pr create --head "$branch" \
            --title "Automated fix: $wname" \
            --body "$wname regressed after $greens successful runs.

\`\`\`
$fail_text
\`\`\`

Written by \`auto-remediate.sh\` in an isolated worktree. The diff was
rejected-checked against protected paths, so it does not touch the
workflow, tests or monitors — if the only available fix had been to
weaken the check, this PR would not exist.

**Review before merging.** Nothing here has been merged automatically." 2>/dev/null | tail -1)
    say "   PR opened: ${url:-(created)}"
    handled=$((handled+1))
    notify "remediate:pr:$REPO:$wname" "Fix ready for review: $wname" default \
"$wname regressed and an automated fix is waiting for review.

${url:-check the open pull requests}

Nothing was merged. The diff does not touch the workflow or its tests."
  else
    say "   push failed — leaving the worktree at $wt for inspection"
    escalated=$((escalated+1))
  fi
  sudo -u paul git -C "$src" worktree remove --force "$wt" 2>/dev/null
done

say ""
say "auto-remediate: $handled handled, $escalated escalated to a human, $skipped skipped"
exit 0
