---
name: forge
description: Build, audit, deploy, and reason about the Forge + Loom + Crawler substrate. Token-efficient subcommand surface, structured JSON output; reach for these before bash/grep. Use in any tenant or substrate repo. Heavy ops defer to the `forge` MCP server.
---

# /forge — PlausiDen-Forge substrate operations (MCP-backed)

Use when working in any tenant repo (`~/projects/<Name>/`) or substrate repo
(`~/projects/PlausiDen-Forge`, `PlausiDen-Loom`, `PlausiDen-Crawler`).

## Prefer the `forge` MCP server
Forge ships as the **`forge` MCP server** (`forge-mcp`, registered with Claude).
Prefer its typed tools over shelling `forge` — structured JSON, no CLI re-parse,
schemas lazy-load so context cost is per-tool-called. ~21 tools incl.
`forge.orient`, `forge.build`, `forge.audit*`, `forge.doctrine.for`,
`forge.codegen`, `forge.synthesis.preview`, `forge.manifest.validate`,
`forge.docs.query`, `forge.resumption_brief`, `forge.canonical_tasks`,
`forge.bricks`, `forge.budgets`, `forge.exemplars`. Shell fallback:
`~/projects/PlausiDen-Forge/target/release/forge <cmd>`.

## Orientation FIRST
Every session that touches the substrate runs `forge orient` (tool or CLI)
before anything else — it enumerates doctrine rules in scope, the skills
inventory, canonical defaults (axum / tokio / sqlx / maud / serde-deny-unknown /
Ed25519 + ML-DSA / clap / anyhow-thiserror / proptest / tracing), anti-patterns,
and next-step commands. It replaces 5–10 file reads.

## Tenant model
- `PlausiDen-Forge/` — substrate ONLY (Rust crates, phases, docs). No `cms/*.json`, no `static/assets/*`.
- `~/projects/<Tenant>/` — per-tenant repo: `cms/index.json`, `static/assets/*`, `forge.toml`, optional `variables.json` / `palette.json` / `site.json`.
- Build a tenant: `cd ~/projects/<Tenant> && forge build --root .`

## Subcommand cheat-sheet
| Command | What | When |
|---|---|---|
| `forge orient` | Session brief | First action |
| `forge build --root <tenant>` | Run every phase against the tenant | After CMS/substrate edits |
| `forge build --json-report <path>` | Structured build report | Parsing findings programmatically |
| `forge audit <phase> [--explain]` | One-off scan (+ why it fires / how to fix) | Pre-commit, ad-hoc, clearing a strict |
| `forge authoring` | Flag empty / below-floor content fields | Before declaring a tenant authored |
| `forge synthesis preview` | Preview a SiteSpec before generating cms/ | Designing a tenant from spec |
| `forge codegen` | Emit a self-contained axum+tokio+sqlx crate from cms/ | Static build needs server handlers |
| `forge doctrine for <path> [--terse]` | Applicable rules (terse = citation-ready) | Cite rules in PRs |
| `forge fix` | Auto-fix mechanical findings from latest report | After a build with fixable findings |

## Shipped tenant-style surface (as of 2026-06)
`forge.toml [style.*]` drives rendering without hand-authored CSS:
palette / fonts / radius / density, plus `[style.nav|image|weights|sizes|text]`
CSS-var hooks and self-hosted `[[style.webfonts]]`. Tenant overrides emit as an
**external, SRI-pinned `/tenant-style.css`** (CSP `style-src 'self'` — inline is
blocked). `[render] clean_urls=true` → extensionless routing; `<root>/site.json`
→ site-wide shared chrome (nav/footer/brand). `{{ VAR }}` / `@asset-slug`
substitution from `variables.json` / `assets-map.json` is live.

## Substrate-vs-tenant discipline (ABSOLUTE)
Hand-coding HTML / CSS / JS in tenant repos is forbidden — `forge build` flags it
via `substrate_purity`. Every gap is a substrate change:
1. Add a Loom primitive (typed `CmsSection` / `CmsBlock` variant), or
2. Add a Forge phase / gate, or
3. Extend doctrine.
Tenant authoring goes through `CmsSection` premades or `CmsSection::Compose` +
atomic `CmsBlock` primitives (text/heading/image/link/spacer/divider/container/row/column).

## Don't
- Don't bash/grep when a Forge subcommand/tool exists.
- Don't add a new section primitive when atomic `CmsBlock` composition suffices (doctrine `prim-012` rejects one-tenant-shaped premades — generalize to variants/config).
- Don't bake tenant identity into substrate Rust or commit messages — substrate stays generic.
