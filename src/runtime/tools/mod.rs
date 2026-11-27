//! Worktree-confined tool execution with a narrow external interface.
//!
//! Access restrictions (`read_only_mask` combining permanent guard narrowing and
//! user-toggled `/plan` mode) stay private behind behavioral methods, and skill
//! metadata is queried via `SkillIndex::summary_json()`, avoiding field leaks.

mod fs;
mod inspect;
mod lsp;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use self::fs::{glob_stateless, grep_stateless, outline_stateless, read_stateless, FileSnapshot};
use super::guidance::{SkillIndex, SkillStore};
use super::mcp::McpRegistry;
use super::policy::Tier;
use super::subagent::{Role, SubagentConfig, SubagentPool};
use super::tasks::TaskManager;
use crate::config::{paths, HarnessConfig};
use crate::control::Control;
use crate::exec::{isolation, run_live as exec_run_live, Owner};
use crate::llm::ToolCall;

const GUARD_RO: u8 = 0b001;
const PLAN_RO: u8 = 0b010;
const GATE_EDITS: u8 = 0b100;

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
    read_stamps: HashMap<PathBuf, (u128, u64)>,
    seen_subdir_docs: HashSet<PathBuf>,
    enforce_read_guard: bool,
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
            read_stamps: HashMap::new(),
            seen_subdir_docs: HashSet::new(),
            enforce_read_guard: true,
        }
    }

    pub fn with_artifacts_dir(mut self, dir: PathBuf) -> Self {
        self.artifacts_dir = Some(dir);
        self
    }

    pub fn with_read_guard(mut self, enabled: bool) -> Self {
        self.enforce_read_guard = enabled;
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

    /// Clear transient guard narrowing on a fresh user turn while preserving `/plan` read-only.
    pub fn clear_guard_narrow(&mut self) {
        self.read_only_mask &= !GUARD_RO;
    }

    /// Toggle user-controlled read-only planning mode without ever weakening a guard latch.
    /// Returns whether the toolset is effectively read-only afterward.
    pub fn apply_plan_mode(&mut self, enabled: bool) -> bool {
        if enabled {
            self.read_only_mask |= PLAN_RO;
            self.read_only_mask &= !GATE_EDITS;
        } else {
            self.read_only_mask &= !PLAN_RO;
        }
        self.is_read_only()
    }

    /// Configure interactive permission mode (`plan` read-only vs `gate_edits` review-before-edit)
    /// inside the unified bitmask without weakening any active guard latch.
    pub fn configure_mode(&mut self, plan: bool, gate_edits: bool) -> (bool, bool) {
        if plan {
            self.read_only_mask |= PLAN_RO;
            self.read_only_mask &= !GATE_EDITS;
        } else {
            self.read_only_mask &= !PLAN_RO;
            if gate_edits {
                self.read_only_mask |= GATE_EDITS;
            } else {
                self.read_only_mask &= !GATE_EDITS;
            }
        }
        (self.is_read_only(), self.gates_edits())
    }

    pub fn is_read_only(&self) -> bool {
        (self.read_only_mask & (GUARD_RO | PLAN_RO)) != 0
    }

    pub fn gates_edits(&self) -> bool {
        (self.read_only_mask & GATE_EDITS) != 0
    }

    /// Frozen system prompt instructions owned by the toolset (never varies per turn,
    /// keeping the KV prompt cache prefix 100% stable across a session).
    pub fn static_system_addendum(&self) -> String {
        let mut out = String::from(
            "\nYou run inside the twig harness. Prefer outline/read/edit/write/apply_patch/grep/glob over shell cat/sed/grep; \
             use outline(path=...) to map large files or directories before paginating with read(start_line=..., limit=...); \
             always read an existing file before editing or overwriting it; \
             inspect clipped outputs via read(path=\"artifact:<file>\") or past sessions via read(path=\"session:<sid>\"); \
             use task for long/interactive background commands, parallel subagent batches for exploration/review, and papercut for harness defects. \
             Skills use 3 tiers (.twig/skills > ~/.config/twig/skills, cold vault ~/.local/share/twig-skills-vault); \
             manage via `twig skill <list|enable|disable|add|doctor>`, and load `skill-creator` when authoring skills. \
             User TUI controls: /plan (Shift+Tab), /roles, /operator, /dual-plan, /btw, /walkthrough, /rewind (Esc×2), /undo, /skills, $skill-name, /evolve, /verify, !cmd, @file.",
        );
        if let Some(skills) = &self.skills {
            out.push_str(&skills.static_prompt_block());
        }
        if let Some(mcp) = &self.mcp {
            let names = mcp.server_names_summary();
            if !names.is_empty() {
                out.push_str(&format!(
                    "\nConfigured MCP servers (use mcp tool for tools, prompts/skills, and resources): {names}"
                ));
            }
        }
        out
    }

    /// Dynamic per-turn reminders appended at the tail of `messages` rather than mutating `system[0]`.
    pub fn dynamic_turn_reminders(&self, input: &str) -> Vec<String> {
        let mut items = Vec::new();
        if let Some(skills) = &self.skills {
            if let Some(shortlist) = skills.dynamic_turn_matches(input) {
                items.push(shortlist);
            }
        }
        if (self.read_only_mask & PLAN_RO) != 0 {
            items.push(
                "Plan mode is active (read-only). Explore the codebase, analyze requirements, and propose a concrete step-by-step plan. Do not modify files or run mutating commands.".into(),
            );
        }
        items
    }

    /// System prompt instructions owned by the active tool configuration
    /// (retained for compatibility; combines static + dynamic parts).
    pub fn system_addendum_for(&self, input: &str) -> String {
        let mut out = self.static_system_addendum();
        for rem in self.dynamic_turn_reminders(input) {
            out.push('\n');
            out.push_str(&rem);
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
            Some(reg) => {
                let names = reg.server_names_summary();
                if names.is_empty() {
                    "mcp 0 (paused)".to_owned()
                } else {
                    format!("mcp {names}")
                }
            }
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
                "description": format!("{} Returns at most {} lines. Optional 1-based start_line and limit paginate large files, `artifact:<file>` outputs, or `session:<sid>` transcripts.",
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
                "description": "Create or overwrite a file inside the worktree (existing files must be read first). Prefer edit or apply_patch for surgical changes.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "content": {"type": "string"}
                               },
                               "required": ["path", "content"]}}}),
            json!({"type": "function", "function": {
                "name": "apply_patch",
                "description": "Apply a multi-file patch (`*** Begin Patch` ... `*** End Patch` with `*** Add File: <path>`, `*** Update File: <path>` + `@@` hunks, or `*** Delete File: <path>`). Existing files must be read first.",
                "parameters": {"type": "object",
                               "properties": {
                                   "patch": {"type": "string"}
                               },
                               "required": ["patch"]}}}),
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
                "name": "outline",
                "description": "Codebase structural map & reverse-comprehension tool: (1) list definitions with line numbers for a file/directory; (2) pass `symbol` to trace exact `[def]` site(s) and all incoming `[in <caller>]` call/reference sites; (3) set `deps=true` to map module imports/dependencies; (4) set `lsp=true` to query an installed compiler LSP (`rust-analyzer`/`gopls`/`pyright`/`tsserver`/`clangd`) on demand.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "symbol": {"type": "string"},
                                   "deps": {"type": "boolean"},
                                   "lsp": {"type": "boolean"}
                               }}}}),
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
            json!({"type":"function","function":{"name":"skill","description":"List/search project & global skill metadata (optional query), load one named skill body (`name`), or read a bundled Layer-3 reference file inside that skill (`name` + `file`, e.g. `file=\"references/guide.md\"`).",
                "parameters":{"type":"object","properties":{"name":{"type":"string"},"file":{"type":"string"},"query":{"type":"string"}}}}}),
            json!({"type":"function","function":{"name":"subagent","description":"Delegate a bounded task in an isolated Git worktree; scout/advisor/reviewer are OS read-only. Set fork_context=true to clone current conversation context into the subagent without polluting the main session; pass resume_sid to continue a previous subagent; code/test return sid, diff_stat, and artifact patch. Multiple calls run in parallel.",
                "parameters":{"type":"object","properties":{"role":{"type":"string","enum":["code","test","scout","advisor","reviewer"]},"task":{"type":"string"},"resume_sid":{"type":"string"},"fork_context":{"type":"boolean"},"model":{"type":"string"},"effort":{"type":"string","enum":["off","low","medium","high","xhigh"]}},"required":["role","task"]}}}),
            json!({"type":"function","function":{"name":"task","description":"Manage asynchronous background shell tasks (start a long-running build/test/server, send stdin input, poll status, wait for completion, cancel, or list active tasks).",
                "parameters":{"type":"object","properties":{"action":{"type":"string","enum":["start","send","status","wait","cancel","list"]},"command":{"type":"string"},"id":{"type":"string"},"input":{"type":"string"},"timeout_secs":{"type":"integer"}},"required":["action"]}}}),
        ];
        if self
            .mcp
            .as_ref()
            .is_some_and(|reg| !reg.server_names_summary().is_empty())
        {
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

    pub fn list_mcp(&mut self, params: &Value) -> Result<Value, String> {
        let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
        if action != "list" {
            let msg = McpRegistry::configure_project_file(&self.worktree, params)?;
            self.mcp = McpRegistry::discover(&self.worktree);
            let mut report = match &mut self.mcp {
                Some(reg) => reg.list_report(),
                None => json!({"servers": []}),
            };
            report["notice"] = json!(msg);
            return Ok(report);
        }
        Ok(match &mut self.mcp {
            Some(reg) => reg.list_report(),
            None => json!({"servers": []}),
        })
    }

    pub fn list_tasks(&self) -> Value {
        self.tasks.list()
    }

    pub fn cancel_task(&mut self, params: &Value) -> Result<Value, String> {
        let id = params["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("tasks.cancel requires task id")?;
        let snap = self.tasks.cancel(id)?;
        Ok(json!({
            "cancelled": snap,
            "running": self.tasks.running_count(),
            "tasks": self.tasks.list()["tasks"],
        }))
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
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
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
        }
        ;
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
            "apply_patch" => self.apply_patch(
                call.args.get("patch").and_then(Value::as_str).unwrap_or(""),
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
            "outline" => self.outline(
                call.args.get("path").and_then(Value::as_str),
                call.args.get("symbol").and_then(Value::as_str),
                call.args
                    .get("deps")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                call.args
                    .get("lsp")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
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
                    let sub_file = call.args["file"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    if sub_file.is_none() && !name.contains('/') {
                        if let Some(index) = &self.skills {
                            if let Ok(body) = index.load(name) {
                                return body;
                            }
                        }
                    }
                    let spec = match sub_file {
                        Some(rel) => format!("{name}/{}", rel.trim_start_matches('/')),
                        None => name.to_owned(),
                    };
                    SkillStore::default_for(Some(&self.worktree))
                        .load_any(&spec)
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
            "mcp" => {
                let action = call.args["action"].as_str().unwrap_or("call");
                if self.is_read_only() && matches!(action, "call" | "") {
                    return self.read_only_refusal();
                }
                match &mut self.mcp {
                    Some(reg) => {
                        let server = call.args["server"].as_str().unwrap_or("");
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
                        match reg.invoke(server, action, target, args, control) {
                            Ok(out) => {
                                self.last_exit_code = Some(0);
                                out
                            }
                            Err(e) => format!("mcp error: {e}"),
                        }
                    }
                    None => {
                        "no MCP servers configured in .twig/mcp.json or ~/.config/twig/mcp.json"
                            .into()
                    }
                }
            }
            "subagent" => self.delegate(&call.args, emit),
            "task" => self.run_task(&call.args, control, emit),
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
            return self.read_only_refusal();
        }
        let isolated = if self.isolation {
            match isolation::shell(&self.worktree, raw, self.network, self.is_read_only()) {
                Ok(s) => Some(s),
                Err(e) => return format!("isolation error: {e}"),
            }
        } else {
            None
        };
        let pre_stamps: Vec<(PathBuf, (u128, u64))> = self
            .read_stamps
            .iter()
            .map(|(p, &s)| (p.clone(), s))
            .collect();
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
                if let Some(owner) = out.owner {
                    self.groups.push(owner);
                }
                let mut base = if out.text.trim().is_empty() {
                    format!("(exit {}, no output)", out.exit_code.unwrap_or(-1))
                } else if out.exit_code != Some(0) && !out.timed_out {
                    format!("{}\n[exit {}]", out.text, out.exit_code.unwrap_or(-1))
                } else {
                    out.text
                };
                if !pre_stamps.is_empty() {
                    let canon_root = self
                        .worktree
                        .canonicalize()
                        .unwrap_or_else(|_| self.worktree.clone());
                    let mut changed_rels = Vec::new();
                    for (path, old_stamp) in pre_stamps {
                        if fs::file_stamp(&path) != Some(old_stamp) {
                            let rel = path
                                .strip_prefix(&canon_root)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .into_owned();
                            changed_rels.push(rel);
                        }
                    }
                    changed_rels.sort();
                    if !changed_rels.is_empty() {
                        let shown: Vec<&str> = changed_rels.iter().take(6).map(String::as_str).collect();
                        base.push_str(&format!(
                            "\n[note: this command modified previously read file(s): {} — call read() again before editing]",
                            shown.join(", ")
                        ));
                    }
                }
                base
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

    fn run_task(&mut self, args: &Value, control: &Control, emit: &mut dyn FnMut(Value)) -> String {
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
            "send" => {
                let id = args["id"].as_str().unwrap_or("");
                let input = args["input"].as_str().unwrap_or("");
                self.tasks.send_input(id, input, self.is_read_only())
            }
            "wait" => {
                let id = args["id"].as_str().unwrap_or("");
                let secs = args["timeout_secs"].as_u64().unwrap_or(30);
                let r = self.tasks.wait(id, secs, control);
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
                "invalid task action {other:?}; expected start, send, status, wait, cancel, or list"
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
                concurrency: self.agent_concurrency.max(4),
                timeout: self.timeout.max(Duration::from_secs(300)),
                max_output_bytes: 64_000,
                network: self.network,
            };
            self.agent_pool =
                Some(SubagentPool::new(cfg).map_err(|e| format!("subagent setup: {e}"))?);
        }
        Ok(self.agent_pool.as_ref().unwrap().clone())
    }
}

struct SubagentSpec<'a> {
    role: Role,
    role_name: &'a str,
    task: &'a str,
    resume_sid: Option<&'a str>,
    fork_context: bool,
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
            "advisor" => Role::Advisor,
            "reviewer" => Role::Reviewer,
            _ => return Err("invalid subagent role".into()),
        };
        if self.is_read_only() && matches!(role, Role::Code | Role::Test) {
            return Err("refused: read-only mode only permits scout, advisor, or reviewer subagents".into());
        }
        Ok(SubagentSpec {
            role,
            role_name,
            task: args["task"].as_str().unwrap_or(""),
            resume_sid: args.get("resume_sid").and_then(Value::as_str),
            fork_context: args
                .get("fork_context")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            model: args.get("model").and_then(Value::as_str),
            effort: args.get("effort").and_then(Value::as_str),
        })
    }

    /// Execute a batch of `subagent` tool calls concurrently via `SubagentPool`
    /// while preserving original call order and emitting lifecycle events.
    pub fn run_subagents_batch(
        &mut self,
        calls: &[&ToolCall],
        parent_messages: Option<&[Value]>,
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

        let parent_artifacts = self.artifacts_dir.as_deref();
        let results: Vec<Result<SubagentOutcome<'_>, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = parsed
                .into_iter()
                .map(|item| {
                    let pool = pool.clone();
                    s.spawn(move || {
                        let spec = item?;
                        let started = std::time::Instant::now();
                        let run = pool.run_resumable(
                            spec.role,
                            spec.task,
                            spec.model,
                            spec.effort,
                            spec.resume_sid,
                            parent_artifacts,
                            spec.fork_context.then_some(parent_messages).flatten(),
                        );
                        let elapsed_secs = started.elapsed().as_secs();
                        match run {
                            Ok(r) => {
                                let summary = json!({
                                    "sid": r.sid,
                                    "role": r.role,
                                    "worktree": r.worktree,
                                    "exit_code": r.exit_code,
                                    "timed_out": r.timed_out,
                                    "diff_stat": r.diff_stat,
                                    "patch_artifact": r.patch_artifact,
                                    "approved": r.approved,
                                    "review_summary": r.review_summary,
                                    "blockers": r.blockers,
                                    "suggestions": r.suggestions,
                                    "result": r.result,
                                });
                                Ok(SubagentOutcome {
                                    text: serde_json::to_string_pretty(&summary).unwrap_or_default(),
                                    ok: true,
                                    elapsed_secs,
                                    role_name: spec.role_name,
                                })
                            }
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
                    let err_note = (!done.ok).then(|| {
                        done.text
                            .strip_prefix("subagent error:")
                            .unwrap_or(&done.text)
                            .trim()
                            .lines()
                            .next()
                            .unwrap_or("failed")
                            .chars()
                            .take(48)
                            .collect::<String>()
                    });
                    emit(json!({
                        "ev": "subagent.completed",
                        "role": done.role_name,
                        "ok": done.ok,
                        "elapsed_secs": done.elapsed_secs,
                        "error": err_note
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
        let mut results = self.run_subagents_batch(&[&dummy], None, emit);
        let (text, code) = results
            .pop()
            .unwrap_or_else(|| ("subagent failed".into(), None));
        self.last_exit_code = code;
        text
    }

    pub fn is_parallel_readonly(call: &ToolCall, tier: Tier) -> bool {
        tier == Tier::Auto
            && matches!(
                call.name.as_str(),
                "read" | "grep" | "glob" | "outline" | "skill" | "bash"
            )
    }

    /// Execute consecutive `Tier::Auto` read-only tool calls concurrently while
    /// preserving exact deterministic output ordering and updating `read_stamps` /
    /// subdirectory instruction latches afterward.
    pub fn run_readonly_batch(
        &mut self,
        calls: &[&ToolCall],
        control: &Control,
    ) -> Vec<(String, Option<i32>)> {
        struct RoItem {
            text: String,
            exit_code: Option<i32>,
            touched: Option<(PathBuf, PathBuf)>,
            owner: Option<Owner>,
        }

        let worktree = &self.worktree;
        let artifacts = self.artifacts_dir.as_deref();
        let read_max = self.read_max_lines;
        let skills = self.skills.as_ref();
        let is_ro = self.is_read_only();
        let plan_ro = self.read_only_mask == PLAN_RO;
        let use_isolation = self.isolation;
        let network = self.network;
        let timeout = self.timeout;

        let items: Vec<RoItem> = std::thread::scope(|s| {
            let handles: Vec<_> = calls
                .iter()
                .map(|&call| {
                    s.spawn(move || {
                        if control.cancelled() {
                            return RoItem {
                                text: "cancelled before execution".into(),
                                exit_code: None,
                                touched: None,
                                owner: None,
                            };
                        }
                        match call.name.as_str() {
                            "read" => {
                                let (text, code, touched) = read_stateless(
                                    worktree,
                                    artifacts,
                                    read_max,
                                    call.args.get("path").and_then(Value::as_str).unwrap_or(""),
                                    call.args
                                        .get("start_line")
                                        .and_then(Value::as_u64)
                                        .map(|n| n as usize),
                                    call.args
                                        .get("limit")
                                        .and_then(Value::as_u64)
                                        .map(|n| n as usize),
                                );
                                RoItem {
                                    text,
                                    exit_code: code,
                                    touched,
                                    owner: None,
                                }
                            }
                            "grep" => {
                                let (text, code) = grep_stateless(
                                    worktree,
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
                                );
                                RoItem {
                                    text,
                                    exit_code: code,
                                    touched: None,
                                    owner: None,
                                }
                            }
                            "glob" => {
                                let (text, code) = glob_stateless(
                                    worktree,
                                    call.args
                                        .get("pattern")
                                        .and_then(Value::as_str)
                                        .unwrap_or(""),
                                    call.args.get("path").and_then(Value::as_str),
                                );
                                RoItem {
                                    text,
                                    exit_code: code,
                                    touched: None,
                                    owner: None,
                                }
                            }
                            "outline" => {
                                let (text, code) = outline_stateless(
                                    worktree,
                                    call.args.get("path").and_then(Value::as_str),
                                    call.args.get("symbol").and_then(Value::as_str),
                                    call.args
                                        .get("deps")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    call.args
                                        .get("lsp")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                );
                                RoItem {
                                    text,
                                    exit_code: code,
                                    touched: None,
                                    owner: None,
                                }
                            }
                            "skill" => {
                                let text = if let Some(name) = call.args["name"].as_str() {
                                    if let Some(index) = skills {
                                        if let Ok(body) = index.load(name) {
                                            body
                                        } else {
                                            SkillStore::default_for(Some(worktree))
                                                .load_any(name)
                                                .map(|(b, _)| b)
                                                .unwrap_or_else(|e| format!("skill error: {e}"))
                                        }
                                    } else {
                                        SkillStore::default_for(Some(worktree))
                                            .load_any(name)
                                            .map(|(b, _)| b)
                                            .unwrap_or_else(|e| format!("skill error: {e}"))
                                    }
                                } else if let Some(index) = skills {
                                    match call.args["query"].as_str() {
                                        Some(q) if !q.trim().is_empty() => index.search_json(q),
                                        _ => index.summary_json(),
                                    }
                                } else {
                                    "No project or global skills enabled; check `twig skill list` for cold-vault skills.".into()
                                };
                                RoItem {
                                    text,
                                    exit_code: None,
                                    touched: None,
                                    owner: None,
                                }
                            }
                            "bash" => {
                                let raw = call
                                    .args
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if raw.trim().is_empty() {
                                    return RoItem {
                                        text: "refused: empty command".into(),
                                        exit_code: None,
                                        touched: None,
                                        owner: None,
                                    };
                                }
                                if is_ro && !use_isolation {
                                    let msg = if plan_ro {
                                        "refused: plan mode is active, toolset is read-only"
                                    } else {
                                        "refused: guard is narrow, toolset is read-only"
                                    };
                                    return RoItem {
                                        text: msg.into(),
                                        exit_code: None,
                                        touched: None,
                                        owner: None,
                                    };
                                }
                                let isolated = if use_isolation {
                                    match isolation::shell(worktree, raw, network, is_ro) {
                                        Ok(s) => Some(s),
                                        Err(e) => {
                                            return RoItem {
                                                text: format!("isolation error: {e}"),
                                                exit_code: None,
                                                touched: None,
                                                owner: None,
                                            }
                                        }
                                    }
                                } else {
                                    None
                                };
                                let command = isolated.as_deref().unwrap_or(raw);
                                match exec_run_live(worktree, command, timeout, control, &mut |_| {})
                                {
                                    Ok(out) => {
                                        let text = if out.text.trim().is_empty() {
                                            format!("(exit {}, no output)", out.exit_code.unwrap_or(-1))
                                        } else if out.exit_code != Some(0) && !out.timed_out {
                                            format!(
                                                "{}\n[exit {}]",
                                                out.text,
                                                out.exit_code.unwrap_or(-1)
                                            )
                                        } else {
                                            out.text
                                        };
                                        RoItem {
                                            text,
                                            exit_code: out.exit_code,
                                            touched: None,
                                            owner: out.owner,
                                        }
                                    }
                                    Err(e) => RoItem {
                                        text: format!("spawn: {e}"),
                                        exit_code: None,
                                        touched: None,
                                        owner: None,
                                    },
                                }
                            }
                            other => RoItem {
                                text: format!("no such tool {other:?}"),
                                exit_code: None,
                                touched: None,
                                owner: None,
                            },
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or(RoItem {
                        text: "readonly worker panicked".into(),
                        exit_code: None,
                        touched: None,
                        owner: None,
                    })
                })
                .collect()
        });

        let mut out = Vec::with_capacity(items.len());
        for mut item in items {
            if let Some((root, target)) = item.touched {
                self.record_file_stamp(&target);
                let extra = self.discover_subdir_instructions(&root, &target);
                item.text.push_str(&extra);
            }
            if let Some(owner) = item.owner {
                self.groups.push(owner);
            }
            self.last_exit_code = item.exit_code;
            out.push((item.text, item.exit_code));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "twig-tools-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
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
        assert!(
            read.starts_with("   1→hello\n[truncated after 1 lines"),
            "{read}"
        );
        assert!(t.schema()[1]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("at most 1 lines"));
        let clipped = t.clip(&"A".repeat(3000));
        assert!(clipped.contains("clipped") && clipped.starts_with('A') && clipped.ends_with('A'));

        let art_dir = d.join(".art");
        let mut t2 = Tools::with_config(&d, &cfg).with_artifacts_dir(art_dir);
        let big = (1..=400)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let saved = t2.clip_for_call("c99", &big);
        assert!(saved.contains("artifact:tool-c99.txt"));
        let page = t2.run(&call(
            "read",
            json!({"path": "artifact:tool-c99.txt", "start_line": 200, "limit": 1}),
        ));
        assert!(page.starts_with("line-200"), "{page}");
    }

    #[test]
    fn read_before_edit_progressive_matching_apply_patch_and_subdir_docs_work() {
        let d = dir();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/AGENTS.md"), "Keep sub module pure.\n").unwrap();
        std::fs::write(
            d.join("sub/calc.rs"),
            "fn run() {\n        let x = 1;   \n        let y = 2;\n}\n",
        )
        .unwrap();

        let mut t = Tools::new(&d);
        // Unread existing file must be refused by Read-Before-Edit guard.
        let unread_err = t.run(&call(
            "edit",
            json!({"path": "sub/calc.rs", "old_string": "let x = 1;", "new_string": "let x = 10;"}),
        ));
        assert!(unread_err.contains("has not been read"), "{unread_err}");

        // Reading sub/calc.rs returns line numbers and injects sub/AGENTS.md once.
        let r1 = t.run(&call("read", json!({"path": "sub/calc.rs"})));
        assert!(r1.contains("   1→fn run() {") && r1.contains("Keep sub module pure."), "{r1}");
        let r2 = t.run(&call("read", json!({"path": "sub/calc.rs"})));
        assert!(!r2.contains("Keep sub module pure."), "subdir doc must emit only once");

        // Progressive edit: mismatched indentation (2 spaces vs 8) + missing trailing spaces succeeds and preserves 8-space indent!
        let diff = t.run(&call(
            "edit",
            json!({
                "path": "sub/calc.rs",
                "old_string": "  let x = 1;\n  let y = 2;",
                "new_string": "  let x = 10;\n  let y = 20;"
            }),
        ));
        assert!(diff.contains("+        let x = 10;"), "{diff}");

        // Multi-file apply_patch (Update + Add)
        let patch = "\
*** Begin Patch
*** Update File: sub/calc.rs
@@
 fn run() {
-        let x = 10;
+        let x = 42;
         let y = 20;
*** Add File: sub/extra.txt
+hello patch
*** End Patch";
        let patch_out = t.run(&call("apply_patch", json!({"patch": patch})));
        assert!(
            patch_out.contains("+        let x = 42;") && patch_out.contains("created sub/extra.txt"),
            "{patch_out}"
        );

        // External disk edit invalidates mtime/size stamp until re-read.
        std::fs::write(d.join("sub/extra.txt"), "changed externally\n").unwrap();
        let stale = t.run(&call(
            "edit",
            json!({"path": "sub/extra.txt", "old_string": "changed", "new_string": "updated"}),
        ));
        assert!(stale.contains("modified on disk"), "{stale}");

        // `outline` extracts structural symbols for single files and directories.
        let out_file = t.run(&call("outline", json!({"path": "sub/calc.rs"})));
        assert!(out_file.contains("1: fn run()"), "{out_file}");
        let out_dir = t.run(&call("outline", json!({"path": "sub"})));
        assert!(out_dir.contains("## sub/calc.rs"), "{out_dir}");
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

        let found = t.run(&call(
            "grep",
            json!({"pattern": "saturating_add", "glob": "*.rs"}),
        ));
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

    #[test]
    fn reverse_comprehension_xrefs_deps_binary_summary_secret_grep_and_bash_mtime_notice_work() {
        let d = dir();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(
            d.join("src/engine.rs"),
            "use std::path::Path;\n\n// compute_score comment ignored\npub fn compute_score(x: i32) -> i32 {\n    x * 2\n}\n\npub fn run_pipeline(v: i32) -> i32 {\n    let _msg = \"compute_score string literal ignored\";\n    compute_score(v) + 1\n}\n\npub static DEFAULT_SCORE: i32 = compute_score(3);\n",
        )
        .unwrap();
        std::fs::write(d.join(".env"), "API_TOKEN=super-secret-prod\n").unwrap();
        std::fs::write(d.join(".env.example"), "API_TOKEN=placeholder\n").unwrap();
        // Fake 64-bit little-endian x86_64 ELF executable header (20+ bytes)
        let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        elf.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        elf.extend_from_slice(&0x3Eu16.to_le_bytes()); // EM_X86_64
        elf.extend_from_slice(&[0u8; 12]);
        std::fs::write(d.join("artifact.bin"), &elf).unwrap();

        let mut t = Tools::new(&d);

        // 1. Caller/definition xref tracing distinguishes `[def]`, `[in fn run_pipeline]`, and `top-level`
        // after `run_pipeline`'s closing `}`, while ignoring comments and string literals.
        let xref = t.run(&call(
            "outline",
            json!({"path": "src", "symbol": "compute_score"}),
        ));
        assert!(xref.contains("[def] src/engine.rs:4:"), "{xref}");
        assert!(
            xref.contains("[in fn run_pipeline] src/engine.rs:10:"),
            "{xref}"
        );
        assert!(
            xref.contains("[in top-level] src/engine.rs:13:"),
            "{xref}"
        );
        assert!(!xref.contains("src/engine.rs:3:"), "{xref}");
        assert!(!xref.contains("src/engine.rs:9:"), "{xref}");

        // Symlink git patches (mode 120000) are rejected.
        assert!(t
            .apply_git_patch(
                "diff --git a/link b/link\nnew file mode 120000\n--- /dev/null\n+++ b/link\n@@ -0,0 +1 @@\n+/etc/passwd\n",
                "evil"
            )
            .is_err());

        // 2. Module dependency extraction works cleanly.
        let deps = t.run(&call("outline", json!({"path": "src/engine.rs", "deps": true})));
        assert!(deps.contains("use std::path::Path"), "{deps}");

        // 3. Binary file read returns safe structured header metadata + paginated 16B hexdump,
        //    Wasm imports/exports, and secret-redacted `.har` API contract distillation.
        let bin_info = t.run(&call("read", json!({"path": "artifact.bin"})));
        assert!(
            bin_info.contains("ELF 64-bit LSB executable (x86_64)") && bin_info.contains("blake3:"),
            "{bin_info}"
        );
        let hex_info = t.run(&call(
            "read",
            json!({"path": "artifact.bin", "start_line": 1, "limit": 1}),
        ));
        assert!(hex_info.contains("00000000  7f 45 4c 46"), "{hex_info}");

        // Minimal Wasm module with import `env::log` and export `run:fn`:
        let wasm_bytes: &[u8] = &[
            0x00, b'a', b's', b'm', 0x01, 0x00, 0x00, 0x00, // magic + v1
            0x02, 0x0B, 0x01, 0x03, b'e', b'n', b'v', 0x03, b'l', b'o', b'g', 0x00, 0x00, // import
            0x07, 0x07, 0x01, 0x03, b'r', b'u', b'n', 0x00, 0x00, // export
        ];
        std::fs::write(d.join("mod.wasm"), wasm_bytes).unwrap();
        let wasm_info = t.run(&call("read", json!({"path": "mod.wasm"})));
        assert!(
            wasm_info.contains("wasm_imports: env::log")
                && wasm_info.contains("wasm_exports: run:fn"),
            "{wasm_info}"
        );

        let har_json = json!({
            "log": {
                "entries": [
                    {
                        "request": {"method": "GET", "url": "https://ex.test/logo.png", "headers": []},
                        "response": {"status": 200, "content": {"mimeType": "image/png", "text": ""}}
                    },
                    {
                        "request": {
                            "method": "POST",
                            "url": "https://ex.test/api/v1/items",
                            "headers": [{"name": "Authorization", "value": "Bearer super-secret-jwt"}],
                            "postData": {"text": "{\"title\":\"demo\",\"count\":3}"}
                        },
                        "response": {
                            "status": 201,
                            "content": {"mimeType": "application/json; charset=utf-8", "text": "{\"id\":42,\"ok\":true}"}
                        }
                    }
                ]
            }
        });
        std::fs::write(d.join("trace.har"), har_json.to_string()).unwrap();
        let har_info = t.run(&call("read", json!({"path": "trace.har"})));
        assert!(
            har_info.contains("1 API call(s), 1 static asset(s) filtered")
                && har_info.contains("Authorization=<redacted>")
                && !har_info.contains("super-secret-jwt")
                && har_info.contains("req:{count, title}"),
            "{har_info}"
        );

        // 4. Broad directory `grep` skips `.env` secrets while still matching `.env.example`.
        let grep_res = t.run(&call("grep", json!({"pattern": "API_TOKEN"})));
        assert!(grep_res.contains(".env.example:1:"), "{grep_res}");
        assert!(!grep_res.contains("super-secret-prod"), "{grep_res}");

        // 5. `bash` modifying a previously `read` file appends immediate staleness guidance.
        let _ = t.run(&call("read", json!({"path": "src/engine.rs"})));
        let bash_out = t.run(&call(
            "bash",
            json!({"command": "printf '// formatted\\n' >> src/engine.rs"}),
        ));
        assert!(
            bash_out.contains("modified previously read file(s): src/engine.rs"),
            "{bash_out}"
        );
    }
}
