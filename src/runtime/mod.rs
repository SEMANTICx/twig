//! Agent session runtime: JSON-RPC daemon, turn execution, policy, guard,
//! context compaction, playbooks/skills, subagents, and terminal client.

pub mod context;
pub mod guard;
pub mod guidance;
pub mod mcp;
pub mod policy;
pub mod subagent;
pub mod tasks;
pub mod tools;
pub mod turn;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use serde_json::{json, Value};

use self::context::Context;
use self::guard::Guard;
use self::guidance::{inject_playbook, route, Playbook, SkillStore};
use self::policy::{Policy, Tier};
use self::tools::Tools;
use self::turn::{execute_recorded_call, run_turn_live, ExecEnv, TurnCtx};
use crate::config::HarnessConfig;
use crate::control::Control;
use crate::exec::{run_check, Check};
use crate::llm::{Llm, ToolCall};
use crate::record::{Lease, Record};
use crate::verify::{gate, Candidate};

const UNKNOWN_OUTCOME_MSG: &str = "PREVIOUS EXECUTION OUTCOME UNKNOWN. Command was NOT replayed after restart. Inspect the workspace manually before taking further action.";
const RESOLVED_UNKNOWN_MSG: &str = "Previous execution outcome is unknown. The command was NOT replayed. Inspect the workspace manually before issuing any new side effect.";
const MAX_REPO_INSTRUCTIONS_BYTES: usize = 32_768;

/// Load repository instructions in precedence order: `AGENTS.md` -> `CLAUDE.md` -> `.twig/preferences.md`.
fn append_repo_instructions(worktree: &std::path::Path, system: &mut String) {
    let mut budget = MAX_REPO_INSTRUCTIONS_BYTES;
    for rel in ["AGENTS.md", "CLAUDE.md", ".twig/preferences.md"] {
        if budget == 0 {
            break;
        }
        let path = worktree.join(rel);
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = body.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clipped: String = trimmed.chars().take(budget).collect();
        budget = budget.saturating_sub(clipped.len());
        system.push_str(&format!("\n\n## {rel}\n{clipped}"));
    }
}

struct RecoveredState {
    messages: Vec<Value>,
    pending: Vec<Value>,
    constraints: Vec<String>,
    check: Option<Check>,
}

/// Single-pass O(N) session recovery from the append-only hash-chained record.
fn recover_session(rows: &[Value], default_check: Option<Check>) -> RecoveredState {
    let check = rows
        .iter()
        .rev()
        .find(|r| r["kind"] == "acceptance.configured")
        .and_then(|r| serde_json::from_value(r["check"].clone()).ok())
        .or(default_check);

    let mut constraints = Vec::new();
    let mut has_result = HashSet::new();
    let mut resolved_recovered = HashSet::new();
    let mut resolved_any = HashSet::new();
    let mut requested: HashMap<String, (String, Value)> = HashMap::new();
    let mut started: HashMap<String, (String, Value)> = HashMap::new();

    for row in rows {
        let kind = row["kind"].as_str().unwrap_or("");
        match kind {
            "steering.queued" | "steering.applied" => {
                if let Some(text) = row["text"].as_str() {
                    if !text.trim().is_empty() && !constraints.iter().any(|c| c == text) {
                        constraints.push(text.to_owned());
                    }
                }
            }
            "tool.result" => {
                if let Some(id) = row["id"].as_str() {
                    has_result.insert(id.to_owned());
                }
            }
            "approval.resolved" => {
                if let Some(id) = row["id"].as_str() {
                    resolved_any.insert(id.to_owned());
                    if row["recovered"] == true {
                        resolved_recovered.insert(id.to_owned());
                    }
                }
            }
            "approval.requested" => {
                if let Some(id) = row["id"].as_str() {
                    let name = row["name"]
                        .as_str()
                        .or(row["tool"].as_str())
                        .unwrap_or("bash")
                        .to_owned();
                    requested
                        .entry(id.to_owned())
                        .or_insert((name, row["args"].clone()));
                }
            }
            "tool.started" => {
                if let Some(id) = row["id"].as_str() {
                    let name = row["tool"]
                        .as_str()
                        .or(row["name"].as_str())
                        .unwrap_or("bash")
                        .to_owned();
                    started
                        .entry(id.to_owned())
                        .or_insert((name, row["args"].clone()));
                }
            }
            _ => {}
        }
    }

    let mut pending = Vec::new();
    let mut unknown_ids = HashSet::new();
    let mut seen_pending = HashSet::new();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        let kind = row["kind"].as_str().unwrap_or("");
        if id.is_empty()
            || has_result.contains(id)
            || resolved_recovered.contains(id)
            || !matches!(kind, "approval.requested" | "tool.started")
        {
            continue;
        }
        let started_without_result = started.contains_key(id);
        let unresolved_approval =
            requested.contains_key(id) && !resolved_any.contains(id);
        if !started_without_result && !unresolved_approval {
            continue;
        }
        unknown_ids.insert(id.to_owned());
        if requested.contains_key(id) && seen_pending.insert(id.to_owned()) {
            let (name, args) = requested
                .get(id)
                .or_else(|| started.get(id))
                .cloned()
                .unwrap_or_else(|| ("bash".into(), Value::Null));
            pending.push(json!({
                "ev": "approval.requested",
                "id": id,
                "name": name,
                "args": args,
                "recovered": true
            }));
        }
    }

    let checkpoint = rows
        .iter()
        .rposition(|r| r["kind"] == "messages.checkpoint");
    let mut messages: Vec<Value> = checkpoint
        .and_then(|at| rows[at]["messages"].as_array().cloned())
        .unwrap_or_default();
    for row in rows.iter().skip(checkpoint.map_or(0, |n| n + 1)) {
        if row["kind"] == "message" {
            messages.push(row["message"].clone());
        }
    }

    let mut tool_replies = HashSet::new();
    for message in &messages {
        if message["role"] == "tool" {
            if let Some(id) = message["tool_call_id"].as_str() {
                tool_replies.insert(id.to_owned());
            }
        }
    }
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        if id.is_empty() || tool_replies.contains(id) {
            continue;
        }
        let output = match row["kind"].as_str() {
            Some("tool.result") => Some(
                row["output"]
                    .as_str()
                    .unwrap_or("tool completed")
                    .to_owned(),
            ),
            Some("approval.resolved") if row["recovered"] == true => {
                Some(RESOLVED_UNKNOWN_MSG.to_owned())
            }
            Some("approval.resolved") if row["allow"] == false => Some("denied by user".to_owned()),
            _ => None,
        };
        if let Some(output) = output {
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": output}));
            tool_replies.insert(id.to_owned());
        }
    }

    let mut recovered_replies = Vec::new();
    for assistant in messages.iter().filter(|m| m["role"] == "assistant") {
        for call in assistant["tool_calls"].as_array().into_iter().flatten() {
            let Some(id) = call["id"].as_str() else {
                continue;
            };
            if tool_replies.contains(id) {
                continue;
            }
            let output = if unknown_ids.contains(id) {
                UNKNOWN_OUTCOME_MSG
            } else {
                "Session interrupted before this tool call started. It was NOT executed."
            };
            recovered_replies.push(json!({"role": "tool", "tool_call_id": id, "content": output}));
            tool_replies.insert(id.to_owned());
        }
    }
    messages.extend(recovered_replies);

    RecoveredState {
        messages,
        pending,
        constraints,
        check,
    }
}

