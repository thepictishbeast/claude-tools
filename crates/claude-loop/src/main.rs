//! `claude-loop` — atomic CLI for the loop-pause / loop-resume /
//! loops / loop-history skills.
//!
//! Token-efficient Rust replacement for the prior markdown-skill
//! orchestration. Each subcommand collapses what was previously
//! 3-5 separate agent-visible Bash + Write + Read tool calls into
//! ONE Bash invocation. The agent still has to call the Cron* tool
//! API directly (CronList / CronCreate / CronDelete) since those
//! have no shell surface — but every state file + chmod + history
//! op is owned by this binary now.
//!
//! Per AVP-2: explicit error handling via anyhow, tested arg
//! parsing via clap derives, no shell-quoting footguns.

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "claude-loop",
    about = "Atomic CLI for /loop-pause /loop-resume /loops",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Override the state directory (default $HOME/.claude/).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Read CronList JSON on stdin, write paused-loops.json, log
    /// pause event, output the IDs to CronDelete.
    Pause {
        /// Label the pause set (free-form; default derived from
        /// the first job's prompt).
        #[arg(long)]
        label: Option<String>,
    },

    /// Read paused-loops.json, output the jobs to CronCreate as
    /// JSON lines (one per job), log resume event, delete the
    /// state file. Optional interval override applies to ALL
    /// resumed jobs.
    Resume {
        /// Interval override: `5m`, `2h`, `1d`, etc. Converts to
        /// cron internally. Default: use saved cron expr.
        #[arg(long)]
        interval: Option<String>,
    },

    /// Print active + paused + recent-history summary (read-only).
    List,

    /// Print loop-history.jsonl tail (read-only).
    History {
        /// Number of recent lines (default 20).
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },

    /// Auto-inject the canonical SCHEDULED-LOOP self-check preamble into a
    /// loop prompt (idempotent), then print the result. The output is what
    /// you feed to CronCreate / ScheduleWakeup so EVERY loop carries the
    /// self-check by construction, not by the agent's discretion. Reads
    /// `--prompt` or, if omitted, stdin.
    Prep {
        /// The loop prompt. If omitted, read from stdin.
        #[arg(long)]
        prompt: Option<String>,
        /// If set, also inject the crash-guard + per-iteration checkpoint
        /// enforcement block bound to this loop label.
        #[arg(long)]
        label: Option<String>,
    },

    /// Loop crash-guard — call at the START of every loop fire. Reads the
    /// per-label status journal: if the previous fire left an unmatched
    /// "start" (the signature of an API error / crash mid-fire), prints
    /// {"action":"halt"} and exits 3 so the caller STOPS the loop instead of
    /// iterating into the same failure. Otherwise records a new "start" and
    /// prints {"action":"continue","iter":N}. Pass --reset to deliberately
    /// re-arm a halted loop.
    Guard {
        /// Loop label (namespaces the status journal + checkpoints).
        #[arg(long)]
        label: String,
        /// Clear a prior halt/open-start and resume from the next iteration.
        #[arg(long)]
        reset: bool,
    },

    /// Additive per-iteration checkpoint — call at the END of every loop fire
    /// (even idle ticks). Writes a timestamped checkpoint-<stamp>.md (full body
    /// read from stdin), closes the open iteration in the status journal,
    /// appends to INDEX.md, and refreshes the canonical ENTRYPOINT.md recovery
    /// pointer. Checkpoints are NEVER overwritten.
    Checkpoint {
        /// Loop label (must match the `guard --label`).
        #[arg(long)]
        label: String,
        /// One-line summary for INDEX.md / ENTRYPOINT.md.
        #[arg(long)]
        note: Option<String>,
    },

    /// Local kill-switch — run from an OS timer (cron/systemd), NOT the agent.
    /// It uses NO API, so it works even when the agent is fully API-blocked. If
    /// the loop's last journal event is an unmatched "start" older than
    /// --max-age-secs (a fire that began but never completed — the signature of a
    /// blocked/crashed agent), it writes a HALT event + a DISABLED sentinel that
    /// `guard` then honors, stopping the loop locally. Exits 3 when it disables.
    Watchdog {
        /// Loop label (must match the `guard --label`).
        #[arg(long)]
        label: String,
        /// Max seconds a fire may be "started" without completing before the loop
        /// is judged stuck/blocked and DISABLED. Set well above a normal fire's
        /// duration (e.g. 1800 for a 30-min-cadence loop).
        #[arg(long, default_value_t = 1800)]
        max_age_secs: u64,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct PausedJob {
    id_original: String,
    cron: String,
    cadence_human: String,
    recurring: bool,
    prompt: String,
    canary_added: bool,
    paused_at: String,
    label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inflight_tasks: Vec<InflightTask>,
}

#[derive(Serialize, Deserialize, Debug)]
struct InflightTask {
    id: String,
    status: String,
    subject: String,
    #[serde(rename = "activeForm", default)]
    active_form: Option<String>,
}

/// CronList JSON input shape (passed on stdin to `pause`).
/// Caller (the agent) constructs this from the CronList tool
/// output. Minimal fields — everything else `pause` derives.
#[derive(Deserialize, Debug)]
struct CronListEntry {
    id: String,
    cron: String,
    #[serde(default = "default_recurring")]
    recurring: bool,
    prompt: String,
    #[serde(default)]
    cadence_human: Option<String>,
    #[serde(default)]
    inflight_tasks: Vec<InflightTask>,
}

fn default_recurring() -> bool {
    true
}

fn state_dir(override_: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = override_ {
        return Ok(p);
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

fn paused_path(dir: &std::path::Path) -> PathBuf {
    dir.join(".paused-loops.json")
}

fn history_path(dir: &std::path::Path) -> PathBuf {
    dir.join("loop-history.jsonl")
}

fn paused_lock(dir: &std::path::Path) -> PathBuf {
    dir.join(".paused-loops.lock")
}

fn history_lock(dir: &std::path::Path) -> PathBuf {
    dir.join("loop-history.lock")
}

/// AVP-2: 600 perms on state + history (prompts may carry
/// sensitive context).
fn chmod_600(p: &std::path::Path) -> Result<()> {
    let mut perms = fs::metadata(p)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(p, perms)?;
    Ok(())
}

/// Append a single JSON event line to history, under flock.
fn history_append(dir: &std::path::Path, event: &serde_json::Value) -> Result<()> {
    let lock = File::create(history_lock(dir))?;
    lock.lock_exclusive()?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path(dir))?;
    writeln!(f, "{}", serde_json::to_string(event)?)?;
    chmod_600(&history_path(dir))?;
    Ok(())
}

fn cmd_pause(dir: &std::path::Path, label_override: Option<String>) -> Result<()> {
    let mut stdin_buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin_buf)?;
    let entries: Vec<CronListEntry> = serde_json::from_str(&stdin_buf)
        .context("stdin must be a JSON array of CronListEntry — see --help")?;

    if entries.is_empty() {
        // Print empty JSON list so agent doesn't try to CronDelete
        // anything. Matches the skill spec's "no active jobs" path.
        println!("[]");
        return Ok(());
    }

    let now = Utc::now().to_rfc3339();
    let jobs: Vec<PausedJob> = entries
        .into_iter()
        .map(|e| {
            let canary_added = !has_canary(&e.prompt);
            let prompt = if canary_added {
                format!(
                    "{}\n\nNote: if you canceled or stopped this loop, you should NOT be seeing this message.",
                    e.prompt
                )
            } else {
                e.prompt
            };
            let label = label_override
                .clone()
                .unwrap_or_else(|| infer_label(&prompt));
            PausedJob {
                id_original: e.id,
                cron: e.cron,
                cadence_human: e.cadence_human.unwrap_or_else(|| "(unspecified)".into()),
                recurring: e.recurring,
                prompt,
                canary_added,
                paused_at: now.clone(),
                label,
                inflight_tasks: e.inflight_tasks,
            }
        })
        .collect();

    // Merge with prior state if present.
    let pp = paused_path(dir);
    let mut all: Vec<PausedJob> = if pp.exists() {
        let prior_raw = fs::read_to_string(&pp)?;
        serde_json::from_str(&prior_raw).unwrap_or_default()
    } else {
        Vec::new()
    };
    all.extend(jobs);

    // Write under flock.
    let lock = File::create(paused_lock(dir))?;
    lock.lock_exclusive()?;
    fs::write(&pp, serde_json::to_string_pretty(&all)?)?;
    chmod_600(&pp)?;

    // Append one history event per paused job.
    for j in &all {
        history_append(
            dir,
            &serde_json::json!({
                "event": "paused",
                "at": &now,
                "id_original": &j.id_original,
                "cron": &j.cron,
                "label": &j.label,
            }),
        )?;
    }

    // Output the IDs the agent should CronDelete.
    let ids: Vec<&str> = all.iter().map(|j| j.id_original.as_str()).collect();
    println!("{}", serde_json::to_string(&ids)?);
    Ok(())
}

fn cmd_resume(dir: &std::path::Path, interval_override: Option<String>) -> Result<()> {
    let pp = paused_path(dir);
    if !pp.exists() {
        eprintln!("no paused loops to resume");
        println!("[]");
        return Ok(());
    }
    let raw = fs::read_to_string(&pp)?;
    let jobs: Vec<PausedJob> = serde_json::from_str(&raw).context("paused-loops.json malformed")?;
    if jobs.is_empty() {
        println!("[]");
        return Ok(());
    }

    let cron_override = interval_override
        .as_deref()
        .map(interval_to_cron)
        .transpose()?;

    // Output JSON the agent uses to drive CronCreate calls.
    let out: Vec<_> = jobs
        .iter()
        .map(|j| {
            serde_json::json!({
                "cron": cron_override.clone().unwrap_or_else(|| j.cron.clone()),
                "prompt": j.prompt,
                "recurring": j.recurring,
                "label": j.label,
                "id_original": j.id_original,
                "inflight_tasks": j.inflight_tasks.iter().map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "status": t.status,
                        "subject": t.subject,
                        "activeForm": t.active_form,
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    let now = Utc::now().to_rfc3339();
    for j in &jobs {
        history_append(
            dir,
            &serde_json::json!({
                "event": "resumed",
                "at": &now,
                "id_original": &j.id_original,
                "cron": cron_override.clone().unwrap_or_else(|| j.cron.clone()),
                "interval_override": interval_override.clone(),
                "inflight_tasks_replayed": j.inflight_tasks.len(),
            }),
        )?;
    }

    // Consume state file under flock.
    let lock = File::create(paused_lock(dir))?;
    lock.lock_exclusive()?;
    fs::remove_file(&pp)?;

    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn cmd_list(dir: &std::path::Path) -> Result<()> {
    let pp = paused_path(dir);
    let paused: Vec<PausedJob> = if pp.exists() {
        serde_json::from_str(&fs::read_to_string(&pp)?).unwrap_or_default()
    } else {
        Vec::new()
    };
    let hp = history_path(dir);
    let history_tail: Vec<String> = if hp.exists() {
        let raw = fs::read_to_string(&hp)?;
        raw.lines()
            .rev()
            .take(20)
            .map(String::from)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "paused": paused,
            "history_tail": history_tail,
        }))?
    );
    Ok(())
}

fn cmd_history(dir: &std::path::Path, lines: usize) -> Result<()> {
    let hp = history_path(dir);
    if !hp.exists() {
        println!("[]");
        return Ok(());
    }
    let raw = fs::read_to_string(&hp)?;
    let mut tail: Vec<&str> = raw.lines().rev().take(lines).collect();
    tail.reverse();
    for line in tail {
        println!("{}", line);
    }
    Ok(())
}

/// Detect the auto-canary line that /loop-pause adds. Same regexes
/// the skill spec checks for.
fn has_canary(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("cancel") && lower.contains("should not") && lower.contains("see")
        || lower.contains("stop") && lower.contains("should not") && lower.contains("see")
}

/// Infer a short label from the first sentence of the prompt.
fn infer_label(prompt: &str) -> String {
    prompt
        .split(&['.', '\n'][..])
        .next()
        .unwrap_or(prompt)
        .chars()
        .take(60)
        .collect()
}

/// Canonical self-check preamble auto-injected into every loop prompt so an
/// agent treats a fire as "continue, not start", reads TaskList as ground
/// truth, verifies state before acting, and snapshots durable state before
/// compaction. See CLAUDE.md. Kept in ONE place so the wording can't drift.
const SELF_CHECK_PREAMBLE: &str = "THIS IS A SCHEDULED LOOP — self-check before acting. A loop fire is a SIGNAL TO CONTINUE, not a new instruction. FIRST read TaskList (and memory/checkpoint) as ground truth: if a task is in_progress continue it, else work the lowest-ID pending task; never restart finished work; verify state (build logs, deployed artifacts) before assuming an action is needed. Snapshot durable state (TaskUpdate / memory) before the fire ends in case compaction lands. Then:";

/// True if the prompt already carries the self-check declaration, so `prep`
/// is idempotent — re-prepping never stacks preambles.
fn already_prepped(prompt: &str) -> bool {
    prompt.to_lowercase().contains("this is a scheduled loop")
}

/// Prepend the self-check preamble to a loop prompt unless already present.
fn inject_self_check(prompt: &str) -> String {
    if already_prepped(prompt) {
        prompt.trim().to_string()
    } else {
        format!("{SELF_CHECK_PREAMBLE}\n\n{}", prompt.trim())
    }
}

/// `prep`: read a loop prompt (arg or stdin), inject the self-check (and, when
/// `--label` is given, the crash-guard + checkpoint enforcement block), print it.
fn cmd_prep(prompt: Option<String>, label: Option<String>) -> Result<()> {
    let prompt = match prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };
    anyhow::ensure!(
        !prompt.trim().is_empty(),
        "empty prompt — pass --prompt or pipe the loop prompt on stdin"
    );
    let mut out = inject_self_check(&prompt);
    if let Some(label) = label {
        out = inject_guard(&out, &label);
    }
    print!("{}", out);
    Ok(())
}

/// Interval `Nm` / `Nh` / `Nd` → cron expression. Matches the
/// /loop and /loop-resume skill's conversion table.
fn interval_to_cron(s: &str) -> Result<String> {
    let (n_str, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = n_str
        .parse()
        .with_context(|| format!("interval prefix not numeric: {s}"))?;
    let cron = match unit {
        "s" => {
            // Round up to 1m.
            "*/1 * * * *".to_string()
        }
        "m" if n <= 59 => format!("*/{} * * * *", n),
        "m" => {
            let h = n / 60;
            if 24 % h != 0 {
                anyhow::bail!("interval {s} doesn't cleanly divide hours");
            }
            format!("0 */{} * * *", h)
        }
        "h" if n <= 23 => format!("0 */{} * * *", n),
        "d" => format!("0 0 */{} * *", n),
        _ => anyhow::bail!("unsupported interval unit in {s}"),
    };
    Ok(cron)
}

// ---------------------------------------------------------------------------
// Loop crash-guard + additive per-iteration checkpointing.
//
// Two behaviours the user requires of every long-running loop:
//   1. STOP when a previous fire died on an API error (don't iterate into it).
//   2. SAVE STATE on every iteration, additively (never overwrite a checkpoint).
// Plus a single canonical ENTRYPOINT.md so a fresh/recovered session knows
// exactly where to look.
//
// Design: a per-label append-only status journal of {start, done, halt}
// events. A "start" with no matching "done" can ONLY mean the fire that wrote
// it never finished — and since crons fire only while the REPL is idle, the
// previous fire is definitively dead (an API error / crash), not merely slow.
// That unmatched "start" is the stop signal.
// ---------------------------------------------------------------------------

/// One event in a loop's status journal.
#[derive(Deserialize, Debug)]
struct LoopEvent {
    event: String,
    #[serde(default)]
    iter: u64,
    /// RFC-3339 timestamp the event was written (used by the watchdog to age out
    /// an unmatched "start"). Optional for backward-compat with older journals.
    #[serde(default)]
    at: Option<String>,
}

fn loop_ckpt_root(dir: &Path) -> PathBuf {
    dir.join(".loop-checkpoints")
}

/// Sanitize a label so it can never escape the checkpoints dir.
fn sanitize_label(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Slashes are already gone (mapped to '_'), so the result is a single path
    // component. The only remaining traversal risk is a component of exactly
    // "." or ".." (dots are allowed in labels), so collapse those to a safe name.
    if s.is_empty() || s == "." || s == ".." {
        "loop".to_string()
    } else {
        s
    }
}

fn loop_label_dir(dir: &Path, label: &str) -> PathBuf {
    loop_ckpt_root(dir).join(sanitize_label(label))
}

/// Local kill-switch sentinel. Set by `watchdog` (an OS-timer-driven, no-API
/// process) when a fire is detected stuck/blocked, and honored by `guard` so the
/// loop stays stopped even when the agent itself is fully API-blocked and cannot
/// run anything. Cleared by `guard --reset`.
fn disabled_path(dir: &Path, label: &str) -> PathBuf {
    loop_label_dir(dir, label).join("DISABLED")
}

/// Last non-empty event in the status journal, or None if absent/empty.
fn last_event(path: &Path) -> Result<Option<LoopEvent>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    match raw.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(l) => Ok(Some(
            serde_json::from_str(l).context("status.jsonl line malformed")?,
        )),
        None => Ok(None),
    }
}

/// Append one event to a label's status journal under flock.
fn status_append(dir: &Path, label: &str, ev: &serde_json::Value) -> Result<()> {
    let ld = loop_label_dir(dir, label);
    fs::create_dir_all(&ld)?;
    let lock = File::create(ld.join(".status.lock"))?;
    lock.lock_exclusive()?;
    let sp = ld.join("status.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&sp)?;
    writeln!(f, "{}", serde_json::to_string(ev)?)?;
    chmod_600(&sp)?;
    Ok(())
}

/// Pure decision: given the last journal event, should this fire continue or
/// halt, and what iteration number is it? Factored out for testing.
#[derive(Debug, PartialEq)]
struct GuardOutcome {
    halt: bool,
    iter: u64,
    /// True when a prior halt/open-start is being deliberately cleared.
    log_reset: bool,
}

fn guard_decision(last: Option<&LoopEvent>, reset: bool) -> GuardOutcome {
    match last {
        None => GuardOutcome {
            halt: false,
            iter: 1,
            log_reset: false,
        },
        Some(e) => match e.event.as_str() {
            // Previous fire completed cleanly (or was explicitly reset) → continue.
            "done" | "reset" => GuardOutcome {
                halt: false,
                iter: e.iter + 1,
                log_reset: false,
            },
            // Unmatched "start" → previous fire died mid-flight (API error/crash).
            "start" => {
                if reset {
                    GuardOutcome {
                        halt: false,
                        iter: e.iter + 1,
                        log_reset: true,
                    }
                } else {
                    GuardOutcome {
                        halt: true,
                        iter: e.iter,
                        log_reset: false,
                    }
                }
            }
            // Already halted → stay halted unless explicitly re-armed.
            "halt" => {
                if reset {
                    GuardOutcome {
                        halt: false,
                        iter: e.iter + 1,
                        log_reset: true,
                    }
                } else {
                    GuardOutcome {
                        halt: true,
                        iter: e.iter,
                        log_reset: false,
                    }
                }
            }
            // Unknown event type → fail safe by continuing.
            _ => GuardOutcome {
                halt: false,
                iter: e.iter + 1,
                log_reset: false,
            },
        },
    }
}

/// `guard`: crash-check at the START of a fire.
fn cmd_guard(dir: &Path, label: String, reset: bool) -> Result<()> {
    let ld = loop_label_dir(dir, &label);
    fs::create_dir_all(&ld)?;
    let sp = ld.join("status.jsonl");
    let dp = disabled_path(dir, &label);
    // A deliberate re-arm clears any watchdog DISABLED sentinel first.
    if reset && dp.exists() {
        fs::remove_file(&dp).ok();
    }
    let last = last_event(&sp)?;
    // Honor the watchdog's local kill-switch: if DISABLED is set (and not being
    // reset), the loop stays stopped regardless of journal state. This is the
    // backstop for a TOTAL API block — the agent can't run anything that fire, so
    // the OS-timer watchdog sets DISABLED and the local pre-fire gate reads it.
    if dp.exists() {
        let reason =
            "loop DISABLED by the watchdog (a prior fire was blocked/crashed) — re-arm with --reset";
        write_entrypoint(
            dir,
            &label,
            "HALTED",
            last.as_ref().map(|e| e.iter).unwrap_or(0),
            reason,
        )?;
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"action":"halt","reason":reason,"label":label})
            )?
        );
        std::process::exit(3);
    }
    let prev_event = last.as_ref().map(|e| e.event.clone());
    let outcome = guard_decision(last.as_ref(), reset);
    let now = Utc::now().to_rfc3339();

    if outcome.halt {
        let reason = if prev_event.as_deref() == Some("halt") {
            "loop is HALTED from a prior crash and was not re-armed (pass --reset to resume)"
        } else {
            "previous iteration started but never completed — likely an API error / crash mid-fire"
        };
        // Record the halt transition once (don't spam on repeated fires).
        if prev_event.as_deref() == Some("start") {
            status_append(
                dir,
                &label,
                &serde_json::json!({"event":"halt","at":&now,"iter":outcome.iter,"reason":reason}),
            )?;
        }
        write_entrypoint(dir, &label, "HALTED", outcome.iter, reason)?;
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "action":"halt","iter":outcome.iter,"reason":reason,"label":label
            }))?
        );
        std::process::exit(3);
    }

    if outcome.log_reset {
        status_append(
            dir,
            &label,
            &serde_json::json!({"event":"reset","at":&now,"iter":outcome.iter - 1}),
        )?;
    }
    status_append(
        dir,
        &label,
        &serde_json::json!({"event":"start","at":&now,"iter":outcome.iter}),
    )?;
    write_entrypoint(
        dir,
        &label,
        "running",
        outcome.iter,
        &format!("iteration {} started", outcome.iter),
    )?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "action":"continue","iter":outcome.iter,"label":label
        }))?
    );
    Ok(())
}

