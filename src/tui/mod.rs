//! Claude Code warm-terracotta minimalist TUI for `twig chat`, combining:
//! - Claude Code visual skeleton (borderless hairline rules, `⏺`/`⎿` tool trees, warm `#D97757` palette)
//! - Codex CLI motion & input engine (TrueColor cosine shimmer, DEC 2026 sync updates, paste pills, bottom approval selector)
//! - `omp` keyboard efficiency & telemetry (`Ctrl+G` `$EDITOR`, `Alt+Y` code copy, `Ctrl+F` search, compact footer)
//!
//! Deep module with a narrow public surface (`chat(repo, resume, plain)`):
//! automatically falls back to plain stdio when not attached to a TTY.

mod client;
mod composer;
mod markdown;

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{chat_plain, ServeClient, ServerMsg};
use self::composer::Composer;
use self::markdown::{
    display_width, extract_code_blocks, highlight_search, render_markdown, shimmer_spans,
    style_output_line, wrap_plain, ACCENT, ACCENT_PEAK, AMBER, HAIRLINE, IVORY, MUTED, ROSE, SAGE,
    STONE, SURFACE, SURFACE_HI,
};

/// Claude-style breathing star glyph cycle.
const STAR_SPINNER: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];

/// Calm engineering activity verbs cycled during active turns.
const ACTIVITY_VERBS: &[&str] = &[
    "Thinking…",
    "Reasoning…",
    "Analyzing…",
    "Tracing…",
    "Synthesizing…",
    "Working…",
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
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .map_err(|e| format!("enter alternate screen: {e}"))?;
        Ok(Self)
    }

    fn suspend() {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }

    fn resume() -> Result<(), String> {
        enable_raw_mode().map_err(|e| format!("resume raw mode: {e}"))?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .map_err(|e| format!("resume alternate screen: {e}"))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::suspend();
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
    OpenEditor,
    CopyCode(String),
    Quit,
}

struct App {
    repo_path: String,
    repo_label: String,
    git_branch: Option<String>,
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
    approval_sel: usize,
    composer: Composer,
    expand_tools: bool,
    search_open: bool,
    search_query: String,
    search_match_idx: usize,
    search_matches: Vec<usize>,
    scroll: u16,
    max_scroll: u16,
    auto_follow: bool,
    tick: usize,
    last_ctrl_c: Option<Instant>,
    copy_cycle: usize,
    last_copy_at: Option<Instant>,
    status_flash: Option<(String, Instant)>,
}

