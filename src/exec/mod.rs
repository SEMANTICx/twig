//! Bounded nonblocking process execution, session-owned shells, and OS isolation.
//!
//! Pure execution layer: depends only on `control` for cancellation, never on
//! higher-level verification, tools, or agent loops.

pub mod isolation;

use std::io::{self, ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::control::Control;

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Build a platform-native shell `Command` (`sh -c` on Unix, `cmd /C` on Windows).
pub fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    }
}

#[cfg(unix)]
fn kill_group(id: u32) {
    unsafe {
        libc::kill(-(id as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_group(id: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &id.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn kill_tree(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let children =
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
        for child in children
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            kill_tree(child);
        }
    }
    kill_group(pid);
}

#[cfg(target_os = "linux")]
fn status_without_reaping(child: &mut Child) -> io::Result<Option<i32>> {
    let pid = child.id();
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // WNOWAIT leaves the leader waitable so its numeric PID cannot be recycled
    // while session-owned background children still need cleanup.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let code = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        code
    } else {
        128 + code
    }))
}

#[cfg(not(target_os = "linux"))]
fn status_without_reaping(child: &mut Child) -> io::Result<Option<i32>> {
    Ok(child.try_wait()?.map(|st| st.code().unwrap_or(1)))
}

pub struct Owner {
    child: Child,
    #[cfg(unix)]
    capture: Option<UnixStream>,
    stop: Arc<AtomicBool>,
    drain: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

pub struct Output {
    pub text: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub owner: Owner,
}

fn format_live_output(bytes: &[u8], discarded: bool, timed_out: bool, timeout: Duration) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if discarded {
        text.push_str("\n[capture truncated at 1 MiB]\n");
    }
    if timed_out {
        text = format!(
            "timeout: killed process group after {}s\n{text}",
            timeout.as_secs_f64()
        );
    }
    text
}

fn validate_process_limits(input: &[u8], timeout: Duration, max_output_bytes: usize) -> io::Result<(Duration, usize)> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "process input exceeds 8 MiB",
        ));
    }
    Ok((timeout.min(MAX_TIMEOUT), max_output_bytes.min(MAX_OUTPUT_BYTES)))
}

pub fn run(cwd: &Path, command: &str, timeout: Duration) -> io::Result<Output> {
    run_live(cwd, command, timeout, &Control::default(), &mut |_| {})
}

