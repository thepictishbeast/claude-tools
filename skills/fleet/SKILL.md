---
name: fleet
description: Orchestrate the PlausiDen agent fleet (Conductor + worker swarm). Validate a multi-agent plan's scope-disjointness, dispatch a run, run the verify/burn gates, inspect live status or an individual worker, and engage/clear the global STOP. Use when the user wants to run, plan, gate, inspect, or stop the fleet, or open the fleet console. Heavy ops defer to the `fleet` MCP server.
---

# /fleet — agent-fleet orchestration (MCP-backed)

The fleet's control surface ships as the **`fleet` MCP server** (`fleet mcp`,
registered with Claude). Prefer the MCP tools over shelling `fleet` — they are
typed, return structured JSON, and lazy-load their schemas (you only pay context
for tools you actually call). Reach for these before bash.

## MCP tools (the Conductor's control surface)
| Tool | What | Kind |
|---|---|---|
| `fleet_validate_plan` | RAIL 2 dry check: parse a plan, report workers + whether scope-claims are disjoint (`disjoint:false` is a refusal verdict, not an error) | read-only |
| `fleet_run` | Run a plan. Default dry-run; `live:true` enforces RAIL 2 then dispatches workers detached — poll `fleet_status` | gated dispatch |
| `fleet_burn` | Quality-token-burn: run live at high concurrency on real backlog (429-retry + result-accept gate) | dispatch |
| `fleet_gate` | RAIL 3 result-accept gate: check the working tree against the plan's claims (`strays` = changes outside all claims) | read-only |
| `fleet_status` | Bus snapshot: worker messages, kill-switch state, latest result-accept verdict | read-only |
| `fleet_stop` / `fleet_resume` | Engage / clear the global kill switch (STOP) | control |
| `fleet_worker_status` | One worker's live state — status, scope-claim, attempt, last heartbeat/finding, drift flags | read-only |
| `fleet_worker_memory` | Tail a worker's own running notes (memory.md) | read-only |
| `fleet_worker_diff` | A worker's changes within its scope-claim (paths + `git diff --stat`) | read-only |

## Safety (load-bearing — see the 2026-06-03 incident)
- **Never run the fleet against a live / auto-deployed tree.** Use safe-test mode (isolated repo copy). Workers once edited the live ProsperityClub tree and broke dev.plausiden.com.
- `fleet_validate_plan` (RAIL 2, scope-disjointness) and `fleet_gate` (RAIL 3, result-accept) are hard gates, not advisory. Trust the deterministic gate over any worker's self-review.
- `fleet_stop` is the kill switch — engaging it refuses all further spawns until `fleet_resume`.

## Console
The live command-center is `fleet-console` on `:8848` (SSE-driven; `dev4.plausiden.com` is the deployed static dashboard). It exposes message / upload / command / stop-resume. Spawn + per-agent settings are intentionally gated stubs pending safe-test wiring.

## Don't
- Don't dispatch a live run without `fleet_validate_plan` passing first.
- Don't bypass the STOP / safe-test guards to "go faster".
