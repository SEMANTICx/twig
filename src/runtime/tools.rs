//! Worktree-confined tool execution with a narrow external interface.
//!
//! Access restrictions (`read_only_mask` combining permanent guard narrowing and
//! user-toggled `/plan` mode) stay private behind behavioral methods, and skill
//! metadata is queried via `SkillIndex::summary_json()`, avoiding field leaks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::guidance::{SkillIndex, SkillStore};
use super::mcp::McpRegistry;
use super::subagent::{Role, SubagentConfig, SubagentPool};
use super::tasks::TaskManager;
use crate::config::{paths, HarnessConfig};
use crate::control::Control;
use crate::exec::{isolation, run_live as exec_run_live, Owner};
use crate::llm::ToolCall;

const GUARD_RO: u8 = 0b01;
const PLAN_RO: u8 = 0b10;
const MAX_UNDO_SNAPSHOTS: usize = 64;
const MAX_GREP_FILE_BYTES: u64 = 1_048_576;
const MAX_GREP_MATCHES: usize = 100;
const MAX_GLOB_MATCHES: usize = 200;
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".twig-workers",
    "dist",
    "build",
    "__pycache__",
];

struct FileSnapshot {
    path: PathBuf,
    rel: String,
    previous: Option<String>,
}

pub struct Tools {
    worktree: PathBuf,
    read_only_mask: u8,
    timeout: Duration,
    max_output: usize,
    read_max_lines: usize,
    bash_description: String,
    read_description: String,
    groups: Vec<Owner>,
    last_exit_code: Option<i32>,
    isolation: bool,
    network: bool,
    skills: Option<SkillIndex>,
    mcp: Option<McpRegistry>,
    agent_pool: Option<SubagentPool>,
    agent_concurrency: usize,
    artifacts_dir: Option<PathBuf>,
    tasks: TaskManager,
    undo_stack: Vec<FileSnapshot>,
}

