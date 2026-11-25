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
mod transcript;

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use self::client::{
    archive_session_file, chat_plain, copy_to_clipboard, detect_git_branch,
    edit_file_in_external_editor, edit_in_external_editor, ensure_memory_file,
    fetch_magpie_models, format_verify_report, init_project_files, list_sessions, notify_osc99,
    run_doctor, run_git_diff, run_independent_review, run_local_shell, shorten_home,
    write_export_file, ServeClient, ServerMsg, TerminalGuard,
};
use self::composer::{Composer, PopupAccept};
use self::markdown::{
    active_rule_color, breathing_star, fmt_elapsed_compact, fmt_tokens_compact, render_footer,
    shimmer_is_sweeping, summary_shimmer, FooterMode, FooterView, MotionMode, ACCENT, AMBER,
    HAIRLINE, HAIRLINE_FOCUS, MUTED,
};
use self::transcript::{
    render_approval_panel, summarize_tool_args, title_tool_name, NoticeKind, OverlayPicker,
    PickerOutcome, RenderOpts, Transcript,
};
use crate::config::HarnessConfig;
use crate::llm::Provider;

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

#[derive(Debug, Clone)]
struct PendingApproval {
    id: String,
    name: String,
    args: Value,
    recovered: bool,
}

enum LocalMsg {
    Shell {
        id: String,
        output: String,
        exit_code: Option<i32>,
        elapsed: Duration,
    },
    Review(Result<String, String>),
    Doctor(Result<(String, bool), String>),
    Models(Result<Vec<String>, String>),
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    SendTurn(String),
    SendSteer(String),
    Cancel,
    ResolveApproval { id: String, allow: bool },
    SetPlan(bool),
    NewSession,
    SwitchSession(String),
    RenameSession(String),
    ArchiveSession,
    OpenModelPicker,
    SetModel(String),
    UndoEdit,
    CompactNow,
    VerifySession,
    EditMemory,
    InitProject,
    RunDoctor,
    ExportTranscript(Option<String>),
    RunLocalShell(String),
    RunDiff,
    RunReview,
    ToggleMouse,
    OpenEditor,
    CopyCode(String),
    Quit,
}

