//! claude-mesh — client for the agent-mesh channel (MESH-PROTOCOL v1).
//!
//! Wraps the exact plain-git wire format described in MESH-PROTOCOL.md so the
//! manual method and the tool stay identical: one JSON file per message under
//! `bus/`, append-only, committed to a shared private git repo (cross-device
//! transport) and mirrored to a local dir (fast same-host transport).
//!
//! Node id + repo live in `$CLAUDE_MESH_HOME/config.json` (default
//! `~/.claude/mesh`). `CLAUDE_MESH_HOME` can be overridden so several nodes can
//! run on one host (also how the test drives two nodes).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "claude-mesh", version, about = "Message other Claude sessions over agent-mesh")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create this node's config ($CLAUDE_MESH_HOME/config.json).
    Init {
        #[arg(long)]
        role: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Append '#<sid8>' to the id (for multiple same-role sessions on one host).
        #[arg(long)]
        sid: Option<String>,
        /// LOCAL unix user to run git as — used ONLY when it differs from the
        /// current user and exists (e.g. prime: root tool, paul-owned repo).
        /// NOT the GitHub repo owner. Omit on a box where you already own the clone.
        #[arg(long)]
        git_user: Option<String>,
    },
    /// Print this node's id.
    Whoami,
    /// Write nodes/<id>.json into the repo and push.
    Register {
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Post a message. Body is read from stdin.
    Post {
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "fyi")]
        kind: String,
        #[arg(long, default_value = "")]
        subject: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// Room to post into (default "main").
        #[arg(long)]
        room: Option<String>,
        /// Write only to the local bus; do not commit/push.
        #[arg(long)]
        local: bool,
    },
    /// Show messages addressed to me that I haven't seen.
    Inbox {
        #[arg(long)]
        count: bool,
    },
    /// Refresh this node's presence heartbeat.
    Beat,
    /// List who is currently present (fresh heartbeat) vs idle/left.
    Presence,
    /// Mark this node as having left the mesh.
    Leave,
    /// Join a room (start seeing its messages).
    Join {
        room: String,
    },
    /// List rooms seen in the bus and which I've joined.
    Rooms,
    /// Read a room's recent conversation — ALL messages, not just ones for me.
    Log {
        /// Room to read (default: your joined rooms).
        #[arg(long)]
        room: Option<String>,
        /// How many recent messages to show.
        #[arg(long, short = 'n', default_value_t = 25)]
        n: usize,
    },
    /// Print one message (id or id-prefix) and mark it seen.
    Read {
        id: String,
    },
    /// Mark messages seen: a specific id, or --all currently-unread.
    Ack {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// git pull + push the shared repo.
    Sync,
    /// One-line "N new messages" for session hooks (silent if none).
    Nudge,
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    id: String,
    role: String,
    host: String,
    /// The enforced per-session suffix. `id` is always `role@host#<sid8>`.
    #[serde(default)]
    sid8: Option<String>,
    repo: String,
    /// If set, git runs as this user (`sudo -u <user> git`). Use when the tool
    /// runs as root but the repo is owned by someone else (root agent, paul repo).
    #[serde(default)]
    git_user: Option<String>,
    /// Rooms this node participates in (default ["main"]). inbox/nudge only
    /// surface messages in these rooms.
    #[serde(default = "default_rooms")]
    rooms: Vec<String>,
}

fn empty_obj() -> serde_json::Value {
    serde_json::json!({})
}
fn default_room() -> String {
    "main".into()
}
fn default_rooms() -> Vec<String> {
    vec!["main".into()]
}
const PRESENCE_TTL_SECS: i64 = 900; // a node idle >15min drops off the present roster

