//! Callable role-specific `twig serve` subprocesses with Git worktrees, resumable
//! session IDs, structured diff receipts, `Role::Advisor`, and two-tier `/review`.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::paths;
use crate::exec::run_process_streaming;
use crate::llm::Llm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Code,
    Test,
    Scout,
    Advisor,
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Test => "test",
            Self::Scout => "scout",
            Self::Advisor => "advisor",
            Self::Reviewer => "reviewer",
        }
    }

    pub fn read_only(self) -> bool {
        matches!(self, Self::Scout | Self::Advisor | Self::Reviewer)
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
    pub sid: String,
    pub role: String,
    pub worktree: PathBuf,
    pub run_dir: PathBuf,
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub events: Vec<Value>,
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_stat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_artifact: Option<String>,
    pub approved: Option<bool>,
    pub review_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Value>,
}

#[derive(Clone)]
pub struct SubagentPool {
    config: Arc<SubagentConfig>,
    parent_sid: Option<String>,
    slots: Arc<(Mutex<usize>, Condvar)>,
    seq: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Slot(Arc<(Mutex<usize>, Condvar)>);

impl Drop for Slot {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        if let Ok(mut avail) = lock.lock() {
            *avail += 1;
            cvar.notify_one();
        }
    }
}

/// Detect whether a proposed diff touches security/sandbox/integrity-critical paths,
/// automatically escalating `/review` reasoning effort to `xhigh`.
pub fn diff_touches_sensitive_paths(diff: &str) -> bool {
    const SENSITIVE_MARKERS: &[&str] = &[
        "policy.rs",
        "isolation.rs",
        "exec/",
        "record.rs",
        "auth",
        "secret",
        "token",
        "credential",
        ".github/workflows",
        "Cargo.toml",
        "Cargo.lock",
    ];
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("+++ b/").or_else(|| line.strip_prefix("--- a/")) {
            let low = header.to_ascii_lowercase();
            if SENSITIVE_MARKERS.iter().any(|m| low.contains(&m.to_ascii_lowercase())) {
                return true;
            }
        }
    }
    false
}

fn extract_json_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Some(v);
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end >= start)
        .then(|| serde_json::from_str::<Value>(&trimmed[start..=end]).ok())
        .flatten()
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Path::new(&s).canonicalize().ok()
}

/// Resolve the Git repository to isolate for this subagent:
/// 1. Use `base_repo` directly if it is inside a Git worktree.
/// 2. If `twig` was launched from a parent folder like `$HOME` or `~/Dev` (not a Git repo)
///    and `task` names a child Git repo path (e.g. `/home/user/Dev/twig` or `~/Dev/twig`),
///    resolve to that child Git repository so worktree isolation works and `$HOME` is never mounted.
fn resolve_effective_repo(base_repo: &Path, task: &str) -> io::Result<(PathBuf, bool)> {
    if let Some(top) = git_toplevel(base_repo) {
        return Ok((top, true));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    for raw_tok in task.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';')) {
        let tok = raw_tok.trim_end_matches(['.', ':', '/']);
        if !(tok.starts_with('/') || tok.starts_with("~/")) {
            continue;
        }
        let candidate = if let Some(rel) = tok.strip_prefix("~/") {
            home.join(rel)
        } else {
            PathBuf::from(tok)
        };
        let check_dir = if candidate.is_dir() {
            Some(candidate)
        } else {
            candidate.parent().filter(|p| p.is_dir()).map(Path::to_path_buf)
        };
        if let Some(dir) = check_dir {
            if let Ok(canon) = dir.canonicalize() {
                if canon.starts_with(base_repo) {
                    if let Some(top) = git_toplevel(&canon) {
                        if top.starts_with(base_repo) {
                            return Ok((top, true));
                        }
                    }
                }
            }
        }
    }
    Ok((base_repo.to_path_buf(), false))
}

fn collect_worktree_diff(worktree: &Path) -> (Option<String>, String) {
    let stat = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "HEAD", "--stat"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());
    let patch = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    (stat, patch)
}

