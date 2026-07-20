//! Live full-screen mesh monitor (`claude-mesh monitor`).
//!
//! A stays-open view of the conversation that you can also TYPE INTO — compose a
//! reply on the bottom line and press Enter to post it, without leaving the view.
//! Built on crossterm's raw mode only (no ratatui) so it reuses the crate's own
//! `wrap()` + `sender_color()` ANSI helpers directly and word-wraps to the live
//! terminal width — nothing gets cut off on the right on a small/phone screen.
//!
//! Design notes that matter:
//!   * A RAII guard + panic hook always restore the terminal (leave alt-screen,
//!     disable raw mode). A crashed TUI must never strand a phone-SSH session.
//!   * git pull/push runs on a background thread; the UI never blocks on the
//!     network (and we never pull on every refresh — that would hit the same
//!     secondary-rate-limit we already saw).
//!   * Messages render from the LOCAL bus on disk each tick; the sync thread just
//!     refreshes those files underneath us.

use std::io::{stdout, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, Show},
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::{all_messages, load_config, sender_color, send_from_monitor, verify_state, Config, Message};

/// Restores the terminal on drop no matter how we leave (return, `?`, or panic).
struct TermGuard;
impl TermGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        // Bracketed paste: the terminal wraps a paste in \e[200~ … \e[201~ so it
        // arrives as ONE Event::Paste instead of a stream of keystrokes + Enters
        // (which is why multi-line pastes used to fire a send per line).
        execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(TermGuard)
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), Show, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        prev(info);
    }));
}

/// Background git sync: a request on the channel triggers one pull. Extra requests
/// that pile up while a pull is in flight collapse into a single follow-up pull.
fn spawn_sync(cfg: Config) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {} // coalesce
            let _ = crate::git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
        }
    });
    tx
}

struct View {
    full: bool,
    verbose: bool,
    scroll: usize, // lines scrolled up from the bottom; 0 = pinned to latest
    subject: String,
    input: String, // the message body
    subject_focus: bool, // Tab toggles: true = typing the subject, false = the body
    status: String,
    room: Option<String>,
    pane_h: usize, // last drawn message-pane height, for PageUp/PageDown
}

fn load_msgs(cfg: &Config, room: &Option<String>) -> Vec<Message> {
    let want: Vec<String> = room.clone().map(|r| vec![r]).unwrap_or_else(|| cfg.rooms.clone());
    all_messages(cfg).into_iter().filter(|m| want.contains(&m.room)).collect()
}

/// Wrap every message into printable (already-ANSI-coloured) lines for the pane.
fn build_lines(cfg: &Config, msgs: &[Message], width: usize, view: &View) -> Vec<String> {
    let (cs, cd, rst) = ("\x1b[1m", "\x1b[2m", "\x1b[0m");
    let mut out = Vec::new();
    for m in msgs {
        let sc = sender_color(&m.from);
        let hhmm = m.ts.get(11..16).unwrap_or("");
        let vmark = match verify_state(m, cfg) {
            "verified" => " \x1b[32m✓\x1b[0m",
            "FORGED" => " \x1b[1;31m‼FORGED\x1b[0m",
            _ => "",
        };
        let mut head = format!("{sc}{}{rst} {cd}{} · {} → {}{rst}{}", m.from, hhmm, m.room, m.to, vmark);
        if view.verbose {
            head.push_str(&format!(" {cd}[{} · {}]{rst}", m.kind, &m.id[..m.id.len().min(8)]));
        }
        out.push(head); // header text is short; sender name is the only long field
        for l in crate::wrap(m.subject.trim(), width, "  ") {
            out.push(format!("{cs}{l}{rst}"));
        }
        if view.verbose {
            if let Some(r) = &m.reference {
                out.push(format!("{cd}  ↩ re {}{rst}", &r[..r.len().min(8)]));
            }
        }
        let body = m.body.trim();
        if !body.is_empty() {
            let shown = if view.full || body.chars().count() <= 400 {
                body.to_string()
            } else {
                format!("{}…", body.chars().take(400).collect::<String>())
            };
            for l in crate::wrap(&shown, width, "  ") {
                out.push(l);
            }
            if !view.full && body.chars().count() > 400 {
                out.push(format!("{cd}  … Ctrl-F for the full message{rst}"));
            }
        }
        out.push(String::new());
    }
    out
}

