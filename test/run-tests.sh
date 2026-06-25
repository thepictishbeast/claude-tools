#!/bin/sh
# Local test runner. Validates SKILL.md frontmatter, install.sh,
# README structure. Runs from the repo root or test/ directory.
#
# Usage: ./test/run-tests.sh
# Exit: 0 if all tests pass, nonzero otherwise.

set -eu

# Resolve repo root
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

PASS=0
FAIL=0
fail() { echo "  FAIL: $*"; FAIL=$((FAIL+1)); }
pass() { echo "  pass: $*"; PASS=$((PASS+1)); }

# ----------------------------------------------------------------------
echo "[1] SKILL.md frontmatter validation"

for f in skills/*/SKILL.md; do
    skill="$(basename "$(dirname "$f")")"

    # Must start with --- (frontmatter open)
    if ! head -1 "$f" | grep -qx '\-\-\-'; then
        fail "$skill: SKILL.md does not start with ---"
        continue
    fi

    # Must have a closing ---
    if ! awk 'NR==1 && /^---$/{c++; next} c==1 && /^---$/{c++; print NR; exit}' "$f" | grep -q .; then
        fail "$skill: SKILL.md missing closing ---"
        continue
    fi

    # Extract frontmatter
    fm="$(awk '/^---$/{c++; if(c==2)exit} c==1' "$f")"

    # Must have name field
    if ! echo "$fm" | grep -qE '^name: '; then
        fail "$skill: missing 'name:' field"
        continue
    fi

    # Must have description field
    if ! echo "$fm" | grep -qE '^description: '; then
        fail "$skill: missing 'description:' field"
        continue
    fi

    # name should match directory
    declared_name="$(echo "$fm" | awk -F': ' '/^name: /{print $2; exit}')"
    if [ "$declared_name" != "$skill" ]; then
        fail "$skill: frontmatter name '$declared_name' does not match directory '$skill'"
        continue
    fi

    # description should be non-empty
    desc="$(echo "$fm" | awk -F': ' '/^description: /{print $2; exit}')"
    if [ -z "$desc" ]; then
        fail "$skill: description is empty"
        continue
    fi

    pass "$skill: frontmatter valid (name=$skill, description present)"
done

# ----------------------------------------------------------------------
echo ""
echo "[2] install.sh syntax check"

if sh -n install.sh; then
    pass "install.sh: syntax OK"
else
    fail "install.sh: syntax error"
fi

# ----------------------------------------------------------------------
echo ""
echo "[3] install.sh dry-run into temp HOME"

TESTHOME="$(mktemp -d)"
trap 'rm -rf "$TESTHOME"' EXIT

if HOME="$TESTHOME" sh install.sh -f >/dev/null 2>&1; then
    # Verify each skill was installed
    for skill in skills/*/; do
        name="$(basename "$skill")"
        target="$TESTHOME/.claude/skills/$name/SKILL.md"
        if [ -f "$target" ]; then
            pass "install: $name → $target"
        else
            fail "install: $name not at $target"
        fi
    done
else
    fail "install.sh: failed to run with HOME=$TESTHOME"
fi

# ----------------------------------------------------------------------
echo ""
echo "[4] README has required sections"

for section in "## Install" "## Usage" "## State files" "## License"; do
    if grep -qF "$section" README.md; then
        pass "README: has '$section' section"
    else
        fail "README: missing '$section' section"
    fi
done

# ----------------------------------------------------------------------
echo ""
echo "[5] state file path consistency"
# Each SKILL.md must reference the standard state file path
for f in skills/loop-pause/SKILL.md skills/loop-resume/SKILL.md skills/loops/SKILL.md; do
    name="$(basename "$(dirname "$f")")"
    if grep -qF ".paused-loops.json" "$f"; then
        pass "$name: references .paused-loops.json"
    else
        fail "$name: does not mention .paused-loops.json"
    fi
done

# ----------------------------------------------------------------------
echo ""
echo "[6] feature documentation tests"

# pause must document the history file
if grep -qF "loop-history.jsonl" skills/loop-pause/SKILL.md; then
    pass "pause: documents loop-history.jsonl"
else
    fail "pause: missing loop-history.jsonl reference"
fi

# pause must document the canary auto-add
if grep -qiE "canary|self.check" skills/loop-pause/SKILL.md; then
    pass "pause: documents canary auto-add"
else
    fail "pause: canary auto-add feature undocumented"
fi

# pause must refuse on truncated prompts (safety feature)
if grep -qiE "truncated|refuse" skills/loop-pause/SKILL.md; then
    pass "pause: documents refuse-on-truncated-prompt safety"
else
    fail "pause: missing truncated-prompt safeguard documentation"
fi

