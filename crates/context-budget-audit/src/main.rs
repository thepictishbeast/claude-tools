//! `context-budget-audit` — preflight guard for the FIXED part of the prompt.
//!
//! WHY THIS EXISTS
//!
//! On 2026-09-03 a freshly spawned subagent died immediately with:
//!
//! ```text
//! Prompt is too long · the request is ~284909 tokens (limit 200000)
//! but this conversation is only ~906 tokens — the rest is system prompt,
//! tool definitions, and attachment content.
//! ```
//!
//! The conversation was 906 tokens. Nothing the agent did could have saved
//! it: 203 enabled plugins had already pushed the base prompt past the
//! model's 200k window before the agent read a single file. The parent
//! session never noticed, because it was running on a 1M-context model.
//!
//! That is the failure this tool is for. The usual context guard watches the
//! conversation grow and suggests compacting — useless here, because the
//! conversation was not the problem and compaction cannot shrink a tool
//! catalogue. The cost is FIXED, it is known before the run starts, and so
//! it can be checked in advance. That is the whole idea: fail at audit time,
//! on purpose, instead of at spawn time, by surprise.
//!
//! WHAT IT MEASURES
//!
//! Two numbers, and it is explicit about which is which:
//!
//! * A *measured* base-prompt size, recorded with `--observed`. Paste the
//!   number out of a real "Prompt is too long" error (or out of `/context`)
//!   and it is stored in the state file. Measured beats estimated, always.
//! * An *estimated* size, derived from the enabled-plugin count, the MCP
//!   server count, and a per-unit cost. The estimate exists so the tool
//!   says something useful on day one, before anyone has hit a failure. It
//!   is calibrated automatically the first time you record a measurement.
//!
//! Either way the check is the same: does the base prompt leave enough room
//! in each configured model's window to do actual work?
//!
//! Exit codes:
//! * 0 — every configured model has room
//! * 1 — at least one configured model is over budget (this is the useful failure)
//! * 2 — operational error (unreadable config, bad arguments)
//!
//! Usage:
//! ```sh
//! context-budget-audit                      # check the current config
//! context-budget-audit --observed 284909    # record a real measurement and calibrate
//! context-budget-audit --json               # machine-readable, for hooks and CI
//! context-budget-audit --headroom 60000     # require more free space than the default
//! ```
//!
//! Wire it into a SessionStart hook, or run it after changing which plugins
//! are enabled — the two moments when the answer can change.

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Working room a session needs BEYOND the base prompt to be worth starting.
/// A model whose whole window is swallowed by tool definitions can technically
/// start and cannot usefully finish, which is the failure mode that looks like
/// success until it does not.
const DEFAULT_HEADROOM: u64 = 40_000;

/// Fallback per-plugin cost, in tokens, used only until a real measurement
/// calibrates it. Derived from the 2026-09-03 incident: ~285k observed across
/// 203 enabled plugins, minus roughly 10k of non-plugin system prompt.
const DEFAULT_TOKENS_PER_PLUGIN: f64 = 1_350.0;

/// Fallback per-MCP-server cost. Servers vary enormously (a two-tool server
/// versus a fifty-tool one), so this is deliberately a coarse placeholder that
/// a measurement replaces.
const DEFAULT_TOKENS_PER_MCP: f64 = 2_000.0;

/// Everything that is neither a plugin nor an MCP server: the base system
/// prompt, built-in tool definitions, CLAUDE.md, memory index.
const DEFAULT_BASE_OVERHEAD: f64 = 10_000.0;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Claude config directory. Defaults to `~/.claude`.
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// Path to `.claude.json`. Defaults to `~/.claude.json`.
    #[arg(long)]
    claude_json: Option<PathBuf>,
    /// Record a REAL measured base-prompt size in tokens and calibrate the
    /// estimator from it. Read it off a "Prompt is too long" error or /context.
    #[arg(long)]
    observed: Option<u64>,
    /// Tokens of working room a model must have left after the base prompt.
    #[arg(long, default_value_t = DEFAULT_HEADROOM)]
    headroom: u64,
    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