pub struct Daemon<'a> {
    llm: &'a dyn Llm,
    worktree: PathBuf,
    runs_dir: PathBuf,
    record: Record,
    guard: Guard,
    system: String,
    tools: Tools,
    policy: Policy,
    messages: Vec<Value>,
    audit: Vec<Value>,
    /// T2 calls waiting on a human. The only place the harness ever waits.
    pending: Vec<Value>,
    /// Unstarted tool calls remaining after a non-interactive mid-batch pause.
    deferred_calls: Vec<(ToolCall, Tier)>,
    journal: Option<PathBuf>,
    candidate: Option<Candidate>,
    min_branch: f64,
    max_verify_rounds: u32,
    playbooks: Vec<Playbook>,
    control: Arc<Control>,
    interactive: bool,
    _lease: Lease,
    context: Context,
    check: Option<Check>,
    model_override: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    plan_mode: bool,
    todos: Vec<Value>,
}

impl<'a> Daemon<'a> {
    pub fn new(
        llm: &'a dyn Llm,
        worktree: impl Into<PathBuf>,
        runs_dir: impl Into<PathBuf>,
        sid: impl Into<String>,
        cfg: &HarnessConfig,
    ) -> Result<Self, String> {
        let worktree: PathBuf = worktree.into();
        let runs_dir: PathBuf = runs_dir.into();
        let sid = sid.into();
        let lease = Lease::acquire(&runs_dir.join(format!("{sid}.lock")))
            .map_err(|e| e.to_string())?;
        let task_file = worktree.join(".twig/project.json");
        let project: Value = if task_file.exists() {
            serde_json::from_str(&std::fs::read_to_string(&task_file).map_err(|e| e.to_string())?)
                .map_err(|e| format!("project config: {e}"))?
        } else {
            json!({})
        };
        let default_check = project
            .get("check")
            .map(|c| serde_json::from_value(c.clone()).map_err(|e| format!("project check: {e}")))
            .transpose()?;
        let record =
            Record::open(runs_dir.join(format!("{sid}.jsonl")), sid).map_err(|e| e.to_string())?;
        let rows = record.tail(usize::MAX).map_err(|e| e.to_string())?;
        let title = rows
            .iter()
            .rev()
            .find(|r| r["kind"] == "session.renamed")
            .and_then(|r| r["title"].as_str())
            .map(str::to_owned);
        let plan_mode = rows
            .iter()
            .rev()
            .find(|r| r["kind"] == "mode.changed")
            .and_then(|r| r["plan"].as_bool())
            .unwrap_or(false);
        let todos = rows
            .iter()
            .rev()
            .find(|r| r["kind"] == "todo.updated")
            .and_then(|r| r["items"].as_array().cloned())
            .unwrap_or_default();
        let recovered = recover_session(&rows, default_check);
        let guard =
            Guard::new(&worktree, cfg.max_turns, cfg.stall_turns, 3).ignoring([runs_dir.clone()]);
        let mut context = Context::new(cfg.context_max_bytes, runs_dir.join("artifacts"));
        for c in recovered.constraints {
            context.add_constraint(c);
        }
        let policy = Policy::new();
        for row in &rows {
            if row["kind"] == "policy.allowed" {
                if let Some(pat) = row["pattern"].as_str() {
                    policy.allow_for_session(pat);
                }
            }
        }
        let mut tools =
            Tools::with_config(&worktree, cfg).with_artifacts_dir(runs_dir.join("artifacts"));
        tools.apply_plan_mode(plan_mode);
        let initial_effort = std::env::var("TWIG_EFFORT")
            .ok()
            .and_then(|v| crate::llm::normalize_effort(&v).ok())
            .unwrap_or_else(|| llm.effort());
        if initial_effort != llm.effort() {
            llm.set_effort(initial_effort.as_deref());
        }

        Ok(Self {
            llm,
            _lease: lease,
            context,
            check: recovered.check,
            model_override: None,
            effort: initial_effort,
            title,
            plan_mode,
            todos,
            tools,
            system: cfg.system.clone(),
            worktree,
            runs_dir,
            record,
            guard,
            policy,
            messages: recovered.messages,
            audit: Vec::new(),
            pending: recovered.pending,
            deferred_calls: Vec::new(),
            journal: None,
            candidate: None,
            min_branch: 0.0,
            max_verify_rounds: 2,
            playbooks: Playbook::builtin(),
            control: Default::default(),
            interactive: false,
        })
    }

