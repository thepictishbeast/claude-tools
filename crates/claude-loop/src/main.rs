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

    /// API-error sentinel — run from an OS timer (cron/systemd), NOT the agent.
    /// NO API, so it works even when the agent is fully API-blocked. Scans the
    /// most-recently-active Claude Code transcript(s) for an API error —
    /// especially the usage-policy / cybersecurity-classifier block that
    /// silently halts a loop — and on FIRST detection STOPS every running loop,
    /// backs up the full conversation transcript + a RESUME.md, and notifies
    /// (desktop toast + optional email). Idempotent + recency-gated. Run once
    /// with `--setup` to set the notification email.
    Sentinel {
        /// Configure notification prefs (prompts for email if interactive), then exit.
        #[arg(long)]
        setup: bool,
        /// Set the notification email non-interactively (implies email enabled).
        #[arg(long)]
        email: Option<String>,
        /// Disable email notification (desktop toast stays on).
        #[arg(long)]
        no_email: bool,
        /// Only act on a transcript modified within this many minutes (recency gate).
        #[arg(long, default_value_t = 10)]
        within_mins: i64,
        /// KiB of each transcript's tail to scan for the error (transcripts can be huge).
        #[arg(long, default_value_t = 512)]
        tail_kb: u64,
        /// Read-only audit: classify every recent transcript's API-error and print
        /// a report. Acts on nothing, emails nothing, exits 0. Use a wide
        /// --within-mins to sweep history (`sentinel --scan-only --within-mins 100000`).
        #[arg(long)]
        scan_only: bool,
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

// ---------------------------------------------------------------------------
// sentinel — API-error sentinel. OS-timer-driven (NO API): scans the most
// recently-active Claude Code transcript(s) for an API error — especially the
// usage-policy / cybersecurity-classifier block that silently halts a loop —
// and on FIRST detection: (1) STOPS every running loop (DISABLED + HALT, the
// same machinery the watchdog uses), (2) backs up the FULL conversation
// transcript (the raw JSONL Claude Code writes every turn — present whether or
// not checkpoint/rewind was used) + a RESUME.md, (3) notifies via desktop toast
// + optional email. Idempotent (per-incident marker) + recency-gated so it
// never re-fires on a historical error or acts on a stale transcript.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug)]
struct SentinelConfig {
    /// Notification email; None = none on file.
    #[serde(default)]
    email: Option<String>,
    /// Master switch for email (opt-out even with an address on file).
    #[serde(default)]
    email_enabled: bool,
    /// Desktop toast on/off (best-effort; needs a desktop session).
    #[serde(default = "default_true")]
    desktop_enabled: bool,
    /// Extra transcript roots beyond `<state-dir>/projects` (e.g. another
    /// user's `~/.claude/projects` when loops run under a different account).
    #[serde(default)]
    extra_project_roots: Vec<PathBuf>,
    /// Ignore blocks in automated sub-agent / SDK sessions (entrypoint sdk-*).
    /// Those are transient sub-calls whose parent session owns its own errors;
    /// alerting on each one is noise (the original spam was sdk-py sub-sessions).
    /// Default true.
    #[serde(default = "default_true")]
    ignore_sdk: bool,
    /// Global alert-email cooldown (minutes): after one alert email, suppress
    /// further alert EMAILS for this long. Blocks are still backed up + loops
    /// stopped silently — only the EMAIL is rate-limited, so a recurring block
    /// can't spam. Default 120.
    #[serde(default = "default_cooldown")]
    cooldown_mins: i64,
}
fn default_true() -> bool {
    true
}
fn default_cooldown() -> i64 {
    120
}
impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            email: None,
            email_enabled: false,
            desktop_enabled: true,
            extra_project_roots: vec![],
            ignore_sdk: true,
            cooldown_mins: 120,
        }
    }
}