#[derive(Serialize, Deserialize, Clone)]
struct Message {
    v: u32,
    id: String,
    from: String,
    to: String,
    ts: String,
    kind: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(default = "default_room")]
    room: String,
    #[serde(default = "empty_obj")]
    ext: serde_json::Value,
    #[serde(default)]
    sig: Option<String>,
}

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"))
}
fn base_home() -> PathBuf {
    env::var_os("CLAUDE_MESH_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".claude/mesh"))
}
/// Per-session identity dir: several Claude sessions share one host+root, so a
/// node's config/seen are keyed by session id. An explicit CLAUDE_MESH_HOME
/// forces a flat dir (used for extra nodes / tests).
fn session_dir() -> PathBuf {
    if env::var_os("CLAUDE_MESH_HOME").is_some() {
        return base_home();
    }
    if let Ok(sid) = env::var("CLAUDE_CODE_SESSION_ID") {
        if sid.len() >= 8 {
            return base_home().join("sessions").join(&sid[..8]);
        }
    }
    base_home()
}
fn config_path() -> PathBuf {
    session_dir().join("config.json")
}
fn seen_path() -> PathBuf {
    session_dir().join("seen")
}
// The local fast-path bus is SHARED across co-located sessions (only identity is per-session).
fn local_bus() -> PathBuf {
    base_home().join("bus")
}

fn load_config() -> Result<Config> {
    let p = config_path();
    let s = fs::read_to_string(&p)
        .with_context(|| format!("no mesh config at {} — run: claude-mesh init --role <role>", p.display()))?;
    let mut cfg: Config = serde_json::from_str(&s)?;
    // ENFORCE the #sid8 suffix: a legacy bare id (role@host) is upgraded in
    // memory so every code path sees a unique, collision-proof id.
    if !cfg.id.contains('#') {
        let sid8 = cfg.sid8.clone().unwrap_or_else(|| derive_sid8(None));
        cfg.id = format!("{}@{}#{}", cfg.role, cfg.host, sid8);
        cfg.sid8 = Some(sid8);
    } else if cfg.sid8.is_none() {
        cfg.sid8 = cfg.id.split('#').nth(1).map(str::to_string);
    }
    Ok(cfg)
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now_rfc3339() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
fn now_stamp() -> String {
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The mandatory per-session suffix. Every node id is `role@host#<sid8>`, so two
/// sessions picking the same role+host can never collide. Priority: explicit
/// override > first 8 of $CLAUDE_CODE_SESSION_ID > random (still unique).
fn derive_sid8(explicit: Option<String>) -> String {
    if let Some(s) = explicit {
        let s: String = sanitize(&s).chars().take(8).collect();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(sid) = env::var("CLAUDE_CODE_SESSION_ID") {
        if sid.len() >= 8 {
            return sid[..8].to_string();
        }
    }
    rand_hex(4) // 8 hex chars
}

/// The host-scoped role prefix `role@host` (an id with the #sid8 stripped).
/// Addressing a message here reaches every session of that role on that host.
fn bare(cfg: &Config) -> String {
    format!("{}@{}", cfg.role, cfg.host)
}

/// The current effective unix user (`id -un`), falling back to $USER then root.
fn current_user() -> String {
    Command::new("id").arg("-un").output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "root".into())
}
/// Is `u` a real local unix account? (`id -u <u>` succeeds)
fn user_exists(u: &str) -> bool {
    Command::new("id").arg("-u").arg(u).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Decide which user (if any) git should be `sudo -u`'d to. Pure so it can be
/// tested: escalate ONLY to a real local account different from the current one.
fn sudo_target(git_user: Option<&str>, current: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
    git_user
        .filter(|u| !u.is_empty() && *u != current && exists(u))
        .map(|u| u.to_string())
}

fn git(cfg: &Config, args: &[&str]) -> Result<String> {
    // `git_user` names the LOCAL unix account git should run as. Only escalate
    // with `sudo -u` when it is a real local user DIFFERENT from the current one
    // (the prime case: Claude runs as root, the repo is paul's). If it is unset,
    // equals the current user, or is not a local account at all (a cross-device
    // node that was mis-told to pass the GitHub repo-owner), run git directly as
    // the current user — who already owns the clone + token. This is what makes
    // cross-device nodes work; before, any non-local git_user hard-failed sudo.
    let cur = current_user();
    let sudo_as = sudo_target(cfg.git_user.as_deref(), &cur, user_exists);
    let mut cmd = match sudo_as.as_deref() {
        Some(u) => {
            let mut c = Command::new("sudo");
            c.arg("-u").arg(u).arg("git");
            c
        }
        None => Command::new("git"),
    };
    cmd.arg("-C").arg(&cfg.repo).args(args);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("git {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
fn git_ok(cfg: &Config, args: &[&str]) -> bool {
    git(cfg, args).is_ok()
}

/// Read + parse every message file under a bus dir.
fn read_bus(dir: &PathBuf, into: &mut HashMap<String, Message>) {
    for f in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = f.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(m) = fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<Message>(&s).ok()) {
            into.entry(m.id.clone()).or_insert(m);
        }
    }
}

/// All known messages (repo bus + local bus), deduped by id, sorted by ts.
fn all_messages(cfg: &Config) -> Vec<Message> {
    let mut map = HashMap::new();
    read_bus(&PathBuf::from(&cfg.repo).join("bus"), &mut map);
    read_bus(&local_bus(), &mut map);
    let mut v: Vec<Message> = map.into_values().collect();
    v.sort_by(|a, b| a.ts.cmp(&b.ts));
    v
}

fn addressed_to_me(m: &Message, cfg: &Config) -> bool {
    m.to == cfg.id                              // this exact session
        || m.to == "all"                        // everyone
        || m.to == format!("role:{}", cfg.role) // every session of my role, any host
        || m.to == bare(cfg)                    // role@host — host-scoped role broadcast
                                                 // (also delivers legacy bare-addressed messages)
}

fn load_seen() -> HashSet<String> {
    fs::read_to_string(seen_path()).map(|s| s.lines().map(|l| l.trim().to_string()).collect()).unwrap_or_default()
}
fn mark_seen(ids: &[String]) {
    let mut set = load_seen();
    let mut changed = false;
    for id in ids {
        if set.insert(id.clone()) {
            changed = true;
        }
    }
    if changed {
        let _ = fs::create_dir_all(session_dir());
        let mut all: Vec<_> = set.into_iter().collect();
        all.sort();
        let _ = fs::write(seen_path(), all.join("\n") + "\n");
    }
}

fn unread(cfg: &Config) -> Vec<Message> {
    let seen = load_seen();
    all_messages(cfg)
        .into_iter()
        .filter(|m| m.from != cfg.id && cfg.rooms.contains(&m.room) && addressed_to_me(m, cfg) && !seen.contains(&m.id))
        .collect()
}

fn cmd_init(role: String, host: Option<String>, repo: Option<PathBuf>, sid: Option<String>, git_user: Option<String>) -> Result<()> {
    let host = host.unwrap_or_else(|| {
        Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "localhost".into())
    });
    let repo = repo.unwrap_or_else(|| PathBuf::from("/home/paul/projects/agent-mesh"));
    let sid8 = derive_sid8(sid);
    let id = format!("{role}@{host}#{sid8}"); // enforced: never bare
    let cfg = Config { id: id.clone(), role, host, sid8: Some(sid8), repo: repo.to_string_lossy().to_string(), git_user, rooms: default_rooms() };
    fs::create_dir_all(session_dir())?;
    fs::create_dir_all(local_bus())?;
    fs::write(config_path(), serde_json::to_string_pretty(&cfg)? + "\n")?;
    println!("node {id} -> {}", config_path().display());
    Ok(())
}

fn cmd_register(note: String) -> Result<()> {
    let cfg = load_config()?;
    let nodes = PathBuf::from(&cfg.repo).join("nodes");
    fs::create_dir_all(&nodes)?;
    let rec = serde_json::json!({
        "v": 1, "id": cfg.id, "role": cfg.role, "host": cfg.host,
        "protocol_versions": [1], "registered_at": now_rfc3339(),
        "pubkey": serde_json::Value::Null, "note": note,
    });
    let path = nodes.join(format!("{}.json", sanitize(&cfg.id)));
    fs::write(&path, serde_json::to_string_pretty(&rec)? + "\n")?;
    let rel = format!("nodes/{}.json", sanitize(&cfg.id));
    let _ = git(&cfg, &["add", &rel]);
    let _ = commit(&cfg, &format!("register {}", cfg.id));
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let pushed = git_ok(&cfg, &["push"]);
    println!("registered {} ({})", cfg.id, if pushed { "pushed" } else { "local only — run: claude-mesh sync" });
    Ok(())
}

fn commit(cfg: &Config, msg: &str) -> Result<String> {
    git(cfg, &["-c", &format!("user.name={}", cfg.id), "-c", "user.email=mesh@plausiden.com", "commit", "-q", "-m", msg])
}

fn cmd_post(to: String, kind: String, subject: String, reference: Option<String>, repo_ctx: Option<String>, room: Option<String>, local: bool) -> Result<()> {
    let cfg = load_config()?;
    let mut body = String::new();
    let _ = std::io::stdin().read_to_string(&mut body);
    let id = rand_hex(8);
    let m = Message {
        v: 1,
        id: id.clone(),
        from: cfg.id.clone(),
        to,
        ts: now_rfc3339(),
        kind,
        reference,
        subject,
        body: body.trim_end().to_string(),
        repo: repo_ctx,
        room: room.unwrap_or_else(default_room),
        ext: empty_obj(),
        sig: None,
    };
    let fname = format!("{}-{}-{}.json", now_stamp(), sanitize(&cfg.id), &id[..8]);
    let json = serde_json::to_string_pretty(&m)? + "\n";
    // fast local copy (same-host readers see it without a pull)
    fs::create_dir_all(local_bus())?;
    fs::write(local_bus().join(&fname), &json)?;
    // durable copy in the shared repo
    let repo_bus = PathBuf::from(&cfg.repo).join("bus");
    fs::create_dir_all(&repo_bus)?;
    fs::write(repo_bus.join(&fname), &json)?;
    if !local {
        let rel = format!("bus/{fname}");
        git(&cfg, &["add", &rel])?;
        commit(&cfg, &format!("msg: {}", m.subject))?;
        let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
        let pushed = git_ok(&cfg, &["push"]);
        println!("posted {} -> {} ({})", &id[..8], m.to, if pushed { "pushed" } else { "local commit — run: claude-mesh sync" });
    } else {
        println!("posted {} -> {} (local only)", &id[..8], m.to);
    }
    heartbeat(&cfg); // any activity refreshes presence (throttled inside)
    Ok(())
}

fn line(m: &Message) -> String {
    format!("{}  [{}] {} → {}  {}  (id {})", m.ts, m.kind, m.from, m.to, m.subject, &m.id[..m.id.len().min(8)])
}

fn cmd_inbox(count: bool) -> Result<()> {
    let cfg = load_config()?;
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let msgs = unread(&cfg);
    if count {
        println!("{}", msgs.len());
        return Ok(());
    }
    if msgs.is_empty() {
        println!("no new messages for {}", cfg.id);
        return Ok(());
    }
    println!("{} new message(s) for {}:", msgs.len(), cfg.id);
    for m in &msgs {
        println!("  {}", line(m));
    }
    println!("read one: claude-mesh read <id>   |   mark all seen: claude-mesh ack --all");
    Ok(())
}

fn cmd_read(id: String) -> Result<()> {
    let cfg = load_config()?;
    let m = all_messages(&cfg).into_iter().find(|m| m.id == id || m.id.starts_with(&id));
    let m = m.with_context(|| format!("no message matching {id}"))?;
    println!("{}", serde_json::to_string_pretty(&m)?);
    mark_seen(&[m.id]);
    Ok(())
}

fn cmd_ack(id: Option<String>, all: bool) -> Result<()> {
    let cfg = load_config()?;
    if all {
        let ids: Vec<String> = unread(&cfg).into_iter().map(|m| m.id).collect();
        let n = ids.len();
        mark_seen(&ids);
        println!("acked {n} message(s)");
    } else if let Some(id) = id {
        let m = all_messages(&cfg).into_iter().find(|m| m.id == id || m.id.starts_with(&id)).with_context(|| format!("no message matching {id}"))?;
        mark_seen(&[m.id]);
        println!("acked {id}");
    } else {
        bail!("give an id or --all");
    }
    Ok(())
}

fn cmd_sync() -> Result<()> {
    let cfg = load_config()?;
    let pulled = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let pushed = git_ok(&cfg, &["push"]);
    println!("sync: pull {} push {}", if pulled { "ok" } else { "skip" }, if pushed { "ok" } else { "skip" });
    Ok(())
}

fn cmd_whoami() -> Result<()> {
    println!("{}", load_config()?.id);
    Ok(())
}

fn cmd_nudge() -> Result<()> {
    // Silent + exit 0 on any problem, so a session hook never breaks.
    let cfg = match load_config() {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    heartbeat(&cfg); // every prompt (via coord.sh) refreshes presence; idle sessions expire
    let msgs = unread(&cfg);
    if msgs.is_empty() {
        return Ok(());
    }
    let subjects: Vec<String> = msgs.iter().take(3).map(|m| format!("{} from {}", m.subject, m.from)).collect();
    println!("📨 {} new agent-mesh message(s) for {} — `claude-mesh inbox`", msgs.len(), cfg.id);
    for s in subjects {
        println!("   • {s}");
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Status {
    id: String,
    role: String,
    host: String,
    #[serde(default)]
    rooms: Vec<String>,
    last_seen: String,
    #[serde(default)]
    status: String,
}

fn status_path(cfg: &Config) -> PathBuf {
    PathBuf::from(&cfg.repo).join("nodes").join(format!("{}.status", sanitize(&cfg.id)))
}
fn secs_since_mtime(p: &Path) -> Option<u64> {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|e| e.as_secs())
}
fn write_status(cfg: &Config, state: &str) -> Result<()> {
    fs::create_dir_all(PathBuf::from(&cfg.repo).join("nodes"))?;
    let st = Status {
        id: cfg.id.clone(),
        role: cfg.role.clone(),
        host: cfg.host.clone(),
        rooms: cfg.rooms.clone(),
        last_seen: now_rfc3339(),
        status: state.into(),
    };
    fs::write(status_path(cfg), serde_json::to_string_pretty(&st)? + "\n")?;
    let rel = format!("nodes/{}.status", sanitize(&cfg.id));
    let _ = git(cfg, &["add", &rel]);
    let _ = commit(cfg, &format!("beat {} ({state})", cfg.id));
    let _ = git_ok(cfg, &["pull", "--rebase", "--autostash"]);
    let _ = git_ok(cfg, &["push"]);
    Ok(())
}
/// Refresh presence, throttled so activity doesn't churn git more than ~1/5min.
fn heartbeat(cfg: &Config) {
    if secs_since_mtime(&status_path(cfg)).is_some_and(|s| s < 300) {
        return;
    }
    let _ = write_status(cfg, "online");
}
fn cmd_beat() -> Result<()> {
    let cfg = load_config()?;
    write_status(&cfg, "online")?;
    println!("beat {} (rooms: {})", cfg.id, cfg.rooms.join(","));
    Ok(())
}
fn cmd_leave() -> Result<()> {
    let cfg = load_config()?;
    write_status(&cfg, "left")?;
    println!("left the mesh: {}", cfg.id);
    Ok(())
}
fn cmd_presence() -> Result<()> {
    let cfg = load_config()?;
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let now = Utc::now();
    let (mut present, mut away) = (Vec::new(), Vec::new());
    for f in fs::read_dir(PathBuf::from(&cfg.repo).join("nodes")).into_iter().flatten().flatten() {
        let p = f.path();
        if p.extension().and_then(|e| e.to_str()) != Some("status") {
            continue;
        }
        if let Some(st) = fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<Status>(&s).ok()) {
            let age = chrono::DateTime::parse_from_rfc3339(&st.last_seen)
                .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
                .unwrap_or(i64::MAX);
            let line = format!("{:<30} rooms=[{}] seen {}s ago", st.id, st.rooms.join(","), age.max(0));
            if st.status != "left" && age < PRESENCE_TTL_SECS {
                present.push(line);
            } else {
                away.push(format!("{line}{}", if st.status == "left" { " (left)" } else { " (idle)" }));
            }
        }
    }
    present.sort();
    away.sort();
    println!("PRESENT ({}):", present.len());
    for l in &present {
        println!("  🟢 {l}");
    }
    if !away.is_empty() {
        println!("away/left ({}):", away.len());
        for l in &away {
            println!("  ⚪ {l}");
        }
    }
    Ok(())
}
fn cmd_join(room: String) -> Result<()> {
    let mut cfg = load_config()?;
    if !cfg.rooms.contains(&room) {
        cfg.rooms.push(room.clone());
        fs::write(config_path(), serde_json::to_string_pretty(&cfg)? + "\n")?;
    }
    println!("joined '{room}' — now in: {}", cfg.rooms.join(","));
    Ok(())
}
fn cmd_rooms() -> Result<()> {
    let cfg = load_config()?;
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let mut set: std::collections::BTreeSet<String> = all_messages(&cfg).into_iter().map(|m| m.room).collect();
    set.insert("main".into());
    println!("rooms (★ joined):");
    for r in set {
        println!("  {} {r}", if cfg.rooms.contains(&r) { "★" } else { " " });
    }
    Ok(())
}
fn cmd_log(room: Option<String>, n: usize) -> Result<()> {
    let cfg = load_config()?;
    let _ = git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
    let want: Vec<String> = room.map(|r| vec![r]).unwrap_or_else(|| cfg.rooms.clone());
    let msgs: Vec<Message> = all_messages(&cfg).into_iter().filter(|m| want.contains(&m.room)).collect();
    let start = msgs.len().saturating_sub(n);
    println!("conversation in [{}] — {} of {} message(s):", want.join(","), msgs.len() - start, msgs.len());
    for m in &msgs[start..] {
        let refd = m.reference.as_deref().map(|r| format!(" ↩{}", &r[..r.len().min(6)])).unwrap_or_default();
        println!("  {}  {:<22} → {:<18} [{}]{} {}", m.ts, m.from, m.to, m.room, refd, m.subject);
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init { role, host, repo, sid, git_user } => cmd_init(role, host, repo, sid, git_user),
        Cmd::Whoami => cmd_whoami(),
        Cmd::Register { note } => cmd_register(note),
        Cmd::Post { to, kind, subject, r#ref, repo, room, local } => cmd_post(to, kind, subject, r#ref, repo, room, local),
        Cmd::Inbox { count } => cmd_inbox(count),
        Cmd::Read { id } => cmd_read(id),
        Cmd::Ack { id, all } => cmd_ack(id, all),
        Cmd::Sync => cmd_sync(),
        Cmd::Nudge => cmd_nudge(),
        Cmd::Beat => cmd_beat(),
        Cmd::Presence => cmd_presence(),
        Cmd::Leave => cmd_leave(),
        Cmd::Join { room } => cmd_join(room),
        Cmd::Rooms => cmd_rooms(),
        Cmd::Log { room, n } => cmd_log(room, n),
    }
}

#[cfg(test)]
mod tests {
    use super::sudo_target;

    // Local accounts on the test's mental model: root + paul + admin exist.
    fn exists(u: &str) -> bool { matches!(u, "root" | "paul" | "admin") }

    #[test]
    fn prime_root_tool_paul_repo_escalates() {
        // prime: tool runs as root, repo owned by paul -> sudo -u paul.
        assert_eq!(sudo_target(Some("paul"), "root", exists), Some("paul".into()));
    }
    #[test]
    fn cross_device_github_owner_does_not_escalate() {
        // THE BUG: a node mis-told to pass the GitHub repo owner. Not a local
        // user -> run git directly as the current user instead of failing sudo.
        assert_eq!(sudo_target(Some("thepictishbeast"), "admin", exists), None);
    }
    #[test]
    fn git_user_equals_current_no_redundant_sudo() {
        assert_eq!(sudo_target(Some("admin"), "admin", exists), None);
    }
    #[test]
    fn unset_git_user_no_sudo() {
        assert_eq!(sudo_target(None, "root", exists), None);
    }
    #[test]
    fn empty_git_user_no_sudo() {
        assert_eq!(sudo_target(Some(""), "root", exists), None);
    }
}