impl Tools {
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self::with_config(worktree, &HarnessConfig::default())
    }

    pub fn with_config(worktree: impl Into<PathBuf>, cfg: &HarnessConfig) -> Self {
        let worktree: PathBuf = worktree.into();
        let skills = SkillIndex::discover_default(&worktree);
        let mcp = McpRegistry::discover(&worktree);
        let tasks = TaskManager::new(worktree.clone(), cfg.isolation, cfg.network);
        Self {
            worktree,
            read_only_mask: 0,
            timeout: Duration::from_secs(cfg.bash_timeout_secs),
            max_output: cfg.max_output,
            read_max_lines: cfg.read_max_lines,
            bash_description: cfg.bash_description.clone(),
            read_description: cfg.read_description.clone(),
            groups: Vec::new(),
            last_exit_code: None,
            isolation: cfg.isolation,
            network: cfg.network,
            skills,
            mcp,
            agent_pool: None,
            agent_concurrency: cfg.subagent_concurrency,
            artifacts_dir: None,
            tasks,
            undo_stack: Vec::new(),
        }
    }

    pub fn with_artifacts_dir(mut self, dir: PathBuf) -> Self {
        self.artifacts_dir = Some(dir);
        self
    }

    /// Re-scan project & global skills on disk (e.g. after `/skills enable` or `/evolve`).
    pub fn reload_skills(&mut self) {
        self.skills = SkillIndex::discover_default(&self.worktree);
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// Degrade the toolset to read-only mode permanently (e.g. when the progress guard narrows).
    pub fn restrict_read_only(&mut self) {
        self.read_only_mask |= GUARD_RO;
    }

    /// Toggle user-controlled read-only planning mode without ever weakening a guard latch.
    /// Returns whether the toolset is effectively read-only afterward.
    pub fn apply_plan_mode(&mut self, enabled: bool) -> bool {
        if enabled {
            self.read_only_mask |= PLAN_RO;
        } else {
            self.read_only_mask &= !PLAN_RO;
        }
        self.read_only_mask != 0
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only_mask != 0
    }

    /// System prompt instructions owned by the active tool configuration
    /// (harness self-awareness, hybrid Layer-1 skills, MCP, and plan mode).
    pub fn system_addendum_for(&self, input: &str) -> String {
        let mut out = String::from(
            "\nYou run inside the twig harness. Prefer read/edit/write/grep/glob over shell cat/sed/grep; \
             inspect clipped outputs via read(path=\"artifact:<file>\"); use task for long background commands, \
             parallel subagent batches for exploration/review, and papercut for harness defects. \
             Skills use 3 tiers (.twig/skills > ~/.config/twig/skills, cold vault ~/.local/share/twig-skills-vault); \
             manage via `twig skill <list|enable|disable|add|doctor>`, and load `skill-creator` when authoring skills. \
             User TUI controls: /plan (Shift+Tab), /rewind (Esc×2), /undo, /skills, $skill-name, /evolve, /verify, !cmd, @file.",
        );
        if let Some(skills) = &self.skills {
            out.push_str(&skills.system_prompt_block_for(input));
        }
        if let Some(mcp) = &self.mcp {
            out.push_str(&format!(
                "\nConfigured MCP servers (use mcp tool for tools, prompts/skills, and resources): {}",
                mcp.server_names_summary()
            ));
        }
        if (self.read_only_mask & PLAN_RO) != 0 {
            out.push_str(
                "\nPlan mode is active (read-only). Explore the codebase, analyze requirements, and propose a concrete step-by-step plan. Do not modify files or run mutating commands.",
            );
        }
        out
    }

    /// Compact one-line summary of the active harness environment for the TUI welcome header.
    pub fn env_summary(&self) -> String {
        let (active, vault) = SkillStore::default_for(Some(&self.worktree)).counts();
        let skills_part = if vault > 0 {
            format!("skills {active} (+{vault} vault)")
        } else {
            format!("skills {active}")
        };
        let loaded_docs: Vec<&str> = ["AGENTS.md", "CLAUDE.md", ".twig/preferences.md"]
            .into_iter()
            .filter(|rel| {
                std::fs::metadata(self.worktree.join(rel))
                    .is_ok_and(|m| m.is_file() && m.len() > 0)
            })
            .collect();
        let docs_part = if loaded_docs.is_empty() {
            "prefs /init".to_owned()
        } else {
            format!("docs {}", loaded_docs.join(", "))
        };
        let mcp_part = match &self.mcp {
            Some(reg) => format!("mcp {}", reg.server_names_summary()),
            None => "mcp 0".to_owned(),
        };
        let sandbox_part = if self.isolation {
            "sandbox bwrap"
        } else {
            "sandbox off"
        };
        format!("{skills_part}  ·  {docs_part}  ·  {mcp_part}  ·  {sandbox_part}")
    }

    pub fn schema(&self) -> Value {
        let mut list = vec![
            json!({"type": "function", "function": {
                "name": "bash",
                "description": self.bash_description,
                "parameters": {"type": "object",
                               "properties": {"command": {"type": "string"}},
                               "required": ["command"]}}}),
            json!({"type": "function", "function": {
                "name": "read",
                "description": format!("{} Returns at most {} lines. Optional 1-based start_line and limit paginate large files or `artifact:<file>` side-channel outputs.",
                                       self.read_description, self.read_max_lines),
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "start_line": {"type": "integer"},
                                   "limit": {"type": "integer"}
                               },
                               "required": ["path"]}}}),
            json!({"type": "function", "function": {
                "name": "edit",
                "description": "Replace an exact substring inside an existing file in the worktree. Fails unless old_string matches uniquely (or replace_all is true). Returns a unified diff.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "old_string": {"type": "string"},
                                   "new_string": {"type": "string"},
                                   "replace_all": {"type": "boolean"}
                               },
                               "required": ["path", "old_string", "new_string"]}}}),
            json!({"type": "function", "function": {
                "name": "write",
                "description": "Create or overwrite a file inside the worktree. Prefer edit for surgical changes to existing files.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "content": {"type": "string"}
                               },
                               "required": ["path", "content"]}}}),
            json!({"type": "function", "function": {
                "name": "grep",
                "description": "Search text across files in the worktree (skips .git, target, node_modules). Returns path:line: text matches.",
                "parameters": {"type": "object",
                               "properties": {
                                   "pattern": {"type": "string"},
                                   "path": {"type": "string"},
                                   "glob": {"type": "string"},
                                   "ignore_case": {"type": "boolean"}
                               },
                               "required": ["pattern"]}}}),
            json!({"type": "function", "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern (*, **, ?) inside the worktree.",
                "parameters": {"type": "object",
                               "properties": {
                                   "pattern": {"type": "string"},
                                   "path": {"type": "string"}
                               },
                               "required": ["pattern"]}}}),
            json!({"type": "function", "function": {
                "name": "todo",
                "description": "Update the live multi-step task progress checklist shown to the user.",
                "parameters": {"type": "object",
                               "properties": {
                                   "items": {
                                       "type": "array",
                                       "items": {
                                           "type": "object",
                                           "properties": {
                                               "content": {"type": "string"},
                                               "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                           },
                                           "required": ["content", "status"]
                                       }
                                   }
                               },
                               "required": ["items"]}}}),
            json!({"type": "function", "function": {
                "name": "papercut",
                "description": "Report a concrete tool, skill or harness defect you encountered. Give what you tried, expected, observed, and a recovery or proposed fix. Report once per issue, then continue working.",
                "parameters": {"type":"object", "additionalProperties": false,
                    "properties": {
                        "component":{"type":"string"}, "attempted":{"type":"string"},
                        "expected":{"type":"string"}, "actual":{"type":"string"},
                        "recovery":{"type":"string"}},
                    "required":["component","attempted","expected","actual","recovery"]}}}),
            json!({"type":"function","function":{"name":"skill","description":"List/search project & global skill metadata (optional query), or load one named skill body only when needed.",
                "parameters":{"type":"object","properties":{"name":{"type":"string"},"query":{"type":"string"}}}}}),
            json!({"type":"function","function":{"name":"subagent","description":"Delegate a bounded task in a fresh isolated Git worktree; reviewer/scout are OS read-only. Multiple subagent calls in one turn run in parallel up to concurrency limit. Optional model/effort override per role.",
                "parameters":{"type":"object","properties":{"role":{"type":"string","enum":["code","test","scout","reviewer"]},"task":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string","enum":["off","low","medium","high","xhigh"]}},"required":["role","task"]}}}),
            json!({"type":"function","function":{"name":"task","description":"Manage asynchronous background shell tasks (start a long-running build/test/server, poll status, wait for completion, cancel, or list active tasks).",
                "parameters":{"type":"object","properties":{"action":{"type":"string","enum":["start","status","wait","cancel","list"]},"command":{"type":"string"},"id":{"type":"string"},"timeout_secs":{"type":"integer"}},"required":["action"]}}}),
        ];
        if self.mcp.is_some() {
            list.push(json!({
                "type": "function",
                "function": {
                    "name": "mcp",
                    "description": "Interact with a configured MCP server: call a tool, or list/load prompts (skills over MCP) and resources via action=tools|call|prompts|get_prompt|resources|read_resource.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"},
                            "action": {"type": "string", "enum": ["call", "tools", "prompts", "get_prompt", "resources", "read_resource"]},
                            "tool": {"type": "string"},
                            "prompt": {"type": "string"},
                            "uri": {"type": "string"},
                            "arguments": {"type": "object"}
                        },
                        "required": ["server"]
                    }
                }
            }));
        }
        Value::Array(list)
    }

    pub fn list_mcp(&mut self) -> Value {
        match &mut self.mcp {
            Some(reg) => reg.list_report(),
            None => json!({"servers": []}),
        }
    }

    pub fn list_tasks(&self) -> Value {
        self.tasks.list()
    }

    pub fn running_tasks_count(&self) -> usize {
        self.tasks.running_count()
    }

    /// Clip at the tool boundary: head and tail survive, the middle is elided.
    pub fn clip(&self, text: &str) -> String {
        self.clip_for_call("", text)
    }

    /// Clip oversized tool output while saving the full text to an artifact side-channel
    /// readable via `read(path="artifact:<file>", start_line=...)`.
    pub fn clip_for_call(&self, call_id: &str, text: &str) -> String {
        if text.len() <= self.max_output {
            return text.to_string();
        }
        let saved_hint = if !call_id.is_empty() {
            if let Some(dir) = &self.artifacts_dir {
                let safe_id: String = call_id
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                    .take(48)
                    .collect();
                let fname = format!("tool-{safe_id}.txt");
                if std::fs::create_dir_all(dir).is_ok()
                    && std::fs::write(dir.join(&fname), text).is_ok()
                {
                    let total_lines = text.lines().count();
                    format!(
                        "; full output ({total_lines} lines) saved to artifact:{fname} — inspect via read(path=\"artifact:{fname}\", start_line=...)"
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let half = self.max_output / 2;
        let head: String = text.chars().take(half).collect();
        let tail: Vec<char> = text.chars().rev().take(half).collect();
        let tail: String = tail.into_iter().rev().collect();
        format!(
            "{head}\n... [{} bytes clipped{saved_hint}] ...\n{tail}",
            text.len() - self.max_output
        )
    }

    pub fn skill_metadata(&self) -> String {
        self.skills
            .as_ref()
            .map(SkillIndex::summary_json)
            .unwrap_or_default()
    }

    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit_code
    }

    /// Errors are data: the model gets text back either way.
    pub fn run(&mut self, call: &ToolCall) -> String {
        self.run_live(call, &Control::default(), &mut |_| {})
    }

    pub fn run_live(
        &mut self,
        call: &ToolCall,
        control: &Control,
        emit: &mut dyn FnMut(Value),
    ) -> String {
        self.last_exit_code = None;
        match call.name.as_str() {
            "bash" => {
                let cmd = call
                    .args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.run_bash(&call.id, cmd, control, emit)
            }
            "read" => self.read(
                call.args.get("path").and_then(Value::as_str).unwrap_or(""),
                call.args
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
                call.args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            ),
            "edit" => self.edit(
                call.args.get("path").and_then(Value::as_str).unwrap_or(""),
                call.args
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                call.args
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                call.args
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "write" => self.write_file(
                call.args.get("path").and_then(Value::as_str).unwrap_or(""),
                call.args
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            "grep" => self.grep(
                call.args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                call.args.get("path").and_then(Value::as_str),
                call.args.get("glob").and_then(Value::as_str),
                call.args
                    .get("ignore_case")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "glob" => self.glob(
                call.args
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                call.args.get("path").and_then(Value::as_str),
            ),
            "papercut" => {
                let fields = ["component", "attempted", "expected", "actual", "recovery"];
                if fields.iter().all(|f| {
                    call.args[*f]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 4000)
                }) {
                    "papercut recorded".into()
                } else {
                    "invalid papercut: each required field must contain 1..4000 bytes".into()
                }
            }
            "skill" => {
                if let Some(name) = call.args["name"].as_str() {
                    if let Some(index) = &self.skills {
                        if let Ok(body) = index.load(name) {
                            return body;
                        }
                    }
                    // Allow explicit loading from the cold vault without requiring global enable.
                    SkillStore::default_for(Some(&self.worktree))
                        .load_any(name)
                        .map(|(body, _)| body)
                        .unwrap_or_else(|e| format!("skill error: {e}"))
                } else if let Some(index) = &self.skills {
                    match call.args["query"].as_str() {
                        Some(q) if !q.trim().is_empty() => index.search_json(q),
                        _ => index.summary_json(),
                    }
                } else {
                    "No project or global skills enabled; check `twig skill list` for cold-vault skills.".into()
                }
            }
            "todo" => self.update_todo(&call.args, emit),
            "mcp" => match &mut self.mcp {
                Some(reg) => {
                    let server = call.args["server"].as_str().unwrap_or("");
                    let action = call.args["action"].as_str().unwrap_or("call");
                    let target = match action {
                        "get_prompt" => call.args["prompt"]
                            .as_str()
                            .or_else(|| call.args["tool"].as_str())
                            .unwrap_or(""),
                        "read_resource" => call.args["uri"]
                            .as_str()
                            .or_else(|| call.args["tool"].as_str())
                            .unwrap_or(""),
                        _ => call.args["tool"].as_str().unwrap_or(""),
                    };
                    let args = call
                        .args
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    match reg.invoke(server, action, target, args) {
                        Ok(out) => {
                            self.last_exit_code = Some(0);
                            out
                        }
                        Err(e) => format!("mcp error: {e}"),
                    }
                }
                None => "no MCP servers configured in .twig/mcp.json or ~/.config/twig/mcp.json".into(),
            },
            "subagent" => self.delegate(&call.args, emit),
            "task" => self.run_task(&call.args, emit),
            other => format!("no such tool {other:?}"),
        }
    }

    fn run_bash(
        &mut self,
        call_id: &str,
        raw: &str,
        control: &Control,
        emit: &mut dyn FnMut(Value),
    ) -> String {
        if raw.trim().is_empty() {
            return "refused: empty command".into();
        }
        if self.is_read_only() && !self.isolation {
            return if self.read_only_mask == PLAN_RO {
                "refused: plan mode is active, toolset is read-only".into()
            } else {
                "refused: guard is narrow, toolset is read-only".into()
            };
        }
        let isolated = if self.isolation {
            match isolation::shell(&self.worktree, raw, self.network, self.is_read_only()) {
                Ok(s) => Some(s),
                Err(e) => return format!("isolation error: {e}"),
            }
        } else {
            None
        };
        let command = isolated.as_deref().unwrap_or(raw);
        let id = call_id.to_owned();
        match exec_run_live(
            &self.worktree,
            command,
            self.timeout,
            control,
            &mut |text| emit(json!({"ev": "tool.output", "id": id, "text": text})),
        ) {
            Ok(out) => {
                self.last_exit_code = out.exit_code;
                self.groups.push(out.owner);
                if out.text.trim().is_empty() {
                    format!("(exit {}, no output)", out.exit_code.unwrap_or(-1))
                } else if out.exit_code != Some(0) && !out.timed_out {
                    format!("{}\n[exit {}]", out.text, out.exit_code.unwrap_or(-1))
                } else {
                    out.text
                }
            }
            Err(e) => format!("spawn: {e}"),
        }
    }

    fn update_todo(&mut self, args: &Value, emit: &mut dyn FnMut(Value)) -> String {
        let Some(items) = args.get("items").and_then(Value::as_array) else {
            return "invalid todo: expected items array".into();
        };
        let mut normalized = Vec::new();
        let mut completed = 0usize;
        for item in items.iter().take(32) {
            let content = item["content"].as_str().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let status = match item["status"].as_str().unwrap_or("pending") {
                "completed" => {
                    completed += 1;
                    "completed"
                }
                "in_progress" => "in_progress",
                _ => "pending",
            };
            normalized.push(json!({"content": content, "status": status}));
        }
        emit(json!({"ev": "todo.updated", "items": normalized}));
        self.last_exit_code = Some(0);
        format!("todo updated ({completed}/{} completed)", normalized.len())
    }

    fn run_task(&mut self, args: &Value, emit: &mut dyn FnMut(Value)) -> String {
        let action = args["action"].as_str().unwrap_or("list");
        let res = match action {
            "start" => {
                let cmd = args["command"].as_str().unwrap_or("");
                let r = self.tasks.start(cmd, self.is_read_only());
                if let Ok(ref snap) = r {
                    emit(json!({
                        "ev": "task.started",
                        "id": snap["id"],
                        "command": snap["command"],
                        "running": self.tasks.running_count()
                    }));
                }
                r
            }
            "status" => {
                let id = args["id"].as_str().unwrap_or("");
                self.tasks.status(id)
            }
            "wait" => {
                let id = args["id"].as_str().unwrap_or("");
                let secs = args["timeout_secs"].as_u64().unwrap_or(30);
                let r = self.tasks.wait(id, secs);
                if let Ok(ref snap) = r {
                    if snap["status"] != "running" {
                        emit(json!({
                            "ev": "task.completed",
                            "id": snap["id"],
                            "status": snap["status"],
                            "exit_code": snap["exit_code"],
                            "elapsed_secs": snap["elapsed_secs"],
                            "running": self.tasks.running_count()
                        }));
                    }
                }
                r
            }
            "cancel" => {
                let id = args["id"].as_str().unwrap_or("");
                let r = self.tasks.cancel(id);
                if let Ok(ref snap) = r {
                    emit(json!({
                        "ev": "task.completed",
                        "id": snap["id"],
                        "status": "cancelled",
                        "exit_code": snap["exit_code"],
                        "elapsed_secs": snap["elapsed_secs"],
                        "running": self.tasks.running_count()
                    }));
                }
                r
            }
            "list" => Ok(self.tasks.list()),
            other => Err(format!(
                "invalid task action {other:?}; expected start, status, wait, cancel, or list"
            )),
        };
        match res {
            Ok(val) => {
                self.last_exit_code = Some(0);
                serde_json::to_string_pretty(&val).unwrap_or_default()
            }
            Err(e) => {
                self.last_exit_code = Some(1);
                format!("task error: {e}")
            }
        }
    }

    fn ensure_agent_pool(&mut self) -> Result<SubagentPool, String> {
        if self.agent_pool.is_none() {
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            let cfg = SubagentConfig {
                twig: executable,
                repository: self.worktree.clone(),
                runs_root: paths::state_dir().join("subagents"),
                bwrap: "/usr/bin/bwrap".into(),
                concurrency: self.agent_concurrency,
                timeout: self.timeout,
                max_output_bytes: 64_000,
                network: self.network,
            };
            self.agent_pool = Some(
                SubagentPool::new(cfg).map_err(|e| format!("subagent setup: {e}"))?,
            );
        }
        Ok(self.agent_pool.as_ref().unwrap().clone())
    }
}

struct SubagentSpec<'a> {
    role: Role,
    role_name: &'a str,
    task: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
}

struct SubagentOutcome<'a> {
    text: String,
    ok: bool,
    elapsed_secs: u64,
    role_name: &'a str,
}

impl Tools {
    fn parse_subagent_args<'a>(&self, args: &'a Value) -> Result<SubagentSpec<'a>, String> {
        if std::env::var_os("TWIG_SUBAGENT").is_some() {
            return Err("subagent recursion is disabled; return a result to the parent".into());
        }
        let role_name = args["role"].as_str().unwrap_or("code");
        let role = match role_name {
            "code" => Role::Code,
            "test" => Role::Test,
            "scout" => Role::Scout,
            "reviewer" => Role::Reviewer,
            _ => return Err("invalid subagent role".into()),
        };
        if self.is_read_only() && matches!(role, Role::Code | Role::Test) {
            return Err("refused: read-only mode only permits scout or reviewer subagents".into());
        }
        Ok(SubagentSpec {
            role,
            role_name,
            task: args["task"].as_str().unwrap_or(""),
            model: args.get("model").and_then(Value::as_str),
            effort: args.get("effort").and_then(Value::as_str),
        })
    }

    /// Execute a batch of `subagent` tool calls concurrently via `SubagentPool`
    /// while preserving original call order and emitting lifecycle events.
    pub fn run_subagents_batch(
        &mut self,
        calls: &[&ToolCall],
        emit: &mut dyn FnMut(Value),
    ) -> Vec<(String, Option<i32>)> {
        let mut parsed = Vec::with_capacity(calls.len());
        let mut any_valid = false;
        for call in calls {
            let p = self.parse_subagent_args(&call.args);
            if let Ok(spec) = &p {
                any_valid = true;
                emit(json!({
                    "ev": "subagent.started",
                    "role": spec.role_name,
                    "task": spec.task,
                    "model": spec.model,
                    "effort": spec.effort,
                    "parallel": calls.len() > 1
                }));
            }
            parsed.push(p);
        }
        if !any_valid {
            return parsed
                .into_iter()
                .map(|r| (r.err().unwrap_or_default(), None))
                .collect();
        }
        let pool = match self.ensure_agent_pool() {
            Ok(p) => p,
            Err(e) => {
                return calls.iter().map(|_| (e.clone(), None)).collect();
            }
        };

        let results: Vec<Result<SubagentOutcome<'_>, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = parsed
                .into_iter()
                .map(|item| {
                    let pool = pool.clone();
                    s.spawn(move || {
                        let spec = item?;
                        let started = std::time::Instant::now();
                        let run = pool.run_with_options(
                            spec.role,
                            spec.task,
                            spec.model,
                            spec.effort,
                        );
                        let elapsed_secs = started.elapsed().as_secs();
                        match run {
                            Ok(r) => Ok(SubagentOutcome {
                                text: serde_json::to_string(&r).unwrap_or_default(),
                                ok: true,
                                elapsed_secs,
                                role_name: spec.role_name,
                            }),
                            Err(e) => Ok(SubagentOutcome {
                                text: format!("subagent error: {e}"),
                                ok: false,
                                elapsed_secs,
                                role_name: spec.role_name,
                            }),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err("subagent worker panicked".into()))
                })
                .collect()
        });

        let mut out = Vec::with_capacity(results.len());
        for res in results {
            match res {
                Ok(done) => {
                    emit(json!({
                        "ev": "subagent.completed",
                        "role": done.role_name,
                        "ok": done.ok,
                        "elapsed_secs": done.elapsed_secs
                    }));
                    out.push((done.text, done.ok.then_some(0)));
                }
                Err(msg) => out.push((msg, None)),
            }
        }
        if let Some((_, code)) = out.last() {
            self.last_exit_code = *code;
        }
        out
    }

    fn delegate(&mut self, args: &Value, emit: &mut dyn FnMut(Value)) -> String {
        let dummy = ToolCall {
            id: String::new(),
            name: "subagent".into(),
            args: args.clone(),
        };
        let mut results = self.run_subagents_batch(&[&dummy], emit);
        let (text, code) = results
            .pop()
            .unwrap_or_else(|| ("subagent failed".into(), None));
        self.last_exit_code = code;
        text
    }
}

impl Tools {

    /// Revert the most recent `edit` or `write` tool change in this session.
    pub fn undo_last_edit(&mut self) -> Result<String, String> {
        if self.is_read_only() {
            return Err("refused: read-only mode is active".into());
        }
        let snap = self
            .undo_stack
            .pop()
            .ok_or_else(|| "nothing to undo in this session".to_owned())?;
        match snap.previous {
            Some(prev) => {
                std::fs::write(&snap.path, &prev)
                    .map_err(|e| format!("restore {}: {e}", snap.rel))?;
                Ok(format!("restored {} ({} bytes)", snap.rel, prev.len()))
            }
            None => {
                if snap.path.exists() {
                    std::fs::remove_file(&snap.path)
                        .map_err(|e| format!("remove {}: {e}", snap.rel))?;
                }
                Ok(format!("removed newly created {}", snap.rel))
            }
        }
    }

    fn push_undo(&mut self, path: PathBuf, rel: String, previous: Option<String>) {
        if self.undo_stack.len() >= MAX_UNDO_SNAPSHOTS {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(FileSnapshot {
            path,
            rel,
            previous,
        });
    }

    fn resolve_existing_in_worktree(&self, path: &str) -> Result<(PathBuf, PathBuf), String> {
        let root = self
            .worktree
            .canonicalize()
            .map_err(|e| format!("worktree: {e}"))?;
        let target = root
            .join(path)
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        if !target.starts_with(&root) {
            return Err(format!("refused: {path} resolves outside the worktree"));
        }
        Ok((root, target))
    }

    fn resolve_writable_in_worktree(&self, path: &str) -> Result<(PathBuf, PathBuf), String> {
        if path.trim().is_empty() {
            return Err("refused: empty path".into());
        }
        let rel = Path::new(path);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("refused: {path} must stay inside the worktree"));
        }
        let root = self
            .worktree
            .canonicalize()
            .map_err(|e| format!("worktree: {e}"))?;
        let candidate = root.join(rel);
        let mut ancestor = candidate.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| format!("refused: invalid path {path}"))?;
        }
        let canon_ancestor = ancestor
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        if !canon_ancestor.starts_with(&root) {
            return Err(format!("refused: {path} resolves outside the worktree"));
        }
        let tail = candidate
            .strip_prefix(ancestor)
            .map_err(|_| format!("refused: {path} resolves outside the worktree"))?;
        Ok((root, canon_ancestor.join(tail)))
    }

    fn read_only_refusal(&self) -> String {
        if self.read_only_mask == PLAN_RO {
            "refused: plan mode is active, toolset is read-only".into()
        } else {
            "refused: guard is narrow, toolset is read-only".into()
        }
    }

    fn read(&mut self, path: &str, start_line: Option<usize>, limit: Option<usize>) -> String {
        let target = if let Some(fname) = path.strip_prefix("artifact:") {
            let Some(dir) = &self.artifacts_dir else {
                return "artifact storage is not configured".into();
            };
            if fname.is_empty() || fname.contains('/') || fname.contains('\\') || fname.contains("..") {
                return format!("refused: invalid artifact name {fname:?}");
            }
            let Ok(root) = dir.canonicalize() else {
                return format!("artifact:{fname}: not found");
            };
            let Ok(real) = root.join(fname).canonicalize() else {
                return format!("artifact:{fname}: not found");
            };
            if !real.starts_with(&root) {
                return format!("refused: {path} resolves outside artifacts");
            }
            real
        } else {
            match self.resolve_existing_in_worktree(path) {
                Ok((_, p)) => p,
                Err(e) => return e,
            }
        };
        match std::fs::read_to_string(&target) {
            Ok(text) => {
                self.last_exit_code = Some(0);
                let skip = start_line.unwrap_or(1).saturating_sub(1);
                let take = limit.unwrap_or(self.read_max_lines).clamp(1, self.read_max_lines);
                let mut lines = text.lines().skip(skip);
                let mut result = lines
                    .by_ref()
                    .take(take)
                    .collect::<Vec<_>>()
                    .join("\n");
                if lines.next().is_some() {
                    let next_line = skip + take + 1;
                    result.push_str(&format!(
                        "\n[truncated after {take} lines; pass start_line={next_line} to continue]"
                    ));
                }
                if result.is_empty() {
                    "(empty file)".into()
                } else {
                    result
                }
            }
            Err(e) => format!("{path}: {e}"),
        }
    }

    fn edit(
        &mut self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> String {
        if self.is_read_only() {
            return self.read_only_refusal();
        }
        if old_string.is_empty() {
            return "refused: old_string must not be empty".into();
        }
        if old_string == new_string {
            return "refused: old_string and new_string are identical".into();
        }
        let (root, target) = match self.resolve_existing_in_worktree(path) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let before = match std::fs::read_to_string(&target) {
            Ok(s) => s,
            Err(e) => return format!("{path}: {e}"),
        };
        let positions: Vec<usize> = before.match_indices(old_string).map(|(i, _)| i).collect();
        if positions.is_empty() {
            return format!("edit failed: old_string not found in {path}");
        }
        if positions.len() > 1 && !replace_all {
            let line_nums: Vec<String> = positions
                .iter()
                .take(8)
                .map(|&idx| (before[..idx].bytes().filter(|&b| b == b'\n').count() + 1).to_string())
                .collect();
            return format!(
                "edit failed: old_string matched {} times in {path} (lines {}); include more context or set replace_all=true",
                positions.len(),
                line_nums.join(", ")
            );
        }
        let after = if replace_all {
            before.replace(old_string, new_string)
        } else {
            before.replacen(old_string, new_string, 1)
        };
        let rel = target
            .strip_prefix(&root)
            .unwrap_or(&target)
            .to_string_lossy()
            .into_owned();
        if let Err(e) = std::fs::write(&target, &after) {
            return format!("{path}: {e}");
        }
        self.push_undo(target, rel.clone(), Some(before.clone()));
        self.last_exit_code = Some(0);
        format_unified_diff(&rel, &before, &after)
    }

    fn write_file(&mut self, path: &str, content: &str) -> String {
        if self.is_read_only() {
            return self.read_only_refusal();
        }
        let (root, target) = match self.resolve_writable_in_worktree(path) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let previous = if target.exists() {
            std::fs::read_to_string(&target).ok()
        } else {
            None
        };
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("create parent dirs for {path}: {e}");
            }
        }
        if let Err(e) = std::fs::write(&target, content) {
            return format!("{path}: {e}");
        }
        let rel = target
            .strip_prefix(&root)
            .unwrap_or(&target)
            .to_string_lossy()
            .into_owned();
        let diff = match &previous {
            Some(prev) => format_unified_diff(&rel, prev, content),
            None => format!(
                "created {rel} ({} lines, {} bytes)",
                content.lines().count(),
                content.len()
            ),
        };
        self.push_undo(target, rel, previous);
        self.last_exit_code = Some(0);
        diff
    }

    fn grep(
        &mut self,
        pattern: &str,
        subpath: Option<&str>,
        file_glob: Option<&str>,
        ignore_case: bool,
    ) -> String {
        if pattern.is_empty() {
            return "refused: empty grep pattern".into();
        }
        let (root, base) = match self.resolve_existing_in_worktree(subpath.unwrap_or(".")) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.to_owned()
        };
        let mut files = Vec::new();
        collect_files(&base, &root, &mut files);
        files.sort();

        let mut matches = Vec::new();
        let mut truncated = false;
        for (rel, abs) in files {
            if let Some(g) = file_glob {
                if !glob_match(g, &rel)
                    && abs
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_none_or(|n| !glob_match(g, n))
                {
                    continue;
                }
            }
            let Ok(meta) = std::fs::metadata(&abs) else {
                continue;
            };
            if meta.len() > MAX_GREP_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&abs) else {
                continue;
            };
            if bytes.iter().take(512).any(|&b| b == 0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (idx, line) in text.lines().enumerate() {
                let hit = if ignore_case {
                    line.to_lowercase().contains(&needle)
                } else {
                    line.contains(&needle)
                };
                if hit {
                    matches.push(format!("{rel}:{}: {line}", idx + 1));
                    if matches.len() >= MAX_GREP_MATCHES {
                        truncated = true;
                        break;
                    }
                }
            }
            if truncated {
                break;
            }
        }
        self.last_exit_code = Some(0);
        if matches.is_empty() {
            return format!("no matches for {pattern:?}");
        }
        if truncated {
            matches.push(format!("[capped at {MAX_GREP_MATCHES} matches; narrow path or glob]"));
        }
        matches.join("\n")
    }

    fn glob(&mut self, pattern: &str, subpath: Option<&str>) -> String {
        if pattern.trim().is_empty() {
            return "refused: empty glob pattern".into();
        }
        let (root, base) = match self.resolve_existing_in_worktree(subpath.unwrap_or(".")) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut files = Vec::new();
        collect_files(&base, &root, &mut files);
        let mut matched: Vec<String> = files
            .into_iter()
            .filter_map(|(rel, _)| {
                let target = if let Some(sub) = subpath {
                    let prefix = format!("{}/", sub.trim_end_matches('/'));
                    rel.strip_prefix(&prefix).unwrap_or(&rel).to_owned()
                } else {
                    rel.clone()
                };
                (glob_match(pattern, &target) || glob_match(pattern, &rel)).then_some(rel)
            })
            .collect();
        matched.sort();
        self.last_exit_code = Some(0);
        if matched.is_empty() {
            return format!("no files matched {pattern:?}");
        }
        let truncated = matched.len() > MAX_GLOB_MATCHES;
        matched.truncate(MAX_GLOB_MATCHES);
        if truncated {
            matched.push(format!("[capped at {MAX_GLOB_MATCHES} files]"));
        }
        matched.join("\n")
    }
}