fn sentinel_config_path(dir: &Path) -> PathBuf {
    dir.join(".sentinel.json")
}
fn load_sentinel_config(dir: &Path) -> SentinelConfig {
    fs::read_to_string(sentinel_config_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_sentinel_config(dir: &Path, c: &SentinelConfig) -> Result<()> {
    let p = sentinel_config_path(dir);
    fs::write(&p, serde_json::to_string_pretty(c)?)?;
    chmod_600(&p)?;
    Ok(())
}

/// Distinctive message-text signatures of the SPECIFIC block this sentinel
/// exists for (matched lowercased): the cybersecurity-safeguard block
/// ("...safeguards flagged this message for a cybersecurity topic ... apply for
/// an exemption: .../cyber-use-case") and the usage-policy block. Deliberately
/// multi-word PHRASES, never broad single words — transient errors (overloaded,
/// rate-limit, network, auth, malformed) must NOT match or the sentinel spams.
const POLICY_SIGNATURES: &[&str] = &[
    "cybersecurity topic", // robust to "flagged this message for a cybersecurity topic" / "flagged for a cybersecurity topic"
    "cyber-use-case",      // the exemption-form URL fragment — present in every cyber block
    "flagged this message for a cybersecurity",
    "unable to respond to this request",
    "violate our usage policy",
];

struct ApiErrorHit {
    transcript: PathBuf,
    session_id: String,
    /// Claude Code entrypoint that hit the error: "cli" (interactive / loop
    /// fire) vs "sdk-py"/"sdk-cli" (automated sub-agent / SDK call).
    entrypoint: String,
    error_code: String,
    message: String,
    is_policy_block: bool,
    /// Stable per-incident key (session id + hash of the matched line).
    key: String,
}

/// Read at most the last `max_bytes` of a (possibly huge — 500MB+) transcript.
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max_bytes))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Recursively pull the first `"text"` string out of a transcript entry.
fn extract_error_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get("text") {
                return Some(s.clone());
            }
            m.values().find_map(extract_error_text)
        }
        serde_json::Value::Array(a) => a.iter().find_map(extract_error_text),
        _ => None,
    }
}

/// FNV-1a — a dep-free stable hash for the idempotency key.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Parse one transcript line. If it is an API-error entry
/// (`isApiErrorMessage:true`), return `(error_code, message, is_policy_block,
/// entrypoint)` — classifying policy/cyber vs every other (transient) error.
/// Shared by the live detector and the read-only `--scan-only` audit report.
fn classify_error_line(line: &str) -> Option<(String, String, bool, String)> {
    if !line.contains("\"isApiErrorMessage\":true") && !line.contains("\"isApiErrorMessage\": true") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let error_code = v
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("api_error")
        .to_string();
    let message = extract_error_text(&v).unwrap_or_else(|| error_code.clone());
    let low = message.to_lowercase();
    let is_policy = POLICY_SIGNATURES.iter().any(|s| low.contains(s));
    let entrypoint = v
        .get("entrypoint")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .to_string();
    Some((error_code, message, is_policy, entrypoint))
}

/// Pure cooldown check (factored out for testing): true if `last_iso` is within
/// `cooldown_mins` of `now` — i.e. an alert email was sent recently, so suppress.
fn email_in_cooldown(last_iso: Option<&str>, cooldown_mins: i64, now: chrono::DateTime<Utc>) -> bool {
    last_iso
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|t| now.signed_duration_since(t.with_timezone(&Utc)).num_minutes() < cooldown_mins)
        .unwrap_or(false)
}

/// Scan a transcript's tail for an API-error entry; classify policy-block vs
/// generic. Returns the most recent (closest to the tail) hit, or None.
fn detect_api_error(transcript: &Path, tail_kb: u64) -> Option<ApiErrorHit> {
    let raw = read_tail(transcript, tail_kb * 1024)?;
    for line in raw.lines().rev() {
        if !line.contains("\"isApiErrorMessage\":true")
            && !line.contains("\"isApiErrorMessage\": true")
        {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // partial first line from the byte-bounded tail
        };
        let error_code = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("api_error")
            .to_string();
        let message = extract_error_text(&v).unwrap_or_else(|| error_code.clone());
        let low = message.to_lowercase();
        let is_policy_block = POLICY_SIGNATURES.iter().any(|s| low.contains(s));
        // This sentinel exists for the policy / cyber-safeguard block ONLY.
        // Every other API error (overloaded, rate-limit, network, auth,
        // malformed) is transient / self-recovering and must NEVER alert —
        // skip it and keep scanning earlier lines for a real policy block.
        if !is_policy_block {
            continue;
        }
        let entrypoint = v
            .get("entrypoint")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = transcript
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // `key` becomes a marker FILENAME, so derive it from a path-sanitized
        // form of the session id (raw id kept for display / `claude --resume`).
        let key = format!("{}-{:016x}", sanitize_label(&session_id), fnv1a(line));
        return Some(ApiErrorHit {
            transcript: transcript.to_path_buf(),
            session_id,
            entrypoint,
            error_code,
            message,
            is_policy_block,
            key,
        });
    }
    None
}

