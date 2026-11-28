//! Durable GitHub maintenance queue, isolated worktrees, verification, and opt-in shipping.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::HarnessConfig;
use crate::exec::{isolation, run as exec_run, run_process};
use crate::record::Record;
use crate::runtime::subagent::{SubagentConfig, SubagentPool};

const QUEUE_EVENT: &str = "twig.automation.event.v1";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
struct Lock(File);

#[cfg(not(unix))]
struct Lock {
    _file: File,
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn lock(path: &Path) -> std::io::Result<Lock> {
    let lock_path = path.with_extension("lock");
    #[cfg(unix)]
    {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        for _ in 0..500 {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Lock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::WouldBlock {
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(unix))]
    {
        for _ in 0..500 {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(f) => {
                    return Ok(Lock {
                        _file: f,
                        path: lock_path,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "automation state lock busy",
    ))
}

fn rows(path: &Path) -> std::io::Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let verified = Record::verify_rows(path)
        .map_err(|(index, why)| std::io::Error::other(format!("queue record {index}: {why}")))?;
    for row in &verified {
        if row["kind"] != QUEUE_EVENT {
            return Err(std::io::Error::other(
                "unexpected record kind in automation queue",
            ));
        }
    }
    Ok(verified)
}

fn migrate_legacy(path: &Path) -> std::io::Result<()> {
    let legacy = fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (index, line) in legacy
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let row: Value = serde_json::from_str(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("legacy queue line {}: {error}", index + 1),
            )
        })?;
        if row.get("event").and_then(Value::as_str).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("legacy queue line {} has no event", index + 1),
            ));
        }
        events.push(row);
    }
    let temp = path.with_extension("jsonl.migrating");
    let backup = path.with_extension("legacy.jsonl");
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    for event in events {
        Record::open(&temp, "automation")?.append(QUEUE_EVENT, event)?;
    }
    fs::rename(path, &backup)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::rename(&backup, path);
        return Err(error);
    }
    Ok(())
}

fn append_row(path: &Path, row: &Value) -> std::io::Result<()> {
    Record::open(path, "automation")?.append(QUEUE_EVENT, row.clone())?;
    Ok(())
}

fn append(path: &Path, row: &Value) -> std::io::Result<()> {
    let _guard = lock(path)?;
    append_row(path, row)
}

fn default_git() -> PathBuf {
    "git".into()
}
fn default_gh() -> PathBuf {
    "gh".into()
}
fn default_interval() -> u64 {
    300
}
fn default_check_timeout() -> u64 {
    600
}
fn default_lease() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ShippingPermissions {
    pub replies: bool,
    pub push: bool,
    pub create_pr: bool,
    pub merge: bool,
    pub release: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationConfig {
    pub repository: String,
    pub repository_path: PathBuf,
    pub label: String,
    pub state_dir: PathBuf,
    #[serde(default = "default_interval")]
    pub poll_interval_secs: u64,
    pub include_pull_requests: bool,
    pub check_command: String,
    #[serde(default = "default_check_timeout")]
    pub check_timeout_secs: u64,
    #[serde(default = "default_lease")]
    pub lease_secs: u64,
    pub permissions: ShippingPermissions,
    pub reply: Option<String>,
    pub create_pr: bool,
    pub merge: bool,
    pub release_tag: Option<String>,
    #[serde(default = "default_git")]
    pub git_path: PathBuf,
    #[serde(default = "default_gh")]
    pub gh_path: PathBuf,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            repository: String::new(),
            repository_path: ".".into(),
            label: "twig".into(),
            state_dir: "../.twig-automation".into(),
            poll_interval_secs: 300,
            include_pull_requests: false,
            check_command: String::new(),
            check_timeout_secs: 600,
            lease_secs: 3600,
            permissions: Default::default(),
            reply: None,
            create_pr: false,
            merge: false,
            release_tag: None,
            git_path: default_git(),
            gh_path: default_gh(),
        }
    }
}

