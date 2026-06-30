# claude-tools

[![test](https://github.com/thepictishbeast/claude-tools/actions/workflows/test.yml/badge.svg)](https://github.com/thepictishbeast/claude-tools/actions/workflows/test.yml)

> **RENAMED 2026-05-20**: this repo was `claude-loop-tools`. GitHub's
> rename redirect keeps the old URL working for clones, but new
> clones should use:
> ```sh
> git clone https://github.com/thepictishbeast/claude-tools
> ```
> Existing clones: `git remote set-url origin https://github.com/thepictishbeast/claude-tools.git`
>
> **Scope expanded**: this repo is now the **umbrella for all Claude
> Code tooling** (skills, MCPs, helper binaries) — not just `/loop-*`.
> Run `/loop-update` (or `update.sh` directly) regularly to pull
> the latest set into `~/.claude/skills/`. See [CONTRIBUTING.md](
> ./CONTRIBUTING.md) for the "add a new tool" workflow (Rust first,
> token-efficient, automates a frequent task).
>
> **AI agents (Claude / Codex / Cursor / etc.) — read [`META.md`](./META.md)
> first.** Single-page handoff: what this repo is, install/sync, current
> tools, the 4 design rules, the 3-step add-a-tool recipe. ~5 min read,
> answers every question before you start.

Pause, resume, edit, and audit Claude Code cron jobs (e.g. `/loop`).

Claude Code ships with `/loop` but no way to pause a long-running
loop without losing it, change its interval without re-typing the
prompt, audit what's been scheduled over time, or compose loops with
TaskList / Monitor, plus first-class skills for the agent fleet and the
Forge substrate. This adds these skills:

- **`/loop <verb>`** — manage active loops. Consolidated 2026-06-30 from
  the former loop-pause / loop-resume / loop-edit / loop-stop / loop-track /
  loop-update skills into one verb-routed command, backed by the
  `claude-loop` binary. Verbs:
  - `pause` — pause all active cron jobs (state → `~/.claude/.paused-loops.json`, cron deleted, nothing lost).
  - `resume [5m]` — restore the paused jobs, optional inline interval override.
  - `edit` — change interval/prompt of an already-running loop without pause+resume.
  - `stop` — permanently cancel a loop (deletes cron AND clears paused state).
  - `track <id>` — register a raw cron into loop history.
  - `update` — pull latest claude-tools + reinstall.
- **`/loops`** — show a unified view of active + paused + recent
  history (last 20 events). Auto-discovers untracked crons (raw
  `/loop` invocations not yet in history) as a side effect.
- **`/fleet`** — orchestrate the agent fleet (Conductor + worker swarm).
  Defers heavy ops to the `fleet` MCP server (validate-plan / run / gate /
  status / per-worker introspection / global STOP).
- **`/forge`** — build / audit / reason about the Forge + Loom + Crawler
  substrate. Defers to the `forge` MCP server (~21 typed tools).
- **`/loop-from-task`** — wrap a TaskList task as a self-terminating
  loop. Fires periodically, works on the task, `/loop-stop`s itself
  when the task is marked completed.
- **`/loop-on`** — event-driven loop using `Monitor` instead of cron.
  Watches conditions (`pr-merged:`, `ci-status:`, `task-completed:`,
  `port-open:`, custom `watch-cmd:`); fires the `then:` prompt when
  the condition triggers.
- **`/loop-health`** — single-shot lint diagnostic. Reports missing
  canaries, oversized prompts, stale paused entries, untracked crons,
  history-rotation candidates, stale skill-rename leftovers.
- **`/checkpoint`** — save FULL session state (tasks + active loops
  + background processes + dirty git trees + handoff note) to
  `~/.claude/.checkpoint/`. Use before you `/exit` so you can pick up
  exactly where you left off.
- **`/restore`** — first command of a new session. Auto-updates the
  toolkit from upstream, then reads the `~/.claude/.checkpoint/`
  state, re-creates the TaskList, resumes paused loops, shows the
  handoff note, then deletes the checkpoint.

History is appended to `~/.claude/loop-history.jsonl` on every
pause/resume/edit/stop — append-only, one JSON line per event.

## The non-interruption rule (please read first)

> **A loop fire is a SIGNAL to continue, not a command to start
> something new.** If your agent is mid-task when the loop fires,
> finish that task first. The loop will fire again in N minutes;
> the work waits.

This is the single load-bearing discipline that makes loops
useful instead of disruptive. Without it, agents drop mid-work
to "act on" the re-injected prompt, duplicate effort across
iterations, and ship half-finished commits.

Concrete rules:

1. **Loop fires don't interrupt current tool calls.** Finish
   the call, *then* read the prompt.
2. **In-flight tasks have priority over the loop prompt.** If
   `TaskList` shows an `in_progress` task, continue it. The
   prompt's TASK PRIORITY list applies only when nothing is
   in-flight.
3. **Mid-iter user messages are tracked, not acted on
   immediately.** The user typing while the agent is editing a
   file should produce a new `TaskCreate`, not a context
   switch.
4. **Re-injection is not a new directive.** The cron-injected
   prompt is verbose by design. The same text every fire means
   "standing posture" not "fresh instruction."
5. **Tight ticks → one-line health check, no log entry unless
   state changed.** Idle fires that produce no work should
   leave no trace.

[`CLAUDE.md`](./CLAUDE.md) is the canonical contract — every
loop participant (AI agent or human operator) reads it before
starting work. The rules above are the executive summary.

## Patterns (general-purpose loop design)

See [`docs/LOOP_PATTERNS.md`](./docs/LOOP_PATTERNS.md) for the
catalogue covering:

- **Stop conditions** — empty task list / max iterations / error
  budget exhausted / deadline / success condition met / drift
  detection / external signal / composed-with-OR semantics.
- **Interval strategies** — fixed cron / dynamic mode (no
  interval token, self-paced via `ScheduleWakeup`) / adaptive
  cron (agent retunes its own cadence via `/loop-edit` based on
  observed state) / exponential backoff on idle / time-windowed
  (work-hours fast, night slow).
- **General-purpose loop design checklist + prompt template** —
  every decision (cadence shape / interval / stop conditions /
  scope / reporting / recovery) explicit before scheduling.

This toolkit deliberately keeps the skills low-level (`/loop`
schedules, `/loop-edit` retunes, `/loop-stop` cancels). The
PATTERNS doc shows how to compose them into loops that fit
arbitrary work shapes — bursty / steady / decreasing /
event-driven — without retyping verbose prompts.

## Install

```sh
git clone https://github.com/thepictishbeast/claude-tools
cd claude-tools
mkdir -p ~/.claude/skills
cp -r skills/* ~/.claude/skills/
```

Restart Claude Code (or open a new session) so the skills are
discovered.

## Usage

```
/loop 1m do-some-recurring-task         # start a loop
…
/loops                                  # what's active/paused/recent

# Modify in place (no need for pause+resume cycle):
/loop-edit 5m                           # change interval, keep prompt
/loop-edit prompt: "new task text"      # change prompt, keep interval
/loop-edit 5m prompt: "..."             # change both
/loop-edit prompt-append: "and CC me"   # tack onto existing prompt

# Pause/resume cycle (state preserved between):
/loop-pause                             # state saved, loop cancelled
/loop-resume                            # restore exactly as paused
/loop-resume 5m                         # restore with new cadence
$EDITOR ~/.claude/.paused-loops.json    # hand-edit anything before resume

# Permanent stop:
/loop-stop                              # delete cron + clear state

# Full session restart (paul wants to relaunch Claude Code):
/checkpoint                             # save tasks, loops, bg procs, notes
# then /exit + relaunch Claude Code (use --continue for chat history)
/restore                                # first command in new session
```

## Editing the prompt or cron of a paused loop

The state file is a plain JSON array; edit with any text editor:

```sh
$EDITOR ~/.claude/.paused-loops.json
```

Each entry has:

| field           | purpose                                              |
|-----------------|------------------------------------------------------|
| `cron`          | cron expression (e.g. `*/5 * * * *`)                 |
| `cadence_human` | human-readable cadence (informational only)          |
| `recurring`     | boolean — does the cron auto-renew?                  |
| `prompt`        | the verbatim message Claude receives each fire       |
| `canary_added`  | whether `/loop-pause` auto-added a canary line            |
| `paused_at`     | ISO-8601 UTC timestamp                               |
| `label`         | optional short label (informational only)            |
| `id_original`   | the original job ID from before pause (informational)|

Save and `/loop-resume` — the new values take effect.

## The canary

`/loop-pause` auto-appends a self-check sentence to the prompt if one is
not present:

> Note: if you canceled or stopped this loop, you should NOT be
> seeing this message.

This catches the bug where the cron keeps firing after the loop
logic has reached its "done" condition — the canary in the message
tells the agent to stop and clean up.

Disable canary auto-add by editing the skill's SKILL.md (find the
"Auto-canary check" section and remove it), or set the saved entry's
`canary_added` field to `false` and remove the sentence by hand from
`prompt`.

## State files

| File                                | Mode | Purpose                          |
|-------------------------------------|------|----------------------------------|
| `~/.claude/.paused-loops.json`      | 600  | Currently-paused loops           |
| `~/.claude/loop-history.jsonl`      | 600  | Append-only audit history        |

The mode-600 default is because prompts may contain sensitive
context. The skills `chmod 600` after writing.

## Design constraints

- Cron entries created with `CronCreate` are **session-scoped** —
  they die when the Claude Code session ends. The state file
  persists, so `/loop-resume` works even across sessions.
- `/loop` dynamic mode (no interval, uses `ScheduleWakeup`) isn't
  cron-backed and isn't visible to these tools. Pausing a
  dynamic-mode loop is "stop replying" — which happens automatically
  when the user doesn't interact.
- These skills don't try to deduplicate or validate cron logic.
  They're a thin layer over `CronCreate` / `CronDelete` / `CronList`.

## Loop hygiene (for AI agents)

When `/loop` fires every minute, the same prompt is re-injected each
time. Without explicit hygiene, agents tend to:

- Restart finished work every iteration
- Drop mid-task work to "act on" the re-injected prompt
- Lose track of what's in-flight vs. queued vs. done
- Bloat the iteration log with duplicate state

**[`CLAUDE.md`](./CLAUDE.md) is the contract every loop participant
reads first.** Key rules it codifies:

1. A loop fire is a SIGNAL to continue, not a command to start
   something new. Finish in-flight work first.
2. Maintain an explicit task list (`TaskCreate` / `TaskUpdate`).
   The task list is the durable state that survives between fires;
   the loop prompt itself doesn't carry state.
3. Mid-iter user messages get acked + tracked as tasks, not allowed
   to interrupt the current tool call.
4. Tight-tick fires should produce a one-line health check and stop —
   don't append redundant log entries for "still healthy".
5. Substantive fires do work fully — don't fragment a 5-minute task
   across 5 separate iterations.

Recommended workflow:

```
/loop 5m work on the tasks in my TaskList; create new ones for
follow-ups; log only when state changes
```

vs. the failure mode:

```
/loop 1m do everything you can       # too aggressive — every fire
                                       # tries to start fresh work
```

## License

MIT.