/// Pure: is the loop stuck? True iff the last journal event is an unmatched
/// "start" whose age exceeds max_age_secs (a fire that began but never wrote its
/// "done" — the signature of an agent that was blocked/crashed mid-fire).
fn watchdog_stuck(last: Option<&LoopEvent>, age_secs: i64, max_age_secs: u64) -> bool {
    matches!(last, Some(e) if e.event == "start") && age_secs > max_age_secs as i64
}

/// `watchdog`: OS-timer-driven local kill-switch (NO API). See the enum doc.
fn cmd_watchdog(dir: &Path, label: String, max_age_secs: u64) -> Result<()> {
    let ld = loop_label_dir(dir, &label);
    let sp = ld.join("status.jsonl");
    let dp = disabled_path(dir, &label);
    if dp.exists() {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"status":"already-disabled","label":label}))?
        );
        return Ok(());
    }
    let last = last_event(&sp)?;
    // Age of the last event — only meaningful when it is an unmatched "start".
    let age_secs: i64 = last
        .as_ref()
        .and_then(|e| e.at.as_deref())
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|dt| {
            Utc::now()
                .signed_duration_since(dt.with_timezone(&Utc))
                .num_seconds()
        })
        .unwrap_or(0);

    if watchdog_stuck(last.as_ref(), age_secs, max_age_secs) {
        let iter = last.as_ref().map(|e| e.iter).unwrap_or(0);
        let reason = format!(
            "watchdog: fire iter {} started {}s ago and never completed (> {}s) — agent blocked/crashed; loop DISABLED locally",
            iter, age_secs, max_age_secs
        );
        fs::create_dir_all(&ld)?;
        status_append(
            dir,
            &label,
            &serde_json::json!({"event":"halt","at":Utc::now().to_rfc3339(),"iter":iter,"reason":&reason}),
        )?;
        fs::write(&dp, format!("{reason}\n"))?;
        chmod_600(&dp)?;
        write_entrypoint(dir, &label, "HALTED", iter, &reason)?;
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"status":"DISABLED","iter":iter,"age_secs":age_secs,"reason":reason,"label":label})
            )?
        );
        std::process::exit(3);
    }
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"status":"healthy","label":label,"age_secs":age_secs})
        )?
    );
    Ok(())
}

