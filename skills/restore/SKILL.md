---
name: restore
description: Re-hydrate a Claude Code session from a /checkpoint. Reads ~/.claude/.checkpoint/, recreates the TaskList, resumes paused loops via /loop-resume, prints the handoff note + dirty-git-tree summary. Use as the first command after relaunching Claude Code if you ran /checkpoint before exiting.
---

# /restore — re-hydrate session state from a /checkpoint

## When to run this

First command of a new Claude Code session, after the previous
session ended with `/checkpoint`. If there's no checkpoint on disk,
this skill tells you so and stops — no harm.

## Steps

State directory: `$HOME/.claude/.checkpoint/`.

### 0. Auto-update claude-tools (added 2026-05-17)

Before rehydrating session state, refresh the toolkit itself so this
session uses the latest skills. Look for a cloned `claude-tools`
repo at these paths in order:

```
$HOME/Development/claude-tools
$HOME/claude-tools
$HOME/projects/claude-tools
$HOME/git/claude-tools
$HOME/code/claude-tools
/tmp/claude-tools
```

If the first hit is under `/tmp`, treat it as WRONG-HOME: /tmp is
noexec + volatile on hardened hosts (`./update.sh` fails with
"permission denied" despite correct exec bits — run via `sh` and
re-clone to `~/Development/claude-tools`).

For the first hit (verify with `git remote get-url origin | grep
claude-tools`), if the working tree is clean:

```sh
cd "$REPO"
./update.sh --quiet -f
```

Behavior:
- If already up to date: silent, no output.
- If anything changed: show one-line "claude-tools updated:
  <N> commits, <M> skills" message. Continue with restore.
- If `update.sh` exits nonzero: log a one-line warning, continue
  restore with old version. Don't block rehydration on a network blip.
- If working tree is dirty: skip the update with a one-line note
  ("repo has local changes, skipped auto-update"). The user can
  manually clean up later.

After update succeeds, **note that newly-installed skills are NOT
loaded into THIS session** — Claude Code only loads skills at
session start. If the update brought in new skill names, the user
needs to relaunch Claude Code again to use them. Surface this
explicitly: "auto-updated; restart Claude Code to load new skills X, Y."

### 1. Verify a checkpoint exists

If `$HOME/.claude/.checkpoint/MANIFEST.json` is missing or
`.checkpoint/` directory is absent: tell user "no checkpoint to
restore." Stop.

### 2. Read the manifest

Parse it. Show the user a one-line summary of what's coming:

```
Restoring checkpoint from 2026-05-17T07:50:00Z:
  3 tasks · 1 paused loop · 1 dirty git tree
```

### 3. Show the handoff note

Display `$HOME/.claude/.checkpoint/handoff.md` verbatim to the user.
This is "what we were doing." Lets them sanity-check before
re-hydrating.

### 4. Re-create the tasks (rebuild the full UI task list)

`/clear` and exit wipe the in-flight TaskList from the UI. This step
puts it back — every task, with descriptions, status, AND the
blocked/blocked-by graph. Do it in **two passes** so dependencies
survive the ID renumbering.

Each `tasks.json` entry may carry: `id` (the OLD session id),
`subject`, `description`, `activeForm`, `status`, `blockedBy` (a list
of OLD ids). Resolve each task's body with a fallback chain:
`description` → else `note` (older checkpoints used `note`) → else
reuse `subject`. `TaskCreate` REQUIRES a non-empty description, so
never pass an empty one — the subject is always a valid last resort.

**Pass 1 — create + status.** Keep a map `oldId -> newId`. For each
entry: `TaskCreate { subject, description (resolved via the fallback
chain above), activeForm }`;
capture the new id from the result; record `map[entry.id] = newId`.
Then `TaskUpdate { taskId: newId, status }` to match the checkpoint
(`pending` / `in_progress` / `completed`).

**Pass 2 — re-link dependencies.** For each entry that has a
`blockedBy` list, translate every old id through the map and call
`TaskUpdate { taskId: map[entry.id], addBlockedBy: [map[o] for o in entry.blockedBy if o in map] }`.
Log (don't fail on) any old id missing from the map. This restores
the dependency graph, not just a flat list.

New ids will differ from the old session's — that's expected and
fine; the map keeps the graph intact.

### 5. Resume paused loops

Invoke the `/loop-resume` skill via the Skill tool. It reads
`.paused-loops.json` (which `/checkpoint` already populated via
`/loop-pause`) and CronCreates each.

### 6. Show background-process losses

Cat `$HOME/.claude/.checkpoint/processes.txt`. If non-empty: tell
the user which background processes were running at checkpoint time
that died on exit. Ask whether they want to re-kick any.

### 7. Show dirty-git summary

Cat `$HOME/.claude/.checkpoint/git-status.txt`. If non-empty: list
the repos with uncommitted changes as a reminder.

### 8. Delete the checkpoint

Once everything's restored, **remove** `$HOME/.claude/.checkpoint/`.
This is single-shot — leaving it on disk would cause confusion if
the user runs `/restore` again later.

If any step failed (e.g. TaskCreate errored), KEEP the checkpoint
files for the failed entries so the user can retry.

### 9. Final report

```
Restored:
- 3 tasks (re-created with new IDs)
- 1 loop resumed (job ID f88a2110, every minute)

Background processes that died on exit:
- pid=12345 (cargo build) — you'd need to re-kick if still needed

Dirty git trees (still uncommitted, paul-side decision):
- /home/paul/projects/claude-tools

Pick up where you left off.
```

## Edge cases

- **No checkpoint**: report cleanly and stop. Don't error.
- **Malformed checkpoint files**: report which file is broken, leave
  the checkpoint dir in place, don't repair. User can manually fix.
- **Partial restore** (some steps succeeded, some failed): KEEP the
  checkpoint dir, report what worked + what didn't. User can manually
  fix or re-run.

## Don't

- Don't re-execute task descriptions as commands. Tasks are
  declarative descriptions of work — restoring means re-creating
  the entries, not running them.
- Don't auto-commit anything. Dirty git trees stay dirty.
- Don't try to re-launch background processes. Show them, let the
  user decide.