fn projects_roots(dir: &Path, cfg: &SentinelConfig) -> Vec<PathBuf> {
    let mut roots = vec![dir.join("projects")];
    roots.extend(cfg.extra_project_roots.iter().cloned());
    roots
}

/// Transcripts (`<root>/<slug>/*.jsonl`) modified within `within_mins`.
fn recent_transcripts(roots: &[PathBuf], within_mins: i64) -> Vec<PathBuf> {
    let cutoff = Utc::now().timestamp() - within_mins.max(0) * 60;
    let mut out = vec![];
    for root in roots {
        let Ok(slugs) = fs::read_dir(root) else {
            continue;
        };
        for slug in slugs.flatten() {
            let Ok(files) = fs::read_dir(slug.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let mtime = f
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if mtime >= cutoff {
                    out.push((mtime, p));
                }
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    out.into_iter().map(|(_, p)| p).collect()
}

/// Stop every loop whose journal shows an unmatched "start" (a fire in flight),
/// using the same DISABLED + HALT sentinel the watchdog writes.
fn stop_all_running_loops(dir: &Path, hit: &ApiErrorHit) -> Result<Vec<String>> {
    let root = loop_ckpt_root(dir);
    let mut stopped = vec![];
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(stopped);
    };
    for e in entries.flatten() {
        if !e.path().is_dir() {
            continue;
        }
        let label = e.file_name().to_string_lossy().to_string();
        if label.starts_with('.') {
            continue; // skip .sentinel-actioned etc.
        }
        let last = last_event(&e.path().join("status.jsonl"))?;
        let running = matches!(last.as_ref(), Some(ev) if ev.event == "start");
        let dp = disabled_path(dir, &label);
        if !running || dp.exists() {
            continue;
        }
        let iter = last.as_ref().map(|ev| ev.iter).unwrap_or(0);
        let reason = format!(
            "sentinel: API error ({}) in session {} — loop DISABLED so it can't iterate into the block{}",
            hit.error_code,
            hit.session_id,
            if hit.is_policy_block { " (usage-policy / classifier block)" } else { "" }
        );
        status_append(
            dir,
            &label,
            &serde_json::json!({"event":"halt","at":Utc::now().to_rfc3339(),"iter":iter,"reason":&reason}),
        )?;
        fs::write(&dp, format!("{reason}\n"))?;
        chmod_600(&dp).ok();
        write_entrypoint(dir, &label, "HALTED", iter, &reason)?;
        stopped.push(label);
    }
    Ok(stopped)
}

/// Copy the full transcript + a RESUME.md into a fresh incident dir. Works
/// without checkpoint/rewind because the transcript is the raw conversation.
fn backup_conversation(dir: &Path, hit: &ApiErrorHit, stamp: &str) -> Result<PathBuf> {
    let bdir = loop_ckpt_root(dir).join(format!("api-incident-{stamp}"));
    fs::create_dir_all(&bdir)?;
    let dest = bdir.join("transcript.jsonl");
    fs::copy(&hit.transcript, &dest).ok();
    chmod_600(&dest).ok();
    let resume = bdir.join("RESUME.md");
    let body = format!(
        "# API-error incident — {stamp}\n\n\
A loop/session hit an API error{policy}.\n\n\
- **Error code:** `{code}`\n\
- **Message:** {msg}\n\
- **Session id:** `{sid}`\n\
- **Original transcript:** `{orig}`\n\
- **Full backup copy:** `transcript.jsonl` (this dir)\n\n\
## Resume (canonical)\n\
Re-attach to the original session — the entire conversation comes back:\n\n\
```sh\nclaude --resume {sid}\n```\n\n\
If that session is gone, `transcript.jsonl` here is the complete raw\n\
conversation (every turn Claude Code wrote, independent of checkpoint/rewind).\n\
Open it or feed it to a fresh session to review + continue.\n\n\
## Why the loop stopped\n\
The sentinel DISABLED all running loops so they don't iterate into the same\n\
block. Re-arm a fixed loop with `claude-loop guard --label <L> --reset`.\n",
        policy = if hit.is_policy_block { " (usage-policy / cybersecurity-classifier block)" } else { "" },
        code = hit.error_code,
        msg = hit.message.replace('\n', " "),
        sid = hit.session_id,
        orig = hit.transcript.display(),
    );
    fs::write(&resume, body)?;
    chmod_600(&resume).ok();
    Ok(bdir)
}

/// Best-effort desktop toast (Linux/macOS/Windows). Never fails the run.
fn desktop_notify(title: &str, body: &str) -> bool {
    use std::process::Command;
    let ok = |r: std::io::Result<std::process::ExitStatus>| r.map(|s| s.success()).unwrap_or(false);
    match std::env::consts::OS {
        "linux" => ok(Command::new("notify-send")
            .args(["-u", "critical", title, body])
            .status()),
        "macos" => ok(Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{}\" with title \"{}\"",
                    body.replace('"', "'"),
                    title.replace('"', "'")
                ),
            ])
            .status()),
        "windows" => ok(Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "New-BurntToastNotification -Text '{}','{}'",
                    title.replace('\'', "`'"),
                    body.replace('\'', "`'")
                ),
            ])
            .status()),
        _ => false,
    }
}

