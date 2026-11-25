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

use std::io::{self, IsTerminal};
use std::path::Path;
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
    archive_session_file, build_turn_params, chat_plain, copy_to_clipboard, detect_git_branch,
    edit_file_in_external_editor, edit_in_external_editor, ensure_memory_file,
    format_config_report, format_init_report, format_mcp_report, format_tasks_report,
    format_verify_report, init_project_files, list_sessions, load_image_data_url, notify_osc99,
    paste_clipboard_image, read_memory_preview, run_git_diff, shorten_home, write_export_file,
    LocalMsg, LocalWorkers, ServeClient, ServerMsg, TerminalGuard, EVOLVE_PROMPT,
};
use self::composer::{parse_slash, Composer, SlashCommand};
use self::markdown::{
    active_rule_color, breathing_star, fmt_elapsed_compact, format_status_notice, render_footer,
    shimmer_is_sweeping, summary_shimmer, FooterMode, FooterView, MotionMode, ACCENT, AMBER,
    HAIRLINE, HAIRLINE_FOCUS, MUTED,
};
use self::transcript::{
    parse_todos, render_todo_strip, summarize_tool_args, title_tool_name, todo_strip_height,
    ApprovalKeyResult, ApprovalQueue, NoticeKind, OverlayPicker, PickerOutcome, RenderOpts,
    TodoEntry, Transcript,
};
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

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    SendTurn(String),
    SendSteer(String),
    Cancel,
    ResolveApproval {
        id: String,
        allow: bool,
        remember: bool,
    },
    SetPlan(bool),
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
    playbook: Option<String>,
    guard_level: Option<String>,
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
    tasks_req: Option<u64>,
    skills_req: Option<u64>,
    running_tasks: usize,
    todos: Vec<TodoEntry>,
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
            model: std::env::var("TWIG_MODEL")
                .ok()
                .or_else(Provider::saved_model)
                .unwrap_or_else(|| "magpie".into()),
            effort: Provider::saved_effort(),
            plan_mode: false,
            playbook: None,
            guard_level: None,
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
            tasks_req: None,
            skills_req: None,
            running_tasks: 0,
            todos: Vec::new(),
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
        self.tasks_req = None;
        self.skills_req = None;
        self.running_tasks = 0;
        self.todos.clear();
        self.pending_images.clear();
        self.transcript.clear();
        self.approvals.clear();
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
                if let Some(val) = result.get("effort") {
                    self.effort = val.as_str().map(str::to_owned);
                }
                if let Some(title) = result.get("title").and_then(Value::as_str) {
                    self.title = Some(title.to_owned());
                }
                if let Some(plan) = result.get("plan").and_then(Value::as_bool) {
                    self.plan_mode = plan;
                }
                if let Some(items) = result.get("todos").and_then(Value::as_array) {
                    self.todos = parse_todos(items);
                }
                if let Some(rt) = result.get("running_tasks").and_then(Value::as_u64) {
                    self.running_tasks = rt as usize;
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
                    self.git_stat_dirty = true;
                    if let Some(summary) = result.get("summary").and_then(Value::as_str) {
                        self.flash(summary);
                        self.transcript
                            .push_notice(NoticeKind::Success, format!("undo: {summary}"));
                    }
                }
                if self.rewind_req == Some(id) {
                    self.rewind_req = None;
                    let n = result["rewound_turns"].as_u64().unwrap_or(1);
                    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
                        self.transcript.clear();
                        self.transcript.replay_history(messages);
                    }
                    self.approvals.clear();
                    if let Some(prefill) = self.rewind_prefill.take() {
                        self.composer.set(&prefill);
                    }
                    self.flash(format!("Rewound {n} turn(s)"));
                    self.transcript.push_notice(
                        NoticeKind::Success,
                        format!("rewound conversation by {n} turn(s)"),
                    );
                }
                if self.mcp_req == Some(id) {
                    self.mcp_req = None;
                    let report = format_mcp_report(&result);
                    if report.starts_with("no MCP") {
                        self.transcript.push_notice(NoticeKind::Info, report);
                    } else {
                        self.transcript
                            .push_notice(NoticeKind::Playbook, "configured MCP servers");
                        self.transcript.push_assistant(report);
                    }
                }
                if self.tasks_req == Some(id) {
                    self.tasks_req = None;
                    if let Some(r) = result.get("running").and_then(Value::as_u64) {
                        self.running_tasks = r as usize;
                    }
                    let report = format_tasks_report(&result);
                    if report.starts_with("no background") {
                        self.transcript.push_notice(NoticeKind::Info, report);
                    } else {
                        self.transcript
                            .push_notice(NoticeKind::Playbook, "background tasks");
                        self.transcript.push_assistant(report);
                    }
                }
                if self.skills_req == Some(id) {
                    self.skills_req = None;
                    let action = result["action"].as_str().unwrap_or("list");
                    let ok = result["ok"] != false;
                    let report = result["report"].as_str().unwrap_or("").trim_end();
                    if matches!(action, "enable" | "disable") {
                        self.composer.refresh_skills(&self.repo_raw);
                        self.flash(report);
                        self.transcript
                            .push_notice(NoticeKind::Success, report.to_owned());
                    } else {
                        self.transcript.push_notice(
                            if ok { NoticeKind::Playbook } else { NoticeKind::Warn },
                            format!("skills · {action}"),
                        );
                        if !report.is_empty() {
                            self.transcript
                                .push_assistant(format!("```text\n{report}\n```"));
                        }
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
                        let _ = self.approvals.push_from_value(p);
                    }
                }
                if self.active_turn_req == Some(id) {
                    self.running = false;
                    self.run_started = None;
                    self.last_delta_at = None;
                    self.active_turn_req = None;
                    self.git_stat_dirty = true;
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
            LocalMsg::Shell { id, output, exit_code, elapsed } => {
                self.transcript.finish_tool(&id, Some(output), exit_code, Some(elapsed));
                self.transcript.follow_tail();
                self.git_stat_dirty = true;
            }
            LocalMsg::Review(Ok(text)) => {
                self.transcript.push_notice(NoticeKind::Success, "independent review completed");
                self.transcript.push_assistant(text);
            }
            LocalMsg::Review(Err(e)) => self.transcript.push_notice(NoticeKind::Error, format!("review: {e}")),
            LocalMsg::Doctor(Ok((output, ok))) => {
                self.transcript.push_notice(
                    if ok { NoticeKind::Success } else { NoticeKind::Warn },
                    if ok { "twig doctor: all checks passed" } else { "twig doctor reported issues" },
                );
                if !output.is_empty() {
                    self.transcript.push_assistant(format!("```text\n{output}\n```"));
                }
            }
            LocalMsg::Doctor(Err(e)) => self.transcript.push_notice(NoticeKind::Error, format!("doctor: {e}")),
            LocalMsg::Models(Ok(models)) if !models.is_empty() => {
                self.overlay_picker = Some(OverlayPicker::model(models, self.model.clone()));
            }
            LocalMsg::Models(Ok(_)) => self.transcript.push_notice(
                NoticeKind::Info,
                format!("active model: {} (magpie returned no model list; use `/model <name>` or `/model reset`)", self.model),
            ),
            LocalMsg::Models(Err(e)) => self.transcript.push_notice(
                NoticeKind::Warn,
                format!("active model: {} (could not fetch magpie models: {e}; use `/model <name>`)", self.model),
            ),
            LocalMsg::GitStat(stat) => self.git_stat = stat,
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
                if let Some((name, summary)) = self.approvals.push_from_value(ev) {
                    self.pending_notify = Some((
                        "twig · approval needed".into(),
                        format!("{name}: {summary}"),
                    ));
                }
            }
            Some("approval.resolved") => {
                let id = ev["id"].as_str().unwrap_or("");
                let allow = ev["allow"] == true;
                self.approvals.resolve_id(id);
                self.transcript.push_notice(
                    if allow { NoticeKind::Success } else { NoticeKind::Warn },
                    format!("approval {id}: {}", if allow { "approved" } else { "denied" }),
                );
            }
            Some("playbook.selected") => {
                if let Some(name) = ev["name"].as_str() {
                    self.playbook = Some(name.to_owned());
                    self.transcript.push_notice(NoticeKind::Playbook, format!("playbook routed · {name}"));
                }
            }
            Some("skill.loaded") => {
                let name = ev["name"].as_str().unwrap_or("skill");
                let scope = ev["scope"].as_str().unwrap_or("global");
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    format!("skill loaded · ${name} ({scope})"),
                );
            }
            Some("guard.tripped") => {
                if let Some(level) = ev["level"].as_str() {
                    self.guard_level = Some(level.to_owned());
                    self.transcript.push_notice(NoticeKind::Warn, format!("guard tripped · {level}"));
                }
            }
            Some("guard.repetition") => {
                let tool = ev["tool"].as_str().unwrap_or("tool");
                self.transcript.push_notice(NoticeKind::Warn, format!("loop-breaker: nudged agent after repeated failing `{tool}` call"));
            }
            Some("mode.changed") => {
                if let Some(plan) = ev["plan"].as_bool() {
                    self.plan_mode = plan;
                }
            }
            Some("todo.updated") => {
                if let Some(items) = ev["items"].as_array() {
                    self.todos = parse_todos(items);
                }
            }
            Some("policy.allowed") => {
                if let Some(pat) = ev["pattern"].as_str() {
                    self.transcript.push_notice(NoticeKind::Success, format!("session rule added: always allow `{pat}`"));
                }
            }
            Some("subagent.started") => {
                let role = ev["role"].as_str().unwrap_or("subagent");
                let task = ev["task"].as_str().unwrap_or("");
                let parallel = if ev["parallel"] == true { " parallel" } else { "" };
                self.update_status_header(format!("Subagent [{role}]"));
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    format!(
                        "subagent[{role}{parallel}] started · {}",
                        task.chars().take(64).collect::<String>()
                    ),
                );
            }
            Some("subagent.completed") => {
                let role = ev["role"].as_str().unwrap_or("subagent");
                let ok = ev["ok"] == true;
                let secs = ev["elapsed_secs"].as_u64().unwrap_or(0);
                self.transcript.push_notice(
                    if ok { NoticeKind::Success } else { NoticeKind::Warn },
                    format!(
                        "subagent[{role}] {} ({})",
                        if ok { "completed" } else { "failed" },
                        fmt_elapsed_compact(secs)
                    ),
                );
            }
            Some("task.started") => {
                let id = ev["id"].as_str().unwrap_or("bg");
                let cmd = ev["command"].as_str().unwrap_or("");
                self.running_tasks = ev["running"].as_u64().unwrap_or(1) as usize;
                self.transcript.push_notice(
                    NoticeKind::Info,
                    format!("task {id} started · {}", cmd.chars().take(60).collect::<String>()),
                );
            }
            Some("task.completed") => {
                let id = ev["id"].as_str().unwrap_or("bg");
                let status = ev["status"].as_str().unwrap_or("done");
                self.running_tasks = ev["running"].as_u64().unwrap_or(0) as usize;
                self.transcript.push_notice(
                    if status == "completed" { NoticeKind::Success } else { NoticeKind::Warn },
                    format!("task {id} {status}"),
                );
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
                    if passed { NoticeKind::Success } else { NoticeKind::Error },
                    if passed { "external check passed" } else { "external check failed — returning evidence to agent" },
                );
            }
            Some("verify.passed") => self.transcript.push_notice(NoticeKind::Success, "differential verification passed"),
            Some("verify.failed") => self.transcript.push_notice(NoticeKind::Error, "differential verification failed"),
            Some("papercut.filed") => {
                let comp = ev["report"]["component"].as_str().unwrap_or("harness");
                self.transcript.push_notice(NoticeKind::Info, format!("papercut filed · {comp}"));
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
                KeyCode::Char('v' | 'V') => {
                    self.attach_image(None);
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
                PickerOutcome::RewindTo { turns_back, prompt } => {
                    self.overlay_picker = None;
                    self.rewind_prefill = Some(prompt);
                    Action::Slash(SlashCommand::RewindSession(turns_back))
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
            };
        }

        if self.transcript.search_is_open() {
            self.transcript.search_handle_key(key);
            return Action::None;
        }

        match self.approvals.handle_key(key) {
            ApprovalKeyResult::Ignored => {}
            ApprovalKeyResult::Consumed => return Action::None,
            ApprovalKeyResult::Resolve {
                id,
                allow,
                remember,
            } => {
                return Action::ResolveApproval {
                    id,
                    allow,
                    remember,
                };
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
                self.transcript.scroll_by(-3);
                return Action::None;
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.transcript.scroll_by(3);
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
            MouseEventKind::ScrollUp => self.transcript.scroll_by(-4),
            MouseEventKind::ScrollDown => self.transcript.scroll_by(4),
            _ => {}
        }
    }

    fn attach_image(&mut self, opt_path: Option<&str>) {
        let loaded = match opt_path {
            Some(p) => load_image_data_url(&self.repo_raw, p).map(Some),
            None => paste_clipboard_image(),
        };
        match loaded {
            Ok(Some((label, url))) => {
                self.pending_images.push((label.clone(), url));
                self.flash(format!("Attached image: {label}"));
                self.transcript.push_notice(
                    NoticeKind::Success,
                    format!("attached image `{label}` (will send with next message)"),
                );
            }
            Ok(None) => self.transcript.push_notice(
                NoticeKind::Info,
                "no image in clipboard — pass `/image path/to/file.png` or `@file.png`",
            ),
            Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
        }
    }

    fn open_rewind_picker(&mut self) {
        let turns = self.transcript.user_turns();
        if turns.is_empty() {
            self.transcript
                .push_notice(NoticeKind::Info, "no user turns in this session to rewind");
        } else {
            self.overlay_picker = Some(OverlayPicker::rewind(turns));
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

        if let Some(cmd) = parse_slash(trimmed, self.plan_mode) {
            return match cmd {
                SlashCommand::Quit => Action::Quit,
                SlashCommand::SetPlan(v) => Action::SetPlan(v),
                SlashCommand::SetEffort(level) => Action::SetEffort(level),
                SlashCommand::SetModel(m) => Action::SetModel(m),
                SlashCommand::SwitchSession(sid) => Action::SwitchSession(sid),
                SlashCommand::Cancel => Action::Cancel,
                SlashCommand::OpenEditor => Action::OpenEditor,
                SlashCommand::ToggleMouse => Action::ToggleMouse,
                SlashCommand::Steer(text) => {
                    self.transcript.push_steering(text.clone());
                    Action::SendSteer(text)
                }
                SlashCommand::ResolveApproval { id, allow } => Action::ResolveApproval {
                    id,
                    allow,
                    remember: false,
                },
                SlashCommand::OpenEffortPicker => {
                    self.overlay_picker = Some(OverlayPicker::effort(self.effort.clone()));
                    Action::None
                }
                SlashCommand::OpenResume => {
                    let sessions = list_sessions();
                    if sessions.is_empty() {
                        self.transcript
                            .push_notice(NoticeKind::Info, "no saved sessions found");
                    } else {
                        self.overlay_picker = Some(OverlayPicker::resume(sessions));
                    }
                    Action::None
                }
                SlashCommand::OpenRewindPicker => {
                    self.open_rewind_picker();
                    Action::None
                }
                SlashCommand::AttachImage(opt_path) => {
                    self.attach_image(opt_path.as_deref());
                    Action::None
                }
                SlashCommand::EvolveSession => {
                    if self.running {
                        self.flash("Wait for the active turn to finish before running /evolve");
                        Action::None
                    } else {
                        self.transcript.push_notice(
                            NoticeKind::Playbook,
                            "evolve: requesting human-reviewed .twig/preferences or skill proposal…",
                        );
                        self.running = true;
                        self.run_started = Some(Instant::now());
                        self.update_status_header(String::from("Reflecting"));
                        Action::SendTurn(EVOLVE_PROMPT.to_owned())
                    }
                }
                SlashCommand::ClearImages => {
                    self.pending_images.clear();
                    self.flash("Cleared pending images");
                    Action::None
                }
                SlashCommand::ShowMemory => {
                    match read_memory_preview(&self.repo_raw) {
                        Ok(Some(body)) if !body.trim().is_empty() => {
                            self.transcript.push_notice(NoticeKind::Info, "project memory (.twig/preferences.md)");
                            self.transcript.push_assistant(body);
                        }
                        Ok(Some(_)) => self.transcript.push_notice(NoticeKind::Info, ".twig/preferences.md is empty"),
                        Ok(None) => self.transcript.push_notice(NoticeKind::Info, "no .twig/preferences.md yet — run /memory or /init"),
                        Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                    }
                    Action::None
                }
                SlashCommand::ShowStatus => {
                    let msg = format_status_notice(&self.footer_view(None, ""), self.turns_used, self.context_bytes, self.context_max);
                    self.transcript.push_notice(NoticeKind::Info, msg);
                    Action::None
                }
                SlashCommand::ShowConfig => {
                    match format_config_report() {
                        Ok(msg) => self.transcript.push_notice(NoticeKind::Info, msg),
                        Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                    }
                    Action::None
                }
                SlashCommand::ToggleExpand => { self.transcript.toggle_expand(); Action::None }
                SlashCommand::OpenFind(q) => { self.transcript.search_open_with(&q); Action::None }
                SlashCommand::CopyCode => match self.transcript.copy_latest_code_block() {
                    Ok((text, msg)) => { self.flash(msg); Action::CopyCode(text) }
                    Err(e) => { self.flash(e); Action::None }
                },
                SlashCommand::ClearTranscript => { self.transcript.clear(); Action::None }
                SlashCommand::ShowHelp => { self.set_footer_mode(FooterMode::ShortcutOverlay); Action::None }
                other => Action::Slash(other),
            };
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
        let search_open = self.transcript.search_is_open();
        let todo_h: u16 = if !has_approval && !has_picker && area.height > 14 {
            todo_strip_height(&self.todos)
        } else {
            0
        };
        let activity_h: u16 = if self.running && !has_approval && !has_picker {
            1
        } else {
            0
        };
        let search_h: u16 = if search_open { 1 } else { 0 };
        let bottom_panel_h: u16 = if let Some(picker) = &self.overlay_picker {
            picker.desired_height()
        } else if has_approval {
            8
        } else {
            self.composer.desired_height(area.width as usize)
        };
        let footer_h = self.footer_height();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(todo_h),
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

        if todo_h > 0 {
            render_todo_strip(frame, chunks[1], &self.todos);
        }

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
            frame.render_widget(Paragraph::new(Line::from(spans)), chunks[2]);
        }

        if search_open {
            self.transcript.render_search_bar(frame, chunks[3]);
        }

        if let Some(picker) = &self.overlay_picker {
            picker.render(frame, chunks[4]);
        } else if has_approval {
            self.approvals.render(frame, chunks[4]);
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
                    "Ask twig anything… (/ command · @ file · $ skill · ! shell)",
                )
            };
            self.composer.render(
                frame,
                chunks[4],
                rule_color,
                prompt_color,
                placeholder,
                !search_open,
            );
            if !search_open {
                self.composer
                    .render_popup(frame, chunks[4], chunks[0].height);
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
            "↑/↓ select · Enter confirm · a always · Esc deny  "
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
        render_footer(frame, chunks[5], &self.footer_view(flash, right_hint));
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
                let imgs = std::mem::take(&mut self.pending_images);
                let (params, labels) = build_turn_params(&self.repo_raw, text, imgs);
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
            Action::ResolveApproval { id, allow, remember } => {
                self.send_rpc(
                    client,
                    "approval.resolve",
                    json!({"id": id, "allow": allow, "remember": remember}),
                    "",
                );
            }
            Action::SetPlan(next) => {
                if self.send_rpc(client, "mode.set", json!({"plan": next}), "").is_some() {
                    self.plan_mode = next;
                    let msg = if next {
                        "Plan mode ON (read-only · Shift+Tab to exit)"
                    } else {
                        "Plan mode OFF (full edit access)"
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

    fn handle_slash_io(
        &mut self,
        cmd: SlashCommand,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        client: &mut ServeClient,
        workers: &LocalWorkers,
    ) -> Result<(), String> {
        match cmd {
            SlashCommand::NewSession => {
                if self.replace_client(client, None) {
                    self.flash("Started new session");
                }
            }
            SlashCommand::ForkSession => {
                self.fork_req = self.send_rpc(client, "session.fork", json!({}), "Forking session…");
            }
            SlashCommand::RewindSession(turns) => {
                self.rewind_req = self.send_rpc(client, "session.rewind", json!({"turns": turns}), &format!("Rewinding {turns} turn(s)…"));
            }
            SlashCommand::RenameSession(title) => {
                if self.send_rpc(client, "session.rename", json!({"title": title}), &format!("Renamed session → {title}")).is_some() {
                    self.title = Some(title);
                }
            }
            SlashCommand::ArchiveSession => {
                let old_sid = self.sid.clone();
                if self.replace_client(client, None) {
                    match archive_session_file(&old_sid) {
                        Ok(dst) => {
                            let short: String = old_sid.chars().take(10).collect();
                            self.flash(format!("Archived {short}"));
                            self.transcript.push_notice(NoticeKind::Success, format!("archived session → {dst}"));
                        }
                        Err(e) => self.transcript.push_notice(NoticeKind::Warn, e),
                    }
                }
            }
            SlashCommand::OpenModelPicker => {
                self.flash("Fetching models from magpie…");
                workers.spawn_models();
            }
            SlashCommand::UndoEdit => {
                self.undo_req = self.send_rpc(client, "edit.undo", json!({}), "Reverting last edit…");
            }
            SlashCommand::ListTasks => {
                self.tasks_req = self.send_rpc(client, "tasks.list", json!({}), "Listing background tasks…");
            }
            SlashCommand::ManageSkills { action, name, project } => {
                self.skills_req = self.send_rpc(
                    client,
                    "skill.manage",
                    json!({"action": action, "name": name, "project": project}),
                    &format!("Skills: {action}…"),
                );
            }
            SlashCommand::ListMcp => {
                self.mcp_req = self.send_rpc(client, "mcp.list", json!({}), "Checking MCP servers…");
            }
            SlashCommand::CompactNow => {
                self.send_rpc(client, "session.compact", json!({}), "Compacting context…");
            }
            SlashCommand::VerifySession => {
                self.verify_req = self.send_rpc(client, "session.verify", json!({}), "Verifying session hash chain…");
            }
            SlashCommand::EditMemory => match ensure_memory_file(&self.repo_raw) {
                Ok((path, created)) => {
                    TerminalGuard::suspend();
                    let edited = edit_file_in_external_editor(&path);
                    TerminalGuard::resume(self.mouse_enabled)?;
                    terminal.clear().map_err(|e| e.to_string())?;
                    match edited {
                        Ok(lines) => {
                            let verb = if created { "Created" } else { "Saved" };
                            let msg = format!("{verb} .twig/preferences.md ({lines} lines) — auto-loaded on next turn");
                            self.flash(&msg);
                            self.transcript.push_notice(NoticeKind::Success, msg);
                        }
                        Err(e) => self.flash(format!("Editor failed: {e}")),
                    }
                }
                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
            },
            SlashCommand::InitProject => match init_project_files(&self.repo_raw) {
                Ok((created, existing)) => {
                    self.flash("Initialized .twig/");
                    self.transcript.push_notice(NoticeKind::Success, format_init_report(&created, &existing));
                }
                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
            },
            SlashCommand::RunDoctor => {
                self.flash("Running twig doctor…");
                workers.spawn_doctor(self.repo_raw.clone());
            }
            SlashCommand::ExportTranscript(custom) => {
                let md = self.transcript.export_markdown(&self.sid, self.title.as_deref(), &self.model, &self.repo_path);
                match write_export_file(&self.repo_raw, &self.sid, custom.as_deref(), &md) {
                    Ok(pretty) => {
                        self.flash("Exported transcript to Markdown");
                        self.transcript.push_notice(NoticeKind::Success, format!("exported transcript ({} bytes) → {pretty}", md.len()));
                    }
                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                }
            }
            SlashCommand::RunDiff => match run_git_diff(&self.repo_raw) {
                Ok(diff) if diff.trim().is_empty() => {
                    self.transcript.push_notice(NoticeKind::Info, "working tree clean — no diff");
                }
                Ok(diff) => {
                    let id = format!("diff-{}", self.next_local_id);
                    self.next_local_id += 1;
                    self.transcript.push_tool_snapshot(id, "diff".into(), "git diff".into(), "local".into(), diff, Some(0));
                }
                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
            },
            SlashCommand::RunReview => {
                self.transcript.push_notice(NoticeKind::Playbook, "starting independent read-only review on current git diff…");
                self.transcript.follow_tail();
                workers.spawn_review(self.repo_raw.clone());
            }
            _ => {}
        }
        Ok(())
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
                remember: false
            }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: true,
                remember: true
            }
        );
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::ResolveApproval {
                id: "call_1".into(),
                allow: false,
                remember: false
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
        let welcome = render_frame_text(&mut app, 100, 28);
        assert!(welcome.contains("✻ twig") && !welcome.contains("╭─"));

        assert_eq!(
            app.handle_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::SetPlan(true)
        );
        app.plan_mode = true;
        assert_eq!(app.dispatch_input("/effort high"), Action::SetEffort("high".into()));
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
        app.apply_local_msg(LocalMsg::GitStat(Some((42, 8))));
        let active_frame = render_frame_text(&mut app, 130, 28);
        assert!(active_frame.contains("◈ plan"));
        assert!(active_frame.contains("+42") && active_frame.contains("-8"));
        assert!(active_frame.contains("⚙ 1 task"));
        assert!(active_frame.contains("high"));
        assert!(active_frame.contains("Plan progress  1/2"));
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
            Action::Slash(SlashCommand::RewindSession(1))
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
}