impl AutomationConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self
            .repository
            .split_once('/')
            .is_none_or(|(o, r)| o.is_empty() || r.is_empty() || r.contains('/'))
        {
            return Err("repository must be OWNER/REPO".into());
        }
        if self.label.trim().is_empty() || self.check_command.trim().is_empty() {
            return Err("label and check_command are required".into());
        }
        if self.poll_interval_secs == 0 || self.lease_secs < 30 || self.check_timeout_secs == 0 {
            return Err("poll interval, lease (>=30s), and check timeout must be positive".into());
        }
        if (self.reply.is_some() && !self.permissions.replies)
            || (self.create_pr && (!self.permissions.create_pr || !self.permissions.push))
            || (self.merge && !self.permissions.merge)
            || (self.release_tag.is_some()
                && (!self.permissions.release || !self.permissions.push))
        {
            return Err(
                "shipping action configured without its explicit permission; PR creation and release also require push"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub repository: String,
    pub kind: SourceKind,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
}

impl WorkItem {
    pub fn new(
        repository: impl Into<String>,
        kind: SourceKind,
        number: u64,
        title: impl Into<String>,
        body: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        let repository = repository.into();
        let kind_id = if kind == SourceKind::Issue {
            "issue"
        } else {
            "pr"
        };
        Self {
            id: format!("github:{repository}:{kind_id}:{number}"),
            repository,
            kind,
            number,
            title: title.into(),
            body: body.into(),
            url: url.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub approved: bool,
    pub summary: String,
}

pub trait TaskExecutor {
    fn execute(
        &mut self,
        item: &WorkItem,
        worktree: &Path,
        feedback: Option<&str>,
    ) -> Result<String, String>;
    fn verify(&mut self, item: &WorkItem, worktree: &Path) -> Result<String, String>;
    fn review(&mut self, item: &WorkItem, diff: &str, evidence: &str) -> Result<Review, String>;
}

struct JobLease {
    path: PathBuf,
    owner: String,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl JobLease {
    fn acquire(path: PathBuf, seconds: u64) -> std::io::Result<Option<Self>> {
        fs::create_dir_all(path.parent().unwrap())?;
        let _guard = lock(&path)?;
        if let Ok(body) = fs::read_to_string(&path) {
            let v: Value = serde_json::from_str(&body).map_err(std::io::Error::other)?;
            if v["expires"].as_u64().unwrap_or(u64::MAX) > now() {
                return Ok(None);
            }
        }
        let owner = format!("{}-{}", std::process::id(), now());
        write_json(&path, &json!({"owner": owner, "expires": now() + seconds}))?;
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        let token = owner.clone();
        let thread = std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                rx.recv_timeout(Duration::from_secs((seconds / 3).max(1)))
            {
                let Ok(_g) = lock(&p) else { break };
                let Ok(s) = fs::read_to_string(&p) else {
                    break;
                };
                if serde_json::from_str::<Value>(&s)
                    .ok()
                    .and_then(|v| v["owner"].as_str().map(str::to_owned))
                    .as_deref()
                    != Some(&token)
                {
                    break;
                }
                if write_json(&p, &json!({"owner": token, "expires": now() + seconds})).is_err() {
                    break;
                }
            }
        });
        Ok(Some(Self {
            path,
            owner,
            stop: Some(tx),
            thread: Some(thread),
        }))
    }
}

impl Drop for JobLease {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(_g) = lock(&self.path) {
            if fs::read_to_string(&self.path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v["owner"].as_str().map(str::to_owned))
                .as_deref()
                == Some(&self.owner)
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    serde_json::to_writer(&mut f, value).map_err(std::io::Error::other)?;
    f.sync_all()?;
    fs::rename(tmp, path)
}

fn run_command(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> std::io::Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(p) = cwd {
        cmd.current_dir(p);
    }
    let result = run_process(&mut cmd, &[], timeout, 1_048_576)?;
    if result.timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "automation command timed out",
        ));
    }
    if result.truncated {
        return Err(std::io::Error::other(
            "automation command output exceeded 1 MiB",
        ));
    }
    let code = result.exit_code.unwrap_or(1);
    #[cfg(unix)]
    let status = std::process::ExitStatus::from_raw(code << 8);
    #[cfg(windows)]
    let status = std::process::ExitStatus::from_raw(code as u32);
    Ok(Output {
        status,
        stdout: result.stdout,
        stderr: result.stderr,
    })
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

pub struct Automation {
    config: AutomationConfig,
    queue: PathBuf,
    worktrees: PathBuf,
}

impl Automation {
    pub fn open(config: AutomationConfig) -> Result<Self, String> {
        config.validate()?;
        let repo = config
            .repository_path
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let check = run_command(
            &config.git_path,
            &args(&[
                "-C",
                repo.to_string_lossy().as_ref(),
                "rev-parse",
                "--show-toplevel",
            ]),
            None,
            Duration::from_secs(30),
        )
        .map_err(|e| e.to_string())?;
        if !check.status.success() {
            return Err("repository_path is not a Git worktree".into());
        }
        fs::create_dir_all(&config.state_dir).map_err(|e| e.to_string())?;
        let state = config.state_dir.canonicalize().map_err(|e| e.to_string())?;
        let queue = state.join("queue.jsonl");
        let backup = queue.with_extension("legacy.jsonl");
        let temp = queue.with_extension("jsonl.migrating");
        if !queue.exists() && temp.exists() {
            Record::verify_chain(&temp)
                .map_err(|(index, why)| format!("queue migration recovery {index}: {why}"))?;
            fs::rename(&temp, &queue).map_err(|e| e.to_string())?;
        } else if !queue.exists() && backup.exists() {
            fs::rename(&backup, &queue).map_err(|e| e.to_string())?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&queue)
            .map_err(|e| e.to_string())?;
        let _guard = lock(&queue).map_err(|e| e.to_string())?;
        let contents = fs::read_to_string(&queue).map_err(|e| e.to_string())?;
        let first = contents
            .lines()
            .find(|line| !line.trim().is_empty())
            .and_then(|line| serde_json::from_str::<Value>(line).ok());
        if !contents.trim().is_empty() && first.as_ref().is_some_and(|row| row["kind"] != QUEUE_EVENT)
        {
            if first.as_ref().is_some_and(|row| row["kind"].is_string()) {
                return Err("automation queue has an unsupported or corrupt record kind".into());
            }
            migrate_legacy(&queue).map_err(|e| format!("automation queue migration: {e}"))?;
        }
        rows(&queue).map_err(|e| format!("automation queue integrity: {e}"))?;
        drop(_guard);
        let worktrees = state.join("worktrees");
        fs::create_dir_all(&worktrees).map_err(|e| e.to_string())?;
        Ok(Self {
            config: AutomationConfig {
                repository_path: repo,
                state_dir: state,
                ..config
            },
            queue,
            worktrees,
        })
    }

    pub fn ingest(&mut self, items: Vec<WorkItem>) -> Result<usize, String> {
        let _g = lock(&self.queue).map_err(|e| e.to_string())?;
        let mut seen = HashSet::new();
        for row in rows(&self.queue).map_err(|e| e.to_string())? {
            if let Some(id) = row["item"]["id"].as_str() {
                seen.insert(id.to_owned());
            }
        }
        let mut count = 0;
        for item in items {
            let expected = WorkItem::new(&item.repository, item.kind, item.number, "", "", "").id;
            if item.repository != self.config.repository || item.id != expected {
                return Err(format!("invalid source item id {}", item.id));
            }
            if item.kind == SourceKind::PullRequest && !self.config.include_pull_requests {
                continue;
            }
            if seen.insert(item.id.clone()) {
                append_row(
                    &self.queue,
                    &json!({"event": "queued", "at": now(), "item": item}),
                )
                .map_err(|e| e.to_string())?;
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn poll_once(&mut self, executor: &mut dyn TaskExecutor) -> Result<Vec<String>, String> {
        let items = self.fetch_github()?;
        self.ingest(items)?;
        self.run_once(executor)
    }

    pub fn run_once(&mut self, executor: &mut dyn TaskExecutor) -> Result<Vec<String>, String> {
        let _g = lock(&self.queue).map_err(|e| e.to_string())?;
        let mut items: BTreeMap<String, (WorkItem, String)> = BTreeMap::new();
        let mut intents = HashSet::new();
        let mut shipped = HashSet::new();
        for row in rows(&self.queue).map_err(|e| e.to_string())? {
            match row["event"].as_str().unwrap_or("") {
                "queued" => {
                    let i: WorkItem =
                        serde_json::from_value(row["item"].clone()).map_err(|e| e.to_string())?;
                    items.entry(i.id.clone()).or_insert((i, "queued".into()));
                }
                "state" => {
                    if let (Some(id), Some(s)) = (row["id"].as_str(), row["state"].as_str()) {
                        if let Some(x) = items.get_mut(id) {
                            x.1 = s.into();
                        }
                    }
                }
                "shipping_intent" => {
                    if let (Some(id), Some(a)) = (row["id"].as_str(), row["action"].as_str()) {
                        intents.insert(format!("{id}/{a}"));
                    }
                }
                "shipping_result" => {
                    if let (Some(id), Some(a), Some(r)) = (
                        row["id"].as_str(),
                        row["action"].as_str(),
                        row["result"].as_str(),
                    ) {
                        intents.remove(&format!("{id}/{a}"));
                        if r == "confirmed" {
                            shipped.insert(id.to_owned());
                        }
                    }
                }
                e => return Err(format!("invalid automation event {e:?}")),
            }
        }
        drop(_g);
        let mut completed = Vec::new();
        for (item, _previous_state) in items
            .into_values()
            .filter(|(_, s)| !matches!(s.as_str(), "done" | "dropped" | "needs_attention"))
        {
            if shipped.contains(&item.id)
                || intents
                    .iter()
                    .any(|x| x.starts_with(&format!("{}/", item.id)))
            {
                self.state(&item.id, "needs_attention")?;
                continue;
            }
            let key = blake3::hash(item.id.as_bytes()).to_hex().to_string();
            let lease_path = self
                .config
                .state_dir
                .join("leases")
                .join(format!("{key}.json"));
            let Some(_lease) =
                JobLease::acquire(lease_path, self.config.lease_secs).map_err(|e| e.to_string())?
            else {
                continue;
            };
            self.state(&item.id, "claimed")?;
            match self.run_item(&item, executor) {
                Ok(()) => {
                    self.state(&item.id, "done")?;
                    completed.push(item.id);
                }
                Err(e) => {
                    self.state(&item.id, "failed")?;
                    let _ = self.report(&item, &format!("failed: {e}"));
                }
            }
        }
        Ok(completed)
    }

    pub fn watch(&mut self, executor: &mut dyn TaskExecutor) -> Result<(), String> {
        loop {
            self.poll_once(executor)?;
            std::thread::sleep(Duration::from_secs(self.config.poll_interval_secs));
        }
    }

    pub fn resolve_shipping(&self, id: &str, action: &str, completed: bool) -> Result<(), String> {
        append(
            &self.queue,
            &json!({
                "event": "shipping_result",
                "at": now(),
                "id": id,
                "action": action,
                "result": if completed { "confirmed" } else { "not_performed" }
            }),
        )
        .map_err(|e| e.to_string())
    }

    fn state(&self, id: &str, s: &str) -> Result<(), String> {
        append(
            &self.queue,
            &json!({"event": "state", "at": now(), "id": id, "state": s}),
        )
        .map_err(|e| e.to_string())
    }

    fn fetch_github(&self) -> Result<Vec<WorkItem>, String> {
        let mut all = Vec::new();
        for kind in [SourceKind::Issue, SourceKind::PullRequest] {
            if kind == SourceKind::PullRequest && !self.config.include_pull_requests {
                continue;
            }
            let verb = if kind == SourceKind::Issue {
                "issue"
            } else {
                "pr"
            };
            let out = run_command(
                &self.config.gh_path,
                &args(&[
                    verb,
                    "list",
                    "--repo",
                    &self.config.repository,
                    "--label",
                    &self.config.label,
                    "--state",
                    "open",
                    "--limit",
                    "100",
                    "--json",
                    "number,title,body,url",
                ]),
                None,
                Duration::from_secs(60),
            )
            .map_err(|e| format!("gh list: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "gh {verb} list: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            let list: Vec<Value> =
                serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
            for v in list {
                let n = v["number"].as_u64().ok_or("GitHub item missing number")?;
                all.push(WorkItem::new(
                    &self.config.repository,
                    kind,
                    n,
                    v["title"].as_str().unwrap_or(""),
                    v["body"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                ));
            }
        }
        Ok(all)
    }

    fn git(&self, cwd: &Path, a: &[&str]) -> Result<Output, String> {
        let mut av = vec!["-C".to_string(), cwd.to_string_lossy().into_owned()];
        av.extend(args(a));
        let o = run_command(&self.config.git_path, &av, None, Duration::from_secs(60))
            .map_err(|e| e.to_string())?;
        if o.status.success() {
            Ok(o)
        } else {
            Err(format!(
                "git {}: {}",
                a.join(" "),
                String::from_utf8_lossy(&o.stderr)
            ))
        }
    }

    fn git_text(&self, cwd: &Path, a: &[&str]) -> Result<String, String> {
        Ok(String::from_utf8(self.git(cwd, a)?.stdout)
            .map_err(|e| e.to_string())?
            .trim()
            .to_string())
    }

    fn ensure_worktree(&self, item: &WorkItem) -> Result<(PathBuf, String), String> {
        let key = blake3::hash(item.id.as_bytes()).to_hex().to_string();
        let path = self.worktrees.join(&key);
        let basefile = self
            .config
            .state_dir
            .join("bases")
            .join(format!("{key}.txt"));
        if path.exists() {
            let head = self.git_text(&path, &["rev-parse", "--is-inside-work-tree"])?;
            if head != "true" {
                return Err("existing worktree is invalid".into());
            }
            let base =
                fs::read_to_string(&basefile).map_err(|_| "worktree has no recorded baseline")?;
            return Ok((path, base.trim().to_string()));
        }
        let base = self.git_text(&self.config.repository_path, &["rev-parse", "HEAD"])?;
        let branch = format!("twig/{key}");
        self.git(
            &self.config.repository_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                path.to_string_lossy().as_ref(),
            ],
        )?;
        fs::create_dir_all(basefile.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(basefile, &base).map_err(|e| e.to_string())?;
        Ok((path, base))
    }

    fn run_item(&mut self, item: &WorkItem, executor: &mut dyn TaskExecutor) -> Result<(), String> {
        let (worktree, base) = self.ensure_worktree(item)?;
        self.state(&item.id, "running")?;
        let mut feedback = None;
        for _ in 0..3 {
            executor.execute(item, &worktree, feedback.as_deref())?;
            let wrapped = isolation::shell(&worktree, &self.config.check_command, false, false)?;
            let check = exec_run(
                &worktree,
                &wrapped,
                Duration::from_secs(self.config.check_timeout_secs),
            )
            .map_err(|e| e.to_string())?;
            let evidence = format!(
                "command: {}\nexit: {:?}\ntimeout: {}\n{}",
                self.config.check_command, check.exit_code, check.timed_out, check.text
            );
            if check.exit_code != Some(0) || check.timed_out {
                feedback = Some(format!("external check failed:\n{evidence}"));
                continue;
            }
            let verified = executor.verify(item, &worktree)?;
            self.git(&worktree, &["add", "-A"])?;
            let diff = self.git_text(&worktree, &["diff", "--cached", "--binary", &base])?;
            if diff.trim().is_empty() {
                return Err("no changes relative to task baseline".into());
            }
            let review = executor.review(
                item,
                &diff,
                &format!("{evidence}\nverification:\n{verified}"),
            )?;
            if !review.approved {
                feedback = Some(format!("review rejected: {}", review.summary));
                continue;
            }
            if self.git(&worktree, &["config", "user.name"]).is_err() {
                self.git(&worktree, &["config", "user.name", "Twig Automation"])?;
            }
            if self.git(&worktree, &["config", "user.email"]).is_err() {
                self.git(
                    &worktree,
                    &["config", "user.email", "twig-automation@localhost"],
                )?;
            }
            self.git(
                &worktree,
                &[
                    "commit",
                    "-m",
                    &format!(
                        "twig: {} #{}",
                        if item.kind == SourceKind::Issue {
                            "issue"
                        } else {
                            "PR"
                        },
                        item.number
                    ),
                ],
            )?;
            let sha = self.git_text(&worktree, &["rev-parse", "HEAD"])?;
            let patch = self.git_text(&worktree, &["diff", "--binary", &base, &sha])?;
            self.report(
                item,
                &format!(
                    "status: verified and committed\ncommit: {}\nreview: {}\n\ncheck:\n{}",
                    sha.trim(),
                    review.summary,
                    evidence
                ),
            )?;
            return self.ship(item, &worktree, sha.trim(), &patch, &review.summary);
        }
        Err("verification and independent review failed after three attempts".into())
    }

    fn report(&self, item: &WorkItem, text: &str) -> Result<(), String> {
        let dir = self.config.state_dir.join("reports");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{}.md", blake3::hash(item.id.as_bytes()).to_hex())))
            .map_err(|e| e.to_string())?;
        writeln!(f, "# {}\n\n{}\n\n{}", item.title, item.url, text).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())
    }

    fn intent(&self, item: &WorkItem, action: &str, allowed: bool) -> Result<(), String> {
        if !allowed {
            return Err(format!("remote {action} not explicitly permitted"));
        }
        append(
            &self.queue,
            &json!({"event": "shipping_intent", "at": now(), "id": item.id, "action": action}),
        )
        .map_err(|e| e.to_string())
    }

    fn remote(
        &self,
        item: &WorkItem,
        action: &str,
        allowed: bool,
        args: &[String],
    ) -> Result<Output, String> {
        self.intent(item, action, allowed)?;
        let out = run_command(&self.config.gh_path, args, None, Duration::from_secs(120))
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            let e = String::from_utf8_lossy(&out.stderr).into_owned();
            let _ = self.report(item, &format!("{action} requires manual inspection: {e}"));
            return Err(e);
        }
        self.resolve_shipping(&item.id, action, true)?;
        Ok(out)
    }

    fn ship(
        &self,
        item: &WorkItem,
        worktree: &Path,
        sha: &str,
        diff: &str,
        review: &str,
    ) -> Result<(), String> {
        let branch = self.git_text(worktree, &["branch", "--show-current"])?;
        if let Some(body) = &self.config.reply {
            let verb = if item.kind == SourceKind::Issue {
                "issue"
            } else {
                "pr"
            };
            let a = args(&[
                verb,
                "comment",
                &item.number.to_string(),
                "--repo",
                &self.config.repository,
                "--body",
                body,
            ]);
            self.remote(item, "reply", self.config.permissions.replies, &a)?;
        }
        if self.config.merge
            && !(self.config.create_pr
                && self.config.permissions.create_pr
                && self.config.permissions.merge)
        {
            return Err(
                "merge requires explicit create_pr and merge permissions in the same run".into(),
            );
        }
        if !self.config.permissions.push {
            return Ok(());
        }
        self.intent(item, "push", true)?;
        let _ = self.git(
            worktree,
            &["push", "--set-upstream", "origin", branch.trim()],
        )?;
        self.resolve_shipping(&item.id, "push", true)?;

        let mut created_pr: Option<String> = None;
        if self.config.create_pr && self.config.permissions.create_pr {
            let body = format!("Verified commit `{sha}`\n\n{review}\n\n```diff\n{diff}\n```");
            let a = args(&[
                "pr",
                "create",
                "--repo",
                &self.config.repository,
                "--head",
                branch.trim(),
                "--title",
                &format!("{} (#{})", item.title, item.number),
                "--body",
                &body,
            ]);
            let o = self.remote(item, "create_pr", self.config.permissions.create_pr, &a)?;
            created_pr = Some(String::from_utf8_lossy(&o.stdout).trim().to_owned());
        }
        if self.config.merge
            && self.config.create_pr
            && self.config.permissions.merge
            && self.config.permissions.create_pr
        {
            let target = created_pr
                .as_deref()
                .filter(|url| !url.is_empty())
                .ok_or("merge target PR was not returned by explicit PR creation")?;
            let a = args(&[
                "pr",
                "merge",
                target,
                "--repo",
                &self.config.repository,
                "--squash",
            ]);
            self.remote(item, "merge", self.config.permissions.merge, &a)?;
        }
        if let Some(tag) = &self.config.release_tag {
            let a = args(&[
                "release",
                "create",
                tag,
                "--repo",
                &self.config.repository,
                "--target",
                branch.trim(),
                "--title",
                tag,
                "--notes",
                &format!("Verified Twig change for {}", item.url),
            ]);
            self.remote(item, "release", self.config.permissions.release, &a)?;
        }
        Ok(())
    }
}

/// Runs a fresh Twig session per author/reviewer call; the reviewer receives only diff and evidence.
pub struct TwigExecutor {
    executable: PathBuf,
    runs: PathBuf,
    check: String,
    task_timeout: Duration,
    check_timeout: Duration,
    sequence: u64,
    repository: Option<PathBuf>,
}

impl TwigExecutor {
    pub fn new(
        executable: impl Into<PathBuf>,
        runs: impl Into<PathBuf>,
        check: impl Into<String>,
        task_timeout: Duration,
        check_timeout: Duration,
    ) -> Self {
        Self {
            executable: executable.into(),
            runs: runs.into(),
            check: check.into(),
            task_timeout,
            check_timeout,
            sequence: 0,
            repository: None,
        }
    }

    fn turn(&mut self, cwd: &Path, input: &str) -> Result<String, String> {
        self.sequence += 1;
        let run_dir = self
            .runs
            .join(format!("{}-{}", std::process::id(), self.sequence));
        fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;
        let config = run_dir.join("config.json");
        let (mut cfg, _) = HarnessConfig::resolve()?;
        cfg.isolation = true;
        cfg.save(&config).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&self.executable);
        cmd.arg("serve")
            .arg(cwd)
            .arg(&run_dir)
            .env("TWIG_CONFIG", &config)
            .env("TWIG_SUBAGENT", "1");
        let req = json!({"id": 1, "method": "turn", "params": {"input": input, "role": "code"}})
            .to_string()
            + "\n";
        let out = run_process(
            &mut cmd,
            req.as_bytes(),
            self.task_timeout.min(Duration::from_secs(600)),
            1_048_576,
        )
        .map_err(|e| e.to_string())?;
        if out.timed_out || out.truncated || out.exit_code != Some(0) {
            return Err("automated author did not complete within its execution bounds".into());
        }
        let response = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|r| r["id"] == 1)
            .ok_or_else(|| {
                format!(
                    "author response missing: {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })?;
        if !response["error"].is_null() {
            return Err(response["error"].to_string());
        }
        if response["result"]["done"] != true || response["result"]["reason"] != "final" {
            return Err("author paused, cancelled or exhausted budget".into());
        }
        Ok(response["result"]["text"].as_str().unwrap_or("").into())
    }
}

impl TaskExecutor for TwigExecutor {
    fn execute(
        &mut self,
        item: &WorkItem,
        worktree: &Path,
        feedback: Option<&str>,
    ) -> Result<String, String> {
        self.repository = Some(worktree.into());
        let input = format!(
            "Implement this GitHub task locally. Do not commit, push, reply, merge, create a PR or release. Leave the checked diff for the parent. External check: {}\n# {}\n{}\n{}\n{}",
            self.check,
            item.title,
            item.url,
            item.body,
            feedback.unwrap_or("")
        );
        self.turn(worktree, &input)
    }

    fn verify(&mut self, _: &WorkItem, _worktree: &Path) -> Result<String, String> {
        let _ = self.check_timeout;
        Ok("checked by isolated automation harness".into())
    }

    fn review(&mut self, item: &WorkItem, diff: &str, evidence: &str) -> Result<Review, String> {
        let repo = self.repository.clone().ok_or("author workspace missing")?;
        let mut config = SubagentConfig::for_repo(repo).map_err(|e| e.to_string())?;
        config.twig = self.executable.clone();
        config.runs_root = self.runs.join("reviewers");
        config.concurrency = 1;
        config.timeout = self.task_timeout.min(Duration::from_secs(600));
        let result = SubagentPool::new(config)
            .map_err(|e| e.to_string())?
            .fresh_review(
                diff,
                &format!("Task: {}\n{}\n{evidence}", item.title, item.body),
            )
            .map_err(|e| e.to_string())?;
        Ok(Review {
            approved: result.approved == Some(true),
            summary: result
                .review_summary
                .unwrap_or_else(|| "reviewer did not produce a valid verdict".into()),
        })
    }
}
