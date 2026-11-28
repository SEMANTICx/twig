//! TUI message ingestion, event rendering updates, and slash-command execution
//! for `App` (split out to keep `tui/mod.rs` lean and cohesive).

use std::io;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::{json, Value};

use super::client::{
    edit_file_in_external_editor, ensure_memory_file, format_config_report, format_init_report,
    format_mcp_report, format_tasks_report, format_verify_report, init_project_files,
    load_image_data_url, paste_clipboard_image, read_memory_preview, run_git_diff,
    write_export_file, LocalMsg, LocalWorkers, ServeClient, ServerMsg, TerminalGuard,
    EVOLVE_PROMPT,
};
use super::composer::{parse_slash, SlashCommand};
use super::markdown::{format_status_notice, FooterMode};
use super::overlays::{summarize_tool_args, title_tool_name, OverlayPicker};
use super::strips::{parse_todos, BtwCard, SubagentEntry, SubagentStatus, TodoEntry};
use super::transcript::NoticeKind;
use crate::llm::Llm;
use super::{Action, App};

impl App {
    pub(super) fn apply_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Event(ev) => self.apply_event(&ev),
            ServerMsg::Reply { id, result } => {
                if let Some(sid) = result.get("sid").and_then(Value::as_str) {
                    self.sid = sid.to_owned();
                }
                if let Some(model) = result.get("model").and_then(Value::as_str) {
                    self.model = model.to_owned();
                }
                if result.get("env_override") == Some(&Value::Bool(true)) {
                    self.transcript.push_notice(
                        NoticeKind::Warn,
                        "model saved for this session; note: TWIG_MODEL/TWIG_ROLES in shell env overrides saved model on restart",
                    );
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
                if let Some(ge) = result.get("gate_edits").and_then(Value::as_bool) {
                    self.gate_edits = ge;
                }
                if let Some(op) = result.get("operator").and_then(Value::as_bool) {
                    self.operator_mode = op;
                }
                if let Some(st) = result.get("study").and_then(Value::as_bool) {
                    self.study_mode = st;
                }
                if let Some(items) = result.get("todos").and_then(Value::as_array) {
                    self.todos = parse_todos(items);
                    self.todos_done_at = (!self.todos.is_empty()
                        && self.todos.iter().all(TodoEntry::is_completed))
                    .then(Instant::now);
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
                if let Some(env) = result.get("env_summary").and_then(Value::as_str) {
                    self.env_summary = env.to_owned();
                }
                if self.history_req == Some(id) {
                    self.history_req = None;
                    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
                        self.transcript.replay_history(messages);
                    }
                    if let Some(children) = result.get("child_threads").and_then(Value::as_array) {
                        self.subagents.clear();
                        self.subagent_transcripts.clear();
                        for child in children {
                            let Some(csid) = child["sid"].as_str().filter(|s| !s.is_empty()) else {
                                continue;
                            };
                            let role = child["role"].as_str().unwrap_or("scout").to_owned();
                            let model = child["model"].as_str().map(str::to_owned);
                            let task = child["task"].as_str().unwrap_or("").to_owned();
                            let turns = child["turns"].as_u64().unwrap_or(0) as u32;
                            let tool_count = child["tool_count"].as_u64().unwrap_or(0) as u32;
                            let mut tr = super::transcript::Transcript::new();
                            if let Some(msgs) = child.get("messages").and_then(Value::as_array) {
                                tr.replay_history(msgs);
                            } else if !task.is_empty() {
                                tr.push_user(task.clone());
                            }
                            self.subagent_transcripts.insert(csid.to_owned(), tr);
                            self.subagents.push(SubagentEntry {
                                id: csid.to_owned(),
                                child_sid: Some(csid.to_owned()),
                                role,
                                model,
                                task,
                                parallel: false,
                                active_turn: false,
                                started_at: Instant::now(),
                                elapsed_secs: Some(0),
                                status: SubagentStatus::Completed,
                                error: None,
                                activity: None,
                                turns,
                                tool_count,
                                trace: Vec::new(),
                            });
                        }
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
                    self.git_stat_dirty = true;
                    let n = result["rewound_turns"].as_u64().unwrap_or(1);
                    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
                        self.transcript.clear();
                        self.transcript.replay_history(messages);
                    }
                    self.approvals.clear();
                    if let Some(prefill) = self.rewind_prefill.take() {
                        self.composer.set(&prefill);
                    }
                    let extra = result["code_restored"]
                        .as_str()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default();
                    self.flash(format!("Rewound {n} turn(s)"));
                    self.transcript.push_notice(
                        NoticeKind::Success,
                        format!("rewound conversation by {n} turn(s){extra}"),
                    );
                }
                if self.mcp_req == Some(id) {
                    self.mcp_req = None;
                    if let Some(notice) = result.get("notice").and_then(Value::as_str) {
                        self.flash(notice);
                        self.transcript
                            .push_notice(NoticeKind::Success, notice.to_owned());
                    }
                    let report = format_mcp_report(&result);
                    if report.starts_with("no MCP") {
                        self.transcript.push_notice(NoticeKind::Info, report);
                    } else {
                        self.transcript
                            .push_notice(NoticeKind::Playbook, "configured MCP servers");
                        self.transcript.push_assistant(report);
                    }
                }
                if self.blackboard_req == Some(id) {
                    self.blackboard_req = None;
                    if let Some(cleared) = result.get("cleared").and_then(Value::as_u64) {
                        let msg = format!("Cleared {cleared} fact(s) from RCI blackboard");
                        self.flash(&msg);
                        self.transcript.push_notice(NoticeKind::Info, msg);
                    } else {
                        let facts = result["facts"].as_array().cloned().unwrap_or_default();
                        if facts.is_empty() {
                            self.transcript.push_notice(
                                NoticeKind::Info,
                                "RCI split-fact blackboard is empty (parallel subagents populate `path:line` facts automatically)",
                            );
                        } else {
                            let body: Vec<String> = facts
                                .iter()
                                .filter_map(Value::as_str)
                                .map(|f| format!("- {f}"))
                                .collect();
                            self.transcript.push_notice(
                                NoticeKind::Playbook,
                                format!("RCI split-fact blackboard ({} facts)", body.len()),
                            );
                            self.transcript.push_assistant(body.join("\n"));
                        }
                    }
                }
                if self.tasks_req == Some(id) {
                    self.tasks_req = None;
                    if let Some(r) = result.get("running").and_then(Value::as_u64) {
                        self.running_tasks = r as usize;
                    }
                    if let Some(arr) = result.get("tasks").and_then(Value::as_array) {
                        self.cached_tasks = arr.clone();
                    }
                    if self.open_tasks_drawer {
                        self.open_tasks_drawer = false;
                        self.overlay_picker = Some(OverlayPicker::tasks(
                            self.subagents.clone(),
                            self.cached_tasks.clone(),
                        ));
                    } else {
                        let report = format_tasks_report(&result);
                        if report.starts_with("no background") {
                            self.transcript.push_notice(NoticeKind::Info, report);
                        } else {
                            self.transcript
                                .push_notice(NoticeKind::Playbook, "background tasks");
                            self.transcript.push_assistant(report);
                        }
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
                            if ok {
                                NoticeKind::Playbook
                            } else {
                                NoticeKind::Warn
                            },
                            format!("skills · {action}"),
                        );
                        if !report.is_empty() {
                            self.transcript
                                .push_assistant(format!("```text\n{report}\n```"));
                        }
                    }
                }
                if self.roles_req == Some(id) {
                    self.roles_req = None;
                    let report = result["report"].as_str().unwrap_or("").trim_end();
                    self.transcript
                        .push_notice(NoticeKind::Playbook, "multi-model role routing");
                    if !report.is_empty() {
                        self.transcript
                            .push_assistant(format!("```text\n{report}\n```"));
                    }
                }
                if self.btw_req == Some(id) {
                    self.btw_req = None;
                    let q = result["question"].as_str().unwrap_or("btw").to_owned();
                    let ans = result["answer"].as_str().unwrap_or("").to_owned();
                    self.absorb_usage(&result["usage"]);
                    self.btw_card = Some(BtwCard {
                        question: q,
                        answer: Some(ans),
                    });
                }
                if self.apply_req == Some(id) {
                    self.apply_req = None;
                    self.git_stat_dirty = true;
                    let sum = result["summary"].as_str().unwrap_or("patch applied");
                    self.flash(sum);
                    self.transcript.push_notice(
                        NoticeKind::Success,
                        format!("{sum} (reversible via `/undo`)"),
                    );
                }
                if self.commit_req == Some(id) {
                    self.commit_req = None;
                    self.git_stat_dirty = true;
                    let sum = result["summary"].as_str().unwrap_or("committed changes");
                    let full_msg = result["message"].as_str().unwrap_or("");
                    self.flash(sum);
                    self.transcript.push_notice(NoticeKind::Success, sum.to_owned());
                    if full_msg.contains('\n') {
                        self.transcript
                            .push_assistant(format!("```text\n{full_msg}\n```"));
                    }
                }
                if self.pr_req == Some(id) {
                    self.pr_req = None;
                    self.git_stat_dirty = true;
                    let sum = result["summary"].as_str().unwrap_or("pull request ready");
                    self.flash(sum);
                    self.transcript.push_notice(NoticeKind::Success, sum.to_owned());
                }
                if self.worktree_req == Some(id) {
                    self.worktree_req = None;
                    if result["action"] == "switched" {
                        if let Some(wt) = result["worktree"].as_str() {
                            self.repo_raw = wt.to_owned();
                            self.repo_path = super::client::shorten_home(wt);
                            self.repo_label = std::path::Path::new(wt)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| wt.to_owned());
                            self.git_branch = result["branch"]
                                .as_str()
                                .map(str::to_owned)
                                .or_else(|| super::client::detect_git_branch(wt));
                            self.composer = super::composer::Composer::new(wt);
                            self.git_stat_dirty = true;
                        }
                        let sum = result["summary"].as_str().unwrap_or("switched worktree");
                        self.flash(sum);
                        self.transcript.push_notice(NoticeKind::Success, sum.to_owned());
                    } else if let Some(rep) = result["report"].as_str() {
                        self.transcript
                            .push_notice(NoticeKind::Playbook, "git worktrees");
                        self.transcript
                            .push_assistant(format!("```text\n{rep}\n```"));
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
                        if reason == "final" && self.approvals.is_empty() {
                            if !self.todos_updated_in_turn && !self.todos.is_empty() {
                                self.todos.clear();
                                self.todos_done_at = None;
                            }
                            if let Some(next_q) = self.queued_turns.pop_front() {
                                self.auto_send_next = Some(next_q);
                            }
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

    pub(super) fn apply_local_msg(&mut self, msg: LocalMsg) {
        match msg {
            LocalMsg::Shell {
                id,
                output,
                exit_code,
                elapsed,
            } => {
                let cmd = self
                    .last_shell_cmd
                    .clone()
                    .unwrap_or_else(|| "shell".into());
                self.last_shell = Some((cmd, output.clone(), exit_code));
                self.transcript
                    .finish_tool(&id, Some(output), exit_code, Some(elapsed));
                self.git_stat_dirty = true;
            }
            LocalMsg::Review(Ok(text)) => {
                self.transcript
                    .push_notice(NoticeKind::Success, "independent review completed");
                self.transcript.push_assistant(text);
            }
            LocalMsg::Review(Err(e)) => self
                .transcript
                .push_notice(NoticeKind::Error, format!("review: {e}")),
            LocalMsg::Walkthrough(Ok(text)) => {
                self.transcript
                    .push_notice(NoticeKind::Success, "change walkthrough ready");
                self.transcript.push_assistant(text);
            }
            LocalMsg::Walkthrough(Err(e)) => self
                .transcript
                .push_notice(NoticeKind::Error, format!("walkthrough: {e}")),
            LocalMsg::DualPlan(Ok(text)) => {
                self.transcript
                    .push_notice(NoticeKind::Success, "blind dual-model plan synthesized");
                self.transcript.push_assistant(text);
            }
            LocalMsg::DualPlan(Err(e)) => self
                .transcript
                .push_notice(NoticeKind::Error, format!("dual-plan: {e}")),
            LocalMsg::Doctor(Ok((output, ok))) => {
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
            LocalMsg::Doctor(Err(e)) => self
                .transcript
                .push_notice(NoticeKind::Error, format!("doctor: {e}")),
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
            LocalMsg::SubagentEvent(ev) => self.apply_event(&ev),
        }
    }

    pub(super) fn apply_event(&mut self, ev: &Value) {
        match ev["ev"].as_str() {
            Some("turn.started") => {
                self.running = true;
                if self.run_started.is_none() {
                    self.run_started = Some(Instant::now());
                    self.guard_level = None;
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
                    format!(
                        "model transient error — retrying (attempt {attempt}, {secs}s backoff)…"
                    ),
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
                    let changed = self.guard_level.as_deref() != Some(level);
                    self.guard_level = Some(level.to_owned());
                    if changed {
                        self.transcript
                            .push_notice(NoticeKind::Warn, format!("guard tripped · {level}"));
                    }
                }
            }
            Some("guard.repetition") => {
                let tool = ev["tool"].as_str().unwrap_or("tool");
                self.transcript.push_notice(
                    NoticeKind::Warn,
                    format!("loop-breaker: nudged agent after repeated failing `{tool}` call"),
                );
            }
            Some("mode.changed") => {
                if let Some(plan) = ev["plan"].as_bool() {
                    self.plan_mode = plan;
                }
                if let Some(ge) = ev["gate_edits"].as_bool() {
                    self.gate_edits = ge;
                }
            }
            Some("operator.changed") => {
                if let Some(op) = ev["enabled"].as_bool() {
                    self.operator_mode = op;
                }
            }
            Some("study.changed") => {
                if let Some(st) = ev["enabled"].as_bool() {
                    self.study_mode = st;
                }
            }
            Some("todo.updated") => {
                if let Some(items) = ev["items"].as_array() {
                    self.todos = parse_todos(items);
                    self.todos_updated_in_turn = true;
                    self.todos_done_at = (!self.todos.is_empty()
                        && self.todos.iter().all(TodoEntry::is_completed))
                    .then(Instant::now);
                }
            }
            Some("policy.allowed") => {
                if let Some(pat) = ev["pattern"].as_str() {
                    self.transcript.push_notice(
                        NoticeKind::Success,
                        format!("session rule added: always allow `{pat}`"),
                    );
                }
            }
            Some("subagent.started") => {
                let raw_id = ev["id"].as_str().unwrap_or("");
                let role = ev["role"].as_str().unwrap_or("subagent").to_owned();
                let model = ev["model"].as_str().map(str::to_owned);
                let child_sid = ev["sid"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let id = if raw_id.is_empty() {
                    format!("{role}-{}", self.subagents.len() + 1)
                } else {
                    raw_id.to_owned()
                };
                let task = ev["task"].as_str().unwrap_or("").trim().to_owned();
                let parallel = ev["parallel"] == true;
                self.update_status_header(format!("Subagent [{role}]"));
                let mut child_tr = super::transcript::Transcript::new();
                if !task.is_empty() {
                    child_tr.push_user(task.clone());
                }
                self.subagent_transcripts.insert(id.clone(), child_tr);
                self.subagents.push(SubagentEntry {
                    id,
                    child_sid,
                    role,
                    model,
                    task,
                    parallel,
                    active_turn: true,
                    started_at: Instant::now(),
                    elapsed_secs: None,
                    status: SubagentStatus::Running,
                    error: None,
                    activity: None,
                    turns: 0,
                    tool_count: 0,
                    trace: Vec::new(),
                });
                if self.subagents.len() > 16 {
                    let drop_n = self.subagents.len() - 16;
                    for dropped in self.subagents.drain(..drop_n) {
                        if self.viewing_subagent.as_deref() != Some(dropped.id.as_str()) {
                            self.subagent_transcripts.remove(&dropped.id);
                        }
                    }
                }
                if let Some(OverlayPicker::Tasks { subagents, .. }) = &mut self.overlay_picker {
                    *subagents = self.subagents.clone();
                }
            }
            Some("subagent.progress") => {
                let id = ev["id"].as_str().unwrap_or("");
                let role = ev["role"].as_str().unwrap_or("subagent");
                let child_sid = ev["sid"].as_str().filter(|s| !s.is_empty());
                let activity = ev["activity"].as_str().unwrap_or("").trim();
                let turns = ev["turns"].as_u64().unwrap_or(0) as u32;
                let tool_count = ev["tool_count"].as_u64().unwrap_or(0) as u32;
                let match_idx = (!id.is_empty())
                    .then(|| {
                        self.subagents
                            .iter()
                            .position(|s| s.status == SubagentStatus::Running && s.id == id)
                    })
                    .flatten()
                    .or_else(|| {
                        self.subagents
                            .iter()
                            .position(|s| s.status == SubagentStatus::Running && s.role == role)
                    });
                if let Some(idx) = match_idx {
                    let entry = &mut self.subagents[idx];
                    if let Some(csid) = child_sid {
                        entry.child_sid = Some(csid.to_owned());
                    }
                    if turns > entry.turns {
                        entry.turns = turns;
                    }
                    if tool_count > entry.tool_count {
                        entry.tool_count = tool_count;
                    }
                    if !activity.is_empty() {
                        entry.activity = Some(activity.to_owned());
                        if !activity.starts_with("thinking")
                            && !activity.starts_with("starting")
                            && entry.trace.last().map(String::as_str) != Some(activity)
                        {
                            entry.trace.push(activity.to_owned());
                            if entry.trace.len() > 6 {
                                entry.trace.remove(0);
                            }
                        }
                    }
                    if let Some(OverlayPicker::Tasks { subagents, .. }) = &mut self.overlay_picker {
                        *subagents = self.subagents.clone();
                    }
                }
            }
            Some("subagent.stream") => {
                let id = ev["id"].as_str().unwrap_or("");
                let role = ev["role"].as_str().unwrap_or("subagent");
                let key = if !id.is_empty() && self.subagent_transcripts.contains_key(id) {
                    Some(id.to_owned())
                } else {
                    self.subagents
                        .iter()
                        .rev()
                        .find(|s| s.status == SubagentStatus::Running && s.role == role)
                        .map(|s| s.id.clone())
                };
                if let Some(k) = key {
                    let child = &ev["child"];
                    if let Some(tr) = self.subagent_transcripts.get_mut(&k) {
                        apply_child_stream_event(tr, child);
                    }
                }
            }
            Some("subagent.completed") => {
                let id = ev["id"].as_str().unwrap_or("");
                let role = ev["role"].as_str().unwrap_or("subagent");
                let child_sid = ev["sid"].as_str().filter(|s| !s.is_empty());
                let model = ev["model"].as_str().map(str::to_owned);
                let has_patch = ev["patch_artifact"].as_str().is_some();
                let ok = ev["ok"] == true;
                let secs = ev["elapsed_secs"].as_u64().unwrap_or(0);
                let err_note = ev["error"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let next_status = if ok {
                    SubagentStatus::Completed
                } else {
                    SubagentStatus::Failed
                };
                let match_idx = (!id.is_empty())
                    .then(|| {
                        self.subagents
                            .iter()
                            .position(|s| s.status == SubagentStatus::Running && s.id == id)
                    })
                    .flatten()
                    .or_else(|| {
                        self.subagents
                            .iter()
                            .position(|s| s.status == SubagentStatus::Running && s.role == role)
                    })
                    .or_else(|| {
                        self.subagents
                            .iter()
                            .position(|s| s.status == SubagentStatus::Running)
                    });
                let (done_id, resolved_sid) = if let Some(idx) = match_idx {
                    if let Some(csid) = child_sid {
                        self.subagents[idx].child_sid = Some(csid.to_owned());
                    }
                    if self.subagents[idx].model.is_none() {
                        self.subagents[idx].model = model;
                    }
                    self.subagents[idx].status = next_status;
                    self.subagents[idx].elapsed_secs = Some(secs);
                    self.subagents[idx].error = err_note.clone();
                    self.subagents[idx].activity = None;
                    (
                        self.subagents[idx].id.clone(),
                        self.subagents[idx].child_sid.clone(),
                    )
                } else {
                    let fallback_id = if id.is_empty() {
                        format!("{role}-{}", self.subagents.len() + 1)
                    } else {
                        id.to_owned()
                    };
                    let csid = child_sid.map(str::to_owned);
                    self.subagents.push(SubagentEntry {
                        id: fallback_id.clone(),
                        child_sid: csid.clone(),
                        role: role.to_owned(),
                        model,
                        task: String::new(),
                        parallel: false,
                        active_turn: true,
                        started_at: Instant::now(),
                        elapsed_secs: Some(secs),
                        status: next_status,
                        error: err_note.clone(),
                        activity: None,
                        turns: 0,
                        tool_count: 0,
                        trace: Vec::new(),
                    });
                    (fallback_id, csid)
                };
                if has_patch {
                    self.git_stat_dirty = true;
                }
                if let Some(tr) = self.subagent_transcripts.get_mut(&done_id) {
                    if ok {
                        let apply_hint = match (has_patch, resolved_sid.as_deref()) {
                            (true, Some(csid)) => format!(" · patch ready: `/apply {csid}`"),
                            _ => String::new(),
                        };
                        tr.push_notice(
                            NoticeKind::Success,
                            format!(
                                "subagent [{role}] completed in {secs}s{apply_hint} (Enter reply · Esc ← main)"
                            ),
                        );
                    } else {
                        tr.push_notice(
                            NoticeKind::Error,
                            format!(
                                "subagent [{role}] failed: {}",
                                err_note.as_deref().unwrap_or("error")
                            ),
                        );
                    }
                }
                if let Some(OverlayPicker::Tasks { subagents, .. }) = &mut self.overlay_picker {
                    *subagents = self.subagents.clone();
                }
            }
            Some("task.started") => {
                let id = ev["id"].as_str().unwrap_or("bg");
                let cmd = ev["command"].as_str().unwrap_or("");
                self.running_tasks = ev["running"].as_u64().unwrap_or(1) as usize;
                self.transcript.push_notice(
                    NoticeKind::Info,
                    format!(
                        "task {id} started · {}",
                        cmd.chars().take(60).collect::<String>()
                    ),
                );
            }
            Some("task.completed") => {
                let id = ev["id"].as_str().unwrap_or("bg");
                let status = ev["status"].as_str().unwrap_or("done");
                self.running_tasks = ev["running"].as_u64().unwrap_or(0) as usize;
                self.transcript.push_notice(
                    if status == "completed" {
                        NoticeKind::Success
                    } else {
                        NoticeKind::Warn
                    },
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
            Some("verify.passed") => self
                .transcript
                .push_notice(NoticeKind::Success, "differential verification passed"),
            Some("verify.failed") => self
                .transcript
                .push_notice(NoticeKind::Error, "differential verification failed"),
            Some("hook.blocked") => {
                let cmd = ev["command"].as_str().unwrap_or("stop_hook");
                self.transcript.push_notice(
                    NoticeKind::Warn,
                    format!("stop_hook `{cmd}` returned exit 2 — returning diagnostics to agent"),
                );
            }
            Some("hook.warn") => {
                let cmd = ev["command"].as_str().unwrap_or("stop_hook");
                let code = ev["exit_code"].as_i64().unwrap_or(1);
                self.transcript.push_notice(
                    NoticeKind::Info,
                    format!("stop_hook `{cmd}` exited with {code} (non-blocking; exit 2 blocks)"),
                );
            }
            Some("papercut.filed") => {
                let comp = ev["report"]["component"].as_str().unwrap_or("harness");
                self.transcript
                    .push_notice(NoticeKind::Info, format!("papercut filed · {comp}"));
            }
            _ => {}
        }
    }

    pub(super) fn attach_image(&mut self, opt_path: Option<&str>) {
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

    pub(super) fn open_rewind_picker(&mut self) {
        let turns = self.transcript.user_turns();
        if turns.is_empty() {
            self.transcript
                .push_notice(NoticeKind::Info, "no user turns in this session to rewind");
        } else {
            self.overlay_picker = Some(OverlayPicker::rewind(turns));
        }
    }

    pub(super) fn open_resume_picker(&mut self, all_repos: bool) {
        let sessions = if all_repos {
            crate::runtime::session::scan_sessions(None, 25)
        } else {
            let scoped =
                crate::runtime::session::scan_sessions(Some(std::path::Path::new(&self.repo_raw)), 25);
            if scoped.is_empty() {
                let global = crate::runtime::session::scan_sessions(None, 25);
                if !global.is_empty() {
                    self.overlay_picker = Some(OverlayPicker::resume(global, true));
                    return;
                }
            }
            scoped
        };
        if sessions.is_empty() && all_repos {
            self.transcript
                .push_notice(NoticeKind::Info, "no saved sessions found");
        } else {
            self.overlay_picker = Some(OverlayPicker::resume(sessions, all_repos));
        }
    }

    pub(super) fn dispatch_input(&mut self, raw: &str) -> Action {
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

        if let Some(mem) = trimmed.strip_prefix('#') {
            if !trimmed.contains('\n') {
                let note = mem.trim();
                if !note.is_empty() {
                    self.overlay_picker = Some(OverlayPicker::memory_save(note.to_owned()));
                }
                return Action::None;
            }
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
                    note: None,
                },
                SlashCommand::OpenEffortPicker => {
                    self.overlay_picker = Some(OverlayPicker::effort(self.effort.clone()));
                    Action::None
                }
                SlashCommand::OpenResume => {
                    self.open_resume_picker(false);
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
                            self.transcript.push_notice(
                                NoticeKind::Info,
                                "project memory (.twig/preferences.md)",
                            );
                            self.transcript.push_assistant(body);
                        }
                        Ok(Some(_)) => self
                            .transcript
                            .push_notice(NoticeKind::Info, ".twig/preferences.md is empty"),
                        Ok(None) => self.transcript.push_notice(
                            NoticeKind::Info,
                            "no .twig/preferences.md yet — run /memory or /init",
                        ),
                        Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                    }
                    Action::None
                }
                SlashCommand::ShowStatus => {
                    let msg = format_status_notice(
                        &self.footer_view(None, ""),
                        self.turns_used,
                        self.context_bytes,
                        self.context_max,
                    );
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
                SlashCommand::ToggleExpand => {
                    self.transcript.toggle_expand();
                    Action::None
                }
                SlashCommand::OpenFind(q) => {
                    self.transcript.search_open_with(&q);
                    Action::None
                }
                SlashCommand::CopyCode => match self.active_transcript_mut().copy_latest_code_block() {
                    Ok((text, msg)) => {
                        self.flash(msg);
                        Action::CopyCode(text)
                    }
                    Err(e) => {
                        self.flash(e);
                        Action::None
                    }
                },
                SlashCommand::ClearTranscript => {
                    self.active_transcript_mut().clear();
                    Action::None
                }
                SlashCommand::ShowHelp => {
                    self.set_footer_mode(FooterMode::ShortcutOverlay);
                    Action::None
                }
                other => Action::Slash(other),
            };
        }

        if let Some(sub_id) = self.viewing_subagent.clone() {
            if let Some(idx) = self.subagents.iter().position(|s| s.id == sub_id) {
                if self.subagents[idx].status == SubagentStatus::Running {
                    self.composer.set(trimmed);
                    self.flash("Subagent is still running — wait for it to finish or Esc ← main");
                    return Action::None;
                }
                let child_sid = self.subagents[idx]
                    .child_sid
                    .clone()
                    .unwrap_or_else(|| self.subagents[idx].id.clone());
                let role = self.subagents[idx].role.clone();
                let model = self.subagents[idx].model.clone();
                self.subagents[idx].status = SubagentStatus::Running;
                self.subagents[idx].active_turn = true;
                self.subagents[idx].started_at = Instant::now();
                self.subagents[idx].elapsed_secs = None;
                self.subagents[idx].error = None;
                self.subagents[idx].activity = Some("starting…".into());
                let text = trimmed.to_owned();
                if let Some(tr) = self.subagent_transcripts.get_mut(&sub_id) {
                    tr.push_user(text.clone());
                    tr.follow_tail();
                }
                self.flash(format!("Sent message to subagent [{role}]"));
                return Action::SendSubagentTurn {
                    id: sub_id,
                    child_sid,
                    role,
                    model,
                    input: text,
                };
            }
            self.viewing_subagent = None;
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

    pub(super) fn handle_slash_io(
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
                self.fork_req =
                    self.send_rpc(client, "session.fork", json!({}), "Forking session…");
            }
            SlashCommand::RewindSession {
                turns,
                restore_code,
            } => {
                self.rewind_req = self.send_rpc(
                    client,
                    "session.rewind",
                    json!({"turns": turns, "restore_code": restore_code}),
                    &format!("Rewinding {turns} turn(s)…"),
                );
            }
            SlashCommand::RenameSession(title) => {
                if self
                    .send_rpc(
                        client,
                        "session.rename",
                        json!({"title": title}),
                        &format!("Renamed session → {title}"),
                    )
                    .is_some()
                {
                    self.title = Some(title);
                }
            }
            SlashCommand::ArchiveSession => {
                let old_sid = self.sid.clone();
                if self.replace_client(client, None) {
                    match crate::runtime::session::archive_session(&old_sid) {
                        Ok(dst) => {
                            let short: String = old_sid.chars().take(10).collect();
                            self.flash(format!("Archived {short}"));
                            self.transcript.push_notice(
                                NoticeKind::Success,
                                format!("archived session → {dst}"),
                            );
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
                self.undo_req =
                    self.send_rpc(client, "edit.undo", json!({}), "Reverting last edit…");
            }
            SlashCommand::SwitchAgent(target) => {
                match target.as_deref() {
                    Some("main" | "parent" | "root") => {
                        self.viewing_subagent = None;
                        self.flash("Returned to main session");
                    }
                    Some(q) => {
                        let found = self
                            .subagents
                            .iter()
                            .rev()
                            .find(|s| {
                                s.id == q
                                    || s.child_sid.as_deref() == Some(q)
                                    || s.role.eq_ignore_ascii_case(q)
                            })
                            .map(|s| s.id.clone());
                        if let Some(id) = found {
                            self.viewing_subagent = Some(id);
                        } else {
                            self.flash(format!("No subagent matching `{q}`"));
                        }
                    }
                    None => {
                        if self.subagents.is_empty() {
                            self.transcript.push_notice(
                                NoticeKind::Info,
                                "no subagents in this session yet (press Ctrl+T for tasks)",
                            );
                        } else if self.subagents.len() == 1 {
                            if self.viewing_subagent.is_some() {
                                self.viewing_subagent = None;
                                self.flash("Returned to main session");
                            } else {
                                self.viewing_subagent = Some(self.subagents[0].id.clone());
                            }
                        } else {
                            self.overlay_picker = Some(OverlayPicker::tasks(
                                self.subagents.clone(),
                                self.cached_tasks.clone(),
                            ));
                        }
                    }
                }
            }
            SlashCommand::ListTasks => {
                self.tasks_req = self.send_rpc(
                    client,
                    "tasks.list",
                    json!({}),
                    if self.open_tasks_drawer {
                        ""
                    } else {
                        "Listing background tasks…"
                    },
                );
            }
            SlashCommand::CancelTask(task_id) => {
                self.open_tasks_drawer = true;
                self.tasks_req = self.send_rpc(
                    client,
                    "tasks.cancel",
                    json!({"id": task_id}),
                    &format!("Cancelled background task {task_id}"),
                );
            }
            SlashCommand::ManageSkills {
                action,
                name,
                project,
            } => {
                self.skills_req = self.send_rpc(
                    client,
                    "skill.manage",
                    json!({"action": action, "name": name, "project": project}),
                    &format!("Skills: {action}…"),
                );
            }
            SlashCommand::ManageMcp { action, server } => {
                let flash_msg = match action.as_str() {
                    "init_rea" => "Registering `rea` MCP in .twig/mcp.json (disabled by default)…".to_owned(),
                    "enable" | "disable" => format!(
                        "{} MCP server `{}`…",
                        if action == "enable" { "Enabling" } else { "Disabling" },
                        server.as_deref().unwrap_or("")
                    ),
                    _ => "Checking MCP servers…".to_owned(),
                };
                self.mcp_req = self.send_rpc(
                    client,
                    "mcp.list",
                    json!({"action": action, "server": server}),
                    &flash_msg,
                );
            }
            SlashCommand::Blackboard { action, fact } => {
                self.blackboard_req = self.send_rpc(
                    client,
                    "blackboard",
                    json!({"action": action, "fact": fact}),
                    "",
                );
            }
            SlashCommand::RunRci(sub) => {
                let repo_path = std::path::Path::new(&self.repo_raw);
                let sessions_dir = crate::config::paths::state_dir().join("sessions");
                match sub.as_str() {
                    "harvest" => match crate::eval::rci::harvest_sessions(&sessions_dir) {
                        Ok(rep) => {
                            self.transcript.push_notice(
                                NoticeKind::Playbook,
                                format!(
                                    "RCI label-free harvest: {} verified collective session(s) out of {} valid ledgers",
                                    rep.verified_collective_sessions, rep.chain_valid_files
                                ),
                            );
                            if let Ok(pretty) = serde_json::to_string_pretty(&rep.role_credits) {
                                self.transcript
                                    .push_assistant(format!("```json\n{pretty}\n```"));
                            }
                        }
                        Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                    },
                    "distill" | "distill --online" => {
                        let online = sub.contains("--online");
                        match crate::eval::rci::distill_role_notes(repo_path, &sessions_dir, online)
                        {
                            Ok((rep, files)) => {
                                let msg = format!(
                                    "RCI distilled {} role protocol cards ({} verified sessions harvested)",
                                    files.len(),
                                    rep.verified_collective_sessions
                                );
                                self.flash(&msg);
                                self.transcript.push_notice(NoticeKind::Success, msg);
                                self.transcript.push_assistant(
                                    files
                                        .into_iter()
                                        .map(|f| format!("- `{f}`"))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                );
                            }
                            Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                        }
                    }
                    _ => {
                        let msg = crate::runtime::blackboard::status_report(repo_path);
                        self.transcript.push_notice(NoticeKind::Info, msg);
                    }
                }
            }
            SlashCommand::CompactNow => {
                self.send_rpc(client, "session.compact", json!({}), "Compacting context…");
            }
            SlashCommand::VerifySession => {
                self.verify_req = self.send_rpc(
                    client,
                    "session.verify",
                    json!({}),
                    "Verifying session hash chain…",
                );
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
            SlashCommand::InitProject => match init_project_files(&self.repo_raw) {
                Ok((created, existing)) => {
                    self.flash("Initialized .twig/");
                    self.transcript
                        .push_notice(NoticeKind::Success, format_init_report(&created, &existing));
                }
                Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
            },
            SlashCommand::RunDoctor => {
                self.flash("Running twig doctor…");
                workers.spawn_doctor(self.repo_raw.clone());
            }
            SlashCommand::ExportTranscript(custom) => {
                let md = self.transcript.export_markdown(
                    &self.sid,
                    self.title.as_deref(),
                    &self.model,
                    &self.repo_path,
                );
                match write_export_file(&self.repo_raw, &self.sid, custom.as_deref(), &md) {
                    Ok(pretty) => {
                        self.flash("Exported transcript to Markdown");
                        self.transcript.push_notice(
                            NoticeKind::Success,
                            format!("exported transcript ({} bytes) → {pretty}", md.len()),
                        );
                    }
                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                }
            }
            SlashCommand::RunDiff => match run_git_diff(&self.repo_raw) {
                Ok(diff) if diff.trim().is_empty() => {
                    self.transcript
                        .push_notice(NoticeKind::Info, "working tree clean — no diff");
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
            SlashCommand::RunReview => {
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    "starting independent read-only review on current git diff…",
                );
                self.transcript.follow_tail();
                workers.spawn_review(self.repo_raw.clone());
            }
            SlashCommand::RunWalkthrough => {
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    "generating structured change walkthrough from git diff…",
                );
                self.transcript.follow_tail();
                workers.spawn_walkthrough(self.repo_raw.clone());
            }
            SlashCommand::DualPlan(goal) => {
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    format!("running blind dual-plan (`plan` × `advisor`) · {goal}"),
                );
                self.transcript.follow_tail();
                workers.spawn_dual_plan(self.repo_raw.clone(), goal);
            }
            SlashCommand::SideQuestion(q) => {
                self.btw_card = Some(BtwCard {
                    question: q.clone(),
                    answer: None,
                });
                self.btw_req = self.send_rpc(
                    client,
                    "session.btw",
                    json!({"question": q}),
                    "",
                );
            }
            SlashCommand::ExecutePlan(extra) => {
                if self.running {
                    self.flash("Wait for the active turn to finish before running /go");
                    return Ok(());
                }
                if self.plan_mode {
                    let _ = client.send("mode.set", json!({"plan": false}));
                    self.plan_mode = false;
                }
                if self.study_mode {
                    let _ = client.send("mode.study", json!({"enabled": false}));
                    self.study_mode = false;
                }
                let code_model = crate::llm::Provider::new().roles().get("code").to_owned();
                self.model = code_model.clone();
                let prompt = match extra {
                    Some(note) => format!(
                        "Proceed to implement the agreed plan above step-by-step, then run relevant tests/lints to verify.\nAdditional instructions: {note}"
                    ),
                    None => "Proceed to implement the agreed plan above step-by-step, then run relevant tests/lints to verify.".to_owned(),
                };
                self.transcript.push_notice(
                    NoticeKind::Playbook,
                    format!("handoff `/go` → write access enabled · executing with `{code_model}`"),
                );
                self.transcript.push_user(prompt.clone());
                self.running = true;
                self.run_started = Some(Instant::now());
                self.update_status_header(String::from("Implementing"));
                let (mut params, _) = super::client::build_turn_params(
                    &self.repo_raw,
                    prompt,
                    std::mem::take(&mut self.pending_images),
                    self.last_shell.as_ref(),
                );
                params["role"] = json!("code");
                match client.send("turn", params) {
                    Ok(id) => self.active_turn_req = Some(id),
                    Err(e) => {
                        self.running = false;
                        self.transcript.push_notice(NoticeKind::Error, e);
                    }
                }
            }
            SlashCommand::CommitChanges(hint) => {
                self.commit_req = self.send_rpc(
                    client,
                    "git.commit",
                    json!({"hint": hint}),
                    "Generating commit message & committing…",
                );
            }
            SlashCommand::CreatePr(arg) => {
                self.pr_req = self.send_rpc(
                    client,
                    "git.pr",
                    json!({"arg": arg}),
                    "Committing, pushing & opening Pull Request…",
                );
            }
            SlashCommand::ManageWorktree(arg) => {
                let flash = match arg.as_deref() {
                    None | Some("list") => "Listing git worktrees…".to_owned(),
                    Some("back" | "main") => "Handing session back to primary checkout…".to_owned(),
                    Some(name) => format!("Live handoff to isolated worktree `{name}`…"),
                };
                self.worktree_req = self.send_rpc(
                    client,
                    "git.worktree",
                    json!({"arg": arg}),
                    &flash,
                );
            }
            SlashCommand::QueueFollowUp(arg) => match arg.as_deref() {
                None | Some("list") => {
                    if self.queued_turns.is_empty() {
                        self.transcript.push_notice(
                            NoticeKind::Info,
                            "follow-up queue is empty — press Alt+Enter while running or `/queue <prompt>`",
                        );
                    } else {
                        let rows: Vec<String> = self
                            .queued_turns
                            .iter()
                            .enumerate()
                            .map(|(i, p)| format!("  {}. {p}", i + 1))
                            .collect();
                        self.transcript
                            .push_notice(NoticeKind::Playbook, "queued follow-up prompts");
                        self.transcript
                            .push_assistant(format!("```text\n{}\n```", rows.join("\n")));
                    }
                }
                Some("clear") => {
                    let n = self.queued_turns.len();
                    self.queued_turns.clear();
                    self.flash(format!("Cleared {n} queued follow-up(s)"));
                }
                Some(prompt) => {
                    if !self.running {
                        let act = self.dispatch_input(prompt);
                        let _ = self.handle_action(act, terminal, client, workers)?;
                    } else {
                        self.queued_turns.push_back(prompt.to_owned());
                        let n = self.queued_turns.len();
                        self.flash(format!("Queued follow-up #{n}"));
                        self.transcript.push_notice(
                            NoticeKind::Info,
                            format!("queued next #{n}: {prompt}"),
                        );
                    }
                }
            },
            SlashCommand::ApplySession(sid) => {
                self.apply_req = self.send_rpc(
                    client,
                    "session.apply",
                    json!({"sid": sid}),
                    &format!("Applying patch from {sid}…"),
                );
            }
            SlashCommand::GcSessions => {
                match crate::runtime::session::gc_sessions_and_subagents() {
                    Ok((wt, empty)) => {
                        let msg = format!(
                            "gc: removed {wt} clean subagent worktree(s) and {empty} empty session file(s)"
                        );
                        self.flash(&msg);
                        self.transcript.push_notice(NoticeKind::Success, msg);
                    }
                    Err(e) => self.transcript.push_notice(NoticeKind::Error, e),
                }
            }
            SlashCommand::ShowRoles => {
                self.roles_req =
                    self.send_rpc(client, "roles.get", json!({}), "Checking role routing…");
            }
            SlashCommand::ResetRoles => {
                self.roles_req = self.send_rpc(
                    client,
                    "roles.set",
                    json!({"action": "reset"}),
                    "Resetting role routing…",
                );
            }
            SlashCommand::SetRole {
                role,
                model,
                effort,
            } => {
                self.roles_req = self.send_rpc(
                    client,
                    "roles.set",
                    json!({"role": role, "model": model, "effort": effort}),
                    &format!("Updating role `{role}`…"),
                );
            }
            SlashCommand::SetOperator(opt) => {
                let next = opt.unwrap_or(!self.operator_mode);
                if self
                    .send_rpc(client, "mode.operator", json!({"enabled": next}), "")
                    .is_some()
                {
                    self.operator_mode = next;
                    let msg = if next {
                        "Operator mode ON (Agent-as-Memory orchestration active)"
                    } else {
                        "Operator mode OFF"
                    };
                    self.flash(msg);
                    self.transcript.push_notice(NoticeKind::Playbook, msg);
                }
            }
            SlashCommand::SetStudy { enabled, target } => {
                let next = enabled.unwrap_or(!self.study_mode);
                if self
                    .send_rpc(client, "mode.study", json!({"enabled": next}), "")
                    .is_some()
                {
                    self.study_mode = next;
                }
                if let Some(goal) = target {
                    if self.running {
                        self.flash("Wait for the active turn to finish before running /study");
                        return Ok(());
                    }
                    let role = if self.plan_mode { "plan" } else { "code" };
                    let prompt = format!(
                        "[Clean-Room Study & SPEC Extraction]\n\
                         Target: {goal}\n\
                         Inspect observable behavior, structure (`read` headers/hexdump/.har, `outline` xrefs/deps), or run safe black-box probes without fabricating unseen internals. \
                         Produce a structured specification separating `[Observed]`, `[Derived]`, `[Inferred]`, and `[Unknown / Limitation]`, followed by concrete acceptance test cases ready for `/go`."
                    );
                    self.transcript.push_notice(
                        NoticeKind::Playbook,
                        format!("REA Study mode ON · studying `{goal}`"),
                    );
                    self.transcript.push_user(prompt.clone());
                    self.running = true;
                    self.run_started = Some(Instant::now());
                    self.update_status_header(String::from("Studying"));
                    let (mut params, _) = super::client::build_turn_params(
                        &self.repo_raw,
                        prompt,
                        std::mem::take(&mut self.pending_images),
                        self.last_shell.as_ref(),
                    );
                    params["role"] = json!(role);
                    match client.send("turn", params) {
                        Ok(id) => self.active_turn_req = Some(id),
                        Err(e) => {
                            self.running = false;
                            self.transcript.push_notice(NoticeKind::Error, e);
                        }
                    }
                } else {
                    let msg = if next {
                        "REA Study mode ON (Observed/Derived/Inferred/Unknown provenance active · `/study off` disables)"
                    } else {
                        "REA Study mode OFF (zero prompt overhead)"
                    };
                    self.flash(msg);
                    self.transcript.push_notice(NoticeKind::Playbook, msg);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn apply_child_stream_event(tr: &mut super::transcript::Transcript, ev: &Value) {
    match ev["ev"].as_str() {
        Some("model.reasoning") => {
            if let Some(d) = ev["text"].as_str() {
                tr.append_reasoning_delta(d);
            }
        }
        Some("model.delta") => {
            if let Some(d) = ev["text"].as_str() {
                tr.append_assistant_delta(d);
            }
        }
        Some("tool.started") => {
            let id = ev["id"].as_str().unwrap_or("").to_owned();
            let name = ev["tool"].as_str().unwrap_or("tool").to_owned();
            let tier = ev["tier"].as_str().unwrap_or("auto").to_owned();
            let summary = summarize_tool_args(&name, &ev["args"]);
            tr.push_tool_started(id, name, summary, tier);
        }
        Some("tool.output") => {
            tr.append_tool_output(
                ev["id"].as_str().unwrap_or(""),
                ev["text"].as_str().unwrap_or(""),
            );
        }
        Some("tool.completed") => {
            tr.finish_tool(
                ev["id"].as_str().unwrap_or(""),
                None,
                ev["exit_code"].as_i64().map(|c| c as i32),
                None,
            );
        }
        Some("model.retry") => {
            let attempt = ev["attempt"].as_u64().unwrap_or(1);
            let secs = ev["backoff_secs"].as_u64().unwrap_or(1);
            tr.push_notice(
                NoticeKind::Warn,
                format!("model transient error — retrying (attempt {attempt}, {secs}s)…"),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::markdown::MotionMode;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn render_text(app: &mut App, width: u16, height: u16) -> String {
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
    fn subagent_thread_tree_interactive_chat_mouse_copy_and_todo_lifecycle_work() {
        let mut app = App::new(".");
        app.motion = MotionMode::Reduced;

        // 1. Live subagent stream + 0-token parent isolation + interactive follow-up turn
        let cells_before = app.transcript.len();
        app.apply_event(&json!({
            "ev": "subagent.started", "id": "sub-1", "role": "scout", "task": "Map tui modules", "parallel": true
        }));
        app.apply_event(&json!({
            "ev": "subagent.started", "id": "sub-2", "role": "code", "task": "Fix web timeout", "parallel": true
        }));
        app.apply_event(&json!({
            "ev": "subagent.progress", "id": "sub-1", "sid": "scout-sid-1", "role": "scout",
            "activity": "Read(src/tui/mod.rs)", "turns": 2, "tool_count": 5
        }));
        app.apply_event(&json!({
            "ev": "subagent.stream", "id": "sub-1", "role": "scout",
            "child": {"ev": "model.delta", "text": "Subagent-1 live finding: TUI uses Ratatui."}
        }));
        assert_eq!(app.transcript.len(), cells_before);
        assert!(render_text(&mut app, 128, 28).contains("Read(src/tui/mod.rs) · 5 tools · t2"));

        app.apply_event(&json!({
            "ev": "subagent.completed", "id": "sub-1", "sid": "scout-sid-1", "role": "scout", "ok": true, "elapsed_secs": 3
        }));
        app.handle_key(press(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(press(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.viewing_subagent.as_deref(), Some("sub-1"));

        // Direct interactive follow-up inside viewed subagent thread!
        for ch in "Check line 120 too".chars() {
            app.handle_key(press(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SendSubagentTurn {
                id: "sub-1".into(),
                child_sid: "scout-sid-1".into(),
                role: "scout".into(),
                model: None,
                input: "Check line 120 too".into(),
            }
        );
        app.handle_key(press(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.viewing_subagent.as_deref(), Some("sub-2"));
        app.handle_key(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.viewing_subagent.is_none());

        // 2. Persistent child_threads remount via `session.history`
        app.history_req = Some(42);
        app.apply_server_msg(ServerMsg::Reply {
            id: 42,
            result: json!({
                "sid": "main-99",
                "messages": [{"role": "user", "content": "audit"}],
                "child_threads": [{
                    "sid": "scout-restored",
                    "role": "scout",
                    "task": "Audit MCP",
                    "turns": 1,
                    "tool_count": 3,
                    "messages": [
                        {"role": "user", "content": "Audit MCP"},
                        {"role": "assistant", "content": "MCP findings ready."}
                    ]
                }]
            }),
        });
        assert_eq!(app.subagents.len(), 1);
        app.viewing_subagent = Some("scout-restored".into());
        assert!(render_text(&mut app, 110, 24).contains("MCP findings ready."));
        app.viewing_subagent = None;

        // 3. Plan progress (`todo`) idle state + `+1 more` + follow-up cleanup
        app.running = true;
        app.apply_event(&json!({
            "ev": "todo.updated",
            "items": [
                {"content": "Step A", "status": "completed"},
                {"content": "Step B", "status": "in_progress"},
                {"content": "Step C", "status": "pending"},
                {"content": "Step D", "status": "pending"},
                {"content": "Step E", "status": "pending"}
            ]
        }));
        app.running = false;
        let idle_todo = render_text(&mut app, 110, 26);
        assert!(idle_todo.contains("Plan progress  1/5  · idle  · +1 more") && idle_todo.contains("◐ Step B"));
        // Follow-up turn finishing without updating todo clears stale checklist
        app.todos_updated_in_turn = false;
        app.active_turn_req = Some(77);
        app.apply_server_msg(ServerMsg::Reply {
            id: 77,
            result: json!({"done": true, "reason": "final", "text": "All done"}),
        });
        assert!(app.todos.is_empty());

        // 4. Native mouse drag-selection auto-copy + scroll retention
        app.transcript.clear();
        for i in 0..30 {
            app.transcript.push_user(format!("Prompt line {i}"));
            app.transcript.push_assistant(format!("Answer line {i}"));
        }
        let _ = render_text(&mut app, 100, 24);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 18,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 18,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            Action::CopyCode(_)
        ));
        app.handle_key(press(KeyCode::Home, KeyModifiers::NONE));
        let _ = render_text(&mut app, 100, 24);
        app.apply_event(&json!({"ev": "model.delta", "text": "Tail update"}));
        assert!(render_text(&mut app, 100, 24).contains("Prompt line 0"));
    }
}