# resume must document the interval override feature
if grep -qiE "loop-resume 5m|interval.*override|argument" skills/loop-resume/SKILL.md; then
    pass "resume: documents interval override"
else
    fail "resume: interval override feature undocumented"
fi

# resume must document the cron-conversion table
if grep -qE "^\| .*[Pp]attern.*\|.*[Cc]ron" skills/loop-resume/SKILL.md; then
    pass "resume: documents interval→cron conversion table"
else
    fail "resume: missing interval→cron conversion table"
fi

# resume must document the immediate-execute-now behavior (per /loop)
if grep -qiE "execute.*now|don't wait.*first.*fire|first.*iter.*run.*now" skills/loop-resume/SKILL.md; then
    pass "resume: documents execute-now semantics"
else
    fail "resume: execute-now semantics undocumented"
fi

# loops must document the three views (active + paused + history)
if grep -qiE "active" skills/loops/SKILL.md && \
   grep -qiE "paused" skills/loops/SKILL.md && \
   grep -qiE "history" skills/loops/SKILL.md; then
    pass "loops: documents all three views"
else
    fail "loops: missing one of {active, paused, history} view"
fi

# ----------------------------------------------------------------------
echo ""
echo "[7] example files validate"

if [ -d examples ]; then
    # paused-loops.example.json must be valid JSON and an array
    if [ -f examples/paused-loops.example.json ]; then
        if python3 -c "
import json, sys
d = json.load(open('examples/paused-loops.example.json'))
assert isinstance(d, list), 'top level must be array'
for entry in d:
    for field in ('cron','prompt','recurring'):
        assert field in entry, f'entry missing {field}'
" 2>/dev/null; then
            pass "examples/paused-loops.example.json: valid JSON with required fields"
        else
            fail "examples/paused-loops.example.json: invalid or missing required fields"
        fi
    else
        fail "examples/paused-loops.example.json: file missing"
    fi

    # loop-history.example.jsonl must be one valid JSON object per line
    if [ -f examples/loop-history.example.jsonl ]; then
        if python3 -c "
import json
with open('examples/loop-history.example.jsonl') as f:
    for i, line in enumerate(f, 1):
        line = line.strip()
        if not line:
            continue
        d = json.loads(line)
        assert 'event' in d, f'line {i} missing event field'
        assert 'at' in d, f'line {i} missing at field'
" 2>/dev/null; then
            pass "examples/loop-history.example.jsonl: valid JSONL with required fields"
        else
            fail "examples/loop-history.example.jsonl: invalid or missing required fields"
        fi
    else
        fail "examples/loop-history.example.jsonl: file missing"
    fi

    # examples/README.md must explain both files
    if [ -f examples/README.md ]; then
        if grep -qF "paused-loops.example.json" examples/README.md && \
           grep -qF "loop-history.example.jsonl" examples/README.md; then
            pass "examples/README.md: documents both example files"
        else
            fail "examples/README.md: doesn't reference both example files"
        fi
    else
        fail "examples/README.md: file missing"
    fi
else
    fail "examples/ directory missing"
fi

# ----------------------------------------------------------------------
echo ""
echo "[7c] checkpoint + restore skills (full-session state save/load)"

for skill in checkpoint restore; do
    if [ -f "skills/$skill/SKILL.md" ]; then
        pass "skill: $skill exists"
    else
        fail "skill: $skill missing"
        continue
    fi
    declared="$(awk '/^---$/{c++; if(c==2)exit} c==1 && /^name: /{print $2; exit}' "skills/$skill/SKILL.md")"
    if [ "$declared" = "$skill" ]; then
        pass "skill: $skill frontmatter name matches dir"
    else
        fail "skill: $skill frontmatter name is '$declared' (want '$skill')"
    fi
done

# checkpoint must document the .checkpoint/ state dir
if grep -qF '.checkpoint/' skills/checkpoint/SKILL.md; then
    pass "checkpoint: documents ~/.claude/.checkpoint/ state dir"
else
    fail "checkpoint: missing .checkpoint/ state dir reference"
fi

# checkpoint must list the captured artifacts (tasks, loops, processes, git)
for kind in tasks loops process git; do
    if grep -qi "$kind" skills/checkpoint/SKILL.md; then
        pass "checkpoint: captures $kind"
    else
        fail "checkpoint: doesn't mention capturing $kind"
    fi
done

# restore must reverse what checkpoint did
if grep -qiE "TaskCreate|re-create.*tasks" skills/restore/SKILL.md; then
    pass "restore: re-creates tasks"
else
    fail "restore: missing task re-create"
fi

if grep -qiE "/loop-resume|paused-loops.json" skills/restore/SKILL.md; then
    pass "restore: invokes /loop-resume"
else
    fail "restore: missing /loop-resume invocation"
fi