/// Hydrate a freshly detached subagent worktree with the parent's uncommitted tracked diff
/// and untracked non-ignored files, then record a local detached baseline commit so
/// `git diff HEAD` inside the subagent reflects ONLY the subagent's own delta!
fn hydrate_from_parent_worktree(repo: &Path, sub_wt: &Path) {
    use std::io::Write;
    let mut dirty = false;
    if let Ok(diff_out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "HEAD", "--binary"])
        .output()
    {
        if diff_out.status.success() && !diff_out.stdout.is_empty() {
            if let Ok(mut child) = Command::new("git")
                .arg("-C")
                .arg(sub_wt)
                .args(["apply", "--whitespace=nowarn", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&diff_out.stdout);
                }
                if child.wait().is_ok_and(|s| s.success()) {
                    dirty = true;
                }
            }
        }
    }

    if let Ok(untracked) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .output()
    {
        if untracked.status.success() {
            for rel in String::from_utf8_lossy(&untracked.stdout)
                .split('\0')
                .filter(|s| !s.is_empty())
            {
                let src = repo.join(rel);
                if super::policy::is_sensitive_secret_path(rel) {
                    continue;
                }
                if let Ok(meta) = std::fs::symlink_metadata(&src) {
                    if !meta.file_type().is_symlink()
                        && meta.is_file()
                        && meta.len() <= 4 * 1024 * 1024
                    {
                        let dst = sub_wt.join(rel);
                        if let Some(parent) = dst.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if std::fs::copy(&src, &dst).is_ok() {
                            dirty = true;
                        }
                    }
                }
            }
        }
    }

    if dirty {
        let _ = Command::new("git")
            .arg("-C")
            .arg(sub_wt)
            .args(["add", "-A"])
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(sub_wt)
            .args([
                "-c",
                "user.name=twig",
                "-c",
                "user.email=twig@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "twig-parent-baseline",
            ])
            .output();
    }
}

#[cfg(target_os = "linux")]
fn bwrap_usable(bwrap: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let map = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = map.lock() {
        if let Some(&cached) = guard.get(bwrap) {
            return cached;
        }
    }
    let ok = bwrap.is_file()
        && Command::new(bwrap)
            .args([
                "--die-with-parent",
                "--new-session",
                "--unshare-pid",
                "--ro-bind",
                "/",
                "/",
                "--",
                "/bin/true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if let Ok(mut guard) = map.lock() {
        guard.insert(bwrap.to_path_buf(), ok);
    }
    ok
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn build_linux_bwrap_command(
    bwrap: &Path,
    twig: &Path,
    role: Role,
    orig_repo: &Path,
    host_worktree: &Path,
    host_runs: &Path,
    sid: &str,
    network: bool,
) -> Command {
    let mut command = Command::new(bwrap);
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
    for sys_dir in ["/lib64", "/usr/lib", "/usr/local", "/opt"] {
        if Path::new(sys_dir).exists() {
            command.args(["--ro-bind", sys_dir, sys_dir]);
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
        "--dir",
        "/tmp/cargo-home",
    ]);
    command.args(["--ro-bind"]).arg(twig).arg("/tool/twig");
    for path in ["/etc/ssl", "/etc/resolv.conf", "/etc/alternatives", "/etc/ld.so.cache"] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }

    let mut path_entries = Vec::new();
    let mut rustup_env: Option<PathBuf> = None;
    let mut has_cargo_cache = false;
    if let Ok(home) = std::env::var("HOME") {
        let home_pb = PathBuf::from(&home);
        // Safe read-only developer toolchains & skill vault only (never ~/.ssh, ~/.aws,
        // ~/.gnupg, ~/.config, or full ~/.local/share which holds keyrings & session logs).
        for rel in [
            ".rustup",
            ".local/bin",
            ".local/share/twig-skills-vault",
            ".local/share/mise",
            ".nvm",
            ".fnm",
            ".volta",
            ".bun",
            ".pyenv",
        ] {
            let p = home_pb.join(rel);
            if p.exists() {
                command.arg("--ro-bind").arg(&p).arg(&p);
            }
        }
        if home_pb.join(".rustup").is_dir() {
            rustup_env = Some(home_pb.join(".rustup"));
        }
        // Writable `/tmp/cargo-home` avoids EROFS on `.package-cache` while sharing cached crates/bins read-only.
        for sub in ["bin", "registry", "git", "config.toml"] {
            let src = home_pb.join(".cargo").join(sub);
            if src.exists() {
                has_cargo_cache = true;
                command
                    .arg("--ro-bind")
                    .arg(&src)
                    .arg(format!("/tmp/cargo-home/{sub}"));
            }
        }
        if home_pb.join(".cargo/bin").is_dir() {
            path_entries.push("/tmp/cargo-home/bin".to_owned());
        }
        for bin_rel in [".local/bin", ".volta/bin", ".bun/bin"] {
            let bp = home_pb.join(bin_rel);
            if bp.is_dir() {
                path_entries.push(bp.display().to_string());
            }
        }
    }
    for sys_bin in ["/usr/local/bin", "/usr/bin", "/bin"] {
        if Path::new(sys_bin).is_dir() {
            path_entries.push(sys_bin.to_owned());
        }
    }

    let mount_flag = if role.read_only() {
        "--ro-bind"
    } else {
        "--bind"
    };
    command
        .args([mount_flag])
        .arg(host_worktree)
        .arg("/workspace");
    // Also mount the isolated worktree at `orig_repo`'s host path inside the mount namespace
    // so prompts referencing absolute project paths (e.g. `/home/user/Dev/repo/...`) resolve
    // seamlessly to the hydrated isolated worktree without exposing `$HOME`.
    let home_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok());
    if orig_repo.is_absolute()
        && orig_repo != Path::new("/")
        && orig_repo != Path::new("/workspace")
        && home_dir.as_deref() != Some(orig_repo)
    {
        command
            .args([mount_flag])
            .arg(host_worktree)
            .arg(orig_repo);
    }
    command.args(["--bind"]).arg(host_runs).arg("/runs");
    if role.read_only() && host_runs.join("review.diff").exists() {
        command
            .arg("--ro-bind")
            .arg(host_runs.join("review.diff"))
            .arg("/runs/review.diff");
    }
    if !network {
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
            sid,
        ])
        .env_clear()
        .env("PATH", path_entries.join(":"))
        .env("HOME", "/tmp")
        .env("TWIG_SUBAGENT", "1");
    if has_cargo_cache {
        command.env("CARGO_HOME", "/tmp/cargo-home");
        if !network {
            command.env("CARGO_NET_OFFLINE", "true");
        }
    }
    if let Some(rh) = rustup_env {
        command.env("RUSTUP_HOME", rh);
    }
    command
}