    pub fn record_path(&self) -> &std::path::Path {
        self.record.path()
    }

    pub fn sid(&self) -> String {
        self.record
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    fn drain_deferred_calls(&mut self, emit: &mut dyn FnMut(Value)) -> Result<(), String> {
        let remaining = std::mem::take(&mut self.deferred_calls);
        let mut iter = remaining.into_iter();
        while let Some((call, tier)) = iter.next() {
            if tier == Tier::Deny {
                self.record
                    .append(
                        "approval.requested",
                        json!({"id": call.id, "name": call.name, "args": call.args}),
                    )
                    .map_err(|e| e.to_string())?;
                let ev = json!({
                    "ev": "approval.requested",
                    "id": call.id,
                    "name": call.name,
                    "args": call.args
                });
                self.pending.push(ev.clone());
                emit(ev);
                self.deferred_calls = iter.collect();
                break;
            }
            let audit = &mut self.audit;
            let mut sink = |c: &ToolCall, out: &str| {
                audit.push(json!({"id": c.id, "args": c.args, "bytes": out.len()}));
            };
            execute_recorded_call(
                ExecEnv {
                    tools: &mut self.tools,
                    guard: &mut self.guard,
                    record: &mut self.record,
                    messages: &mut self.messages,
                    control: &self.control,
                },
                &call,
                tier,
                &mut sink,
                emit,
            )?;
        }
        Ok(())
    }

    pub fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        match method {
            "ping" => {
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                Ok(json!({
                    "pong": true,
                    "sid": self.sid(),
                    "model": model,
                    "effort": self.effort,
                    "title": self.title,
                    "plan": self.plan_mode,
                    "read_only": self.tools.is_read_only(),
                    "context_bytes": self.context.used_bytes(&self.messages),
                    "context_max": self.context.max_bytes(),
                    "running_tasks": self.tools.running_tasks_count(),
                    "env_summary": self.tools.env_summary(),
                }))
            }

            "session.history" => {
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                let turns = self
                    .messages
                    .iter()
                    .filter(|m| m["role"] == "assistant")
                    .count();
                let playbooks: Vec<Value> = self
                    .playbooks
                    .iter()
                    .map(|p| json!({"name": p.name, "when": p.when, "steps": p.steps}))
                    .collect();
                Ok(json!({
                    "sid": self.sid(),
                    "title": self.title,
                    "model": model,
                    "effort": self.effort,
                    "plan": self.plan_mode,
                    "read_only": self.tools.is_read_only(),
                    "turns": turns,
                    "messages": self.messages,
                    "todos": self.todos,
                    "pending": self.pending,
                    "context_bytes": self.context.used_bytes(&self.messages),
                    "context_max": self.context.max_bytes(),
                    "playbooks": playbooks,
                    "skills": self.tools.skill_metadata(),
                    "env_summary": self.tools.env_summary(),
                }))
            }

            "session.rename" => {
                let title = params
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or("session.rename requires nonempty title")?
                    .to_owned();
                self.title = Some(title.clone());
                self.record
                    .append("session.renamed", json!({"title": title}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"title": title}))
            }

            "session.compact" => {
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                match self.context.compact_now(&mut self.messages, self.llm, &model)? {
                    Some(compaction) => {
                        self.record
                            .append("context.compacted", json!({"compaction": compaction}))
                            .map_err(|e| e.to_string())?;
                        self.record
                            .append("messages.checkpoint", json!({"messages": self.messages}))
                            .map_err(|e| e.to_string())?;
                        emit(json!({"ev": "context.compacted", "compaction": compaction}));
                        Ok(json!({
                            "compacted": true,
                            "compaction": compaction,
                            "context_bytes": self.context.used_bytes(&self.messages),
                            "context_max": self.context.max_bytes(),
                        }))
                    }
                    None => Ok(json!({
                        "compacted": false,
                        "context_bytes": self.context.used_bytes(&self.messages),
                        "context_max": self.context.max_bytes(),
                    })),
                }
            }

            "model.set" => {
                let requested = params
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                if requested.is_empty() || requested == "reset" || requested == "default" {
                    self.model_override = None;
                } else {
                    self.model_override = Some(requested.to_owned());
                }
                let _ = crate::llm::Provider::save_model(self.model_override.as_deref());
                let active = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                let env_override = std::env::var_os("TWIG_MODEL").is_some()
                    || std::env::var_os("TWIG_ROLES").is_some();
                Ok(json!({"model": active, "env_override": env_override}))
            }

            "effort.set" => {
                let raw = params.get("effort").and_then(Value::as_str).unwrap_or("");
                let next = crate::llm::normalize_effort(raw)?;
                self.effort = next.clone();
                self.llm.set_effort(next.as_deref());
                let _ = crate::llm::Provider::save_effort(next.as_deref());
                self.record
                    .append("effort.changed", json!({"effort": next}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"effort": self.effort}))
            }