/// `checkpoint`: additive save-state at the END of a fire.
fn cmd_checkpoint(dir: &Path, label: String, note: Option<String>) -> Result<()> {
    let ld = loop_label_dir(dir, &label);
    fs::create_dir_all(&ld)?;
    let sp = ld.join("status.jsonl");
    let iter = last_event(&sp)?.map(|e| e.iter).unwrap_or(0);

    // Optional full state body on stdin (skip if attached to a terminal so an
    // interactive invocation never hangs waiting for input).
    let mut body = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut body).ok();
    }

    let now = Utc::now();
    let stamp = now.format("%Y%m%d-%H%M%SZ").to_string();
    let nowrfc = now.to_rfc3339();
    let fname = format!("checkpoint-{stamp}.md");
    let fpath = ld.join(&fname);

    let mut content = format!("# checkpoint {nowrfc} — label={label} iter={iter}\n\n");
    if let Some(n) = &note {
        content.push_str(&format!("**{}**\n\n", n));
    }
    if !body.trim().is_empty() {
        content.push_str(body.trim());
        content.push('\n');
    }
    // Never clobber an existing checkpoint (same-second collision guard).
    let fpath = if fpath.exists() {
        ld.join(format!("checkpoint-{stamp}-{}.md", std::process::id()))
    } else {
        fpath
    };
    fs::write(&fpath, content)?;
    chmod_600(&fpath)?;

    status_append(
        dir,
        &label,
        &serde_json::json!({"event":"done","at":&nowrfc,"iter":iter,"note":note}),
    )?;
    append_index(
        &ld,
        &nowrfc,
        &fpath.file_name().unwrap().to_string_lossy(),
        note.as_deref().unwrap_or(""),
    )?;
    write_entrypoint(
        dir,
        &label,
        "idle",
        iter,
        &format!(
            "iter {iter} checkpointed -> {}",
            fpath.file_name().unwrap().to_string_lossy()
        ),
    )?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "checkpoint": fpath.file_name().unwrap().to_string_lossy(),
            "iter": iter,
            "path": fpath.to_string_lossy(),
        }))?
    );
    Ok(())
}

