# META — handoff for any Claude (or other agent) working with this repo

**This is the canonical orientation doc.** If you've been routed here
by a `/restore` checkpoint, a teammate's hand-off note, or your own
memory pointing at `~/.claude/skills/`, read this once before doing
anything. After reading, you should know: what the repo is, how
to keep it synced, what's installed, the 4 contribution rules,
and how to add a new tool.

---

## 1. What this repo is

`claude-tools` (formerly `claude-loop-tools`, renamed 2026-05-20)
is the **single umbrella for every Claude Code skill, helper
binary, and MCP server** the `thepictishbeast` org ships. The
`/loop-*` family was the first tool set; new tools (file-adr,
audit-unused-dep, regression-guard, …) live alongside in the
same workspace.

**One repo to clone. One repo to push to. One place to file
issues.** Anything tool-adjacent that doesn't already have a home
goes here.

URL: <https://github.com/thepictishbeast/claude-tools>

---

## 2. Install / sync

**First-time setup** on a fresh machine:

```sh
git clone https://github.com/thepictishbeast/claude-tools ~/Development/claude-tools
cd ~/Development/claude-tools
sh install.sh -f
```

This builds every Rust binary under `crates/*` (via `cargo install --path crates/<name> --root ~/.local --force`) and copies every skill spec under `skills/*` into `~/.claude/skills/`.

**Subsequent syncs** — invoke either skill any time:

- `/loop-update` — git-pulls + re-runs install.sh
- `/restore` — same, runs automatically on session start

You should run `/loop-update` at the start of any work session that
touches Claude tooling. The repo evolves; stale clones produce
divergent agents.

### 2b. Session auto-refresh (added 2026-07-07)

Every Claude session should start current without manual syncs.
`lib/session-refresh.sh` pulls this repo **and** `PlausiDen-Meta`
(≤15s timeout each, never blocks), re-runs `install.sh` when
claude-tools changed, and emits a one-line status. Wire it as a
SessionStart hook in `~/.claude/settings.json`:

```json
"hooks": { "SessionStart": [ { "hooks": [ {
  "type": "command",
  "command": "sh $HOME/Development/claude-tools/lib/session-refresh.sh",
  "timeout": 45, "statusMessage": "Refreshing claude-tools + Meta"
} ] } ] }
```

Notes: hook stdout lands in the session's context — when it says
`updated`, re-read META.md (this file) / PlausiDen-Meta before
relying on stale conventions. NEW skills still only load at the
NEXT session start. Clone location is canonical
`~/Development/claude-tools` (NOT `/tmp` — noexec + volatile);
`~/claude-tools` may be a symlink to it for older path lists.

---

## 3. What's installed (snapshot 2026-05-20)

### Binaries (`~/.local/bin/`)
| Binary | Crate | Purpose |
|---|---|---|
| `claude-loop` | `crates/claude-loop` | subcommands `pause` / `resume` / `list` / `history` / `prep` for the `loop`/`loops`/`loop-*` skills, plus loop crash-safety: `guard` (stop-on-API-error), `checkpoint` (additive per-iteration save-state), `watchdog` (OS-timer local kill-switch that stops a loop even when the agent is fully API-blocked — pair with `lib/claude-loop-fire.sh`), and `sentinel` (OS-timer API-error sentinel — see below) |
| `file-adr` | `crates/file-adr` | scaffold an ADR (Nygard format) + update index |
| `audit-unused-dep` | `crates/audit-unused-dep` | find zero-call-site direct deps in a Cargo.toml |
| `regression-guard` | `crates/regression-guard` | generate a REGRESSION-GUARD test skeleton anchored on a refactor commit |

### Skills (`~/.claude/skills/`)
15 skills total (loop-pause/resume/edit/stop/track/update consolidated into
one `loop` verb-routed skill 2026-06-30; + `fleet` + `forge`, which defer heavy
ops to their MCP servers per the prefer-MCP policy). New skills get listed in
README.md "Tools" section + get a directory under `skills/<name>/SKILL.md`.

