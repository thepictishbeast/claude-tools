# Loop patterns — stop conditions, adaptive cadence, general-purpose loop design

A reference catalogue of reusable patterns for the loops `/loop`
+ this toolkit produce. Covers **stop conditions** (when to end
a loop automatically) and **interval strategies** (fixed,
dynamic, adaptive). Use this to compose loops that fit your work
shape rather than retyping a verbose prompt every time.

The toolkit is intentionally low-level — `/loop` creates cron
entries, `/loop-*` skills manage their lifecycle. The PATTERNS
on top of these primitives are what turn a cron job into a
*useful* loop. This doc enumerates the patterns; the skills are
unchanged.

---

## Part 0 — The non-interruption rule (load-bearing prerequisite)

> **A loop fire is a SIGNAL to continue, not a command to start
> something new. Finish what you're doing, then address the
> loop.**

Every other pattern in this doc assumes this is in place.
Without it, no stop condition fires correctly (work is always
"in progress" because nothing finishes), and no interval
strategy holds (the agent constantly drops mid-task work to
re-react to the re-injected prompt).

What this means in practice:

| Scenario | Right behavior |
|---|---|
| Loop fires while agent is editing a file | Finish the edit, save, commit — THEN read the new prompt. |
| Loop fires while agent is running tests | Wait for the test results. The 1-min cron fires again; the test takes 90s; the second fire arrives and tests are still running; finish them, commit, then handle the prompt. |
| Loop fires with a new explicit user message attached | Inject the message as a `TaskCreate` (don't context-switch). Finish current work. Address the new task when it surfaces via TaskList ordering. |
| Loop fires and `TaskList` shows an `in_progress` task | Continue that task. The loop's "TASK PRIORITY" ordering applies only when *nothing* is in flight. |
| Loop fires and there's a half-written commit | Finish + commit + push. The loop's stop conditions can't fire correctly with uncommitted work in the tree. |
| Loop fires and `TaskList` is empty | THIS is when the loop's priority list takes effect. Pick the next task per the prompt's ordering. |

Why this matters: cron has no concept of "is the agent busy?"
It fires on a schedule regardless. The agent's job is to treat
the fire as a *reminder to keep working*, not a *fresh
instruction*. Without this discipline:

- Commits get half-staged, half-pushed, leaving repos in
  inconsistent states across iterations
- Test runs get interrupted, producing false-negative CI runs
- Tasks accumulate as the agent restarts the same work each
  minute
- The transcript fills with "let me start X" / "actually let me
  start Y" / "wait, the loop said do Z" without anything
  actually shipping

The skills support this: `/loop-edit` doesn't fire the prompt
immediately (per its spec), `/loop-pause` preserves state, and
the prompt convention says "SELF-CHECKS FIRST" to nudge agents
toward task-list-based continuity rather than reflexive
re-acting.

If you're the operator writing a loop prompt: lead with
SELF-CHECKS that start with `TaskList`. If you're the agent
participating in a loop: when a fire arrives, read `TaskList`
before reading the prompt body. This single habit accounts for
most of the value the toolkit provides.

See also: [`CLAUDE.md`](../CLAUDE.md) at the repo root — the
canonical contract for agents participating in any loop
produced by this toolkit. The README's "non-interruption rule"
section is the executive summary; CLAUDE.md is the full
treatment.

---

## Part 1 — Stop conditions

Cron jobs in this toolkit auto-expire after 7 days. Until then
they fire forever unless told to stop. Three ways to stop:

1. **Manual** — `/loop-stop <id>` or `/loop-stop all`.
2. **Self-cancel from inside the loop** — the agent decides to
   stop and calls `CronDelete <this-job-id>` mid-iteration.
3. **External trigger** — a separate process detects the stop
   condition and runs `/loop-stop` for you.

**Self-cancel is the right answer most of the time.** The loop
prompt embeds the stop condition; the agent runs the check on
every fire; when the condition fires, the agent cancels itself.

Stop-condition patterns:

### 1.1 Empty task list

> "Stop when there's no work left."

```
SELF-CHECK FIRST: TaskList. If zero pending AND zero in_progress,
email me a status report at <addr> then CronDelete <this-id>.
```

The agent calls TaskList every fire; when it's empty, the loop
ends. Most natural fit for "do all the things on my list and
then stop."

### 1.2 Max iterations cap

> "Stop after N iterations even if there's more work."

```
SELF-CHECK FIRST: read ~/.claude/loop-iters.txt (default 0).
Increment. Write back. If > 100, CronDelete <this-id> + report.
```

Useful as a safety net — caps runaway loops at a documented
ceiling. Pair with another stop condition; don't rely on this
alone as the primary stop.

### 1.3 Error budget exhausted

> "Stop after the build fails N times in a row."

```
SELF-CHECK FIRST: read ~/.claude/loop-failures.txt (default 0).
If last build/CI run failed → increment + write. If passed →
reset to 0. If counter > 5, CronDelete <this-id> + email me the
last failure log.
```

Mirrors error-budget SLO discipline at the loop layer. Three
consecutive red CI runs probably means something is structurally
broken and you want the loop to STOP making it worse, not keep
trying the same thing.

### 1.4 Deadline / time budget

> "Stop at midnight Friday" or "stop after running for 8 hours."

```
SELF-CHECK FIRST: read ~/.claude/loop-started.txt (write on iter
1). If now - started > 8h, CronDelete <this-id>. If now > <ISO
deadline>, CronDelete <this-id>.
```

Useful for compute-budget caps + "I want this done before the
weekend" deadlines. The bundled `/schedule` skill is the better
fit for time-pinned recurring work — this pattern is for
"budget runs out X hours from now."

### 1.5 Success condition met

> "Stop when the test suite is green AND the docs site rebuilds
> cleanly AND CI is fully green."

```
SELF-CHECK FIRST: run the success check (composite bash
command). If exit 0, CronDelete <this-id> + email success
report. If exit non-zero, continue iteration.
```

Useful for "drive until done" loops where you have a clear
binary success condition. The agent runs the check every fire;
when it passes, you're done.

### 1.6 Drift / soft success

> "Stop when nothing has changed for N iterations."

```
SELF-CHECK FIRST: hash the working state (`git rev-parse HEAD`
of relevant repos + count of pending TaskList items). Compare to
last fire's hash in ~/.claude/loop-state-hash.txt. If unchanged
for 5 consecutive fires, CronDelete <this-id>.
```

Use when the loop is supposed to drive change and the absence
of change means it's stuck or done. "Nothing changed in 5
ticks" usually means either everything's done or the work is
blocked — either way, useful to stop.

### 1.7 External signal

> "Stop when this file exists" or "stop when this URL returns 200."

```
SELF-CHECK FIRST: if test -f /tmp/STOP-MY-LOOP; then CronDelete
<this-id> + remove the file + exit. Otherwise continue.
```

Lets external systems trigger a stop without needing to find +
delete the cron entry. Useful when the operator wants to stop
the loop from outside the agent's session (CI run finished,
external job kicked off, etc).