// --- polished internal email (multipart/alternative HTML + text fallback) ---
// Reusable renderer: nice card layout, inline CSS (email clients strip <style>),
// clear accent header, KV details, a copyable monospace code block, numbered
// steps, and bulletproof link buttons. Not log-dump plaintext.

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "prime".into())
}
/// Pull the first `max` http(s) URLs out of free text (for the links section).
fn first_urls(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for tok in s.split(|c: char| c.is_whitespace()) {
        if tok.starts_with("http://") || tok.starts_with("https://") {
            let u = tok.trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | ']' | '"' | '\'')).to_string();
            if !out.contains(&u) { out.push(u); }
            if out.len() >= max { break; }
        }
    }
    out
}

/// Everything needed to render a polished notification email.
struct MailParts {
    accent: String,                 // header bar color, e.g. "#C0392B"
    kicker: String,                 // small uppercase label
    title: String,                  // headline
    intro: String,                  // one-line summary
    kv: Vec<(String, String)>,      // detail rows (value shown monospace)
    code: Option<(String, String)>, // (label, monospace block — e.g. resume cmd)
    steps: Vec<String>,             // "what to do" numbered list
    links: Vec<(String, String)>,   // (label, url) → buttons
    footer: String,
}

fn render_html(m: &MailParts) -> String {
    let mut kv = String::new();
    if !m.kv.is_empty() {
        kv.push_str("<tr><td style=\"padding:4px 28px 8px;\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">");
        for (k, v) in &m.kv {
            kv.push_str(&format!(
                "<tr><td style=\"padding:5px 0;color:#5b6472;font-size:13px;width:130px;vertical-align:top;\">{}</td><td style=\"padding:5px 0;color:#1a1f2b;font-size:13px;font-family:SFMono-Regular,Consolas,monospace;word-break:break-all;\">{}</td></tr>",
                esc_html(k), esc_html(v)
            ));
        }
        kv.push_str("</table></td></tr>");
    }
    let code = m.code.as_ref().map(|(label, body)| format!(
        "<tr><td style=\"padding:12px 28px;\"><div style=\"color:#5b6472;font-size:12px;margin-bottom:6px;\">{}</div><div style=\"background:#0f1420;color:#c8d3e6;font-family:SFMono-Regular,Consolas,monospace;font-size:13px;padding:12px 14px;border-radius:8px;white-space:pre-wrap;word-break:break-all;\">{}</div></td></tr>",
        esc_html(label), esc_html(body)
    )).unwrap_or_default();
    let steps = if m.steps.is_empty() { String::new() } else {
        let items: String = m.steps.iter().enumerate().map(|(i, s)| format!(
            "<tr><td style=\"padding:4px 8px 4px 0;vertical-align:top;color:{};font-weight:700;font-size:14px;\">{}.</td><td style=\"padding:4px 0;color:#1a1f2b;font-size:14px;line-height:1.5;\">{}</td></tr>",
            m.accent, i + 1, esc_html(s)
        )).collect();
        format!("<tr><td style=\"padding:8px 28px;\"><div style=\"color:#5b6472;font-size:12px;text-transform:uppercase;letter-spacing:.08em;margin-bottom:8px;\">What to do</div><table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\">{items}</table></td></tr>")
    };
    let links = if m.links.is_empty() { String::new() } else {
        let btns: String = m.links.iter().map(|(label, url)| format!(
            "<a href=\"{}\" style=\"display:inline-block;margin:6px 8px 6px 0;padding:9px 16px;background:{};color:#ffffff;text-decoration:none;border-radius:8px;font-size:13px;font-weight:600;\">{}</a>",
            esc_html(url), m.accent, esc_html(label)
        )).collect();
        format!("<tr><td style=\"padding:6px 28px 12px;\">{btns}</td></tr>")
    };
    format!(
        "<!doctype html><html><body style=\"margin:0;padding:0;background:#f4f5f7;\">\
<div style=\"display:none;max-height:0;overflow:hidden;opacity:0;\">{intro}</div>\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f4f5f7;padding:24px 12px;\"><tr><td align=\"center\">\
<table role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:600px;max-width:600px;background:#ffffff;border-radius:12px;overflow:hidden;box-shadow:0 1px 3px rgba(16,24,40,.1);font-family:-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\">\
<tr><td style=\"background:{accent};padding:20px 28px;\"><div style=\"color:rgba(255,255,255,.82);font-size:11px;letter-spacing:.12em;text-transform:uppercase;font-weight:700;\">{kicker}</div><div style=\"color:#ffffff;font-size:21px;font-weight:700;margin-top:5px;line-height:1.25;\">{title}</div></td></tr>\
<tr><td style=\"padding:22px 28px 6px;color:#1a1f2b;font-size:15px;line-height:1.55;\">{introhtml}</td></tr>\
{kv}{code}{steps}{links}\
<tr><td style=\"padding:16px 28px 24px;border-top:1px solid #eceef2;color:#8a94a6;font-size:12px;line-height:1.5;\">{footer}</td></tr>\
</table></td></tr></table></body></html>",
        accent = m.accent, kicker = esc_html(&m.kicker), title = esc_html(&m.title),
        intro = esc_html(&m.intro), introhtml = esc_html(&m.intro),
        kv = kv, code = code, steps = steps, links = links, footer = esc_html(&m.footer),
    )
}

fn render_text(m: &MailParts) -> String {
    let mut o = format!("{}\n{}\n\n{}\n", m.kicker, m.title, m.intro);
    for (k, v) in &m.kv { o.push_str(&format!("  {k}: {v}\n")); }
    if let Some((label, body)) = &m.code { o.push_str(&format!("\n{label}:\n    {body}\n")); }
    if !m.steps.is_empty() {
        o.push_str("\nWhat to do:\n");
        for (i, s) in m.steps.iter().enumerate() { o.push_str(&format!("  {}. {}\n", i + 1, s)); }
    }
    for (label, url) in &m.links { o.push_str(&format!("\n{label}: {url}\n")); }
    o.push_str(&format!("\n--\n{}\n", m.footer));
    o
}

/// Send a multipart/alternative (text + HTML) email via `sendmail -t`.
fn send_email_rich(to: &str, subject: &str, m: &MailParts) -> bool {
    use std::process::{Command, Stdio};
    let boundary = format!("=_cl_{:016x}", fnv1a(&format!("{to}{subject}{}", m.title)));
    let msg = format!(
        "To: {to}\r\nFrom: PlausiDen Guard <claude-tools@plausiden.com>\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n--{b}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{text}\r\n--{b}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{html}\r\n--{b}--\r\n",
        to = to, subject = subject, b = boundary, text = render_text(m), html = render_html(m)
    );
    if let Ok(mut c) = Command::new("sendmail").arg("-t").stdin(Stdio::piped()).spawn() {
        if let Some(si) = c.stdin.as_mut() {
            let _ = si.write_all(msg.as_bytes());
        }
        return c.wait().map(|s| s.success()).unwrap_or(false);
    }
    false
}

/// Read-only audit (`--scan-only`): classify the last API-error in every recent
/// transcript and print a JSON report. No action, no email, no marker, exit 0.
/// This is the real-world FP/FN instrument — point it at the live ~/.claude
/// transcripts with a wide --within-mins. `would_alert` = what the live timer
/// WOULD have done (policy block AND not an ignored sdk-* sub-agent).
fn sentinel_scan_report(dir: &Path, within_mins: i64, tail_kb: u64) -> Result<()> {
    let cfg = load_sentinel_config(dir);
    let roots = projects_roots(dir, &cfg);
    let mut rows = Vec::new();
    for t in recent_transcripts(&roots, within_mins) {
        let Some(raw) = read_tail(&t, tail_kb * 1024) else {
            continue;
        };
        for line in raw.lines().rev() {
            if let Some((code, msg, is_policy, ep)) = classify_error_line(line) {
                let sid = t
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let would_alert = is_policy && !(cfg.ignore_sdk && ep.starts_with("sdk"));
                rows.push(serde_json::json!({
                    "session": sid, "entrypoint": ep, "error": code,
                    "policy_block": is_policy, "would_alert": would_alert,
                    "snippet": msg.chars().take(100).collect::<String>(),
                }));
                break; // only the most recent API-error per transcript
            }
        }
    }
    let policy = rows.iter().filter(|r| r["policy_block"] == serde_json::json!(true)).count();
    let alert = rows.iter().filter(|r| r["would_alert"] == serde_json::json!(true)).count();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scan_only": true, "within_mins": within_mins,
            "transcripts_with_api_error": rows.len(),
            "policy_blocks": policy, "would_alert": alert,
            "rows": rows,
        }))?
    );
    Ok(())
}