### API-error sentinel (`claude-loop sentinel`)

An OS-timer-driven (NO-API) safety net for the recurring usage-policy /
cybersecurity-classifier block that silently halts a loop. Because it touches no
API, it works even when the agent is fully blocked. Every minute it scans the
most-recently-active Claude Code transcript(s) under `<HOME>/.claude/projects/`
for an `isApiErrorMessage` entry; on the FIRST detection of an incident it:

1. **Stops every running loop** — writes the same `DISABLED` + HALT sentinel the
   `watchdog` uses (honored by `guard` on the next fire). Caveat: only stops
   loops that run `guard` at fire-start.
2. **Backs up the whole conversation** — copies the raw transcript JSONL (the
   entire conversation Claude Code writes every turn, regardless of whether
   checkpoint/rewind was ever used) + a `RESUME.md` into
   `~/.claude/.loop-checkpoints/api-incident-<stamp>/`. `RESUME.md` names
   `claude --resume <session-id>` as the canonical resume; the copied JSONL is
   the review/safety fallback.
3. **Notifies** — desktop toast (`notify-send` / `osascript` / PowerShell,
   best-effort) + email via the system mailer if enabled.

Idempotent (one action per incident, keyed on session + matched line) and
recency-gated (`--within-mins`, default 30) so it never re-fires on a historical
error. Set up notifications with `claude-loop sentinel --setup` (prompts for an
email; opt out with `--no-email`). Config: `~/.claude/.sentinel.json`. Run it
from `dist/systemd/claude-sentinel.{service,timer}` (every 1 min).

Audit read-only with `claude-loop sentinel --scan-only --within-mins <N>`: it
classifies every recent transcript's API-error (policy vs transient) and prints
a JSON report — acts on nothing, emails nothing. `test/sentinel-proof.sh` is a
black-box FP/FN harness (synthetic transcripts, isolated state-dirs, email off).
Proven 2026-06-30: synthetic corpus 16/16 (0 FP/0 FN) + a real-transcript sweep
(1,182 transcripts → 186 real policy-blocks vs 996 transient, 0 FP / 0 FN).
Detection keys on the MESSAGE TEXT (`cybersecurity topic` / `cyber-use-case` /
`unable to respond to this request` / `violate our usage policy`), never the
error code (`invalid_request` is generic). Residual FN risk = an unknown FUTURE
re-wording of the block; the `cyber-use-case` exemption-URL anchor is the most
stable mitigation (present in every cyber block).

### Skill-vs-MCP allocation policy (paul 2026-05-21)

> **Cap installed skills at ~15. Beyond that, the discovery + token
> overhead exceeds the benefit. Use MCP servers for additional
> tool surface.**

The cap is a soft target, not a hard rule — intent is "improve
quality + reduce tokens". If a 16th skill genuinely belongs, the
right move is usually to retire a lower-value skill or fold two
adjacent skills into one.

Allocation guidance:

- **Skill** = workflow / convention / when-to-use guidance.
  Workflow shape, decision context, doctrine that the agent
  benefits from holding in context every turn.
- **MCP** = callable typed operation (deferred-schema-loaded so
  it costs no context tokens until invoked). Dozens of MCP tools
  is fine; one skill listing is in every system reminder.

When in doubt: if it's a noun (a callable), it's MCP. If it's a
verb the agent does (a way of doing work), it's a skill.

---

## 4. The 4 design rules (binding for any new tool)

These rules emerged from user feedback 2026-05-19 and are now the
contract for every contribution:

1. **Atomic visible execution** — the agent should see ONE Bash
   call per skill invocation, not 5+ tool calls of manual
   orchestration. Wrap multi-step shell ops in a Rust binary.
   The only tool calls allowed beyond that Bash are Cron* /
   Read / Write / TaskUpdate APIs that have no shell surface.

2. **Token-efficient** — minimal prose in skill specs; tight
   code-block-based step lists; one summary at end (no per-step
   ack chatter). Skills are loaded into every relevant agent's
   context, so verbosity multiplies.