### Combining stop conditions

Most real loops want multiple stop conditions composed with
**OR** semantics — any one fires, the loop ends:

```
SELF-CHECK FIRST (run every iteration before new work):
- TaskList: if empty → stop
- read ~/.claude/loop-failures.txt: if > 5 → stop + email
- now > 2026-06-01T00:00:00Z deadline → stop
- /tmp/STOP-MY-LOOP exists → stop + remove file
```

Document each composed condition explicitly in the prompt.
"Implicit" stop conditions get forgotten and cause loops that
should have stopped to keep running.

---

## Part 2 — Interval strategies

Three architectures:

### 2.1 Fixed cron

> "Every N minutes/hours/days."

The default `/loop` behavior. `/loop 5m <prompt>` schedules an
entry that fires every 5 minutes. Pros: predictable, simple,
external observers can see the cadence. Cons: doesn't adapt to
work shape — bursts of activity get throttled, idle periods
waste fires.

Use when: the work shape is uniform OR you specifically want
external predictability (status-page polling, periodic backup
trigger, etc).

### 2.2 Dynamic mode (`/loop` without interval token)

> "I'll decide how long to wait between fires myself."

`/loop <prompt>` (no interval) puts the bundled `/loop` skill in
**dynamic mode**: the agent runs the prompt once, then calls
`ScheduleWakeup` with a self-chosen delay. Next fire re-enters
the skill, re-runs the prompt, re-schedules.

Pros: agent adapts cadence to observed state. Run fast while
there's work; back off while idle.

Cons: agent has to decide the delay every fire, which is one
more decision per iteration.

Recommended delay-selection rules (from the bundled skill
description):

- **60s–270s** when actively polling external state that isn't
  notify-tracked (CI run, deploy queue). Cache stays warm.
- **1200s–1800s** (20-30 min) for genuine idle ticks. The
  Anthropic prompt cache TTL is 5 minutes; sleeping past 300s
  pays a cache miss. Don't pick 300s — pay the miss once for
  many minutes of saved compute, or stay under 270s.
- Don't pick round-number minutes if the user's request is
  approximate. Off-:00-mark minimizes herd cost.

Use when: work shape is bursty + you want efficiency over
predictability.

### 2.3 Adaptive cron (fixed cadence + agent self-reschedule)

> "Start at 1 min; back off to 15 min after N idle iterations;
> snap back to 1 min when work appears."

A hybrid: cron-scheduled at a fast cadence, but the agent calls
`/loop-edit Nm` from inside the loop to retune the cadence
based on observed state.