fn build_external_cli_command(
    cli_spec: &str,
    role: Role,
    task: &str,
    worktree: &Path,
) -> io::Result<Command> {
    let rest = cli_spec.strip_prefix("cli:").unwrap_or(cli_spec);
    let (engine, submodel) = match rest.split_once(':') {
        Some((e, m)) if !m.trim().is_empty() => (e.trim(), Some(m.trim())),
        _ => (rest.trim(), None),
    };
    match engine {
        "codex" => {
            let mut cmd = Command::new("codex");
            cmd.arg("exec")
                .arg("--skip-git-repo-check")
                .arg("-C")
                .arg(worktree);
            if role.read_only() {
                cmd.args(["--sandbox", "read-only"]);
            } else {
                cmd.arg("--full-auto");
            }
            if let Some(m) = submodel {
                cmd.args(["-m", m]);
            }
            cmd.arg(task).current_dir(worktree);
            Ok(cmd)
        }
        "claude" => {
            let mut cmd = Command::new("claude");
            cmd.args(["-p", task]);
            if role.read_only() {
                cmd.args(["--permission-mode", "plan"]);
            } else {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(m) = submodel {
                cmd.args(["--model", m]);
            }
            cmd.current_dir(worktree);
            Ok(cmd)
        }
        "gemini" => {
            let mut cmd = Command::new("gemini");
            cmd.args(["-p", task]);
            if !role.read_only() {
                cmd.arg("--yolo");
            }
            if let Some(m) = submodel {
                cmd.args(["-m", m]);
            }
            cmd.current_dir(worktree);
            Ok(cmd)
        }
        "agy" | "antigravity" => {
            let mut cmd = Command::new("agy");
            cmd.args(["-p", task, "--disable-slash-commands"]);
            if role.read_only() {
                cmd.args(["--mode", "plan"]);
            } else {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(m) = submodel {
                cmd.args(["--model", m]);
            }
            cmd.current_dir(worktree);
            Ok(cmd)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported external CLI runner {other:?}; expected cli:codex, cli:claude, cli:agy, or cli:gemini"),
        )),
    }
}

impl SubagentPool {
    pub fn new(config: SubagentConfig) -> io::Result<Self> {
        Self::new_for_parent(config, None)
    }

    pub fn new_for_parent(
        config: SubagentConfig,
        parent_sid: Option<String>,
    ) -> io::Result<Self> {
        if config.concurrency == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subagent concurrency must be positive",
            ));
        }
        #[cfg(target_os = "linux")]
        if !config.bwrap.is_absolute() || !config.twig.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "twig and bwrap paths must be absolute",
            ));
        }
        #[cfg(not(target_os = "linux"))]
        if !config.twig.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "twig path must be absolute",
            ));
        }
        Ok(Self {
            slots: Arc::new((Mutex::new(config.concurrency), Condvar::new())),
            config: Arc::new(config),
            parent_sid: parent_sid.filter(|s| !s.trim().is_empty()),
            seq: Arc::new(AtomicU64::new(0)),
        })
    }

    #[cfg(test)]
    fn acquire(&self) -> io::Result<Slot> {
        let (lock, _) = &*self.slots;
        let mut available = lock
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

    fn acquire_blocking(&self) -> io::Result<Slot> {
        let (lock, cvar) = &*self.slots;
        let wait_budget = self
            .config
            .timeout
            .saturating_mul(4)
            .max(Duration::from_secs(600));
        let deadline = std::time::Instant::now() + wait_budget;
        let mut available = lock
            .lock()
            .map_err(|_| io::Error::other("subagent pool poisoned"))?;
        while *available == 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for free subagent slot",
                ));
            }
            let (guard, _) = cvar
                .wait_timeout(available, deadline.saturating_duration_since(now))
                .map_err(|_| io::Error::other("subagent pool poisoned"))?;
            available = guard;
        }
        *available -= 1;
        Ok(Slot(self.slots.clone()))
    }

    pub fn run(&self, role: Role, task: &str) -> io::Result<SubagentResult> {
        self.prepare_and_run(role, "sub", task, None, None, None, None, None, None, None)
    }

    pub fn run_with_options(
        &self,
        role: Role,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> io::Result<SubagentResult> {
        self.prepare_and_run(role, "sub", task, None, model, effort, None, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_resumable(
        &self,
        role: Role,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
        resume_sid: Option<&str>,
        parent_artifacts: Option<&Path>,
        fork_messages: Option<&[Value]>,
    ) -> io::Result<SubagentResult> {
        self.prepare_and_run(
            role,
            "sub",
            task,
            None,
            model,
            effort,
            resume_sid,
            parent_artifacts,
            fork_messages,
            None,
        )
    }

    /// Execute a subagent while streaming out-of-band UI events (`subagent.progress` and
    /// `subagent.stream`) directly from child JSONL stdout without leaking any tokens into
    /// the parent LLM context.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn run_resumable_live(
        &self,
        role: Role,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
        resume_sid: Option<&str>,
        parent_artifacts: Option<&Path>,
        fork_messages: Option<&[Value]>,
        on_event: Option<&(dyn Fn(Value) + Send + Sync)>,
    ) -> io::Result<SubagentResult> {
        self.prepare_and_run(
            role,
            "sub",
            task,
            None,
            model,
            effort,
            resume_sid,
            parent_artifacts,
            fork_messages,
            on_event,
        )
    }

    pub fn fresh_review(&self, diff: &str, evidence: &str) -> io::Result<SubagentResult> {
        if diff.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "review requires an actual diff",
            ));
        }
        let sensitive = diff_touches_sensitive_paths(diff);
        let review_effort = if sensitive { "xhigh" } else { "high" };
        let task = format!(
            "Independently review the proposed diff in /runs/review.diff and verification evidence below. Do not edit files.\n\
             Classify findings into two strict tiers:\n\
             1. `blockers`: ONLY real correctness bugs, security/sandbox escapes, data loss, or broken invariants, each WITH a concrete `file` and `trigger` path.\n\
             2. `suggestions`: non-blocking maintainability, naming, or test-coverage improvements.\n\
             Set `approved: true` if and only if `blockers` is empty.\n\
             Return ONLY valid JSON: {{\"approved\": boolean, \"summary\": string, \"blockers\": [{{\"file\": string, \"trigger\": string, \"issue\": string}}], \"suggestions\": [{{\"file\": string, \"note\": string}}]}}.\n\
             Evidence:\n{evidence}"
        );
        self.prepare_and_run(
            Role::Reviewer,
            "rev",
            &task,
            Some(diff),
            None,
            Some(review_effort),
            None,
            None,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn prepare_and_run(
        &self,
        role: Role,
        prefix: &str,
        task: &str,
        review_diff: Option<&str>,
        model_override: Option<&str>,
        effort_override: Option<&str>,
        resume_sid: Option<&str>,
        parent_artifacts: Option<&Path>,
        fork_messages: Option<&[Value]>,
        on_event: Option<&(dyn Fn(Value) + Send + Sync)>,
    ) -> io::Result<SubagentResult> {
        if task.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subagent task is empty",
            ));
        }
        let _slot = self.acquire_blocking()?;
        let base_repo = self.config.repository.canonicalize()?;
        let (repo, has_git) = resolve_effective_repo(&base_repo, task)?;
        if !has_git && !role.read_only() {
            return Err(io::Error::other(
                "writable subagents require a Git repository; cd into a repo or use scout/reviewer/advisor",
            ));
        }
        let home_canon = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .and_then(|p| p.canonicalize().ok());
        if !has_git && home_canon.as_deref() == Some(repo.as_path()) {
            return Err(io::Error::other(
                "current directory is $HOME (not a Git repo); include target project path in subagent task or cd into the repo",
            ));
        }
        std::fs::create_dir_all(&self.config.runs_root)?;
        let root = self.config.runs_root.canonicalize()?;

        let sid = match resume_sid.map(str::trim).filter(|s| !s.is_empty()) {
            Some(existing) => {
                if !paths::valid_session(existing) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid resume_sid {existing:?}"),
                    ));
                }
                existing.to_owned()
            }
            None => {
                let seq = self.seq.fetch_add(1, Ordering::Relaxed);
                format!(
                    "{prefix}-{}-{:x}-{seq}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                )
            }
        };

        if let Some(cb) = on_event {
            cb(json!({
                "ev": "subagent.progress",
                "sid": sid,
                "activity": "starting…",
                "turns": 0,
                "tool_count": 0,
            }));
        }

        let worktree = if has_git {
            root.join(format!("{sid}-wt"))
        } else {
            repo.clone()
        };
        let run_dir = root.join(format!("{sid}-runs"));
        std::fs::create_dir_all(&run_dir)?;
        if let Some(diff) = review_diff {
            std::fs::write(run_dir.join("review.diff"), diff)?;
        }
        if resume_sid.is_none() {
            if let Some(msgs) = fork_messages.filter(|m| !m.is_empty()) {
                let mut seeded =
                    crate::record::Record::open(run_dir.join(format!("{sid}.jsonl")), sid.clone())?;
                seeded.append(
                    "session.meta",
                    json!({
                        "worktree": worktree.display().to_string(),
                        "parent_sid": self.parent_sid,
                        "role": role.as_str(),
                        "forked_context": true,
                    }),
                )?;
                seeded.append("messages.checkpoint", json!({"messages": msgs}))?;
            }
        }

        let created_worktree = has_git && !worktree.exists();
        if created_worktree {
            let add = Command::new("git")
                .args(["worktree", "add", "--detach", "--force"])
                .arg(&worktree)
                .arg("HEAD")
                .current_dir(&repo)
                .output()?;
            if !add.status.success() {
                if resume_sid.is_none() {
                    let _ = std::fs::remove_dir_all(&run_dir);
                }
                return Err(io::Error::other(
                    String::from_utf8_lossy(&add.stderr).into_owned(),
                ));
            }
            hydrate_from_parent_worktree(&repo, &worktree);
        }

        let mut result = self.run_in_worktree(
            &sid,
            role,
            task,
            &repo,
            &worktree,
            &run_dir,
            model_override,
            effort_override,
            on_event,
        );

        if let Ok(ref mut ok_res) = result {
            if has_git && !role.read_only() {
                let (stat, patch) = collect_worktree_diff(&worktree);
                ok_res.diff_stat = stat;
                if !patch.trim().is_empty() {
                    let fname = format!("subagent-{sid}.patch");
                    let _ = std::fs::write(run_dir.join(&fname), &patch);
                    if let Some(art_dir) = parent_artifacts {
                        if std::fs::create_dir_all(art_dir).is_ok()
                            && std::fs::write(art_dir.join(&fname), &patch).is_ok()
                        {
                            ok_res.patch_artifact = Some(format!("artifact:{fname}"));
                        }
                    }
                }
            }
        }

        if has_git {
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
        }
        if result.is_err() && resume_sid.is_none() {
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        result
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn run_in_worktree(
        &self,
        sid: &str,
        role: Role,
        task: &str,
        orig_repo: &Path,
        worktree: &Path,
        run_dir: &Path,
        model_override: Option<&str>,
        effort_override: Option<&str>,
        on_event: Option<&(dyn Fn(Value) + Send + Sync)>,
    ) -> io::Result<SubagentResult> {
        let host_worktree = worktree.canonicalize()?;
        let host_runs = run_dir.canonicalize()?;
        #[cfg(target_os = "linux")]
        let use_bwrap = bwrap_usable(&self.config.bwrap);
        #[cfg(not(target_os = "linux"))]
        let use_bwrap = false;

        let mut command = {
            #[cfg(target_os = "linux")]
            if use_bwrap {
                build_linux_bwrap_command(
                    &self.config.bwrap,
                    &self.config.twig,
                    role,
                    orig_repo,
                    &host_worktree,
                    &host_runs,
                    sid,
                    self.config.network,
                )
            } else {
                let mut command = Command::new(&self.config.twig);
                command
                    .arg("serve")
                    .arg(&host_worktree)
                    .arg(&host_runs)
                    .arg(sid)
                    .current_dir(&host_worktree)
                    .env("TWIG_SUBAGENT", "1");
                command
            }
            #[cfg(not(target_os = "linux"))]
            {
                let mut command = Command::new(&self.config.twig);
                command
                    .arg("serve")
                    .arg(&host_worktree)
                    .arg(&host_runs)
                    .arg(sid)
                    .current_dir(&host_worktree)
                    .env("TWIG_SUBAGENT", "1");
                command
            }
        };

        let provider = crate::llm::Provider::new();
        let resolved_model = model_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| provider.roles().get(role.as_str()).to_owned());

        // External CLI worker bridge (`cli:codex`, `cli:claude`, `cli:gemini`) running inside
        // the isolated Git worktree with automatic diff & review extraction.
        if resolved_model.starts_with("cli:") {
            if let Some(cb) = on_event {
                cb(json!({
                    "ev": "subagent.progress",
                    "activity": format!("running {resolved_model}"),
                    "turns": 1,
                    "tool_count": 0
                }));
            }
            let effective_task = task.replace(
                "/runs/review.diff",
                &host_runs.join("review.diff").display().to_string(),
            );
            let mut ext_cmd =
                build_external_cli_command(&resolved_model, role, &effective_task, &host_worktree)?;
            if (resolved_model.starts_with("cli:agy")
                || resolved_model.starts_with("cli:antigravity"))
                && effort_override
                    .map(str::trim)
                    .is_some_and(|e| matches!(e, "low" | "medium" | "high" | "xhigh"))
            {
                ext_cmd.args(["--effort", effort_override.unwrap().trim()]);
            }
            let capture = run_process_streaming(
                &mut ext_cmd,
                &[],
                self.config.timeout,
                self.config.max_output_bytes,
                &mut |line| {
                    if let Some(cb) = on_event {
                        let snippet: String = line.chars().take(512).collect();
                        cb(json!({
                            "ev": "subagent.stream",
                            "child": {"ev": "model.delta", "text": format!("{snippet}\n")}
                        }));
                    }
                },
            )?;
            let text = String::from_utf8_lossy(&capture.stdout).trim().to_owned();
            let err_text = String::from_utf8_lossy(&capture.stderr).trim().to_owned();
            let full_text = if text.is_empty() {
                err_text
            } else if err_text.is_empty() {
                text.clone()
            } else {
                format!("{text}\n{err_text}")
            };
            let ok = capture.exit_code == Some(0) && !capture.timed_out;
            let (approved, review_summary, blockers, suggestions) =
                if role == Role::Reviewer && ok {
                    extract_json_object(&text)
                        .map(|review| {
                            let blks = review
                                .get("blockers")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            let sugs = review
                                .get("suggestions")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            let app = review
                                .get("approved")
                                .and_then(Value::as_bool)
                                .unwrap_or(blks.is_empty());
                            let sum = review
                                .get("summary")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            (Some(app && blks.is_empty()), sum, blks, sugs)
                        })
                        .unwrap_or((None, None, Vec::new(), Vec::new()))
                } else {
                    (None, None, Vec::new(), Vec::new())
                };
            return Ok(SubagentResult {
                sid: sid.to_owned(),
                role: role.as_str().into(),
                worktree: host_worktree,
                run_dir: host_runs,
                output: full_text.clone(),
                exit_code: capture.exit_code,
                timed_out: capture.timed_out,
                truncated: capture.truncated,
                events: Vec::new(),
                result: Some(json!({
                    "done": ok,
                    "reason": if ok { "final" } else { "cli_error" },
                    "text": full_text,
                    "runner": resolved_model,
                })),
                diff_stat: None,
                patch_artifact: None,
                approved,
                review_summary,
                blockers,
                suggestions,
            });
        }

        for key in ["TWIG_BASE_URL", "TWIG_API_KEY"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        if let Some(ref p) = self.parent_sid {
            command.env("TWIG_PARENT_SID", p);
        } else if let Ok(value) = std::env::var("TWIG_PARENT_SID") {
            command.env("TWIG_PARENT_SID", value);
        }
        command.env("TWIG_MODEL", &resolved_model);

        let resolved_effort = effort_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| provider.role_effort(role.as_str()))
            .or_else(|| provider.effort());
        if let Some(eff) = resolved_effort {
            command.env("TWIG_EFFORT", eff);
        }

        let mut request = String::new();
        if role.read_only() {
            request.push_str(&format!(
                "{}\n",
                json!({"id":0,"method":"mode.set","params":{"plan":true}})
            ));
        }
        let host_home_prefix = std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|h| format!("{}/", h.trim_end_matches('/')));
        let mut normalized_task = if let Some(ref hp) = host_home_prefix {
            task.replace("~/", hp)
        } else {
            task.to_owned()
        };
        if !use_bwrap {
            normalized_task = normalized_task.replace(
                "/runs/review.diff",
                &host_runs.join("review.diff").display().to_string(),
            );
        }
        let mut effective_task = format!(
            "[Workspace root (cwd): `.` — inspect target files directly via relative paths like `src/...`]\n{normalized_task}"
        );
        if let Some(role_card) = super::blackboard::load_role_note(orig_repo, role.as_str()) {
            effective_task = format!(
                "[Role Protocol Card (.twig/notes/{}.md)]\n{role_card}\n\n{effective_task}",
                role.as_str()
            );
        }
        request.push_str(&format!(
            "{}\n",
            json!({"id":1,"method":"turn","params":{"input":effective_task,"role":role.as_str()}})
        ));
        let mut live_turns: u32 = 0;
        let mut live_tools: u32 = 0;
        let capture = run_process_streaming(
            &mut command,
            request.as_bytes(),
            self.config.timeout,
            self.config.max_output_bytes,
            &mut |line| {
                let Some(cb) = on_event else {
                    return;
                };
                if !line.starts_with('{') || !line.contains("\"ev\"") {
                    return;
                }
                let Ok(mut ev) = serde_json::from_str::<Value>(line) else {
                    return;
                };
                let kind = ev["ev"].as_str().unwrap_or("");
                if kind == "tool.started" {
                    live_tools = live_tools.saturating_add(1);
                }
                if let Some((activity, next_turns)) =
                    summarize_child_progress_event(&ev, live_turns)
                {
                    live_turns = next_turns;
                    cb(json!({
                        "ev": "subagent.progress",
                        "activity": activity,
                        "turns": live_turns,
                        "tool_count": live_tools
                    }));
                }
                if matches!(
                    kind,
                    "turn.started"
                        | "model.reasoning"
                        | "model.delta"
                        | "tool.started"
                        | "tool.output"
                        | "tool.completed"
                        | "model.retry"
                ) {
                    if kind == "tool.output" {
                        if let Some(t) = ev["text"].as_str() {
                            if t.len() > 4096 {
                                let clipped: String = t.chars().take(2048).collect();
                                ev["text"] = Value::String(clipped);
                            }
                        }
                    }
                    cb(json!({
                        "ev": "subagent.stream",
                        "child": ev
                    }));
                }
            },
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
        let (approved, review_summary, blockers, suggestions) = if role == Role::Reviewer
            && capture.exit_code == Some(0)
            && !capture.timed_out
            && !capture.truncated
        {
            result_value
                .as_ref()
                .filter(|r| r["done"] == true && r["reason"] == "final")
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str)
                .and_then(extract_json_object)
                .map(|review| {
                    let blks = review
                        .get("blockers")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let sugs = review
                        .get("suggestions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let app = review
                        .get("approved")
                        .and_then(Value::as_bool)
                        .unwrap_or(blks.is_empty());
                    let sum = review
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    (Some(app && blks.is_empty()), sum, blks, sugs)
                })
                .unwrap_or((None, None, Vec::new(), Vec::new()))
        } else {
            (None, None, Vec::new(), Vec::new())
        };
        Ok(SubagentResult {
            sid: sid.to_owned(),
            role: role.as_str().into(),
            worktree: host_worktree,
            run_dir: host_runs,
            output: raw,
            exit_code: capture.exit_code,
            timed_out: capture.timed_out,
            truncated: capture.truncated,
            events,
            result: result_value,
            diff_stat: None,
            patch_artifact: None,
            approved,
            review_summary,
            blockers,
            suggestions,
        })
    }
}