fn cmd_sentinel(
    dir: &Path,
    setup: bool,
    email: Option<String>,
    no_email: bool,
    within_mins: i64,
    tail_kb: u64,
    scan_only: bool,
) -> Result<()> {
    if scan_only {
        return sentinel_scan_report(dir, within_mins, tail_kb);
    }
    if setup || email.is_some() || no_email {
        let mut cfg = load_sentinel_config(dir);
        if no_email {
            cfg.email_enabled = false;
        } else if let Some(e) = email {
            cfg.email = Some(e);
            cfg.email_enabled = true;
        } else if std::io::stdin().is_terminal() {
            print!("Notification email (blank = disable email): ");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let e = line.trim();
            if e.is_empty() {
                cfg.email_enabled = false;
            } else {
                cfg.email = Some(e.to_string());
                cfg.email_enabled = true;
            }
        }
        save_sentinel_config(dir, &cfg)?;
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "email": cfg.email, "email_enabled": cfg.email_enabled,
                "desktop_enabled": cfg.desktop_enabled
            }))?
        );
        return Ok(());
    }

    let cfg = load_sentinel_config(dir);
    let roots = projects_roots(dir, &cfg);
    // Prefer a policy block if several recent transcripts carry errors.
    let mut hit: Option<ApiErrorHit> = None;
    for t in recent_transcripts(&roots, within_mins) {
        if let Some(h) = detect_api_error(&t, tail_kb) {
            let policy = h.is_policy_block;
            if hit.is_none() || policy {
                hit = Some(h);
            }
            if policy {
                break;
            }
        }
    }
    let Some(hit) = hit else {
        println!("{}", serde_json::to_string(&serde_json::json!({"status":"healthy"}))?);
        return Ok(());
    };

    // Ignore automated sub-agent / SDK sessions (entrypoint sdk-*): a block in
    // a transient sub-call is not the operator's loop/conversation, and alerting
    // on each is noise — the original 7-email spam was all sdk-py sub-sessions.
    if cfg.ignore_sdk && hit.entrypoint.starts_with("sdk") {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"status":"ignored-sdk-subagent","entrypoint":hit.entrypoint,"session":hit.session_id}))?
        );
        return Ok(());
    }

    // Idempotency: act once per incident.
    let marker_dir = loop_ckpt_root(dir).join(".sentinel-actioned");
    fs::create_dir_all(&marker_dir)?;
    let marker = marker_dir.join(&hit.key);
    if marker.exists() {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"status":"already-actioned","key":hit.key}))?
        );
        return Ok(());
    }

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let stopped = stop_all_running_loops(dir, &hit)?;
    let backup = backup_conversation(dir, &hit, &stamp)?;
    let title = if hit.is_policy_block {
        "Claude Code blocked (usage-policy)"
    } else {
        "Claude Code API error"
    };
    let summary = format!(
        "{} — session {} stopped {} loop(s). Backup + RESUME.md: {}",
        hit.error_code,
        hit.session_id,
        stopped.len(),
        backup.display()
    );
    let mut desktop = false;
    let mut emailed = false;
    if cfg.desktop_enabled {
        desktop = desktop_notify(title, &summary);
    }
    // Global email cooldown: the backup + loop-stop ALWAYS happen, but suppress
    // the alert EMAIL if we emailed within cooldown_mins, so a recurring block
    // can't spam the inbox. The incident is still recorded by the marker below.
    let cooldown_path = loop_ckpt_root(dir).join(".sentinel-last-email");
    let in_cooldown = email_in_cooldown(
        fs::read_to_string(&cooldown_path).ok().as_deref(),
        cfg.cooldown_mins,
        Utc::now(),
    );
    if cfg.email_enabled && !in_cooldown {
        if let Some(addr) = &cfg.email {
            let host = hostname();
            let kind = if hit.is_policy_block { "cybersecurity-classifier block" } else { "API error" };
            let mut links: Vec<(String, String)> = first_urls(&hit.message, 2)
                .into_iter()
                .map(|u| (if u.contains("cyber-use-case") { "Apply for exemption".to_string() } else { "Learn more".to_string() }, u))
                .collect();
            links.push(("Support article".into(), "https://support.claude.com/en/articles/15363606".into()));
            let parts = MailParts {
                accent: if hit.is_policy_block { "#B4232B".into() } else { "#B26A00".into() },
                kicker: "PlausiDen API Guard · Incident".into(),
                title: if hit.is_policy_block { "A cyber-classifier block halted a session".into() } else { "An API error halted a session".into() },
                intro: format!(
                    "{host} detected a {kind} in an interactive session and stopped {} running loop(s). Your conversation is backed up and fully resumable — here's how.",
                    stopped.len()
                ),
                kv: vec![
                    ("Host".into(), host.clone()),
                    ("Error code".into(), hit.error_code.clone()),
                    ("Session".into(), hit.session_id.clone()),
                    ("Loops stopped".into(), if stopped.is_empty() { "none (not a guarded loop)".into() } else { stopped.join(", ") }),
                    ("Message".into(), hit.message.chars().take(240).collect::<String>()),
                ],
                code: Some(("Resume the full conversation".into(), format!("claude --resume {}", hit.session_id))),
                steps: vec![
                    "Run the command above to re-attach to the session — the entire conversation returns.".into(),
                    format!("If that session is gone, the complete raw transcript is preserved at {}/transcript.jsonl — open it or hand it to a fresh session.", backup.display()),
                    "Fix the underlying cause, then re-arm any halted loop with:  claude-loop guard --label <LABEL> --reset".into(),
                ],
                links,
                footer: format!(
                    "Sent by the claude-loop sentinel on {host} at {}.  Disable: systemctl disable --now claude-sentinel.timer  ·  Reconfigure: claude-loop sentinel --setup",
                    Utc::now().to_rfc3339()
                ),
            };
            emailed = send_email_rich(addr, "PlausiDen API Guard — session halted, backup ready", &parts);
            if emailed {
                let _ = fs::write(&cooldown_path, Utc::now().to_rfc3339());
                chmod_600(&cooldown_path).ok();
            }
        }
    }
    fs::write(&marker, format!("actioned {}\n", Utc::now().to_rfc3339()))?;
    chmod_600(&marker).ok();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "status":"ACTIONED","policy_block":hit.is_policy_block,"error":hit.error_code,
            "session":hit.session_id,"loops_stopped":stopped,
            "backup":backup.to_string_lossy(),"entrypoint":hit.entrypoint,
            "notified":{"desktop":desktop,"email":emailed,"email_suppressed_cooldown":in_cooldown}
        }))?
    );
    std::process::exit(3);
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
        Cmd::Sentinel {
            setup,
            email,
            no_email,
            within_mins,
            tail_kb,
            scan_only,
        } => cmd_sentinel(&dir, setup, email, no_email, within_mins, tail_kb, scan_only),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_detects_policy_block() {
        let dir = std::env::temp_dir().join(format!("cl-sentinel-a-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
        // The REAL cybersecurity-safeguard block shape (entrypoint sdk-py).
        std::fs::write(&f, "{\"type\":\"user\"}\n{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"API Error: Opus 4.7's safeguards flagged this message for a cybersecurity topic. Apply for an exemption: https://claude.com/form/cyber-use-case?token=x\"}]},\"error\":\"invalid_request\",\"isApiErrorMessage\":true,\"entrypoint\":\"sdk-py\"}\n").unwrap();
        let hit = detect_api_error(&f, 512).expect("hit");
        assert!(hit.is_policy_block);
        assert_eq!(hit.entrypoint, "sdk-py");
        assert_eq!(hit.session_id, "11111111-2222-3333-4444-555555555555");
        assert!(hit.key.starts_with("11111111-2222-3333-4444-555555555555-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sentinel_ignores_non_policy_api_error() {
        let dir = std::env::temp_dir().join(format!("cl-sentinel-b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("aaaa.jsonl");
        // A transient/auth API error is NOT the policy block — must be ignored
        // entirely (this is what stopped the spam: only policy blocks act).
        std::fs::write(&f, "{\"message\":{\"content\":[{\"text\":\"Not logged in\"}]},\"error\":\"authentication_failed\",\"isApiErrorMessage\":true}\n").unwrap();
        assert!(detect_api_error(&f, 512).is_none(), "non-policy API errors must be ignored");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- false-positive / false-negative boundary corpus (benign inputs only) ---
    fn detect_line(tag: &str, line: &str) -> Option<bool> {
        let dir = std::env::temp_dir().join(format!("cl-sx-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
        std::fs::write(&f, format!("{line}\n")).unwrap();
        let r = detect_api_error(&f, 512).map(|h| h.is_policy_block);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn sentinel_fp_flagged_in_billing_not_cyber() {
        // "flagged" present but not the cyber block — must NOT fire.
        assert_eq!(detect_line("fp1", "{\"message\":{\"content\":[{\"text\":\"Your payment method was flagged for manual review by billing.\"}]},\"error\":\"invalid_request\",\"isApiErrorMessage\":true,\"entrypoint\":\"cli\"}"), None);
    }

    #[test]
    fn sentinel_fp_violates_retry_policy_not_usage() {
        // "violates"+"policy" present but not "violate our usage policy" — must NOT fire.
        assert_eq!(detect_line("fp2", "{\"message\":{\"content\":[{\"text\":\"Request rejected: it violates our retry policy, please back off.\"}]},\"error\":\"invalid_request\",\"isApiErrorMessage\":true,\"entrypoint\":\"cli\"}"), None);
    }

    #[test]
    fn sentinel_fp_content_mention_is_not_a_block() {
        // A normal turn that DISCUSSES cybersecurity (no isApiErrorMessage) must NOT fire.
        assert_eq!(detect_line("fp3", "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Sure — let's discuss the cybersecurity topic and the cyber-use-case exemption form.\"}]}}"), None);
    }

    #[test]
    fn sentinel_fn_phrasing_variant_still_caught() {
        // Phrasing variant without "this message" — "cybersecurity topic" still catches it.
        assert_eq!(detect_line("fn1", "{\"message\":{\"content\":[{\"text\":\"API Error: safeguards flagged for a cybersecurity topic.\"}]},\"error\":\"invalid_request\",\"isApiErrorMessage\":true,\"entrypoint\":\"cli\"}"), Some(true));
    }

    #[test]
    fn sentinel_cooldown_suppresses_recent_email() {
        let now = Utc::now();
        // emailed 1 min ago → within the 120-min cooldown → suppress.
        assert!(email_in_cooldown(Some(&(now - chrono::Duration::minutes(1)).to_rfc3339()), 120, now));
        // emailed 3h ago → past cooldown → allow.
        assert!(!email_in_cooldown(Some(&(now - chrono::Duration::minutes(180)).to_rfc3339()), 120, now));
        // never emailed → allow.
        assert!(!email_in_cooldown(None, 120, now));
    }

    #[test]
    fn sentinel_ignores_clean_transcript() {
        let dir = std::env::temp_dir().join(format!("cl-sentinel-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bbbb.jsonl");
        std::fs::write(&f, "{\"type\":\"assistant\",\"stop_reason\":\"end_turn\"}\n{\"type\":\"user\"}\n").unwrap();
        assert!(detect_api_error(&f, 512).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sentinel_fnv1a_is_stable() {
        assert_eq!(fnv1a("abc"), fnv1a("abc"));
        assert_ne!(fnv1a("abc"), fnv1a("abd"));
    }

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