/// Append a row to a label's append-only INDEX.md (creates header once).
fn append_index(ld: &Path, at: &str, fname: &str, note: &str) -> Result<()> {
    let ip = ld.join("INDEX.md");
    let header_needed = !ip.exists();
    let lock = File::create(ld.join(".index.lock"))?;
    lock.lock_exclusive()?;
    let mut f = OpenOptions::new().create(true).append(true).open(&ip)?;
    if header_needed {
        writeln!(
            f,
            "# Checkpoint Index (append-only — newest at bottom)\n\n| at | file | note |\n|---|---|---|"
        )?;
    }
    writeln!(f, "| {} | {} | {} |", at, fname, note.replace('|', "\\|"))?;
    chmod_600(&ip)?;
    Ok(())
}

/// Rewrite the single canonical recovery pointer. This file is derived state
/// (a pointer), so overwriting it each tick is correct — the checkpoints it
/// points AT are the additive, never-overwritten source of truth.
fn write_entrypoint(dir: &Path, label: &str, status: &str, iter: u64, detail: &str) -> Result<()> {
    let root = loop_ckpt_root(dir);
    fs::create_dir_all(&root)?;
    let ep = root.join("ENTRYPOINT.md");
    let now = Utc::now().to_rfc3339();
    let ld = loop_label_dir(dir, label);
    let content = format!(
        "# LOOP RECOVERY ENTRYPOINT — read this FIRST on any resume\n\n\
Generated: {now}\n\
Loop label: `{label}`\n\
Status: **{status}** (iteration {iter}) — {detail}\n\n\
## Where to look (in order)\n\
1. Latest full checkpoint: `ls {ld}/checkpoint-*.md | sort | tail -1`\n\
2. Per-iteration index:    `{ld}/INDEX.md`\n\
3. Status journal:         `{ld}/status.jsonl` (last line = current position)\n\
4. All loops:              `ls {root}/`\n\n\
## Rule\n\
If Status is **HALTED**, the loop stopped because a previous fire died (likely an\n\
API error). Do NOT re-arm blindly: read the latest checkpoint, fix the cause, then\n\
re-arm — the first fire after re-arming runs `claude-loop guard --label {label} --reset`.\n",
        now = now,
        label = label,
        status = status,
        iter = iter,
        detail = detail,
        ld = ld.display(),
        root = root.display(),
    );
    fs::write(&ep, content)?;
    chmod_600(&ep)?;
    Ok(())
}