fn compact_arg_snippet(raw: &str, max_chars: usize) -> String {
    let single = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = single.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        trimmed.to_owned()
    } else {
        let mut s: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn compact_path_tail(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches("./");
    let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    let shown = if parts.len() > 2 {
        parts[parts.len() - 2..].join("/")
    } else {
        trimmed.to_owned()
    };
    compact_arg_snippet(&shown, 32)
}

/// Convert a child `twig serve` JSONL event into a tiny UI-only `(activity, turns)` tuple.
/// Never enters the parent LLM's context; purely for operator progress rendering.
fn summarize_child_progress_event(ev: &Value, current_turns: u32) -> Option<(String, u32)> {
    match ev["ev"].as_str()? {
        "turn.started" => {
            let next = ev["turns_used"]
                .as_u64()
                .map(|n| n as u32 + 1)
                .unwrap_or(current_turns.saturating_add(1));
            Some((format!("thinking (t{next})"), next))
        }
        "model.retry" => Some(("retrying model…".into(), current_turns.max(1))),
        "tool.started" => {
            let tool = ev["tool"].as_str().unwrap_or("tool");
            let args = &ev["args"];
            let label = match tool {
                "read" | "read_file" => {
                    let p = args["path"].as_str().unwrap_or("");
                    if p.is_empty() {
                        "Read".into()
                    } else {
                        format!("Read({})", compact_path_tail(p))
                    }
                }
                "edit" | "edit_file" => {
                    let p = args["path"].as_str().unwrap_or("");
                    if p.is_empty() {
                        "Edit".into()
                    } else {
                        format!("Edit({})", compact_path_tail(p))
                    }
                }
                "write" | "write_file" => {
                    let p = args["path"].as_str().unwrap_or("");
                    if p.is_empty() {
                        "Write".into()
                    } else {
                        format!("Write({})", compact_path_tail(p))
                    }
                }
                "outline" => {
                    let p = args["path"].as_str().unwrap_or("");
                    if p.is_empty() {
                        "Outline".into()
                    } else {
                        format!("Outline({})", compact_path_tail(p))
                    }
                }
                "apply_patch" => "Patch".into(),
                "grep" => {
                    let q = args["pattern"].as_str().unwrap_or("");
                    if q.is_empty() {
                        "Grep".into()
                    } else {
                        format!("Grep({})", compact_arg_snippet(q, 24))
                    }
                }
                "glob" => {
                    let q = args["pattern"].as_str().unwrap_or("");
                    if q.is_empty() {
                        "Glob".into()
                    } else {
                        format!("Glob({})", compact_arg_snippet(q, 24))
                    }
                }
                "list_dir" => {
                    let p = args["path"].as_str().unwrap_or(".");
                    format!("List({})", compact_path_tail(p))
                }
                "bash" => {
                    let c = args["command"].as_str().unwrap_or("");
                    if c.is_empty() {
                        "Bash".into()
                    } else {
                        format!("Bash({})", compact_arg_snippet(c, 28))
                    }
                }
                "skill" => {
                    let n = args["name"].as_str().unwrap_or("");
                    if n.is_empty() {
                        "Skill".into()
                    } else {
                        format!("Skill({n})")
                    }
                }
                "mcp" => {
                    let s = args["server"].as_str().unwrap_or("");
                    let t = args["tool"].as_str().unwrap_or("");
                    if !s.is_empty() && !t.is_empty() {
                        format!("MCP({s}/{t})")
                    } else {
                        "MCP".into()
                    }
                }
                "todo" => "Todo".into(),
                "task" => {
                    let a = args["action"].as_str().unwrap_or("");
                    if a.is_empty() {
                        "Task".into()
                    } else {
                        format!("Task({a})")
                    }
                }
                "web_search" => {
                    let q = args["query"].as_str().unwrap_or("");
                    format!("WebSearch({})", compact_arg_snippet(q, 24))
                }
                "web_fetch" => {
                    let u = args["url"].as_str().unwrap_or("");
                    format!("WebFetch({})", compact_arg_snippet(u, 24))
                }
                other => other.to_owned(),
            };
            Some((label, current_turns.max(1)))
        }
        _ => None,
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
        // `acquire_blocking` waits on Condvar until `_slot` is released instead of failing immediately.
        let pool_wait = pool.clone();
        let waiter = std::thread::spawn(move || pool_wait.acquire_blocking().is_ok());
        std::thread::sleep(Duration::from_millis(25));
        drop(_slot);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn detects_sensitive_diff_paths_and_parses_fenced_review_json() {
        assert!(diff_touches_sensitive_paths(
            "--- a/src/runtime/policy.rs\n+++ b/src/runtime/policy.rs\n@@ -1 +1 @@"
        ));
        assert!(!diff_touches_sensitive_paths(
            "--- a/src/tui/markdown.rs\n+++ b/src/tui/markdown.rs\n@@ -1 +1 @@"
        ));
        let fenced = "```json\n{\"approved\":false,\"summary\":\"bug\",\"blockers\":[{\"file\":\"a.rs\",\"trigger\":\"x=0\",\"issue\":\"div0\"}]}\n```";
        let parsed = extract_json_object(fenced).unwrap();
        assert_eq!(parsed["approved"], false);
        assert_eq!(parsed["blockers"].as_array().unwrap().len(), 1);

        let ro_cmd = build_external_cli_command(
            "cli:codex:o3",
            Role::Advisor,
            "review plan",
            Path::new("/tmp"),
        )
        .unwrap();
        let ro_args: Vec<String> = ro_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(ro_args.contains(&"read-only".to_owned()));
        assert!(ro_args.contains(&"o3".to_owned()));

        let rw_cmd =
            build_external_cli_command("cli:claude:sonnet", Role::Code, "fix bug", Path::new("/tmp"))
                .unwrap();
        let rw_args: Vec<String> = rw_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(rw_args.contains(&"--dangerously-skip-permissions".to_owned()));

        assert_eq!(
            summarize_child_progress_event(&json!({"ev": "turn.started", "turns_used": 2}), 0),
            Some(("thinking (t3)".into(), 3))
        );
        assert_eq!(
            summarize_child_progress_event(
                &json!({"ev": "tool.started", "tool": "read", "args": {"path": "src/runtime/subagent.rs"}}),
                3
            ),
            Some(("Read(runtime/subagent.rs)".into(), 3))
        );
        assert_eq!(
            summarize_child_progress_event(
                &json!({"ev": "tool.started", "tool": "outline", "args": {"path": "src/tui/mod.rs"}}),
                3
            ),
            Some(("Outline(tui/mod.rs)".into(), 3))
        );
        assert_eq!(
            summarize_child_progress_event(
                &json!({"ev": "tool.started", "tool": "bash", "args": {"command": "cargo test --lib"}}),
                3
            ),
            Some(("Bash(cargo test --lib)".into(), 3))
        );
        assert_eq!(
            summarize_child_progress_event(&json!({"ev": "token.delta", "text": "hello"}), 3),
            None
        );
    }
}
