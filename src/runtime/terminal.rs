//! A thin terminal client of the same line-delimited protocol as IDE clients.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::config::paths;

pub fn chat(repo: &str, resume: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg(repo)
        .arg(paths::state_dir().join("sessions"));
    if let Some(id) = resume {
        command.arg(id);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("missing server stdout")?;
    let input = std::sync::Arc::new(std::sync::Mutex::new(
        child.stdin.take().ok_or("missing server stdin")?,
    ));
    let reader = std::thread::spawn(move || {
        let terminal = std::io::stdout().is_terminal();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let mut out = std::io::stdout().lock();
            match v["ev"].as_str() {
                Some("model.delta") => {
                    let _ = write!(out, "{}", v["text"].as_str().unwrap_or(""));
                }
                Some("tool.started") => {
                    let _ = writeln!(out, "\n[tool {}]", v["id"].as_str().unwrap_or(""));
                }
                Some("tool.output") => {
                    let _ = write!(out, "{}", v["text"].as_str().unwrap_or(""));
                }
                Some("approval.requested") => {
                    let _ = writeln!(
                        out,
                        "\nApproval {}: {}\n/approve ID or /deny ID",
                        v["id"], v["args"]
                    );
                }
                Some("turn.completed") => {
                    if v["reason"] == "final" {
                        let _ = writeln!(out);
                    }
                }
                Some("context.compacted") => {
                    let _ = writeln!(out, "\n[context compacted]");
                }
                Some(_) => {}
                None => {
                    if let Some(e) = v.get("error") {
                        let _ = writeln!(out, "\nError: {e}");
                    } else if v["result"]["done"].is_boolean() {
                        let _ = writeln!(
                            out,
                            "\n[{}]",
                            v["result"]["reason"].as_str().unwrap_or("done")
                        );
                    }
                    if terminal {
                        let _ = write!(out, "\n> ");
                    }
                }
            }
            let _ = out.flush();
        }
    });
    println!("Twig · magpie\nEnter a task. During a run use /steer TEXT, /cancel, /approve ID, /deny ID. /quit exits.");
    let mut id = 0;
    for line in std::io::stdin().lock().lines() {
        let text = line.map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            continue;
        }
        id += 1;
        let mut request = if text == "/quit" {
            break;
        } else if text == "/cancel" {
            json!({"method": "cancel"})
        } else if let Some(t) = text.strip_prefix("/steer ") {
            json!({"method": "steer", "params": {"input": t}})
        } else if let Some(t) = text.strip_prefix("/approve ") {
            json!({"method": "approval.resolve", "params": {"id": t.trim(), "allow": true}})
        } else if let Some(t) = text.strip_prefix("/deny ") {
            json!({"method": "approval.resolve", "params": {"id": t.trim(), "allow": false}})
        } else {
            json!({"method": "turn", "params": {"input": text}})
        };
        request["id"] = json!(id);
        let mut writer = input.lock().map_err(|_| "input lock")?;
        writeln!(writer, "{request}").map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }
    if std::io::stdin().is_terminal() {
        let mut w = input.lock().unwrap();
        let _ = writeln!(w, "{{\"method\":\"cancel\"}}");
    }
    drop(input);
    let _ = child.wait();
    let _ = reader.join();
    Ok(())
}
