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
    scur: usize, // cursor (char index) in subject
    body: String,
    cur: usize,  // cursor (char index) in body
    body_scroll: usize, // top visual row of the body viewport
    body_w: usize,      // body wrap width from the last render (for arrow nav)
    last_total: usize,  // conversation line count at last render (scroll anchoring)
    quit_armed: bool,   // Ctrl-C once with an unsent draft arms; twice quits
    post_room: String,  // the room a sent message lands in (shown in compose title)
    focus: Focus,
    to: String,          // recipient id, or "all"
    to_label: String,    // friendly recipient label
    picker: Option<usize>, // Some(selected) when the @-picker is open
    picker_snap: Vec<(String, String)>, // FROZEN target list while the picker is open
    status: String,
}

fn byte_of(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

/// Split `text` into visual rows of at most `width` display columns, honoring hard
/// newlines. Each row = (start char index, row text). Always at least one row —
/// this is what lets the compose box grow and the cursor map to a screen cell.
fn visual_rows(text: &str, width: usize) -> Vec<(usize, String)> {
    let w = width.max(4);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_w = 0usize;
    let mut row_start = 0usize;
    let mut idx = 0usize;
    for c in text.chars() {
        if c == '\n' {
            rows.push((row_start, std::mem::take(&mut row)));
            row_w = 0;
            idx += 1;
            row_start = idx;
            continue;
        }
        let cw = crate::ch_w(c);
        if row_w + cw > w && !row.is_empty() {
            rows.push((row_start, std::mem::take(&mut row)));
            row_w = 0;
            row_start = idx;
        }
        row.push(c);
        row_w += cw;
        idx += 1;
    }
    rows.push((row_start, row));
    rows
}

/// (visual row, display column) of char-cursor `cur` within `rows`.
fn cursor_rc(rows: &[(usize, String)], cur: usize) -> (usize, usize) {
    for (i, (start, s)) in rows.iter().enumerate() {
        let len = s.chars().count();
        let end = start + len;
        if cur < *start {
            continue;
        }
        if cur <= end {
            // On a soft wrap the next row starts exactly at `end`; the cursor
            // belongs at that row's column 0, not past this row's edge.
            if cur == end {
                if let Some((ns, _)) = rows.get(i + 1) {
                    if *ns == end {
                        continue;
                    }
                }
            }
            let col: usize = s.chars().take(cur - start).map(crate::ch_w).sum();
            return (i, col);
        }
    }
    let last = rows.len().saturating_sub(1);
    let col = rows.last().map(|(_, s)| crate::str_w(s)).unwrap_or(0);
    (last, col)
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
            Focus::Subject => {
                let t = s.replace(['\n', '\r'], " "); // subject stays single-line
                let b = byte_of(&self.subject, self.scur);
                self.subject.insert_str(b, &t);
                self.scur += t.chars().count();
            }
            Focus::Body => {
                let t = s.replace('\r', "");
                let b = byte_of(&self.body, self.cur);
                self.body.insert_str(b, &t);
                self.cur += t.chars().count();
            }
        }
    }
    fn backspace(&mut self) {
        match self.focus {
            Focus::Subject => {
                if self.scur > 0 {
                    let (b0, b1) = (byte_of(&self.subject, self.scur - 1), byte_of(&self.subject, self.scur));
                    self.subject.replace_range(b0..b1, "");
                    self.scur -= 1;
                }
            }
            Focus::Body => {
                if self.cur > 0 {
                    let (b0, b1) = (byte_of(&self.body, self.cur - 1), byte_of(&self.body, self.cur));
                    self.body.replace_range(b0..b1, "");
                    self.cur -= 1;
                }
            }
        }
    }
    fn delete_at(&mut self) {
        match self.focus {
            Focus::Subject => {
                if self.scur < self.subject.chars().count() {
                    let (b0, b1) = (byte_of(&self.subject, self.scur), byte_of(&self.subject, self.scur + 1));
                    self.subject.replace_range(b0..b1, "");
                }
            }
            Focus::Body => {
                if self.cur < self.body.chars().count() {
                    let (b0, b1) = (byte_of(&self.body, self.cur), byte_of(&self.body, self.cur + 1));
                    self.body.replace_range(b0..b1, "");
                }
            }
        }
    }
    fn move_left(&mut self) {
        match self.focus {
            Focus::Subject => self.scur = self.scur.saturating_sub(1),
            Focus::Body => self.cur = self.cur.saturating_sub(1),
        }
    }
    fn move_right(&mut self) {
        match self.focus {
            Focus::Subject => self.scur = (self.scur + 1).min(self.subject.chars().count()),
            Focus::Body => self.cur = (self.cur + 1).min(self.body.chars().count()),
        }
    }
    /// Up/Down within the body's wrapped rows. Returns false when moving up from
    /// row 0 (caller shifts focus to the subject line).
    fn move_vert(&mut self, down: bool) -> bool {
        let rows = visual_rows(&self.body, self.body_w);
        let (r, _) = cursor_rc(&rows, self.cur);
        let colc = self.cur.saturating_sub(rows[r].0); // column in chars
        if down {
            if r + 1 < rows.len() {
                let (ns, s) = &rows[r + 1];
                self.cur = ns + colc.min(s.chars().count());
            }
            true
        } else if r == 0 {
            false
        } else {
            let (ps, s) = &rows[r - 1];
            self.cur = ps + colc.min(s.chars().count());
            true
        }
    }
    fn home_end(&mut self, end: bool) {
        match self.focus {
            Focus::Subject => self.scur = if end { self.subject.chars().count() } else { 0 },
            Focus::Body => {
                let rows = visual_rows(&self.body, self.body_w);
                let (r, _) = cursor_rc(&rows, self.cur);
                let (start, s) = &rows[r];
                self.cur = if end { start + s.chars().count() } else { *start };
            }
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
    // Compose height GROWS with the body (up to a third of the screen) so long or
    // pasted text never runs out of sight below the box.
    let area_w = f.area().width as usize;
    app.body_w = area_w.saturating_sub(4).max(4); // borders (2) + "› " prefix (2)
    let rows = visual_rows(&app.body, app.body_w);
    let max_body = ((f.area().height as usize) / 3).max(3);
    let body_h = rows.len().clamp(1, max_body);
    let compose_h = (body_h + 3) as u16; // + subject line + 2 border rows
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(compose_h), Constraint::Length(1)])
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
    // Anchor while reading: if scrolled up and new lines appended, grow scroll by
    // the same amount so the text doesn't slide out from under the reader.
    if app.scroll > 0 && app.last_total > 0 && total > app.last_total {
        app.scroll += total - app.last_total;
    }
    app.last_total = total;
    let max_scroll = total.saturating_sub(inner_h);
    if app.scroll > max_scroll {
        app.scroll = max_scroll;
    }
    let offset = total.saturating_sub(inner_h).saturating_sub(app.scroll);
    let pos = if max_scroll == 0 { "".to_string() } else { format!(" ↑{}/{} ", app.scroll, max_scroll) };
    let title = format!(" conversation {}{}", if app.full { "· full " } else { "" }, pos);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((offset.min(u16::MAX as usize) as u16, 0)),
        chunks[1],
    );

    // ── compose (multi-line editor with a real cursor) ──
    let ct = Line::from(vec![
        Span::raw(" compose "),
        Span::styled(format!("→ {}", app.to_label), Style::default().fg(Color::Yellow)),
        Span::styled(format!(" in #{} ", app.post_room), dim), // where a send LANDS
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
    // Body viewport: keep the cursor's row visible; rows beyond the box scroll.
    let (crow_i, ccol) = cursor_rc(&rows, app.cur);
    let view_h = crows[1].height.max(1) as usize;
    if crow_i < app.body_scroll {
        app.body_scroll = crow_i;
    }
    if crow_i >= app.body_scroll + view_h {
        app.body_scroll = crow_i + 1 - view_h;
    }
    let blines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(app.body_scroll)
        .take(view_h)
        .map(|(i, (_, s))| {
            let prefix = if i == 0 { Span::styled("› ", bp) } else { Span::styled("  ", dim) };
            Line::from(vec![prefix, Span::raw(s.clone())])
        })
        .collect();
    f.render_widget(Paragraph::new(blines), crows[1]);
    // Show the REAL terminal cursor in the focused field so you always know
    // where the next character lands.
    if app.picker.is_none() {
        let right_edge = carea.x + carea.width.saturating_sub(1);
        match app.focus {
            Focus::Body => {
                let y = crows[1].y + (crow_i - app.body_scroll) as u16;
                let x = crows[1].x + 2 + ccol.min(u16::MAX as usize) as u16;
                f.set_cursor_position(ratatui::layout::Position::new(x.min(right_edge), y));
            }
            Focus::Subject => {
                let sw: usize = app.subject.chars().take(app.scur).map(crate::ch_w).sum();
                let x = crows[0].x + 5 + sw.min(u16::MAX as usize) as u16;
                f.set_cursor_position(ratatui::layout::Position::new(x.min(right_edge), crows[0].y));
            }
        }
    }

    // ── help / status ──
    let help = if app.picker.is_some() {
        "↑/↓ pick · Enter choose · Esc cancel".to_string()
    } else {
        // Status outranks the key list — a warning must never be pushed off-screen.
        if app.status.is_empty() {
            "arrows edit · Enter newline · ^S SEND · Tab fields · @ address · ^F full · PgUp/Dn scroll · ^C quit".to_string()
        } else {
            format!("▶ {}", app.status)
        }
    };
    f.render_widget(Paragraph::new(Span::styled(help, Style::default().fg(Color::Green))), chunks[3]);

    // ── @-picker overlay (renders the FROZEN snapshot, not live present) ──
    if let Some(sel) = app.picker {
        let targets = &app.picker_snap;
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
    // ratatui's panic hook restores raw mode + alt screen, but knows nothing about
    // bracketed paste — chain a hook so a panic can't leave the shell spewing
    // ~200~/~201~ around every paste.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste);
        prev_hook(info);
    }));
    let sync_tx = spawn_sync(cfg.clone());
    let _ = sync_tx.send(());

    let room_label = room.clone().unwrap_or_else(|| cfg.rooms.join(","));
    let post_room = room.clone().unwrap_or_else(|| cfg.rooms.first().cloned().unwrap_or_else(|| "main".into()));
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
        scur: 0,
        body: String::new(),
        cur: 0,
        body_scroll: 0,
        body_w: 60,
        last_total: 0,
        quit_armed: false,
        post_room,
        focus: Focus::Body,
        to: "all".into(),
        to_label: "everyone".into(),
        picker: None,
        picker_snap: Vec::new(),
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
        let targets = app.picker_snap.clone(); // frozen when the picker opened
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
    let was_armed = app.quit_armed;
    app.quit_armed = false; // any key other than a second Ctrl-C disarms
    match code {
        KeyCode::Char('c') | KeyCode::Char('q') if ctrl => {
            // An unsent draft is not destroyed by one reflexive Ctrl-C.
            if (!app.subject.is_empty() || !app.body.is_empty()) && !was_armed {
                app.quit_armed = true;
                app.status = "unsent draft — Ctrl-C again to quit".into();
            } else {
                return true;
            }
        }
        KeyCode::Char('f') if ctrl => app.full = !app.full,
        KeyCode::Char('r') if ctrl => {
            let _ = sync_tx.send(());
            *last_sync = Instant::now();
            app.status = "syncing…".into();
        }
        // '@' opens the address picker only when compose is empty (so you pick a
        // recipient first); once you're typing, '@' is a literal char (paul@host).
        // Ctrl-T (re)opens the picker any time. Both FREEZE the target list.
        KeyCode::Char('@') if app.subject.is_empty() && app.body.is_empty() => {
            app.picker_snap = app.picker_targets();
            app.picker = Some(0);
        }
        KeyCode::Char('t') if ctrl => {
            app.picker_snap = app.picker_targets();
            app.picker = Some(0);
        }
        KeyCode::Tab | KeyCode::BackTab => app.focus = if app.focus == Focus::Subject { Focus::Body } else { Focus::Subject },
        // Conversation scroll: PgUp/PgDn pages, Ctrl-↑/↓ lines. Plain arrows EDIT.
        KeyCode::PageUp => app.scroll = app.scroll.saturating_add(8),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(8),
        KeyCode::Up if ctrl => app.scroll = app.scroll.saturating_add(1),
        KeyCode::Down if ctrl => app.scroll = app.scroll.saturating_sub(1),
        // Editor navigation — the cursor moves through the text like any editor.
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up => {
            // Up from the body's first row hops into the subject line.
            let leave = app.focus == Focus::Body && !app.move_vert(false);
            if leave {
                app.focus = Focus::Subject;
            }
        }
        KeyCode::Down => {
            if app.focus == Focus::Subject {
                app.focus = Focus::Body;
            } else {
                app.move_vert(true);
            }
        }
        KeyCode::Home => app.home_end(false),
        KeyCode::End => app.home_end(true),
        KeyCode::Delete => app.delete_at(),
        // Enter = NEWLINE in the body, like any editor (from the subject it hops
        // into the body). Sending is EXPLICIT: Ctrl-S. Note a "Ctrl-J newline"
        // binding is impossible — Ctrl-J and Enter are the same byte (0x0A), so
        // the previous design submitted when you wanted a new line.
        KeyCode::Enter => match app.focus {
            Focus::Subject => app.focus = Focus::Body,
            Focus::Body => app.insert("\n"),
        },
        KeyCode::Char('s') if ctrl => {
            let subject = app.subject.trim().to_string();
            let body = app.body.trim().to_string();
            if !subject.is_empty() || !body.is_empty() {
                match send_from_monitor(&app.cfg, &app.to, &app.post_room.clone(), &subject, &body) {
                    Ok(delivery) => {
                        // never show a bare ✓ for a message that only hit disk —
                        // the compose line must not overstate delivery either
                        app.status = match delivery {
                            crate::Delivery::Pushed => "sent ✓".into(),
                            crate::Delivery::Committed => "sent (will sync)".into(),
                            crate::Delivery::OnDiskOnly => "on disk only — NOT committed, run sync".into(),
                        };
                        app.subject.clear();
                        app.body.clear();
                        app.scur = 0;
                        app.cur = 0;
                        app.body_scroll = 0;
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

#[cfg(test)]
mod tests {
    use super::{cursor_rc, visual_rows};

    #[test]
    fn newline_makes_separate_rows() {
        // The Paul bug class: "first\nsecond" must be TWO rows, never a submit
        // side-effect or one collapsed line.
        let rows = visual_rows("first\nsecond", 40);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "first");
        assert_eq!(rows[1].1, "second");
        // char indices: "second" starts after "first" + the newline char
        assert_eq!(rows[1].0, 6);
    }
    #[test]
    fn soft_wrap_splits_and_cursor_lands_on_next_row() {
        let rows = visual_rows("abcdefgh", 4);
        assert_eq!(rows.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(), ["abcd", "efgh"]);
        // cursor at char 4 sits at the START of the wrapped row, not past row 0's edge
        assert_eq!(cursor_rc(&rows, 4), (1, 0));
        assert_eq!(cursor_rc(&rows, 8), (1, 4)); // end of text
    }
    #[test]
    fn cursor_after_newline_is_next_row_col0() {
        let rows = visual_rows("ab\ncd", 40);
        assert_eq!(cursor_rc(&rows, 2), (0, 2)); // before the newline
        assert_eq!(cursor_rc(&rows, 3), (1, 0)); // after it
    }
    #[test]
    fn wide_chars_wrap_by_display_width() {
        // 3 CJK chars = 6 columns; at width 4 only two fit per row
        let rows = visual_rows("中文字", 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "中文");
        // cursor after the first wide char = display column 2
        assert_eq!(cursor_rc(&rows, 1), (0, 2));
    }
    #[test]
    fn empty_text_still_one_row() {
        let rows = visual_rows("", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(cursor_rc(&rows, 0), (0, 0));
    }
}
