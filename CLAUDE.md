# CLAUDE.md — instructions for AI agents participating in a /loop

If you are an AI agent (Claude or otherwise) and you receive a message
labeled `THIS IS A SCHEDULED LOOP` (or any cron-injected prompt from
this tooling), **read this file before acting on it.**

The friction this file exists to prevent: cron fires every N minutes
and injects the same prompt. If you reflexively start a new line of
work on every fire, you drop whatever you were mid-doing, you
duplicate effort across iterations, and the loop log fills with
half-finished threads.

## The single rule

> **A loop fire is a SIGNAL to continue, not a command to start
> something new.** If you're mid-task when the loop fires, finish
> that task first. The loop will fire again in N minutes; the work
> waits.

## Steps when a loop prompt fires

1. **Check the in-flight task list FIRST**, before doing anything
   else. Use `TaskList` if you're in an environment that has it,
   otherwise read your prior turn's notes.
   - If a task is `in_progress`, continue it.
   - If multiple tasks are `pending` and not blocked, work the
     lowest-ID one.
   - Only start a new task if the in-flight list is empty.

2. **Don't restart finished work.** If your last turn ended with
   "iter27 logged. system stable.", the next loop fire does NOT mean
   re-do iter27. Pick up where you left off — usually that's "do
   the next thing on the list, or report steady state."

3. **The loop prompt is verbose by design.** It's the same text
   every fire because cron has no per-fire state. Don't treat each
   re-read as a new directive from the user. Treat repeated phrases
   ("Finish tasks, run CI...") as a standing posture, not a fresh
   instruction.

4. **The canary line in the prompt** ("if you canceled or stopped
   the loop, you should NOT be seeing this message") is for YOU to
   self-check: if you're seeing the message, you haven't stopped.
   Don't echo the canary back at the user.

## Save state on EVERY substantive fire — assume compaction is imminent

Compaction can land between this fire and the next. The next-you
reads memory + checkpoint + task list, not chat. Before ending each
substantive fire, snapshot:

- `TaskList` (TaskCreate / TaskUpdate) — durable across compaction
- `~/.claude/projects/<slug>/memory/*.md` — durable across sessions
- `~/.claude/.checkpoint/handoff.md` — read first by `/restore`
- An append to `~/.claude/loop-history.jsonl` describing what changed

Rule of thumb: if a future-you in a fresh context would need to know
what this fire did, write it to disk before the fire ends. Don't
trust chat history alone.

Tight-tick fires (no state change) skip the *narrative* handoff — but
see the crash-guard rule below: when a loop is armed with
`claude-loop guard`, you still close every fire with a `checkpoint`
call (it's cheap) so the start/done journal stays balanced.

## Crash-guard + checkpoint every fire (stop-on-API-error)

If this loop was prepped with `claude-loop prep --label <L>` (the
recommended setup), two calls are MANDATORY every fire — they are how
the loop survives crashes and an API error doesn't turn into an
infinite re-fire:

1. **STEP 0, before anything**: `claude-loop guard --label <L>`.
   - `{"action":"continue"}` (exit 0) → proceed.
   - `{"action":"halt"}` (exit 3) → the PREVIOUS fire died mid-flight
     (almost always an API error). **CronDelete this job and STOP.**
     Do not iterate into the same failure.
2. **LAST STEP, every fire even idle ticks**: pipe a one-line state
   summary into `claude-loop checkpoint --label <L> --note "…"`. This
   writes an additive, never-overwritten `checkpoint-<stamp>.md`,
   closes the iteration in the journal, and refreshes the canonical
   `~/.claude/.loop-checkpoints/ENTRYPOINT.md` recovery pointer.

**Hard stop on ANY API error this fire — especially a usage-policy /
cybersecurity-classifier block.** The instant you hit one, STOP:
CronDelete the job and do not retry. Re-firing a blocked loop
compounds the block and prevents the user from working. One API error
= stop the loop, full stop. Re-arm later (after fixing the cause) with
`claude-loop guard --label <L> --reset`.

On resume / recovery, read `~/.claude/.loop-checkpoints/ENTRYPOINT.md`
FIRST — it names the loop's current status and the latest checkpoint.

## Maintaining the task list

Use `TaskCreate` / `TaskUpdate` / `TaskList` (or equivalent) to
track open work explicitly. The task list is the durable state
that survives between fires; the loop prompt itself does not carry
state.

Recommended task hygiene:

- **Each substantive piece of work = one task.** "Build vaultwarden
  setup" is a task. "Tick the loop log" is not — that's continuous.
- **Mark `in_progress` BEFORE starting work**, `completed` only
  when truly done (not when paused or blocked).
- **If you discover blockers, create them as new tasks** with
  `addBlockedBy` so the dependency is visible.
- **Don't let tasks accumulate.** When a task is no longer relevant,
  delete it. The list should reflect actual current work.

## Tight-tick vs substantive iters

Loops on short cadences (every minute) often spend most fires in
"nothing new to do" state. Don't fight this — embrace it.

**For steady-state ticks**: do a one-line health check and stop.
Don't append redundant log lines. The log should only grow when
something *changed*.

**For substantive iters**: when there IS work, do it fully. Don't
fragment a 5-minute task across 5 separate iterations just because
the cron fired five times.

## Mid-iter user messages

The user may send a message while you're mid-iter. The system will
inject it as a `<system-reminder>` with the new message.

When this happens:

1. **Finish your current tool call**, then address the user.
2. **Don't switch contexts mid-tool-call** even if the message looks
   urgent — finish what you started, ack what they sent, then act.
3. **If their message creates a new task**, create it via TaskCreate
   so it's tracked. Don't let it sink into chat history.

## Pausing / resuming the loop

If the user wants to stop temporarily:

```
/loop-pause                              # pause all active cron jobs
                                    # state → ~/.claude/.paused-loops.json
                                    # cron entry deleted (session-scoped)
```

State is preserved. The user can edit the JSON to change interval
or prompt, then `/loop-resume` (or `/loop-resume 5m` to override interval).

If the user wants to stop permanently: `CronDelete <id>` directly.
Don't leave dangling cron jobs.

## Don't

- **Don't echo the verbose loop prompt back.** The user can see it.
  Repeating it back wastes tokens and pollutes the transcript.
- **Don't speculate about why the user set the loop up this way.**
  Trust the directive; do the work.
- **Don't start unrelated work because the loop says "do whatever
  you can".** Stay in the scope the loop prompt defines.
- **Don't burn iterations on cosmetic improvements** when there's
  open in-flight or pending substantive work.

## A good loop session looks like

```
iter12 — substantive: closed CrowdSec acquisition gap (4 silently-
inactive sources). Logged. Tests passing.

iter13 — tick. State unchanged. Skipped log entry.

iter14 — substantive: noticed NTP not synced. Installed chrony.
Logged.

iter15-30 — most ticks, two substantive (limits.conf, AIDE
exclusions). Only changed-state logged.

iter31+ — tight ticks, paul-todo accumulating, awaiting user.
```

Not:

```
iter12 — let me redo the full system audit just in case
iter13 — let me write a 500-line doctrine update
iter14 — let me ignore the existing task list and start fresh
```

## See also

- [`docs/SHARING_PROTOCOL.md`](./docs/SHARING_PROTOCOL.md) — the
  parallel doc for `claude-secrets`, applies the same "minimize
  echo, maximize tracking" pattern to credential handling.
- The `/loops` skill: shows active + paused + last 20 history events.
  Run it first when entering a new loop session to see where you are.
