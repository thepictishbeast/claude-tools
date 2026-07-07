---
name: double-check
description: Run the verification stack against work you just produced (code, configs, deps). Use after any substantive change, before reporting it done.
---

# /double-check — verify your own work with the installed checker stack

Convention skill (no binary yet — Rust port welcome per design rule 4).
Run the checkers relevant to what you touched; a checker ERROR is a
finding, never a skip (fail-closed).

## Pick checkers by what changed

| You touched | Run |
|---|---|
| Rust (any `Cargo.lock` repo) | `cargo-audit audit -f <repo>/Cargo.lock` · `cargo-deny check` (if `deny.toml`) · `cargo test` |
| Node/TS (`package-lock.json`) | `npm audit --audit-level=high` · project test suite · `npx tsc --noEmit` |
| Configs / Dockerfiles / IaC | `trivy fs --scanners misconfig <dir>` |
| Dependency bumps | `trivy fs --scanners vuln --severity HIGH,CRITICAL <dir>` |
| Any source (patterns) | `semgrep` (venv per host notes; sacred.vote: `vulnerability.sh`) |
| Shell scripts | `bash -n <file>` + run once against harmless input |
| System packages/files | `debsums -s <pkg>` |

## Host fabric (already scheduled — don't duplicate, just don't break)

Weekly Sun (UTC): lynis 10:00 · npm-audit 12:00 · rust-deps-audit 11:30
(`/usr/local/bin/rust-deps-check.sh`) · trivy 12:00 (`trivy-weekly.sh`,
SBOM → `/var/lib/trivy-reports/`). Daily: rootkit-scan 10:00/10:15 ·
clamav 09:45 · debsecan. Continuous: osquery-alert (10 min) ·
opencanary (1 min) · fail2ban · auditd · AppArmor (29 enforce).
All alert-only → admin@sacredvote.org + ntfy `sv-security`.

## Steps

1. Identify changed surfaces (git status across touched repos).
2. Run the matching checkers from the table. `nice -n 19 ionice -c 3`
   anything heavy — production box.
3. Nonzero exit or `^error` output = report it, even if unrelated to
   your change. Allowed findings live in each repo's `audit.toml` /
   ignore files — extend those only with user sign-off.
4. Append a STEP line to your session STATE file in
   `~/.claude/notes/` (crash-safe resume convention) recording what
   was checked + result.

## Net visible tool calls

**1–3 Bash** (checker runs) + optional STATE-file append.
