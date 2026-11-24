//! Callable role-specific `twig serve` subprocesses with fresh worktrees and bwrap.
use crate::execution::run_process;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role { Code, Test, Scout, Reviewer }
impl Role {
    fn as_str(self) -> &'static str { match self { Self::Code => "code", Self::Test => "test", Self::Scout => "scout", Self::Reviewer => "reviewer" } }
    fn read_only(self) -> bool { matches!(self, Self::Scout | Self::Reviewer) }
}
#[derive(Debug, Clone)]
pub struct SubagentConfig {
    pub twig: PathBuf,
    pub repository: PathBuf,
    pub runs_root: PathBuf,
    pub bwrap: PathBuf,
    pub concurrency: usize,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub network: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult { pub role: String, pub worktree: PathBuf, pub output: String, pub exit_code: Option<i32>, pub timed_out: bool, pub truncated: bool, pub events: Vec<Value>, pub result: Option<Value> }
#[derive(Clone)]
pub struct SubagentPool { config: Arc<SubagentConfig>, slots: Arc<Mutex<usize>> }
#[derive(Debug)]
struct Slot(Arc<Mutex<usize>>);
impl Drop for Slot { fn drop(&mut self) { *self.0.lock().unwrap() += 1; } }
impl SubagentPool {
    pub fn new(config: SubagentConfig) -> io::Result<Self> {
        if config.concurrency == 0 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "subagent concurrency must be positive")); }
        if !config.bwrap.is_absolute() || !config.twig.is_absolute() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "twig and bwrap paths must be absolute")); }
        Ok(Self { slots: Arc::new(Mutex::new(config.concurrency)), config: Arc::new(config) })
    }
    fn acquire(&self) -> io::Result<Slot> {
        let mut available = self.slots.lock().map_err(|_| io::Error::other("subagent pool poisoned"))?;
        if *available == 0 { return Err(io::Error::new(io::ErrorKind::WouldBlock, "subagent concurrency limit reached")); }
        *available -= 1; Ok(Slot(self.slots.clone()))
    }
    pub fn run(&self, role: Role, task: &str) -> io::Result<SubagentResult> {
        let _slot = self.acquire()?;
        self.run_inner(role, task)
    }
    fn run_inner(&self, role: Role, task: &str) -> io::Result<SubagentResult> {
        if task.trim().is_empty() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "subagent task is empty")); }
        let repo = self.config.repository.canonicalize()?;
        let git = |args: &[&str]| Command::new("git").args(args).current_dir(&repo).output();
        let status = git(&["status", "--porcelain"])?;
        if !status.status.success() { return Err(io::Error::other("cannot inspect repository status")); }
        // The caller's shared edits are never copied into or modified by the agent.
        std::fs::create_dir_all(&self.config.runs_root)?;
        let root = self.config.runs_root.canonicalize()?;
        let name = format!("twig-agent-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let worktree = root.join(&name);
        let run_dir = root.join(format!("{name}-runs"));
        std::fs::create_dir(&run_dir)?;
        let add = Command::new("git").args(["worktree", "add", "--detach", "--force"]).arg(&worktree).arg("HEAD").current_dir(&repo).output()?;
        if !add.status.success() { let _ = std::fs::remove_dir_all(&run_dir); return Err(io::Error::other(String::from_utf8_lossy(&add.stderr).into_owned())); }
        let result = self.run_in_worktree(role, task, &worktree, &run_dir);
        if result.is_err() {
            let _ = Command::new("git").args(["worktree", "remove", "--force"]).arg(&worktree).current_dir(&repo).output();
            let _ = std::fs::remove_dir_all(&worktree);
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        result
    }
    fn run_in_worktree(&self, role: Role, task: &str, worktree: &Path, run_dir: &Path) -> io::Result<SubagentResult> {
        let worktree = worktree.canonicalize()?;
        let runs = run_dir.canonicalize()?;
        let mut command = Command::new(&self.config.bwrap);
        command.args(["--die-with-parent", "--new-session", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
        command.args(["--dir", "/workspace", "--dir", "/runs", "--dir", "/tool"]);
        command.args(["--ro-bind-try"]).arg(self.config.twig.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "twig path is not UTF-8"))?).arg("/tool/twig");
        for path in ["/home", "/root", "/tmp", "/run/user"] { command.args(["--tmpfs", path]); }
        if role.read_only() { command.args(["--ro-bind"]).arg(&worktree).arg("/workspace"); }
        else { command.args(["--bind"]).arg(&worktree).arg("/workspace"); }
        command.args(["--bind"]).arg(&runs).arg("/runs").args(["--chdir", "/workspace"]);
        let request = format!("{}\n", json!({"id":1,"method":"turn","params":{"input":task,"role":role.as_str()}}));
        command.args(["/tool/twig", "serve", "/workspace", "/runs"])
            .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", "/nonexistent")
            .env("TWIG_BASE_URL", std::env::var("TWIG_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3425/v1"));
        let capture = run_process(&mut command, request.as_bytes(), self.config.timeout, self.config.max_output_bytes)?;
        let mut raw = String::from_utf8_lossy(&capture.stdout).into_owned();
        raw.push_str(&String::from_utf8_lossy(&capture.stderr));
        let mut events = Vec::new(); let mut result_value = None;
        for line in raw.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if value.get("ev").is_some() { events.push(value); }
                else if value["id"] == 1 { result_value = value.get("result").cloned().or_else(|| value.get("error").cloned()); }
            }
        }
        Ok(SubagentResult { role: role.as_str().into(), worktree: worktree.to_path_buf(), output: raw, exit_code: capture.exit_code, timed_out: capture.timed_out, truncated: capture.truncated, events, result: result_value })
    }
    pub fn fresh_review(&self, diff: &str, evidence: &str) -> io::Result<SubagentResult> {
        if diff.trim().is_empty() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "review requires an actual diff")); }
        self.run(Role::Reviewer, &format!("Review this proposed diff; do not edit. Identify correctness and security defects only.\nActual diff:\n{diff}\nVerification evidence:\n{evidence}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unbounded_or_empty_configuration_and_review_without_diff() {
        let base = SubagentConfig { twig: "/usr/bin/twig".into(), repository: "/tmp".into(), runs_root: "/tmp".into(), bwrap: "/usr/bin/bwrap".into(), concurrency: 1, timeout: Duration::from_secs(1), max_output_bytes: 128, network: false };
        let mut invalid = base.clone(); invalid.concurrency = 0;
        assert!(SubagentPool::new(invalid).is_err());
        let pool = SubagentPool::new(base).unwrap();
        assert!(pool.fresh_review(" ", "test failed").is_err());
        let _slot = pool.acquire().unwrap();
        assert_eq!(pool.acquire().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }
}
