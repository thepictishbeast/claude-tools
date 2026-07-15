//! claude-session — track (and later isolate) Claude Code sessions so that two
//! sessions running in the same working directory stop clashing.
//!
//! Stage 1 (this file): a per-session registry + collision detection. Each
//! session writes its own small JSON record (no shared file → no lock
//! contention, which would be the very clash we're fixing). `list` reads them
//! all and flags any repo whose *live* working tree is claimed by more than one
//! active session — the situation that corrupts indexes and stomps commits.
//!
//! Stage 2 will add `isolate`/`release`/`guard` (git-worktree isolation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "claude-session", version, about = "Track & isolate Claude Code sessions")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record (or refresh) THIS session in the registry.
    Register {
        /// Override the session id (else $CLAUDE_CODE_SESSION_ID, else newest transcript).
        #[arg(long)]
        session_id: Option<String>,
    },
    /// List known sessions and flag collisions (2+ active sessions sharing a live tree).
    List,
}

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    session_id: String,
    pwd: String,
    #[serde(default)]
    repo_root: Option<String>,
    #[serde(default)]
    worktree: Option<String>,
    #[serde(default)]
    transcript: Option<String>,
    started_at: String,
    last_seen: String,
}

const ACTIVE_SECS: u64 = 600; // a session is "active" if its transcript/record changed within 10 min

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"))
}
fn reg_dir() -> PathBuf {
    home().join(".claude/sessions")
}
fn reg_path(sid: &str) -> PathBuf {
    reg_dir().join(format!("{sid}.json"))
}
fn projects_dir() -> PathBuf {
    home().join(".claude/projects")
}
fn short(sid: &str) -> &str {
    &sid[..sid.len().min(8)]
}

/// Session ids are UUID-shaped. Restrict to that charset so a value coming from
/// the environment can never contain `/` or `..` and steer the registry path.
fn valid_sid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Newest `<sid>.jsonl` under ~/.claude/projects/*/ — the currently-liveliest session.
fn newest_transcript() -> Option<(String, PathBuf)> {
    let mut best: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for d in fs::read_dir(projects_dir()).into_iter().flatten().flatten() {
        if !d.path().is_dir() {
            continue;
        }
        for f in fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(m) = f.metadata().and_then(|m| m.modified()) {
                if best.as_ref().is_none_or(|(bt, _, _)| m > *bt) {
                    let sid = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                    best = Some((m, sid, p));
                }
            }
        }
    }
    best.map(|(_, sid, p)| (sid, p))
}

fn transcript_for(sid: &str) -> Option<PathBuf> {
    for d in fs::read_dir(projects_dir()).into_iter().flatten().flatten() {
        let cand = d.path().join(format!("{sid}.jsonl"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn resolve_sid(explicit: Option<String>) -> Option<String> {
    explicit
        .or_else(|| env::var("CLAUDE_CODE_SESSION_ID").ok().filter(|s| !s.is_empty()))
        .or_else(|| newest_transcript().map(|(sid, _)| sid))
        .filter(|s| valid_sid(s)) // reject anything that isn't UUID-shaped
}

fn git_toplevel(pwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(pwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn fresh(path: &str) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < ACTIVE_SECS)
}

/// Active if the transcript grew recently (Claude appends every turn) or the
/// record itself was refreshed recently.
fn is_active(rec: &Record) -> bool {
    rec.transcript.as_deref().is_some_and(fresh)
        || fresh(&reg_path(&rec.session_id).to_string_lossy())
}

fn load_all() -> Vec<Record> {
    let mut v = vec![];
    for f in fs::read_dir(reg_dir()).into_iter().flatten().flatten() {
        if f.path().extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(r) = fs::read_to_string(f.path())
                .ok()
                .and_then(|s| serde_json::from_str::<Record>(&s).ok())
            {
                v.push(r);
            }
        }
    }
    v
}

fn cmd_register(session_id: Option<String>) -> Result<()> {
    let sid = resolve_sid(session_id)
        .context("could not resolve session id (set $CLAUDE_CODE_SESSION_ID or pass --session-id)")?;
    let pwd = env::current_dir()?.to_string_lossy().to_string();
    let repo_root = git_toplevel(Path::new(&pwd));
    let transcript = transcript_for(&sid).map(|p| p.to_string_lossy().to_string());
    fs::create_dir_all(reg_dir())?;
    let path = reg_path(&sid);
    let now = Utc::now().to_rfc3339();
    // preserve started_at + any worktree already recorded for this session
    let (started_at, worktree) = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Record>(&s).ok())
        .map(|p| (p.started_at, p.worktree))
        .unwrap_or_else(|| (now.clone(), None));
    let rec = Record { session_id: sid.clone(), pwd, repo_root, worktree, transcript, started_at, last_seen: now };
    fs::write(&path, serde_json::to_string_pretty(&rec)?)?;
    println!("registered session {} -> {}", short(&sid), path.display());
    Ok(())
}

fn cmd_list() -> Result<()> {
    let recs = load_all();
    if recs.is_empty() {
        println!("no sessions registered (run: claude-session register)");
        return Ok(());
    }
    // repo_root -> active sessions working in its LIVE tree (no worktree)
    let mut live: HashMap<String, Vec<String>> = HashMap::new();
    println!("{:<10} {:<7} {:<9} repo / pwd", "session", "active", "tree");
    for r in &recs {
        let act = is_active(r);
        let isolated = r.worktree.is_some();
        println!(
            "{:<10} {:<7} {:<9} {}",
            short(&r.session_id),
            if act { "yes" } else { "no" },
            if isolated { "worktree" } else { "live" },
            r.repo_root.clone().unwrap_or_else(|| r.pwd.clone())
        );
        if act && !isolated {
            if let Some(repo) = &r.repo_root {
                live.entry(repo.clone()).or_default().push(short(&r.session_id).to_string());
            }
        }
    }
    let mut collision = false;
    for (repo, sids) in &live {
        if sids.len() > 1 {
            collision = true;
            println!("\n⚠ COLLISION: {} active sessions share the LIVE tree of {} — {:?}", sids.len(), repo, sids);
            println!("  -> isolate one (stage 2): claude-session isolate   # run inside that session");
        }
    }
    if !collision {
        println!("\nno collisions: no repo has 2+ active sessions in its live working tree.");
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Register { session_id } => cmd_register(session_id),
        Cmd::List => cmd_list(),
    }
}
