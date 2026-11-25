//! Codex/Claude/omp-inspired Ratatui TUI for `twig chat`.
//!
//! Deep module with a narrow public surface (`chat(repo, resume, plain)`):
//! - Owns `App` state, key routing, and rendering privately (zero field leaks)
//! - Drives the exact same `twig serve` JSON-RPC protocol via `client::ServeClient`
//! - Automatically falls back to plain line-delimited stdio when not attached to a TTY

mod client;
mod markdown;

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{chat_plain, ServeClient, ServerMsg};
use self::markdown::{display_width, render_markdown, wrap_plain};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/steer ", "Queue steering constraint for next safe turn"),
    ("/cancel", "Cancel active run and child processes"),
    ("/approve ", "Approve a pending high-risk tool call"),
    ("/deny ", "Deny a pending high-risk tool call"),
    ("/expand", "Toggle expanded/collapsed tool outputs (Ctrl+O)"),
    ("/clear", "Clear local transcript view"),
    ("/help", "Show keyboard shortcuts and commands"),
    ("/quit", "Exit twig chat"),
];

/// Entrypoint for `twig chat`: interactive Ratatui TUI on a TTY, plain stdio otherwise.
pub fn chat(repo: &str, resume: Option<&str>, plain: bool) -> Result<(), String> {
    if plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return chat_plain(repo, resume);
    }
    run_tui(repo, resume)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| format!("terminal raw mode: {e}"))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| format!("enter alternate screen: {e}"))?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[derive(Debug, Clone)]
struct PendingApproval {
    id: String,
    name: String,
    args: Value,
    recovered: bool,
}

#[derive(Debug, Clone)]
struct ToolCell {
    id: String,
    name: String,
    summary: String,
    tier: String,
    output: String,
    started_at: Instant,
    elapsed: Option<Duration>,
    exit_code: Option<i32>,
    finished: bool,
}

#[derive(Debug, Clone)]
enum NoticeKind {
    Info,
    Warn,
    Error,
    Success,
}

#[derive(Debug, Clone)]
enum Cell {
    User(String),
    Steering(String),
    Assistant(String),
    Tool(ToolCell),
    Notice { kind: NoticeKind, text: String },
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    SendTurn(String),
    SendSteer(String),
    Cancel,
    ResolveApproval { id: String, allow: bool },
    Quit,
}

struct Composer {
    text: String,
    cursor: usize, // byte offset in `text`
    history: Vec<String>,
    history_idx: Option<usize>,
    draft: String,
}

impl Composer {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: String::new(),
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_idx = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_idx = None;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor);
        self.text.drain(self.cursor..next);
        self.history_idx = None;
    }

    fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.text[..self.cursor];
        let trimmed = prefix.trim_end();
        let cut = trimmed
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text.drain(cut..self.cursor);
        self.cursor = cut;
        self.history_idx = None;
    }

    fn clear_to_start(&mut self) {
        if self.cursor > 0 {
            self.text.drain(..self.cursor);
            self.cursor = 0;
            self.history_idx = None;
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            if let Some(ch) = self.text[self.cursor..].chars().next() {
                self.cursor += ch.len_utf8();
            }
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_idx {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next);
        self.text = self.history[next].clone();
        self.cursor = self.text.len();
    }

    fn history_down(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 < self.history.len() {
            let next = idx + 1;
            self.history_idx = Some(next);
            self.text = self.history[next].clone();
        } else {
            self.history_idx = None;
            self.text = std::mem::take(&mut self.draft);
        }
        self.cursor = self.text.len();
    }

    fn take_submitted(&mut self) -> String {
        let submitted = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.history_idx = None;
        self.draft.clear();
        if !submitted.trim().is_empty() && self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
        }
        submitted
    }

    fn set(&mut self, value: &str) {
        self.text = value.to_owned();
        self.cursor = self.text.len();
        self.history_idx = None;
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn desired_height(&self, inner_width: usize) -> u16 {
        let lines = wrap_plain(&self.text, inner_width);
        (lines.len() as u16 + 2).clamp(3, 7)
    }

    fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        border_color: Color,
        title: &str,
        placeholder: &str,
    ) {
        let inner_w = area.width.saturating_sub(2).max(16) as usize;
        let display_text: Vec<Line<'static>> = if self.text.is_empty() {
            vec![Line::from(Span::styled(
                placeholder.to_owned(),
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            wrap_plain(&self.text, inner_w)
                .into_iter()
                .map(Line::from)
                .collect()
        };
        let composer_box = Paragraph::new(display_text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(title.to_owned(), Style::default().fg(border_color))),
        );
        frame.render_widget(composer_box, area);

        let before_cursor = &self.text[..self.cursor];
        let wrapped_before = wrap_plain(before_cursor, inner_w);
        let cur_row = wrapped_before.len().saturating_sub(1) as u16;
        let cur_col = wrapped_before
            .last()
            .map(|l| display_width(l) as u16)
            .unwrap_or(0);
        let max_inner_h = area.height.saturating_sub(2);
        if cur_row < max_inner_h {
            frame.set_cursor_position((
                area.x + 1 + cur_col.min(inner_w as u16),
                area.y + 1 + cur_row,
            ));
        }
    }

    fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        if !self.text.starts_with('/') || self.text.contains('\n') || self.text.contains(' ') {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|(cmd, _)| cmd.starts_with(self.text.as_str()))
            .collect()
    }
}

