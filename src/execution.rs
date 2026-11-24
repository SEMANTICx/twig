//! Bounded command/test checks and JSON-lines differential replay.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
}
fn default_timeout() -> u64 { 120_000 }
fn default_output() -> usize { 1_048_576 }
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub truncated: bool,
    pub passed: bool,
}
pub struct Capture { pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub exit_code: Option<i32>, pub timed_out: bool, pub truncated: bool }
struct ChildOwner(Option<Child>);
impl Drop for ChildOwner {
    fn drop(&mut self) { if let Some(child) = &mut self.0 { crate::process::kill_tree(child.id()); let _ = child.wait(); } }
}
fn drain(stream: &mut UnixStream, bytes: &mut Vec<u8>, remaining: &mut usize) -> io::Result<bool> {
    let mut buf = [0; 8192];
    let mut truncated = false;
    for _ in 0..32 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => { let take = n.min(*remaining); bytes.extend_from_slice(&buf[..take]); *remaining -= take; truncated |= take < n; },
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(truncated)
}
/// Nonblocking bounded stdin/stdout/stderr; terminates all children before reaping.
pub fn run_process(command: &mut Command, input: &[u8], timeout: Duration, max_output_bytes: usize) -> io::Result<Capture> {
    let (mut stdin, input_child) = UnixStream::pair()?;
    let (mut stdout, out_child) = UnixStream::pair()?;
    let (mut stderr, err_child) = UnixStream::pair()?;
    stdin.set_nonblocking(true)?; stdout.set_nonblocking(true)?; stderr.set_nonblocking(true)?;
    let child = command.process_group(0).stdin(Stdio::from(OwnedFd::from(input_child)))
        .stdout(Stdio::from(OwnedFd::from(out_child))).stderr(Stdio::from(OwnedFd::from(err_child))).spawn()?;
    let mut owner = ChildOwner(Some(child));
    let pid = owner.0.as_ref().unwrap().id();
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timeout overflow"))?;
    let mut capture = Capture { stdout: vec![], stderr: vec![], exit_code: None, timed_out: false, truncated: false };
    let mut sent = 0;
    let mut closed = false;
    loop {
        if !closed {
            match stdin.write(&input[sent..]) {
                Ok(n) => sent += n,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {},
                Err(e) if matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) => closed = true,
                Err(e) => return Err(e),
            }
            if sent == input.len() { let _ = stdin.shutdown(std::net::Shutdown::Write); closed = true; }
        }
        let mut remaining = max_output_bytes.saturating_sub(capture.stdout.len() + capture.stderr.len());
        capture.truncated |= drain(&mut stdout, &mut capture.stdout, &mut remaining)?;
        capture.truncated |= drain(&mut stderr, &mut capture.stderr, &mut remaining)?;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        if rc != 0 { return Err(io::Error::last_os_error()); }
        if unsafe { info.si_pid() } != 0 {
            capture.exit_code = Some(if info.si_code == libc::CLD_EXITED { unsafe { info.si_status() } } else { 128 + unsafe { info.si_status() } });
            break;
        }
        if Instant::now() >= deadline { capture.timed_out = true; break; }
        std::thread::sleep(Duration::from_millis(5));
    }
    crate::process::kill_tree(pid);
    let mut remaining = max_output_bytes.saturating_sub(capture.stdout.len() + capture.stderr.len());
    capture.truncated |= drain(&mut stdout, &mut capture.stdout, &mut remaining)?;
    capture.truncated |= drain(&mut stderr, &mut capture.stderr, &mut remaining)?;
    if let Some(mut child) = owner.0.take() { child.wait()?; }
    Ok(capture)
}
pub fn run_check(cwd: &Path, check: &Check) -> io::Result<CheckResult> {
    let out = run_process(Command::new("sh").args(["-c", &check.command]).current_dir(cwd), &[], Duration::from_millis(check.timeout_ms.max(1)), check.max_output_bytes)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let available = check.max_output_bytes.saturating_sub(text.len());
    let mut end = available.min(stderr.len()); while !stderr.is_char_boundary(end) { end -= 1; }
    text.push_str(&stderr[..end]);
    let truncated = out.truncated || end < stderr.len();
    Ok(CheckResult { command: check.command.clone(), output: text, exit_code: out.exit_code, timed_out: out.timed_out, truncated, passed: !out.timed_out && out.exit_code == Some(0) })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DifferentialCase { pub input: Value, pub expected: Value }
#[derive(Debug, Clone, Serialize)]
pub struct DifferentialResult { pub passed: bool, pub cases: usize, pub failures: Vec<usize>, pub output: String }
/// JSON-lines protocol; malformed or extra output is a failure, never ignored.
pub fn run_differential(cwd: &Path, command: &str, cases: &[DifferentialCase], timeout: Duration, max_output_bytes: usize) -> io::Result<DifferentialResult> {
    let mut payload = Vec::new();
    for case in cases { serde_json::to_writer(&mut payload, &case.input)?; payload.push(b'\n'); }
    let out = run_process(Command::new("sh").args(["-c", command]).current_dir(cwd), &payload, timeout, max_output_bytes)?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let actual: Result<Vec<Value>, _> = text.lines().map(serde_json::from_str).collect();
    let failures = cases.iter().enumerate().filter_map(|(i,c)| (actual.as_ref().ok().and_then(|a| a.get(i)).is_none_or(|a| !crate::verify::eq_json(a, &c.expected, 1e-9))).then_some(i)).collect::<Vec<_>>();
    Ok(DifferentialResult { passed: !cases.is_empty() && !out.timed_out && !out.truncated && out.exit_code == Some(0) && failures.is_empty() && actual.as_ref().is_ok_and(|a| a.len() == cases.len()), cases: cases.len(), failures, output: text })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_bounded_and_replay_is_strict() {
        let cwd = Path::new("/tmp");
        let out = run_check(cwd, &Check { command: "while :; do echo loud; done".into(), timeout_ms: 40, max_output_bytes: 100 }).unwrap();
        assert!(out.timed_out && !out.passed && out.truncated); assert!(out.output.len() <= 100);
        let cases = [DifferentialCase { input: serde_json::json!({"x":1}), expected: serde_json::json!({"x":1}) }];
        assert!(run_differential(cwd, "cat", &cases, Duration::from_secs(1), 1024).unwrap().passed);
        assert!(!run_differential(cwd, "echo not-json; cat", &cases, Duration::from_secs(1), 1024).unwrap().passed);
    }
}