```
SELF-CHECK FIRST:
- TaskList. If empty → /loop-edit 30m (back off — nothing to do).
- TaskList has work → /loop-edit 1m (catch up — full speed).
- CI red → /loop-edit 5m (steady pace while diagnosing).
```

The agent doesn't need to know its own job ID — `/loop-edit`
operates on the unique active loop (or asks for disambiguation
if multiple).

Pros: cron's predictability + dynamic-mode's adaptivity. State
adapts the cadence; the cadence is always visible externally.

Cons: more complex prompt; agent has to remember to call edit.

Use when: long-running loops where work density varies a lot
over the loop's lifetime (e.g. "build platform, then maintain
it" — front-loaded with work, back-loaded with idle).

### 2.4 Exponential backoff on idle

> "Slow down monotonically the longer there's nothing to do."

```
SELF-CHECK FIRST:
- TaskList. If empty AND last iter was idle → /loop-edit 2× current cadence.
- Cap at e.g. 1h.
- Reset to 1m when work appears.
```

Pattern from networking + retry logic, applied to loops. Idle
fires get cheaper over time; once work appears, snap back to
fast.

Use when: the loop is mostly a watcher (rarely needs to act).
Don't use when bursty fast response matters more than efficiency.

### 2.5 Time-windowed cadence

> "Fast during work hours, slow at night."

```
SELF-CHECK FIRST:
- read current hour. If 9-17 local → /loop-edit 1m.
- Otherwise → /loop-edit 30m.
- Skip the edit if cadence already matches the target.
```

Useful when the agent is collaborating with a human who wants
fast turnaround during their day but doesn't want logs piling
up overnight.

### Switching between strategies mid-flight

`/loop-edit Nm` accepts any new cadence — switching from 1m to
hourly is one command. `/loop-pause` + edit `~/.claude/.paused-
loops.json` → `/loop-resume` for more complex transitions.

The toolkit doesn't enforce a single strategy; pick the one
that fits your work shape now, change it later when it
doesn't.

---

## Part 3 — General-purpose loop design checklist

Before scheduling a loop, decide each of the following
explicitly:

| Decision | Options |
|---|---|
| **Cadence shape** | Fixed cron / dynamic / adaptive / exponential / time-windowed |
| **Initial interval** | 30s / 1m / 5m / 30m / 1h / etc |
| **Stop conditions** | One or more from §1 |
| **Stop action** | `CronDelete <this-id>` from inside the loop, plus optional email/notification |
| **Self-check sequence** | What to verify before each iteration's work (task list, git state, CI health, fmt+tests, memory) |
| **Scope** | Which repos / capabilities / file paths the loop is allowed to touch |
| **Reporting** | When the loop sends mail / writes a log / pings a status page |
| **Recovery** | What happens if the agent dies mid-iter (the cron fires again; durable state must absorb the gap) |

A loop prompt with all of these specified explicitly tends to
behave predictably across sessions. A loop prompt that elides
any of them tends to drift or restart work it has already done.

### Prompt template — fully-specified loop

```
LOOP <name> (every <interval> per <owner> <date>). Scope: <repo list>.

SELF-CHECKS FIRST (run every iteration before new work):
1. TaskList — any in_progress? Continue it.
2. Git cleanliness — check each repo in scope.
3. CI health — gh run list for each repo.
4. fmt + tests on the recent repo.
5. Memory — read new memory files.

STOP CONDITIONS (any fires → end loop):
- TaskList empty → CronDelete <this-id> + email <addr>.
- Failures > 5 → CronDelete <this-id> + email failure log.
- After 2026-06-01 → CronDelete <this-id>.

EXECUTION DISCIPLINE:
- One focused increment per iteration.
- Standard commit / push / verify CI sequence.
- Every commit carries docs + tests + audits inline.

TASK PRIORITY:
- <ordered list>

End each iteration with a one-line status summary.
```

The bundled `/loop` skill + the `/loop-*` skills in this toolkit
take care of scheduling + lifecycle; the **prompt** is where
you express the policy. Treat the prompt as a small program;
write it the way you'd write any other small program — with
explicit pre-conditions, clear stop semantics, and named
collaborators.

---

## Crash-guard + per-iteration checkpointing (stop-on-API-error)

A long-running loop has two failure modes that the bundled `/loop`
primitive does not handle:

1. **A fire dies mid-iteration** (an API error — most commonly a
   usage-policy / cybersecurity-classifier block). The cron keeps
   firing on schedule, so the loop *re-enters the same failure every
   interval*, which compounds the block and starves the user.
2. **State is lost across fires** — compaction or a CLI restart wipes
   the in-session TaskList, and there's no durable per-iteration trail.

