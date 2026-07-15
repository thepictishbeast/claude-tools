//! claude-session — track and isolate Claude Code sessions so that two sessions
//! running in the same working directory stop clashing (corrupted git index,
//! stomped commits, restore picking the wrong session).
//!
//! Two layers:
//!   * **Registry** — each session writes its own small JSON record
//!     (~/.claude/sessions/<sid>.json). Per-session files, not one shared file,
//!     so the registry itself is never the thing two sessions fight over.
//!     `list`/`guard` read them and flag any repo whose *live* tree is claimed
//!     by 2+ active sessions.
//!   * **Isolation** — `isolate` puts THIS session in its own git worktree
//!     (separate index + files, shared history, a per-session branch), so work
//!     can't collide. `release` tears it down.
//!
//! Complementary to the Agent OS SESSION-PROTOCOL (that governs bootstrap +
//! project/goal registry); this governs the session *process* + working tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Result};
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
        /// Override session id (else $CLAUDE_CODE_SESSION_ID, else newest transcript).
        #[arg(long)]
        session_id: Option<String>,
    },
    /// List known sessions and flag collisions (2+ active sessions sharing a live tree).
    List,
    /// Put THIS session in its own git worktree, isolated from the shared live tree.
    Isolate {
        /// Repo to isolate (default: the git repo at cwd).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Base ref for the session branch (default: HEAD).
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Remove this session's worktree and clear it from the registry.
    Release {
        #[arg(long)]
        session_id: Option<String>,
        /// Keep the per-session branch instead of deleting it.
        #[arg(long)]
        keep_branch: bool,
    },
    /// Advisory: exit 3 if another active session shares this repo's live tree.
    Guard {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
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
    branch: Option<String>,
    #[serde(default)]
    transcript: Option<String>,
    started_at: String,
    last_seen: String,
}

const ACTIVE_SECS: u64 = 600; // "active" if transcript/record changed within 10 min

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
fn worktrees_root() -> PathBuf {
    home().join(".claude/worktrees")
}
fn short(sid: &str) -> &str {
    &sid[..sid.len().min(8)]
}

/// Session ids are UUID-shaped. Restrict to that charset so a value from the
/// environment can never contain `/` or `..` and steer the registry path.
fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

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
        .filter(|s| valid_sid(s))
}

fn run_git(args: &[&str], cwd: &Path) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_toplevel(pwd: &Path) -> Option<String> {
    run_git(&["rev-parse", "--show-toplevel"], pwd).ok().filter(|s| !s.is_empty())
}

fn fresh(path: &str) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < ACTIVE_SECS)
}

fn is_active(rec: &Record) -> bool {
    rec.transcript.as_deref().is_some_and(fresh) || fresh(&reg_path(&rec.session_id).to_string_lossy())
}

fn load_one(sid: &str) -> Option<Record> {
    fs::read_to_string(reg_path(sid)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn load_all() -> Vec<Record> {
    let mut v = vec![];
    for f in fs::read_dir(reg_dir()).into_iter().flatten().flatten() {
        if f.path().extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(r) = fs::read_to_string(f.path()).ok().and_then(|s| serde_json::from_str::<Record>(&s).ok()) {
                if valid_sid(&r.session_id) {
                    v.push(r);
                }
            }
        }
    }
    v
}

fn save(rec: &Record) -> Result<()> {
    fs::create_dir_all(reg_dir())?;
    fs::write(reg_path(&rec.session_id), serde_json::to_string_pretty(rec)?)?;
    Ok(())
}

fn record_for(sid: &str) -> Record {
    load_one(sid).unwrap_or_else(|| Record {
        session_id: sid.to_string(),
        pwd: env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        repo_root: None,
        worktree: None,
        branch: None,
        transcript: transcript_for(sid).map(|p| p.to_string_lossy().to_string()),
        started_at: Utc::now().to_rfc3339(),
        last_seen: Utc::now().to_rfc3339(),
    })
}

fn cmd_register(session_id: Option<String>) -> Result<()> {
    let sid = resolve_sid(session_id)
        .context("could not resolve session id (set $CLAUDE_CODE_SESSION_ID or pass --session-id)")?;
    let pwd = env::current_dir()?.to_string_lossy().to_string();
    let mut rec = record_for(&sid);
    rec.pwd = pwd.clone();
    rec.repo_root = git_toplevel(Path::new(&pwd));
    rec.transcript = transcript_for(&sid).map(|p| p.to_string_lossy().to_string());
    rec.last_seen = Utc::now().to_rfc3339();
    save(&rec)?;
    println!("registered session {} -> {}", short(&sid), reg_path(&sid).display());
    Ok(())
}

fn cmd_list() -> Result<()> {
    let recs = load_all();
    if recs.is_empty() {
        println!("no sessions registered (run: claude-session register)");
        return Ok(());
    }
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
            println!("  -> isolate one: claude-session isolate   # run inside that session");
        }
    }
    if !collision {
        println!("\nno collisions: no repo has 2+ active sessions in its live working tree.");
    }
    Ok(())
}