3. **Automates a frequent task** — encodes a real repeated
   workflow (3+ tool calls × 3+ uses per typical session), not
   theoretical convenience. If you find yourself doing the same
   N-step sequence 3 times manually, propose a skill for it.

4. **Rust, not shell** — type safety, testability, clap-derived
   `--help`, no shell-quoting footguns. Reference shape:
   `crates/claude-loop/src/main.rs` or `crates/file-adr/src/main.rs`.
   Pre-existing shell wrappers in `bin/` are migrating
   incrementally; **new tools start in Rust**.

---

## 5. How to add a new tool (3 steps)

1. **Scaffold the crate**:
   ```sh
   cargo new --bin crates/<name>
   ```
   Copy a `Cargo.toml` shape from `crates/file-adr/Cargo.toml`
   (inherits workspace deps via `.workspace = true`). Write the
   binary as a single `src/main.rs` with embedded `#[cfg(test)]
   mod tests`. Use clap (derive), anyhow, serde where applicable.

2. **Write the skill spec** at `skills/<name>/SKILL.md`:
   ```markdown
   ---
   name: <name>
   description: <one short sentence — what it does + when to use>
   ---

   # /<name> — short title

   Thin wrapper around `<binary>` (installed at
   `~/.local/bin/<name>` by `install.sh`). Binary owns
   <list shell ops it owns>.

   ## Steps
   1. <agent action 1>
   2. **Invoke the binary**: `<binary> [args]`
   3. <agent action 2>

   ## Net visible tool calls
   **N** total: <list>
   ```

3. **Test + commit**:
   ```sh
   cargo test --workspace
   sh install.sh -f
   git add -A
   git commit -m "feat: <name> — <one-line>"
   git push
   ```
   `install.sh` auto-picks up new crates — no need to edit it.

That's it. No new repo, no manifest edits, no submodules.

---

## 6. Cross-session conventions

- **Loop hygiene** — read [`CLAUDE.md`](./CLAUDE.md). The "non-
  interruption rule" (a loop fire is a SIGNAL, not a command)
  is load-bearing.
- **Memory** — your session memory lives in
  `~/.claude/projects/<workspace>/memory/`. Conventions in the
  global `~/.claude/CLAUDE.md`. Save user-facing decisions there,
  not in this repo.
- **TaskList scope** — tasks are session-scoped. They survive
  via `/checkpoint` + `/restore` only. Cross-session work
  coordination lives in this repo's GitHub issues (when there are
  any) or in the consuming project's tracker.
- **Commit signing** — use a `Co-Authored-By: Claude Opus 4.7
  (<session-id>) <noreply@anthropic.com>` trailer on commits you
  author so multi-agent contributions are auditable.

---

## 7. Where to push

- **This repo**: `main` branch directly for small fixes; PR for
  anything ≥ a new crate. Tests must pass.
- **Consuming projects** (PlausiDen-AI, PlausiDen-LFI,
  Neurosymbolic-Toolkit, etc.): respect their own branch policies.
- **Don't fork claude-tools to a personal namespace** — push to
  `thepictishbeast/claude-tools` or open a PR from a branch.

---

## 8. Where to find help

- [`README.md`](./README.md) — capability surface + usage examples
- [`CLAUDE.md`](./CLAUDE.md) — loop hygiene doctrine
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — PR workflow + scope
- [`docs/`](./docs) — design notes (LOOP_PATTERNS, SHARING_PROTOCOL, etc.)
- Source: every binary has `--help` via clap derives

If you're confused about whether something belongs in this repo:
default to "yes if it's a skill/MCP/helper for Claude Code", "no
if it's a domain-specific tool for one project."

---

## 9. The single rule to remember

> **Read this file before contributing. Then update it when you
> add a tool or change a convention.**

META.md drift is what makes meta-orientation docs useless. If you
change the structure (new crate, new skill, new rule, renamed
repo, etc.), update the relevant section here in the same commit.
