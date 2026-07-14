# Loop policy — no wasted intervals, no accidental stops

Owner directive (2026-07-13): "we need a policy on loops. so we don't get
wasted intervals but also don't get the loop to stop randomly when i want it
to continue. it needs to infinitely do good work." This is that policy. It
binds every scheduled loop (ScheduleWakeup, cron, /loop) on every project.

## Rule 1 — A loop never stops by accident

Every turn in an active loop ends in exactly one of two states:

1. **Armed**: a wakeup/cron is scheduled, OR a harness-tracked background job
   is running (its completion notification re-invokes us — that counts).
2. **Explicitly stopped**: the loop states WHY it stopped (all work blocked on
   the owner / owner said stop / sentinel pause) and HOW it resumes. A stop is
   a decision announced in the turn's final message — never an omission.

Ending a turn with neither is a policy violation. If unsure, arm.

## Rule 2 — No wasted intervals

A wakeup that lands only to discover "the gate is still running" is a burned
interval. Prevent it:

- **Harness notifications are the primary re-entry.** A tracked background
  job re-invokes us when it finishes. Do NOT schedule a short wakeup to poll
  it — schedule a LONG fallback (>= 20 min) that only fires if the
  notification never arrives (hung job insurance).
- **When scheduling against a known duration** (external CI, DNS, cert
  issuance), set the wakeup to land just AFTER expected completion, not on a
  fixed short cadence.
- **Do work while waiting** when a second workstream exists that can't
  conflict with the pending commit (different repo, read-only research,
  docs). Never dirty the tree a running gate is about to certify.

## Rule 3 — Every tick does the highest-value available thing

Priority ladder, evaluated each tick:

1. **In-flight step** (continue what's mid-flight; never restart done work —
   verify from git/task board, not memory).
2. **Highest-ROI unblocked task** on the board.
3. **Real signals**: gate failures, monitor alerts (site-gate/log-watch),
   security findings — fix root causes, not symptoms.
4. **Owner-blocked?** Then idle heartbeat (Rule 4). NEVER invent busywork —
   cosmetic churn, re-audits of green systems, and speculative refactors are
   worse than idling.

## Rule 4 — Idle heartbeat instead of stopping

When every task is blocked on the owner, the loop does NOT stop. It drops to
a **long-interval heartbeat (30–60 min)** that each beat:

- checks whether any blocker cleared (owner replied, ADR ack'd, files arrived);
- glances at monitors (failed units, disk, gate status) and acts on real
  findings;
- otherwise ends silently, re-armed. No log spam, no commits, no token burn
  beyond the glance.

This is what "infinitely do good work" means in practice: infinite readiness,
zero manufactured work.

## Rule 5 — Interruptions don't derail

Owner messages mid-loop: capture as tasks immediately (TaskCreate), finish
the current atomic step (never leave a broken tree), then re-order by ROI.
The owner's latest priority wins ties. Sentinel/usage-limit pauses are
honored and stated; the loop resumes automatically at reset.

## Rule 6 — Ticks are atomic and durable

Each tick: verify ground truth → do one bounded thing → gate → commit →
save state (memory + task board) → re-arm. A tick that can't finish its
thing commits nothing, leaves the tree clean, records where it stopped, and
re-arms. Crash-recovery reads state from disk, never from chat memory.

## Anti-patterns (all observed in the wild; all banned)

- Short wakeup polling a harness-tracked job (burns intervals). 
- Ending a turn un-armed because "everything seems done" without stating the
  stop decision.
- Re-running a green gate "just in case" with no diff.
- Marking a task complete to make the board look finished (tests red,
  work partial).
- Treating each identical cron prompt as a fresh directive instead of a
  continuation signal (see CLAUDE.md "single rule").

## Amendment 2026-07-14 (owner directive, overrides Rule sections above)

Paul: "why isnt your loop interval 1m ... it needs to infinitely do good
work" + "what part of dont stop do you not understand".

1. **Interval is 1 minute, owner-set.** Not a judgment call. Idle-heartbeat
   stretching (20-60 min) is retired; an idle tick self-checks cheaply and
   ends. The usage-guard breaker is the ONLY thing that may pause firing,
   and it lifts itself at quota reset.
2. **Never stop arming.** `feedback_loop_idle_when_no_signal` (stop after 3
   idle ticks) is superseded for paul's improvement loops. Blocked-on-paul
   is not idle: re-verify state, advance the next unblocked ladder item,
   however small.
3. **Owner decisions default to CEO mode.** If a decision is reversible,
   make the call, execute, and report it with the revert path - don't park
   work as "awaiting paul" (the CMS access route sat parked for a day;
   the executable answer took 20 minutes).
