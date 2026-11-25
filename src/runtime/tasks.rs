//! Session-scoped asynchronous background shell tasks.
//!
//! Spawns worktree-confined background shell commands in dedicated process
//! groups, captures bounded combined stdout/stderr in a background drain thread,
//! supports non-busy-wait `wait` via condition variable, and cleans up running
//! process groups on `cancel` or `Drop`.

use std::io::Read;
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::control::Control;
use crate::exec::{isolation, kill_tree, shell_command};

const MAX_TASK_BUFFER_BYTES: usize = 64 * 1024;
const STATUS_TAIL_BYTES: usize = 4 * 1024;
const MAX_WAIT_SECS: u64 = 300;
const MAX_CONCURRENT_TASKS: usize = 8;

fn append_task_chunk(shared_bg: &Arc<(Mutex<TaskState>, Condvar)>, chunk: &[u8]) {
    if let Ok(mut s) = shared_bg.0.lock() {
        let n = chunk.len();
        if s.output.len() + n <= MAX_TASK_BUFFER_BYTES {
            s.output.extend_from_slice(chunk);
        } else {
            s.truncated = true;
            if n >= MAX_TASK_BUFFER_BYTES {
                s.output.clear();
                s.output
                    .extend_from_slice(&chunk[n - MAX_TASK_BUFFER_BYTES..n]);
            } else {
                let drop_n = (s.output.len() + n) - MAX_TASK_BUFFER_BYTES;
                s.output.drain(..drop_n);
                s.output.extend_from_slice(chunk);
            }
        }
    }
}

struct TaskState {
    output: Vec<u8>,
    truncated: bool,
    exit_code: Option<i32>,
    finished: bool,
    cancelled: bool,
    elapsed_secs: u64,
}

struct ManagedTask {
    id: String,
    command: String,
    pid: u32,
    started: Instant,
    shared: Arc<(Mutex<TaskState>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl ManagedTask {
    fn is_running(&self) -> bool {
        self.shared
            .0
            .lock()
            .map(|s| !s.finished)
            .unwrap_or(false)
    }

    fn terminate(&mut self) {
        let already_done = self
            .shared
            .0
            .lock()
            .map(|mut s| {
                if s.finished {
                    true
                } else {
                    s.cancelled = true;
                    false
                }
            })
            .unwrap_or(true);
        if !already_done {
            kill_tree(self.pid);
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }

    fn snapshot(&self, include_tail: bool) -> Value {
        let state = self.shared.0.lock().unwrap();
        let status = if !state.finished {
            "running"
        } else if state.cancelled {
            "cancelled"
        } else if state.exit_code == Some(0) {
            "completed"
        } else {
            "failed"
        };
        let elapsed = if state.finished {
            state.elapsed_secs
        } else {
            self.started.elapsed().as_secs()
        };
        if include_tail {
            let start = state.output.len().saturating_sub(STATUS_TAIL_BYTES);
            let tail = String::from_utf8_lossy(&state.output[start..]).into_owned();
            json!({
                "id": self.id,
                "command": self.command,
                "status": status,
                "exit_code": state.exit_code,
                "elapsed_secs": elapsed,
                "output_bytes": state.output.len(),
                "truncated": state.truncated,
                "output": tail
            })
        } else {
            json!({
                "id": self.id,
                "command": self.command,
                "status": status,
                "exit_code": state.exit_code,
                "elapsed_secs": elapsed,
                "output_bytes": state.output.len()
            })
        }
    }
}

pub struct TaskManager {
    worktree: PathBuf,
    isolation: bool,
    network: bool,
    next_id: u64,
    tasks: Vec<ManagedTask>,
}

impl TaskManager {
    pub fn new(worktree: PathBuf, isolation: bool, network: bool) -> Self {
        Self {
            worktree,
            isolation,
            network,
            next_id: 0,
            tasks: Vec::new(),
        }
    }

    pub fn running_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.is_running()).count()
    }

    pub fn start(&mut self, raw_command: &str, read_only: bool) -> Result<Value, String> {
        let trimmed = raw_command.trim();
        if trimmed.is_empty() {
            return Err("task start requires a non-empty command".into());
        }
        if read_only {
            return Err("refused: read-only mode does not allow starting background tasks".into());
        }
        if self.running_count() >= MAX_CONCURRENT_TASKS {
            return Err(format!(
                "background task limit reached ({MAX_CONCURRENT_TASKS} running); wait or cancel an existing task first"
            ));
        }

        let isolated = if self.isolation {
            Some(
                isolation::shell(&self.worktree, trimmed, self.network, false)
                    .map_err(|e| format!("isolation error: {e}"))?,
            )
        } else {
            None
        };
        let effective = isolated.as_deref().unwrap_or(trimmed);

        let shared = Arc::new((
            Mutex::new(TaskState {
                output: Vec::new(),
                truncated: false,
                exit_code: None,
                finished: false,
                cancelled: false,
                elapsed_secs: 0,
            }),
            Condvar::new(),
        ));
        let started = Instant::now();

        #[cfg(unix)]
        let (pid, worker) = {
            let (mut reader, writer) = UnixStream::pair().map_err(|e| format!("pipe: {e}"))?;
            let out_fd: OwnedFd = writer
                .try_clone()
                .map_err(|e| format!("dup stdout: {e}"))?
                .into();
            let err_fd: OwnedFd = writer.into();
            let mut cmd = shell_command(effective);
            let mut child = cmd
                .current_dir(&self.worktree)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::from(out_fd))
                .stderr(Stdio::from(err_fd))
                .spawn()
                .map_err(|e| format!("spawn task: {e}"))?;
            let pid = child.id();
            let shared_bg = Arc::clone(&shared);
            let worker = std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => append_task_chunk(&shared_bg, &buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let exit_code = child.wait().ok().and_then(|st| st.code());
                if let Ok(mut s) = shared_bg.0.lock() {
                    s.exit_code = exit_code;
                    s.finished = true;
                    s.elapsed_secs = started.elapsed().as_secs();
                    shared_bg.1.notify_all();
                }
            });
            (pid, worker)
        };

