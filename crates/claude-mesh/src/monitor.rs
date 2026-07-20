//! Live full-screen mesh monitor (`claude-mesh monitor`) — ratatui rebuild.
//!
//! A polished, stays-open view of the conversation you can type into: bordered
//! panes, per-session colour, a real scrollback, a multi-line paste-safe compose
//! box, live presence, friendly names (the owner is just "Paul"; duplicate roles
//! disambiguate as governor¹/governor²), and an @-picker that addresses a specific
//! session so the message injects into that session's next prompt.
//!
//! Rendering is ratatui; the network (git pull/push) runs off-thread so the UI
//! never blocks, and messages render from the LOCAL bus each tick.

use std::collections::HashMap;
use std::fs;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use ratatui::{
    crossterm::{
        event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
    },
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::{all_messages, load_config, send_from_monitor, verify_state, Config, Message};

const PRESENCE_TTL: i64 = 900;

// ── friendly names ────────────────────────────────────────────────────────────
fn role_of(id: &str) -> String {
    id.split('@').next().unwrap_or(id).to_string()
}

/// Map canonical ids (role@host#sid8) → clean display names. The owner is just
/// "Paul". Everyone else is their bare role ("governor", "substrate") — a number
/// is added ONLY when two or more of that role are present at the same time
/// (governor 1, governor 2), so you never see noise like `#adc1a8f3` or a stray
/// superscript on a lone session.
fn display_map(all_ids: &[String], present: &[String]) -> HashMap<String, String> {
    let mut pres: Vec<String> = present.to_vec();
    pres.sort();
    pres.dedup();
    let mut present_by_role: HashMap<String, Vec<String>> = HashMap::new();
    for id in &pres {
        present_by_role.entry(role_of(id)).or_default().push(id.clone());
    }
    let mut numbered: HashMap<String, String> = HashMap::new();
    for (role, list) in &present_by_role {
        if list.len() > 1 && role != "owner" {
            for (i, id) in list.iter().enumerate() {
                numbered.insert(id.clone(), format!("{role} {}", i + 1));
            }
        }
    }
    let mut out = HashMap::new();
    let mut all: Vec<String> = all_ids.to_vec();
    all.sort();
    all.dedup();
    for id in &all {
        let name = if let Some(n) = numbered.get(id) {
            n.clone()
        } else if role_of(id) == "owner" {
            "Paul".to_string()
        } else {
            role_of(id)
        };
        out.insert(id.clone(), name);
    }
    out
}
fn short(id: &str) -> String {
    role_of(id)
}
fn name_of(map: &HashMap<String, String>, id: &str) -> String {
    map.get(id).cloned().unwrap_or_else(|| short(id))
}

/// A stable colour per display name so each session reads as one voice.
fn sender_color(name: &str) -> Color {
    if name == "Paul" {
        return Color::White;
    }
    const PAL: [Color; 6] = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta, Color::LightBlue, Color::LightRed];
    let h = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    PAL[(h as usize) % PAL.len()]
}

// ── data ───────────────────────────────────────────────────────────────────────
fn load_msgs(cfg: &Config, room: &Option<String>) -> Vec<Message> {
    let want: Vec<String> = room.clone().map(|r| vec![r]).unwrap_or_else(|| cfg.rooms.clone());
    all_messages(cfg).into_iter().filter(|m| want.contains(&m.room)).collect()
}

