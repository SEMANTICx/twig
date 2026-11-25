//! Claude Code warm-terracotta minimalist TUI for `twig chat`:
//! - Claude Code visual craft: rounded terracotta welcome card, `⏺`/`⎿` aligned tool trees,
//!   organic breathing floral star (`· ✢ ✳ ✶ ✻ ✽`), 2-col breathing margins, hairline composer
//! - Codex (`codex-rs/tui`) engine: cadenced column-width shimmer, unified diff line-number gutter,
//!   paste pills, `FooterMode` + `?` shortcut overlay, 200ms onset delay, `TWIG_REDUCED_MOTION`
//! - Charmbracelet/omp smoothness: 60fps exponential-eased scrolling, DEC 2026 sync updates,
//!   event-batching frame loop, `Ctrl+F` search, `Ctrl+G` `$EDITOR`, `Alt+Y` OSC 52 code copy

mod client;
mod composer;
mod markdown;

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{
    chat_plain, copy_to_clipboard, detect_git_branch, edit_in_external_editor, shorten_home,
    ServeClient, ServerMsg, TerminalGuard,
};
use self::composer::Composer;
use self::markdown::{
    active_rule_color, breathing_star, display_width, extract_code_blocks, fmt_elapsed_compact,
    highlight_search, render_markdown, render_welcome_card, shimmer_is_sweeping,
    shortcut_overlay_lines, streaming_caret, style_output_line, summary_shimmer, tool_glyph,
    wrap_plain, DiffGutterState, MotionMode, SmoothScroll, ACCENT, ACCENT_PEAK, AMBER, HAIRLINE,
    HAIRLINE_FOCUS, IVORY, MUTED, ROSE, SAGE, STONE, SURFACE, SURFACE_HI,
};

/// Calm engineering activity verbs rotated gently during active turns.
const ACTIVITY_VERBS: &[&str] = &[
    "Thinking",
    "Reasoning",
    "Analyzing",
    "Tracing",
    "Synthesizing",
    "Working",
];

/// Entrypoint for `twig chat`: interactive Ratatui TUI on a TTY, plain stdio otherwise.
pub fn chat(repo: &str, resume: Option<&str>, plain: bool) -> Result<(), String> {
    if plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return chat_plain(repo, resume);
    }
    run_tui(repo, resume)
}