fn cmd_isolate(repo: Option<PathBuf>, base: Option<String>, session_id: Option<String>) -> Result<()> {
    let sid = resolve_sid(session_id).context("could not resolve session id")?;
    let sid8 = short(&sid).to_string();
    let cwd = env::current_dir()?;
    let repo_root = match repo {
        Some(r) => r,
        None => PathBuf::from(git_toplevel(&cwd).context("not inside a git repo (pass --repo)")?),
    };

    // Already isolated (and the worktree still exists)? Idempotent.
    if let Some(rec) = load_one(&sid) {
        if let Some(wt) = rec.worktree.as_deref() {
            if Path::new(wt).is_dir() {
                println!("already isolated: {wt}");
                return Ok(());
            }
        }
    }

    let name = repo_root.file_name().and_then(|s| s.to_str()).unwrap_or("repo");
    let dest = worktrees_root().join(name).join(&sid8);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let dest_s = dest.to_string_lossy().to_string();
    let branch = format!("session/{sid8}");
    let base_ref = base.unwrap_or_else(|| "HEAD".to_string());

    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = run_git(&["rev-parse", "--verify", branch_ref.as_str()], &repo_root).is_ok();
    if branch_exists {
        run_git(&["worktree", "add", dest_s.as_str(), branch.as_str()], &repo_root)?;
    } else {
        run_git(&["worktree", "add", "-b", branch.as_str(), dest_s.as_str(), base_ref.as_str()], &repo_root)?;
    }

    let mut rec = record_for(&sid);
    rec.repo_root = Some(repo_root.to_string_lossy().to_string());
    rec.worktree = Some(dest_s.clone());
    rec.branch = Some(branch.clone());
    rec.last_seen = Utc::now().to_rfc3339();
    save(&rec)?;

    println!("isolated session {sid8}");
    println!("  worktree: {dest_s}");
    println!("  branch:   {branch}");
    println!("  -> cd {dest_s}   (work there; merge the branch back later)");
    println!("  -> release with: claude-session release");
    Ok(())
}

fn cmd_release(session_id: Option<String>, keep_branch: bool) -> Result<()> {
    let sid = resolve_sid(session_id).context("could not resolve session id")?;
    let rec = load_one(&sid).context("no registry record for this session")?;
    let wt = rec.worktree.clone().context("this session has no worktree recorded")?;
    let repo_root = rec.repo_root.clone().context("no repo_root recorded")?;
    let repo_path = PathBuf::from(&repo_root);

    // `git worktree remove` refuses if the worktree is dirty — surface that as a
    // helpful hint rather than silently discarding work.
    run_git(&["worktree", "remove", wt.as_str()], &repo_path).map_err(|e| {
        anyhow::anyhow!("{e}\n  the worktree has uncommitted changes — commit/stash in {wt} first, or `git worktree remove --force` by hand if you're sure")
    })?;

    if !keep_branch {
        if let Some(branch) = &rec.branch {
            let _ = run_git(&["branch", "-D", branch.as_str()], &repo_path); // best-effort
        }
    }

    let mut rec = rec;
    rec.worktree = None;
    rec.branch = None;
    rec.last_seen = Utc::now().to_rfc3339();
    save(&rec)?;
    println!("released worktree for {} ({wt})", short(&sid));
    Ok(())
}

fn cmd_guard(repo: Option<PathBuf>) -> Result<()> {
    let cwd = env::current_dir()?;
    let target = match repo {
        Some(r) => r.to_string_lossy().to_string(),
        None => git_toplevel(&cwd).unwrap_or_else(|| cwd.to_string_lossy().to_string()),
    };
    let others: Vec<String> = load_all()
        .into_iter()
        .filter(|r| is_active(r) && r.worktree.is_none() && r.repo_root.as_deref() == Some(target.as_str()))
        .map(|r| short(&r.session_id).to_string())
        .collect();
    if others.len() > 1 {
        eprintln!("⚠ {} active sessions share the live tree of {} — {:?}", others.len(), target, others);
        eprintln!("  isolate this one: claude-session isolate");
        std::process::exit(3);
    }
    println!("ok: no live-tree collision for {target}");
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Register { session_id } => cmd_register(session_id),
        Cmd::List => cmd_list(),
        Cmd::Isolate { repo, base, session_id } => cmd_isolate(repo, base, session_id),
        Cmd::Release { session_id, keep_branch } => cmd_release(session_id, keep_branch),
        Cmd::Guard { repo } => cmd_guard(repo),
    }
}
