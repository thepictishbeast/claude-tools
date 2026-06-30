# Known issues — guard resilience (2026-06-30)

Filed as a committed doc because the GitHub API/PAT was returning HTTP 401 at
the time (could not open Issues via `gh`/REST). Convert these to formal Issues
once auth is restored.

---

## Issue 1 — guard should emit a self-identifying line on every run

When `claude-loop guard` runs at STEP 0 of a fire, its JSON
(`{"action":"continue","iter":N,"label":"..."}`) does not announce that it IS
the guard, nor what `continue` asserts about the PRIOR fire vs the CURRENT one.

**Observed failure (2026-06-30):** after a mid-fire API-error crash, a human
manually resumed the same agent context. The agent re-read the STEP-0
`"continue"` from earlier in the transcript as "the previous fire completed
cleanly, no API error" — which was false; the fire had since died.

**Request:** guard always prints an explicit self-check line, e.g.

    [claude-loop guard] iter=78 action=continue
      meaning: prior fire closed cleanly; THIS fire is now marked STARTED at <ts>
               and MUST be checkpointed before exit, or the next guard will HALT.

so a reader knows (a) this output is the guard, (b) exactly what `continue`
claims, (c) that the current fire is now an OPEN iteration.

---

## Issue 2 — STEP-0 "continue" masks a same-fire API-error crash (false-clean on resume)

**Sequence observed 2026-06-30 (label `sacredvote`):**
1. Cron fires iter 78 -> guard STEP 0 writes `start iter78` and returns
   `action:continue` (correct at that instant: iter 77 had closed cleanly).
2. iter 78 then dies on an API error BEFORE writing its `done`/checkpoint.
3. A human manually resumes the same context. The agent quotes the earlier
   STEP-0 `continue` as evidence "no API error, completed cleanly."
4. Only a FRESH `guard` run returns
   `{"action":"halt","reason":"previous iteration started but never completed"}`
   (exit 3), because now there is an unmatched iter-78 `start`.

**Gap:** within a single fire there is no signal that the current iteration is
open-and-uncheckpointed. The STEP-0 `continue` says nothing about whether the
CURRENT fire will complete, yet reads as reassuring.

**Requests:**
- (a) On every invocation, guard reports "current fire opened at <ts>, not yet
  checkpointed" when an iteration is open.
- (b) Consider a heartbeat/lease so a crashed fire is detectable promptly,
  instead of only on the next guard run.
- (c) Document that STEP-0 `continue` is a statement about the PRIOR fire only.

**Repro artifact:** `status.jsonl` shows `{"event":"start","iter":78}` at
2026-06-30T23:13:06 with no matching `done`; a fresh `guard` run -> halt/exit 3.