/// Render one compose field to a single visible line: newlines shown as ⏎ (a
/// pasted multi-line body stays on one line), and if it's longer than `avail`
/// columns, show the tail (so the cursor end stays visible). Returns the display
/// string and the cursor column offset within the field.
fn field_line(text: &str, avail: usize) -> (String, usize) {
    let disp: String = text.replace(['\n', '\r'], "⏎");
    let n = disp.chars().count();
    if n <= avail {
        (disp, n)
    } else {
        let tail: String = disp.chars().skip(n - avail.saturating_sub(1)).collect();
        (format!("…{tail}"), avail) // cursor sits at the right edge
    }
}

fn draw(lines: &[String], view: &mut View, cols: u16, rows: u16, room_label: &str) -> Result<()> {
    let mut o = stdout();
    let pane_h = rows.saturating_sub(4).max(1) as usize; // reserve: separator + status + subject + body
    view.pane_h = pane_h;
    let total = lines.len();
    let max_scroll = total.saturating_sub(pane_h);
    if view.scroll > max_scroll {
        view.scroll = max_scroll;
    }
    let start = total.saturating_sub(pane_h).saturating_sub(view.scroll);
    let end = (start + pane_h).min(total);

    queue!(o, Clear(ClearType::All))?;
    for (row, line) in lines[start..end].iter().enumerate() {
        queue!(o, MoveTo(0, row as u16), Print(line))?;
    }
    let sep = rows.saturating_sub(4);
    let mode = format!(
        "{}{}",
        if view.full { "FULL " } else { "" },
        if view.verbose { "VERBOSE " } else { "" },
    );
    let scrolled = if view.scroll > 0 { format!("↑{} ", view.scroll) } else { String::new() };
    queue!(o, MoveTo(0, sep), Print(format!("\x1b[2m{}\x1b[0m", "─".repeat(cols as usize))))?;
    queue!(
        o,
        MoveTo(0, sep + 1),
        Clear(ClearType::CurrentLine),
        Print(format!(
            "\x1b[2m[{room_label}] {mode}{scrolled}· Tab subj/msg · Ctrl-F full · Ctrl-V verbose · Ctrl-R sync · Ctrl-C quit\x1b[0m",
        )),
    )?;
    if !view.status.is_empty() {
        queue!(o, Print(format!("  \x1b[32m{}\x1b[0m", view.status)))?;
    }
    // Two-field compose: subject on one line, body on the next. Tab moves focus;
    // the focused prompt lights up and the cursor rests in that field.
    let (subj_p, body_p) = if view.subject_focus {
        ("\x1b[1;33mSubj›\x1b[0m ", "\x1b[2m›\x1b[0m ")
    } else {
        ("\x1b[2mSubj›\x1b[0m ", "\x1b[1;36m›\x1b[0m ")
    };
    let (subj_disp, subj_cur) = field_line(&view.subject, (cols as usize).saturating_sub(6));
    let (body_disp, body_cur) = field_line(&view.input, (cols as usize).saturating_sub(2));
    queue!(o, MoveTo(0, sep + 2), Clear(ClearType::CurrentLine), Print(format!("{subj_p}{subj_disp}")))?;
    queue!(o, MoveTo(0, sep + 3), Clear(ClearType::CurrentLine), Print(format!("{body_p}{body_disp}")))?;
    // Park the cursor in whichever field has focus (prompt widths: "Subj› " = 6, "› " = 2).
    let (crow, base, cur) = if view.subject_focus { (sep + 2, 6u16, subj_cur) } else { (sep + 3, 2u16, body_cur) };
    let col = (base + cur as u16).min(cols.saturating_sub(1));
    queue!(o, MoveTo(col, crow), Show)?;
    o.flush()?;
    Ok(())
}