`claude-loop` solves both with a per-label append-only status journal
of `start` / `done` / `halt` events, plus an additive checkpoint per
fire. Because crons only fire while the REPL is idle, a new fire can't
begin while the previous one is still running — so a `start` with no
matching `done` can *only* mean the previous fire died. That unmatched
`start` is the stop signal.

```
# STEP 0 of every fire — crash-guard:
claude-loop guard --label <L>
#   → {"action":"continue","iter":N}     exit 0  → proceed
#   → {"action":"halt", ...}             exit 3  → CronDelete this job + STOP
# (deliberately re-arm a halted loop with:  claude-loop guard --label <L> --reset)

# LAST STEP of every fire (even idle ticks) — additive save-state:
printf '%s' "$STATE_SUMMARY" | claude-loop checkpoint --label <L> --note "what changed"
#   → writes ~/.claude/.loop-checkpoints/<L>/checkpoint-<UTCstamp>.md  (NEVER overwritten)
#   → closes the open iteration, appends INDEX.md, refreshes ENTRYPOINT.md
```

Wire it in automatically with `prep --label`, which injects both
instructions (and an explicit "hard-stop on any API error, especially
a usage-policy block" rule) ahead of the loop prompt:

```sh
claude-loop prep --label <L> --prompt "$YOUR_LOOP_PROMPT" | <feed to CronCreate>
```

**Recovery entrypoint.** `~/.claude/.loop-checkpoints/ENTRYPOINT.md`
is the single canonical "where to look" pointer, rewritten each fire.
A fresh or recovered session reads it first: it names the loop's
current status (running / idle / **HALTED**), iteration number, and
the path to the latest checkpoint + status journal. If status is
HALTED, the loop stopped on a crash — read the latest checkpoint, fix
the cause, then re-arm with `--reset`.

This pattern composes with the stop conditions in §1: the crash-guard
is an *involuntary* stop (the loop died), while §1's conditions are
*voluntary* (the work is done). A robust loop uses both.

### Stopping a loop when the agent is FULLY blocked (offline kill-switch)

The `guard`-at-STEP-0 above runs *inside the agent's fire*. That is enough
for a crash that kills one fire but leaves the agent able to run later. It is
NOT enough for a **total** API block (e.g. a usage-policy / cybersecurity
classifier that flags the whole context): the agent can't run `guard`,
`CronDelete`, or anything — so an in-memory cron just keeps firing into the
block and compounds it. The agent cannot save itself.

The fix is a kill-switch that runs **outside** the agent:

- **`claude-loop watchdog --label <L> --max-age-secs N`** — run from an OS
  timer (cron/systemd), NOT the agent. Uses NO API. If the journal's last
  event is an unmatched `start` older than N (a fire that began but never
  completed — the blocked-agent signature), it writes a HALT event + a
  `DISABLED` sentinel and exits 3. Set N well above a normal fire (e.g. 1200).
- **`guard` honors `DISABLED`** — once set, every `guard` returns halt until
  `guard --reset` clears it. So a disabled loop never auto-resumes.
- **`lib/claude-loop-fire.sh <label> <prompt-file>`** — the missing piece for
  *true* offline enforcement: drive the loop from an OS timer through this
  wrapper instead of an in-memory `CronCreate`. It runs `guard` LOCALLY (no
  API) and only invokes `claude -p` if guard passes. A DISABLED loop is
  short-circuited before any API call, so a blocked loop physically stops
  making calls — even with no human and no working agent.

Recommended wiring (OS crontab):

```
*/30 * * * *  /path/claude-loop-fire.sh myloop /path/prompt.txt   # fire (gated locally)
*/10 * * * *  claude-loop watchdog --label myloop --max-age-secs 1200  # kill-switch
```

In the wrapper model the WRAPPER runs `guard`, so build the fire prompt with
`prep` *without* `--label` (the prompt should only do work + end with
`checkpoint`); otherwise the agent would double-count the `start`.

In-memory `CronCreate` loops can still use the in-fire `guard` + the watchdog
(which disables on stuck + alerts), but cannot be physically stopped while the
agent is blocked — only the OS-cron-wrapper model gives that. Choose the
wrapper model when unattended/offline resilience matters.

---

## See also

- `CLAUDE.md` — agent-side instructions for participating in a
  loop (what to do when a loop fire injects a prompt).
- `skills/loop-pause/SKILL.md` + `loop-resume/`, `loop-edit/`,
  `loop-stop/`, `loops/` — the individual skill specs.
- `skills/checkpoint/` + `restore/` — session-scoped state
  preservation across `/exit`s.
- Anthropic's bundled `/loop` skill (in Claude Code itself) —
  the primitive this toolkit extends.

This doc is general-purpose: nothing in it is specific to any
one project. If you find your loop prompt accumulating
project-specific scaffolding, factor the scaffolding into a
project-specific skill and keep the prompt itself describing
*what* the loop does, not *how* the project's tooling works.