/// Persisted calibration, so a measurement taken once keeps paying off.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Last measured base-prompt size in tokens.
    observed_tokens: Option<u64>,
    /// Enabled-plugin count at the moment of that measurement.
    observed_plugins: Option<usize>,
    /// MCP server count at the moment of that measurement.
    observed_mcp: Option<usize>,
    /// ISO-8601 date the measurement was taken, for staleness judgement.
    observed_at: Option<String>,
}

/// A model the config actually asks for, and the window it has.
#[derive(Debug, Serialize)]
struct ModelSlot {
    /// Where the model came from, e.g. "main session" or "subagent default".
    role: String,
    /// The raw identifier as written in config, e.g. "claude-opus-5" or "opus[1m]".
    id: String,
    /// Context window in tokens.
    window: u64,
}

/// Context windows by model family. The `[1m]` suffix is what selects the
/// long-context variant, and it is the single most common reason two agents in
/// the same session behave differently: the parent fits, the child does not.
fn window_for(model_id: &str) -> u64 {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("[1m]") || lower.contains("-1m") {
        return 1_000_000;
    }
    // Every current family is 200k without the long-context suffix.
    200_000
}

fn main() -> Result<()> {
    let args = Args::parse();

    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let config_dir = args
        .config_dir
        .clone()
        .or_else(|| home.as_ref().map(|h| h.join(".claude")))
        .context("could not resolve config directory (HOME unset?)")?;
    let claude_json = args
        .claude_json
        .clone()
        .or_else(|| home.as_ref().map(|h| h.join(".claude.json")))
        .context("could not resolve .claude.json (HOME unset?)")?;

    let settings_path = config_dir.join("settings.json");
    let settings = read_json(&settings_path)?;
    let root = read_json(&claude_json).unwrap_or(serde_json::Value::Null);

    let enabled_plugins = count_enabled_plugins(&settings);
    let mcp_servers = count_mcp_servers(&root);
    let models = collect_models(&settings, &root);

    // Load, optionally update, and persist calibration.
    let state_path = config_dir.join("context-budget-audit.json");
    let mut state = read_state(&state_path);
    if let Some(observed) = args.observed {
        state.observed_tokens = Some(observed);
        state.observed_plugins = Some(enabled_plugins);
        state.observed_mcp = Some(mcp_servers);
        state.observed_at = Some(today());
        write_state(&state_path, &state)?;
    }

    let (base_tokens, source) = estimate_base(&state, enabled_plugins, mcp_servers);

    // A model is over budget when the base prompt plus the required working
    // room does not fit. Reporting the two separately matters: "does not fit at
    // all" and "fits but leaves no room to work" are different conversations.
    let mut over: Vec<&ModelSlot> = Vec::new();
    for m in &models {
        if base_tokens + args.headroom > m.window {
            over.push(m);
        }
    }

    if args.json {
        let payload = serde_json::json!({
            "enabledPlugins": enabled_plugins,
            "mcpServers": mcp_servers,
            "baseTokens": base_tokens,
            "baseTokensSource": source,
            "headroom": args.headroom,
            "models": models,
            "overBudget": over.iter().map(|m| &m.role).collect::<Vec<_>>(),
            "ok": over.is_empty(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("context-budget-audit");
        println!("  enabled plugins : {enabled_plugins}");
        println!("  mcp servers     : {mcp_servers}");
        println!("  base prompt     : ~{base_tokens} tokens ({source})");
        println!("  required room   : {} tokens", args.headroom);
        println!();
        for m in &models {
            let needed = base_tokens + args.headroom;
            let verdict = if needed > m.window { "OVER" } else { "ok" };
            println!(
                "  [{verdict:>4}] {:<18} {:<22} window {}",
                m.role,
                m.id,
                human(m.window)
            );
        }
        if !over.is_empty() {
            println!();
            println!("  {} of {} configured models cannot run here.", over.len(), models.len());
            println!("  The base prompt is FIXED cost -- it is paid before any file is read,");
            println!("  and compacting the conversation cannot reduce it. Either:");
            println!("    1. disable plugins you do not use (largest single lever), or");
            println!("    2. give the affected role a long-context model (the [1m] suffix).");
            if state.observed_tokens.is_none() {
                println!();
                println!("  This figure is ESTIMATED. Record a real one to make it exact:");
                println!("    context-budget-audit --observed <tokens-from-the-error-or-/context>");
            }
        }
    }

    if over.is_empty() {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Enabled plugins in `settings.json`. Only entries explicitly set true count;
/// a plugin present but disabled costs nothing.
fn count_enabled_plugins(settings: &serde_json::Value) -> usize {
    settings
        .get("enabledPlugins")
        .and_then(|v| v.as_object())
        .map(|o| o.values().filter(|v| v.as_bool() == Some(true)).count())
        .unwrap_or(0)
}

/// MCP servers, counted across the global block and every project block.
/// Project-scoped servers still load for sessions in that project, so a global
/// count alone understates the cost.
fn count_mcp_servers(root: &serde_json::Value) -> usize {
    let global = root
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|o| o.len())
        .unwrap_or(0);
    let per_project: usize = root
        .get("projects")
        .and_then(|v| v.as_object())
        .map(|projects| {
            projects
                .values()
                .filter_map(|p| p.get("mcpServers").and_then(|v| v.as_object()))
                .map(|o| o.len())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    global + per_project
}

/// Which models this configuration will actually ask for. Missing entries are
/// skipped rather than guessed: reporting a model the user has not configured
/// would be noise, and the point of the tool is a signal worth acting on.
fn collect_models(settings: &serde_json::Value, root: &serde_json::Value) -> Vec<ModelSlot> {
    let mut out = Vec::new();
    if let Some(m) = settings.get("model").and_then(|v| v.as_str()) {
        out.push(ModelSlot {
            role: "main session".into(),
            id: m.to_string(),
            window: window_for(m),
        });
    }
    for (key, role) in [
        ("teammateDefaultModel", "subagent default"),
        ("subagentModel", "subagent override"),
    ] {
        if let Some(m) = root.get(key).and_then(|v| v.as_str()) {
            out.push(ModelSlot {
                role: role.into(),
                id: m.to_string(),
                window: window_for(m),
            });
        }
    }
    // With nothing configured, subagents inherit whatever the CLI defaults to.
    // Assume the short window, because that is the case that breaks.
    if out.iter().all(|m| m.role != "subagent default") {
        out.push(ModelSlot {
            role: "subagent default".into(),
            id: "(unset -- assumed short context)".into(),
            window: 200_000,
        });
    }
    out
}

/// Measured if we have one, estimated otherwise.
///
/// A measurement is rescaled when the plugin count has moved since it was
/// taken, so turning ten plugins off is reflected immediately instead of
/// waiting for the next failure to re-measure.
fn estimate_base(state: &State, plugins: usize, mcp: usize) -> (u64, &'static str) {
    match (state.observed_tokens, state.observed_plugins) {
        (Some(observed), Some(observed_plugins)) if observed_plugins > 0 => {
            if observed_plugins == plugins {
                (observed, "measured")
            } else {
                // Scale the plugin-attributable share; leave the fixed overhead
                // alone, since it does not move with the plugin count.
                let per_plugin =
                    (observed as f64 - DEFAULT_BASE_OVERHEAD).max(0.0) / observed_plugins as f64;
                let scaled = DEFAULT_BASE_OVERHEAD + per_plugin * plugins as f64;
                (scaled.round() as u64, "measured, rescaled for current plugin count")
            }
        }
        (Some(observed), _) => (observed, "measured"),
        _ => {
            let est = DEFAULT_BASE_OVERHEAD
                + DEFAULT_TOKENS_PER_PLUGIN * plugins as f64
                + DEFAULT_TOKENS_PER_MCP * mcp as f64;
            (est.round() as u64, "estimated")
        }
    }
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// A missing or corrupt state file is not an error: it just means nothing has
/// been measured yet, which is the normal starting condition.
fn read_state(path: &Path) -> State {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_state(path: &Path, state: &State) -> Result<()> {
    let raw = serde_json::to_string_pretty(state)?;
    fs::write(path, raw).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

fn human(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}k", tokens / 1_000)
    }
}

/// Date only, no clock dependency -- the crate deliberately avoids pulling
/// chrono in for a single field that exists to answer "is this stale?".
fn today() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_context_suffix_selects_the_big_window() {
        assert_eq!(window_for("opus[1m]"), 1_000_000);
        assert_eq!(window_for("claude-opus-5[1m]"), 1_000_000);
        assert_eq!(window_for("claude-opus-5"), 200_000);
        assert_eq!(window_for("claude-haiku-4-5-20251001"), 200_000);
    }

    #[test]
    fn only_enabled_plugins_are_counted() {
        let v = serde_json::json!({
            "enabledPlugins": { "a": true, "b": false, "c": true }
        });
        assert_eq!(count_enabled_plugins(&v), 2);
    }

    #[test]
    fn missing_plugin_block_costs_nothing() {
        assert_eq!(count_enabled_plugins(&serde_json::json!({})), 0);
    }

    #[test]
    fn mcp_count_includes_the_heaviest_project() {
        let v = serde_json::json!({
            "mcpServers": { "g1": {} },
            "projects": {
                "/a": { "mcpServers": { "p1": {}, "p2": {} } },
                "/b": { "mcpServers": { "p3": {} } }
            }
        });
        // One global plus the worst project, not the sum of all projects: a
        // single session only ever loads one project's servers.
        assert_eq!(count_mcp_servers(&v), 3);
    }

    #[test]
    fn a_measurement_beats_the_estimate() {
        let state = State {
            observed_tokens: Some(284_909),
            observed_plugins: Some(203),
            observed_mcp: Some(0),
            observed_at: Some("2026-09-03".into()),
        };
        let (tokens, source) = estimate_base(&state, 203, 0);
        assert_eq!(tokens, 284_909);
        assert_eq!(source, "measured");
    }

    #[test]
    fn turning_plugins_off_lowers_the_measured_figure() {
        let state = State {
            observed_tokens: Some(284_909),
            observed_plugins: Some(203),
            observed_mcp: Some(0),
            observed_at: Some("2026-09-03".into()),
        };
        let (before, _) = estimate_base(&state, 203, 0);
        let (after, source) = estimate_base(&state, 20, 0);
        assert!(after < before, "halving the plugin count must lower the figure");
        assert!(source.starts_with("measured"));
        // And it must land somewhere sane rather than collapsing to zero.
        assert!(after > DEFAULT_BASE_OVERHEAD as u64);
    }

    #[test]
    fn estimate_is_used_when_nothing_has_been_measured() {
        let (tokens, source) = estimate_base(&State::default(), 100, 2);
        assert_eq!(source, "estimated");
        assert!(tokens > 100_000);
    }

    #[test]
    fn an_unset_subagent_model_is_assumed_short() {
        let settings = serde_json::json!({ "model": "opus[1m]" });
        let root = serde_json::json!({});
        let models = collect_models(&settings, &root);
        let sub = models
            .iter()
            .find(|m| m.role == "subagent default")
            .expect("an unset subagent model must still be reported");
        assert_eq!(sub.window, 200_000);
    }

    #[test]
    fn the_incident_config_is_reported_as_over_budget() {
        // The exact 2026-09-03 shape: a 1M parent and a short-window child.
        let settings = serde_json::json!({ "model": "opus[1m]" });
        let root = serde_json::json!({ "teammateDefaultModel": "claude-opus-5" });
        let models = collect_models(&settings, &root);
        let base = 284_909u64;
        let over: Vec<_> = models
            .iter()
            .filter(|m| base + DEFAULT_HEADROOM > m.window)
            .collect();
        assert_eq!(over.len(), 1, "only the short-window role should fail");
        assert_eq!(over[0].role, "subagent default");
    }
}