#[cfg(unix)]
pub fn run_live(
    cwd: &Path,
    command: &str,
    timeout: Duration,
    control: &Control,
    emit: &mut dyn FnMut(String),
) -> io::Result<Output> {
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let capture = reader.try_clone()?;
    let out: OwnedFd = writer.try_clone()?.into();
    let err: OwnedFd = writer.into();
    let mut cmd = shell_command(command);
    let child = cmd
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()?;
    let mut owner = Owner {
        child,
        capture: Some(capture),
        stop: Default::default(),
        drain: None,
    };
    let deadline = Instant::now() + timeout;
    const CAP: usize = 1_048_576;
    let mut bytes = Vec::new();
    let mut discarded = false;
    let mut buf = [0; 8192];
    let mut drain_once = |reader: &mut UnixStream| -> io::Result<()> {
        for _ in 0..128 {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes.len() < CAP {
                        emit(String::from_utf8_lossy(&buf[..n]).into_owned());
                    }
                    let take = n.min(CAP.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&buf[..take]);
                    discarded |= take < n;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    };
    let (code, timed_out) = loop {
        drain_once(&mut reader)?;
        if let Some(code) = status_without_reaping(&mut owner.child)? {
            break (code, false);
        }
        if control.cancelled() {
            kill_group(owner.child.id());
            break (130, false);
        }
        if Instant::now() >= deadline {
            kill_group(owner.child.id());
            break (124, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drain_once(&mut reader)?;
    let mut capture = owner.capture.take().expect("capture");
    let stop = owner.stop.clone();
    owner.drain = Some(std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while !stop.load(Ordering::Relaxed) {
            match capture.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }));
    let text = format_live_output(&bytes, discarded, timed_out, timeout);
    Ok(Output {
        text,
        exit_code: Some(code),
        timed_out,
        owner,
    })
}

#[cfg(not(unix))]
pub fn run_live(
    cwd: &Path,
    command: &str,
    timeout: Duration,
    control: &Control,
    emit: &mut dyn FnMut(String),
) -> io::Result<Output> {
    let mut cmd = shell_command(command);
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    for pipe in [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = pipe;
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut owner = Owner {
        child,
        stop: Default::default(),
        drain: None,
    };
    let deadline = Instant::now() + timeout;
    const CAP: usize = 1_048_576;
    let mut bytes = Vec::new();
    let mut discarded = false;
    let mut drain_rx = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| {
        while let Ok(chunk) = rx.try_recv() {
            if bytes.len() < CAP {
                emit(String::from_utf8_lossy(&chunk).into_owned());
            }
            let take = chunk.len().min(CAP.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..take]);
            discarded |= take < chunk.len();
        }
    };
    let (code, timed_out) = loop {
        drain_rx(&rx);
        if let Some(code) = status_without_reaping(&mut owner.child)? {
            break (code, false);
        }
        if control.cancelled() {
            kill_group(owner.child.id());
            let _ = owner.child.kill();
            break (130, false);
        }
        if Instant::now() >= deadline {
            kill_group(owner.child.id());
            let _ = owner.child.kill();
            break (124, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drain_rx(&rx);
    let text = format_live_output(&bytes, discarded, timed_out, timeout);
    Ok(Output {
        text,
        exit_code: Some(code),
        timed_out,
        owner,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
}

fn default_timeout() -> u64 {
    120_000
}

fn default_output() -> usize {
    1_048_576
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub passed: bool,
}

pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
}

struct ChildOwner(Option<Child>);

impl Drop for ChildOwner {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            kill_tree(child.id());
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn drain_bounded(
    stream: &mut UnixStream,
    bytes: &mut Vec<u8>,
    remaining: &mut usize,
) -> io::Result<bool> {
    let mut buf = [0; 8192];
    let mut truncated = false;
    for _ in 0..32 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(*remaining);
                bytes.extend_from_slice(&buf[..take]);
                *remaining -= take;
                truncated |= take < n;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(truncated)
}

/// Nonblocking bounded stdin/stdout/stderr; terminates all children before reaping.
#[cfg(unix)]
pub fn run_process(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
    max_output_bytes: usize,
) -> io::Result<Capture> {
    let (timeout, max_output_bytes) = validate_process_limits(input, timeout, max_output_bytes)?;
    let (mut stdin, input_child) = UnixStream::pair()?;
    let (mut stdout, out_child) = UnixStream::pair()?;
    let (mut stderr, err_child) = UnixStream::pair()?;
    stdin.set_nonblocking(true)?;
    stdout.set_nonblocking(true)?;
    stderr.set_nonblocking(true)?;
    let child = command
        .process_group(0)
        .stdin(Stdio::from(OwnedFd::from(input_child)))
        .stdout(Stdio::from(OwnedFd::from(out_child)))
        .stderr(Stdio::from(OwnedFd::from(err_child)))
        .spawn()?;
    let mut owner = ChildOwner(Some(child));
    let pid = owner.0.as_ref().unwrap().id();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "timeout overflow"))?;
    let mut capture = Capture {
        stdout: vec![],
        stderr: vec![],
        exit_code: None,
        timed_out: false,
        truncated: false,
    };
    let mut sent = 0;
    let mut closed = false;
    loop {
        if !closed {
            match stdin.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e)
                    if matches!(e.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset) =>
                {
                    closed = true;
                }
                Err(e) => return Err(e),
            }
            if sent == input.len() {
                let _ = stdin.shutdown(std::net::Shutdown::Write);
                closed = true;
            }
        }
        let mut remaining =
            max_output_bytes.saturating_sub(capture.stdout.len() + capture.stderr.len());
        capture.truncated |= drain_bounded(&mut stdout, &mut capture.stdout, &mut remaining)?;
        capture.truncated |= drain_bounded(&mut stderr, &mut capture.stderr, &mut remaining)?;
        if let Some(code) = status_without_reaping(owner.0.as_mut().unwrap())? {
            capture.exit_code = Some(code);
            break;
        }
        if Instant::now() >= deadline {
            capture.timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    kill_tree(pid);
    let mut remaining =
        max_output_bytes.saturating_sub(capture.stdout.len() + capture.stderr.len());
    capture.truncated |= drain_bounded(&mut stdout, &mut capture.stdout, &mut remaining)?;
    capture.truncated |= drain_bounded(&mut stderr, &mut capture.stderr, &mut remaining)?;
    if let Some(mut child) = owner.0.take() {
        child.wait()?;
    }
    Ok(capture)
}

#[cfg(not(unix))]
pub fn run_process(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
    max_output_bytes: usize,
) -> io::Result<Capture> {
    let (timeout, max_output_bytes) = validate_process_limits(input, timeout, max_output_bytes)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&payload);
        });
    }
    let read_capped = |pipe: Option<Box<dyn Read + Send>>, cap: usize| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut trunc = false;
            if let Some(mut r) = pipe {
                let mut buf = [0u8; 8192];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let take = n.min(cap.saturating_sub(out.len()));
                    out.extend_from_slice(&buf[..take]);
                    trunc |= take < n;
                }
            }
            (out, trunc)
        })
    };
    let out_h = read_capped(
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        max_output_bytes,
    );
    let err_h = read_capped(
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        max_output_bytes,
    );
    let mut owner = ChildOwner(Some(child));
    let pid = owner.0.as_ref().unwrap().id();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "timeout overflow"))?;
    let mut exit_code = None;
    let mut timed_out = false;
    loop {
        if let Some(code) = status_without_reaping(owner.0.as_mut().unwrap())? {
            exit_code = Some(code);
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    kill_tree(pid);
    if let Some(mut c) = owner.0.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let (stdout, t1) = out_h.join().unwrap_or_default();
    let (stderr, t2) = err_h.join().unwrap_or_default();
    Ok(Capture {
        stdout,
        stderr,
        exit_code,
        timed_out,
        truncated: t1 || t2,
    })
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub fn run_check(cwd: &Path, check: &Check) -> io::Result<CheckResult> {
    let mut cmd = shell_command(&check.command);
    cmd.current_dir(cwd);
    let out = run_process(
        &mut cmd,
        &[],
        Duration::from_millis(check.timeout_ms.max(1)),
        check.max_output_bytes,
    )?;
    let mut truncated = out.truncated;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.len() > check.max_output_bytes {
        text.truncate(floor_char_boundary(&text, check.max_output_bytes));
        truncated = true;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let available = check.max_output_bytes.saturating_sub(text.len());
    let stderr_end = floor_char_boundary(&stderr, available);
    text.push_str(&stderr[..stderr_end]);
    truncated |= stderr_end < stderr.len();
    Ok(CheckResult {
        command: check.command.clone(),
        output: text,
        exit_code: out.exit_code,
        timed_out: out.timed_out,
        truncated,
        passed: !out.timed_out && out.exit_code == Some(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_outputs_and_continuous_output_remain_bounded() {
        let out = run(
            Path::new("/tmp"),
            "head -c 100000 /dev/zero; printf end",
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!out.timed_out && out.text.ends_with("end"));
        let out = run(Path::new("/tmp"), "yes", Duration::from_millis(100)).unwrap();
        assert!(out.timed_out && out.text.len() < 1_049_000);
    }

    #[test]
    fn timeout_preserves_evidence_and_background_writer_does_not_block() {
        let out = run(
            Path::new("/tmp"),
            "printf before; sleep 30 & wait",
            Duration::from_millis(80),
        )
        .unwrap();
        assert!(out.timed_out && out.text.contains("before") && out.text.contains("0.08s"));
        let start = Instant::now();
        let out = run(
            Path::new("/tmp"),
            "sleep 30 & printf ready",
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!out.timed_out && out.text.contains("ready"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn late_background_logs_do_not_block_later_work() {
        let path = std::env::temp_dir().join(format!("twig-background-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!(
            "(sleep 0.05; head -c 5000000 /dev/zero; printf done > '{}') & printf ready",
            path.display()
        );
        let out = run(Path::new("/tmp"), &command, Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            path.exists(),
            "background writer blocked after initial shell returned"
        );
        drop(out);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bounded_check_caps_output_and_timeout() {
        let out = run_check(
            Path::new("/tmp"),
            &Check {
                command: "while :; do echo loud; done".into(),
                timeout_ms: 40,
                max_output_bytes: 100,
            },
        )
        .unwrap();
        assert!(out.timed_out && !out.passed && out.truncated);
        assert!(out.output.len() <= 100);
    }
}
