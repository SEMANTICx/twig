//! Worktree-confined tool execution with a narrow external interface.
//!
//! Access restrictions (`read_only_mask` combining permanent guard narrowing and
//! user-toggled `/plan` mode) stay private behind behavioral methods, and skill
//! metadata is queried via `SkillIndex::summary_json()`, avoiding field leaks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::guidance::SkillIndex;
use super::subagent::{Role, SubagentConfig, SubagentPool};
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
    agent_pool: Option<SubagentPool>,
    agent_concurrency: usize,
    undo_stack: Vec<FileSnapshot>,
}

impl Tools {
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self::with_config(worktree, &HarnessConfig::default())
    }

    pub fn with_config(worktree: impl Into<PathBuf>, cfg: &HarnessConfig) -> Self {
        let worktree: PathBuf = worktree.into();
        let skills = SkillIndex::discover_default(&worktree);
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
            agent_pool: None,
            agent_concurrency: cfg.subagent_concurrency,
            undo_stack: Vec::new(),
        }
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

    /// System prompt instructions owned by the active tool configuration (skills + plan mode).
    pub fn system_addendum(&self) -> String {
        let mut out = String::new();
        let metadata = self.skill_metadata();
        if !metadata.is_empty() {
            out.push_str(&format!(
                "\nAvailable project skills (load body via skill tool only when relevant): {metadata}"
            ));
        }
        if (self.read_only_mask & PLAN_RO) != 0 {
            out.push_str(
                "\nPlan mode is active (read-only). Explore the codebase, analyze requirements, and propose a concrete step-by-step plan. Do not modify files or run mutating commands.",
            );
        }
        out
    }

    pub fn schema(&self) -> Value {
        json!([
            {"type": "function", "function": {
                "name": "bash",
                "description": self.bash_description,
                "parameters": {"type": "object",
                               "properties": {"command": {"type": "string"}},
                               "required": ["command"]}}},
            {"type": "function", "function": {
                "name": "read",
                "description": format!("{} Returns at most {} lines. Optional 1-based start_line and limit paginate large files.",
                                       self.read_description, self.read_max_lines),
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "start_line": {"type": "integer"},
                                   "limit": {"type": "integer"}
                               },
                               "required": ["path"]}}},
            {"type": "function", "function": {
                "name": "edit",
                "description": "Replace an exact substring inside an existing file in the worktree. Fails unless old_string matches uniquely (or replace_all is true). Returns a unified diff.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "old_string": {"type": "string"},
                                   "new_string": {"type": "string"},
                                   "replace_all": {"type": "boolean"}
                               },
                               "required": ["path", "old_string", "new_string"]}}},
            {"type": "function", "function": {
                "name": "write",
                "description": "Create or overwrite a file inside the worktree. Prefer edit for surgical changes to existing files.",
                "parameters": {"type": "object",
                               "properties": {
                                   "path": {"type": "string"},
                                   "content": {"type": "string"}
                               },
                               "required": ["path", "content"]}}},
            {"type": "function", "function": {
                "name": "grep",
                "description": "Search text across files in the worktree (skips .git, target, node_modules). Returns path:line: text matches.",
                "parameters": {"type": "object",
                               "properties": {
                                   "pattern": {"type": "string"},
                                   "path": {"type": "string"},
                                   "glob": {"type": "string"},
                                   "ignore_case": {"type": "boolean"}
                               },
                               "required": ["pattern"]}}},
            {"type": "function", "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern (*, **, ?) inside the worktree.",
                "parameters": {"type": "object",
                               "properties": {
                                   "pattern": {"type": "string"},
                                   "path": {"type": "string"}
                               },
                               "required": ["pattern"]}}},
            {"type": "function", "function": {
                "name": "papercut",
                "description": "Report a concrete tool, skill or harness defect you encountered. Give what you tried, expected, observed, and a recovery or proposed fix. Report once per issue, then continue working.",
                "parameters": {"type":"object", "additionalProperties": false,
                    "properties": {
                        "component":{"type":"string"}, "attempted":{"type":"string"},
                        "expected":{"type":"string"}, "actual":{"type":"string"},
                        "recovery":{"type":"string"}},
                    "required":["component","attempted","expected","actual","recovery"]}}},
            {"type":"function","function":{"name":"skill","description":"List project skill metadata, or load one named skill only when needed.",
                "parameters":{"type":"object","properties":{"name":{"type":"string"}}}}},
            {"type":"function","function":{"name":"subagent","description":"Delegate a bounded task in a fresh isolated Git worktree; reviewer/scout are OS read-only. Returned worktree changes are not automatically applied.",
                "parameters":{"type":"object","properties":{"role":{"type":"string","enum":["code","test","scout","reviewer"]},"task":{"type":"string"}},"required":["role","task"]}}}
        ])
    }

    /// Clip at the tool boundary: head and tail survive, the middle is elided.
    pub fn clip(&self, text: &str) -> String {
        if text.len() <= self.max_output {
            return text.to_string();
        }
        let half = self.max_output / 2;
        let head: String = text.chars().take(half).collect();
        let tail: Vec<char> = text.chars().rev().take(half).collect();
        let tail: String = tail.into_iter().rev().collect();
        format!(
            "{head}\n... [{} bytes clipped] ...\n{tail}",
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
            "skill" => match &self.skills {
                Some(index) => match call.args["name"].as_str() {
                    Some(name) => index
                        .load(name)
                        .unwrap_or_else(|e| format!("skill error: {e}")),
                    None => index.summary_json(),
                },
                None => "No project skills found under .twig/skills".into(),
            },
            "subagent" => self.delegate(&call.args),
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

    fn delegate(&mut self, args: &Value) -> String {
        if std::env::var_os("TWIG_SUBAGENT").is_some() {
            return "subagent recursion is disabled; return a result to the parent".into();
        }
        let role = match args["role"].as_str().unwrap_or("code") {
            "code" => Role::Code,
            "test" => Role::Test,
            "scout" => Role::Scout,
            "reviewer" => Role::Reviewer,
            _ => return "invalid subagent role".into(),
        };
        if self.is_read_only() && matches!(role, Role::Code | Role::Test) {
            return "refused: read-only mode only permits scout or reviewer subagents".into();
        }
        if self.agent_pool.is_none() {
            let executable = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => return e.to_string(),
            };
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
            match SubagentPool::new(cfg) {
                Ok(p) => self.agent_pool = Some(p),
                Err(e) => return format!("subagent setup: {e}"),
            }
        }
        match self
            .agent_pool
            .as_ref()
            .unwrap()
            .run(role, args["task"].as_str().unwrap_or(""))
        {
            Ok(r) => serde_json::to_string(&r).unwrap_or_default(),
            Err(e) => format!("subagent error: {e}"),
        }
    }

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
        let (_, target) = match self.resolve_existing_in_worktree(path) {
            Ok(v) => v,
            Err(e) => return e,
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
        let mut t = Tools::with_config(dir(), &cfg);
        let read = t.run(&call("read", json!({"path": "note.txt"})));
        assert!(read.starts_with("hello\n[truncated after 1 lines"), "{read}");
        assert!(t.schema()[1]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("at most 1 lines"));
        let clipped = t.clip(&"A".repeat(3000));
        assert!(clipped.contains("clipped") && clipped.starts_with('A') && clipped.ends_with('A'));
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
