//! Two tools, both confined to the worktree. Limits and descriptions come from
//! `HarnessConfig`, so the improve loop can tune them without touching code.
//!
//! Few tools is the cost model: tool schemas dominate the initial context, and
//! a four-tool harness already sits on the Pareto frontier.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::HarnessConfig;
use crate::provider::ToolCall;

pub struct Tools {
    worktree: PathBuf,
    pub read_only: bool,
    timeout: Duration,
    max_output: usize,
    read_max_lines: usize,
    bash_description: String,
    read_description: String,
    groups: Vec<crate::process::Owner>,
    last_exit_code: Option<i32>,
    isolation: bool,
    network: bool,
    skills: Option<crate::skills::SkillIndex>,
    agent_pool: Option<crate::subagent::SubagentPool>,
    agent_concurrency: usize,
}

impl Tools {
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self::with_config(worktree, &HarnessConfig::default())
    }

    pub fn with_config(worktree: impl Into<PathBuf>, cfg: &HarnessConfig) -> Self {
        let worktree: PathBuf = worktree.into();
        let skills = crate::skills::SkillIndex::discover(&worktree.join(".twig/skills")).ok();
        Self {
            worktree,
            read_only: false,
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
        }
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
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
                "description": format!("{} Returns at most {} lines.",
                                       self.read_description, self.read_max_lines),
                "parameters": {"type": "object",
                               "properties": {"path": {"type": "string"}},
                               "required": ["path"]}}},
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
        format!("{head}\n... [{} bytes clipped] ...\n{tail}", text.len() - self.max_output)
    }

    /// Errors are data: the model gets text back either way.
    pub fn run(&mut self, call: &ToolCall) -> String {
        self.last_exit_code = None;
        match call.name.as_str() {
            "bash" => self.bash(call.args.get("command").and_then(Value::as_str).unwrap_or("")),
            "read" => self.read(call.args.get("path").and_then(Value::as_str).unwrap_or("")),
            "papercut" => {
                let fields = ["component", "attempted", "expected", "actual", "recovery"];
                if fields.iter().all(|f| call.args[*f].as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 4000)) {
                    "papercut recorded".into()
                } else { "invalid papercut: each required field must contain 1..4000 bytes".into() }
            }
            "skill" => match &self.skills {
                Some(index) => match call.args["name"].as_str() {
                    Some(name)=>index.load(name).unwrap_or_else(|e|format!("skill error: {e}")),
                    None=>serde_json::to_string(index.metadata()).unwrap_or_default(),
                },
                None=>"No project skills found under .twig/skills".into(),
            },
            "subagent" => self.delegate(&call.args),
            other => format!("no such tool {other:?}"),
        }
    }

    pub fn skill_metadata(&self) -> String {
        self.skills.as_ref().map(|s|serde_json::to_string(s.metadata()).unwrap_or_default()).unwrap_or_default()
    }

    fn delegate(&mut self, args: &Value) -> String {
        if std::env::var_os("TWIG_SUBAGENT").is_some() { return "subagent recursion is disabled; return a result to the parent".into(); }
        let role=match args["role"].as_str().unwrap_or("code") {
            "code"=>crate::subagent::Role::Code,"test"=>crate::subagent::Role::Test,
            "scout"=>crate::subagent::Role::Scout,"reviewer"=>crate::subagent::Role::Reviewer,
            _=>return "invalid subagent role".into(),
        };
        if self.agent_pool.is_none() {
            let executable=match std::env::current_exe(){Ok(p)=>p,Err(e)=>return e.to_string()};
            let cfg=crate::subagent::SubagentConfig {twig:executable,repository:self.worktree.clone(),
                runs_root:crate::paths::state_dir().join("subagents"),bwrap:"/usr/bin/bwrap".into(),
                concurrency:self.agent_concurrency,timeout:self.timeout,max_output_bytes:64_000,network:self.network};
            match crate::subagent::SubagentPool::new(cfg){Ok(p)=>self.agent_pool=Some(p),Err(e)=>return format!("subagent setup: {e}")}
        }
        match self.agent_pool.as_ref().unwrap().run(role,args["task"].as_str().unwrap_or("")) {
            Ok(r)=>serde_json::to_string(&r).unwrap_or_default(),Err(e)=>format!("subagent error: {e}"),
        }
    }

    pub fn last_exit_code(&self) -> Option<i32> { self.last_exit_code }

    pub fn run_live(&mut self, call: &ToolCall, control: &crate::control::Control,
        emit: &mut dyn FnMut(serde_json::Value)) -> String {
        if call.name != "bash" { return self.run(call); }
        let raw = call.args["command"].as_str().unwrap_or("");
        let isolated = if self.isolation { match crate::isolation::shell(&self.worktree, raw, self.network, self.read_only) {
            Ok(s)=>Some(s), Err(e)=>return format!("isolation error: {e}")
        }} else {None};
        let command = isolated.as_deref().unwrap_or(raw);
        self.last_exit_code = None;
        if self.read_only && !self.isolation { return "refused: read-only session requires OS isolation".into(); }
        let id = call.id.clone();
        match crate::process::run_live(&self.worktree, command, self.timeout, control,
            &mut |text|emit(serde_json::json!({"ev":"tool.output","id":id,"text":text}))) {
            Ok(out) => {
                self.last_exit_code = out.exit_code;
                self.groups.push(out.owner);
                if out.text.is_empty() { format!("(exit {})", out.exit_code.unwrap_or(-1)) } else { out.text }
            }
            Err(e) => format!("tool error: {e}"),
        }
    }

    fn bash(&mut self, command: &str) -> String {
        if self.read_only {
            return "refused: guard is narrow, toolset is read-only".into();
        }
        if command.trim().is_empty() { return "refused: empty command".into(); }
        let isolated = if self.isolation { match crate::isolation::shell(&self.worktree, command, self.network, self.read_only) {
            Ok(s)=>Some(s), Err(e)=>return format!("isolation error: {e}")
        }} else {None};
        let command = isolated.as_deref().unwrap_or(command);
        match crate::process::run(&self.worktree, command, self.timeout) {
            Ok(out) => {
                self.groups.push(out.owner);
                self.last_exit_code = out.exit_code;
                if out.text.trim().is_empty() {
                    format!("(exit {}, no output)", out.exit_code.unwrap_or(-1))
                } else if out.exit_code != Some(0) && !out.timed_out {
                    format!("{}\n[exit {}]", out.text, out.exit_code.unwrap_or(-1))
                } else { out.text }
            }
            Err(e) => format!("spawn: {e}"),
        }
    }

    fn read(&mut self, path: &str) -> String {
        let root = match self.worktree.canonicalize() {
            Ok(p) => p,
            Err(e) => return format!("worktree: {e}"),
        };
        let target = match root.join(path).canonicalize() {
            Ok(p) => p,
            Err(e) => return format!("{path}: {e}"),
        };
        if !target.starts_with(&root) {
            return format!("refused: {path} resolves outside the worktree");
        }
        match std::fs::read_to_string(&target) {
            Ok(text) => {
                self.last_exit_code = Some(0);
                let mut lines = text.lines();
                let mut result = lines.by_ref().take(self.read_max_lines).collect::<Vec<_>>().join("\n");
                if lines.next().is_some() {
                    result.push_str(&format!("\n[truncated after {} lines; use bash with sed -n for later lines]", self.read_max_lines));
                }
                if result.is_empty() { "(empty file)".into() } else { result }
            }
            Err(e) => format!("{path}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "twig-tools-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("note.txt"), "hello\nworld\n").unwrap();
        d
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall { id: "t".into(), name: name.into(), args }
    }

    #[test]
    fn bash_runs_in_the_worktree() {
        let mut t = Tools::new(dir());
        assert!(t.run(&call("bash", json!({"command": "cat note.txt"}))).contains("hello"));
    }

    #[test]
    fn read_is_confined() {
        let mut t = Tools::new(dir());
        assert!(t.run(&call("read", json!({"path": "note.txt"}))).contains("hello"));
        let escape = t.run(&call("read", json!({"path": "../../../etc/passwd"})));
        assert!(escape.starts_with("refused"), "{escape}");
    }

    #[test]
    fn narrow_refuses_writes() {
        let mut t = Tools::new(dir());
        t.read_only = true;
        assert!(t.run(&call("bash", json!({"command": "echo hi"}))).starts_with("refused"));
    }

    #[test]
    fn config_drives_limits_and_schema() {
        let cfg = HarnessConfig { max_output: 1000, read_max_lines: 1, ..HarnessConfig::default() };
        let mut t = Tools::with_config(dir(), &cfg);
        let read = t.run(&call("read", json!({"path": "note.txt"})));
        assert!(read.starts_with("hello\n[truncated after 1 lines"), "{read}");
        assert!(t.schema()[1]["function"]["description"].as_str().unwrap().contains("at most 1 lines"));
        let clipped = t.clip(&"A".repeat(3000));
        assert!(clipped.contains("clipped") && clipped.starts_with('A') && clipped.ends_with('A'));
    }
}
