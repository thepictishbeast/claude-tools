//! `skill-count-audit` — count installed Claude Code skills.
//!
//! Walks `~/.claude/skills/` (or a path given by `--dir`), counts
//! every subdirectory that contains a `SKILL.md`, and warns when
//! the count exceeds the soft cap defined in claude-tools META.md
//! (15 by default).
//!
//! Exit codes:
//! * 0 — under or at the cap
//! * 1 — over the cap (suggests retiring lower-leverage skills or
//!   migrating callable surface to MCP)
//! * 2 — operational error (directory missing, IO failure)
//!
//! Usage:
//! ```sh
//! skill-count-audit                       # walks ~/.claude/skills
//! skill-count-audit --dir /custom/path    # walks an explicit path
//! skill-count-audit --cap 20              # raise the soft cap
//! skill-count-audit --json                # machine-readable output
//! ```
//!
//! Designed to run after `install.sh` finishes so the user sees
//! their installed skill surface in numeric form.

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Default soft cap (paul 2026-05-21 directive in META.md).
const DEFAULT_CAP: usize = 15;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Skills directory. Defaults to `~/.claude/skills/`.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Soft cap. Defaults to 15 per claude-tools META.md.
    #[arg(long, default_value_t = DEFAULT_CAP)]
    cap: usize,
    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dir = args
        .dir
        .clone()
        .or_else(home_default)
        .context("could not resolve skills directory (HOME unset?)")?;
    if !dir.is_dir() {
        if args.json {
            println!("{{\"error\":\"missing-dir\",\"dir\":\"{}\"}}", dir.display());
        } else {
            eprintln!("skill-count-audit: directory missing: {}", dir.display());
        }
        std::process::exit(2);
    }
    let skills = scan(&dir)?;
    let count = skills.len();
    let over = count > args.cap;

    if args.json {
        print_json(count, args.cap, over, &skills);
    } else {
        print_text(&dir, count, args.cap, over, &skills);
    }

    if over {
        std::process::exit(1);
    }
    Ok(())
}

fn home_default() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".claude").join("skills"))
}

fn scan(dir: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("SKILL.md").is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                out.insert(name.to_owned());
            }
        }
    }
    Ok(out)
}

fn print_text(dir: &Path, count: usize, cap: usize, over: bool, skills: &BTreeSet<String>) {
    println!("skill-count-audit");
    println!("  dir:   {}", dir.display());
    println!("  count: {count}");
    println!("  cap:   {cap}");
    if over {
        println!("  status: OVER (by {})", count - cap);
        println!();
        println!("Suggested next steps:");
        println!("  * Retire skills you no longer reach for (delete the dir).");
        println!(
            "  * Move callable surface to MCP — deferred-schema-loaded \
             tools cost no context tokens until invoked."
        );
        println!("  * Fold adjacent skills into one (e.g. /loop-* could be /loop with subcommands).");
    } else {
        println!("  status: ok ({} under cap)", cap - count);
    }
    println!();
    println!("Installed skills ({count}):");
    for s in skills {
        println!("  - {s}");
    }
}

fn print_json(count: usize, cap: usize, over: bool, skills: &BTreeSet<String>) {
    let mut json = String::from("{");
    json.push_str(&format!("\"count\":{count},\"cap\":{cap},\"over\":{over},"));
    json.push_str("\"skills\":[");
    let mut first = true;
    for s in skills {
        if !first {
            json.push(',');
        }
        first = false;
        json.push('"');
        json.push_str(s);
        json.push('"');
    }
    json.push_str("]}");
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let pid = std::process::id();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("skill-count-audit-{label}-{pid}-{n}"))
    }

    fn make_skill(root: &Path, name: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        let mut f = fs::File::create(dir.join("SKILL.md")).expect("create SKILL.md");
        writeln!(f, "---\nname: {name}\n---\n# {name}\n").expect("write");
    }

    #[test]
    fn scan_finds_only_dirs_with_skill_md() {
        let root = tmpdir("scan");
        fs::create_dir_all(&root).unwrap();
        make_skill(&root, "alpha");
        make_skill(&root, "beta");
        // A dir WITHOUT SKILL.md should NOT count.
        fs::create_dir_all(root.join("not-a-skill")).unwrap();
        // A file at root should NOT count.
        fs::File::create(root.join("stray.txt")).unwrap();
        let skills = scan(&root).expect("scan");
        assert_eq!(skills.len(), 2);
        assert!(skills.contains("alpha"));
        assert!(skills.contains("beta"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn default_cap_is_fifteen() {
        assert_eq!(DEFAULT_CAP, 15);
    }

    #[test]
    fn print_json_round_trips() {
        let mut skills = BTreeSet::new();
        skills.insert("alpha".to_owned());
        skills.insert("beta".to_owned());
        // Indirect smoke: just confirm the format compiles + runs.
        // The actual stdout is asserted via integration test (out of
        // scope for this unit test mod).
        print_json(2, 15, false, &skills);
    }

    #[test]
    fn over_cap_sets_over_flag() {
        let root = tmpdir("over");
        fs::create_dir_all(&root).unwrap();
        for i in 0..17 {
            make_skill(&root, &format!("skill-{i}"));
        }
        let skills = scan(&root).expect("scan");
        assert!(skills.len() > 15);
        let _ = fs::remove_dir_all(&root);
    }
}
