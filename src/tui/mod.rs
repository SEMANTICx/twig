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
mod markdown;
mod math_table;
mod overlays;
mod slash;
mod transcript;

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
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{
    build_turn_params, chat_plain, copy_to_clipboard, detect_git_branch, edit_in_external_editor,
    notify_osc99, shorten_home, LocalWorkers, ServeClient, ServerMsg, TerminalGuard,
};
use self::composer::{Composer, SlashCommand};
use self::markdown::{
    active_rule_color, breathing_star, fmt_elapsed_compact, render_footer, shimmer_is_sweeping,
    summary_shimmer, FooterMode, FooterView, MotionMode, ACCENT, AMBER, BASH_MODE_COLOR, HAIRLINE,
    HAIRLINE_FOCUS, IVORY, MUTED,
};
use self::overlays::{
    btw_card_height, queued_strip_height, render_btw_card, render_queued_strip,
    render_subagent_strip, render_todo_strip, subagent_strip_height, title_tool_name,
    todo_strip_height, ApprovalKeyResult, ApprovalQueue, BtwCard, OverlayPicker, PickerOutcome,
    SubagentEntry, SubagentStatus, TodoEntry,
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

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    SendTurn(String),
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
    active_turn_req: Option<u64>,
    history_req: Option<u64>,
    verify_req: Option<u64>,
    undo_req: Option<u64>,
    fork_req: Option<u64>,
    rewind_req: Option<u64>,
    rewind_prefill: Option<String>,
    mcp_req: Option<u64>,
    blackboard_req: Option<u64>,
    tasks_req: Option<u64>,
    skills_req: Option<u64>,
    roles_req: Option<u64>,
    btw_req: Option<u64>,
    apply_req: Option<u64>,
    commit_req: Option<u64>,
    pr_req: Option<u64>,
    worktree_req: Option<u64>,
    queued_turns: std::collections::VecDeque<String>,
    auto_send_next: Option<String>,
    last_shell_cmd: Option<String>,
    last_shell: Option<(String, String, Option<i32>)>,
    running_tasks: usize,
    cached_tasks: Vec<Value>,
    open_tasks_drawer: bool,
    todos: Vec<TodoEntry>,
    subagents: Vec<SubagentEntry>,
    subagent_transcripts: std::collections::HashMap<String, Transcript>,
    viewing_subagent: Option<String>,
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
            active_turn_req: None,
            history_req: None,
            verify_req: None,
            undo_req: None,
            fork_req: None,
            rewind_req: None,
            rewind_prefill: None,
            mcp_req: None,
            blackboard_req: None,
            tasks_req: None,
            skills_req: None,
            roles_req: None,
            btw_req: None,
            apply_req: None,
            commit_req: None,
            pr_req: None,
            worktree_req: None,
            queued_turns: std::collections::VecDeque::new(),
            auto_send_next: None,
            last_shell_cmd: None,
            last_shell: None,
            running_tasks: 0,
            cached_tasks: Vec::new(),
            open_tasks_drawer: false,
            todos: Vec::new(),
            subagents: Vec::new(),
            subagent_transcripts: std::collections::HashMap::new(),
            viewing_subagent: None,
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
        self.active_turn_req = None;
        self.verify_req = None;
        self.undo_req = None;
        self.fork_req = None;
        self.rewind_req = None;
        self.rewind_prefill = None;
        self.mcp_req = None;
        self.blackboard_req = None;
        self.tasks_req = None;
        self.skills_req = None;
        self.roles_req = None;
        self.btw_req = None;
        self.apply_req = None;
        self.commit_req = None;
        self.pr_req = None;
        self.worktree_req = None;
        self.queued_turns.clear();
        self.auto_send_next = None;
        self.running_tasks = 0;
        self.cached_tasks.clear();
        self.open_tasks_drawer = false;
        self.todos.clear();
        self.subagents.clear();
        self.subagent_transcripts.clear();
        self.viewing_subagent = None;
        self.btw_card = None;
        self.pending_images.clear();
        self.transcript.clear();
        self.approvals.clear();
        self.overlay_picker = None;
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
                    return match self.transcript.copy_latest_reply() {
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
                    return match self.transcript.copy_latest_code_block() {
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
                    self.open_tasks_drawer = true;
                    return Action::Slash(SlashCommand::ListTasks);
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
                    self.transcript.clear();
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
                    self.rewind_prefill = Some(prompt);
                    Action::Slash(SlashCommand::RewindSession {
                        turns: turns_back,
                        restore_code,
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
                self.active_transcript_mut().scroll_by(-12);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.active_transcript_mut().scroll_by(12);
                return Action::None;
            }
            KeyCode::Home
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.active_transcript_mut().scroll_to_top();
                return Action::None;
            }
            KeyCode::End
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.active_transcript_mut().follow_tail();
                return Action::None;
            }
            KeyCode::Esc => {
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
                    self.open_tasks_drawer = true;
                    return Action::Slash(SlashCommand::ListTasks);
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

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.active_transcript_mut().scroll_by(-4),
            MouseEventKind::ScrollDown => self.active_transcript_mut().scroll_by(4),
            _ => {}
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
            todo_strip_height(&self.todos)
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
            let banner_bg = Color::Rgb(28, 36, 40);
            let banner_line = Line::from(vec![
                Span::styled(
                    format!(" ◈ Subagent [{role}] "),
                    Style::default()
                        .fg(Color::Rgb(20, 20, 20))
                        .bg(BASH_MODE_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {state_tag}{act_part}{tc_part}  "),
                    Style::default().fg(IVORY).bg(banner_bg),
                ),
                Span::styled(
                    if self.subagents.len() > 1 {
                        "(Tab next · ^O expand · Esc ← main)"
                    } else {
                        "(^O expand · Esc ← main)"
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
            render_todo_strip(frame, chunks[1], &self.todos);
        }
        if let (true, Some(card)) = (btw_h > 0, self.btw_card.as_ref()) {
            render_btw_card(frame, chunks[2], card);
        }
        if queued_h > 0 {
            render_queued_strip(frame, chunks[3], &self.queued_turns);
        }

        if activity_h > 0 {
            let act_pad = if area.width >= 120 {
                "    "
            } else if area.width >= 80 {
                "   "
            } else if area.width >= 56 {
                "  "
            } else {
                " "
            };
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
            let (rule_color, prompt_color, placeholder) = if self.running {
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
            self.history_req = Some(req_id);
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
                if let ServerMsg::Reply { id, ref result } = msg {
                    if self.fork_req == Some(id) {
                        self.fork_req = None;
                        if let Some(new_sid) = result.get("sid").and_then(Value::as_str) {
                            let new_sid = new_sid.to_owned();
                            if self.replace_client(client, Some(&new_sid)) {
                                let short: String = new_sid.chars().take(10).collect();
                                self.flash(format!("Forked session → {short}"));
                            }
                            continue;
                        }
                    }
                }
                self.apply_server_msg(msg);
            }
            while let Some(msg) = workers.try_recv() {
                self.apply_local_msg(msg);
            }
            self.maybe_refresh_git_stat(workers);
            if !self.running && self.approvals.is_empty() {
                if let Some(next_prompt) = self.auto_send_next.take() {
                    let act = self.dispatch_input(&next_prompt);
                    let _ = self.handle_action(act, terminal, client, workers)?;
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

    fn handle_action(
        &mut self,
        action: Action,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
        workers: &LocalWorkers,
    ) -> Result<bool, String> {
        match action {
            Action::None => {}
            Action::SendTurn(text) => {
                // Keep any still-running subagents, clear completed ones from prior turns.
                self.subagents
                    .retain(|s| s.status == SubagentStatus::Running);
                let imgs = std::mem::take(&mut self.pending_images);
                let (params, labels) =
                    build_turn_params(&self.repo_raw, text, imgs, self.last_shell.as_ref());
                if !labels.is_empty() {
                    self.transcript.push_notice(
                        NoticeKind::Info,
                        format!("attached images: {}", labels.join(", ")),
                    );
                }
                match client.send("turn", params) {
                    Ok(id) => self.active_turn_req = Some(id),
                    Err(e) => {
                        self.running = false;
                        self.transcript.push_notice(NoticeKind::Error, e);
                    }
                }
            }
            Action::SendSteer(text) => {
                self.send_rpc(client, "steer", json!({"input": text}), "");
            }
            Action::Cancel => {
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

#[cfg(test)]
mod tests {
    use super::client::LocalMsg;
    use super::*;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn render_frame_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
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
    fn codex_slash_commands_bang_shell_mouse_and_history_replay_work() {
        let mut app = App::new(".");
        for ch in "!git status".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::RunLocalShell("git status".into())
        );

        for ch in "/new".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Slash(SlashCommand::NewSession)
        );
        assert_eq!(
            app.handle_key(press(KeyCode::Char('m'), KeyModifiers::ALT)),
            Action::ToggleMouse
        );

        app.history_req = Some(9);
        app.apply_server_msg(ServerMsg::Reply {
            id: 9,
            result: json!({
                "sid": "sess-123",
                "title": "refactor tui",
                "model": "qwen3",
                "turns": 1,
                "context_bytes": 4096,
                "context_max": 10000,
                "messages": [{"role": "user", "content": "check build"}]
            }),
        });
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.title.as_deref(), Some("refactor tui"));
        assert_eq!(app.context_pct(), Some(40));
    }

    #[test]
    fn question_mark_toggles_codex_shortcut_overlay_and_esc_twice_clears_draft() {
        let mut app = App::new(".");
        app.handle_key(press(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::ShortcutOverlay);
        let frame_text = render_frame_text(&mut app, 92, 24);
        assert!(frame_text.contains("compose in $EDITOR"));
        assert!(frame_text.contains("Shift+Tab"));

        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::Contextual);
        app.handle_key(press(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::EscHint);
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn approval_selector_and_copy_search_shortcuts_work() {
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
                allow: true,
                remember: false,
                note: None,
            }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true,
                remember: true,
                note: None,
            }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: false,
                remember: false,
                note: None,
            }
        );
        app.approvals.clear();

        app.transcript.push_assistant("Code:\n```rust\nlet b = 2;\n```".into());
        assert_eq!(
            app.handle_key(press(KeyCode::Char('y'), KeyModifiers::ALT)),
            Action::CopyCode("let b = 2;".into())
        );
        assert_eq!(
            app.handle_key(press(KeyCode::Char('Y'), KeyModifiers::ALT | KeyModifiers::SHIFT)),
            Action::CopyCode("Code:\n```rust\nlet b = 2;\n```".into())
        );
        app.handle_key(press(KeyCode::Char('f'), KeyModifiers::CONTROL));
        for ch in "let b".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let _ = render_frame_text(&mut app, 90, 24);
        assert_eq!(app.transcript.search_match_count(), 1);
    }

    #[test]
    fn renders_welcome_telemetry_tools_approval_and_new_commands() {
        let mut app = App::new("/tmp/demo");
        app.env_summary = "skills 10 (+13 vault)  ·  docs AGENTS.md  ·  mcp 0  ·  sandbox off".into();
        let welcome = render_frame_text(&mut app, 110, 28);
        assert!(welcome.contains("✻ twig") && !welcome.contains("╭─"));
        assert!(welcome.contains("skills 10 (+13 vault)") && welcome.contains("docs AGENTS.md"));
        assert!(!welcome.contains("ctx 0%"));
        // Responsive rendering across phone (36x16), split pane (60x20), and ultrawide (180x40).
        let phone = render_frame_text(&mut app, 36, 16);
        assert!(phone.contains("✻ twig") && phone.contains("Ask twig… (/ @ $ !)"));
        assert!(render_frame_text(&mut app, 60, 20).contains("✻ twig"));
        assert!(render_frame_text(&mut app, 180, 40).contains("skills 10 (+13 vault)"));

        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetPlan(true)
        );
        app.plan_mode = true;
        assert_eq!(app.dispatch_input("/effort high"), Action::SetEffort("high".into()));
        assert_eq!(
            app.dispatch_input("/go focus on error cases"),
            Action::Slash(SlashCommand::ExecutePlan(Some("focus on error cases".into())))
        );
        assert_eq!(
            app.dispatch_input("/commit"),
            Action::Slash(SlashCommand::CommitChanges(None))
        );
        assert_eq!(
            app.dispatch_input("/fork"),
            Action::Slash(SlashCommand::ForkSession)
        );

        app.apply_server_msg(ServerMsg::Reply {
            id: 1,
            result: json!({
                "pong": true, "sid": "abc123456789", "model": "demo-model",
                "effort": "high", "plan": true, "context_bytes": 3400, "context_max": 10000
            }),
        });
        app.apply_event(&json!({
            "ev": "todo.updated",
            "items": [
                {"content": "Inspect runtime", "status": "completed"},
                {"content": "Wire Codex parity", "status": "in_progress"}
            ]
        }));
        app.apply_event(&json!({
            "ev": "model.usage",
            "usage": {"input_tokens": 1000, "output_tokens": 200, "cached_input_tokens": 800}
        }));
        app.verify_req = Some(17);
        app.apply_server_msg(ServerMsg::Reply {
            id: 17,
            result: json!({
                "chain": {"ok": true, "entries": 12},
                "guard": {"samples": 2, "stall": 0, "digest": "abcdef123456"},
                "policy": {"hits": 5, "misses": 1, "size": 3},
                "plan": true, "read_only": true
            }),
        });
        app.apply_event(&json!({
            "ev": "tool.started", "id": "t1", "tool": "bash",
            "args": {"command": "git diff"}, "tier": "auto"
        }));
        app.apply_event(&json!({"ev": "tool.output", "id": "t1", "text": "@@ -1,1 +1,1 @@\n-old\n+new\n"}));
        app.apply_event(&json!({"ev": "tool.completed", "id": "t1", "bytes": 24, "exit_code": 0}));

        app.apply_event(&json!({
            "ev": "task.started", "id": "bg-1", "command": "cargo test", "running": 1
        }));
        app.apply_event(&json!({
            "ev": "subagent.started", "role": "reviewer", "task": "Audit session recovery", "parallel": true
        }));
        app.apply_local_msg(LocalMsg::GitStat(Some((42, 8))));
        let active_frame = render_frame_text(&mut app, 130, 28);
        assert!(active_frame.contains("◈ plan"));
        assert!(active_frame.contains("+42") && active_frame.contains("-8"));
        assert!(active_frame.contains("⚙ 1 task"));
        assert!(active_frame.contains("high"));
        assert!(active_frame.contains("Plan progress  1/2"));
        assert!(active_frame.contains("◈ Subagents") && active_frame.contains("Audit session recovery"));
        assert!(active_frame.contains("ctx 34%"));
        assert!(active_frame.contains("cache 80%"));
        assert!(active_frame.contains("blake3 chain: ok (12 entries)"));
        assert!(active_frame.contains("$ Bash"));

        app.transcript.push_user("first prompt".into());
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.footer_mode, FooterMode::RewindHint);
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.overlay_picker, Some(OverlayPicker::Rewind { .. })));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Slash(SlashCommand::RewindSession {
                turns: 1,
                restore_code: false
            })
        );
        assert_eq!(app.rewind_prefill.as_deref(), Some("first prompt"));

        app.apply_event(&json!({
            "ev": "approval.requested", "id": "t2", "name": "bash",
            "args": {"command": "rm -rf /"}
        }));
        let approval_frame = render_frame_text(&mut app, 100, 28);
        assert!(approval_frame.contains("Approve Bash?"));
        assert!(approval_frame.contains("1. Yes, allow once"));
        assert!(approval_frame.contains("2. Always allow this pattern"));
    }

    #[test]
    fn claude_cli_benchmark_ux_diff_approval_modes_queue_and_drawers_work() {
        let dir = std::env::temp_dir().join(format!("twig-claude-ux-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "line1\nline2_target\nline3\nline4\n").unwrap();

        let mut app = App::new(dir.to_str().unwrap());

        // 1. Shift+Tab 3-mode cycle: Default -> Plan -> Review Edits -> Default
        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetPlan(true)
        );
        app.plan_mode = true;
        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetMode {
                plan: false,
                gate_edits: true
            }
        );
        app.plan_mode = false;
        app.gate_edits = true;
        assert!(render_frame_text(&mut app, 110, 24).contains("review-edits"));
        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetMode {
                plan: false,
                gate_edits: false
            }
        );
        app.gate_edits = false;

        // 2. Edit stays unfolded as `● Update(...) +1 -1` with word-level diff even while Read/Grep fold!
        app.transcript.push_user("修改配置".into());
        app.transcript.push_tool_snapshot(
            "r1".into(),
            "read".into(),
            "src/lib.rs".into(),
            "auto".into(),
            "line1\n".into(),
            Some(0),
        );
        app.transcript.push_tool_snapshot(
            "g1".into(),
            "grep".into(),
            "line2".into(),
            "auto".into(),
            "src/lib.rs:2".into(),
            Some(0),
        );
        app.transcript.push_tool_snapshot(
            "e1".into(),
            "edit".into(),
            "src/lib.rs".into(),
            "audit".into(),
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n line1\n-let port = 8080;\n+let port = 9090;\n line3\n".into(),
            Some(0),
        );
        let chat_view = render_frame_text(&mut app, 110, 28);
        assert!(chat_view.contains("▸ 2 tools (1 read · 1 search)"));
        assert!(chat_view.contains("● Update(src/lib.rs)") && chat_view.contains("+1") && chat_view.contains("-1"));
        assert!(chat_view.contains("let port = 9090;"));

        // 3. Approval panel renders Edit diff preview & supports inline Option 3 feedback typing!
        app.apply_event(&json!({
            "ev": "approval.requested",
            "id": "ed_1",
            "name": "edit",
            "args": {"path": "src/lib.rs", "old_string": "let a = 1;", "new_string": "let a = 2;"}
        }));
        let ed_frame = render_frame_text(&mut app, 110, 28);
        assert!(ed_frame.contains("Approve Edit?") && ed_frame.contains("let a = 2;"));
        app.handle_key(press(KeyCode::Tab, KeyModifiers::NONE));
        for ch in "use const".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "ed_1".into(),
                allow: false,
                remember: false,
                note: Some("use const".into()),
            }
        );
        app.approvals.clear();

        // 4. Composer `!` bash mode & `#` memory quick-save mode
        app.composer.set("!cargo check");
        assert!(render_frame_text(&mut app, 100, 24).contains("! bash mode"));
        app.composer.set("");
        assert_eq!(app.dispatch_input("# Always run deslop-please"), Action::None);
        assert!(matches!(app.overlay_picker, Some(OverlayPicker::MemorySave { .. })));
        assert_eq!(app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)), Action::None);
        assert!(
            std::fs::read_to_string(dir.join(".twig/preferences.md"))
                .unwrap()
                .contains("Always run deslop-please")
        );

        // 5. Running `Ctrl+B` background detach + Queued pill + `↑` pop-to-edit + `/btw` floating card + `↓` drawer
        app.running = true;
        assert_eq!(
            app.handle_key(press(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Action::BackgroundActive
        );
        app.composer.set("检查一下 @src/lib.rs#L2-3");
        app.handle_key(press(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(app.queued_turns.len(), 1);
        assert!(render_frame_text(&mut app, 110, 28).contains("↳ Queued"));
        // Pressing `↑` in empty composer pops queued message right back into composer!
        app.handle_key(press(KeyCode::Up, KeyModifiers::NONE));
        assert!(app.queued_turns.is_empty());
        assert_eq!(app.composer.raw_text(), "检查一下 @src/lib.rs#L2-3");
        let (turn_params, _) = self::client::build_turn_params(
            dir.to_str().unwrap(),
            app.composer.take_submitted(),
            Vec::new(),
            None,
        );
        assert!(turn_params["input"].as_str().unwrap().contains("line2_target"));

        // `/btw` card floats above composer and dismisses on Space
        app.btw_card = Some(BtwCard {
            question: "why flock?".into(),
            answer: Some("Crash-safe kernel release.".into()),
        });
        assert!(render_frame_text(&mut app, 110, 28).contains("Crash-safe kernel release."));
        app.handle_key(press(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.btw_card.is_none());

        // `↓` in empty composer opens Subagents & Tasks drawer
        app.running = false;
        app.apply_event(&json!({
            "ev": "subagent.started", "id": "sub-1", "role": "scout", "task": "Map tui modules", "parallel": true
        }));
        app.apply_event(&json!({
            "ev": "subagent.started", "id": "sub-2", "role": "scout", "task": "Map runtime tools", "parallel": true
        }));
        let cells_before = app.transcript.len();
        app.apply_event(&json!({
            "ev": "subagent.progress", "id": "sub-1", "role": "scout", "activity": "Read(src/tui/mod.rs)", "turns": 2, "tool_count": 5
        }));
        app.apply_event(&json!({
            "ev": "subagent.progress", "id": "sub-2", "role": "scout", "activity": "Grep(SubagentPool)", "turns": 1, "tool_count": 2
        }));
        app.apply_event(&json!({
            "ev": "subagent.stream", "id": "sub-1", "role": "scout",
            "child": {"ev": "model.delta", "text": "Subagent-1 live finding: TUI uses Ratatui."}
        }));
        app.apply_event(&json!({
            "ev": "subagent.stream", "id": "sub-2", "role": "scout",
            "child": {"ev": "model.delta", "text": "Subagent-2 live finding: Tools run via mpsc."}
        }));
        // Out-of-band progress and child stream must never pollute the main transcript (0-token UI guarantee).
        assert_eq!(app.transcript.len(), cells_before);
        let strip_frame = render_frame_text(&mut app, 128, 28);
        assert!(strip_frame.contains("Read(src/tui/mod.rs) · 5 tools · t2"));
        assert!(strip_frame.contains("Grep(SubagentPool) · 2 tools · t1"));

        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(app.overlay_picker, Some(OverlayPicker::Tasks { .. })));
        app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE));
        let drawer_frame = render_frame_text(&mut app, 120, 28);
        assert!(drawer_frame.contains("trace (t2): Read(src/tui/mod.rs)"));

        // Codex-style live view switcher: `v` enters sub-1 live stream, `Tab` cycles to sub-2, `Esc` returns to main!
        app.handle_key(press(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.viewing_subagent.as_deref(), Some("sub-1"));
        let sub1_frame = render_frame_text(&mut app, 120, 28);
        assert!(sub1_frame.contains("Subagent [scout]") && sub1_frame.contains("Subagent-1 live finding"));
        app.handle_key(press(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.viewing_subagent.as_deref(), Some("sub-2"));
        let sub2_frame = render_frame_text(&mut app, 120, 28);
        assert!(sub2_frame.contains("Subagent-2 live finding"));
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.viewing_subagent.is_none());

        // Scrolling up must NEVER snap back to bottom when background tools/deltas stream in.
        app.transcript.clear();
        app.motion = MotionMode::Reduced;
        for i in 0..35 {
            app.transcript.push_user(format!("Prompt line {i}"));
            app.transcript.push_assistant(format!("Answer line {i}"));
        }
        let _ = render_frame_text(&mut app, 100, 24);
        app.handle_mouse(MouseEventKind::ScrollUp);
        app.handle_key(press(KeyCode::Home, KeyModifiers::NONE));
        let top_frame = render_frame_text(&mut app, 100, 24);
        assert!(top_frame.contains("Prompt line 0"));
        assert!(top_frame.contains("live tail (End)"));

        app.apply_event(&json!({
            "ev": "tool.started", "id": "t99", "tool": "read_file", "args": {"path": "Cargo.toml"}, "tier": "auto"
        }));
        app.apply_event(&json!({"ev": "model.delta", "text": "More streaming output at bottom"}));
        app.transcript.push_assistant("Background review completed".into());
        let still_top = render_frame_text(&mut app, 100, 24);
        assert!(
            still_top.contains("Prompt line 0"),
            "scroll position snapped back to bottom during tool/assistant updates!"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