impl App {
    fn new(repo: &str) -> Self {
        let canonical = Path::new(repo)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(repo).to_path_buf());
        let repo_label = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.to_owned());
        let repo_path = shorten_home(&canonical.display().to_string());
        Self {
            repo_path,
            repo_label,
            git_branch: detect_git_branch(repo),
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
            approval_sel: 0,
            composer: Composer::new(repo),
            expand_tools: false,
            search_open: false,
            search_query: String::new(),
            search_match_idx: 0,
            search_matches: Vec::new(),
            scroll: 0,
            max_scroll: 0,
            auto_follow: true,
            tick: 0,
            last_ctrl_c: None,
            copy_cycle: 0,
            last_copy_at: None,
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
                            self.approval_sel = 0;
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
                    if let Some(sid_part) =
                        rest.split_whitespace().find_map(|w| w.strip_prefix("sid="))
                    {
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
                    self.approval_sel = 0;
                }
            }
            Some("approval.resolved") => {
                let id = ev["id"].as_str().unwrap_or("");
                let allow = ev["allow"] == true;
                self.approvals.retain(|a| a.id != id);
                self.approval_sel = 0;
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
                    self.push_notice(NoticeKind::Info, format!("playbook routed · {name}"));
                }
            }
            Some("guard.tripped") => {
                if let Some(level) = ev["level"].as_str() {
                    self.guard_level = Some(level.to_owned());
                    self.push_notice(NoticeKind::Warn, format!("guard tripped · {level}"));
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
                self.push_notice(NoticeKind::Info, format!("papercut filed · {comp}"));
            }
            _ => {}
        }
    }

    fn copy_latest_code_block(&mut self) -> Action {
        let mut all_blocks = Vec::new();
        for cell in &self.cells {
            if let Cell::Assistant(text) = cell {
                all_blocks.extend(extract_code_blocks(text));
            }
        }
        if all_blocks.is_empty() {
            self.flash("No assistant code blocks to copy");
            return Action::None;
        }
        let within_cycle = self
            .last_copy_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(8));
        if within_cycle {
            self.copy_cycle = (self.copy_cycle + 1) % all_blocks.len();
        } else {
            self.copy_cycle = 0;
        }
        self.last_copy_at = Some(Instant::now());
        let idx = all_blocks.len() - 1 - self.copy_cycle;
        let block = all_blocks[idx].clone();
        let line_n = block.lines().count();
        self.flash(format!(
            "Copied code block #{}/{} ({line_n} lines)",
            self.copy_cycle + 1,
            all_blocks.len()
        ));
        Action::CopyCode(block)
    }

    fn jump_to_search_match(&mut self) {
        if let Some(&line_no) = self.search_matches.get(self.search_match_idx) {
            self.auto_follow = false;
            self.scroll = (line_no.saturating_sub(3) as u16).min(self.max_scroll);
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        if self.search_open {
            let clean = pasted.replace(['\r', '\n'], " ");
            self.search_query.push_str(&clean);
            self.search_match_idx = 0;
            return;
        }
        if let Some(pill) = self.composer.handle_paste(pasted) {
            self.flash(format!("Folded paste into {pill}"));
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }

        // Global Alt shortcuts (`omp` Alt+Y copy latest code block).
        if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('y' | 'Y'))
        {
            return self.copy_latest_code_block();
        }

        // Global Ctrl shortcuts.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.search_open {
                        self.search_open = false;
                        return Action::None;
                    }
                    if self.running {
                        self.last_ctrl_c = Some(Instant::now());
                        self.flash("Interrupting active turn… (Ctrl+C again to quit)");
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
                    self.flash("Cleared draft (Ctrl+C again to quit)");
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
                KeyCode::Char('f') => {
                    self.search_open = !self.search_open;
                    self.search_match_idx = 0;
                    if !self.search_open {
                        self.search_query.clear();
                        self.search_matches.clear();
                    }
                    return Action::None;
                }
                KeyCode::Char('g') => return Action::OpenEditor,
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

        // Inline search mode (solves fullscreen Ctrl+F search).
        if self.search_open {
            match key.code {
                KeyCode::Esc => {
                    self.search_open = false;
                    self.search_query.clear();
                    self.search_matches.clear();
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    self.search_match_idx = 0;
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    if !self.search_matches.is_empty() {
                        self.search_match_idx = (self.search_match_idx + self.search_matches.len()
                            - 1)
                            % self.search_matches.len();
                        self.jump_to_search_match();
                    }
                }
                KeyCode::Enter => {
                    if !self.search_matches.is_empty() {
                        self.search_match_idx =
                            (self.search_match_idx + 1) % self.search_matches.len();
                        self.jump_to_search_match();
                    }
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    self.search_match_idx = 0;
                }
                _ => {}
            }
            return Action::None;
        }

        // Codex/Claude bottom-pane replacement approval selector.
        if let Some(pending) = self.approvals.front() {
            let id = pending.id.clone();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.approval_sel = self.approval_sel.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.approval_sel = (self.approval_sel + 1).min(1);
                    return Action::None;
                }
                KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return Action::ResolveApproval { id, allow: true };
                }
                KeyCode::Char('2') | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    return Action::ResolveApproval { id, allow: false };
                }
                KeyCode::Enter => {
                    let allow = self.approval_sel == 0;
                    return Action::ResolveApproval { id, allow };
                }
                _ => {}
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
                    self.flash("Interrupting active turn…");
                    return Action::Cancel;
                }
                if !self.composer.is_empty() {
                    self.composer.set("");
                }
                return Action::None;
            }
            _ => {}
        }

        // Popup navigation takes precedence for Up/Down/Tab when active.
        if self.composer.has_popup() {
            match key.code {
                KeyCode::Up => {
                    self.composer.popup_move(-1);
                    return Action::None;
                }
                KeyCode::Down => {
                    self.composer.popup_move(1);
                    return Action::None;
                }
                KeyCode::Tab => {
                    self.composer.accept_popup();
                    return Action::None;
                }
                _ => {}
            }
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
                self.composer.accept_popup();
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
        if trimmed == "/copy" {
            return self.copy_latest_code_block();
        }
        if trimmed == "/editor" {
            return Action::OpenEditor;
        }
        if trimmed == "/find" {
            self.search_open = true;
            return Action::None;
        }
        if let Some(q) = trimmed.strip_prefix("/find ") {
            self.search_open = true;
            self.search_query = q.trim().to_owned();
            self.search_match_idx = 0;
            return Action::None;
        }
        if trimmed == "/help" {
            self.push_notice(
                NoticeKind::Info,
                "Enter send (or steer while running) · Shift+Enter newline · @ file · Ctrl+O tool tree · Ctrl+F find · Ctrl+G $EDITOR · Alt+Y copy code · Esc cancel · Ctrl+C quit",
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

        // Codex/Claude UX: pressing Enter while a turn is active queues a steer.
        if self.running {
            let text = trimmed.to_owned();
            self.cells.push(Cell::Steering(text.clone()));
            self.auto_follow = true;
            return Action::SendSteer(text);
        }

        let text = trimmed.to_owned();
        self.cells.push(Cell::User(text.clone()));
        self.running = true;
        self.run_started = Some(Instant::now());
        self.auto_follow = true;
        Action::SendTurn(text)
    }

    fn active_activity_label(&self) -> String {
        if let Some(Cell::Tool(tool)) = self
            .cells
            .iter()
            .rev()
            .find(|c| matches!(c, Cell::Tool(t) if !t.finished))
        {
            return format!("Running {}…", title_tool_name(&tool.name));
        }
        // Rotate gently every ~3 seconds (60 ticks at 50ms).
        let idx = (self.tick / 60) % ACTIVITY_VERBS.len();
        ACTIVITY_VERBS[idx].to_owned()
    }

    fn build_transcript_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let inner = width.saturating_sub(3).max(16);

        if self.cells.is_empty() {
            let branch = self
                .git_branch
                .as_deref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "  ✳ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "twig",
                    Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(MUTED),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(self.model.clone(), Style::default().fg(ACCENT_PEAK)),
                Span::styled(" · ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{}{branch}", self.repo_label),
                    Style::default().fg(STONE),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!("    {}", self.repo_path),
                Style::default().fg(MUTED),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Tips: Enter send/steer · @ file · Ctrl+O tool tree · Ctrl+F find · Ctrl+G editor · Alt+Y copy code",
                Style::default().fg(MUTED),
            )));
            return lines;
        }

        for cell in &self.cells {
            match cell {
                Cell::User(text) => {
                    lines.push(Line::from(""));
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(3))
                        .into_iter()
                        .enumerate()
                    {
                        let prefix = if idx == 0 { " ❯ " } else { "   " };
                        let pad = width
                            .saturating_sub(display_width(prefix) + display_width(&wrapped) + 1);
                        lines.push(Line::from(vec![
                            Span::styled(
                                prefix,
                                Style::default()
                                    .fg(ACCENT)
                                    .bg(SURFACE)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(IVORY).bg(SURFACE)),
                            Span::styled(" ".repeat(pad), Style::default().bg(SURFACE)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Steering(text) => {
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(10))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { "  ↳ steer " } else { "          " },
                                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(AMBER)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Assistant(text) => {
                    let md_lines = render_markdown(text, inner.saturating_sub(2));
                    for (idx, line) in md_lines.into_iter().enumerate() {
                        let mut spans = vec![if idx == 0 {
                            Span::styled(" ⏺ ", Style::default().fg(IVORY))
                        } else {
                            Span::raw("   ")
                        }];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Tool(tool) => {
                    let elapsed = tool
                        .elapsed
                        .unwrap_or_else(|| tool.started_at.elapsed())
                        .as_secs_f32();
                    let (icon, icon_color) = if !tool.finished {
                        (
                            STAR_SPINNER[(self.tick / 2) % STAR_SPINNER.len()],
                            ACCENT,
                        )
                    } else if tool.exit_code.is_none() || tool.exit_code == Some(0) {
                        ("⏺", SAGE)
                    } else {
                        ("⏺", ROSE)
                    };

                    let mut header = vec![
                        Span::styled(format!(" {icon} "), Style::default().fg(icon_color)),
                        Span::styled(
                            title_tool_name(&tool.name),
                            Style::default().fg(IVORY).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if !tool.summary.is_empty() {
                        header.push(Span::styled("(", Style::default().fg(MUTED)));
                        header.push(Span::styled(
                            tool.summary.clone(),
                            Style::default().fg(STONE),
                        ));
                        header.push(Span::styled(")", Style::default().fg(MUTED)));
                    }
                    if tool.tier != "auto" {
                        header.push(Span::styled(
                            format!(" [{}]", tool.tier),
                            Style::default().fg(AMBER),
                        ));
                    }
                    let mut meta = format!("  {elapsed:.1}s");
                    if let Some(code) = tool.exit_code.filter(|&c| c != 0) {
                        meta.push_str(&format!(" · exit {code}"));
                    }
                    header.push(Span::styled(meta, Style::default().fg(MUTED)));
                    lines.push(Line::from(header));

                    // Claude-style `⎿` hanging child output tree.
                    let out_lines: Vec<&str> = tool.output.lines().collect();
                    if !out_lines.is_empty() {
                        let show_lines = if self.expand_tools {
                            200
                        } else if !tool.finished {
                            5
                        } else if tool.exit_code.is_some_and(|c| c != 0) {
                            6
                        } else {
                            1
                        };
                        let total = out_lines.len();
                        let skip = total.saturating_sub(show_lines);
                        let is_compact_summary =
                            !self.expand_tools && tool.finished && tool.exit_code.unwrap_or(0) == 0;

                        if is_compact_summary {
                            let first_preview = out_lines
                                .iter()
                                .find(|l| !l.trim().is_empty())
                                .copied()
                                .unwrap_or("");
                            let clipped = clip_str(first_preview.trim(), inner.saturating_sub(32));
                            let mut spans = vec![
                                Span::styled("   ⎿  ", Style::default().fg(MUTED)),
                                Span::styled(clipped, style_output_line(first_preview)),
                            ];
                            if total > 1 {
                                spans.push(Span::styled(
                                    format!("  … +{total} lines (ctrl+o to expand)"),
                                    Style::default().fg(MUTED),
                                ));
                            }
                            lines.push(Line::from(spans));
                        } else {
                            let mut first_row = true;
                            if skip > 0 {
                                lines.push(Line::from(vec![
                                    Span::styled("   ⎿  ", Style::default().fg(MUTED)),
                                    Span::styled(
                                        format!("… ({skip} earlier lines hidden · ctrl+o to expand)"),
                                        Style::default().fg(MUTED),
                                    ),
                                ]));
                                first_row = false;
                            }
                            for raw in &out_lines[skip..] {
                                let style = style_output_line(raw);
                                for wrapped in wrap_plain(raw, inner.saturating_sub(7)) {
                                    let branch = if first_row { "   ⎿  " } else { "      " };
                                    first_row = false;
                                    lines.push(Line::from(vec![
                                        Span::styled(branch, Style::default().fg(MUTED)),
                                        Span::styled(wrapped, style),
                                    ]));
                                }
                            }
                        }
                    }
                }
                Cell::Notice { kind, text } => {
                    let (prefix, color) = match kind {
                        NoticeKind::Info => (" · ", MUTED),
                        NoticeKind::Warn => (" ! ", AMBER),
                        NoticeKind::Error => (" ✗ ", ROSE),
                        NoticeKind::Success => (" ✓ ", SAGE),
                    };
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(3))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { prefix } else { "   " },
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
        let has_approval = !self.approvals.is_empty();
        let activity_h: u16 = if self.running && !has_approval { 1 } else { 0 };
        let search_h: u16 = if self.search_open { 1 } else { 0 };
        let bottom_panel_h: u16 = if has_approval {
            7
        } else {
            self.composer.desired_height(area.width as usize)
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(activity_h),
                Constraint::Length(search_h),
                Constraint::Length(bottom_panel_h),
                Constraint::Length(1),
            ])
            .split(area);

        // 1. Borderless transcript viewport
        let transcript_area = chunks[0];
        let mut lines = self.build_transcript_lines(transcript_area.width as usize);

        // Apply live Ctrl+F search matches & highlights.
        if self.search_open && !self.search_query.is_empty() {
            let needle = self.search_query.to_lowercase();
            self.search_matches = lines
                .iter()
                .enumerate()
                .filter_map(|(idx, line)| {
                    line.to_string()
                        .to_lowercase()
                        .contains(&needle)
                        .then_some(idx)
                })
                .collect();
            if !self.search_matches.is_empty() {
                self.search_match_idx = self.search_match_idx.min(self.search_matches.len() - 1);
                let active_line = self.search_matches[self.search_match_idx];
                lines = lines
                    .into_iter()
                    .enumerate()
                    .map(|(i, l)| highlight_search(l, &self.search_query, i == active_line))
                    .collect();
            }
        } else {
            self.search_matches.clear();
        }

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

        // Subtle terracotta live-tail indicator when scrolled up
        if !self.auto_follow && self.scroll < self.max_scroll && transcript_area.width > 24 {
            let badge = " ↓ live tail (End) ";
            let badge_w = display_width(badge) as u16;
            let badge_area = Rect {
                x: transcript_area.x + transcript_area.width.saturating_sub(badge_w + 1),
                y: transcript_area.y + transcript_area.height.saturating_sub(1),
                width: badge_w,
                height: 1,
            };
            frame.render_widget(Clear, badge_area);
            frame.render_widget(
                Paragraph::new(badge).style(
                    Style::default()
                        .fg(Color::Rgb(24, 20, 18))
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                badge_area,
            );
        }

        // 2. Claude star + Codex TrueColor Shimmer activity row
        if activity_h > 0 {
            let star = STAR_SPINNER[(self.tick / 2) % STAR_SPINNER.len()];
            let verb = self.active_activity_label();
            let phase = (self.tick as f32) * 0.045;
            let secs = self
                .run_started
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            let mut spans = vec![Span::styled(
                format!(" {star} "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )];
            spans.extend(shimmer_spans(&verb, phase));
            spans.push(Span::styled(
                format!("  ({secs:.1}s · esc to interrupt · enter to steer)"),
                Style::default().fg(MUTED),
            ));
            frame.render_widget(Paragraph::new(Line::from(spans)), chunks[1]);
        }

        // 3. Inline transcript search bar (`Ctrl+F`)
        if self.search_open {
            let count_str = if self.search_query.is_empty() {
                "type to search".to_owned()
            } else if self.search_matches.is_empty() {
                "0 matches".to_owned()
            } else {
                format!("{}/{}", self.search_match_idx + 1, self.search_matches.len())
            };
            let bar = Line::from(vec![
                Span::styled(
                    " Find: ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.search_query.clone(), Style::default().fg(IVORY)),
                Span::styled(
                    format!("  [{count_str} · Enter next · Shift+Enter prev · Esc close]"),
                    Style::default().fg(MUTED),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(bar).style(Style::default().bg(SURFACE)),
                chunks[2],
            );
            let cur_x = chunks[2].x + 7 + display_width(&self.search_query) as u16;
            if cur_x < chunks[2].x + chunks[2].width {
                frame.set_cursor_position((cur_x, chunks[2].y));
            }
        }

        // 4. Bottom pane: either Codex/Claude approval selector OR hairline borderless composer
        if let Some(pending) = self.approvals.front() {
            self.render_approval_panel(frame, chunks[3], pending);
        } else {
            let (rule_color, prompt_color, placeholder) = if self.running {
                (
                    ACCENT,
                    AMBER,
                    "Type steering guidance and press Enter (or Esc to cancel)…",
                )
            } else {
                (
                    HAIRLINE,
                    ACCENT,
                    "Ask twig to edit, debug, or inspect… (@ file · / command)",
                )
            };
            self.composer.render(
                frame,
                chunks[3],
                rule_color,
                prompt_color,
                placeholder,
                !self.search_open,
            );
            if !self.search_open {
                self.composer
                    .render_popup(frame, chunks[3], transcript_area.height);
            }
        }

        // 5. Compact `omp`-inspired warm telemetry footer
        self.render_footer(frame, chunks[4]);
    }

    fn render_approval_panel(&self, frame: &mut Frame, area: Rect, pending: &PendingApproval) {
        if area.height < 6 || area.width < 12 {
            return;
        }
        let rule = "─".repeat(area.width as usize);
        let cmd_preview = summarize_tool_args(&pending.name, &pending.args);
        let title = if pending.recovered {
            format!(
                " ⚠ Recovered approval ({}) — acknowledges WITHOUT replaying",
                pending.id
            )
        } else {
            format!(
                " ⚠ Approve {}? ({})",
                title_tool_name(&pending.name),
                pending.id
            )
        };

        let opt = |idx: usize, label: &str| -> Line<'static> {
            let active = self.approval_sel == idx;
            let bg = if active { SURFACE_HI } else { Color::Reset };
            let marker = if active { " ❯ " } else { "   " };
            let fg = if active { IVORY } else { STONE };
            let pad = (area.width as usize).saturating_sub(display_width(marker) + display_width(label));
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default()
                        .fg(ACCENT)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label.to_owned(),
                    Style::default().fg(fg).bg(bg).add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            ])
        };

        let preview_line = format!("   $ {}", clip_str(&cmd_preview, area.width.saturating_sub(6) as usize));
        let preview_pad = (area.width as usize).saturating_sub(display_width(&preview_line));

        let rows = vec![
            Line::from(Span::styled(rule.clone(), Style::default().fg(AMBER))),
            Line::from(Span::styled(
                title,
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(preview_line, Style::default().fg(ACCENT_PEAK).bg(SURFACE)),
                Span::styled(" ".repeat(preview_pad), Style::default().bg(SURFACE)),
            ]),
            Line::from(""),
            opt(0, "1. Yes, allow once (y / Enter)"),
            opt(1, "2. No, deny and tell agent (n / Esc)"),
            Line::from(Span::styled(rule, Style::default().fg(AMBER))),
        ];
        frame.render_widget(Paragraph::new(rows), area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let flash = self
            .status_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(s, _)| s.as_str());

        let left: Line<'static> = if let Some(msg) = flash {
            Line::from(Span::styled(format!(" {msg}"), Style::default().fg(AMBER)))
        } else {
            let mut spans = vec![
                Span::styled(
                    " ✳ ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.repo_label.clone(), Style::default().fg(STONE)),
            ];
            if let Some(branch) = &self.git_branch {
                spans.push(Span::styled(
                    format!(" ({branch})"),
                    Style::default().fg(MUTED),
                ));
            }
            spans.push(Span::styled(
                format!(" · {}", self.model),
                Style::default().fg(ACCENT_PEAK),
            ));
            if let Some(book) = &self.playbook {
                spans.push(Span::styled(
                    format!(" · {book}"),
                    Style::default().fg(AMBER),
                ));
            }
            if let Some(level) = &self.guard_level {
                spans.push(Span::styled(
                    format!(" · guard:{level}"),
                    Style::default().fg(ROSE),
                ));
            }
            if !self.sid.is_empty() {
                let short: String = self.sid.chars().take(10).collect();
                spans.push(Span::styled(
                    format!(" · {short}"),
                    Style::default().fg(MUTED),
                ));
            }
            if self.turns_used > 0 {
                spans.push(Span::styled(
                    format!(" · t{}", self.turns_used),
                    Style::default().fg(MUTED),
                ));
            }
            Line::from(spans)
        };

        let right_hint = if !self.approvals.is_empty() {
            "↑/↓ select · Enter confirm · Esc deny "
        } else if self.search_open {
            "Enter next · Esc close "
        } else if self.expand_tools {
            "tools:expanded · ^O compact · ^F find · ^G edit "
        } else {
            "^O expand · ^F find · ^G edit · Alt+Y copy "
        };
        let right_w = (display_width(right_hint) as u16).min(area.width / 2);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(right_w)])
            .split(area);
        frame.render_widget(Paragraph::new(left), cols[0]);
        frame.render_widget(
            Paragraph::new(Span::styled(right_hint, Style::default().fg(MUTED))),
            cols[1],
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

            // DEC Mode 2026 synchronized update eliminates frame tearing in Kitty/Ghostty/tmux.
            let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
            let draw_res = terminal.draw(|f| self.draw(f)).map(|_| ());
            let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
            draw_res.map_err(|e| format!("draw tui: {e}"))?;

            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Paste(data) => self.handle_paste(&data),
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
                        Action::OpenEditor => {
                            let draft = self.composer.draft_expanded();
                            TerminalGuard::suspend();
                            let edited = edit_in_external_editor(&draft);
                            TerminalGuard::resume()?;
                            terminal.clear().map_err(|e| e.to_string())?;
                            match edited {
                                Ok(text) => {
                                    self.composer.set(text.trim_end_matches('\n'));
                                    self.flash("Loaded draft from external editor");
                                }
                                Err(e) => self.flash(format!("Editor failed: {e}")),
                            }
                        }
                        Action::CopyCode(block) => {
                            copy_to_clipboard(&block);
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

fn title_tool_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "Tool".into(),
    }
}

fn clip_str(s: &str, max_chars: usize) -> String {
    let limit = max_chars.max(8);
    let clipped: String = s.chars().take(limit).collect();
    if s.chars().count() > limit {
        format!("{clipped}…")
    } else {
        clipped
    }
}

fn summarize_tool_args(name: &str, args: &Value) -> String {
    let raw = match name {
        "bash" => args["command"].as_str().unwrap_or(""),
        "read" => args["path"].as_str().unwrap_or(""),
        "skill" => args["name"].as_str().unwrap_or("list"),
        "subagent" => args["task"].as_str().unwrap_or(""),
        "papercut" => args["component"].as_str().unwrap_or(""),
        _ => "",
    };
    clip_str(&raw.replace('\n', " "), 92)
}

fn detect_git_branch(repo: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", repo, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_owned()
}

fn edit_in_external_editor(initial: &str) -> Result<String, String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let path = std::env::temp_dir().join(format!("twig-prompt-{}.md", std::process::id()));
    std::fs::write(&path, initial).map_err(|e| e.to_string())?;
    let parts = shlex::split(&editor).unwrap_or_else(|| vec![editor]);
    let (bin, args) = parts
        .split_first()
        .ok_or_else(|| "empty $EDITOR".to_owned())?;
    let status = Command::new(bin)
        .args(args)
        .arg(&path)
        .status()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    if !status.success() {
        return Err(format!("{bin} exited with {status}"));
    }
    Ok(content)
}

/// Copy text to system clipboard via OSC 52 (works in Kitty/Ghostty/WezTerm/tmux)
/// and best-effort `wl-copy` on Wayland.
fn copy_to_clipboard(text: &str) {
    let b64 = encode_base64(text.as_bytes());
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{b64}\x07");
    let _ = stdout.flush();
    if let Ok(mut child) = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
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
    fn composer_handles_cjk_editing_paste_pills_and_running_steer() {
        let mut app = App::new(".");
        for ch in "修复测试".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.handle_key(press(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_paste("line1\nline2\nline3\nline4\nline5");
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SendTurn("修复测line1\nline2\nline3\nline4\nline5".into())
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
    fn approval_selector_supports_arrows_numbers_and_hotkeys() {
        let mut app = App::new(".");
        app.apply_event(&json!({
            "ev": "approval.requested",
            "id": "call_1",
            "name": "bash",
            "args": {"command": "git reset --hard"}
        }));
        // Default option 0 = allow
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true
            }
        );
        // Move down to option 1 = deny
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: false
            }
        );
        // Quick number key 1
        assert_eq!(
            app.handle_key(press(KeyCode::Char('1'), KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true
            }
        );
    }

    #[test]
    fn alt_y_cycles_assistant_code_blocks_and_ctrl_f_searches() {
        let mut app = App::new(".");
        app.cells.push(Cell::Assistant(
            "First:\n```rust\nlet a = 1;\n```\nSecond:\n```rust\nlet b = 2;\n```".into(),
        ));
        assert_eq!(
            app.handle_key(press(KeyCode::Char('y'), KeyModifiers::ALT)),
            Action::CopyCode("let b = 2;".into())
        );
        assert_eq!(
            app.handle_key(press(KeyCode::Char('y'), KeyModifiers::ALT)),
            Action::CopyCode("let a = 1;".into())
        );

        // Open Ctrl+F search and type query
        app.handle_key(press(KeyCode::Char('f'), KeyModifiers::CONTROL));
        for ch in "let b".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.search_matches.len(), 1);
    }

    #[test]
    fn renders_claude_warm_minimal_tui_with_tools_shimmer_and_approval() {
        let mut app = App::new("/tmp/demo");
        app.apply_server_msg(ServerMsg::Reply {
            id: 1,
            result: json!({"pong": true, "sid": "abc123456789"}),
        });
        app.cells.push(Cell::User("Check diff".into()));
        app.apply_event(&json!({"ev": "turn.started", "model": "demo-model", "turns_used": 1}));
        app.apply_event(&json!({"ev": "model.delta", "text": "Checking **git** diff."}));
        app.apply_event(&json!({
            "ev": "tool.started",
            "id": "t1",
            "tool": "bash",
            "args": {"command": "git diff"},
            "tier": "auto"
        }));
        app.apply_event(&json!({"ev": "tool.output", "id": "t1", "text": "-old\n+new\n"}));
        app.apply_event(&json!({"ev": "tool.completed", "id": "t1", "bytes": 10, "exit_code": 0}));

        // First render while running (shows shimmer activity row + ⏺/⎿ tool tree)
        let backend = TestBackend::new(92, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let active_frame: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(active_frame.contains("⏺"));
        assert!(active_frame.contains("⎿"));
        assert!(active_frame.contains("Bash"));
        assert!(active_frame.contains("esc to interrupt"));

        // Now trigger approval overlay and verify bottom selector
        app.apply_event(&json!({
            "ev": "approval.requested",
            "id": "t2",
            "name": "bash",
            "args": {"command": "rm -rf /"}
        }));
        terminal.draw(|f| app.draw(f)).unwrap();
        let approval_frame: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(approval_frame.contains("Approve Bash?"));
        assert!(approval_frame.contains("1. Yes, allow once"));
        assert!(approval_frame.contains("rm -rf /"));
    }

    #[test]
    fn base64_encoder_matches_rfc4648() {
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(b"twig"), "dHdpZw==");
    }
}
