# SESSION-PROTOCOL — how every Claude/agent session bootstraps and operates

Canonical per-session instruction for this infrastructure. Auto-refreshed each
session (claude-tools pulls itself). Part of the PlausiDen Agent OS
(subsystem **S**). Full platform spec: `~/.claude/notes/plausiden-agent-os-spec.md`.

## On session start (partly automated via SessionStart hooks)
1. **Auto-refresh** — `lib/session-refresh.sh` pulls claude-tools + PlausiDen-Meta
   and re-installs on change. (hook, automatic)
2. **Registry context** — `lib/session-context.sh` injects the durable
   `agent-registry` summary (all projects/goals/tasks/wishlist + how to query).
   (hook, automatic)
3. **Resume state** — if resuming: read `~/.claude/notes/sacredvote-loop-STATE.md`
   FIRST, then `~/.claude/projects/-/session-state.md`. If a `/checkpoint` exists,
   run `/restore`.
4. **Memory** — `MEMORY.md` index loads automatically; open specific
   `project_*/feedback_*` files as relevance demands.

## While working
- **Registry is the backbone.** New projects/goals/wishlist/audit-checks go in
  `agent-registry` (they persist across sessions), not only the ephemeral session
  task list. Query it: `agent-registry list --kind ... --status ... --tag ...`.
- **Double-check / verify.** After any substantive change, run `/double-check`
  and the relevant installed scanners (semgrep, trivy, cargo-audit, lynis) +
  drive the real flow before declaring done. Fewer errors > faster.
- **Reuse before rebuild.** Check existing repos/tooling first; contribute back
  rather than reinventing (claude-tools, agent-* toolkit, PlausiDen-Meta).
- **Quality checks** (generic, growing — Agent OS subsystem Q): mobile, desktop,
  a11y (WCAG), light/dark themes, performance, security/compliance. Add new
  check types to `agent-registry` as `kind=check` when discovered.

## Before ending substantive work (assume compaction/crash imminent)
- Update the session TaskList (durable across compaction).
- Write durable facts to `~/.claude/projects/-/memory/*.md` (+ MEMORY.md index).
- Append a state line to `~/.claude/notes/*-STATE.md`.
- Register any new project/goal/wishlist/check in `agent-registry`.
- Rule of thumb: if a future-you in a fresh context would need to know it, write
  it to disk before the turn ends.

## Reliability posture (standing directive, 2026-07-13)
Collect signal on errors/quality; recover cleanly when things break; improve over
time and carry lessons into future sessions via memory + this doc. Use cheaper
models to monitor/route and escalate to larger models for hard steps
(Agent OS subsystem I — self-improvement + model tiering, in design).

## This doc grows
When a new "we need to check/audit/improve X" directive arrives, add it here
AND register it in `agent-registry` (kind=check or wishlist). The checklist
accretes; nothing gets lost.
