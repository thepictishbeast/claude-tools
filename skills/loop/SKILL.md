---
name: loop
description: Manage active Claude Code /loop cron jobs — pause, resume, stop/cancel, edit (interval/prompt), track an untracked cron into history, or update the claude-tools install. Use whenever the user wants to pause / resume / stop / cancel / kill / change / reschedule a loop, register an existing cron, or upgrade loop tooling. Pass the verb as the first argument (e.g. "pause", "resume 5m", "stop", "edit", "track <id>", "update").
---

# /loop — manage Claude Code loop cron jobs (Rust-binary backed)

One skill for the whole loop-management family (consolidated from the former
loop-pause / loop-resume / loop-stop / loop-edit / loop-track / loop-update
skills, 2026-06-30). Backed by the `claude-loop` binary
(`~/.local/bin/claude-loop`, built by claude-tools `install.sh`). The binary
owns every shell op (state JSON, history, locking); the agent only makes the
`Cron*` tool calls. Route on the **first argument** (the verb):

| Verb | Does | Mechanism |
|---|---|---|
| `pause` | Pause all active loops; save state for resume | `CronList` → pipe its JSON to `claude-loop pause` → `CronDelete` the returned IDs |
| `resume [interval]` | Restore paused loops (optional new cadence) | `claude-loop resume [--interval <i>]` → `CronCreate` ×N from its JSON-line output |
| `stop` | Permanently stop a loop (delete cron + clear paused state) | `CronList` → `CronDelete` the entry; clear `~/.claude/.paused-loops.json` |
| `edit` | Change interval/prompt without losing the prompt | `CronList` (full prompt) → `CronDelete <old>` → `CronCreate` with the edits |
| `track <id>` | Register a raw cron into loop history | `CronList` (verify id) → `claude-loop` logs a `discovered` event |
| `update` | Pull latest claude-tools + reinstall skills/binaries | run `~/projects/claude-tools/update.sh -f`; report which commits arrived |
| _(none)_ | Show current loop state | defer to the `loops` skill (`claude-loop list`) |

## Rules
- **Never inline-truncate prompts.** `CronList` truncates prompts at ~80 chars; for `edit` / `track` / `resume` read the full prompt from the binary's state file, not the truncated `CronList` view.
- Each verb is a thin path — 2–3 visible tool calls (one `Bash` to the binary + the `Cron*` calls). Let the binary do the file work.
- `pause` / `resume` are symmetric (state in `~/.claude/.paused-loops.json`). `stop` is terminal — no state kept.
- Pipe every loop prompt through `claude-loop prep` before `CronCreate` — it injects the canonical SCHEDULED-LOOP self-check preamble (idempotent), so every fire carries it by construction.
- Crash-safety lives in `claude-loop guard` / `checkpoint` / `watchdog`; see `docs/LOOP_PATTERNS.md`.

## Don't
- Don't `CronDelete` + `CronCreate` from scratch for an `edit` — that loses the full prompt unless you pulled it first.
- Don't append a history event if the `Cron*` call failed.
