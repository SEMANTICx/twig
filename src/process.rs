//! Bounded nonblocking socket capture; unreaped group leaders retain PID ownership.
use std::io::{Read, ErrorKind};
use std::os::fd::OwnedFd;
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn kill_group(id: u32) {
    // Negative pid selects the command's dedicated process group.
    unsafe { libc::kill(-(id as i32), libc::SIGKILL); }
}

pub fn kill_tree(pid: u32) {
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
    for child in children.split_whitespace().filter_map(|s| s.parse::<u32>().ok()) { kill_tree(child); }
    kill_group(pid);
}

pub struct Owner {
    child: Child,
    capture: Option<UnixStream>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    drain: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        kill_group(self.child.id());
        let _ = self.child.wait();
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(drain) = self.drain.take() { let _ = drain.join(); }
    }
}

pub struct Output {
    pub text: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub owner: Owner,
}

fn status_without_reaping(pid: u32) -> std::io::Result<Option<i32>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // WNOWAIT leaves the leader waitable: its numeric PID cannot be recycled
    // while session-owned background children still need cleanup.
    let rc = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
    if rc != 0 { return Err(std::io::Error::last_os_error()); }
    if unsafe { info.si_pid() } == 0 { return Ok(None); }
    let code = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED { code } else { 128 + code }))
}

pub fn run(cwd: &Path, command: &str, timeout: Duration) -> std::io::Result<Output> {
    run_live(cwd, command, timeout, &crate::control::Control::default(), &mut |_| {})
}

pub fn run_live(cwd: &Path, command: &str, timeout: Duration,
    control: &crate::control::Control, emit: &mut dyn FnMut(String)) -> std::io::Result<Output> {
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let capture = reader.try_clone()?; // all fallible descriptor duplication precedes spawn
    let out: OwnedFd = writer.try_clone()?.into();
    let err: OwnedFd = writer.into();
    let child = Command::new("sh").args(["-c", command]).current_dir(cwd)
        .process_group(0).stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err)).spawn()?;
    let mut owner = Owner { child, capture: Some(capture), stop: Default::default(), drain: None };
    let deadline = Instant::now() + timeout;
    const CAP: usize = 1_048_576;
    let mut bytes = Vec::new();
    let mut discarded = false;
    let mut buf = [0; 8192];
    let mut drain = |reader: &mut UnixStream| -> std::io::Result<()> {
        // Bounded work per poll even if a writer never stops producing output.
        for _ in 0..128 {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes.len() < CAP { emit(String::from_utf8_lossy(&buf[..n]).into_owned()); }
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
        drain(&mut reader)?;
        if let Some(code) = status_without_reaping(owner.child.id())? { break (code, false); }
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
    drain(&mut reader)?;
    // Continue discarding later background logs with bounded memory. The session
    // owns the drainer and stops it after terminating the command group.
    let mut capture = owner.capture.take().expect("capture");
    let stop = owner.stop.clone();
    owner.drain = Some(std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match capture.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {},
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                Err(e) if e.kind() == ErrorKind::Interrupted => {},
                Err(_) => break,
            }
        }
    }));
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if discarded { text.push_str("\n[capture truncated at 1 MiB]\n"); }
    if timed_out { text = format!("timeout: killed process group after {}s\n{text}", timeout.as_secs_f64()); }
    Ok(Output { text, exit_code: Some(code), timed_out, owner })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_outputs_and_continuous_output_remain_bounded() {
        let out = run(Path::new("/tmp"), "head -c 100000 /dev/zero; printf end", Duration::from_secs(2)).unwrap();
        assert!(!out.timed_out && out.text.ends_with("end"));
        let out = run(Path::new("/tmp"), "yes", Duration::from_millis(100)).unwrap();
        assert!(out.timed_out && out.text.len() < 1_049_000);
    }
    #[test]
    fn timeout_preserves_evidence_and_background_writer_does_not_block() {
        let out = run(Path::new("/tmp"), "printf before; sleep 30 & wait", Duration::from_millis(80)).unwrap();
        assert!(out.timed_out && out.text.contains("before") && out.text.contains("0.08s"));
        let start = Instant::now();
        let out = run(Path::new("/tmp"), "sleep 30 & printf ready", Duration::from_secs(2)).unwrap();
        assert!(!out.timed_out && out.text.contains("ready"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn late_background_logs_do_not_block_later_work() {
        let path = std::env::temp_dir().join(format!("twig-background-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let command = format!("(sleep 0.05; head -c 5000000 /dev/zero; printf done > '{}') & printf ready", path.display());
        let out = run(Path::new("/tmp"), &command, Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !path.exists() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        assert!(path.exists(), "background writer blocked after initial shell returned");
        drop(out);
        let _ = std::fs::remove_file(path);
    }
}