        #[cfg(not(unix))]
        let (pid, worker) = {
            let mut cmd = shell_command(effective);
            let mut child = cmd
                .current_dir(&self.worktree)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn task: {e}"))?;
            let pid = child.id();
            let out_pipe = child.stdout.take();
            let err_pipe = child.stderr.take();
            let shared_bg = Arc::clone(&shared);
            let worker = std::thread::spawn(move || {
                let mut readers = Vec::new();
                for pipe in [
                    out_pipe.map(|s| Box::new(s) as Box<dyn Read + Send>),
                    err_pipe.map(|s| Box::new(s) as Box<dyn Read + Send>),
                ]
                .into_iter()
                .flatten()
                {
                    let sb = Arc::clone(&shared_bg);
                    readers.push(std::thread::spawn(move || {
                        let mut r = pipe;
                        let mut buf = [0u8; 4096];
                        while let Ok(n) = r.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            append_task_chunk(&sb, &buf[..n]);
                        }
                    }));
                }
                for h in readers {
                    let _ = h.join();
                }
                let exit_code = child.wait().ok().and_then(|st| st.code());
                if let Ok(mut s) = shared_bg.0.lock() {
                    s.exit_code = exit_code;
                    s.finished = true;
                    s.elapsed_secs = started.elapsed().as_secs();
                    shared_bg.1.notify_all();
                }
            });
            (pid, worker)
        };

        self.next_id += 1;
        let id = format!("bg-{}", self.next_id);

        let task = ManagedTask {
            id: id.clone(),
            command: trimmed.to_owned(),
            pid,
            started,
            shared,
            worker: Some(worker),
        };
        let snap = task.snapshot(false);
        self.tasks.push(task);
        Ok(snap)
    }

    pub fn status(&self, id: &str) -> Result<Value, String> {
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.snapshot(true))
            .ok_or_else(|| format!("unknown background task {id:?}"))
    }

    pub fn wait(&self, id: &str, timeout_secs: u64, control: &Control) -> Result<Value, String> {
        let task = self
            .tasks
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown background task {id:?}"))?;
        let timeout = Duration::from_secs(timeout_secs.clamp(1, MAX_WAIT_SECS));
        let deadline = Instant::now() + timeout;
        let mut guard = task.shared.0.lock().unwrap();
        while !guard.finished {
            if control.cancelled() {
                drop(guard);
                return Err("wait interrupted by cancellation".into());
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                break;
            }
            let slice = rem.min(Duration::from_millis(100));
            let (next, _) = task.shared.1.wait_timeout(guard, slice).unwrap();
            guard = next;
        }
        drop(guard);
        Ok(task.snapshot(true))
    }

    pub fn cancel(&mut self, id: &str) -> Result<Value, String> {
        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown background task {id:?}"))?;
        task.terminate();
        Ok(task.snapshot(true))
    }

    pub fn list(&self) -> Value {
        let items: Vec<Value> = self.tasks.iter().map(|t| t.snapshot(false)).collect();
        json!({
            "running": self.running_count(),
            "tasks": items
        })
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        for task in &mut self.tasks {
            task.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_task_lifecycle_start_wait_cancel_and_read_only() {
        let dir = std::env::temp_dir().join(format!("twig-bg-task-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut mgr = TaskManager::new(dir.clone(), false, true);

        // Read-only refuses start.
        assert!(mgr.start("echo nope", true).is_err());

        let ctrl = Control::default();
        // Start + wait completes cleanly with output.
        let started = mgr.start("echo hello-bg", false).unwrap();
        let id = started["id"].as_str().unwrap().to_owned();
        let done = mgr.wait(&id, 5, &ctrl).unwrap();
        assert_eq!(done["status"], "completed");
        assert_eq!(done["exit_code"], 0);
        assert!(done["output"].as_str().unwrap().contains("hello-bg"));

        // Start a long sleep + verify wait obeys cancellation immediately, then cancel cleanly.
        let slow = mgr.start("sleep 30", false).unwrap();
        let slow_id = slow["id"].as_str().unwrap().to_owned();
        assert_eq!(mgr.running_count(), 1);
        let cancel_ctrl = Control::default();
        cancel_ctrl.cancel();
        assert!(mgr.wait(&slow_id, 30, &cancel_ctrl).is_err());
        let cancelled = mgr.cancel(&slow_id).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(mgr.running_count(), 0);
        assert_eq!(mgr.list()["tasks"].as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