# restore must delete the checkpoint after consuming it
if grep -qiE "delete.*checkpoint|remove.*checkpoint|single.shot" skills/restore/SKILL.md; then
    pass "restore: deletes checkpoint after consuming"
else
    fail "restore: doesn't delete checkpoint on success"
fi

# CONTRIBUTING.md must exist + welcome AI agents
if [ -f CONTRIBUTING.md ] && grep -qiE "AI agent|Claude|Codex|Cursor" CONTRIBUTING.md; then
    pass "CONTRIBUTING.md: welcomes AI-agent contributors"
else
    fail "CONTRIBUTING.md: missing or doesn't welcome AI agents"
fi

# ----------------------------------------------------------------------
echo ""
echo "[7b] loop-edit + loop-stop skills"

for skill in loop-edit loop-stop; do
    if [ -f "skills/$skill/SKILL.md" ]; then
        pass "skill: $skill exists"
    else
        fail "skill: $skill missing"
        continue
    fi

    # frontmatter name matches dir
    declared="$(awk '/^---$/{c++; if(c==2)exit} c==1 && /^name: /{print $2; exit}' "skills/$skill/SKILL.md")"
    if [ "$declared" = "$skill" ]; then
        pass "skill: $skill frontmatter name matches dir"
    else
        fail "skill: $skill frontmatter name is '$declared' (want '$skill')"
    fi
done

# loop-edit must document the interval-or-prompt distinction
if grep -qiE "prompt:|prompt-append:" skills/loop-edit/SKILL.md; then
    pass "loop-edit: documents prompt: / prompt-append: syntax"
else
    fail "loop-edit: missing prompt: / prompt-append: syntax"
fi

# loop-edit must NOT execute immediately (different from /loop-resume)
if grep -qiE "Do NOT execute|not.*execute.*immediately" skills/loop-edit/SKILL.md; then
    pass "loop-edit: documents no-immediate-execute semantics"
else
    fail "loop-edit: missing no-immediate-execute semantics"
fi

# loop-stop must distinguish itself from loop-pause
if grep -qiE "pause.*resumable.*stop.*gone|stop.*permanently|pause.*temporarily" skills/loop-stop/SKILL.md; then
    pass "loop-stop: distinguishes from loop-pause"
else
    fail "loop-stop: doesn't clearly distinguish from loop-pause"
fi

# loop-stop must mention history event 'stopped'
if grep -qF '"event":"stopped"' skills/loop-stop/SKILL.md; then
    pass "loop-stop: logs 'stopped' history event"
else
    fail "loop-stop: missing 'stopped' history event"
fi

# ----------------------------------------------------------------------
echo ""
echo "[8] CLAUDE.md (loop hygiene doctrine)"

if [ -f CLAUDE.md ]; then
    pass "CLAUDE.md: file exists"
else
    fail "CLAUDE.md: missing (loop hygiene doctrine for AI agents)"
fi

# Must document the "don't restart on every fire" rule
if grep -qiE "fire is a SIGNAL to continue|don't.*restart|finish.*current" CLAUDE.md 2>/dev/null; then
    pass "CLAUDE.md: documents don't-restart-on-fire rule"
else
    fail "CLAUDE.md: missing the don't-restart-on-fire rule"
fi

# Must mention task list integration
if grep -qiE "TaskList|TaskCreate|task list" CLAUDE.md 2>/dev/null; then
    pass "CLAUDE.md: references task-list integration"
else
    fail "CLAUDE.md: missing task-list integration guidance"
fi

# Must explain tight-tick vs substantive
if grep -qiE "tight.tick|steady.state|substantive" CLAUDE.md 2>/dev/null; then
    pass "CLAUDE.md: distinguishes tight-tick vs substantive iters"
else
    fail "CLAUDE.md: missing tight-tick/substantive distinction"
fi

# README must point at CLAUDE.md
if grep -qF "CLAUDE.md" README.md; then
    pass "README: references CLAUDE.md"
else
    fail "README: missing CLAUDE.md reference"
fi

# loops SKILL.md must document the inflight-task view
if grep -qiE "Inflight tasks|TaskList|task state" skills/loops/SKILL.md; then
    pass "loops skill: documents inflight-task view"
else
    fail "loops skill: missing inflight-task view"
fi

# ----------------------------------------------------------------------
echo ""
echo "[9] New skills (loop-update / loop-track / loop-from-task / loop-on / loop-health)"
for skill in loop-update loop-track loop-from-task loop-on loop-health; do
    if [ -f "skills/$skill/SKILL.md" ]; then
        pass "skill: $skill exists"
    else
        fail "skill: $skill missing"
        continue
    fi
    declared=$(awk '/^name:/{print $2; exit}' "skills/$skill/SKILL.md")
    if [ "$declared" = "$skill" ]; then
        pass "skill: $skill frontmatter name matches dir"
    else
        fail "skill: $skill frontmatter name is '$declared' (want '$skill')"
    fi
