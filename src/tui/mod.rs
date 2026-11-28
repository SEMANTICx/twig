//! Borderless Claude-Code warm-terracotta TUI with full Codex-parity workflow:
//! - Zero box borders: pure typography welcome header, inset hairline composer, Codex full-width
//!   slash-command & `@file` selection bar, and hairline approval selector
//! - Full Codex/Claude/twig command suite: `/plan` (`Shift+Tab`), `/new`, `/resume`, `/rename`,
//!   `/archive`, `/status`, `/verify`, `/diff`, `/review`, `/memory`, `/init`, `/doctor`,
//!   `/export`, `/skills`, `/model`, `/compact`, `/config`, `/mouse`, `!cmd`
//! - Live token/cache/context telemetry footer, Kitty OSC 99 desktop notifications,
//!   60fps exponential smooth scroll, cadenced shimmer, breathing floral star, and syntax highlighting

mod client;
mod composer;
mod highlight;
mod markdown;
mod math_table;
mod overlays;
mod selection;
mod slash;
mod strips;
mod theme;
mod transcript;
#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{
    build_turn_params, chat_plain, copy_to_clipboard, detect_git_branch, edit_in_external_editor,
    notify_osc99, shorten_home, LocalWorkers, ServeClient, TerminalGuard,
};
use self::composer::{Composer, SlashCommand};
use self::overlays::{ApprovalKeyResult, ApprovalQueue, OverlayPicker, PickerOutcome};
use self::selection::{MouseSelectAction, TextSelection};
use self::strips::{
    btw_card_height, queued_strip_height, render_btw_card, render_queued_strip,
    render_subagent_strip, render_todo_strip, subagent_strip_height, todo_strip_height, BtwCard,
    SubagentEntry, SubagentStatus, TodoEntry,
};
use self::theme::{
    active_rule_color, breathing_star, fmt_elapsed_compact, horizontal_pad, render_footer,
    shimmer_is_sweeping, summary_shimmer, title_tool_name, FooterMode, FooterView, MotionMode,
    ACCENT, AMBER, BASH_MODE_COLOR, HAIRLINE, HAIRLINE_FOCUS, IVORY, MUTED, ON_ACCENT_FG,
    SUBAGENT_BANNER_BG,
};
use self::transcript::{NoticeKind, RenderOpts, Transcript};
use crate::llm::{Llm, Provider};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingRpc {
    Turn,
    History,
    Verify,
    Undo,
    Fork,
    Rewind { prefill: Option<String> },
    Mcp,
    Blackboard,
    Tasks { open_drawer: bool },
    Skills,
    Roles,
    Btw,
    Apply,
    Commit,
    Pr,
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutoReviewState {
    Idle,
    AwaitTurnEnd { fixes_left: u8 },
    ReadyToSpawn { fixes_left: u8 },
    Reviewing { fixes_left: u8 },
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    SendTurn {
        input: String,
        role: Option<&'static str>,
    },
    SendSubagentTurn {
        id: String,
        child_sid: String,
        role: String,
        model: Option<String>,
        input: String,
    },
    SendSteer(String),
    Cancel,
    BackgroundActive,
    ResolveApproval {
        id: String,
        allow: bool,
        remember: bool,
        note: Option<String>,
    },
    SetPlan(bool),
    SetMode {
        plan: bool,
        gate_edits: bool,
    },
    SetEffort(String),
    SwitchSession(String),
    SetModel(String),
    RunLocalShell(String),
    ToggleMouse,
    OpenEditor,
    CopyCode(String),
    Slash(SlashCommand),
    Quit,
}

struct App {
    repo_raw: String,
    repo_path: String,
    repo_label: String,
    git_branch: Option<String>,
    git_stat: Option<(usize, usize)>,
    git_stat_dirty: bool,
    sid: String,
    title: Option<String>,
    model: String,
    effort: Option<String>,
    plan_mode: bool,
    gate_edits: bool,
    operator_mode: bool,
    study_mode: bool,
    playbook: Option<String>,
    guard_level: Option<String>,
    env_summary: String,
    turns_used: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    context_bytes: usize,
    context_max: usize,
    running: bool,
    run_started: Option<Instant>,
    last_delta_at: Option<Instant>,
    status_header: String,
    status_header_started: Instant,
    pending_rpcs: std::collections::HashMap<u64, PendingRpc>,
    queued_turns: std::collections::VecDeque<String>,
    auto_send_next: Option<String>,
    auto_review: AutoReviewState,
    last_shell_cmd: Option<String>,
    last_shell: Option<(String, String, Option<i32>)>,
    running_tasks: usize,
    cached_tasks: Vec<Value>,
    todos: Vec<TodoEntry>,
    todos_updated_in_turn: bool,
    todos_done_at: Option<Instant>,
    subagents: Vec<SubagentEntry>,
    subagent_transcripts: std::collections::HashMap<String, Transcript>,
    viewing_subagent: Option<String>,
    selection: TextSelection,
    last_screen: Option<ratatui::buffer::Buffer>,
    btw_card: Option<BtwCard>,
    pending_images: Vec<(String, String)>,
    transcript: Transcript,
    approvals: ApprovalQueue,
    overlay_picker: Option<OverlayPicker>,
    composer: Composer,
    mouse_enabled: bool,
    motion: MotionMode,
    footer_mode: FooterMode,
    footer_mode_at: Instant,
    last_frame_at: Instant,
    last_ctrl_c: Option<Instant>,
    status_flash: Option<(String, Instant)>,
    next_local_id: u64,
    pending_notify: Option<(String, String)>,
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
        let provider = Provider::new();
        let now = Instant::now();
        Self {
            repo_raw: repo.to_owned(),
            repo_path,
            repo_label,
            git_branch: detect_git_branch(repo),
            git_stat: None,
            git_stat_dirty: true,
            sid: String::new(),
            title: None,
            model: provider.roles().get("code").to_owned(),
            effort: provider.effort(),
            plan_mode: false,
            gate_edits: false,
            operator_mode: false,
            study_mode: false,
            playbook: None,
            guard_level: None,
            env_summary: String::new(),
            turns_used: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            context_bytes: 0,
            context_max: 0,
            running: false,
            run_started: None,
            last_delta_at: None,
            status_header: String::from("Working"),
            status_header_started: now,
            pending_rpcs: std::collections::HashMap::new(),
            queued_turns: std::collections::VecDeque::new(),
            auto_send_next: None,
            auto_review: AutoReviewState::Idle,
            last_shell_cmd: None,
            last_shell: None,
            running_tasks: 0,
            cached_tasks: Vec::new(),
            todos: Vec::new(),
            todos_updated_in_turn: false,
            todos_done_at: None,
            subagents: Vec::new(),
            subagent_transcripts: std::collections::HashMap::new(),
            viewing_subagent: None,
            selection: TextSelection::default(),
            last_screen: None,
            btw_card: None,
            pending_images: Vec::new(),
            transcript: Transcript::new(),
            approvals: ApprovalQueue::default(),
            overlay_picker: None,
            composer: Composer::new(repo),
            mouse_enabled: true,
            motion: MotionMode::from_env(),
            footer_mode: FooterMode::Contextual,
            footer_mode_at: now,
            last_frame_at: now,
            last_ctrl_c: None,
            status_flash: None,
            next_local_id: 1,
            pending_notify: None,
        }
    }

    fn reset_session_view(&mut self) {
        self.sid.clear();
        self.title = None;
        self.plan_mode = false;
        self.gate_edits = false;
        self.operator_mode = false;
        self.study_mode = false;
        self.playbook = None;
        self.guard_level = None;
        self.turns_used = 0;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_tokens = 0;
        self.context_bytes = 0;
        self.running = false;
        self.run_started = None;
        self.last_delta_at = None;
        self.pending_rpcs.clear();
        self.queued_turns.clear();
        self.auto_send_next = None;
        self.auto_review = AutoReviewState::Idle;
        self.running_tasks = 0;
        self.cached_tasks.clear();
        self.todos.clear();
        self.todos_updated_in_turn = false;
        self.todos_done_at = None;
        self.subagents.clear();
        self.subagent_transcripts.clear();
        self.viewing_subagent = None;
        self.selection.clear();
        self.btw_card = None;
        self.pending_images.clear();
        self.transcript.clear();
        self.approvals.clear();
        self.overlay_picker = None;
    }

    pub(super) fn set_workspace_repo(&mut self, repo: &str, branch_override: Option<String>) {
        let canonical = Path::new(repo)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(repo).to_path_buf());
        self.repo_raw = repo.to_owned();
        self.repo_label = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.to_owned());
        self.repo_path = shorten_home(&canonical.display().to_string());
        self.git_branch = branch_override.or_else(|| detect_git_branch(repo));
        self.composer = Composer::new(repo);
        self.git_stat_dirty = true;
    }

    pub(super) fn begin_turn(
        &mut self,
        input: String,
        header: &str,
        role: Option<&'static str>,
        push_user_cell: bool,
    ) -> Action {
        for s in &mut self.subagents {
            if s.status != SubagentStatus::Running {
                s.active_turn = false;
            }
        }
        if self.todos_done_at.is_some() {
            self.todos.clear();
            self.todos_done_at = None;
        }
        self.todos_updated_in_turn = false;
        if push_user_cell {
            self.transcript.push_user(input.clone());
        }
        self.running = true;
        self.run_started = Some(Instant::now());
        self.update_status_header(header.to_owned());
        Action::SendTurn { input, role }
    }

    fn active_transcript_mut(&mut self) -> &mut Transcript {
        if let Some(id) = self.viewing_subagent.as_deref() {
            if self.subagent_transcripts.contains_key(id) {
                return self.subagent_transcripts.get_mut(id).unwrap();
            }
        }
        &mut self.transcript
    }

    fn cycle_subagent_view(&mut self) {
        if self.subagents.len() <= 1 {
            return;
        }
        let cur_idx = self
            .viewing_subagent
            .as_deref()
            .and_then(|id| self.subagents.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let next = &self.subagents[(cur_idx + 1) % self.subagents.len()];
        self.viewing_subagent = Some(next.id.clone());
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

    fn next_poll_interval(&self) -> Duration {
        if self.transcript.is_scroll_animating(self.motion) {
            return Duration::from_millis(16);
        }
        let has_running_subagent = self
            .subagents
            .iter()
            .any(|s| s.status == SubagentStatus::Running);
        if (self.running || has_running_subagent)
            && self.approvals.is_empty()
            && self.motion == MotionMode::Animated
        {
            let streaming_now = self
                .last_delta_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(600));
            if streaming_now
                || shimmer_is_sweeping(self.status_header_started.elapsed(), self.motion)
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
        let running_subs = self
            .subagents
            .iter()
            .filter(|s| s.status == SubagentStatus::Running)
            .count();
        if running_subs > 1 {
            self.update_status_header(format!("Running {running_subs} subagents"));
            return;
        }
        if let Some(tool_name) = self.transcript.active_tool_name() {
            self.update_status_header(format!("Running {}", title_tool_name(tool_name)));
            return;
        }
        let secs = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let idx = ((secs / 8) as usize) % ACTIVITY_VERBS.len();
        self.update_status_header(ACTIVITY_VERBS[idx].to_owned());
    }

    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(i) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = self.input_tokens.saturating_add(i);
        }
        if let Some(o) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = self.output_tokens.saturating_add(o);
        }
        if let Some(c) = usage.get("cached_input_tokens").and_then(Value::as_u64) {
            self.cached_tokens = self.cached_tokens.saturating_add(c);
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        if self.transcript.search_is_open() {
            self.transcript.search_paste(pasted);
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

        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('Y')
                | KeyCode::Char('y')
                    if key.code == KeyCode::Char('Y')
                        || key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    return match self.active_transcript_mut().copy_latest_reply() {
                        Ok((text, msg)) => {
                            self.flash(msg);
                            Action::CopyCode(text)
                        }
                        Err(e) => {
                            self.flash(e);
                            Action::None
                        }
                    };
                }
                KeyCode::Char('y') => {
                    return match self.active_transcript_mut().copy_latest_code_block() {
                        Ok((text, msg)) => {
                            self.flash(msg);
                            Action::CopyCode(text)
                        }
                        Err(e) => {
                            self.flash(e);
                            Action::None
                        }
                    };
                }
                KeyCode::Char('m' | 'M') => return Action::ToggleMouse,
                KeyCode::Char('v' | 'V') => {
                    self.attach_image(None);
                    return Action::None;
                }
                KeyCode::Enter if self.running && !self.composer.is_empty() => {
                    let text = self.composer.take_submitted();
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        self.queued_turns.push_back(trimmed.to_owned());
                        let n = self.queued_turns.len();
                        self.flash(format!("Queued follow-up #{n} (runs right after active turn)"));
                        self.transcript.push_notice(
                            NoticeKind::Info,
                            format!("queued next #{n}: {trimmed} (`/queue clear` cancels)"),
                        );
                    }
                    return Action::None;
                }
                _ => {}
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.selection.has_range() {
                        if let Some(text) = self.selection.extract_text(self.last_screen.as_ref()) {
                            self.selection.clear();
                            self.flash(format!("Copied selection ({} chars)", text.chars().count()));
                            return Action::CopyCode(text);
                        }
                        self.selection.clear();
                        return Action::None;
                    }
                    if self.overlay_picker.is_some() {
                        self.overlay_picker = None;
                        return Action::None;
                    }
                    if self.transcript.search_is_open() {
                        self.transcript.search_close();
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
                KeyCode::Char('b') if self.running => return Action::BackgroundActive,
                KeyCode::Char('t') => {
                    if matches!(self.overlay_picker, Some(OverlayPicker::Tasks { .. })) {
                        self.overlay_picker = None;
                        return Action::None;
                    }
                    return Action::Slash(SlashCommand::ListTasks { open_drawer: true });
                }
                KeyCode::Char('o') => {
                    let expanded = self.active_transcript_mut().toggle_expand();
                    self.flash(if expanded {
                        "Tool outputs: expanded"
                    } else {
                        "Tool outputs: compact"
                    });
                    return Action::None;
                }
                KeyCode::Char('f') => {
                    self.active_transcript_mut().search_toggle();
                    return Action::None;
                }
                KeyCode::Char('r') => {
                    let hist = self.composer.history();
                    if hist.is_empty() {
                        self.flash("Prompt history is empty");
                    } else {
                        self.overlay_picker = Some(OverlayPicker::history(hist));
                    }
                    return Action::None;
                }
                KeyCode::Char('g') => return Action::OpenEditor,
                KeyCode::Char('l') => {
                    self.active_transcript_mut().clear();
                    return Action::None;
                }
                KeyCode::Char('a' | 'e' | 'u' | 'w' | 'j') => {
                    self.composer.handle_key(key);
                    return Action::None;
                }
                _ => {}
            }
        }

        if let Some(picker) = self.overlay_picker.as_mut() {
            return match picker.handle_key(key) {
                PickerOutcome::None => Action::None,
                PickerOutcome::Close => {
                    self.overlay_picker = None;
                    Action::None
                }
                PickerOutcome::ToggleResumeScope => {
                    let next_all = match self.overlay_picker.as_ref() {
                        Some(OverlayPicker::Resume { all_repos, .. }) => !*all_repos,
                        _ => true,
                    };
                    self.open_resume_picker(next_all);
                    Action::None
                }
                PickerOutcome::SelectSession(sid) => {
                    self.overlay_picker = None;
                    Action::SwitchSession(sid)
                }
                PickerOutcome::SelectModel(model) => {
                    self.overlay_picker = None;
                    Action::SetModel(model)
                }
                PickerOutcome::SelectEffort(level) => {
                    self.overlay_picker = None;
                    Action::SetEffort(level)
                }
                PickerOutcome::RewindTo {
                    turns_back,
                    prompt,
                    restore_code,
                } => {
                    self.overlay_picker = None;
                    Action::Slash(SlashCommand::RewindSession {
                        turns: turns_back,
                        restore_code,
                        prefill: Some(prompt),
                    })
                }
                PickerOutcome::ForkSession => {
                    self.overlay_picker = None;
                    Action::Slash(SlashCommand::ForkSession)
                }
                PickerOutcome::FillComposer(entry) => {
                    self.overlay_picker = None;
                    self.composer.set(&entry);
                    Action::None
                }
                PickerOutcome::ViewSubagent(sub_id) => {
                    self.overlay_picker = None;
                    if !self.subagent_transcripts.contains_key(&sub_id) {
                        let mut tr = Transcript::new();
                        if let Some(entry) = self.subagents.iter().find(|s| s.id == sub_id) {
                            if !entry.task.is_empty() {
                                tr.push_user(entry.task.clone());
                            }
                        }
                        self.subagent_transcripts.insert(sub_id.clone(), tr);
                    }
                    self.viewing_subagent = Some(sub_id);
                    Action::None
                }
                PickerOutcome::CancelTask(task_id) => {
                    self.overlay_picker = None;
                    Action::Slash(SlashCommand::CancelTask(task_id))
                }
                PickerOutcome::SaveMemory { note, global } => {
                    self.overlay_picker = None;
                    match self::client::append_quick_memory(&self.repo_raw, &note, global) {
                        Ok(where_saved) => {
                            let msg = format!("Saved memory to {where_saved}: {note}");
                            self.flash(&msg);
                            self.transcript.push_notice(NoticeKind::Success, msg);
                        }
                        Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                    }
                    Action::None
                }
            };
        }

        if self.active_transcript_mut().search_is_open() {
            self.active_transcript_mut().search_handle_key(key);
            return Action::None;
        }

        match self.approvals.handle_key(key) {
            ApprovalKeyResult::Ignored => {}
            ApprovalKeyResult::Consumed => return Action::None,
            ApprovalKeyResult::Resolve {
                id,
                allow,
                remember,
                note,
            } => {
                return Action::ResolveApproval {
                    id,
                    allow,
                    remember,
                    note,
                };
            }
        }

        if key.code == KeyCode::BackTab {
            // Claude Code 3-mode Shift+Tab cycle:
            // Default (auto-accept workspace edits) -> Plan (read-only) -> Review Edits (gate edits) -> Default
            return if !self.plan_mode && !self.gate_edits {
                Action::SetPlan(true)
            } else if self.plan_mode {
                Action::SetMode {
                    plan: false,
                    gate_edits: true,
                }
            } else {
                Action::SetMode {
                    plan: false,
                    gate_edits: false,
                }
            };
        }

        if matches!(key.code, KeyCode::Char(' '))
            && key.modifiers.is_empty()
            && self.composer.is_empty()
            && self.btw_card.is_some()
        {
            self.btw_card = None;
            return Action::None;
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
            KeyCode::Tab
                if key.modifiers.is_empty()
                    && self.composer.is_empty()
                    && self.viewing_subagent.is_some() =>
            {
                self.cycle_subagent_view();
                return Action::None;
            }
            KeyCode::PageUp => {
                self.selection.clear();
                self.active_transcript_mut().scroll_by(-12);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.selection.clear();
                self.active_transcript_mut().scroll_by(12);
                return Action::None;
            }
            KeyCode::Home
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.selection.clear();
                self.active_transcript_mut().scroll_to_top();
                return Action::None;
            }
            KeyCode::End
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.selection.clear();
                self.active_transcript_mut().follow_tail();
                return Action::None;
            }
            KeyCode::Esc => {
                if self.selection.has_range() {
                    self.selection.clear();
                    return Action::None;
                }
                if self.viewing_subagent.is_some() {
                    self.viewing_subagent = None;
                    self.flash("Returned to main session");
                    return Action::None;
                }
                if self.btw_card.is_some() {
                    self.btw_card = None;
                    return Action::None;
                }
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
                } else {
                    let turns = self.transcript.user_turns();
                    if !turns.is_empty() {
                        if self.footer_mode == FooterMode::RewindHint
                            && self.footer_mode_at.elapsed() < Duration::from_secs(2)
                        {
                            self.set_footer_mode(FooterMode::Contextual);
                            self.overlay_picker = Some(OverlayPicker::rewind(turns));
                        } else {
                            self.set_footer_mode(FooterMode::RewindHint);
                        }
                    }
                }
                return Action::None;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.active_transcript_mut().scroll_by(-3);
                return Action::None;
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.active_transcript_mut().scroll_by(3);
                return Action::None;
            }
            // Claude Code: pressing `↑` in an empty composer pops the latest queued follow-up back for editing!
            KeyCode::Up
                if key.modifiers.is_empty()
                    && self.composer.is_empty()
                    && !self.composer.is_browsing_history()
                    && !self.queued_turns.is_empty() =>
            {
                if let Some(popped) = self.queued_turns.pop_back() {
                    self.composer.set(&popped);
                    self.flash("Popped queued follow-up into composer for editing");
                }
                return Action::None;
            }
            // Claude Code: pressing `↓` in an empty composer focuses the Subagents & Tasks drawer!
            KeyCode::Down
                if key.modifiers.is_empty()
                    && self.composer.is_empty()
                    && !self.composer.is_browsing_history()
                    && (!self.subagents.is_empty()
                        || self.running_tasks > 0
                        || !self.cached_tasks.is_empty()) =>
            {
                if self.running_tasks > 0 && self.cached_tasks.is_empty() {
                    return Action::Slash(SlashCommand::ListTasks { open_drawer: true });
                }
                self.overlay_picker = Some(OverlayPicker::tasks(
                    self.subagents.clone(),
                    self.cached_tasks.clone(),
                ));
                return Action::None;
            }
            _ => {}
        }

        if let Some(submitted) = self.composer.handle_key(key) {
            self.dispatch_input(&submitted)
        } else {
            Action::None
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Action {
        match self.selection.on_mouse(mouse, self.last_screen.as_ref()) {
            MouseSelectAction::None => Action::None,
            MouseSelectAction::Scroll(delta) => {
                self.active_transcript_mut().scroll_by(delta);
                Action::None
            }
            MouseSelectAction::Copied(text) => {
                self.flash(format!("Copied selection ({} chars)", text.chars().count()));
                Action::CopyCode(text)
            }
        }
    }

    fn context_pct(&self) -> Option<usize> {
        (self.context_max > 0).then(|| ((self.context_bytes * 100) / self.context_max).min(100))
    }

    fn footer_view<'a>(&'a self, flash: Option<&'a str>, right_hint: &'a str) -> FooterView<'a> {
        FooterView {
            mode: self.footer_mode,
            flash,
            repo_label: &self.repo_label,
            git_branch: self.git_branch.as_deref(),
            git_stat: self.git_stat,
            model: &self.model,
            effort: self.effort.as_deref(),
            title: self.title.as_deref(),
            sid: &self.sid,
            plan_mode: self.plan_mode,
            gate_edits: self.gate_edits,
            operator_mode: self.operator_mode,
            pending_images: self.pending_images.len(),
            running_tasks: self.running_tasks,
            context_pct: self.context_pct(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            playbook: self.playbook.as_deref(),
            guard_level: self.guard_level.as_deref(),
            mouse_enabled: self.mouse_enabled,
            right_hint,
        }
    }

    fn footer_height(&self) -> u16 {
        match self.footer_mode {
            FooterMode::ShortcutOverlay => 5,
            FooterMode::Contextual
            | FooterMode::QuitReminder
            | FooterMode::EscHint
            | FooterMode::RewindHint => 1,
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_frame_at).as_secs_f32();
        self.last_frame_at = now;

        self.sync_status_header();
        if matches!(
            self.footer_mode,
            FooterMode::QuitReminder | FooterMode::EscHint | FooterMode::RewindHint
        ) && self.footer_mode_at.elapsed() >= Duration::from_millis(2500)
        {
            self.footer_mode = FooterMode::Contextual;
        }

        let area = frame.area();
        let has_approval = !self.approvals.is_empty();
        let has_picker = self.overlay_picker.is_some();
        let search_open = self
            .viewing_subagent
            .as_deref()
            .and_then(|id| self.subagent_transcripts.get(id))
            .map(|t| t.search_is_open())
            .unwrap_or_else(|| self.transcript.search_is_open());
        let todo_h: u16 = if !has_approval && !has_picker && area.height > 14 {
            todo_strip_height(&self.todos, self.running, self.todos_done_at)
        } else {
            0
        };
        let btw_h: u16 = if !has_approval && !has_picker && area.height > 14 {
            btw_card_height(self.btw_card.as_ref(), area.width as usize)
        } else {
            0
        };
        let queued_h: u16 = if !has_approval && !has_picker && area.height > 12 {
            queued_strip_height(&self.queued_turns)
        } else {
            0
        };
        let activity_h: u16 = if self.running && !has_approval && !has_picker {
            1
        } else {
            0
        };
        let search_h: u16 = if search_open { 1 } else { 0 };
        let max_bottom = area.height.saturating_sub(5).max(3);
        let bottom_panel_h: u16 = if let Some(picker) = &self.overlay_picker {
            picker.desired_height().min(max_bottom)
        } else if has_approval {
            self.approvals.desired_height(area.width).min(max_bottom)
        } else {
            let max_c = if area.height < 18 { 5 } else { 8 };
            self.composer.desired_height(area.width as usize, max_c).min(max_bottom)
        };
        let footer_h = self.footer_height().min(area.height.saturating_sub(4).max(1));
        let subagent_h: u16 = if !has_approval && !has_picker && area.height > 14 {
            subagent_strip_height(&self.subagents)
                .min(area.height.saturating_sub(bottom_panel_h + footer_h + 4))
        } else {
            0
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(todo_h),
                Constraint::Length(btw_h),
                Constraint::Length(queued_h),
                Constraint::Length(activity_h),
                Constraint::Length(search_h),
                Constraint::Length(bottom_panel_h),
                Constraint::Length(subagent_h),
                Constraint::Length(footer_h),
            ])
            .split(area);

        let run_elapsed = self.run_started.map(|t| t.elapsed()).unwrap_or_default();
        let live_streaming = self.running
            && self
                .last_delta_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(600));
        let view_sub_id = self.viewing_subagent.clone();
        if let Some(sub_id) = view_sub_id.as_deref() {
            let sub_meta = self.subagents.iter().find(|s| s.id == sub_id).cloned();
            let sub_running = sub_meta
                .as_ref()
                .is_some_and(|s| s.status == SubagentStatus::Running);
            let sub_elapsed = sub_meta
                .as_ref()
                .map(|s| {
                    s.elapsed_secs
                        .map(Duration::from_secs)
                        .unwrap_or_else(|| s.started_at.elapsed())
                })
                .unwrap_or_default();
            let banner_area = Rect {
                x: chunks[0].x,
                y: chunks[0].y,
                width: chunks[0].width,
                height: 1,
            };
            let body_area = Rect {
                x: chunks[0].x,
                y: chunks[0].y.saturating_add(1),
                width: chunks[0].width,
                height: chunks[0].height.saturating_sub(1),
            };
            let role = sub_meta.as_ref().map(|s| s.role.as_str()).unwrap_or("subagent");
            let state_tag = match sub_meta.as_ref().map(|s| s.status) {
                Some(SubagentStatus::Running) => "✦ live",
                Some(SubagentStatus::Completed) => "✓ done",
                Some(SubagentStatus::Failed) => "✕ failed",
                None => "◈ view",
            };
            let act_part = sub_meta
                .as_ref()
                .and_then(|s| s.activity.as_deref())
                .map(|a| format!(" · {a}"))
                .unwrap_or_default();
            let tc_part = sub_meta
                .as_ref()
                .filter(|s| s.tool_count > 0)
                .map(|s| format!(" · {} tools", s.tool_count))
                .unwrap_or_default();
            let banner_bg = SUBAGENT_BANNER_BG;
            let banner_line = Line::from(vec![
                Span::styled(
                    format!(" ◈ Subagent [{role}] "),
                    Style::default()
                        .fg(ON_ACCENT_FG)
                        .bg(BASH_MODE_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {state_tag}{act_part}{tc_part}  "),
                    Style::default().fg(IVORY).bg(banner_bg),
                ),
                Span::styled(
                    if self.subagents.len() > 1 {
                        "(Enter chat · Tab next · ^O expand · Esc ← main)"
                    } else {
                        "(Enter chat · ^O expand · Esc ← main)"
                    },
                    Style::default().fg(MUTED).bg(banner_bg),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(banner_line).style(Style::default().bg(banner_bg)),
                banner_area,
            );
            if let Some(sub_tr) = self.subagent_transcripts.get_mut(sub_id) {
                sub_tr.render(
                    frame,
                    body_area,
                    RenderOpts {
                        repo_path: &self.repo_path,
                        env_summary: &self.env_summary,
                        live_streaming: sub_running,
                        run_elapsed: sub_elapsed,
                        dt,
                        motion: self.motion,
                    },
                );
            }
        } else {
            self.transcript.render(
                frame,
                chunks[0],
                RenderOpts {
                    repo_path: &self.repo_path,
                    env_summary: &self.env_summary,
                    live_streaming,
                    run_elapsed,
                    dt,
                    motion: self.motion,
                },
            );
        }

        if todo_h > 0 {
            render_todo_strip(frame, chunks[1], &self.todos, self.running);
        }
        if let (true, Some(card)) = (btw_h > 0, self.btw_card.as_ref()) {
            render_btw_card(frame, chunks[2], card);
        }
        if queued_h > 0 {
            render_queued_strip(frame, chunks[3], &self.queued_turns);
        }

        if activity_h > 0 {
            let act_pad = " ".repeat(horizontal_pad(area.width as usize));
            let pretty_elapsed = fmt_elapsed_compact(run_elapsed.as_secs());
            let mut spans = vec![
                Span::raw(act_pad),
                breathing_star(run_elapsed, self.motion, false),
                Span::raw("  "),
            ];
            spans.extend(summary_shimmer(
                &self.status_header,
                self.status_header_started.elapsed(),
                self.motion,
            ));
            let q_badge = if self.queued_turns.is_empty() {
                String::new()
            } else {
                format!(" · queued:{}", self.queued_turns.len())
            };
            let act_hint = if area.width < 58 {
                format!(" ({pretty_elapsed}{q_badge} · esc)")
            } else {
                format!("  ({pretty_elapsed}{q_badge} · esc interrupt · ^B bg · enter steer · alt+enter queue)")
            };
            spans.push(Span::styled(act_hint, Style::default().fg(MUTED)));
            frame.render_widget(Paragraph::new(Line::from(spans)), chunks[4]);
        }

        if search_open {
            if let Some(sub_id) = self.viewing_subagent.as_deref() {
                if let Some(sub_tr) = self.subagent_transcripts.get(sub_id) {
                    sub_tr.render_search_bar(frame, chunks[5]);
                }
            } else {
                self.transcript.render_search_bar(frame, chunks[5]);
            }
        }

        if let Some(picker) = &self.overlay_picker {
            picker.render(frame, chunks[6]);
        } else if has_approval {
            self.approvals.render(frame, chunks[6]);
        } else {
            let narrow_ui = area.width < 58;
            let (rule_color, prompt_color, placeholder) = if let Some(sub_id) =
                self.viewing_subagent.as_deref()
            {
                let role = self
                    .subagents
                    .iter()
                    .find(|s| s.id == sub_id)
                    .map(|s| s.role.as_str())
                    .unwrap_or("subagent");
                let prompt_hint = if !self.composer.is_empty() {
                    ""
                } else if narrow_ui {
                    "Message subagent… (Esc ← main)"
                } else {
                    "Message this subagent thread directly… (Enter send · Tab next · Esc ← main)"
                };
                let _ = role;
                (BASH_MODE_COLOR, BASH_MODE_COLOR, prompt_hint)
            } else if self.running {
                (
                    active_rule_color(run_elapsed, self.motion),
                    AMBER,
                    if narrow_ui { "Steer (Enter) / Esc…" } else { "Type steering guidance and press Enter (or Esc to interrupt)…" },
                )
            } else if self.plan_mode {
                (
                    AMBER,
                    AMBER,
                    if !self.composer.is_empty() { "" } else if narrow_ui { "Plan (RO)… (S-Tab)" } else { "Plan mode (read-only) — ask twig to explore or design… (Shift+Tab cycles)" },
                )
            } else if self.gate_edits {
                (
                    AMBER,
                    ACCENT,
                    if !self.composer.is_empty() { "" } else if narrow_ui { "Review edits… (S-Tab)" } else { "Review edits mode — asks approval with diff preview before editing… (Shift+Tab cycles)" },
                )
            } else if !self.composer.is_empty() {
                (HAIRLINE_FOCUS, ACCENT, "")
            } else {
                (
                    HAIRLINE,
                    ACCENT,
                    if narrow_ui { "Ask twig… (/ @ $ !)" } else { "Ask twig anything… (/ command · @ file#L · $ skill · ! shell · # memory)" },
                )
            };
            self.composer.render(
                frame,
                chunks[6],
                rule_color,
                prompt_color,
                placeholder,
                !search_open,
            );
            if !search_open {
                self.composer
                    .render_popup(frame, chunks[6], chunks[0].height);
            }
        }

        if subagent_h > 0 {
            render_subagent_strip(frame, chunks[7], &self.subagents, self.motion);
        }

        let flash = self
            .status_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(s, _)| s.as_str());
        let right_hint = if let Some(picker) = &self.overlay_picker {
            picker.hint()
        } else if !self.approvals.is_empty() {
            "↑/↓ · Enter · a always · Tab feedback · n deny"
        } else if self.transcript.search_is_open() {
            "Enter next · Esc close"
        } else if !self.composer.is_empty() && self.running {
            "Enter steer · Alt+Enter queue · ^B bg"
        } else if !self.composer.is_empty() {
            "Enter send · Shift+Enter newline · ^G"
        } else if self.plan_mode || self.gate_edits {
            "Shift+Tab mode · ^O tools · ?"
        } else if self.transcript.is_expanded() {
            "tools:on · ^O fold · ^T · ?"
        } else {
            "^O tools · Shift+Tab · ^T · ?"
        };
        render_footer(frame, chunks[8], &self.footer_view(flash, right_hint));
        self.last_screen = Some(frame.buffer_mut().clone());
        self.selection.paint(frame.buffer_mut());
    }

    fn connect_session(
        &mut self,
        repo: &str,
        resume: Option<&str>,
    ) -> Result<ServeClient, String> {
        let mut client = ServeClient::spawn(repo, resume)?;
        let _ = client.send("ping", json!({}));
        let _ = client.send("approval.list", json!({}));
        if let Ok(req_id) = client.send("session.history", json!({})) {
            self.pending_rpcs.insert(req_id, PendingRpc::History);
        }
        Ok(client)
    }

    fn replace_client(&mut self, client: &mut ServeClient, resume: Option<&str>) -> bool {
        let cancel = self.running;
        self.reset_session_view();
        let repo = self.repo_raw.clone();
        match self.connect_session(&repo, resume) {
            Ok(new_client) => {
                let old = std::mem::replace(client, new_client);
                old.shutdown(cancel);
                true
            }
            Err(e) => {
                self.transcript.push_notice(NoticeKind::Error, e);
                false
            }
        }
    }

    fn send_rpc(
        &mut self,
        client: &mut ServeClient,
        method: &str,
        params: Value,
        flash: &str,
    ) -> Option<u64> {
        match client.send(method, params) {
            Ok(id) => {
                if !flash.is_empty() {
                    self.flash(flash);
                }
                Some(id)
            }
            Err(e) => {
                self.transcript.push_notice(NoticeKind::Error, e);
                None
            }
        }
    }

    pub(super) fn send_tracked_rpc(
        &mut self,
        client: &mut ServeClient,
        method: &str,
        params: Value,
        flash: &str,
        kind: PendingRpc,
    ) {
        if let Some(id) = self.send_rpc(client, method, params, flash) {
            self.pending_rpcs.insert(id, kind);
        }
    }

    fn maybe_refresh_git_stat(&mut self, workers: &LocalWorkers) {
        if self.git_stat_dirty {
            self.git_stat_dirty = false;
            workers.spawn_git_stat(self.repo_raw.clone());
        }
    }

    fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
        workers: &LocalWorkers,
    ) -> Result<bool, String> {
        loop {
            while let Some(msg) = client.try_recv() {
                if let Some(new_sid) = self.apply_server_msg(msg) {
                    if self.replace_client(client, Some(&new_sid)) {
                        let short: String = new_sid.chars().take(10).collect();
                        self.flash(format!("Forked session → {short}"));
                    }
                }
            }
            while let Some(msg) = workers.try_recv() {
                self.apply_local_msg(msg);
            }
            self.maybe_refresh_git_stat(workers);
            if !self.running && self.approvals.is_empty() {
                if let Some(next_prompt) = self.auto_send_next.take() {
                    let act = self.dispatch_input(&next_prompt);
                    let _ = self.handle_action(act, terminal, client, workers)?;
                } else if self.take_ready_auto_review() {
                    workers.spawn_review(self.repo_raw.clone(), true);
                }
            }
            if let Some((title, body)) = self.pending_notify.take() {
                notify_osc99(&title, &body);
            }

            let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
            let draw_res = terminal.draw(|f| self.draw(f)).map(|_| ());
            let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
            draw_res.map_err(|e| format!("draw tui: {e}"))?;

            if event::poll(self.next_poll_interval()).map_err(|e| e.to_string())? {
                loop {
                    match event::read().map_err(|e| e.to_string())? {
                        Event::Paste(data) => self.handle_paste(&data),
                        Event::Key(key) => {
                            let action = self.handle_key(key);
                            if self.handle_action(action, terminal, client, workers)? {
                                return Ok(self.running);
                            }
                        }
                        Event::Mouse(mouse) => {
                            let action = self.handle_mouse(mouse);
                            if self.handle_action(action, terminal, client, workers)? {
                                return Ok(self.running);
                            }
                        }
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

    fn handle_action(
        &mut self,
        action: Action,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
        workers: &LocalWorkers,
    ) -> Result<bool, String> {
        match action {
            Action::None => {}
            Action::SendTurn { input, role } => {
                let imgs = std::mem::take(&mut self.pending_images);
                let (mut params, labels) =
                    build_turn_params(&self.repo_raw, input, imgs, self.last_shell.as_ref());
                if let Some(r) = role {
                    params["role"] = json!(r);
                }
                if !labels.is_empty() {
                    self.transcript.push_notice(
                        NoticeKind::Info,
                        format!("attached images: {}", labels.join(", ")),
                    );
                }
                match client.send("turn", params) {
                    Ok(id) => {
                        self.pending_rpcs.insert(id, PendingRpc::Turn);
                    }
                    Err(e) => {
                        self.running = false;
                        self.run_started = None;
                        self.transcript.push_notice(NoticeKind::Error, e);
                    }
                }
            }
            Action::SendSubagentTurn {
                id,
                child_sid,
                role,
                model,
                input,
            } => {
                workers.spawn_subagent_turn(
                    self.repo_raw.clone(),
                    self.sid.clone(),
                    id,
                    child_sid,
                    role,
                    model,
                    self.effort.clone(),
                    input,
                );
            }
            Action::SendSteer(text) => {
                self.send_rpc(client, "steer", json!({"input": text}), "");
            }
            Action::Cancel => {
                self.auto_review = AutoReviewState::Idle;
                self.auto_send_next = None;
                let _ = client.send("cancel", json!({}));
            }
            Action::BackgroundActive => {
                if client.send("background", json!({})).is_ok() {
                    self.flash("Detached active command to background (^B)");
                }
            }
            Action::ResolveApproval {
                id,
                allow,
                remember,
                note,
            } => {
                self.send_rpc(
                    client,
                    "approval.resolve",
                    json!({"id": id, "allow": allow, "remember": remember, "note": note}),
                    "",
                );
            }
            Action::SetPlan(next) => {
                if self
                    .send_rpc(
                        client,
                        "mode.set",
                        json!({"plan": next, "gate_edits": false}),
                        "",
                    )
                    .is_some()
                {
                    self.plan_mode = next;
                    self.gate_edits = false;
                    let msg = if next {
                        "Plan mode ON (read-only · Shift+Tab cycles modes)"
                    } else {
                        "Auto-accept edits ON (standard mode)"
                    };
                    self.flash(msg);
                    self.transcript.push_notice(NoticeKind::Playbook, msg);
                }
            }
            Action::SetMode { plan, gate_edits } => {
                if self
                    .send_rpc(
                        client,
                        "mode.set",
                        json!({"plan": plan, "gate_edits": gate_edits}),
                        "",
                    )
                    .is_some()
                {
                    self.plan_mode = plan;
                    self.gate_edits = gate_edits;
                    let msg = if plan {
                        "Plan mode ON (read-only · Shift+Tab cycles modes)"
                    } else if gate_edits {
                        "Review edits mode ON (asks approval with diff preview before edits · Shift+Tab cycles)"
                    } else {
                        "Auto-accept edits ON (standard mode · Shift+Tab cycles)"
                    };
                    self.flash(msg);
                    self.transcript.push_notice(NoticeKind::Playbook, msg);
                }
            }
            Action::SetEffort(level) => {
                self.send_rpc(client, "effort.set", json!({"effort": level}), &format!("Reasoning effort → {level}"));
            }
            Action::SwitchSession(sid) => {
                if self.replace_client(client, Some(&sid)) {
                    self.flash(format!("Resumed session {sid}"));
                }
            }
            Action::SetModel(model) => {
                self.send_rpc(client, "model.set", json!({"model": model}), &format!("Switching model → {model} (saved)"));
            }
            Action::Slash(cmd) => self.handle_slash_io(cmd, terminal, client, workers)?,
            Action::RunLocalShell(cmd) => {
                let id = format!("local-{}", self.next_local_id);
                self.next_local_id += 1;
                self.last_shell_cmd = Some(cmd.clone());
                self.transcript.push_tool_started(id.clone(), "bash".into(), format!("! {cmd}"), "local".into());
                workers.spawn_shell(self.repo_raw.clone(), id, cmd);
            }
            Action::ToggleMouse => {
                self.mouse_enabled = !self.mouse_enabled;
                TerminalGuard::set_mouse(self.mouse_enabled);
                self.flash(if self.mouse_enabled {
                    "Mouse scroll: on"
                } else {
                    "Mouse capture off — drag to select text freely (Alt+M to restore)"
                });
            }
            Action::OpenEditor => {
                let draft = self.composer.draft_expanded();
                TerminalGuard::suspend();
                let edited = edit_in_external_editor(&draft);
                TerminalGuard::resume(self.mouse_enabled)?;
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
            Action::Quit => return Ok(true),
        }
        Ok(false)
    }
}

fn run_tui(repo: &str, resume: Option<&str>) -> Result<(), String> {
    let mut app = App::new(repo);
    let mut client = app.connect_session(repo, resume)?;
    let workers = LocalWorkers::new();

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(|e| format!("init terminal: {e}"))?;

    let cancel_on_exit = app.run_loop(&mut terminal, &mut client, &workers)?;

    drop(terminal);
    drop(_guard);
    client.shutdown(cancel_on_exit);
    Ok(())
}
