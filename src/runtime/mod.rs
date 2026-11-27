//! Agent session runtime: JSON-RPC daemon, turn execution, policy, guard,
//! context compaction, playbooks/skills, subagents, and terminal client.

pub mod context;
mod git_ops;
pub mod guard;
pub mod guidance;
pub mod mcp;
pub mod policy;
pub mod session;
pub mod subagent;
pub mod tasks;
pub mod tools;
pub mod turn;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use serde_json::{json, Value};

use self::context::Context;
use self::guard::Guard;
use self::guidance::{inject_playbook, route, Playbook, SkillStore};
use self::policy::{Policy, Tier};
use self::session::{
    append_repo_instructions, capture_worktree_snapshot, detect_model_family, load_child_threads,
    load_session_patch, notify_herdr, recover_session, restore_worktree_snapshot,
    RESOLVED_UNKNOWN_MSG,
};
use self::tools::Tools;
use self::turn::{execute_recorded_call, run_turn_live, ExecEnv, TurnCtx};
use crate::config::HarnessConfig;
use crate::control::Control;
use crate::exec::{run_check, Check};
use crate::llm::{Llm, ToolCall};
use crate::record::{Lease, Record};
use crate::verify::{gate, Candidate};

fn strip_transient_reminders(messages: &mut Vec<Value>) {
    messages.retain(|m| {
        !(m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.trim_start().starts_with("<system-reminder>")))
    });
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
    check_on_change: Option<Check>,
    stop_hook: Option<Check>,
    model_override: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    plan_mode: bool,
    operator_mode: bool,
    study_mode: bool,
    had_checkpoint: bool,
    snapshots: Vec<String>,
    frozen_system: Option<(String, String)>,
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
        let check_on_change = match project.get("check_on_change") {
            Some(Value::String(cmd)) if !cmd.trim().is_empty() => Some(Check {
                command: cmd.trim().to_owned(),
                timeout_ms: 120_000,
                max_output_bytes: 1_048_576,
            }),
            Some(obj) if obj.is_object() => Some(
                serde_json::from_value(obj.clone())
                    .map_err(|e| format!("project check_on_change: {e}"))?,
            ),
            _ => None,
        };
        let stop_hook = match project.get("stop_hook") {
            Some(Value::String(cmd)) if !cmd.trim().is_empty() => Some(Check {
                command: cmd.trim().to_owned(),
                timeout_ms: 30_000,
                max_output_bytes: 32_768,
            }),
            Some(obj) if obj.is_object() => Some(
                serde_json::from_value(obj.clone())
                    .map_err(|e| format!("project stop_hook: {e}"))?,
            ),
            _ => None,
        };
        let (record, rows) =
            Record::open_with_rows(runs_dir.join(format!("{sid}.jsonl")), sid.clone())
                .map_err(|e| e.to_string())?;
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
            Tools::with_config(&worktree, cfg).with_session_context(runs_dir.join("artifacts"), &sid);
        tools.restore_read_stamps(&rows);
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
            check_on_change,
            stop_hook,
            model_override: None,
            effort: initial_effort,
            title,
            plan_mode,
            operator_mode: recovered.operator_mode,
            study_mode: recovered.study_mode,
            had_checkpoint: recovered.had_checkpoint,
            snapshots: recovered.snapshots,
            frozen_system: None,
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

    fn ensure_session_meta(&mut self, role: &str, model: &str) -> Result<(), String> {
        if !self.record.is_empty() {
            return Ok(());
        }
        let canon = self
            .worktree
            .canonicalize()
            .unwrap_or_else(|_| self.worktree.clone());
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty());
        let parent_sid = std::env::var("TWIG_PARENT_SID").ok().filter(|s| !s.is_empty());
        self.record
            .append(
                "session.meta",
                json!({
                    "worktree": canon.display().to_string(),
                    "branch": branch,
                    "parent_sid": parent_sid,
                    "role": role,
                    "model": model,
                }),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn frozen_system_for(&mut self, model: &str) -> String {
        let fam_key = detect_model_family(model).unwrap_or("default").to_owned();
        if let Some((ref cached_key, ref prompt)) = self.frozen_system {
            if cached_key == &fam_key {
                return prompt.clone();
            }
        }
        let mut system = self.system.clone();
        append_repo_instructions(&self.worktree, model, &mut system);
        system.push_str(&self.tools.static_system_addendum());
        self.frozen_system = Some((fam_key, system.clone()));
        system
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
                    "gate_edits": self.tools.gates_edits(),
                    "operator": self.operator_mode,
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
                    "gate_edits": self.tools.gates_edits(),
                    "operator": self.operator_mode,
                    "study": self.study_mode,
                    "read_only": self.tools.is_read_only(),
                    "turns": turns,
                    "messages": self.messages,
                    "todos": self.todos,
                    "child_threads": load_child_threads(&self.sid(), Some(&self.runs_dir.join("subagents"))),
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
                let requested = params.get("model").and_then(Value::as_str).map(str::trim).unwrap_or("");
                self.model_override = (!requested.is_empty() && requested != "reset" && requested != "default").then(|| requested.to_owned());
                let _ = crate::llm::Provider::configure(&json!({"action": "save_model", "model": self.model_override}));
                let active = self.model_override.clone().unwrap_or_else(|| self.llm.roles().get("code").to_string());
                let env_override = std::env::var_os("TWIG_MODEL").is_some() || std::env::var_os("TWIG_ROLES").is_some();
                self.frozen_system = None;
                Ok(json!({"model": active, "env_override": env_override}))
            }

            "roles.get" => crate::llm::Provider::configure(&json!({"action": "get_roles"})),

            "roles.set" => {
                let action = params.get("action").and_then(Value::as_str).unwrap_or("set");
                let res = if action == "reset" {
                    crate::llm::Provider::configure(&json!({"action": "reset_roles"}))?
                } else {
                    crate::llm::Provider::configure(&json!({
                        "action": "set_role",
                        "role": params.get("role"),
                        "model": params.get("model"),
                        "effort": params.get("effort"),
                    }))?
                };
                self.frozen_system = None;
                Ok(res)
            }

            "mode.operator" => {
                let next = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(!self.operator_mode);
                self.operator_mode = next;
                strip_transient_reminders(&mut self.messages);
                self.record
                    .append("operator.changed", json!({"enabled": next}))
                    .map_err(|e| e.to_string())?;
                emit(json!({"ev": "operator.changed", "enabled": next}));
                Ok(json!({"operator": next}))
            }

            "mode.study" => {
                let next = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(!self.study_mode);
                self.study_mode = next;
                strip_transient_reminders(&mut self.messages);
                self.record
                    .append("study.changed", json!({"enabled": next}))
                    .map_err(|e| e.to_string())?;
                emit(json!({"ev": "study.changed", "enabled": next}));
                Ok(json!({"study": next}))
            }

            "session.btw" => {
                let question = params
                    .get("question")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or("session.btw requires a non-empty question")?;
                let default_role = if self.plan_mode { "plan" } else { "code" };
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get(default_role).to_string());
                let system = self.frozen_system_for(&model);
                let mut ephemeral = self.messages.clone();
                ephemeral.push(json!({
                    "role": "user",
                    "content": format!(
                        "[Ephemeral side question (/btw) — answer concisely using current session context. Do NOT propose unrequested edits; this Q&A will not be saved to history.]\n{question}"
                    )
                }));
                let reply = self.llm.complete(&model, &system, &ephemeral, None)?;
                Ok(json!({
                    "question": question,
                    "answer": reply.text,
                    "model": model,
                    "usage": reply.usage,
                }))
            }

            "session.apply" => {
                let sid = params
                    .get("sid")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or("session.apply requires a non-empty `sid`")?;
                let artifacts = self.runs_dir.join("artifacts");
                let patch = load_session_patch(sid, Some(&artifacts))?;
                let summary = self
                    .tools
                    .apply_git_patch(&patch, &format!("session:{sid}"))?;
                self.record
                    .append("session.applied", json!({"sid": sid, "summary": summary}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"sid": sid, "summary": summary}))
            }

            "effort.set" => {
                let raw = params.get("effort").and_then(Value::as_str).unwrap_or("");
                let next = crate::llm::normalize_effort(raw)?;
                self.effort = next.clone();
                self.llm.set_effort(next.as_deref());
                let _ = crate::llm::Provider::configure(
                    &json!({"action": "save_effort", "effort": next}),
                );
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
                let canon = self
                    .worktree
                    .canonicalize()
                    .unwrap_or_else(|_| self.worktree.clone());
                let model = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                forked
                    .append(
                        "session.meta",
                        json!({
                            "worktree": canon.display().to_string(),
                            "parent_sid": self.sid(),
                            "role": "code",
                            "model": model,
                        }),
                    )
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
                let restore_code = params
                    .get("restore_code")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
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
                let mut code_restored: Option<String> = None;
                if restore_code {
                    if let Some(tree_oid) = self.snapshots.get(cut_pos).cloned() {
                        code_restored = Some(restore_worktree_snapshot(&self.worktree, &tree_oid)?);
                    } else {
                        code_restored = Some("no git snapshot available for that turn".into());
                    }
                }
                if cut_pos < self.snapshots.len() {
                    self.snapshots.truncate(cut_pos);
                }
                self.messages.truncate(keep_len);
                self.pending.clear();
                self.deferred_calls.clear();
                self.guard.reset_transient();
                if self.messages.is_empty() {
                    self.todos.clear();
                }
                self.had_checkpoint = true;
                self.record
                    .append(
                        "session.rewound",
                        json!({"turns": rewound_turns, "restore_code": restore_code}),
                    )
                    .map_err(|e| e.to_string())?;
                self.record
                    .append("messages.checkpoint", json!({"messages": self.messages}))
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "rewound_turns": rewound_turns,
                    "code_restored": code_restored,
                    "messages": self.messages,
                    "todos": self.todos,
                    "context_bytes": self.context.used_bytes(&self.messages),
                    "context_max": self.context.max_bytes(),
                    "running_tasks": self.tools.running_tasks_count(),
                    "env_summary": self.tools.env_summary(),
                }))
            }

            "mcp.list" => {
                let res = self.tools.list_mcp(params)?;
                if params.get("action").and_then(Value::as_str).unwrap_or("list") != "list" {
                    self.frozen_system = None;
                }
                Ok(res)
            }
            "tasks.list" => Ok(self.tools.list_tasks()),
            "tasks.cancel" => self.tools.cancel_task(params),
            "blackboard" => Ok(self.tools.blackboard_rpc(params)),
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
                        self.frozen_system = None;
                        Ok(json!({"action": "enable", "ok": true, "report": msg, "env_summary": self.tools.env_summary()}))
                    }
                    "disable" => {
                        let name = params["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or("disable requires a skill name")?;
                        let msg = store.disable(name, project)?;
                        self.tools.reload_skills();
                        self.frozen_system = None;
                        Ok(json!({"action": "disable", "ok": true, "report": msg, "env_summary": self.tools.env_summary()}))
                    }
                    other => Err(format!("unknown skill action: {other}")),
                }
            }

            "mode.set" => {
                let next = params.get("plan").and_then(Value::as_bool).unwrap_or(!self.plan_mode);
                let gate = params.get("gate_edits").and_then(Value::as_bool).unwrap_or_else(|| self.tools.gates_edits());
                self.plan_mode = next;
                strip_transient_reminders(&mut self.messages);
                let (read_only, gate_edits) = self.tools.configure_mode(next, gate);
                self.record.append("mode.changed", json!({"plan": next, "gate_edits": gate_edits})).map_err(|e| e.to_string())?;
                emit(json!({"ev": "mode.changed", "plan": next, "gate_edits": gate_edits, "read_only": read_only}));
                Ok(json!({"plan": next, "gate_edits": gate_edits, "read_only": read_only}))
            }

            "git.commit" => {
                let hint = params.get("hint").and_then(Value::as_str);
                let res = git_ops::run_smart_commit(&self.worktree, self.llm, hint)?;
                self.record
                    .append("git.committed", res.clone())
                    .map_err(|e| e.to_string())?;
                Ok(res)
            }

            "git.pr" => {
                let arg = params.get("arg").and_then(Value::as_str);
                let res = git_ops::ship_pull_request(&self.worktree, self.llm, arg)?;
                self.record
                    .append("git.pr_created", res.clone())
                    .map_err(|e| e.to_string())?;
                Ok(res)
            }

            "git.worktree" => {
                let arg = params.get("arg").and_then(Value::as_str);
                let mut res = git_ops::handoff_worktree(&self.worktree, arg)?;
                if res["action"] == "switched" {
                    if let Some(wt_str) = res["worktree"].as_str() {
                        let next_wt = PathBuf::from(wt_str);
                        let (cfg, _) = HarnessConfig::resolve().unwrap_or_default();
                        let mut next_tools = Tools::with_config(&next_wt, &cfg)
                            .with_session_context(self.runs_dir.join("artifacts"), self.sid());
                        let _ = next_tools.apply_plan_mode(self.plan_mode);
                        self.worktree = next_wt;
                        self.tools = next_tools;
                        self.frozen_system = None;
                        let model = self
                            .model_override
                            .clone()
                            .unwrap_or_else(|| self.llm.roles().get("code").to_string());
                        let _ = self.record.append(
                            "session.meta",
                            json!({
                                "worktree": wt_str,
                                "branch": res["branch"],
                                "role": if self.plan_mode { "plan" } else { "code" },
                                "model": model,
                            }),
                        );
                        res["env_summary"] = json!(self.tools.env_summary());
                    }
                }
                Ok(res)
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
                let default_role = if self.plan_mode { "plan" } else { "code" };
                let role = params
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or(default_role);
                let resolved_role = crate::llm::Provider::configure(
                    &json!({"action": "resolve_role", "role": role}),
                )
                .unwrap_or_else(|_| json!({}));
                let model = self.model_override.clone().unwrap_or_else(|| {
                    if std::env::var_os("TWIG_MODEL").is_none()
                        && std::env::var_os("TWIG_ROLES").is_none()
                    {
                        if let Some(m) = resolved_role.get("model").and_then(Value::as_str) {
                            return m.to_owned();
                        }
                    }
                    self.llm.roles().get(role).to_string()
                });
                if self.effort.is_none() {
                    let role_eff = resolved_role
                        .get("effort")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.llm.role_effort(role));
                    self.llm.set_effort(role_eff.as_deref());
                }
                if !self.pending.is_empty() {
                    return Err("pending tool approval must be resolved first; explicitly /approve ID or /deny ID".into());
                }

                notify_herdr("running", input);
                let _keep_awake = git_ops::KeepAwakeGuard::acquire();
                self.ensure_session_meta(role, &model)?;
                if let Some(tree_oid) = capture_worktree_snapshot(&self.worktree) {
                    self.snapshots.push(tree_oid.clone());
                    let _ = self
                        .record
                        .append("turn.snapshot", json!({"tree": tree_oid}));
                } else {
                    self.snapshots.push(String::new());
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
                strip_transient_reminders(&mut self.messages);
                self.guard.reset_transient();
                self.tools.clear_guard_narrow();
                self.messages.push(user_msg.clone());
                self.record
                    .append("message", json!({"message": user_msg}))
                    .map_err(|e| e.to_string())?;
                self.context.sync_constraints(&mut self.messages);

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

                // Keep `system` frozen across turns for KV prompt-cache prefix hits;
                // emit turn-specific guidance (`Playbook`, `$skill`, `/plan`, `/operator`) as a tail reminder.
                let system = self.frozen_system_for(&model);
                let mut reminders: Vec<String> = Vec::new();
                if let Some(book) = route(input, &self.playbooks) {
                    let mut pb = String::new();
                    inject_playbook(&mut pb, book, "");
                    if !pb.trim().is_empty() {
                        reminders.push(pb.trim().to_owned());
                    }
                    emit(json!({"ev": "playbook.selected", "name": book.name, "steps": book.steps}));
                }
                reminders.extend(self.tools.dynamic_turn_reminders(input));
                if self.operator_mode {
                    reminders.push(
                        "Operator mode is active (RCI Split-Fact Collective Discipline): handle small (<50-line) edits directly; never run shared-information debate across identical subagents (stays <= equal-budget majority vote). Partition disjoint `slice`s across parallel subagents (`scout`/`code`/`test`), forward verbatim `path:line` facts via the session Blackboard, and synthesize/gate with `advisor` or `reviewer`.".into(),
                    );
                }
                if self.study_mode {
                    reminders.push(
                        "REA Study Mode is active (Dynamic + Static Clean-Room Comprehension & Epistemic Provenance): \
                         combine structural inspection (`read` binary/Wasm/Hex/.har summaries, `outline` xrefs/deps/LSP) with active probing (test vectors, `twig verify record`, tracing) to turn hypotheses into verified facts. \
                         Tag claims with four epistemic tiers: `[Observed]` (measured fact with `file:line`, header, or I/O sample), `[Derived]` (logical consequence), `[Inferred]` (unproven hypothesis), `[Unknown / Limitation]` (never fabricate unseen behavior). \
                         For learning/own-code or clean-room reconstruction, explain modification rationale clearly and use `SPEC.md` + `journal.jsonl` + `twig verify` to drive closed-loop reconstruction on `/go`.".into(),
                    );
                }
                let skill_store = SkillStore::default_for(Some(&self.worktree));
                for (name, scope, body) in skill_store.resolve_explicit_invocations(input) {
                    reminders.push(format!("## Explicitly invoked skill: ${name} ({scope})\n{body}"));
                    emit(json!({"ev": "skill.loaded", "name": name, "scope": scope}));
                }
                if !reminders.is_empty() {
                    let rem_msg = json!({
                        "role": "system",
                        "content": format!("<system-reminder>\n{}\n</system-reminder>", reminders.join("\n\n"))
                    });
                    self.messages.push(rem_msg.clone());
                    self.record
                        .append("message", json!({"message": rem_msg}))
                        .map_err(|e| e.to_string())?;
                }

                let turn_start_digest = self
                    .check_on_change
                    .is_some()
                    .then(|| self.guard.digest());
                let mut verify_rounds = 0u32;
                loop {
                    if let Some(compaction) =
                        self.context
                            .compact(&mut self.messages, self.llm, &model)?
                    {
                        self.had_checkpoint = true;
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
                    if self.had_checkpoint {
                        self.record
                            .append("messages.checkpoint", json!({"messages": self.messages}))
                            .map_err(|e| e.to_string())?;
                    }
                    if ["budget", "cancelled", "awaiting_approval"].contains(&result.reason) {
                        if result.reason == "awaiting_approval" {
                            self.had_checkpoint = true;
                            notify_herdr("waiting_input", "awaiting tool approval");
                        } else {
                            notify_herdr("idle", result.reason);
                        }
                        return Ok(json!({"done": true, "reason": result.reason}));
                    }
                    if !self.plan_mode {
                        if let Some(hook) = &self.stop_hook {
                            let h_verdict =
                                run_check(&self.worktree, hook).map_err(|e| e.to_string())?;
                            if h_verdict.exit_code == Some(2) {
                                self.record
                                    .append("hook.blocked", json!({"verdict": h_verdict}))
                                    .map_err(|e| e.to_string())?;
                                emit(json!({"ev": "hook.blocked", "command": hook.command, "output": h_verdict.output}));
                                if verify_rounds >= self.max_verify_rounds {
                                    return Ok(json!({
                                        "done": false,
                                        "reason": "stop_hook_blocked",
                                        "verdict": h_verdict,
                                    }));
                                }
                                verify_rounds += 1;
                                let msg = json!({
                                    "role": "user",
                                    "content": format!(
                                        "Stop hook blocked completion (exit 2 from `{}`):\n{}\nResolve this before finishing.",
                                        hook.command,
                                        h_verdict.output.trim()
                                    )
                                });
                                self.messages.push(msg.clone());
                                self.had_checkpoint = true;
                                self.record
                                    .append("message", json!({"message": msg}))
                                    .map_err(|e| e.to_string())?;
                                self.record
                                    .append("messages.checkpoint", json!({"messages": self.messages}))
                                    .map_err(|e| e.to_string())?;
                                continue;
                            } else if !h_verdict.passed {
                                emit(json!({"ev": "hook.warn", "command": hook.command, "exit_code": h_verdict.exit_code}));
                            }
                        }
                    }
                    let effective_check = self.check.as_ref().or_else(|| {
                        let changed = turn_start_digest
                            .as_deref()
                            .is_some_and(|start| start != self.guard.digest());
                        (changed && !self.plan_mode)
                            .then_some(self.check_on_change.as_ref())
                            .flatten()
                    });
                    if let Some(check) = effective_check {
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
                            let msg = json!({
                                "role": "user",
                                "content": format!(
                                    "External acceptance check failed. Fix the implementation, not the check.\n{}",
                                    verdict.output
                                )
                            });
                            self.messages.push(msg.clone());
                            self.had_checkpoint = true;
                            self.record
                                .append("message", json!({"message": msg}))
                                .map_err(|e| e.to_string())?;
                            self.record
                                .append("messages.checkpoint", json!({"messages": self.messages}))
                                .map_err(|e| e.to_string())?;
                            continue;
                        }
                        if self.journal.is_none() && self.candidate.is_none() {
                            notify_herdr("idle", "verified");
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
                        notify_herdr("idle", "done");
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
                    let msg = json!({
                        "role": "user",
                        "content": format!(
                            "Replay disagrees with the recorded oracle. Fix the code, not the journal:\n{}",
                            serde_json::to_string_pretty(&verdict).unwrap_or_default()
                        )
                    });
                    self.messages.push(msg.clone());
                    self.had_checkpoint = true;
                    self.record
                        .append("message", json!({"message": msg}))
                        .map_err(|e| e.to_string())?;
                    self.record
                        .append("messages.checkpoint", json!({"messages": self.messages}))
                        .map_err(|e| e.to_string())?;
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
                let note = params.get("note").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
                if allow && remember {
                    if matches!(name.as_str(), "bash" | "task") {
                        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                        let pattern = self.policy.allow_for_session(cmd);
                        self.record.append("policy.allowed", json!({"pattern": pattern})).map_err(|e| e.to_string())?;
                        emit(json!({"ev": "policy.allowed", "pattern": pattern}));
                    } else if matches!(name.as_str(), "edit" | "write" | "apply_patch") {
                        let (ro, _) = self.tools.configure_mode(self.plan_mode, false);
                        emit(json!({"ev": "mode.changed", "plan": self.plan_mode, "gate_edits": false, "read_only": ro}));
                    }
                }
                self.record.append("approval.resolved", json!({"id": id, "allow": allow, "remember": remember, "note": note})).map_err(|e| e.to_string())?;
                emit(json!({"ev": "approval.resolved", "id": id, "allow": allow, "remember": remember, "note": note}));

                let output = if !allow {
                    let deny_msg = note.map_or_else(|| "denied by user".to_owned(), |n| format!("denied by user — do this instead: {n}"));
                    self.record.append("tool.result", json!({"id": id, "tool": name, "args": args, "tier": "deny", "output": deny_msg, "exit_code": null})).map_err(|e| e.to_string())?;
                    let message = json!({"role": "tool", "tool_call_id": id, "content": deny_msg});
                    self.messages.push(message.clone());
                    self.record.append("message", json!({"message": message})).map_err(|e| e.to_string())?;
                    deny_msg
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

            "record.verify" => match self.record.verify() {
                Ok(entries) => Ok(json!({"ok": true, "entries": entries})),
                Err((index, why)) => Ok(json!({"ok": false, "bad_index": index, "why": why})),
            },

            "session.verify" => {
                let chain = match self.record.verify() {
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
                    "gate_edits": self.tools.gates_edits(),
                    "operator": self.operator_mode,
                    "study": self.study_mode,
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
                            "background" => {
                                control.request_background();
                                Some(Ok(json!({"backgrounded": true})))
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
                                control.resolve_full(
                                    params["id"].as_str().unwrap_or("").into(),
                                    params["allow"].as_bool().unwrap_or(false),
                                    params["remember"].as_bool().unwrap_or(false),
                                    params.get("note").and_then(Value::as_str).map(str::to_owned),
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

    #[test]
    fn operator_btw_and_git_worktree_rewind_work_without_context_pollution() {
        let dir = std::env::temp_dir().join(format!("twig-daemon-m4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");

        // Initialize a real git repo to exercise non-intrusive GIT_INDEX_FILE worktree snapshots.
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "t@example.com"])
            .current_dir(&dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "twig"])
            .current_dir(&dir)
            .status();
        std::fs::write(dir.join("app.txt"), "v1\n").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .status();
        assert!(std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());

        let llm = StubLlm::new(vec![
            StubLlm::text("turn1 reply"),
            StubLlm::text("ephemeral btw answer"),
        ]);
        let mut daemon =
            Daemon::new(&llm, &dir, &runs, "m4-test", &HarnessConfig::default()).unwrap();

        let op = daemon
            .dispatch("mode.operator", &json!({"enabled": true}), &mut |_| {})
            .unwrap();
        assert_eq!(op["operator"], true);

        let t1 = daemon
            .dispatch("turn", &json!({"input": "inspect app"}), &mut |_| {})
            .unwrap();
        assert_eq!(t1["done"], true);
        let msg_count_before_btw = daemon.messages.len();

        // /btw answers using current context without adding anything to `daemon.messages`.
        let btw = daemon
            .dispatch(
                "session.btw",
                &json!({"question": "what file exists?"}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(btw["answer"], "ephemeral btw answer");
        assert_eq!(daemon.messages.len(), msg_count_before_btw);

        // Mutate tracked + untracked files, then rewind with `restore_code: true`.
        std::fs::write(dir.join("app.txt"), "mutated-v2\n").unwrap();
        std::fs::write(dir.join("stray.txt"), "untracked\n").unwrap();
        let rw = daemon
            .dispatch(
                "session.rewind",
                &json!({"turns": 1, "restore_code": true}),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(rw["rewound_turns"], 1);
        assert_eq!(std::fs::read_to_string(dir.join("app.txt")).unwrap(), "v1\n");
        assert!(!dir.join("stray.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_plan_reminders_never_leak_after_plan_exit_and_git_commit_works() {
        let dir = std::env::temp_dir().join(format!("twig-rem-commit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let runs = dir.join("runs");

        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "t@example.com"])
            .current_dir(&dir)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "twig"])
            .current_dir(&dir)
            .status();
        std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .status();
        assert!(std::process::Command::new("git")
            .args(["commit", "-q", "-m", "feat: initial"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());

        let llm = StubLlm::new(vec![
            StubLlm::text("planned"),
            StubLlm::text("implemented"),
            StubLlm::text("feat: bump v() to 2"),
        ]);
        let mut daemon =
            Daemon::new(&llm, &dir, &runs, "rem-test", &HarnessConfig::default()).unwrap();

        daemon
            .dispatch("mode.set", &json!({"plan": true}), &mut |_| {})
            .unwrap();
        daemon
            .dispatch("turn", &json!({"input": "design change"}), &mut |_| {})
            .unwrap();
        assert!(daemon
            .messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.contains("Plan mode is active"))));

        // Exiting `/plan` immediately purges the stale read-only `<system-reminder>`.
        daemon
            .dispatch("mode.set", &json!({"plan": false}), &mut |_| {})
            .unwrap();
        assert!(!daemon
            .messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.contains("Plan mode is active"))));

        daemon
            .dispatch("turn", &json!({"input": "implement it"}), &mut |_| {})
            .unwrap();
        std::fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        let committed = daemon
            .dispatch("git.commit", &json!({"hint": "bump v"}), &mut |_| {})
            .unwrap();
        assert_eq!(committed["subject"], "feat: bump v() to 2");
        assert!(!committed["sha"].as_str().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_read_requires_approval_and_exit_2_stop_hook_feeds_back_to_model() {
        let dir = std::env::temp_dir().join(format!("twig-secret-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".twig")).unwrap();
        let runs = dir.join("runs");
        std::fs::write(dir.join(".env"), "SECRET=123\n").unwrap();
        std::fs::write(
            dir.join(".twig/project.json"),
            r#"{"stop_hook": "test -f ready.flag || { echo 'missing ready.flag' >&2; exit 2; }"}"#,
        )
        .unwrap();

        // Part A: `read(path=".env")` pauses in `awaiting_approval` instead of leaking silently.
        let llm_secret = StubLlm::new(vec![StubLlm::with_tools(
            "",
            vec![call("s1", "read", json!({"path": ".env"}))],
        )]);
        let mut d_sec =
            Daemon::new(&llm_secret, &dir, &runs, "sec-test", &HarnessConfig::default()).unwrap();
        let paused = d_sec
            .dispatch("turn", &json!({"input": "check env"}), &mut |_| {})
            .unwrap();
        assert_eq!(paused["reason"], "awaiting_approval");

        // Part B: `stop_hook` exit 2 blocks premature completion, feeds stderr back, and succeeds once fixed.
        let llm_hook = StubLlm::new(vec![
            StubLlm::text("done too early"),
            StubLlm::with_tools(
                "",
                vec![call(
                    "w1",
                    "write",
                    json!({"path": "ready.flag", "content": "ok\n"}),
                )],
            ),
            StubLlm::text("now truly done"),
        ]);
        let mut d_hook =
            Daemon::new(&llm_hook, &dir, &runs, "hook-test", &HarnessConfig::default()).unwrap();
        let fin = d_hook
            .dispatch("turn", &json!({"input": "finish task"}), &mut |_| {})
            .unwrap();
        assert_eq!(fin["done"], true);
        assert_eq!(fin["reason"], "final");
        assert!(dir.join("ready.flag").is_file());
        assert!(llm_hook
            .seen()
            .iter()
            .any(|m| m.contains("missing ready.flag")));

        // Part C: `/study` mode is toggleable (default off, injects epistemic provenance when on, zero residue when off)
        // and writing `.twig/project.json` pauses for explicit human approval (`awaiting_approval`).
        let llm_study = StubLlm::new(vec![
            StubLlm::text("[Observed] ok"),
            StubLlm::text("normal reply"),
            StubLlm::with_tools(
                "",
                vec![call(
                    "cp1",
                    "write",
                    json!({"path": ".twig/project.json", "content": "{}"}),
                )],
            ),
        ]);
        let mut d_st =
            Daemon::new(&llm_study, &dir, &runs, "study-test", &HarnessConfig::default()).unwrap();
        d_st.dispatch("mode.study", &json!({"enabled": true}), &mut |_| {})
            .unwrap();
        d_st.dispatch("turn", &json!({"input": "study target"}), &mut |_| {})
            .unwrap();
        assert!(d_st.messages.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("REA Study Mode is active"))
        }));
        d_st.dispatch("mode.study", &json!({"enabled": false}), &mut |_| {})
            .unwrap();
        d_st.dispatch("turn", &json!({"input": "normal turn"}), &mut |_| {})
            .unwrap();
        assert!(!d_st.messages.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("REA Study Mode is active"))
        }));
        let cp_pause = d_st
            .dispatch("turn", &json!({"input": "edit project config"}), &mut |_| {})
            .unwrap();
        assert_eq!(cp_pause["reason"], "awaiting_approval");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