            "session.fork" => {
                let new_sid = crate::config::paths::session_id();
                let mut forked =
                    Record::open(self.runs_dir.join(format!("{new_sid}.jsonl")), new_sid.clone())
                        .map_err(|e| e.to_string())?;
                forked
                    .append("session.forked", json!({"from": self.sid()}))
                    .map_err(|e| e.to_string())?;
                if let Some(title) = &self.title {
                    forked
                        .append("session.renamed", json!({"title": format!("{title} (fork)")}))
                        .map_err(|e| e.to_string())?;
                }
                if self.plan_mode {
                    forked
                        .append("mode.changed", json!({"plan": true}))
                        .map_err(|e| e.to_string())?;
                }
                if !self.todos.is_empty() {
                    forked
                        .append("todo.updated", json!({"items": self.todos}))
                        .map_err(|e| e.to_string())?;
                }
                forked
                    .append("messages.checkpoint", json!({"messages": self.messages}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"sid": new_sid, "messages": self.messages.len()}))
            }

            "session.rewind" => {
                let requested = params.get("turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let n = requested.max(1);
                let user_indices: Vec<usize> = self
                    .messages
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| (m["role"] == "user").then_some(i))
                    .collect();
                if user_indices.is_empty() {
                    return Err("no user turns to rewind".into());
                }
                let cut_pos = user_indices.len().saturating_sub(n);
                let keep_len = user_indices[cut_pos];
                let rewound_turns = user_indices.len() - cut_pos;
                self.messages.truncate(keep_len);
                self.pending.clear();
                self.deferred_calls.clear();
                self.guard.reset_transient();
                if self.messages.is_empty() {
                    self.todos.clear();
                }
                self.record
                    .append("session.rewound", json!({"turns": rewound_turns}))
                    .map_err(|e| e.to_string())?;
                self.record
                    .append("messages.checkpoint", json!({"messages": self.messages}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "rewound_turns": rewound_turns,
                    "messages": self.messages,
                    "todos": self.todos,
                    "context_bytes": self.context.used_bytes(&self.messages),
                    "context_max": self.context.max_bytes(),
                    "running_tasks": self.tools.running_tasks_count(),
                    "env_summary": self.tools.env_summary(),
                }))
            }

            "mcp.list" => Ok(self.tools.list_mcp()),
            "tasks.list" => Ok(self.tools.list_tasks()),
            "skill.manage" => {
                let store = SkillStore::default_for(Some(&self.worktree));
                let action = params["action"].as_str().unwrap_or("list");
                let project = params["project"].as_bool().unwrap_or(false);
                match action {
                    "list" => Ok(json!({"action": "list", "ok": true, "report": store.list_report()})),
                    "doctor" => {
                        let (ok, report) = store.doctor_report();
                        Ok(json!({"action": "doctor", "ok": ok, "report": report}))
                    }
                    "enable" => {
                        let name = params["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or("enable requires a skill name")?;
                        let msg = store.enable(name, project)?;
                        self.tools.reload_skills();
                        Ok(json!({"action": "enable", "ok": true, "report": msg, "env_summary": self.tools.env_summary()}))
                    }
                    "disable" => {
                        let name = params["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or("disable requires a skill name")?;
                        let msg = store.disable(name, project)?;
                        self.tools.reload_skills();
                        Ok(json!({"action": "disable", "ok": true, "report": msg, "env_summary": self.tools.env_summary()}))
                    }
                    other => Err(format!("unknown skill action: {other}")),
                }
            }

            "mode.set" => {
                let next = params
                    .get("plan")
                    .and_then(Value::as_bool)
                    .unwrap_or(!self.plan_mode);
                self.plan_mode = next;
                let read_only = self.tools.apply_plan_mode(next);
                self.record
                    .append("mode.changed", json!({"plan": next}))
                    .map_err(|e| e.to_string())?;
                emit(json!({"ev": "mode.changed", "plan": next, "read_only": read_only}));
                Ok(json!({"plan": next, "read_only": read_only}))
            }

            "steer" => {
                let text = params["input"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("steer requires nonempty input")?;
                self.context.add_constraint(text);
                self.record
                    .append("steering.queued", json!({"text": text}))
                    .map_err(|e| e.to_string())?;
                self.control.steer(text.to_owned());
                Ok(json!({"queued": true}))
            }

            "playbooks.route" => {
                let goal = params.get("goal").and_then(Value::as_str).unwrap_or("");
                match route(goal, &self.playbooks) {
                    Some(book) => Ok(json!({
                        "playbook": book.name,
                        "steps": book.steps,
                        "keyword_hits": book.when.iter()
                            .filter(|k| goal.to_lowercase().contains(k.as_str()))
                            .collect::<Vec<_>>()
                    })),
                    None => Ok(json!({
                        "playbook": Value::Null,
                        "why": "no playbook matched; ask instead of guessing"
                    })),
                }
            }

            "turn" => {
                if let Some(j) = params.get("journal").and_then(Value::as_str) {
                    self.journal = Some(PathBuf::from(j));
                }
                if let Some(c) = params.get("candidate") {
                    if let Some(program) = c.get("program").and_then(Value::as_str) {
                        let args = c
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.candidate = Some(Candidate::new(program, args));
                    }
                }
                if let Some(b) = params.get("min_branch").and_then(Value::as_f64) {
                    self.min_branch = b;
                }

                let input = params.get("input").and_then(Value::as_str).unwrap_or("");
                let role = params.get("role").and_then(Value::as_str).unwrap_or("code");
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get(role).to_string());
                if !self.pending.is_empty() {
                    return Err("pending tool approval must be resolved first; explicitly /approve ID or /deny ID".into());
                }

                let images = params
                    .get("images")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let user_msg = if images.is_empty() {
                    json!({"role": "user", "content": input})
                } else {
                    let mut parts = vec![json!({"type": "text", "text": input})];
                    for img in images {
                        if let Some(url) = img["url"].as_str() {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    json!({"role": "user", "content": parts})
                };
                self.messages.push(user_msg.clone());
                self.context.sync_constraints(&mut self.messages);
                self.record
                    .append("message", json!({"message": user_msg}))
                    .map_err(|e| e.to_string())?;
                self.record
                    .append("messages.checkpoint", json!({"messages": self.messages}))
                    .map_err(|e| e.to_string())?;

                if let Some(check) = params.get("check") {
                    self.check = Some(
                        serde_json::from_value(check.clone())
                            .map_err(|e| format!("check config: {e}"))?,
                    );
                }
                if let Some(check) = &self.check {
                    self.record
                        .append("acceptance.configured", json!({"check": check}))
                        .map_err(|e| e.to_string())?;
                }
                self.tools.reload_skills();
                let mut system = self.system.clone();
                if let Some(book) = route(input, &self.playbooks) {
                    inject_playbook(&mut system, book, "");
                    emit(json!({"ev": "playbook.selected", "name": book.name, "steps": book.steps}));
                }
                append_repo_instructions(&self.worktree, &mut system);
                system.push_str(&self.tools.system_addendum_for(input));
                let skill_store = SkillStore::default_for(Some(&self.worktree));
                for (name, scope, body) in skill_store.resolve_explicit_invocations(input) {
                    system.push_str(&format!(
                        "\n\n## Explicitly invoked skill: ${name} ({scope})\n{body}"
                    ));
                    emit(json!({"ev": "skill.loaded", "name": name, "scope": scope}));
                }

                let mut verify_rounds = 0u32;
                loop {
                    if let Some(compaction) =
                        self.context
                            .compact(&mut self.messages, self.llm, &model)?
                    {
                        self.record
                            .append("context.compacted", json!({"compaction": compaction}))
                            .map_err(|e| e.to_string())?;
                        self.record
                            .append("messages.checkpoint", json!({"messages": self.messages}))
                            .map_err(|e| e.to_string())?;
                        emit(json!({"ev": "context.compacted", "compaction": compaction}));
                    }
                    let result = {
                        let audit = &mut self.audit;
                        let pending = &mut self.pending;
                        let todos = &mut self.todos;
                        let mut sink = |call: &ToolCall, out: &str| {
                            audit.push(json!({"id": call.id, "args": call.args, "bytes": out.len()}));
                        };
                        let mut capture = |ev: Value| {
                            if ev["ev"] == "approval.requested" {
                                pending.push(ev.clone());
                            }
                            if ev["ev"] == "approval.resolved" {
                                pending.retain(|p| p["id"] != ev["id"]);
                            }
                            if ev["ev"] == "todo.updated" {
                                if let Some(items) = ev["items"].as_array() {
                                    *todos = items.clone();
                                }
                            }
                            emit(ev);
                        };
                        run_turn_live(
                            TurnCtx {
                                llm: self.llm,
                                tools: &mut self.tools,
                                record: &mut self.record,
                                guard: &mut self.guard,
                                policy: &self.policy,
                                messages: &mut self.messages,
                                system: &system,
                                model: &model,
                            },
                            &mut sink,
                            &mut capture,
                            &self.control,
                            self.interactive,
                        )?
                    };

                    for steering in result.applied_steering {
                        self.context.add_constraint(steering);
                    }
                    if !result.deferred_calls.is_empty() {
                        self.deferred_calls = result.deferred_calls;
                    }
                    emit(json!({
                        "ev": "context.usage",
                        "bytes": self.context.used_bytes(&self.messages),
                        "max_bytes": self.context.max_bytes()
                    }));

                    if !result.done {
                        continue;
                    }
                    if ["budget", "cancelled", "awaiting_approval"].contains(&result.reason) {
                        return Ok(json!({"done": true, "reason": result.reason}));
                    }
                    if let Some(check) = &self.check {
                        let verdict =
                            run_check(&self.worktree, check).map_err(|e| e.to_string())?;
                        self.record
                            .append("verify.completed", json!({"verdict": verdict}))
                            .map_err(|e| e.to_string())?;
                        emit(json!({"ev": "verify.completed", "verdict": verdict}));
                        if !verdict.passed {
                            if verify_rounds >= self.max_verify_rounds {
                                return Ok(
                                    json!({"done": false, "reason": "verify_failed", "verdict": verdict}),
                                );
                            }
                            verify_rounds += 1;
                            self.messages.push(json!({
                                "role": "user",
                                "content": format!(
                                    "External acceptance check failed. Fix the implementation, not the check.\n{}",
                                    verdict.output
                                )
                            }));
                            self.record
                                .append("messages.checkpoint", json!({"messages": self.messages}))
                                .map_err(|e| e.to_string())?;
                            continue;
                        }
                        if self.journal.is_none() && self.candidate.is_none() {
                            return Ok(json!({
                                "done": true,
                                "reason": "final",
                                "text": result.text,
                                "verified": true
                            }));
                        }
                    }

                    if self.journal.is_some() != self.candidate.is_some() {
                        return Err(
                            "differential verification requires both journal and candidate".into(),
                        );
                    }
                    let (Some(journal), Some(candidate)) = (&self.journal, &self.candidate) else {
                        return Ok(json!({
                            "done": true,
                            "reason": "final",
                            "text": result.text,
                            "verified": Value::Null,
                            "why": "no journal supplied; unverified result"
                        }));
                    };

                    let verdict = gate(journal, candidate, 1, None, self.min_branch, None);
                    let kind = if verdict.ok {
                        "verify.passed"
                    } else {
                        "verify.failed"
                    };
                    self.record
                        .append(kind, json!({"verdict": verdict}))
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": kind, "verdict": verdict}));

                    if verdict.ok {
                        return Ok(json!({
                            "done": true,
                            "reason": "final",
                            "text": result.text,
                            "verified": true
                        }));
                    }
                    if verify_rounds >= self.max_verify_rounds {
                        return Ok(json!({
                            "done": false,
                            "reason": "verify_failed",
                            "verdict": verdict
                        }));
                    }
                    verify_rounds += 1;
                    self.messages.push(json!({
                        "role": "user",
                        "content": format!(
                            "Replay disagrees with the recorded oracle. Fix the code, not the journal:\n{}",
                            serde_json::to_string_pretty(&verdict).unwrap_or_default()
                        )
                    }));
                }
            }

            "approval.list" => Ok(json!({"pending": self.pending})),

            "approval.resolve" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("approval.resolve needs id")?;
                let allow = params
                    .get("allow")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let remember = params
                    .get("remember")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let at = self
                    .pending
                    .iter()
                    .position(|p| p["id"] == id)
                    .ok_or_else(|| format!("no pending approval {id:?}"))?;
                let pending = self.pending.remove(at);

                if pending["recovered"] == true {
                    let history = self.record.tail(usize::MAX).map_err(|e| e.to_string())?;
                    let unresolved = history.iter().any(|r| {
                        ["approval.requested", "tool.started"]
                            .contains(&r["kind"].as_str().unwrap_or(""))
                            && r["id"] == id
                    }) && !history
                        .iter()
                        .any(|r| r["kind"] == "tool.result" && r["id"] == id);
                    if !unresolved {
                        return Err("approval outcome is already recorded".into());
                    }
                    let clipped = self.tools.clip(RESOLVED_UNKNOWN_MSG);
                    let message =
                        json!({"role": "tool", "tool_call_id": id, "content": clipped});
                    if let Some(existing) = self
                        .messages
                        .iter_mut()
                        .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
                    {
                        *existing = message;
                    } else {
                        self.messages.push(message);
                    }
                    self.record
                        .append("messages.checkpoint", json!({"messages": self.messages}))
                        .map_err(|e| e.to_string())?;
                    self.record
                        .append(
                            "approval.resolved",
                            json!({"id": id, "allow": allow, "recovered": true, "replayed": false, "output": RESOLVED_UNKNOWN_MSG}),
                        )
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "approval.resolved", "id": id, "allow": allow, "recovered": true, "replayed": false}));
                    return Ok(
                        json!({"id": id, "allow": allow, "recovered": true, "replayed": false, "output": clipped}),
                    );
                }

                // Same-session active approval paused in non-interactive mode:
                // the command never started, so execute once on allow or record denial.
                let name = pending["name"].as_str().unwrap_or("bash").to_owned();
                let args = pending["args"].clone();
                if allow && remember && matches!(name.as_str(), "bash" | "task") {
                    let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                    let pattern = self.policy.allow_for_session(cmd);
                    self.record
                        .append("policy.allowed", json!({"pattern": pattern}))
                        .map_err(|e| e.to_string())?;
                    emit(json!({"ev": "policy.allowed", "pattern": pattern}));
                }
                self.record
                    .append(
                        "approval.resolved",
                        json!({"id": id, "allow": allow, "remember": remember}),
                    )
                    .map_err(|e| e.to_string())?;
                emit(json!({"ev": "approval.resolved", "id": id, "allow": allow, "remember": remember}));

                let output = if !allow {
                    self.record
                        .append(
                            "tool.result",
                            json!({"id": id, "tool": name, "args": args, "tier": "deny", "output": "denied by user", "exit_code": null}),
                        )
                        .map_err(|e| e.to_string())?;
                    let message =
                        json!({"role": "tool", "tool_call_id": id, "content": "denied by user"});
                    self.messages.push(message.clone());
                    self.record
                        .append("message", json!({"message": message}))
                        .map_err(|e| e.to_string())?;
                    "denied by user".to_owned()
                } else {
                    let call = ToolCall {
                        id: id.to_owned(),
                        name,
                        args,
                    };
                    let audit = &mut self.audit;
                    let mut sink = |c: &ToolCall, out: &str| {
                        audit.push(json!({"id": c.id, "args": c.args, "bytes": out.len()}));
                    };
                    execute_recorded_call(
                        ExecEnv {
                            tools: &mut self.tools,
                            guard: &mut self.guard,
                            record: &mut self.record,
                            messages: &mut self.messages,
                            control: &self.control,
                        },
                        &call,
                        Tier::Deny,
                        &mut sink,
                        emit,
                    )?
                };
                self.drain_deferred_calls(emit)?;
                self.record
                    .append("messages.checkpoint", json!({"messages": self.messages}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "id": id,
                    "allow": allow,
                    "recovered": false,
                    "replayed": false,
                    "output": output
                }))
            }

            "record.verify" => match Record::verify_chain(self.record.path()) {
                Ok(entries) => Ok(json!({"ok": true, "entries": entries})),
                Err((index, why)) => Ok(json!({"ok": false, "bad_index": index, "why": why})),
            },

            "session.verify" => {
                let chain = match Record::verify_chain(self.record.path()) {
                    Ok(entries) => json!({"ok": true, "entries": entries}),
                    Err((index, why)) => json!({"ok": false, "bad_index": index, "why": why}),
                };
                let (samples, stall, digest) = self.guard.report();
                let (hits, misses, size) = self.policy.stats();
                Ok(json!({
                    "chain": chain,
                    "guard": {"samples": samples, "stall": stall, "digest": digest},
                    "policy": {"hits": hits, "misses": misses, "size": size},
                    "plan": self.plan_mode,
                    "read_only": self.tools.is_read_only(),
                }))
            }

            "record.tail" => {
                let n = params.get("n").and_then(Value::as_u64).unwrap_or(20) as usize;
                Ok(json!({"rows": self.record.tail(n).map_err(|e| e.to_string())?}))
            }

            "policy.stats" => {
                let (hits, misses, size) = self.policy.stats();
                Ok(json!({"hits": hits, "misses": misses, "size": size}))
            }

            "guard.report" => {
                let (samples, stall, digest) = self.guard.report();
                Ok(json!({
                    "samples": samples,
                    "stall": stall,
                    "digest": digest,
                    "worktree": self.worktree.to_string_lossy()
                }))
            }

            "audit.drain" => {
                let drained = std::mem::take(&mut self.audit);
                Ok(json!({"rows": drained}))
            }

            "edit.undo" => match self.tools.undo_last_edit() {
                Ok(summary) => {
                    self.record
                        .append("edit.undone", json!({"summary": summary}))
                        .map_err(|e| e.to_string())?;
                    Ok(json!({"ok": true, "summary": summary}))
                }
                Err(e) => Err(e),
            },

            other => Err(format!("unknown method {other:?}")),
        }
    }

    pub fn serve(
        &mut self,
        input: impl BufRead + Send,
        out: impl Write + Send,
    ) -> std::io::Result<()> {
        self.interactive = true;
        let output = Arc::new(Mutex::new(out));
        let control = self.control.clone();
        let (tx, rx) = mpsc::channel::<Value>();
        let outcome = {
            let daemon = &mut *self;
            std::thread::scope(|scope| {
                let worker_output = output.clone();
                let worker = scope.spawn(move || -> std::io::Result<()> {
                    for req in rx {
                        let id = req["id"].clone();
                        let method = req["method"].as_str().unwrap_or("");
                        if method == "turn" {
                            daemon.control.reset();
                        }
                        let mut emit = |v: Value| {
                            if let Ok(mut out) = worker_output.lock() {
                                let _ = writeln!(out, "{v}");
                                let _ = out.flush();
                            }
                        };
                        let result = daemon.dispatch(method, &req["params"], &mut emit);
                        let reply = match result {
                            Ok(v) => json!({"id": id, "result": v}),
                            Err(e) => json!({"id": id, "error": e}),
                        };
                        let mut out = worker_output
                            .lock()
                            .map_err(|_| std::io::Error::other("output lock"))?;
                        writeln!(out, "{reply}")?;
                        out.flush()?;
                    }
                    Ok(())
                });
                for line in input.lines() {
                    let line = line?;
                    let req: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            let mut out = output.lock().unwrap();
                            writeln!(out, "{}", json!({"error": e.to_string()}))?;
                            out.flush()?;
                            continue;
                        }
                    };
                    let params = &req["params"];
                    let immediate: Option<Result<Value, String>> =
                        match req["method"].as_str().unwrap_or("") {
                            "cancel" => {
                                control.cancel();
                                Some(Ok(json!({"cancelled": true})))
                            }
                            "steer" => {
                                let text = params["input"].as_str().unwrap_or("");
                                if text.trim().is_empty() {
                                    Some(Err("steer requires nonempty input".into()))
                                } else {
                                    control.steer(text.to_owned());
                                    Some(Ok(json!({"queued": true})))
                                }
                            }
                            "approval.resolve"
                                if control.waiting().as_deref() == params["id"].as_str() =>
                            {
                                control.resolve_with_scope(
                                    params["id"].as_str().unwrap_or("").into(),
                                    params["allow"].as_bool().unwrap_or(false),
                                    params["remember"].as_bool().unwrap_or(false),
                                );
                                Some(Ok(json!({"queued": true})))
                            }
                            "ping" => Some(Ok(json!({"pong": true}))),
                            _ => None,
                        };
                    if let Some(reply) = immediate {
                        let mut out = output.lock().unwrap();
                        let line = match reply {
                            Ok(result) => json!({"id": req["id"], "result": result}),
                            Err(error) => json!({"id": req["id"], "error": error}),
                        };
                        writeln!(out, "{line}")?;
                        out.flush()?;
                    } else if tx.send(req).is_err() {
                        break;
                    }
                }
                control.close_input();
                drop(tx);
                worker
                    .join()
                    .map_err(|_| std::io::Error::other("session worker panicked"))?
            })
        };
        self.interactive = false;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stub::{call, StubLlm};

    #[test]
    fn approving_recovered_call_never_replays_unknown_side_effect() {
        let dir =
            std::env::temp_dir().join(format!("twig-daemon-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");
        {
            let mut record = Record::open(runs.join("resume.jsonl"), "resume").unwrap();
            let assistant = json!({"role":"assistant","content":"","tool_calls":[{"id":"pending","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf executed > result\"}"}}]});
            record
                .append("message", json!({"message": assistant.clone()}))
                .unwrap();
            record
                .append(
                    "approval.requested",
                    json!({"id":"pending","name":"bash","args":{"command":"printf executed > result"}}),
                )
                .unwrap();
            record
                .append("messages.checkpoint", json!({"messages": [assistant]}))
                .unwrap();
        }
        let model = StubLlm::new(vec![]);
        let mut daemon =
            Daemon::new(&model, &dir, &runs, "resume", &HarnessConfig::default()).unwrap();
        let out = daemon
            .dispatch(
                "approval.resolve",
                &json!({"id": "pending", "allow": true}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(out["recovered"], true);
        assert_eq!(out["replayed"], false);
        assert!(!dir.join("result").exists());
        assert_eq!(
            daemon
                .dispatch("approval.list", &json!({}), &mut |_| {})
                .unwrap()["pending"],
            json!([])
        );
        assert!(Record::verify_chain(daemon.record.path()).is_ok());
    }

    #[test]
    fn recovered_approval_acknowledgement_survives_restart_once() {
        let dir = std::env::temp_dir().join(format!("twig-daemon-ack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");
        {
            let mut record = Record::open(runs.join("resume.jsonl"), "resume").unwrap();
            let assistant = json!({"role":"assistant","content":"","tool_calls":[{"id":"pending","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf executed > result\"}"}}]});
            record
                .append("message", json!({"message": assistant.clone()}))
                .unwrap();
            record
                .append(
                    "approval.requested",
                    json!({"id":"pending","name":"bash","args":{"command":"printf executed > result"}}),
                )
                .unwrap();
            record
                .append("messages.checkpoint", json!({"messages": [assistant]}))
                .unwrap();
        }
        let model = StubLlm::new(vec![]);
        let mut daemon =
            Daemon::new(&model, &dir, &runs, "resume", &HarnessConfig::default()).unwrap();
        daemon
            .dispatch(
                "approval.resolve",
                &json!({"id": "pending", "allow": true}),
                &mut |_| {},
            )
            .unwrap();
        drop(daemon);
        let resumed =
            Daemon::new(&model, &dir, &runs, "resume", &HarnessConfig::default()).unwrap();
        assert_eq!(
            resumed
                .messages
                .iter()
                .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "pending")
                .count(),
            1
        );
        assert!(!dir.join("result").exists());
    }

    #[test]
    fn approved_started_crash_before_result_stays_unknown_and_never_replays() {
        let dir = std::env::temp_dir().join(format!(
            "twig-daemon-approved-started-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");
        {
            let mut record = Record::open(runs.join("crash.jsonl"), "crash").unwrap();
            let assistant = json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf ran > side_effect\"}"}}]});
            record
                .append("message", json!({"message": assistant.clone()}))
                .unwrap();
            record
                .append(
                    "approval.requested",
                    json!({"id":"c1","name":"bash","args":{"command":"printf ran > side_effect"}}),
                )
                .unwrap();
            record
                .append("approval.resolved", json!({"id":"c1","allow":true}))
                .unwrap();
            record
                .append(
                    "tool.started",
                    json!({"id":"c1","tool":"bash","args":{"command":"printf ran > side_effect"}}),
                )
                .unwrap();
            record
                .append("messages.checkpoint", json!({"messages": [assistant]}))
                .unwrap();
        }
        let model = StubLlm::new(vec![]);
        let mut daemon =
            Daemon::new(&model, &dir, &runs, "crash", &HarnessConfig::default()).unwrap();
        let list = daemon
            .dispatch("approval.list", &json!({}), &mut |_| {})
            .unwrap();
        assert_eq!(list["pending"].as_array().unwrap().len(), 1);
        assert_eq!(list["pending"][0]["recovered"], true);
        let out = daemon
            .dispatch(
                "approval.resolve",
                &json!({"id": "c1", "allow": true}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(out["recovered"], true);
        assert_eq!(out["replayed"], false);
        assert!(!dir.join("side_effect").exists());
    }

    #[test]
    fn active_noninteractive_approval_resolves_and_drains_deferred_calls() {
        let dir =
            std::env::temp_dir().join(format!("twig-daemon-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
        let runs = dir.join("runs");
        let llm = StubLlm::new(vec![
            StubLlm::with_tools(
                "",
                vec![
                    call("a", "read", json!({"path": "note.txt"})),
                    call("d", "bash", json!({"command": "rm -rf /"})),
                    call("e", "bash", json!({"command": "printf ok > after.txt"})),
                ],
            ),
            StubLlm::text("done"),
        ]);
        let mut daemon =
            Daemon::new(&llm, &dir, &runs, "active", &HarnessConfig::default()).unwrap();
        let paused = daemon
            .dispatch("turn", &json!({"input": "go"}), &mut |_| {})
            .unwrap();
        assert_eq!(paused["reason"], "awaiting_approval");
        let resolved = daemon
            .dispatch(
                "approval.resolve",
                &json!({"id": "d", "allow": false}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(resolved["allow"], false);
        assert_eq!(resolved["recovered"], false);
        assert!(dir.join("after.txt").exists());
        context::validate_tool_pairs(&daemon.messages).unwrap();
        assert_eq!(
            daemon
                .dispatch("approval.list", &json!({}), &mut |_| {})
                .unwrap()["pending"],
            json!([])
        );
    }

    #[test]
    fn mode_set_plan_toggles_read_only_persists_and_session_verify_reports_integrity() {
        let dir = std::env::temp_dir().join(format!("twig-daemon-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");
        let llm = StubLlm::new(vec![]);
        let mut daemon =
            Daemon::new(&llm, &dir, &runs, "plan-test", &HarnessConfig::default()).unwrap();

        let on = daemon
            .dispatch("mode.set", &json!({"plan": true}), &mut |_| {})
            .unwrap();
        assert_eq!(on["plan"], true);
        assert_eq!(on["read_only"], true);

        let verify = daemon
            .dispatch("session.verify", &json!({}), &mut |_| {})
            .unwrap();
        assert_eq!(verify["chain"]["ok"], true);
        assert_eq!(verify["plan"], true);
        assert!(verify["guard"]["digest"].is_string());

        // Plan mode persists across restart via mode.changed record entry.
        drop(daemon);
        let mut resumed =
            Daemon::new(&llm, &dir, &runs, "plan-test", &HarnessConfig::default()).unwrap();
        let ping = resumed.dispatch("ping", &json!({}), &mut |_| {}).unwrap();
        assert_eq!(ping["plan"], true);
        assert_eq!(ping["read_only"], true);

        // Guard narrowing cannot be cleared by turning plan mode off.
        resumed.tools.restrict_read_only();
        let off = resumed
            .dispatch("mode.set", &json!({"plan": false}), &mut |_| {})
            .unwrap();
        assert_eq!(off["plan"], false);
        assert_eq!(off["read_only"], true);

        // Effort, fork, rewind, and session policy allow work end-to-end.
        let eff = resumed
            .dispatch("effort.set", &json!({"effort": "high"}), &mut |_| {})
            .unwrap();
        assert_eq!(eff["effort"], "high");

        let pat = resumed.policy.allow_for_session("git reset --hard HEAD");
        assert_eq!(pat, "git reset --hard *");
        assert_eq!(
            resumed.policy.classify("git reset --hard origin/main"),
            Tier::Audit
        );

        resumed.messages.push(json!({"role": "user", "content": "one"}));
        resumed.messages.push(json!({"role": "assistant", "content": "a1"}));
        resumed.messages.push(json!({"role": "user", "content": "two"}));
        resumed.messages.push(json!({"role": "assistant", "content": "a2"}));

        let forked = resumed
            .dispatch("session.fork", &json!({}), &mut |_| {})
            .unwrap();
        let fork_sid = forked["sid"].as_str().unwrap();
        assert!(Record::verify_chain(&runs.join(format!("{fork_sid}.jsonl"))).is_ok());

        let rewound = resumed
            .dispatch("session.rewind", &json!({"turns": 1}), &mut |_| {})
            .unwrap();
        assert_eq!(rewound["rewound_turns"], 1);
        assert_eq!(resumed.messages.len(), 2);
    }
}