/// Ids currently present (a `nodes/*.status` heartbeat within the TTL).
fn present_ids(cfg: &Config) -> Vec<String> {
    let dir = PathBuf::from(&cfg.repo).join("nodes");
    let now = Utc::now();
    let mut out = Vec::new();
    for f in fs::read_dir(&dir).into_iter().flatten().flatten() {
        if let Some(v) = fs::read_to_string(f.path()).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            if let (Some(id), Some(ls)) = (v.get("id").and_then(|x| x.as_str()), v.get("last_seen").and_then(|x| x.as_str())) {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ls) {
                    let age = (now - dt.with_timezone(&Utc)).num_seconds();
                    if (0..PRESENCE_TTL).contains(&age) {
                        out.push(id.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// ── app state ───────────────────────────────────────────────────────────────────
#[derive(PartialEq)]
enum Focus {
    Subject,
    Body,
}
struct App {
    cfg: Config,
    room: Option<String>,
    room_label: String,
    msgs: Vec<Message>,
    present: Vec<String>,
    names: HashMap<String, String>,
    scroll: usize, // lines up from the bottom; 0 = pinned to latest
    full: bool,
    subject: String,
    body: String,
    focus: Focus,
    to: String,          // recipient id, or "all"
    to_label: String,    // friendly recipient label
    picker: Option<usize>, // Some(selected) when the @-picker is open
    status: String,
}

impl App {
    fn refresh(&mut self) {
        self.msgs = load_msgs(&self.cfg, &self.room);
        self.present = present_ids(&self.cfg);
        let mut ids: Vec<String> = self.msgs.iter().map(|m| m.from.clone()).collect();
        ids.extend(self.present.iter().cloned());
        ids.push(self.cfg.id.clone());
        self.names = display_map(&ids, &self.present);
        self.to_label = if self.to == "all" { "everyone".into() } else { name_of(&self.names, &self.to) };
    }
    fn insert(&mut self, s: &str) {
        match self.focus {
            Focus::Subject => self.subject.push_str(&s.replace(['\n', '\r'], " ")),
            Focus::Body => self.body.push_str(s),
        }
    }
    fn backspace(&mut self) {
        match self.focus {
            Focus::Subject => { self.subject.pop(); }
            Focus::Body => { self.body.pop(); }
        }
    }
    /// The @-picker's rows as (target_id, display_label), in ONE canonical order so
    /// the renderer and the key handler agree — otherwise the highlighted row could
    /// map to a different recipient. Row 0 is always broadcast ("all").
    fn picker_targets(&self) -> Vec<(String, String)> {
        let mut ids: Vec<String> = self.present.clone();
        ids.sort();
        ids.dedup();
        let mut rest: Vec<(String, String)> = ids.iter().map(|id| (id.clone(), name_of(&self.names, id))).collect();
        rest.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let mut v = vec![("all".to_string(), "everyone".to_string())];
        v.extend(rest);
        v
    }
}

// ── rendering ────────────────────────────────────────────────────────────────────
/// Wrap the conversation into styled lines for the given inner width.
fn conversation_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut out: Vec<Line> = Vec::new();
    for m in &app.msgs {
        let name = name_of(&app.names, &m.from);
        let color = sender_color(&name);
        let hhmm = m.ts.get(11..16).unwrap_or("").to_string();
        let to_disp = if m.to == "all" { "all".to_string() } else { name_of(&app.names, &m.to) };
        let vmark = match verify_state(m, &app.cfg) {
            "verified" => Span::styled(" ✓", Style::default().fg(Color::Green)),
            "FORGED" => Span::styled(" ‼FORGED", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            _ => Span::raw(""),
        };
        out.push(Line::from(vec![
            Span::styled(name, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {hhmm}  → {to_disp}"), dim),
            vmark,
        ]));
        for l in crate::wrap(m.subject.trim(), width, "  ") {
            out.push(Line::from(Span::styled(l, Style::default().add_modifier(Modifier::BOLD))));
        }
        let body = m.body.trim();
        if !body.is_empty() {
            let shown = if app.full || body.chars().count() <= 600 {
                body.to_string()
            } else {
                format!("{}…", body.chars().take(600).collect::<String>())
            };
            for l in crate::wrap(&shown, width, "  ") {
                out.push(Line::from(Span::raw(l)));
            }
            if !app.full && body.chars().count() > 600 {
                out.push(Line::from(Span::styled("  … Ctrl-F for the full message", dim)));
            }
        }
        out.push(Line::from(""));
    }
    out
}

fn render(f: &mut Frame, app: &mut App) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(7), Constraint::Length(1)])
        .split(f.area());

    // ── top bar: room · presence · sync ──
    let mut top: Vec<Span> = vec![
        Span::styled(format!(" mesh #{} ", app.room_label), Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    let mut present_names: Vec<String> = app.present.iter().map(|id| name_of(&app.names, id)).collect();
    present_names.sort();
    present_names.dedup();
    top.push(Span::styled(format!("● {} here", present_names.len()), Style::default().fg(Color::Green)));
    top.push(Span::styled(format!("  {}", present_names.join(" · ")), dim));
    f.render_widget(Paragraph::new(Line::from(top)), chunks[0]);

    // ── conversation ──
    let inner_w = chunks[1].width.saturating_sub(2).max(8) as usize;
    let inner_h = chunks[1].height.saturating_sub(2).max(1) as usize;
    let lines = conversation_lines(app, inner_w);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    if app.scroll > max_scroll {
        app.scroll = max_scroll;
    }
    let offset = total.saturating_sub(inner_h).saturating_sub(app.scroll);
    let pos = if max_scroll == 0 { "".to_string() } else { format!(" ↑{}/{} ", app.scroll, max_scroll) };
    let title = format!(" conversation {}{}", if app.full { "· full " } else { "" }, pos);
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)).scroll((offset as u16, 0)),
        chunks[1],
    );

    // ── compose ──
    let ct = Line::from(vec![
        Span::raw(" compose "),
        Span::styled(format!("→ {}", app.to_label), Style::default().fg(Color::Yellow)),
        Span::raw(" "),
    ]);
    let compose = Block::default().borders(Borders::ALL).title(ct);
    let carea = compose.inner(chunks[2]);
    f.render_widget(compose, chunks[2]);
    let crows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(carea);
    let subj_focus = app.focus == Focus::Subject;
    let sp = if subj_focus { Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) } else { dim };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("Subj ", sp), Span::raw(app.subject.clone())])),
        crows[0],
    );
    let bp = if !subj_focus { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) } else { dim };
    let body_para = Paragraph::new(vec![Line::from(vec![Span::styled("› ", bp), Span::raw(app.body.clone())])])
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(body_para, crows[1]);

    // ── help / status ──
    let help = if app.picker.is_some() {
        "↑/↓ pick · Enter choose · Esc cancel".to_string()
    } else {
        format!("Tab fields · @ session · Ctrl-F full · Ctrl-R sync · PgUp/Dn · Enter send · Ctrl-C quit   {}", app.status)
    };
    f.render_widget(Paragraph::new(Span::styled(help, Style::default().fg(Color::Green))), chunks[3]);

    // ── @-picker overlay ──
    if let Some(sel) = app.picker {
        let targets = app.picker_targets();
        let h = (targets.len() as u16 + 2).min(12);
        let w = 32u16.min(f.area().width);
        let area = Rect { x: chunks[2].x + 1, y: chunks[2].y.saturating_sub(h), width: w, height: h };
        let items: Vec<ListItem> = targets.iter().enumerate().map(|(i, (_, disp))| {
            let st = if i == sel { Style::default().fg(Color::Black).bg(Color::Cyan) } else { Style::default() };
            ListItem::new(Line::from(Span::styled(format!(" @{disp} "), st)))
        }).collect();
        f.render_widget(Clear, area);
        f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" address → ")), area);
    }
}