struct App {
    repo_label: String,
    sid: String,
    model: String,
    playbook: Option<String>,
    guard_level: Option<String>,
    turns_used: u64,
    running: bool,
    run_started: Option<Instant>,
    active_turn_req: Option<u64>,
    cells: Vec<Cell>,
    approvals: VecDeque<PendingApproval>,
    composer: Composer,
    expand_tools: bool,
    scroll: u16,
    max_scroll: u16,
    auto_follow: bool,
    tick: usize,
    last_ctrl_c: Option<Instant>,
    status_flash: Option<(String, Instant)>,
}

impl App {
    fn new(repo: &str) -> Self {
        let label = Path::new(repo)
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| repo.to_owned());
        Self {
            repo_label: label,
            sid: String::new(),
            model: std::env::var("TWIG_MODEL").unwrap_or_else(|_| "magpie".into()),
            playbook: None,
            guard_level: None,
            turns_used: 0,
            running: false,
            run_started: None,
            active_turn_req: None,
            cells: Vec::new(),
            approvals: VecDeque::new(),
            composer: Composer::new(),
            expand_tools: false,
            scroll: 0,
            max_scroll: 0,
            auto_follow: true,
            tick: 0,
            last_ctrl_c: None,
            status_flash: None,
        }
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status_flash = Some((msg.into(), Instant::now()));
    }

    fn push_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.cells.push(Cell::Notice {
            kind,
            text: text.into(),
        });
    }

    fn apply_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Event(ev) => self.apply_event(&ev),
            ServerMsg::Reply { id, result } => {
                if let Some(sid) = result.get("sid").and_then(Value::as_str) {
                    self.sid = sid.to_owned();
                }
                if let Some(pending) = result.get("pending").and_then(Value::as_array) {
                    for p in pending {
                        let pid = p["id"].as_str().unwrap_or("").to_owned();
                        if !pid.is_empty() && !self.approvals.iter().any(|a| a.id == pid) {
                            self.approvals.push_back(PendingApproval {
                                id: pid,
                                name: p["name"].as_str().unwrap_or("bash").to_owned(),
                                args: p["args"].clone(),
                                recovered: p["recovered"] == true,
                            });
                        }
                    }
                }
                if self.active_turn_req == Some(id) {
                    self.running = false;
                    self.run_started = None;
                    self.active_turn_req = None;
                    if let Some(reason) = result.get("reason").and_then(Value::as_str) {
                        if reason != "final" {
                            self.push_notice(
                                if reason == "verify_failed" {
                                    NoticeKind::Error
                                } else {
                                    NoticeKind::Warn
                                },
                                format!("turn ended: {reason}"),
                            );
                        } else if result["verified"] == true {
                            self.push_notice(NoticeKind::Success, "verification passed");
                        }
                    }
                }
            }
            ServerMsg::Error { id, message } => {
                if self.active_turn_req == id {
                    self.running = false;
                    self.run_started = None;
                    self.active_turn_req = None;
                }
                self.push_notice(NoticeKind::Error, message);
            }
            ServerMsg::Stderr(line) => {
                if let Some(rest) = line.strip_prefix("twig: ") {
                    if let Some(sid_part) = rest.split_whitespace().find_map(|w| w.strip_prefix("sid=")) {
                        self.sid = sid_part.to_owned();
                    }
                } else {
                    self.push_notice(NoticeKind::Warn, line);
                }
            }
            ServerMsg::Exited => {
                self.running = false;
                self.run_started = None;
                self.push_notice(NoticeKind::Error, "twig serve process exited");
            }
        }
    }

    fn apply_event(&mut self, ev: &Value) {
        match ev["ev"].as_str() {
            Some("turn.started") => {
                self.running = true;
                if self.run_started.is_none() {
                    self.run_started = Some(Instant::now());
                }
                if let Some(m) = ev["model"].as_str() {
                    self.model = m.to_owned();
                }
                if let Some(t) = ev["turns_used"].as_u64() {
                    self.turns_used = t;
                }
            }
            Some("model.delta") => {
                let delta = ev["text"].as_str().unwrap_or("");
                if delta.is_empty() {
                    return;
                }
                if let Some(Cell::Assistant(buf)) = self.cells.last_mut() {
                    buf.push_str(delta);
                } else {
                    self.cells.push(Cell::Assistant(delta.to_owned()));
                }
            }
            Some("tool.started") => {
                let id = ev["id"].as_str().unwrap_or("").to_owned();
                let name = ev["tool"].as_str().unwrap_or("tool").to_owned();
                let tier = ev["tier"].as_str().unwrap_or("auto").to_owned();
                let summary = summarize_tool_args(&name, &ev["args"]);
                self.cells.push(Cell::Tool(ToolCell {
                    id,
                    name,
                    summary,
                    tier,
                    output: String::new(),
                    started_at: Instant::now(),
                    elapsed: None,
                    exit_code: None,
                    finished: false,
                }));
            }
            Some("tool.output") => {
                let id = ev["id"].as_str().unwrap_or("");
                let chunk = ev["text"].as_str().unwrap_or("");
                if let Some(Cell::Tool(tool)) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, Cell::Tool(t) if t.id == id))
                {
                    // Cap in-memory preview per tool cell at 256 KiB.
                    if tool.output.len() < 262_144 {
                        tool.output.push_str(chunk);
                    }
                }
            }
            Some("tool.completed") => {
                let id = ev["id"].as_str().unwrap_or("");
                let exit_code = ev["exit_code"].as_i64().map(|c| c as i32);
                if let Some(Cell::Tool(tool)) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, Cell::Tool(t) if t.id == id))
                {
                    tool.finished = true;
                    tool.elapsed = Some(tool.started_at.elapsed());
                    tool.exit_code = exit_code;
                }
            }
            Some("approval.requested") => {
                let id = ev["id"].as_str().unwrap_or("").to_owned();
                if !id.is_empty() && !self.approvals.iter().any(|a| a.id == id) {
                    self.approvals.push_back(PendingApproval {
                        id,
                        name: ev["name"].as_str().unwrap_or("bash").to_owned(),
                        args: ev["args"].clone(),
                        recovered: ev["recovered"] == true,
                    });
                }
            }
            Some("approval.resolved") => {
                let id = ev["id"].as_str().unwrap_or("");
                let allow = ev["allow"] == true;
                self.approvals.retain(|a| a.id != id);
                self.push_notice(
                    if allow {
                        NoticeKind::Success
                    } else {
                        NoticeKind::Warn
                    },
                    format!(
                        "approval {id}: {}",
                        if allow { "approved" } else { "denied" }
                    ),
                );
            }
            Some("playbook.selected") => {
                if let Some(name) = ev["name"].as_str() {
                    self.playbook = Some(name.to_owned());
                    self.push_notice(NoticeKind::Info, format!("playbook routed: {name}"));
                }
            }
            Some("guard.tripped") => {
                if let Some(level) = ev["level"].as_str() {
                    self.guard_level = Some(level.to_owned());
                    self.push_notice(NoticeKind::Warn, format!("guard tripped: {level}"));
                }
            }
            Some("context.compacted") => {
                let removed = ev["compaction"]["removed_messages"].as_u64().unwrap_or(0);
                self.push_notice(
                    NoticeKind::Info,
                    format!("context compacted ({removed} older messages archived)"),
                );
            }
            Some("verify.completed") => {
                let passed = ev["verdict"]["passed"] == true;
                self.push_notice(
                    if passed {
                        NoticeKind::Success
                    } else {
                        NoticeKind::Error
                    },
                    if passed {
                        "external check passed"
                    } else {
                        "external check failed — returning evidence to agent"
                    },
                );
            }
            Some("verify.passed") => {
                self.push_notice(NoticeKind::Success, "differential verification passed");
            }
            Some("verify.failed") => {
                self.push_notice(NoticeKind::Error, "differential verification failed");
            }
            Some("papercut.filed") => {
                let comp = ev["report"]["component"].as_str().unwrap_or("harness");
                self.push_notice(NoticeKind::Info, format!("papercut filed: {comp}"));
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }

        // Global shortcuts.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.running {
                        self.last_ctrl_c = Some(Instant::now());
                        self.flash("Cancelling active run… (Ctrl+C again to quit)");
                        return Action::Cancel;
                    }
                    if self
                        .last_ctrl_c
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
                        || self.composer.is_empty()
                    {
                        return Action::Quit;
                    }
                    self.composer.set("");
                    self.last_ctrl_c = Some(Instant::now());
                    self.flash("Cleared input (Ctrl+C again to quit)");
                    return Action::None;
                }
                KeyCode::Char('d') if self.composer.is_empty() => return Action::Quit,
                KeyCode::Char('o') => {
                    self.expand_tools = !self.expand_tools;
                    self.flash(if self.expand_tools {
                        "Tool outputs: expanded"
                    } else {
                        "Tool outputs: compact"
                    });
                    return Action::None;
                }
                KeyCode::Char('l') => {
                    self.cells.clear();
                    self.scroll = 0;
                    self.auto_follow = true;
                    return Action::None;
                }
                KeyCode::Char('a') => {
                    self.composer.move_home();
                    return Action::None;
                }
                KeyCode::Char('e') => {
                    self.composer.move_end();
                    return Action::None;
                }
                KeyCode::Char('u') => {
                    self.composer.clear_to_start();
                    return Action::None;
                }
                KeyCode::Char('w') => {
                    self.composer.delete_word_back();
                    return Action::None;
                }
                KeyCode::Char('j') => {
                    self.composer.insert_char('\n');
                    return Action::None;
                }
                _ => {}
            }
        }

        // Focused approval modal intercepts y / n / Enter / Esc when composer is empty.
        if let Some(pending) = self.approvals.front() {
            if self.composer.is_empty() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        let id = pending.id.clone();
                        return Action::ResolveApproval { id, allow: true };
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        let id = pending.id.clone();
                        return Action::ResolveApproval { id, allow: false };
                    }
                    _ => {}
                }
            }
        }

        // Scroll controls.
        match key.code {
            KeyCode::PageUp => {
                self.auto_follow = false;
                self.scroll = self.scroll.saturating_sub(12);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(12).min(self.max_scroll);
                if self.scroll >= self.max_scroll {
                    self.auto_follow = true;
                }
                return Action::None;
            }
            KeyCode::End
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.auto_follow = true;
                self.scroll = self.max_scroll;
                return Action::None;
            }
            KeyCode::Esc => {
                if self.running {
                    self.flash("Cancelling active run…");
                    return Action::Cancel;
                }
                if !self.composer.is_empty() {
                    self.composer.set("");
                }
                return Action::None;
            }
            _ => {}
        }

        // Composer input.
        match key.code {
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.composer.insert_char('\n');
                Action::None
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Tab => {
                let matches = self.composer.slash_matches();
                if let Some((cmd, _)) = matches.first() {
                    self.composer.set(cmd);
                }
                Action::None
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                Action::None
            }
            KeyCode::Delete => {
                self.composer.delete();
                Action::None
            }
            KeyCode::Left => {
                self.composer.move_left();
                Action::None
            }
            KeyCode::Right => {
                self.composer.move_right();
                Action::None
            }
            KeyCode::Home => {
                self.composer.move_home();
                Action::None
            }
            KeyCode::End => {
                self.composer.move_end();
                Action::None
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.auto_follow = false;
                self.scroll = self.scroll.saturating_sub(3);
                Action::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_add(3).min(self.max_scroll);
                if self.scroll >= self.max_scroll {
                    self.auto_follow = true;
                }
                Action::None
            }
            KeyCode::Up => {
                self.composer.history_up();
                Action::None
            }
            KeyCode::Down => {
                self.composer.history_down();
                Action::None
            }
            KeyCode::Char(c) => {
                self.composer.insert_char(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => {
                self.auto_follow = false;
                self.scroll = self.scroll.saturating_sub(4);
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(4).min(self.max_scroll);
                if self.scroll >= self.max_scroll {
                    self.auto_follow = true;
                }
            }
            _ => {}
        }
    }

    fn submit_composer(&mut self) -> Action {
        let raw = self.composer.take_submitted();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Action::None;
        }
        if trimmed == "/quit" || trimmed == "/exit" {
            return Action::Quit;
        }
        if trimmed == "/cancel" {
            return Action::Cancel;
        }
        if trimmed == "/clear" {
            self.cells.clear();
            self.scroll = 0;
            self.auto_follow = true;
            return Action::None;
        }
        if trimmed == "/expand" {
            self.expand_tools = !self.expand_tools;
            return Action::None;
        }
        if trimmed == "/help" {
            self.push_notice(
                NoticeKind::Info,
                "Shortcuts: Enter send (or steer while running) · Shift+Enter newline · Esc cancel · Ctrl+O toggle tool output · PgUp/PgDn or wheel scroll · y/n approve/deny · Ctrl+C quit",
            );
            return Action::None;
        }
        if let Some(rest) = trimmed.strip_prefix("/steer ") {
            let text = rest.trim().to_owned();
            if !text.is_empty() {
                self.cells.push(Cell::Steering(text.clone()));
                self.auto_follow = true;
                return Action::SendSteer(text);
            }
            return Action::None;
        }
        if let Some(rest) = trimmed.strip_prefix("/approve ") {
            let id = rest.trim().to_owned();
            if !id.is_empty() {
                return Action::ResolveApproval { id, allow: true };
            }
            return Action::None;
        }
        if let Some(rest) = trimmed.strip_prefix("/deny ") {
            let id = rest.trim().to_owned();
            if !id.is_empty() {
                return Action::ResolveApproval { id, allow: false };
            }
            return Action::None;
        }

        // Codex/Claude UX: pressing Enter while a turn is running queues a steer!
        if self.running {
            let text = raw.trim().to_owned();
            self.cells.push(Cell::Steering(text.clone()));
            self.auto_follow = true;
            return Action::SendSteer(text);
        }

        let text = raw.trim().to_owned();
        self.cells.push(Cell::User(text.clone()));
        self.running = true;
        self.run_started = Some(Instant::now());
        self.auto_follow = true;
        Action::SendTurn(text)
    }

    fn build_transcript_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let inner = width.saturating_sub(2).max(16);

        if self.cells.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "  twig ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· {} · {}", self.repo_label, self.model),
                    Style::default().fg(Color::Gray),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                "  Thin Rust agent harness — magpie provides models; twig owns the loop, verifier and record.",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Enter send / steer while running · Shift+Enter newline · Ctrl+O tool output · Esc cancel · /help",
                Style::default().fg(Color::DarkGray),
            )));
            return lines;
        }

        for cell in &self.cells {
            match cell {
                Cell::User(text) => {
                    lines.push(Line::from(""));
                    for wrapped in wrap_plain(text, inner.saturating_sub(2)) {
                        lines.push(Line::from(vec![
                            Span::styled("▌ ", Style::default().fg(Color::Cyan)),
                            Span::styled(
                                wrapped,
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Steering(text) => {
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(9)).into_iter().enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { "↳ steer  " } else { "         " },
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(Color::Yellow)),
                        ]));
                    }
                }
                Cell::Assistant(text) => {
                    lines.extend(render_markdown(text, inner));
                    lines.push(Line::from(""));
                }
                Cell::Tool(tool) => {
                    let elapsed = tool
                        .elapsed
                        .unwrap_or_else(|| tool.started_at.elapsed())
                        .as_secs_f32();
                    let (icon, icon_style) = if !tool.finished {
                        (
                            SPINNER[self.tick % SPINNER.len()],
                            Style::default().fg(Color::Cyan),
                        )
                    } else if tool.exit_code.is_none() || tool.exit_code == Some(0) {
                        ("✓", Style::default().fg(Color::Green))
                    } else {
                        ("✗", Style::default().fg(Color::Red))
                    };
                    let mut header = vec![
                        Span::styled(format!("{icon} "), icon_style),
                        Span::styled(
                            tool.name.clone(),
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {}", tool.summary),
                            Style::default().fg(Color::Gray),
                        ),
                    ];
                    if tool.tier != "auto" {
                        header.push(Span::styled(
                            format!(" [{}]", tool.tier),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    let line_count = tool.output.lines().count();
                    let mut meta = format!(" · {elapsed:.1}s");
                    if let Some(code) = tool.exit_code.filter(|&c| c != 0) {
                        meta.push_str(&format!(" · exit {code}"));
                    }
                    if line_count > 0 {
                        meta.push_str(&format!(" · {line_count}L"));
                    }
                    header.push(Span::styled(meta, Style::default().fg(Color::DarkGray)));
                    lines.push(Line::from(header));

                    let show_lines = if self.expand_tools {
                        200
                    } else if !tool.finished {
                        6
                    } else if tool.exit_code.is_some_and(|c| c != 0) {
                        4
                    } else {
                        0
                    };
                    if show_lines > 0 && !tool.output.trim().is_empty() {
                        let out_lines: Vec<&str> = tool.output.lines().collect();
                        let skip = out_lines.len().saturating_sub(show_lines);
                        if skip > 0 {
                            lines.push(Line::from(Span::styled(
                                format!("  │ … ({skip} earlier lines hidden; Ctrl+O to expand)"),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                        for raw in &out_lines[skip..] {
                            for wrapped in wrap_plain(raw, inner.saturating_sub(4)) {
                                lines.push(Line::from(vec![
                                    Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                                    Span::styled(wrapped, Style::default().fg(Color::Gray)),
                                ]));
                            }
                        }
                    }
                }
                Cell::Notice { kind, text } => {
                    let (prefix, color) = match kind {
                        NoticeKind::Info => ("· ", Color::DarkGray),
                        NoticeKind::Warn => ("! ", Color::Yellow),
                        NoticeKind::Error => ("✗ ", Color::Red),
                        NoticeKind::Success => ("✓ ", Color::Green),
                    };
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(2))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { prefix } else { "  " },
                                Style::default().fg(color),
                            ),
                            Span::styled(wrapped, Style::default().fg(color)),
                        ]));
                    }
                }
            }
        }
        lines
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let composer_inner_w = area.width.saturating_sub(2).max(16) as usize;
        let composer_height = self.composer.desired_height(composer_inner_w);
        let approval_height = if self.approvals.is_empty() { 0 } else { 6 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(approval_height),
                Constraint::Length(composer_height),
                Constraint::Length(1),
            ])
            .split(area);

        // 1. Top header bar
        let mut header_spans = vec![
            Span::styled(
                " twig ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", self.repo_label),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {} ", self.model),
                Style::default().fg(Color::Gray),
            ),
        ];
        if !self.sid.is_empty() {
            let short: String = self.sid.chars().take(12).collect();
            header_spans.push(Span::styled(
                format!("· sid:{short} "),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(book) = &self.playbook {
            header_spans.push(Span::styled(
                format!("· playbook:{book} "),
                Style::default().fg(Color::Magenta),
            ));
        }
        if let Some(level) = &self.guard_level {
            header_spans.push(Span::styled(
                format!("· guard:{level} "),
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(header_spans)).style(Style::default().bg(Color::Rgb(24, 28, 36))),
            chunks[0],
        );

        // 2. Transcript pane
        let transcript_area = chunks[1];
        let lines = self.build_transcript_lines(transcript_area.width as usize);
        let visible_h = transcript_area.height as usize;
        let total_lines = lines.len();
        self.max_scroll = total_lines.saturating_sub(visible_h) as u16;
        if self.auto_follow {
            self.scroll = self.max_scroll;
        } else {
            self.scroll = self.scroll.min(self.max_scroll);
        }
        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll, 0)),
            transcript_area,
        );
        if !self.auto_follow && self.scroll < self.max_scroll && transcript_area.width > 24 {
            let badge = " ↓ live tail (End) ";
            let badge_w = badge.len() as u16;
            let badge_area = Rect {
                x: transcript_area.x + transcript_area.width.saturating_sub(badge_w + 1),
                y: transcript_area.y + transcript_area.height.saturating_sub(1),
                width: badge_w,
                height: 1,
            };
            frame.render_widget(Clear, badge_area);
            frame.render_widget(
                Paragraph::new(badge).style(Style::default().fg(Color::Black).bg(Color::Cyan)),
                badge_area,
            );
        }

        // 3. Focused approval card (when a high-risk tool call awaits decision)
        if let Some(pending) = self.approvals.front() {
            let cmd_preview = summarize_tool_args(&pending.name, &pending.args);
            let title = if pending.recovered {
                format!(
                    " ⚠ Recovered Approval ({}) — acknowledges WITHOUT replaying ",
                    pending.id
                )
            } else {
                format!(" ⚠ Approval Required · {} ({}) ", pending.name, pending.id)
            };
            let body = vec![
                Line::from(Span::styled(
                    cmd_preview,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        " [y / Enter] ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" Approve   "),
                    Span::styled(
                        " [n / Esc] ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" Deny"),
                ]),
            ];
            let card = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(Span::styled(
                            title,
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(card, chunks[2]);
        }

        // 4. Bottom composer
        let (border_color, title) = if !self.approvals.is_empty() {
            (Color::Yellow, " Approval pending (y/n) or type command ")
        } else if self.running {
            (
                Color::Cyan,
                " Running — Enter sends /steer · Esc cancels ",
            )
        } else {
            (Color::Blue, " Task or /command ")
        };
        let placeholder = if self.running {
            "Type steering guidance and press Enter…"
        } else {
            "Describe a coding task, or type / for commands…"
        };
        self.composer
            .render(frame, chunks[3], border_color, title, placeholder);

        // Slash-command autocomplete popup docked cleanly above composer.
        let slash_matches = self.composer.slash_matches();
        if !slash_matches.is_empty() && transcript_area.height > 4 {
            let popup_h = (slash_matches.len() as u16 + 2).min(transcript_area.height);
            let popup_area = Rect {
                x: chunks[3].x,
                y: chunks[3].y.saturating_sub(popup_h),
                width: chunks[3].width.min(64),
                height: popup_h,
            };
            let items: Vec<Line<'static>> = slash_matches
                .into_iter()
                .map(|(cmd, desc)| {
                    Line::from(vec![
                        Span::styled(
                            format!("{cmd:<12}"),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(desc, Style::default().fg(Color::Gray)),
                    ])
                })
                .collect();
            frame.render_widget(Clear, popup_area);
            frame.render_widget(
                Paragraph::new(items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(" Commands (Tab to complete) "),
                ),
                popup_area,
            );
        }

        // 5. Bottom status & hint bar
        let flash = self
            .status_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(s, _)| s.as_str());
        let left_status = if let Some(msg) = flash {
            Span::styled(format!(" {msg}"), Style::default().fg(Color::Yellow))
        } else if !self.approvals.is_empty() {
            Span::styled(
                format!(" ⚠ {} approval(s) waiting", self.approvals.len()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else if self.running {
            let secs = self
                .run_started
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            Span::styled(
                format!(
                    " {} running {secs:.1}s · turn {}",
                    SPINNER[self.tick % SPINNER.len()],
                    self.turns_used.max(1)
                ),
                Style::default().fg(Color::Cyan),
            )
        } else {
            Span::styled(
                format!(" ● ready · turns:{}", self.turns_used),
                Style::default().fg(Color::Green),
            )
        };
        let right_hint = if !self.approvals.is_empty() {
            "y approve · n deny · ^O tools "
        } else if self.running {
            "Enter steer · Esc cancel · ^O tools · PgUp/Dn scroll "
        } else {
            "Enter send · Shift+Enter newline · ^O tools · ^C quit "
        };
        let status_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(right_hint.len() as u16)])
            .split(chunks[4]);
        frame.render_widget(Paragraph::new(left_status), status_cols[0]);
        frame.render_widget(
            Paragraph::new(Span::styled(right_hint, Style::default().fg(Color::DarkGray))),
            status_cols[1],
        );
    }
    fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
    ) -> Result<bool, String> {
        loop {
            while let Some(msg) = client.try_recv() {
                self.apply_server_msg(msg);
            }

            terminal
                .draw(|f| self.draw(f))
                .map_err(|e| format!("draw tui: {e}"))?;

            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) => match self.handle_key(key) {
                        Action::None => {}
                        Action::SendTurn(text) => {
                            match client.send("turn", json!({"input": text})) {
                                Ok(id) => self.active_turn_req = Some(id),
                                Err(e) => {
                                    self.running = false;
                                    self.push_notice(NoticeKind::Error, e);
                                }
                            }
                        }
                        Action::SendSteer(text) => {
                            if let Err(e) = client.send("steer", json!({"input": text})) {
                                self.push_notice(NoticeKind::Error, e);
                            }
                        }
                        Action::Cancel => {
                            let _ = client.send("cancel", json!({}));
                        }
                        Action::ResolveApproval { id, allow } => {
                            if let Err(e) = client.send(
                                "approval.resolve",
                                json!({"id": id, "allow": allow}),
                            ) {
                                self.push_notice(NoticeKind::Error, e);
                            }
                        }
                        Action::Quit => return Ok(self.running),
                    },
                    Event::Mouse(mouse) => self.handle_mouse(mouse.kind),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            self.tick = self.tick.wrapping_add(1);
        }
    }
}

fn summarize_tool_args(name: &str, args: &Value) -> String {
    let raw = match name {
        "bash" => args["command"].as_str().unwrap_or(""),
        "read" => args["path"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("(list)"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    let one_line = raw.replace('\n', " ");
    let clipped: String = one_line.chars().take(100).collect();
    if one_line.chars().count() > 100 {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn run_tui(repo: &str, resume: Option<&str>) -> Result<(), String> {
    let mut client = ServeClient::spawn(repo, resume)?;
    let _ = client.send("ping", json!({}));
    let _ = client.send("approval.list", json!({}));

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(|e| format!("init terminal: {e}"))?;
    let mut app = App::new(repo);

    let cancel_on_exit = app.run_loop(&mut terminal, &mut client)?;

    drop(terminal);
    drop(_guard);
    client.shutdown(cancel_on_exit);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn composer_handles_cjk_editing_and_running_enter_sends_steer() {
        let mut app = App::new(".");
        for ch in "修复测试".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(press(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SendTurn("修复测".into())
        );
        assert!(app.running);

        // While running, plain Enter automatically steers.
        for ch in "不要改接口".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SendSteer("不要改接口".into())
        );
    }

    #[test]
    fn approval_modal_supports_single_key_approve_and_deny() {
        let mut app = App::new(".");
        app.apply_event(&json!({
            "ev": "approval.requested",
            "id": "call_1",
            "name": "bash",
            "args": {"command": "git reset --hard"}
        }));
        assert_eq!(
            app.handle_key(press(KeyCode::Char('y'), KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true
            }
        );
        assert_eq!(
            app.handle_key(press(KeyCode::Char('n'), KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: false
            }
        );
    }

    #[test]
    fn renders_full_tui_with_streaming_tools_and_approval_overlay() {
        let mut app = App::new("/tmp/demo");
        app.apply_server_msg(ServerMsg::Reply {
            id: 1,
            result: json!({"pong": true, "sid": "abc123456789"}),
        });
        app.cells.push(Cell::User("Check status".into()));
        app.apply_event(&json!({"ev": "turn.started", "model": "demo-model", "turns_used": 1}));
        app.apply_event(&json!({"ev": "model.delta", "text": "Checking **git** status."}));
        app.apply_event(&json!({
            "ev": "tool.started",
            "id": "t1",
            "tool": "bash",
            "args": {"command": "git status"},
            "tier": "auto"
        }));
        app.apply_event(&json!({"ev": "tool.output", "id": "t1", "text": "On branch main\n"}));
        app.apply_event(&json!({"ev": "tool.completed", "id": "t1", "bytes": 15, "exit_code": 0}));
        app.apply_event(&json!({
            "ev": "approval.requested",
            "id": "t2",
            "name": "bash",
            "args": {"command": "rm -rf /"}
        }));

        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("twig"));
        assert!(rendered.contains("demo-model"));
        assert!(rendered.contains("Approval Required"));
        assert!(rendered.contains("rm -rf /"));
    }
}