pub fn run(room: Option<String>, full: bool) -> Result<()> {
    let cfg = load_config()?;
    install_panic_hook();
    let _guard = TermGuard::enter()?;
    let sync_tx = spawn_sync(cfg.clone());
    let _ = sync_tx.send(()); // pull once on open

    let post_room = room
        .clone()
        .unwrap_or_else(|| cfg.rooms.first().cloned().unwrap_or_else(|| "main".into()));
    let room_label = room.clone().unwrap_or_else(|| cfg.rooms.join(","));

    let mut view = View { full, verbose: false, scroll: 0, subject: String::new(), input: String::new(), subject_focus: false, status: String::new(), room, pane_h: 1 };
    let mut msgs = load_msgs(&cfg, &view.room);
    let mut last_refresh = Instant::now();
    let mut last_sync = Instant::now();
    let mut dirty = true;

    loop {
        if last_refresh.elapsed() >= Duration::from_millis(600) {
            let fresh = load_msgs(&cfg, &view.room);
            let changed = fresh.len() != msgs.len()
                || fresh.last().map(|m| &m.id) != msgs.last().map(|m| &m.id);
            if changed {
                msgs = fresh;
                dirty = true;
            }
            last_refresh = Instant::now();
            if last_sync.elapsed() >= Duration::from_secs(15) {
                let _ = sync_tx.send(());
                last_sync = Instant::now();
            }
        }

        if dirty {
            let (cols, rows) = terminal::size().unwrap_or((80, 24));
            let width = (cols as usize).saturating_sub(1).clamp(24, 300);
            let lines = build_lines(&cfg, &msgs, width, &view);
            draw(&lines, &mut view, cols, rows, &room_label)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(150))? {
            match event::read()? {
                Event::Paste(text) => {
                    // The whole paste (newlines and all) enters the focused field as one
                    // chunk — never a run of Enters — so multi-line pastes stop firing a
                    // send per line. Subject is single-line, so flatten newlines there.
                    if view.subject_focus {
                        view.subject.push_str(&text.replace(['\n', '\r'], " "));
                    } else {
                        view.input.push_str(&text);
                    }
                    dirty = true;
                }
                Event::Key(k) => {
                if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                view.status.clear();
                match k.code {
                    KeyCode::Char('c') | KeyCode::Char('q') if ctrl => break,
                    KeyCode::Char('f') if ctrl => view.full = !view.full,
                    KeyCode::F(2) => view.full = !view.full,
                    KeyCode::Char('v') if ctrl => view.verbose = !view.verbose,
                    KeyCode::F(3) => view.verbose = !view.verbose,
                    KeyCode::Char('r') if ctrl => {
                        let _ = sync_tx.send(());
                        last_sync = Instant::now();
                        view.status = "syncing…".into();
                    }
                    KeyCode::PageUp => view.scroll = view.scroll.saturating_add(view.pane_h / 2 + 1),
                    KeyCode::PageDown => view.scroll = view.scroll.saturating_sub(view.pane_h / 2 + 1),
                    KeyCode::Home => view.scroll = usize::MAX / 2,
                    KeyCode::End => view.scroll = 0,
                    KeyCode::Tab | KeyCode::BackTab => view.subject_focus = !view.subject_focus,
                    KeyCode::Enter => {
                        let subject = view.subject.trim().to_string();
                        let body = view.input.trim().to_string();
                        if !body.is_empty() || !subject.is_empty() {
                            match send_from_monitor(&cfg, "all", &post_room, &subject, &body) {
                                Ok(pushed) => {
                                    view.status = if pushed { "sent ✓".into() } else { "sent (local — will sync)".into() };
                                    view.subject.clear();
                                    view.input.clear();
                                    view.subject_focus = false;
                                    view.scroll = 0;
                                    msgs = load_msgs(&cfg, &view.room); // show our own line immediately
                                }
                                Err(e) => view.status = format!("send failed: {e}"),
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if view.subject_focus { view.subject.pop(); } else { view.input.pop(); }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        if view.subject_focus { view.subject.push(c); } else { view.input.push(c); }
                    }
                    _ => {}
                }
                dirty = true;
                }
                _ => dirty = true, // resize / focus / other — just repaint
            }
        }
    }
    Ok(())
}