done

if grep -q "auto-discovery" skills/loops/SKILL.md; then
    pass "loops: auto-discovery step documented"
else
    fail "loops: missing auto-discovery step"
fi

if grep -q "Auto-update claude-tools" skills/restore/SKILL.md; then
    pass "restore: auto-update step documented"
else
    fail "restore: missing auto-update step"
fi

if grep -q "In-flight TaskList check" skills/loop-pause/SKILL.md; then
    pass "loop-pause: in-flight TaskList check documented"
else
    fail "loop-pause: missing in-flight check"
fi

if grep -q "inflight_tasks" skills/loop-resume/SKILL.md; then
    pass "loop-resume: inflight_tasks replay documented"
else
    fail "loop-resume: missing inflight_tasks replay"
fi

if [ -x lib/rotate-history.sh ] && bash -n lib/rotate-history.sh; then
    pass "lib/rotate-history.sh: present + parses"
else
    fail "lib/rotate-history.sh: missing or syntax-broken"
fi

if [ -x update.sh ] && bash -n update.sh; then
    pass "update.sh: present + parses"
else
    fail "update.sh: missing or syntax-broken"
fi

if grep -q "Save state on EVERY" CLAUDE.md; then
    pass "CLAUDE.md: save-state-on-fire rule documented"
else
    fail "CLAUDE.md: missing save-state rule"
fi

echo ""
echo "[10] loop crash-guard + per-iteration checkpoint (stop-on-API-error)"

# LOOP_PATTERNS must document the crash-guard + checkpoint subcommands.
if grep -q "claude-loop guard" docs/LOOP_PATTERNS.md; then
    pass "LOOP_PATTERNS: documents 'claude-loop guard'"
else
    fail "LOOP_PATTERNS: missing 'claude-loop guard' crash-guard"
fi
if grep -q "claude-loop checkpoint" docs/LOOP_PATTERNS.md; then
    pass "LOOP_PATTERNS: documents 'claude-loop checkpoint'"
else
    fail "LOOP_PATTERNS: missing 'claude-loop checkpoint'"
fi
if grep -qF "ENTRYPOINT.md" docs/LOOP_PATTERNS.md; then
    pass "LOOP_PATTERNS: documents the ENTRYPOINT.md recovery pointer"
else
    fail "LOOP_PATTERNS: missing ENTRYPOINT.md recovery pointer"
fi

# META must list the new subcommands so agents discover them.
if grep -q "guard" META.md && grep -q "checkpoint" META.md; then
    pass "META: lists guard + checkpoint subcommands"
else
    fail "META: missing guard/checkpoint in binary table"
fi

# CLAUDE.md doctrine must enforce stop-on-API-error.
if grep -qiE "stop-on-API-error|usage-policy|One API error = stop" CLAUDE.md; then
    pass "CLAUDE.md: documents stop-on-API-error doctrine"
else
    fail "CLAUDE.md: missing stop-on-API-error doctrine"
fi

# The binary itself must expose guard + checkpoint (source-level check —
# no build needed: the Subcommand enum is the contract).
if grep -q "Guard {" crates/claude-loop/src/main.rs && \
   grep -q "Checkpoint {" crates/claude-loop/src/main.rs; then
    pass "claude-loop: Guard + Checkpoint subcommands present in source"
else
    fail "claude-loop: missing Guard/Checkpoint subcommands"
fi

# Offline kill-switch: watchdog subcommand + DISABLED handling + wrapper.
if grep -q "Watchdog {" crates/claude-loop/src/main.rs && \
   grep -q "fn cmd_watchdog" crates/claude-loop/src/main.rs; then
    pass "claude-loop: Watchdog subcommand present in source"
else
    fail "claude-loop: missing Watchdog subcommand"
fi
if grep -q "disabled_path" crates/claude-loop/src/main.rs; then
    pass "claude-loop: guard honors the DISABLED kill-switch sentinel"
else
    fail "claude-loop: missing DISABLED sentinel handling"
fi
if [ -f lib/claude-loop-fire.sh ] && sh -n lib/claude-loop-fire.sh; then
    pass "lib/claude-loop-fire.sh: present + parses (OS-cron local pre-fire gate)"
else
    fail "lib/claude-loop-fire.sh: missing or syntax-broken"
fi
if grep -q "claude-loop watchdog" docs/LOOP_PATTERNS.md; then
    pass "LOOP_PATTERNS: documents the offline watchdog kill-switch"
else
    fail "LOOP_PATTERNS: missing the watchdog/offline kill-switch"
fi

echo ""
echo "summary: $PASS pass, $FAIL fail"
[ "$FAIL" -eq 0 ]