/// Ported from `codex-rs/tui/src/bottom_pane/footer.rs`: explicit footer state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FooterMode {
    Contextual,
    ShortcutOverlay,
    QuitReminder,
    EscHint,
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
    Playbook,
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
    last_delta_at: Option<Instant>,
    status_header: String,
    status_header_started: Instant,
    active_turn_req: Option<u64>,
    cells: Vec<Cell>,
    approvals: VecDeque<PendingApproval>,
    approval_sel: usize,
    composer: Composer,
    expand_tools: bool,
    motion: MotionMode,
    footer_mode: FooterMode,
    footer_mode_at: Instant,
    search_open: bool,
    search_query: String,
    search_match_idx: usize,
    search_matches: Vec<usize>,
    scroll: SmoothScroll,
    last_frame_at: Instant,
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
        let now = Instant::now();
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
            last_delta_at: None,
            status_header: String::from("Working"),
            status_header_started: now,
            active_turn_req: None,
            cells: Vec::new(),
            approvals: VecDeque::new(),
            approval_sel: 0,
            composer: Composer::new(repo),
            expand_tools: false,
            motion: MotionMode::from_env(),
            footer_mode: FooterMode::Contextual,
            footer_mode_at: now,
            search_open: false,
            search_query: String::new(),
            search_match_idx: 0,
            search_matches: Vec::new(),
            scroll: SmoothScroll::new(),
            last_frame_at: now,
            last_ctrl_c: None,
            copy_cycle: 0,
            last_copy_at: None,
            status_flash: None,
        }
    }

    fn update_status_header(&mut self, next: String) {
        if self.status_header != next {
            self.status_header = next;
            self.status_header_started = Instant::now();
        }
    }

    fn set_footer_mode(&mut self, mode: FooterMode) {
        self.footer_mode = mode;
        self.footer_mode_at = Instant::now();
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

    /// Adaptive 60fps / low-power pacing:
    /// - 16ms (~60fps) during smooth-scroll easing, active 1s shimmer sweep, or live token deltas
    /// - 45ms (~22fps) during active turns in shimmer rest (keeps floral star & rule breathing silky)
    /// - 200ms when completely idle (near-0% CPU)
    fn next_poll_interval(&self) -> Duration {
        if self.scroll.is_animating(self.motion) {
            return Duration::from_millis(16);
        }
        if self.running && self.approvals.is_empty() && self.motion == MotionMode::Animated {
            let streaming_now = self
                .last_delta_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(600));
            if streaming_now || shimmer_is_sweeping(self.status_header_started.elapsed(), self.motion)
            {
                return Duration::from_millis(16);
            }
            return Duration::from_millis(45);
        }
        Duration::from_millis(200)
    }

    fn sync_status_header(&mut self) {
        if !self.running {
            return;
        }
        if let Some(Cell::Tool(tool)) = self
            .cells
            .iter()
            .rev()
            .find(|c| matches!(c, Cell::Tool(t) if !t.finished))
        {
            self.update_status_header(format!("Running {}", title_tool_name(&tool.name)));
            return;
        }
        let secs = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let idx = ((secs / 8) as usize) % ACTIVITY_VERBS.len();
        self.update_status_header(ACTIVITY_VERBS[idx].to_owned());
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
                    self.last_delta_at = None;
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
                    self.last_delta_at = None;
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
                self.last_delta_at = None;
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
                    self.update_status_header(String::from("Thinking"));
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
                self.last_delta_at = Some(Instant::now());
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
                self.update_status_header(format!("Running {}", title_tool_name(&name)));
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
                    self.push_notice(NoticeKind::Playbook, format!("playbook routed · {name}"));
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
            self.scroll.jump_to(line_no.saturating_sub(3));
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

        if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('y' | 'Y'))
        {
            return self.copy_latest_code_block();
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.search_open {
                        self.search_open = false;
                        return Action::None;
                    }
                    if self.running {
                        self.last_ctrl_c = Some(Instant::now());
                        self.set_footer_mode(FooterMode::QuitReminder);
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
                    self.set_footer_mode(FooterMode::QuitReminder);
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
                    self.scroll.reset();
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

        if matches!(key.code, KeyCode::Char('?'))
            && key.modifiers.is_empty()
            && self.composer.is_empty()
        {
            let next = if self.footer_mode == FooterMode::ShortcutOverlay {
                FooterMode::Contextual
            } else {
                FooterMode::ShortcutOverlay
            };
            self.set_footer_mode(next);
            return Action::None;
        }

        if matches!(
            self.footer_mode,
            FooterMode::ShortcutOverlay | FooterMode::QuitReminder
        ) && !matches!(key.code, KeyCode::Esc)
        {
            self.set_footer_mode(FooterMode::Contextual);
        }

        match key.code {
            KeyCode::PageUp => {
                self.scroll.scroll_by(-12);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll.scroll_by(12);
                return Action::None;
            }
            KeyCode::End
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.scroll.follow_tail();
                return Action::None;
            }
            KeyCode::Esc => {
                if self.footer_mode == FooterMode::ShortcutOverlay {
                    self.set_footer_mode(FooterMode::Contextual);
                    return Action::None;
                }
                if self.running {
                    self.flash("Interrupting active turn…");
                    return Action::Cancel;
                }
                if !self.composer.is_empty() {
                    if self.footer_mode == FooterMode::EscHint
                        && self.footer_mode_at.elapsed() < Duration::from_secs(2)
                    {
                        self.composer.set("");
                        self.set_footer_mode(FooterMode::Contextual);
                    } else {
                        self.set_footer_mode(FooterMode::EscHint);
                    }
                }
                return Action::None;
            }
            _ => {}
        }

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
                self.scroll.scroll_by(-3);
                Action::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll.scroll_by(3);
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
            MouseEventKind::ScrollUp => self.scroll.scroll_by(-4),
            MouseEventKind::ScrollDown => self.scroll.scroll_by(4),
            _ => {}
        }
    }

    fn submit_composer(&mut self) -> Action {
        let raw = self.composer.take_submitted();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Action::None;
        }
        self.set_footer_mode(FooterMode::Contextual);
        if trimmed == "/quit" || trimmed == "/exit" {
            return Action::Quit;
        }
        if trimmed == "/cancel" {
            return Action::Cancel;
        }
        if trimmed == "/clear" {
            self.cells.clear();
            self.scroll.reset();
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
            self.set_footer_mode(FooterMode::ShortcutOverlay);
            return Action::None;
        }
        if let Some(rest) = trimmed.strip_prefix("/steer ") {
            let text = rest.trim().to_owned();
            if !text.is_empty() {
                self.cells.push(Cell::Steering(text.clone()));
                self.scroll.follow_tail();
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

        if self.running {
            let text = trimmed.to_owned();
            self.cells.push(Cell::Steering(text.clone()));
            self.scroll.follow_tail();
            return Action::SendSteer(text);
        }

        let text = trimmed.to_owned();
        self.cells.push(Cell::User(text.clone()));
        self.running = true;
        self.run_started = Some(Instant::now());
        self.update_status_header(String::from("Thinking"));
        self.scroll.follow_tail();
        Action::SendTurn(text)
    }

    fn build_transcript_lines(&self, width: usize) -> Vec<Line<'static>> {
        if self.cells.is_empty() {
            return render_welcome_card(
                &self.model,
                &self.repo_label,
                self.git_branch.as_deref(),
                &self.repo_path,
                width,
            );
        }
        let mut lines = Vec::new();
        // Consistent 2-col left + 2-col right page gutter for comfortable reading rhythm.
        let inner = width.saturating_sub(4).max(16);
        let last_idx = self.cells.len().saturating_sub(1);
        let live_streaming = self.running
            && self
                .last_delta_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(600));

        for (cell_idx, cell) in self.cells.iter().enumerate() {
            let prev_is_tool = cell_idx > 0 && matches!(self.cells[cell_idx - 1], Cell::Tool(_));
            match cell {
                Cell::User(text) => {
                    lines.push(Line::from(""));
                    let content_w = inner.saturating_sub(4);
                    for (idx, wrapped) in wrap_plain(text, content_w).into_iter().enumerate() {
                        let prefix = if idx == 0 { " ❯ " } else { "   " };
                        let pad = inner
                            .saturating_sub(display_width(prefix) + display_width(&wrapped));
                        lines.push(Line::from(vec![
                            Span::raw("  "),
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
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(11))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 {
                                    "  ↳ steer  "
                                } else {
                                    "           "
                                },
                                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(wrapped, Style::default().fg(AMBER)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Assistant(text) => {
                    if prev_is_tool {
                        lines.push(Line::from(""));
                    }
                    let mut md_lines = render_markdown(text, inner.saturating_sub(3));
                    if cell_idx == last_idx && live_streaming {
                        let caret = streaming_caret(
                            self.run_started.map(|t| t.elapsed()).unwrap_or_default(),
                            self.motion,
                        );
                        if let Some(last) = md_lines.last_mut() {
                            last.spans.push(caret);
                        }
                    }
                    for (idx, line) in md_lines.into_iter().enumerate() {
                        let mut spans = vec![if idx == 0 {
                            Span::styled("  ⏺ ", Style::default().fg(IVORY))
                        } else {
                            Span::raw("    ")
                        }];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                    lines.push(Line::from(""));
                }
                Cell::Tool(tool) => {
                    let elapsed_dur = tool.elapsed.unwrap_or_else(|| tool.started_at.elapsed());
                    let icon_span = if !tool.finished {
                        breathing_star(elapsed_dur, self.motion, true)
                    } else if tool.exit_code.is_none() || tool.exit_code == Some(0) {
                        Span::styled("⏺", Style::default().fg(SAGE))
                    } else {
                        Span::styled("⏺", Style::default().fg(ROSE))
                    };

                    let mut header = vec![
                        Span::raw("  "),
                        icon_span,
                        Span::raw(" "),
                        Span::styled(
                            format!("{} ", tool_glyph(&tool.name)),
                            Style::default().fg(MUTED),
                        ),
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
                            format!(" · {}", tool.tier),
                            Style::default().fg(AMBER),
                        ));
                    }
                    let mut meta = format!("  {}", fmt_elapsed_compact(elapsed_dur.as_secs()));
                    if let Some(code) = tool.exit_code.filter(|&c| c != 0) {
                        meta.push_str(&format!(" · exit {code}"));
                    }
                    header.push(Span::styled(meta, Style::default().fg(MUTED)));
                    lines.push(Line::from(header));

                    // Claude-style `    ⎿  ` hanging child output aligned under tool label.
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
                            let clipped = clip_str(first_preview.trim(), inner.saturating_sub(34));
                            let mut spans = vec![
                                Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                                Span::styled(clipped, style_output_line(first_preview)),
                            ];
                            if total > 1 {
                                spans.push(Span::styled(
                                    format!("  … +{total} lines (^O expand)"),
                                    Style::default().fg(MUTED),
                                ));
                            }
                            lines.push(Line::from(spans));
                        } else {
                            let mut first_row = true;
                            let mut gutter = DiffGutterState::default();
                            if skip > 0 {
                                lines.push(Line::from(vec![
                                    Span::styled("    ⎿  ", Style::default().fg(MUTED)),
                                    Span::styled(
                                        format!("… ({skip} earlier lines hidden · ^O to expand)"),
                                        Style::default().fg(MUTED),
                                    ),
                                ]));
                                first_row = false;
                            }
                            for raw in &out_lines[skip..] {
                                for row_spans in gutter.render_line(raw, inner.saturating_sub(8)) {
                                    let branch = if first_row { "    ⎿  " } else { "       " };
                                    first_row = false;
                                    let mut spans =
                                        vec![Span::styled(branch, Style::default().fg(MUTED))];
                                    spans.extend(row_spans);
                                    lines.push(Line::from(spans));
                                }
                            }
                        }
                    }
                }
                Cell::Notice { kind, text } => {
                    let (prefix, color) = match kind {
                        NoticeKind::Playbook => ("  ✦ ", ACCENT),
                        NoticeKind::Info => ("  ◈ ", MUTED),
                        NoticeKind::Warn => ("  ▲ ", AMBER),
                        NoticeKind::Error => ("  ✕ ", ROSE),
                        NoticeKind::Success => ("  ✓ ", SAGE),
                    };
                    for (idx, wrapped) in wrap_plain(text, inner.saturating_sub(4))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::from(vec![
                            Span::styled(
                                if idx == 0 { prefix } else { "    " },
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

    fn footer_height(&self) -> u16 {
        match self.footer_mode {
            FooterMode::ShortcutOverlay => 5,
            FooterMode::Contextual | FooterMode::QuitReminder | FooterMode::EscHint => 1,
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_frame_at).as_secs_f32();
        self.last_frame_at = now;

        self.sync_status_header();
        if matches!(
            self.footer_mode,
            FooterMode::QuitReminder | FooterMode::EscHint
        ) && self.footer_mode_at.elapsed() >= Duration::from_millis(2500)
        {
            self.footer_mode = FooterMode::Contextual;
        }

        let area = frame.area();
        let has_approval = !self.approvals.is_empty();
        let activity_h: u16 = if self.running && !has_approval { 1 } else { 0 };
        let search_h: u16 = if self.search_open { 1 } else { 0 };
        let bottom_panel_h: u16 = if has_approval {
            7
        } else {
            self.composer.desired_height(area.width as usize)
        };
        let footer_h = self.footer_height();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(activity_h),
                Constraint::Length(search_h),
                Constraint::Length(bottom_panel_h),
                Constraint::Length(footer_h),
            ])
            .split(area);

        // 1. Borderless transcript viewport with smooth eased scroll
        let transcript_area = chunks[0];
        let mut lines = self.build_transcript_lines(transcript_area.width as usize);

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

        let max_scroll = lines.len().saturating_sub(transcript_area.height as usize) as u16;
        self.scroll.sync_bounds(max_scroll, self.motion);
        self.scroll.step(dt, self.motion);

        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll.current_u16(), 0)),
            transcript_area,
        );

        if self.scroll.show_tail_badge() && transcript_area.width > 24 {
            let badge = " ↓ live tail (End) ";
            let badge_w = display_width(badge) as u16;
            let badge_area = Rect {
                x: transcript_area.x + transcript_area.width.saturating_sub(badge_w + 2),
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

        // 2. Breathing Claude star + Codex cadenced column-width shimmer row
        if activity_h > 0 {
            let run_elapsed = self.run_started.map(|t| t.elapsed()).unwrap_or_default();
            let pretty_elapsed = fmt_elapsed_compact(run_elapsed.as_secs());
            let mut spans = vec![
                Span::raw("  "),
                breathing_star(run_elapsed, self.motion, false),
                Span::raw(" "),
            ];
            spans.extend(summary_shimmer(
                &self.status_header,
                self.status_header_started.elapsed(),
                self.motion,
            ));
            spans.push(Span::styled(
                format!("  ({pretty_elapsed} · esc to interrupt · enter to steer)"),
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
                    "  Find: ",
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
            let cur_x = chunks[2].x + 8 + display_width(&self.search_query) as u16;
            if cur_x < chunks[2].x + chunks[2].width {
                frame.set_cursor_position((cur_x, chunks[2].y));
            }
        }

        // 4. Bottom pane: rounded approval card OR hairline borderless composer
        if let Some(pending) = self.approvals.front() {
            self.render_approval_panel(frame, chunks[3], pending);
        } else {
            let (rule_color, prompt_color, placeholder) = if self.running {
                let run_elapsed = self.run_started.map(|t| t.elapsed()).unwrap_or_default();
                (
                    active_rule_color(run_elapsed, self.motion),
                    AMBER,
                    "Type steering guidance and press Enter (or Esc to interrupt)…",
                )
            } else if !self.composer.is_empty() {
                (HAIRLINE_FOCUS, ACCENT, "")
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

        // 5. Codex `FooterMode` bottom bar / `?` 2-column shortcut overlay
        self.render_footer(frame, chunks[4]);
    }

    fn render_approval_panel(&self, frame: &mut Frame, area: Rect, pending: &PendingApproval) {
        if area.height < 6 || area.width < 18 {
            return;
        }
        let rule_len = area.width.saturating_sub(2) as usize;
        let rule = format!(" {}", "─".repeat(rule_len));
        let inner_w = area.width.saturating_sub(4) as usize;
        let cmd_preview = summarize_tool_args(&pending.name, &pending.args);
        let title = if pending.recovered {
            format!("  ▲ Recovered approval ({}) — NO replay", pending.id)
        } else {
            format!(
                "  ▲ Approve {}? ({})",
                title_tool_name(&pending.name),
                pending.id
            )
        };

        let opt = |idx: usize, label: &str| -> Line<'static> {
            let active = self.approval_sel == idx;
            let bg = if active { SURFACE_HI } else { Color::Reset };
            let marker = if active { " ▸ " } else { "   " };
            let fg = if active { IVORY } else { STONE };
            let pad = inner_w.saturating_sub(display_width(marker) + display_width(label));
            Line::from(vec![
                Span::raw("  "),
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

        let preview_line = format!(" $ {}", clip_str(&cmd_preview, inner_w.saturating_sub(4)));
        let preview_pad = inner_w.saturating_sub(display_width(&preview_line));

        let rows = vec![
            Line::from(Span::styled(rule.clone(), Style::default().fg(AMBER))),
            Line::from(Span::styled(
                title,
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::raw("  "),
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
        match self.footer_mode {
            FooterMode::ShortcutOverlay => {
                frame.render_widget(Paragraph::new(shortcut_overlay_lines()), area);
                return;
            }
            FooterMode::QuitReminder => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  Press Ctrl+C again to quit",
                        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                    )),
                    area,
                );
                return;
            }
            FooterMode::EscHint => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "  Press Esc again to clear draft",
                        Style::default().fg(AMBER),
                    )),
                    area,
                );
                return;
            }
            FooterMode::Contextual => {}
        }

        let flash = self
            .status_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(s, _)| s.as_str());

        let left: Line<'static> = if let Some(msg) = flash {
            Line::from(Span::styled(format!("  {msg}"), Style::default().fg(AMBER)))
        } else {
            let mut spans = vec![
                Span::styled(
                    "  ✻ ",
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
                    format!(" · ✦ {book}"),
                    Style::default().fg(AMBER),
                ));
            }
            if let Some(level) = &self.guard_level {
                spans.push(Span::styled(
                    format!(" · ▲ {level}"),
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
            "↑/↓ select · Enter confirm · Esc deny  "
        } else if self.search_open {
            "Enter next · Esc close  "
        } else if !self.composer.is_empty() && self.running {
            "Enter steer · Shift+Enter newline  "
        } else if !self.composer.is_empty() {
            "Enter send · Shift+Enter newline · ^G edit  "
        } else if self.expand_tools {
            "tools:expanded · ^O compact · ? shortcuts  "
        } else {
            "? shortcuts · ^O tools · ^F find  "
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

            // DEC Mode 2026 synchronized update renders each frame atomically with zero tearing.
            let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
            let draw_res = terminal.draw(|f| self.draw(f)).map(|_| ());
            let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
            draw_res.map_err(|e| format!("draw tui: {e}"))?;

            // Wait up to adaptive frame budget, then batch-drain all queued events into one frame.
            if event::poll(self.next_poll_interval()).map_err(|e| e.to_string())? {
                loop {
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
                            Action::CopyCode(block) => copy_to_clipboard(&block),
                            Action::Quit => return Ok(self.running),
                        },
                        Event::Mouse(mouse) => self.handle_mouse(mouse.kind),
                        Event::Resize(_, _) => {}
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO).map_err(|e| e.to_string())? {
                        break;
                    }
                }
            }
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

        for ch in "不要改接口".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SendSteer("不要改接口".into())
        );
    }

    #[test]
    fn question_mark_toggles_codex_shortcut_overlay_and_esc_twice_clears_draft() {
        let mut app = App::new(".");
        app.handle_key(press(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::ShortcutOverlay);

        let backend = TestBackend::new(92, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let frame_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(frame_text.contains("compose in $EDITOR"));
        assert!(frame_text.contains("twig"));

        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::Contextual);

        app.handle_key(press(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::EscHint);
        assert!(!app.composer.is_empty());
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.composer.is_empty());
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
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true
            }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: false
            }
        );
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
    fn renders_claude_welcome_card_tools_shimmer_and_rounded_approval() {
        let mut app = App::new("/tmp/demo");
        let backend = TestBackend::new(92, 28);
        let mut terminal = Terminal::new(backend).unwrap();

        // Empty state renders borderless minimal welcome header
        terminal.draw(|f| app.draw(f)).unwrap();
        let welcome: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(welcome.contains("✻ twig"));
        assert!(!welcome.contains("╭─"));

        // Active conversation renders ⏺ / ⎿ tool trees + shimmer status
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
        app.apply_event(&json!({"ev": "tool.output", "id": "t1", "text": "@@ -1,1 +1,1 @@\n-old\n+new\n"}));
        app.apply_event(&json!({"ev": "tool.completed", "id": "t1", "bytes": 24, "exit_code": 0}));

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
        assert!(active_frame.contains("$ Bash"));
        assert!(active_frame.contains("esc to interrupt"));

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
}
