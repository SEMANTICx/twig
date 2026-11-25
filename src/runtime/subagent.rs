//! Callable role-specific `twig serve` subprocesses with fresh Git worktrees and bwrap.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::exec::run_process;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Code,
    Test,
    Scout,
    Reviewer,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Test => "test",
            Self::Scout => "scout",
            Self::Reviewer => "reviewer",
        }
    }

    fn read_only(self) -> bool {
        matches!(self, Self::Scout | Self::Reviewer)
    }
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
pub struct SubagentResult {
    pub role: String,
    pub worktree: PathBuf,
    pub run_dir: PathBuf,
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub events: Vec<Value>,
    pub result: Option<Value>,
    pub approved: Option<bool>,
    pub review_summary: Option<String>,
}

#[derive(Clone)]
pub struct SubagentPool {
    config: Arc<SubagentConfig>,
    slots: Arc<Mutex<usize>>,
    seq: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Slot(Arc<Mutex<usize>>);

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.lock().unwrap() += 1;
    }
}

impl SubagentPool {
    pub fn new(config: SubagentConfig) -> io::Result<Self> {
        if config.concurrency == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subagent concurrency must be positive",
            ));
        }
        if !config.bwrap.is_absolute() || !config.twig.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "twig and bwrap paths must be absolute",
            ));
        }
        Ok(Self {
            slots: Arc::new(Mutex::new(config.concurrency)),
            config: Arc::new(config),
            seq: Arc::new(AtomicU64::new(0)),
        })
    }

    fn acquire(&self) -> io::Result<Slot> {
        let mut available = self
            .slots
            .lock()
            .map_err(|_| io::Error::other("subagent pool poisoned"))?;
        if *available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "subagent concurrency limit reached",
            ));
        }
        *available -= 1;
        Ok(Slot(self.slots.clone()))
    }

    pub fn run(&self, role: Role, task: &str) -> io::Result<SubagentResult> {
        self.prepare_and_run(role, "twig-agent", task, None, None, None)
    }

    pub fn run_with_options(
        &self,
        role: Role,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> io::Result<SubagentResult> {
        self.prepare_and_run(role, "twig-agent", task, None, model, effort)
    }

    pub fn fresh_review(&self, diff: &str, evidence: &str) -> io::Result<SubagentResult> {
        if diff.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "review requires an actual diff",
            ));
        }
        let task = format!(
            "Independently review the actual proposed diff in /runs/review.diff and verification evidence below. Do not edit. Return only JSON: {{\"approved\": boolean, \"summary\": string}}. Any correctness or security defect means approved=false.\nEvidence:\n{evidence}"
        );
        self.prepare_and_run(Role::Reviewer, "twig-review", &task, Some(diff), None, None)
    }

    fn prepare_and_run(
        &self,
        role: Role,
        prefix: &str,
        task: &str,
        review_diff: Option<&str>,
        model_override: Option<&str>,
        effort_override: Option<&str>,
    ) -> io::Result<SubagentResult> {
        if task.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subagent task is empty",
            ));
        }
        let _slot = self.acquire()?;
        let repo = self.config.repository.canonicalize()?;
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output()?;
        if !status.status.success() {
            return Err(io::Error::other("cannot inspect repository status"));
        }
        std::fs::create_dir_all(&self.config.runs_root)?;
        let root = self.config.runs_root.canonicalize()?;
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "{prefix}-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let worktree = root.join(&name);
        let run_dir = root.join(format!("{name}-runs"));
        std::fs::create_dir(&run_dir)?;
        if let Some(diff) = review_diff {
            std::fs::write(run_dir.join("review.diff"), diff)?;
        }
        let add = Command::new("git")
            .args(["worktree", "add", "--detach", "--force"])
            .arg(&worktree)
            .arg("HEAD")
            .current_dir(&repo)
            .output()?;
        if !add.status.success() {
            let _ = std::fs::remove_dir_all(&run_dir);
            return Err(io::Error::other(
                String::from_utf8_lossy(&add.stderr).into_owned(),
            ));
        }
        let result =
            self.run_in_worktree(role, task, &worktree, &run_dir, model_override, effort_override);
        let should_clean_worktree = result.is_err()
            || role.read_only()
            || Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&worktree)
                .output()
                .is_ok_and(|o| o.status.success() && o.stdout.is_empty());
        if should_clean_worktree {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&worktree)
                .current_dir(&repo)
                .output();
            let _ = std::fs::remove_dir_all(&worktree);
        }
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        result
    }

    fn run_in_worktree(
        &self,
        role: Role,
        task: &str,
        worktree: &Path,
        run_dir: &Path,
        model_override: Option<&str>,
        effort_override: Option<&str>,
    ) -> io::Result<SubagentResult> {
        let host_worktree = worktree.canonicalize()?;
        let host_runs = run_dir.canonicalize()?;
        let mut command = Command::new(&self.config.bwrap);
        command.args([
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/bin",
            "/bin",
            "--ro-bind",
            "/lib",
            "/lib",
        ]);
        for lib in ["/lib64", "/usr/lib"] {
            if Path::new(lib).exists() {
                command.args(["--ro-bind", lib, lib]);
            }
        }
        command.args([
            "--dir",
            "/etc",
            "--dir",
            "/tool",
            "--dir",
            "/workspace",
            "--dir",
            "/runs",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ]);
        command
            .args(["--ro-bind"])
            .arg(&self.config.twig)
            .arg("/tool/twig");
        for path in ["/etc/ssl", "/etc/resolv.conf"] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        let mount_flag = if role.read_only() {
            "--ro-bind"
        } else {
            "--bind"
        };
        command
            .args([mount_flag])
            .arg(&host_worktree)
            .arg("/workspace");
        command.args(["--bind"]).arg(&host_runs).arg("/runs");
        if role.read_only() && host_runs.join("review.diff").exists() {
            command
                .arg("--ro-bind")
                .arg(host_runs.join("review.diff"))
                .arg("/runs/review.diff");
        }
        if !self.config.network {
            command.arg("--unshare-net");
        }
        command
            .args([
                "--chdir",
                "/workspace",
                "--",
                "/tool/twig",
                "serve",
                "/workspace",
                "/runs",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/tmp")
            .env("TWIG_SUBAGENT", "1");
        for key in [
            "TWIG_MODEL",
            "TWIG_ROLES",
            "TWIG_EFFORT",
            "TWIG_BASE_URL",
            "TWIG_API_KEY",
        ] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        if let Some(m) = model_override.map(str::trim).filter(|s| !s.is_empty()) {
            command.env("TWIG_MODEL", m);
        } else if std::env::var_os("TWIG_MODEL").is_none()
            && std::env::var_os("TWIG_ROLES").is_none()
        {
            if let Some(saved) = crate::llm::Provider::saved_model() {
                command.env("TWIG_MODEL", saved);
            }
        }
        if let Some(e) = effort_override.map(str::trim).filter(|s| !s.is_empty()) {
            command.env("TWIG_EFFORT", e);
        } else if std::env::var_os("TWIG_EFFORT").is_none() {
            if let Some(saved) = crate::llm::Provider::saved_effort() {
                command.env("TWIG_EFFORT", saved);
            }
        }
        let request = format!(
            "{}\n",
            json!({"id":1,"method":"turn","params":{"input":task,"role":role.as_str()}})
        );
        let capture = run_process(
            &mut command,
            request.as_bytes(),
            self.config.timeout,
            self.config.max_output_bytes,
        )?;
        let mut raw = String::from_utf8_lossy(&capture.stdout).into_owned();
        raw.push_str(&String::from_utf8_lossy(&capture.stderr));
        let mut events = Vec::new();
        let mut result_value = None;
        for line in raw.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if value.get("ev").is_some() {
                    events.push(value);
                } else if value["id"] == 1 {
                    result_value = value
                        .get("result")
                        .cloned()
                        .or_else(|| value.get("error").cloned());
                }
            }
        }
        let (approved, review_summary) = if role == Role::Reviewer
            && capture.exit_code == Some(0)
            && !capture.timed_out
            && !capture.truncated
        {
            result_value
                .as_ref()
                .filter(|r| r["done"] == true && r["reason"] == "final")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .map(|review| {
                    (
                        review.get("approved").and_then(Value::as_bool),
                        review
                            .get("summary")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    )
                })
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        Ok(SubagentResult {
            role: role.as_str().into(),
            worktree: host_worktree,
            run_dir: host_runs,
            output: raw,
            exit_code: capture.exit_code,
            timed_out: capture.timed_out,
            truncated: capture.truncated,
            events,
            result: result_value,
            approved,
            review_summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_or_empty_configuration_and_review_without_diff() {
        let base = SubagentConfig {
            twig: "/usr/bin/twig".into(),
            repository: "/tmp".into(),
            runs_root: "/tmp".into(),
            bwrap: "/usr/bin/bwrap".into(),
            concurrency: 1,
            timeout: Duration::from_secs(1),
            max_output_bytes: 128,
            network: false,
        };
        let mut invalid = base.clone();
        invalid.concurrency = 0;
        assert!(SubagentPool::new(invalid).is_err());
        let pool = SubagentPool::new(base).unwrap();
        assert!(pool.fresh_review(" ", "test failed").is_err());
        let _slot = pool.acquire().unwrap();
        assert_eq!(
            pool.acquire().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