struct App {
    repo_raw: String,
    repo_path: String,
    repo_label: String,
    git_branch: Option<String>,
    sid: String,
    title: Option<String>,
    model: String,
    plan_mode: bool,
    playbook: Option<String>,
    guard_level: Option<String>,
    playbooks_meta: Vec<String>,
    skills_meta: String,
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
    transcript: Transcript,
    approvals: VecDeque<PendingApproval>,
    approval_sel: usize,
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
        let now = Instant::now();
        Self {
            repo_raw: repo.to_owned(),
            repo_path,
            repo_label,
            git_branch: detect_git_branch(repo),
            sid: String::new(),
            title: None,
            model: std::env::var("TWIG_MODEL")
                .ok()
                .or_else(Provider::saved_model)
                .unwrap_or_else(|| "magpie".into()),
            plan_mode: false,
            playbook: None,
            guard_level: None,
            playbooks_meta: Vec::new(),
            skills_meta: String::new(),
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
            transcript: Transcript::new(),
            approvals: VecDeque::new(),
            approval_sel: 0,
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
        self.transcript.clear();
        self.approvals.clear();
        self.approval_sel = 0;
        self.overlay_picker = None;
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
        if self.running && self.approvals.is_empty() && self.motion == MotionMode::Animated {
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

    fn apply_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Event(ev) => self.apply_event(&ev),
            ServerMsg::Reply { id, result } => {
                if let Some(sid) = result.get("sid").and_then(Value::as_str) {
                    self.sid = sid.to_owned();
                }
                if let Some(model) = result.get("model").and_then(Value::as_str) {
                    self.model = model.to_owned();
                }
                if let Some(title) = result.get("title").and_then(Value::as_str) {
                    self.title = Some(title.to_owned());
                }
                if let Some(plan) = result.get("plan").and_then(Value::as_bool) {
                    self.plan_mode = plan;
                }
                if let Some(cb) = result.get("context_bytes").and_then(Value::as_u64) {
                    self.context_bytes = cb as usize;
                }
                if let Some(cm) = result.get("context_max").and_then(Value::as_u64) {
                    self.context_max = cm as usize;
                }
                if let Some(turns) = result.get("turns").and_then(Value::as_u64) {
                    self.turns_used = turns;
                }
                if let Some(books) = result.get("playbooks").and_then(Value::as_array) {
                    self.playbooks_meta = books
                        .iter()
                        .filter_map(|b| b["name"].as_str().map(str::to_owned))
                        .collect();
                }
                if let Some(skills) = result.get("skills").and_then(Value::as_str) {
                    self.skills_meta = skills.to_owned();
                }
                if self.history_req == Some(id) {
                    self.history_req = None;
                    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
                        self.transcript.replay_history(messages);
                    }
                }
                if self.verify_req == Some(id) {
                    self.verify_req = None;
                    let (chain_ok, report) = format_verify_report(&result);
                    self.transcript.push_notice(
                        if chain_ok {
                            NoticeKind::Success
                        } else {
                            NoticeKind::Error
                        },
                        report,
                    );
                }
                if self.undo_req == Some(id) {
                    self.undo_req = None;
                    if let Some(summary) = result.get("summary").and_then(Value::as_str) {
                        self.flash(summary);
                        self.transcript
                            .push_notice(NoticeKind::Success, format!("undo: {summary}"));
                    }
                }
                if let Some(compacted) = result.get("compacted").and_then(Value::as_bool) {
                    if !compacted {
                        self.transcript.push_notice(
                            NoticeKind::Info,
                            "context is already compact; nothing archived",
                        );
                    }
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
                            self.transcript.push_notice(
                                if reason == "verify_failed" {
                                    NoticeKind::Error
                                } else {
                                    NoticeKind::Warn
                                },
                                format!("turn ended: {reason}"),
                            );
                        } else if result["verified"] == true {
                            self.transcript
                                .push_notice(NoticeKind::Success, "verification passed");
                        }
                        self.pending_notify = Some((
                            "twig".into(),
                            format!("{} · turn {reason}", self.repo_label),
                        ));
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
                self.transcript.push_notice(NoticeKind::Error, message);
            }
            ServerMsg::Stderr(line) => {
                if let Some(rest) = line.strip_prefix("twig: ") {
                    if let Some(sid_part) =
                        rest.split_whitespace().find_map(|w| w.strip_prefix("sid="))
                    {
                        self.sid = sid_part.to_owned();
                    }
                } else {
                    self.transcript.push_notice(NoticeKind::Warn, line);
                }
            }
            ServerMsg::Exited => {
                self.running = false;
                self.run_started = None;
                self.last_delta_at = None;
                self.transcript
                    .push_notice(NoticeKind::Error, "twig serve process exited");
            }
        }
    }

    fn apply_local_msg(&mut self, msg: LocalMsg) {
        match msg {
            LocalMsg::Shell {
                id,
                output,
                exit_code,
                elapsed,
            } => {
                self.transcript
                    .finish_tool(&id, Some(output), exit_code, Some(elapsed));
                self.transcript.follow_tail();
            }
            LocalMsg::Review(res) => match res {
                Ok(text) => {
                    self.transcript
                        .push_notice(NoticeKind::Success, "independent review completed");
                    self.transcript.push_assistant(text);
                }
                Err(e) => self
                    .transcript
                    .push_notice(NoticeKind::Error, format!("review: {e}")),
            },
            LocalMsg::Doctor(res) => match res {
                Ok((output, ok)) => {
                    self.transcript.push_notice(
                        if ok {
                            NoticeKind::Success
                        } else {
                            NoticeKind::Warn
                        },
                        if ok {
                            "twig doctor: all checks passed"
                        } else {
                            "twig doctor reported issues"
                        },
                    );
                    if !output.is_empty() {
                        self.transcript
                            .push_assistant(format!("```text\n{output}\n```"));
                    }
                }
                Err(e) => self
                    .transcript
                    .push_notice(NoticeKind::Error, format!("doctor: {e}")),
            },
            LocalMsg::Models(res) => match res {
                Ok(models) if !models.is_empty() => {
                    self.overlay_picker = Some(OverlayPicker::model(models, self.model.clone()));
                }
                Ok(_) => self.transcript.push_notice(
                    NoticeKind::Info,
                    format!(
                        "active model: {} (magpie returned no model list; use `/model <name>` or `/model reset`)",
                        self.model
                    ),
                ),
                Err(e) => self.transcript.push_notice(
                    NoticeKind::Warn,
                    format!(
                        "active model: {} (could not fetch magpie models: {e}; use `/model <name>`)",
                        self.model
                    ),
                ),
            },
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
            Some("model.usage") => self.absorb_usage(&ev["usage"]),
            Some("context.usage") => {
                if let Some(b) = ev["bytes"].as_u64() {
                    self.context_bytes = b as usize;
                }
                if let Some(m) = ev["max_bytes"].as_u64() {
                    self.context_max = m as usize;
                }
            }
            Some("model.reasoning") => {
                let delta = ev["text"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.last_delta_at = Some(Instant::now());
                    self.transcript.append_reasoning_delta(delta);
                }
            }
            Some("model.retry") => {
                let attempt = ev["attempt"].as_u64().unwrap_or(1);
                let secs = ev["backoff_secs"].as_u64().unwrap_or(1);
                self.transcript.push_notice(
                    NoticeKind::Warn,
                    format!("model transient error — retrying (attempt {attempt}, {secs}s backoff)…"),
                );
            }
            Some("model.delta") => {
                let delta = ev["text"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.last_delta_at = Some(Instant::now());
                    self.transcript.append_assistant_delta(delta);
                }
            }
            Some("tool.started") => {
                let id = ev["id"].as_str().unwrap_or("").to_owned();
                let name = ev["tool"].as_str().unwrap_or("tool").to_owned();
                let tier = ev["tier"].as_str().unwrap_or("auto").to_owned();
                let summary = summarize_tool_args(&name, &ev["args"]);
                self.update_status_header(format!("Running {}", title_tool_name(&name)));
                self.transcript.push_tool_started(id, name, summary, tier);
            }
            Some("tool.output") => {
                self.transcript.append_tool_output(
                    ev["id"].as_str().unwrap_or(""),
                    ev["text"].as_str().unwrap_or(""),
                );
            }
            Some("tool.completed") => {
                self.transcript.finish_tool(
                    ev["id"].as_str().unwrap_or(""),
                    None,
                    ev["exit_code"].as_i64().map(|c| c as i32),
                    None,
                );
            }
            Some("approval.requested") => {
                let id = ev["id"].as_str().unwrap_or("").to_owned();
                let name = ev["name"].as_str().unwrap_or("bash").to_owned();
                let summary = summarize_tool_args(&name, &ev["args"]);
                if !id.is_empty() && !self.approvals.iter().any(|a| a.id == id) {
                    self.approvals.push_back(PendingApproval {
                        id,
                        name: name.clone(),
                        args: ev["args"].clone(),
                        recovered: ev["recovered"] == true,
                    });
                    self.approval_sel = 0;
                    self.pending_notify = Some((
                        "twig · approval needed".into(),
                        format!("{name}: {summary}"),
                    ));
                }
            }
            Some("approval.resolved") => {
                let id = ev["id"].as_str().unwrap_or("");
                let allow = ev["allow"] == true;
                self.approvals.retain(|a| a.id != id);
                self.approval_sel = 0;
                self.transcript.push_notice(
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
                    self.transcript
                        .push_notice(NoticeKind::Playbook, format!("playbook routed · {name}"));
                }
            }
            Some("guard.tripped") => {
                if let Some(level) = ev["level"].as_str() {
                    self.guard_level = Some(level.to_owned());
                    self.transcript
                        .push_notice(NoticeKind::Warn, format!("guard tripped · {level}"));
                }
            }
            Some("mode.changed") => {
                if let Some(plan) = ev["plan"].as_bool() {
                    self.plan_mode = plan;
                }
            }
            Some("context.compacted") => {
                let removed = ev["compaction"]["removed_messages"].as_u64().unwrap_or(0);
                self.transcript.push_notice(
                    NoticeKind::Info,
                    format!("context compacted ({removed} older messages archived)"),
                );
            }
            Some("verify.completed") => {
                let passed = ev["verdict"]["passed"] == true;
                self.transcript.push_notice(
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
                self.transcript
                    .push_notice(NoticeKind::Success, "differential verification passed");
            }
            Some("verify.failed") => {
                self.transcript
                    .push_notice(NoticeKind::Error, "differential verification failed");
            }
            Some("papercut.filed") => {
                let comp = ev["report"]["component"].as_str().unwrap_or("harness");
                self.transcript
                    .push_notice(NoticeKind::Info, format!("papercut filed · {comp}"));
            }
            _ => {}
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
                KeyCode::Char('o') => {
                    let expanded = self.transcript.toggle_expand();
                    self.flash(if expanded {
                        "Tool outputs: expanded"
                    } else {
                        "Tool outputs: compact"
                    });
                    return Action::None;
                }
                KeyCode::Char('f') => {
                    self.transcript.search_toggle();
                    return Action::None;
                }
                KeyCode::Char('g') => return Action::OpenEditor,
                KeyCode::Char('l') => {
                    self.transcript.clear();
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

        if let Some(picker) = self.overlay_picker.as_mut() {
            return match picker.handle_key(key) {
                PickerOutcome::None => Action::None,
                PickerOutcome::Close => {
                    self.overlay_picker = None;
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
            };
        }

        if self.transcript.search_is_open() {
            self.transcript.search_handle_key(key);
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

        if key.code == KeyCode::BackTab {
            return Action::SetPlan(!self.plan_mode);
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
                self.transcript.scroll_by(-12);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.transcript.scroll_by(12);
                return Action::None;
            }
            KeyCode::End
                if key.modifiers.contains(KeyModifiers::CONTROL) || self.composer.is_empty() =>
            {
                self.transcript.follow_tail();
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
                KeyCode::Enter if key.modifiers.is_empty() => {
                    match self.composer.accept_popup_on_enter() {
                        PopupAccept::Execute(cmd) => return self.dispatch_input(&cmd),
                        PopupAccept::Filled => return Action::None,
                        PopupAccept::None => {}
                    }
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
            KeyCode::Enter => {
                let raw = self.composer.take_submitted();
                self.dispatch_input(&raw)
            }
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
                self.transcript.scroll_by(-3);
                Action::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.transcript.scroll_by(3);
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
            MouseEventKind::ScrollUp => self.transcript.scroll_by(-4),
            MouseEventKind::ScrollDown => self.transcript.scroll_by(4),
            _ => {}
        }
    }

    fn dispatch_input(&mut self, raw: &str) -> Action {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Action::None;
        }
        self.set_footer_mode(FooterMode::Contextual);

        if let Some(cmd) = trimmed.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                return Action::RunLocalShell(cmd.to_owned());
            }
            return Action::None;
        }

        match trimmed {
            "/quit" | "/exit" => return Action::Quit,
            "/plan" => return Action::SetPlan(!self.plan_mode),
            "/plan on" => return Action::SetPlan(true),
            "/plan off" => return Action::SetPlan(false),
            "/new" => return Action::NewSession,
            "/archive" => return Action::ArchiveSession,
            "/cancel" => return Action::Cancel,
            "/verify" => return Action::VerifySession,
            "/memory" => return Action::EditMemory,
            "/memory show" => {
                let path = Path::new(&self.repo_raw).join(".twig/preferences.md");
                match std::fs::read_to_string(&path) {
                    Ok(body) if !body.trim().is_empty() => {
                        self.transcript
                            .push_notice(NoticeKind::Info, "project memory (.twig/preferences.md)");
                        self.transcript.push_assistant(body);
                    }
                    Ok(_) => self
                        .transcript
                        .push_notice(NoticeKind::Info, ".twig/preferences.md is empty"),
                    Err(_) => self.transcript.push_notice(
                        NoticeKind::Info,
                        "no .twig/preferences.md yet — run /memory or /init",
                    ),
                }
                return Action::None;
            }
            "/init" => return Action::InitProject,
            "/doctor" => return Action::RunDoctor,
            "/export" => return Action::ExportTranscript(None),
            "/clear" => {
                self.transcript.clear();
                return Action::None;
            }
            "/expand" => {
                self.transcript.toggle_expand();
                return Action::None;
            }
            "/copy" => {
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
            "/editor" => return Action::OpenEditor,
            "/mouse" => return Action::ToggleMouse,
            "/diff" => return Action::RunDiff,
            "/undo" => return Action::UndoEdit,
            "/review" => return Action::RunReview,
            "/compact" => return Action::CompactNow,
            "/help" => {
                self.set_footer_mode(FooterMode::ShortcutOverlay);
                return Action::None;
            }
            "/resume" => {
                let sessions = list_sessions();
                if sessions.is_empty() {
                    self.transcript
                        .push_notice(NoticeKind::Info, "no saved sessions found");
                } else {
                    self.overlay_picker = Some(OverlayPicker::resume(sessions));
                }
                return Action::None;
            }
            "/status" => {
                let ctx_pct = self
                    .context_pct()
                    .map(|p| format!("{p}%"))
                    .unwrap_or_else(|| "—".into());
                let total_tok = self.input_tokens + self.output_tokens;
                let cache_pct = (self.cached_tokens * 100)
                    .checked_div(self.input_tokens)
                    .map(|p| format!(" · cache {p}%"))
                    .unwrap_or_default();
                let title = self
                    .title
                    .as_deref()
                    .map(|t| format!(" · title:{t}"))
                    .unwrap_or_default();
                let mode = if self.plan_mode { "plan (RO)" } else { "edit" };
                self.transcript.push_notice(
                    NoticeKind::Info,
                    format!(
                        "sid:{} · model:{} · mode:{mode} · turns:{} · ctx:{ctx_pct} ({}/{}B) · {}{cache_pct}{title}",
                        if self.sid.is_empty() { "—" } else { &self.sid },
                        self.model,
                        self.turns_used,
                        self.context_bytes,
                        self.context_max,
                        fmt_tokens_compact(total_tok)
                    ),
                );
                return Action::None;
            }
            "/skills" | "/playbooks" => {
                let books = if self.playbooks_meta.is_empty() {
                    "bug-fix, feature, port".to_owned()
                } else {
                    self.playbooks_meta.join(", ")
                };
                let skills = if self.skills_meta.trim().is_empty() {
                    "none in .twig/skills or ~/.config/twig/skills".to_owned()
                } else {
                    self.skills_meta.clone()
                };
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    format!("playbooks: {books} · skills: {skills}"),
                );
                return Action::None;
            }
            "/config" => match HarnessConfig::resolve() {
                Ok((cfg, src)) => self.transcript.push_notice(
                    NoticeKind::Info,
                    format!(
                        "config:{src} · max_turns:{} · stall_turns:{} · context_max:{}KB · timeout:{}s · isolation:{}",
                        cfg.max_turns,
                        cfg.stall_turns,
                        cfg.context_max_bytes / 1024,
                        cfg.bash_timeout_secs,
                        cfg.isolation
                    ),
                ),
                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
            },
            "/model" => return Action::OpenModelPicker,
            "/find" => {
                self.transcript.search_open_with("");
                return Action::None;
            }
            _ => {}
        }
        if trimmed == "/config" {
            return Action::None;
        }

        if let Some(path) = trimmed.strip_prefix("/export ") {
            let path = path.trim();
            return Action::ExportTranscript((!path.is_empty()).then(|| path.to_owned()));
        }
        if let Some(sid) = trimmed.strip_prefix("/resume ") {
            let sid = sid.trim();
            if !sid.is_empty() {
                return Action::SwitchSession(sid.to_owned());
            }
            return Action::None;
        }
        if let Some(title) = trimmed.strip_prefix("/rename ") {
            let title = title.trim();
            if !title.is_empty() {
                return Action::RenameSession(title.to_owned());
            }
            return Action::None;
        }
        if let Some(model) = trimmed.strip_prefix("/model ") {
            let model = model.trim();
            if !model.is_empty() {
                return Action::SetModel(model.to_owned());
            }
            return Action::None;
        }
        if let Some(q) = trimmed.strip_prefix("/find ") {
            self.transcript.search_open_with(q.trim());
            return Action::None;
        }
        if let Some(rest) = trimmed.strip_prefix("/steer ") {
            let text = rest.trim().to_owned();
            if !text.is_empty() {
                self.transcript.push_steering(text.clone());
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
            self.transcript.push_steering(text.clone());
            return Action::SendSteer(text);
        }

        let text = trimmed.to_owned();
        self.transcript.push_user(text.clone());
        self.running = true;
        self.run_started = Some(Instant::now());
        self.update_status_header(String::from("Thinking"));
        Action::SendTurn(text)
    }

    fn context_pct(&self) -> Option<usize> {
        (self.context_max > 0).then(|| ((self.context_bytes * 100) / self.context_max).min(100))
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
        let has_picker = self.overlay_picker.is_some();
        let search_open = self.transcript.search_is_open();
        let activity_h: u16 = if self.running && !has_approval && !has_picker {
            1
        } else {
            0
        };
        let search_h: u16 = if search_open { 1 } else { 0 };
        let bottom_panel_h: u16 = if let Some(picker) = &self.overlay_picker {
            picker.desired_height()
        } else if has_approval {
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

        let run_elapsed = self.run_started.map(|t| t.elapsed()).unwrap_or_default();
        let live_streaming = self.running
            && self
                .last_delta_at
                .is_some_and(|t| t.elapsed() < Duration::from_millis(600));
        self.transcript.render(
            frame,
            chunks[0],
            RenderOpts {
                repo_path: &self.repo_path,
                live_streaming,
                run_elapsed,
                dt,
                motion: self.motion,
            },
        );

        if activity_h > 0 {
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

        if search_open {
            self.transcript.render_search_bar(frame, chunks[2]);
        }

        if let Some(picker) = &self.overlay_picker {
            picker.render(frame, chunks[3]);
        } else if let Some(pending) = self.approvals.front() {
            render_approval_panel(
                frame,
                chunks[3],
                &pending.id,
                &pending.name,
                &pending.args,
                pending.recovered,
                self.approval_sel,
            );
        } else {
            let (rule_color, prompt_color, placeholder) = if self.running {
                (
                    active_rule_color(run_elapsed, self.motion),
                    AMBER,
                    "Type steering guidance and press Enter (or Esc to interrupt)…",
                )
            } else if self.plan_mode {
                (
                    AMBER,
                    AMBER,
                    if self.composer.is_empty() {
                        "Plan mode (read-only) — ask twig to explore or design… (Shift+Tab to exit)"
                    } else {
                        ""
                    },
                )
            } else if !self.composer.is_empty() {
                (HAIRLINE_FOCUS, ACCENT, "")
            } else {
                (
                    HAIRLINE,
                    ACCENT,
                    "Ask twig anything… (/ command · @ file · ! shell)",
                )
            };
            self.composer.render(
                frame,
                chunks[3],
                rule_color,
                prompt_color,
                placeholder,
                !search_open,
            );
            if !search_open {
                self.composer
                    .render_popup(frame, chunks[3], chunks[0].height);
            }
        }

        let flash = self
            .status_flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(s, _)| s.as_str());
        let right_hint = if let Some(picker) = &self.overlay_picker {
            picker.hint()
        } else if !self.approvals.is_empty() {
            "↑/↓ select · Enter confirm · Esc deny  "
        } else if self.transcript.search_is_open() {
            "Enter next · Esc close  "
        } else if !self.composer.is_empty() && self.running {
            "Enter steer · Shift+Enter newline  "
        } else if !self.composer.is_empty() {
            "Enter send · Shift+Enter newline · ^G edit  "
        } else if self.plan_mode {
            "Shift+Tab exit plan · ? shortcuts  "
        } else if self.transcript.is_expanded() {
            "tools:expanded · ^O compact · ? shortcuts  "
        } else {
            "? shortcuts · Shift+Tab plan · ^F find  "
        };
        render_footer(
            frame,
            chunks[4],
            &FooterView {
                mode: self.footer_mode,
                flash,
                repo_label: &self.repo_label,
                git_branch: self.git_branch.as_deref(),
                model: &self.model,
                title: self.title.as_deref(),
                sid: &self.sid,
                plan_mode: self.plan_mode,
                context_pct: self.context_pct(),
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_tokens: self.cached_tokens,
                playbook: self.playbook.as_deref(),
                guard_level: self.guard_level.as_deref(),
                mouse_enabled: self.mouse_enabled,
                right_hint,
            },
        );
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

    fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
        local_tx: &Sender<LocalMsg>,
        local_rx: &Receiver<LocalMsg>,
    ) -> Result<bool, String> {
        loop {
            while let Some(msg) = client.try_recv() {
                self.apply_server_msg(msg);
            }
            while let Ok(msg) = local_rx.try_recv() {
                self.apply_local_msg(msg);
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
                        Event::Key(key) => match self.handle_key(key) {
                            Action::None => {}
                            Action::SendTurn(text) => {
                                match client.send("turn", json!({"input": text})) {
                                    Ok(id) => self.active_turn_req = Some(id),
                                    Err(e) => {
                                        self.running = false;
                                        self.transcript.push_notice(NoticeKind::Error, e);
                                    }
                                }
                            }
                            Action::SendSteer(text) => {
                                if let Err(e) = client.send("steer", json!({"input": text})) {
                                    self.transcript.push_notice(NoticeKind::Error, e);
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
                                    self.transcript.push_notice(NoticeKind::Error, e);
                                }
                            }
                            Action::SetPlan(next) => {
                                match client.send("mode.set", json!({"plan": next})) {
                                    Ok(_) => {
                                        self.plan_mode = next;
                                        let msg = if next {
                                            "Plan mode ON (read-only · Shift+Tab to exit)"
                                        } else {
                                            "Plan mode OFF (full edit access)"
                                        };
                                        self.flash(msg);
                                        self.transcript.push_notice(NoticeKind::Playbook, msg);
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::NewSession => {
                                let cancel = self.running;
                                self.reset_session_view();
                                let repo = self.repo_raw.clone();
                                match self.connect_session(&repo, None) {
                                    Ok(new_client) => {
                                        let old = std::mem::replace(client, new_client);
                                        old.shutdown(cancel);
                                        self.flash("Started new session");
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::SwitchSession(sid) => {
                                let cancel = self.running;
                                self.reset_session_view();
                                let repo = self.repo_raw.clone();
                                match self.connect_session(&repo, Some(&sid)) {
                                    Ok(new_client) => {
                                        let old = std::mem::replace(client, new_client);
                                        old.shutdown(cancel);
                                        self.flash(format!("Resumed session {sid}"));
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::ArchiveSession => {
                                let old_sid = self.sid.clone();
                                let cancel = self.running;
                                self.reset_session_view();
                                let repo = self.repo_raw.clone();
                                match self.connect_session(&repo, None) {
                                    Ok(new_client) => {
                                        let old = std::mem::replace(client, new_client);
                                        old.shutdown(cancel);
                                        match archive_session_file(&old_sid) {
                                            Ok(dst) => {
                                                let short: String =
                                                    old_sid.chars().take(10).collect();
                                                self.flash(format!("Archived {short}"));
                                                self.transcript.push_notice(
                                                    NoticeKind::Success,
                                                    format!("archived session → {dst}"),
                                                );
                                            }
                                            Err(e) => {
                                                self.transcript.push_notice(NoticeKind::Warn, e);
                                            }
                                        }
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::RenameSession(title) => {
                                match client.send("session.rename", json!({"title": title})) {
                                    Ok(_) => {
                                        self.title = Some(title.clone());
                                        self.flash(format!("Renamed session → {title}"));
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::OpenModelPicker => {
                                self.flash("Fetching models from magpie…");
                                let tx = local_tx.clone();
                                std::thread::spawn(move || {
                                    let _ = tx.send(LocalMsg::Models(fetch_magpie_models()));
                                });
                            }
                            Action::SetModel(model) => {
                                match client.send("model.set", json!({"model": model})) {
                                    Ok(_) => self.flash(format!("Switching model → {model} (saved)")),
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::UndoEdit => match client.send("edit.undo", json!({})) {
                                Ok(id) => {
                                    self.undo_req = Some(id);
                                    self.flash("Reverting last edit…");
                                }
                                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                            },
                            Action::CompactNow => {
                                match client.send("session.compact", json!({})) {
                                    Ok(_) => self.flash("Compacting context…"),
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::VerifySession => {
                                match client.send("session.verify", json!({})) {
                                    Ok(id) => {
                                        self.verify_req = Some(id);
                                        self.flash("Verifying session hash chain…");
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::EditMemory => match ensure_memory_file(&self.repo_raw) {
                                Ok((path, created)) => {
                                    TerminalGuard::suspend();
                                    let edited = edit_file_in_external_editor(&path);
                                    TerminalGuard::resume(self.mouse_enabled)?;
                                    terminal.clear().map_err(|e| e.to_string())?;
                                    match edited {
                                        Ok(lines) => {
                                            let verb = if created { "Created" } else { "Saved" };
                                            let msg = format!(
                                                "{verb} .twig/preferences.md ({lines} lines) — auto-loaded on next turn"
                                            );
                                            self.flash(&msg);
                                            self.transcript.push_notice(NoticeKind::Success, msg);
                                        }
                                        Err(e) => self.flash(format!("Editor failed: {e}")),
                                    }
                                }
                                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                            },
                            Action::InitProject => match init_project_files(&self.repo_raw) {
                                Ok((created, existing)) => {
                                    let mut parts = Vec::new();
                                    if !created.is_empty() {
                                        parts.push(format!("created {}", created.join(", ")));
                                    }
                                    if !existing.is_empty() {
                                        parts.push(format!("kept existing {}", existing.join(", ")));
                                    }
                                    let msg = format!("twig init: {}", parts.join(" · "));
                                    self.flash("Initialized .twig/");
                                    self.transcript.push_notice(NoticeKind::Success, msg);
                                }
                                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                            },
                            Action::RunDoctor => {
                                self.flash("Running twig doctor…");
                                let tx = local_tx.clone();
                                let repo = self.repo_raw.clone();
                                std::thread::spawn(move || {
                                    let _ = tx.send(LocalMsg::Doctor(run_doctor(&repo)));
                                });
                            }
                            Action::ExportTranscript(custom) => {
                                let md = self.transcript.export_markdown(
                                    &self.sid,
                                    self.title.as_deref(),
                                    &self.model,
                                    &self.repo_path,
                                );
                                match write_export_file(
                                    &self.repo_raw,
                                    &self.sid,
                                    custom.as_deref(),
                                    &md,
                                ) {
                                    Ok(pretty) => {
                                        self.flash("Exported transcript to Markdown");
                                        self.transcript.push_notice(
                                            NoticeKind::Success,
                                            format!(
                                                "exported transcript ({} bytes) → {pretty}",
                                                md.len()
                                            ),
                                        );
                                    }
                                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                                }
                            }
                            Action::RunLocalShell(cmd) => {
                                let id = format!("local-{}", self.next_local_id);
                                self.next_local_id += 1;
                                self.transcript.push_tool_started(
                                    id.clone(),
                                    "bash".into(),
                                    format!("! {cmd}"),
                                    "local".into(),
                                );
                                let tx = local_tx.clone();
                                let repo = self.repo_raw.clone();
                                std::thread::spawn(move || {
                                    let (output, exit_code, elapsed) = run_local_shell(&repo, &cmd);
                                    let _ = tx.send(LocalMsg::Shell {
                                        id,
                                        output,
                                        exit_code,
                                        elapsed,
                                    });
                                });
                            }
                            Action::RunDiff => match run_git_diff(&self.repo_raw) {
                                Ok(diff) if diff.trim().is_empty() => {
                                    self.transcript.push_notice(
                                        NoticeKind::Info,
                                        "working tree clean — no diff",
                                    );
                                }
                                Ok(diff) => {
                                    let id = format!("diff-{}", self.next_local_id);
                                    self.next_local_id += 1;
                                    self.transcript.push_tool_snapshot(
                                        id,
                                        "diff".into(),
                                        "git diff".into(),
                                        "local".into(),
                                        diff,
                                        Some(0),
                                    );
                                }
                                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                            },
                            Action::RunReview => {
                                self.transcript.push_notice(
                                    NoticeKind::Playbook,
                                    "starting independent read-only review on current git diff…",
                                );
                                self.transcript.follow_tail();
                                let tx = local_tx.clone();
                                let repo = self.repo_raw.clone();
                                std::thread::spawn(move || {
                                    let _ = tx
                                        .send(LocalMsg::Review(run_independent_review(&repo)));
                                });
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

fn run_tui(repo: &str, resume: Option<&str>) -> Result<(), String> {
    let mut app = App::new(repo);
    let mut client = app.connect_session(repo, resume)?;
    let (local_tx, local_rx) = mpsc::channel();

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(|e| format!("init terminal: {e}"))?;

    let cancel_on_exit = app.run_loop(&mut terminal, &mut client, &local_tx, &local_rx)?;

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
            Action::NewSession
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
            Action::ResolveApproval { id: "call_1".into(), allow: true }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval { id: "call_1".into(), allow: false }
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
        let welcome = render_frame_text(&mut app, 100, 28);
        assert!(welcome.contains("✻ twig") && !welcome.contains("╭─"));

        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetPlan(true)
        );
        app.plan_mode = true;
        assert_eq!(app.dispatch_input("/plan"), Action::SetPlan(false));
        assert_eq!(app.dispatch_input("/plan on"), Action::SetPlan(true));
        assert_eq!(app.dispatch_input("/memory"), Action::EditMemory);
        assert_eq!(app.dispatch_input("/init"), Action::InitProject);
        assert_eq!(app.dispatch_input("/doctor"), Action::RunDoctor);
        assert_eq!(app.dispatch_input("/archive"), Action::ArchiveSession);
        assert_eq!(app.dispatch_input("/verify"), Action::VerifySession);
        assert_eq!(app.dispatch_input("/undo"), Action::UndoEdit);
        assert_eq!(app.dispatch_input("/model"), Action::OpenModelPicker);
        assert_eq!(app.dispatch_input("/export"), Action::ExportTranscript(None));
        assert_eq!(
            app.dispatch_input("/export out/chat.md"),
            Action::ExportTranscript(Some("out/chat.md".into()))
        );

        app.apply_server_msg(ServerMsg::Reply {
            id: 1,
            result: json!({
                "pong": true, "sid": "abc123456789", "model": "demo-model",
                "plan": true, "context_bytes": 3400, "context_max": 10000
            }),
        });
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

        let active_frame = render_frame_text(&mut app, 110, 28);
        assert!(active_frame.contains("◈ plan"));
        assert!(active_frame.contains("ctx 34%"));
        assert!(active_frame.contains("cache 80%"));
        assert!(active_frame.contains("blake3 chain: ok (12 entries)"));
        assert!(active_frame.contains("$ Bash"));

        app.apply_event(&json!({
            "ev": "approval.requested", "id": "t2", "name": "bash",
            "args": {"command": "rm -rf /"}
        }));
        let approval_frame = render_frame_text(&mut app, 100, 28);
        assert!(approval_frame.contains("Approve Bash?"));
        assert!(approval_frame.contains("1. Yes, allow once"));
    }
}