// ── terminal lifecycle ──────────────────────────────────────────────────────────
fn spawn_sync(cfg: Config) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {}
            let _ = crate::git_ok(&cfg, &["pull", "--rebase", "--autostash"]);
        }
    });
    tx
}

pub fn run(room: Option<String>, full: bool) -> Result<()> {
    let cfg = load_config()?;
    let mut terminal = ratatui::init(); // raw mode + alt screen + panic-restore hook
    let _ = execute!(stdout(), EnableBracketedPaste);
    let sync_tx = spawn_sync(cfg.clone());
    let _ = sync_tx.send(());

    let room_label = room.clone().unwrap_or_else(|| cfg.rooms.join(","));
    let mut app = App {
        cfg,
        room,
        room_label,
        msgs: Vec::new(),
        present: Vec::new(),
        names: HashMap::new(),
        scroll: 0,
        full,
        subject: String::new(),
        body: String::new(),
        focus: Focus::Body,
        to: "all".into(),
        to_label: "everyone".into(),
        picker: None,
        status: String::new(),
    };
    app.refresh();

    let res = event_loop(&mut terminal, &mut app, &sync_tx);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    sync_tx: &mpsc::Sender<()>,
) -> Result<()> {
    let mut last_refresh = Instant::now();
    let mut last_sync = Instant::now();
    let mut dirty = true;
    loop {
        if last_refresh.elapsed() >= Duration::from_millis(600) {
            let before = (app.msgs.len(), app.msgs.last().map(|m| m.id.clone()), app.present.len());
            app.refresh();
            if before != (app.msgs.len(), app.msgs.last().map(|m| m.id.clone()), app.present.len()) {
                dirty = true;
            }
            last_refresh = Instant::now();
            if last_sync.elapsed() >= Duration::from_secs(15) {
                let _ = sync_tx.send(());
                last_sync = Instant::now();
            }
        }
        if dirty {
            terminal.draw(|f| render(f, app))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        match event::read()? {
            Event::Paste(text) => {
                app.insert(&text);
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if handle_key(app, k.code, k.modifiers, sync_tx, &mut last_sync) {
                    break;
                }
                dirty = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Returns true to quit.
fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    sync_tx: &mpsc::Sender<()>,
    last_sync: &mut Instant,
) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // ── @-picker mode ──
    if let Some(sel) = app.picker {
        let targets = app.picker_targets();
        match code {
            KeyCode::Up => app.picker = Some(sel.saturating_sub(1)),
            KeyCode::Down => app.picker = Some((sel + 1).min(targets.len().saturating_sub(1))),
            KeyCode::Esc => app.picker = None,
            KeyCode::Enter => {
                if let Some((id, label)) = targets.get(sel) {
                    app.to = id.clone();
                    app.to_label = label.clone();
                }
                app.picker = None;
            }
            _ => {}
        }
        return false;
    }
    app.status.clear();
    match code {
        KeyCode::Char('c') | KeyCode::Char('q') if ctrl => return true,
        KeyCode::Char('f') if ctrl => app.full = !app.full,
        KeyCode::Char('r') if ctrl => {
            let _ = sync_tx.send(());
            *last_sync = Instant::now();
            app.status = "syncing…".into();
        }
        KeyCode::Char('@') => app.picker = Some(0),
        KeyCode::Tab | KeyCode::BackTab => app.focus = if app.focus == Focus::Subject { Focus::Body } else { Focus::Subject },
        KeyCode::PageUp => app.scroll = app.scroll.saturating_add(8),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(8),
        KeyCode::Up => app.scroll = app.scroll.saturating_add(1),
        KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Home => app.scroll = usize::MAX / 2,
        KeyCode::End => app.scroll = 0,
        KeyCode::Enter => {
            let subject = app.subject.trim().to_string();
            let body = app.body.trim().to_string();
            if !subject.is_empty() || !body.is_empty() {
                let room = app.room.clone().unwrap_or_else(|| app.cfg.rooms.first().cloned().unwrap_or_else(|| "main".into()));
                match send_from_monitor(&app.cfg, &app.to, &room, &subject, &body) {
                    Ok(pushed) => {
                        app.status = if pushed { "sent ✓".into() } else { "sent (will sync)".into() };
                        app.subject.clear();
                        app.body.clear();
                        app.scroll = 0;
                        app.refresh();
                    }
                    Err(e) => app.status = format!("send failed: {e}"),
                }
            }
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Char(c) if !ctrl => app.insert(&c.to_string()),
        _ => {}
    }
    false
}