fn collect_files(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    if dir.is_file() {
        if let Ok(rel) = dir.strip_prefix(root) {
            out.push((rel.to_string_lossy().into_owned(), dir.to_path_buf()));
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|n| IGNORED_DIRS.contains(&n))
            {
                continue;
            }
            collect_files(&path, root, out);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push((rel.to_string_lossy().into_owned(), path));
            }
        }
    }
}

/// Pure-Rust glob matcher supporting `*`, `**`, and `?` over `/` path segments.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut memo = vec![vec![None; t.len() + 1]; p.len() + 1];
    fn go(p: &[char], t: &[char], pi: usize, ti: usize, memo: &mut [Vec<Option<bool>>]) -> bool {
        if let Some(hit) = memo[pi][ti] {
            return hit;
        }
        let res = if pi == p.len() {
            ti == t.len()
        } else if p[pi] == '*' {
            if pi + 1 < p.len() && p[pi + 1] == '*' {
                let next_pi = if pi + 2 < p.len() && p[pi + 2] == '/' {
                    pi + 3
                } else {
                    pi + 2
                };
                go(p, t, next_pi, ti, memo) || (ti < t.len() && go(p, t, pi, ti + 1, memo))
            } else {
                go(p, t, pi + 1, ti, memo)
                    || (ti < t.len() && t[ti] != '/' && go(p, t, pi, ti + 1, memo))
            }
        } else if ti < t.len() && (p[pi] == '?' && t[ti] != '/' || p[pi] == t[ti]) {
            go(p, t, pi + 1, ti + 1, memo)
        } else {
            false
        };
        memo[pi][ti] = Some(res);
        res
    }
    go(&p, &t, 0, 0, &mut memo)
}

