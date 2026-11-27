use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use twig::{
    config::HarnessConfig,
    stub::{call, StubLlm},
    Daemon,
};

struct PipeInner {
    buf: VecDeque<u8>,
    closed: bool,
}

#[derive(Clone)]
struct MemPipe {
    inner: Arc<(Mutex<PipeInner>, Condvar)>,
    read_timeout: Option<Duration>,
}

fn pipe_pair(read_timeout: Option<Duration>) -> (MemPipe, MemPipe) {
    let inner = Arc::new((
        Mutex::new(PipeInner {
            buf: VecDeque::new(),
            closed: false,
        }),
        Condvar::new(),
    ));
    let writer = MemPipe {
        inner: inner.clone(),
        read_timeout: None,
    };
    let reader = MemPipe {
        inner,
        read_timeout,
    };
    (writer, reader)
}

impl MemPipe {
    fn close(&self) {
        let (lock, cvar) = &*self.inner;
        let mut g = lock.lock().unwrap();
        g.closed = true;
        cvar.notify_all();
    }
}

impl Write for MemPipe {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.inner;
        let mut g = lock.lock().unwrap();
        if g.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
        }
        g.buf.extend(data);
        cvar.notify_all();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for MemPipe {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let (lock, cvar) = &*self.inner;
        let mut g = lock.lock().unwrap();
        let deadline = self.read_timeout.map(|d| Instant::now() + d);
        while g.buf.is_empty() && !g.closed {
            if let Some(dl) = deadline {
                let now = Instant::now();
                if now >= dl {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read timeout"));
                }
                let (next_g, res) = cvar.wait_timeout(g, dl - now).unwrap();
                g = next_g;
                if res.timed_out() && g.buf.is_empty() && !g.closed {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read timeout"));
                }
            } else {
                g = cvar.wait(g).unwrap();
            }
        }
        if g.buf.is_empty() {
            return Ok(0);
        }
        let n = out.len().min(g.buf.len());
        for slot in &mut out[..n] {
            *slot = g.buf.pop_front().unwrap();
        }
        Ok(n)
    }
}

fn session(command: &str, action: &str) -> Vec<Value> {
    let dir = std::env::temp_dir().join(format!("twig-interaction-{}-{}", std::process::id(), action));
    std::fs::create_dir_all(&dir).unwrap();
    let llm = StubLlm::new(vec![
        StubLlm::with_tools("", vec![call("one", "bash", json!({"command": command}))]),
        StubLlm::text("finished"),
    ]);
    let (mut input, server_input) = pipe_pair(None);
    let (server_output, output) = pipe_pair(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(output);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut daemon =
                Daemon::new(&llm, &dir, dir.join("runs"), action, &HarnessConfig::default())
                    .unwrap();
            let out_closer = server_output.clone();
            daemon
                .serve(BufReader::new(server_input), server_output)
                .unwrap();
            out_closer.close();
        });
        writeln!(
            input,
            "{}",
            json!({"id": 1, "method": "turn", "params": {"input": "do it"}})
        )
        .unwrap();
        let mut events = Vec::new();
        let mut acted = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if !acted
                && ((action == "approval" && value["ev"] == "approval.requested")
                    || (action == "cancel" && value["ev"] == "tool.output"))
            {
                let request = if action == "approval" {
                    json!({"id": 2, "method": "approval.resolve", "params": {"id": "one", "allow": true}})
                } else {
                    json!({"id": 2, "method": "cancel"})
                };
                writeln!(input, "{request}").unwrap();
                acted = true;
            }
            let done =
                value["id"] == 1 && (value.get("result").is_some() || value.get("error").is_some());
            events.push(value);
            if done {
                break;
            }
        }
        input.close();
        events
    })
}

#[cfg(unix)]
#[test]
fn output_is_live_and_cancel_terminates_running_shell() {
    let start = Instant::now();
    let events = session("printf started; sleep 30; printf SHOULD_NOT_RUN", "cancel");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(events
        .iter()
        .any(|e| e["ev"] == "tool.output" && e["text"].as_str().unwrap_or("").contains("started")));
    assert!(!events
        .iter()
        .any(|e| e["text"].as_str().unwrap_or("").contains("SHOULD_NOT_RUN")));
}

#[test]
fn approval_waits_then_runs_the_exact_command_once() {
    let action = "approval";
    let dir = std::env::temp_dir().join(format!("twig-interaction-{}-{action}", std::process::id()));
    let _ = std::fs::remove_file(dir.join("approvals"));
    let cmd = if cfg!(windows) {
        "echo git reset --hard>> approvals"
    } else {
        "printf 'git reset --hard\\n' >> approvals"
    };
    let events = session(cmd, "approval");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["ev"] == "approval.requested")
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|e| e["ev"] == "tool.started").count(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("approvals"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