/// The crash-guard + checkpoint enforcement block injected into a loop prompt
/// when `prep --label` is given. Bound to a concrete label so the agent runs
/// the right state namespace.
fn guard_block(label: &str) -> String {
    format!(
        "LOOP CRASH-GUARD + PER-ITERATION CHECKPOINTING (enforced):\n\
- STEP 0 (before anything else): run `claude-loop guard --label {label}`. If it prints \"action\":\"halt\" OR exits non-zero, the PREVIOUS fire did not complete — likely an API error. Do NOT continue: CronDelete this job and STOP, then report. (Deliberately re-arm later with `--reset`.)\n\
- HARD STOP on ANY API error this fire — ESPECIALLY a usage-policy / cybersecurity-classifier block. The instant you hit one, STOP: CronDelete this job and do not retry. Re-firing a blocked loop compounds the block and prevents the user from working. One API error = stop the loop, full stop.\n\
- LAST STEP (every fire, even idle ticks): pipe a short state summary into `claude-loop checkpoint --label {label} --note \"<one line: what changed>\"`. State is saved ADDITIVELY every iteration; never skip it.\n",
        label = label
    )
}

fn already_guarded(prompt: &str) -> bool {
    prompt.contains("claude-loop guard --label")
}

fn inject_guard(prompt: &str, label: &str) -> String {
    if already_guarded(prompt) {
        prompt.to_string()
    } else {
        format!("{}\n{}", guard_block(label), prompt)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = state_dir(cli.state_dir.clone())?;
    fs::create_dir_all(&dir)?;
    match cli.cmd {
        Cmd::Pause { label } => cmd_pause(&dir, label),
        Cmd::Resume { interval } => cmd_resume(&dir, interval),
        Cmd::List => cmd_list(&dir),
        Cmd::History { lines } => cmd_history(&dir, lines),
        Cmd::Prep { prompt, label } => cmd_prep(prompt, label),
        Cmd::Guard { label, reset } => cmd_guard(&dir, label, reset),
        Cmd::Checkpoint { label, note } => cmd_checkpoint(&dir, label, note),
        Cmd::Watchdog {
            label,
            max_age_secs,
        } => cmd_watchdog(&dir, label, max_age_secs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_5m() {
        assert_eq!(interval_to_cron("5m").unwrap(), "*/5 * * * *");
    }
    #[test]
    fn interval_2h() {
        assert_eq!(interval_to_cron("2h").unwrap(), "0 */2 * * *");
    }
    #[test]
    fn interval_1d() {
        assert_eq!(interval_to_cron("1d").unwrap(), "0 0 */1 * *");
    }
    #[test]
    fn interval_60m_rounds_to_1h() {
        assert_eq!(interval_to_cron("60m").unwrap(), "0 */1 * * *");
    }
    #[test]
    fn interval_120m_to_2h() {
        assert_eq!(interval_to_cron("120m").unwrap(), "0 */2 * * *");
    }
    #[test]
    fn canary_detects_standard_line() {
        assert!(has_canary(
            "do stuff. if you canceled or stopped this loop, you should NOT be seeing this message."
        ));
    }
    #[test]
    fn canary_absent_on_bare_prompt() {
        assert!(!has_canary("do stuff every 5 minutes"));
    }
    #[test]
    fn infer_label_truncates_to_60() {
        let s = "a".repeat(100);
        assert_eq!(infer_label(&s).len(), 60);
    }
    #[test]
    fn infer_label_first_sentence() {
        assert_eq!(infer_label("First. Second."), "First");
    }
    #[test]
    fn prep_injects_when_absent() {
        let out = inject_self_check("draft the next page every 30m");
        assert!(out.starts_with("THIS IS A SCHEDULED LOOP"));
        assert!(out.contains("draft the next page every 30m"));
    }
    #[test]
    fn prep_idempotent_when_present() {
        let already = "THIS IS A SCHEDULED LOOP — self-check.\n\ndo the thing";
        assert_eq!(inject_self_check(already), already);
    }
    #[test]
    fn prep_detects_declaration_case_insensitively() {
        assert!(already_prepped("this is a scheduled loop, continue"));
        assert!(!already_prepped("a normal prompt"));
    }

    // ---- crash-guard + checkpoint ----

    fn ev(event: &str, iter: u64) -> LoopEvent {
        LoopEvent {
            event: event.to_string(),
            iter,
            at: None,
        }
    }

    #[test]
    fn watchdog_flags_stale_unmatched_start() {
        // A "start" older than the max age = a fire that began but never finished.
        assert!(watchdog_stuck(Some(&ev("start", 4)), 2000, 1800));
    }

    #[test]
    fn watchdog_ignores_recent_start() {
        // A "start" within the window is a normal in-progress fire, not stuck.
        assert!(!watchdog_stuck(Some(&ev("start", 4)), 30, 1800));
    }

    #[test]
    fn watchdog_ignores_completed_and_empty() {
        // A clean "done" (even if old) is not stuck; neither is an empty journal.
        assert!(!watchdog_stuck(Some(&ev("done", 4)), 999999, 1800));
        assert!(!watchdog_stuck(Some(&ev("halt", 4)), 999999, 1800));
        assert!(!watchdog_stuck(None, 999999, 1800));
    }

    #[test]
    fn guard_fresh_journal_continues_at_iter_1() {
        let o = guard_decision(None, false);
        assert_eq!(
            o,
            GuardOutcome {
                halt: false,
                iter: 1,
                log_reset: false
            }
        );
    }

    #[test]
    fn guard_after_done_continues_next_iter() {
        let o = guard_decision(Some(&ev("done", 3)), false);
        assert!(!o.halt);
        assert_eq!(o.iter, 4);
    }

    #[test]
    fn guard_unmatched_start_halts() {
        // The core stop-on-API-error rule: a "start" with no "done" means the
        // previous fire died mid-flight.
        let o = guard_decision(Some(&ev("start", 5)), false);
        assert!(o.halt);
        assert_eq!(o.iter, 5);
    }

    #[test]
    fn guard_reset_resumes_from_open_start() {
        let o = guard_decision(Some(&ev("start", 5)), true);
        assert!(!o.halt);
        assert_eq!(o.iter, 6);
        assert!(o.log_reset);
    }

    #[test]
    fn guard_stays_halted_without_reset() {
        let o = guard_decision(Some(&ev("halt", 2)), false);
        assert!(o.halt);
        assert_eq!(o.iter, 2);
    }

    #[test]
    fn guard_reset_clears_halt() {
        let o = guard_decision(Some(&ev("halt", 2)), true);
        assert!(!o.halt);
        assert_eq!(o.iter, 3);
        assert!(o.log_reset);
    }

    #[test]
    fn sanitize_label_blocks_traversal() {
        // Slashes map to '_', so the result is a single, contained component.
        assert_eq!(sanitize_label("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_label("ok.label-1_2"), "ok.label-1_2");
        assert_eq!(sanitize_label(""), "loop");
        // A standalone "." / ".." component would still escape → collapsed.
        assert_eq!(sanitize_label(".."), "loop");
        assert_eq!(sanitize_label("."), "loop");
    }

    #[test]
    fn inject_guard_is_idempotent() {
        let p = "do the work";
        let once = inject_guard(p, "demo");
        assert!(once.contains("claude-loop guard --label demo"));
        assert!(once.contains("CronDelete this job and STOP"));
        assert_eq!(inject_guard(&once, "demo"), once); // no double-inject
    }

    fn unique_tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "claude-loop-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ))
    }

    #[test]
    fn checkpoint_cycle_is_additive_and_tracks_state() {
        let dir = unique_tmp();
        fs::create_dir_all(&dir).unwrap();

        // Two clean fires.
        cmd_guard(&dir, "t".into(), false).unwrap();
        cmd_checkpoint(&dir, "t".into(), Some("first".into())).unwrap();
        cmd_guard(&dir, "t".into(), false).unwrap();
        cmd_checkpoint(&dir, "t".into(), Some("second".into())).unwrap();

        let ld = loop_label_dir(&dir, "t");
        let journal = fs::read_to_string(ld.join("status.jsonl")).unwrap();
        assert_eq!(journal.lines().count(), 4, "start,done,start,done");

        let ckpts = fs::read_dir(&ld)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("checkpoint-"))
            .count();
        assert_eq!(
            ckpts, 2,
            "every iteration writes its own additive checkpoint"
        );

        assert!(ld.join("INDEX.md").exists());
        assert!(loop_ckpt_root(&dir).join("ENTRYPOINT.md").exists());

        // The journal's last event is the most recent "done" → next guard continues.
        let next = guard_decision(
            last_event(&ld.join("status.jsonl")).unwrap().as_ref(),
            false,
        );
        assert!(!next.halt);
        assert_eq!(next.iter, 3);

        let _ = fs::remove_dir_all(&dir);
    }
}