/// Compact unified diff with 2 lines of surrounding context for `edit` and `write` feedback.
fn format_unified_diff(rel: &str, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (old.len() - prefix)
        && suffix < (new.len() - prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return format!("no changes in {rel}");
    }
    let ctx_start = prefix.saturating_sub(2);
    let old_end = (old.len() - suffix + 2).min(old.len());
    let new_end = (new.len() - suffix + 2).min(new.len());
    let old_count = old_end - ctx_start;
    let new_count = new_end - ctx_start;

    let mut out = format!(
        "--- a/{rel}\n+++ b/{rel}\n@@ -{},{} +{},{} @@\n",
        ctx_start + 1,
        old_count,
        ctx_start + 1,
        new_count
    );
    for line in &old[ctx_start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old[prefix..old.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new[prefix..new.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old[old.len() - suffix..old_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "twig-tools-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("note.txt"), "hello\nworld\n").unwrap();
        d
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "t".into(),
            name: name.into(),
            args,
        }
    }

    #[test]
    fn bash_runs_in_the_worktree() {
        let mut t = Tools::new(dir());
        assert!(t
            .run(&call("bash", json!({"command": "cat note.txt"})))
            .contains("hello"));
    }

    #[test]
    fn read_is_confined() {
        let mut t = Tools::new(dir());
        assert!(t
            .run(&call("read", json!({"path": "note.txt"})))
            .contains("hello"));
        let escape = t.run(&call("read", json!({"path": "../../../etc/passwd"})));
        assert!(escape.starts_with("refused"), "{escape}");
    }

    #[test]
    fn narrow_refuses_writes() {
        let mut t = Tools::new(dir());
        t.restrict_read_only();
        assert!(t
            .run(&call("bash", json!({"command": "echo hi"})))
            .starts_with("refused"));
    }

    #[test]
    fn config_drives_limits_and_schema() {
        let cfg = HarnessConfig {
            max_output: 1000,
            read_max_lines: 1,
            ..HarnessConfig::default()
        };
        let d = dir();
        let mut t = Tools::with_config(&d, &cfg);
        let read = t.run(&call("read", json!({"path": "note.txt"})));
        assert!(read.starts_with("hello\n[truncated after 1 lines"), "{read}");
        assert!(t.schema()[1]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("at most 1 lines"));
        let clipped = t.clip(&"A".repeat(3000));
        assert!(clipped.contains("clipped") && clipped.starts_with('A') && clipped.ends_with('A'));

        let art_dir = d.join(".art");
        let mut t2 = Tools::with_config(&d, &cfg).with_artifacts_dir(art_dir);
        let big = (1..=400).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        let saved = t2.clip_for_call("c99", &big);
        assert!(saved.contains("artifact:tool-c99.txt"));
        let page = t2.run(&call(
            "read",
            json!({"path": "artifact:tool-c99.txt", "start_line": 200, "limit": 1}),
        ));
        assert!(page.starts_with("line-200"), "{page}");
    }

    #[test]
    fn edit_write_grep_glob_and_undo_work_and_respect_read_only() {
        let d = dir();
        let mut t = Tools::new(&d);

        let created = t.run(&call(
            "write",
            json!({"path": "src/lib.rs", "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"}),
        ));
        assert!(created.contains("created src/lib.rs"));

        let diff = t.run(&call(
            "edit",
            json!({"path": "src/lib.rs", "old_string": "a + b", "new_string": "a.saturating_add(b)"}),
        ));
        assert!(diff.contains("-    a + b") && diff.contains("+    a.saturating_add(b)"));

        let found = t.run(&call("grep", json!({"pattern": "saturating_add", "glob": "*.rs"})));
        assert!(found.contains("src/lib.rs:2:"));

        let files = t.run(&call("glob", json!({"pattern": "**/*.rs"})));
        assert_eq!(files, "src/lib.rs");

        // Undo reverts edit, then second undo deletes newly created file.
        assert!(t.undo_last_edit().unwrap().contains("restored src/lib.rs"));
        assert!(std::fs::read_to_string(d.join("src/lib.rs"))
            .unwrap()
            .contains("a + b"));
        assert!(t.undo_last_edit().unwrap().contains("removed newly created"));
        assert!(!d.join("src/lib.rs").exists());

        // Plan mode refuses edits and writes.
        t.apply_plan_mode(true);
        assert!(t
            .run(&call("write", json!({"path": "x.txt", "content": "hi"})))
            .starts_with("refused"));
        assert!(t
            .run(&call(
                "edit",
                json!({"path": "note.txt", "old_string": "hello", "new_string": "bye"})
            ))
            .starts_with("refused"));
        assert!(t.undo_last_edit().is_err());
    }
}
